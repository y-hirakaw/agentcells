//! 設定（config.toml）を、productごとのライン（工場の製造ライン）として読み解く。
//!
//! `agentcells config view` が絵にする材料と、設定の注意（入口からたどり着けないcell、done に行けない輪など）を
//! 出す。絵を描くのは [`crate::view`]。ここは絵に関わらない判定だけにして、テストしやすくする。

use std::collections::{BTreeSet, VecDeque};

use crate::config::{Cell, Config, Product, Target};
use crate::tick;
use crate::transition::{DONE, STOP};

/// 設定の全体。productごとのラインと、どのラインにも入っていないcell。
#[derive(Debug, Clone)]
pub struct Factory {
    pub lines: Vec<Line>,
    /// 定義されているのに、どの `product.cells` にも入っていないcell
    pub unused: Vec<String>,
}

/// 1つのproductのライン。
#[derive(Debug, Clone)]
pub struct Line {
    pub product: String,
    pub issues: String,
    pub repos: Vec<String>,
    /// `product.cells` の順
    pub stations: Vec<Station>,
    /// 入口がない（すべてのcellが、ほかのcellの `next` から来るか、Issueを扱わない）
    pub no_entry: bool,
}

/// ラインの中の1つのcell（機械）。
#[derive(Debug, Clone)]
pub struct Station {
    /// このproductで動くcell。`next` は `[product.next]` で替えたもの
    pub cell: Cell,
    /// `[product.next]` で進め先を替えたときの、元の `next`
    pub replaced: Option<Vec<String>>,
    /// 入口（人がラベルを付けて始めるcell）か
    pub entry: bool,
    /// 入口からいくつ進んだところか（tickが動かす順と同じ決め方。絵の列にする）
    pub depth: usize,
    pub notices: Vec<Notice>,
}

/// cellについての注意。設定としては正しいが、想定通りに流れないかもしれないもの。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// ほかのcellの `next` からは来るが、入口からたどり着けない
    Unreachable,
    /// ほかのcellの `next` から来ず、入口でもない（人がラベルを付けたときだけ動く）
    NotFed,
    /// `done` にも `stop` にも進めない。どうしてかは [`NoExit`]
    NoExit(NoExit),
}

/// `done` にも `stop` にも進めない cell が、どうなっているか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoExit {
    /// 行き止まり。進める先の cell がなく、`needs-human` しか選べない
    DeadEnd,
    /// 抜けられない輪の中にある
    InLoop,
    /// 進める先が、行き止まりか抜けられない輪にしか行けない cell だけ
    LeadsNowhere,
}

impl Notice {
    pub fn describe(self) -> &'static str {
        match self {
            Notice::Unreachable => "入口からたどり着けません（人がラベルを付けたときだけ動きます）",
            Notice::NotFed => {
                "ほかのcellの next から来ず、Issueを扱わないので入口にもなりません（人がラベルを付けたときだけ動きます）"
            }
            Notice::NoExit(NoExit::DeadEnd) => {
                "done にも stop にも進めません（選べるのは needs-human だけです）"
            }
            Notice::NoExit(NoExit::InLoop) => {
                "done にも stop にも行けない輪の中にあります（抜けるには人間が要ります）"
            }
            Notice::NoExit(NoExit::LeadsNowhere) => {
                "進める先が、done にも stop にも行けない cell だけです（抜けるには人間が要ります）"
            }
        }
    }
}

impl Factory {
    /// 注意をすべて、どこのものかを添えて返す。
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        for line in &self.lines {
            notes.extend(line.notes());
        }
        for name in &self.unused {
            notes.push(format!(
                "cell「{name}」は定義されていますが、どのproductの cells にも入っていません"
            ));
        }
        notes
    }
}

impl Line {
    /// このラインの注意。
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.no_entry {
            notes.push(format!(
                "product「{}」には入口（Issueを扱い、ほかのcellの next から来ないcell）がありません",
                self.product
            ));
        }
        for station in &self.stations {
            for notice in &station.notices {
                notes.push(format!(
                    "product「{}」のcell「{}」: {}",
                    self.product,
                    station.cell.name,
                    notice.describe()
                ));
            }
        }
        notes
    }

    pub fn station(&self, name: &str) -> Option<&Station> {
        self.stations.iter().find(|s| s.cell.name == name)
    }
}

/// 設定を読み解く。設定は [`Config::load`] で確かめてあるものとする（定義のないcellは飛ばす）。
pub fn read(config: &Config) -> Factory {
    let lines = config
        .products
        .iter()
        .map(|p| read_line(config, p))
        .collect();
    let unused = config
        .unused_cells()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    Factory { lines, unused }
}

fn read_line(config: &Config, product: &Product) -> Line {
    let cells: Vec<Cell> = product
        .cells
        .iter()
        .filter_map(|name| config.cell_in(product, name))
        .collect();
    let names: BTreeSet<&str> = cells.iter().map(|c| c.name.as_str()).collect();
    let nexts = |cell| cell_nexts(cell, &names);
    let pointed: BTreeSet<&str> = cells.iter().flat_map(nexts).collect();
    let entries: Vec<&str> = cells
        .iter()
        .filter(|c| c.on.contains(&Target::Issue) && !pointed.contains(c.name.as_str()))
        .map(|c| c.name.as_str())
        .collect();
    let find = |name: &str| cells.iter().find(|c| c.name == name);
    // `from` から `next` をたどって行けるcell（`from` は、輪で戻ってきたときだけ入る）
    let reach = |from: &[&str]| -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<&str> = from.iter().copied().collect();
        while let Some(name) = queue.pop_front() {
            for next in find(name).map(nexts).unwrap_or_default() {
                if seen.insert(next.to_string()) {
                    queue.push_back(next);
                }
            }
        }
        seen
    };
    let reachable: BTreeSet<String> = reach(&entries)
        .into_iter()
        .chain(entries.iter().map(|e| e.to_string()))
        .collect();
    // done か stop に行けるcell。行けるcellに進めるなら、そのcellも行ける
    let mut exits: BTreeSet<&str> = cells
        .iter()
        .filter(|c| c.next.iter().any(|n| n == DONE || n == STOP))
        .map(|c| c.name.as_str())
        .collect();
    loop {
        let more: Vec<&str> = cells
            .iter()
            .filter(|c| !exits.contains(c.name.as_str()))
            .filter(|c| nexts(c).iter().any(|n| exits.contains(n)))
            .map(|c| c.name.as_str())
            .collect();
        if more.is_empty() {
            break;
        }
        exits.extend(more);
    }

    let depths = tick::depths(config, product);
    let stations = cells
        .iter()
        .map(|cell| {
            let name = cell.name.as_str();
            let entry = entries.contains(&name);
            let mut notices = Vec::new();
            if !entry && !pointed.contains(name) {
                notices.push(Notice::NotFed);
            } else if !entries.is_empty() && !reachable.contains(name) {
                notices.push(Notice::Unreachable);
            }
            if !exits.contains(name) {
                let kind = if nexts(cell).is_empty() {
                    NoExit::DeadEnd
                } else if reach(&[name]).contains(name) {
                    NoExit::InLoop
                } else {
                    NoExit::LeadsNowhere
                };
                notices.push(Notice::NoExit(kind));
            }
            let replaced = product.next.contains_key(name).then(|| {
                config
                    .cell(name)
                    .map(|c| c.next.clone())
                    .unwrap_or_default()
            });
            Station {
                cell: cell.clone(),
                replaced,
                entry,
                depth: depths.get(name).copied().unwrap_or(0),
                notices,
            }
        })
        .collect();
    Line {
        product: product.name.clone(),
        issues: product.issues.to_string(),
        repos: product.repos.iter().map(|r| r.to_string()).collect(),
        stations,
        no_entry: entries.is_empty(),
    }
}

/// このproductの中の、cellからcellへの進め先（`names` はproductのcell）。
fn cell_nexts<'a>(cell: &'a Cell, names: &BTreeSet<&str>) -> Vec<&'a str> {
    cell.next
        .iter()
        .map(String::as_str)
        .filter(|n| *n != cell.name && names.contains(n))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 標準のcell（triage、implement、review、assess）なら、prompt を書かなくてよい
    const CONFIG: &str = r#"
operator = "me"

[[product]]
name = "app"
issues = "octo/app"
repos = ["octo/app"]
cells = ["triage", "implement", "review"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
next = ["implement", "done"]

[[cell]]
name = "implement"
on = ["issue", "pull_request"]
label = "ac:implement"
next = ["review"]

[[cell]]
name = "review"
on = "pull_request"
label = "ac:review"
next = ["implement", "done"]
"#;

    fn factory(text: &str) -> Factory {
        let config = Config::parse(text, std::path::Path::new("/")).unwrap();
        read(&config)
    }

    fn notices(factory: &Factory, cell: &str) -> Vec<Notice> {
        factory.lines[0].station(cell).unwrap().notices.clone()
    }

    #[test]
    fn the_standard_line_has_no_notices() {
        let factory = factory(CONFIG);
        assert!(factory.notes().is_empty(), "{:?}", factory.notes());
        let line = &factory.lines[0];
        assert!(!line.no_entry);
        assert!(line.station("triage").unwrap().entry);
        assert!(!line.station("implement").unwrap().entry);
        let depths: Vec<usize> = line.stations.iter().map(|s| s.depth).collect();
        assert_eq!(depths, [0, 1, 2]);
    }

    #[test]
    fn finds_a_loop_that_never_reaches_done() {
        let text = CONFIG.replace(
            "label = \"ac:review\"\nnext = [\"implement\", \"done\"]",
            "label = \"ac:review\"\nnext = [\"implement\"]",
        );
        let factory = factory(&text);
        for cell in ["implement", "review"] {
            assert_eq!(
                notices(&factory, cell),
                [Notice::NoExit(NoExit::InLoop)],
                "{cell}"
            );
        }
        // triage は done に進めるので、輪には入らない
        assert!(notices(&factory, "triage").is_empty());
    }

    #[test]
    fn a_cell_that_only_feeds_a_loop_is_not_in_it() {
        let text = CONFIG
            .replace(
                "label = \"ac:review\"\nnext = [\"implement\", \"done\"]",
                "label = \"ac:review\"\nnext = [\"implement\"]",
            )
            .replace(
                "label = \"ac:triage\"\nnext = [\"implement\", \"done\"]",
                "label = \"ac:triage\"\nnext = [\"implement\"]",
            );
        let factory = factory(&text);
        assert_eq!(
            notices(&factory, "triage"),
            [Notice::NoExit(NoExit::LeadsNowhere)]
        );
        assert_eq!(
            notices(&factory, "implement"),
            [Notice::NoExit(NoExit::InLoop)]
        );
    }

    #[test]
    fn a_dead_end_is_not_a_loop() {
        let text = CONFIG.replace(
            "label = \"ac:review\"\nnext = [\"implement\", \"done\"]",
            "label = \"ac:review\"",
        );
        let factory = factory(&text);
        assert_eq!(
            notices(&factory, "review"),
            [Notice::NoExit(NoExit::DeadEnd)]
        );
        // implement は review に進めるので、行き止まりではない
        assert_eq!(
            notices(&factory, "implement"),
            [Notice::NoExit(NoExit::LeadsNowhere)]
        );
    }

    #[test]
    fn stop_is_also_a_way_out() {
        let text = CONFIG.replace(
            "label = \"ac:review\"\nnext = [\"implement\", \"done\"]",
            "label = \"ac:review\"\nnext = [\"implement\", \"stop\"]",
        );
        assert!(factory(&text).notes().is_empty());
    }

    #[test]
    fn finds_cells_that_nothing_feeds() {
        // PRだけを扱うcellがどこからも来なければ、入口にならない
        let text = CONFIG
            .replace(
                "cells = [\"triage\", \"implement\", \"review\"]",
                "cells = [\"triage\", \"implement\", \"review\", \"polish\"]",
            )
            .replace(
                "[[cell]]\nname = \"review\"",
                "[[cell]]\nname = \"polish\"\non = \"pull_request\"\nlabel = \"ac:polish\"\nprompt = \"p.md\"\nnext = [\"review\"]\n\n[[cell]]\nname = \"review\"",
            );
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), "prompt").unwrap();
        let config = Config::parse(&text, dir.path()).unwrap();
        let factory = read(&config);
        assert_eq!(notices(&factory, "polish"), [Notice::NotFed]);
        assert!(!factory.lines[0].station("polish").unwrap().entry);
        assert!(notices(&factory, "review").is_empty());
    }

    #[test]
    fn finds_cells_unreachable_from_the_entry() {
        // triage は done だけ。implement と review は互いに来るが、入口からはたどれない
        let text = CONFIG
            .replace(
                "next = [\"implement\", \"done\"]\n\n[[cell]]\nname = \"implement\"",
                "next = [\"done\"]\n\n[[cell]]\nname = \"implement\"",
            )
            .replace(
                "on = [\"issue\", \"pull_request\"]",
                "on = \"pull_request\"",
            );
        let factory = factory(&text);
        assert_eq!(notices(&factory, "implement"), [Notice::Unreachable]);
        assert_eq!(notices(&factory, "review"), [Notice::Unreachable]);
        assert!(notices(&factory, "triage").is_empty());
    }

    #[test]
    fn a_line_without_an_entry_is_noted() {
        let text = CONFIG
            .replace(
                "cells = [\"triage\", \"implement\", \"review\"]",
                "cells = [\"implement\", \"review\"]",
            )
            .replace(
                "on = [\"issue\", \"pull_request\"]",
                "on = \"pull_request\"",
            );
        let factory = factory(&text);
        assert!(factory.lines[0].no_entry);
        let notes = factory.notes();
        assert!(notes.iter().any(|n| n.contains("入口")), "{notes:?}");
        // 入口がなければ、たどり着けないとは言わない（入口がないことだけを出す）
        assert!(notices(&factory, "implement").is_empty());
        // triage はどのproductにも入っていない
        assert_eq!(factory.unused, ["triage"]);
        assert!(notes.iter().any(|n| n.contains("cell「triage」")
            && n.contains("どのproductの cells にも入っていません")));
    }

    #[test]
    fn product_next_replaces_where_a_cell_goes() {
        let text = CONFIG.replace(
            "cells = [\"triage\", \"implement\", \"review\"]",
            "cells = [\"triage\", \"implement\", \"review\"]\n\n[product.next]\nreview = [\"stop\"]",
        );
        let factory = factory(&text);
        let review = factory.lines[0].station("review").unwrap();
        assert_eq!(review.cell.next, ["stop"]);
        assert_eq!(
            review.replaced.as_deref(),
            Some(&["implement".to_string(), "done".to_string()][..])
        );
        assert!(
            factory.lines[0]
                .station("triage")
                .unwrap()
                .replaced
                .is_none()
        );
        assert!(factory.notes().is_empty(), "{:?}", factory.notes());
    }
}
