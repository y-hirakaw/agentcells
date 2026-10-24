//! `~/.agentcells/config.toml` の読み込みと確認。
//!
//! product（どのリポジトリのIssueを、どのリポジトリで作業するか）と、
//! cell（どのラベルを拾い、どのプロンプトで動くか）を書く。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::home::RepoName;
use crate::labels;
use crate::transition;

/// `agentcells setup` が、設定ファイルがないときに置く雛形。
pub const TEMPLATE: &str = r#"# agentcellsの設定。書き方は https://github.com/y-hirakaw/agentcells を見てください。
#
# product: どのリポジトリのIssueを、どのリポジトリで作業するか
#
# [[product]]
# name = "agentcells"
# issues = "y-hirakaw/agentcells"    # Issueを置くリポジトリ
# repos = ["y-hirakaw/agentcells"]   # 作業するリポジトリ
# cells = ["triage"]                 # このproductで動かすcell
#
# cell: どのラベルが付いたIssue/PRを拾い、どのプロンプトで動くか
#
# [[cell]]
# name = "triage"
# on = "issue"                       # issue か pull_request（両方なら ["issue", "pull_request"]）
# label = "ac:triage"                # 拾うラベル（ac: で始める）
# prompt = "cells/triage.md"         # プロンプトのファイル（このファイルからの相対パス）
# next = ["implement", "done"]       # 進めてよい先（cellの名前か done）。needs-human はいつでも選べる
# max_rounds = 3                     # 往復の上限（1〜3。省略すると3）
# timeout_minutes = 30               # 1回の実行の上限（省略すると30）
"#;

/// 往復の上限。これより多く往復させず、人間に回す。
pub const MAX_ROUNDS_LIMIT: u32 = 3;
const DEFAULT_MAX_ROUNDS: u32 = MAX_ROUNDS_LIMIT;
const DEFAULT_TIMEOUT_MINUTES: u32 = 30;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, rename = "product")]
    pub products: Vec<Product>,
    #[serde(default, rename = "cell")]
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Product {
    pub name: String,
    /// Issueを置くリポジトリ
    pub issues: RepoName,
    /// 作業するリポジトリ
    pub repos: Vec<RepoName>,
    /// このproductで動かすcellの名前
    pub cells: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub name: String,
    /// 扱うもの。`"issue"` のように1つでも、`["issue", "pull_request"]` のようにリストでも書ける
    #[serde(deserialize_with = "one_or_many")]
    pub on: Vec<Target>,
    /// 拾うラベル
    pub label: String,
    /// プロンプトのファイル。読み込んだあとは絶対パスになっている
    pub prompt: PathBuf,
    /// 進めてよい先（cellの名前か `done`）。`needs-human` は書かなくても選べる
    #[serde(default)]
    pub next: Vec<String>,
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
    #[serde(default = "default_timeout_minutes")]
    pub timeout_minutes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Issue,
    PullRequest,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Target::Issue => "issue",
            Target::PullRequest => "pull_request",
        }
    }
}

fn one_or_many<'de, D>(deserializer: D) -> std::result::Result<Vec<Target>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Target),
        Many(Vec<Target>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(t) => vec![t],
        OneOrMany::Many(ts) => ts,
    })
}

fn default_max_rounds() -> u32 {
    DEFAULT_MAX_ROUNDS
}

fn default_timeout_minutes() -> u32 {
    DEFAULT_TIMEOUT_MINUTES
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::new(format!("{} を読めません: {e}", path.display()))
                .with_hint("`agentcells setup` を実行すると雛形が置かれます")
        })?;
        let base = path.parent().unwrap_or(Path::new("."));
        Self::parse(&text, base).map_err(|e| {
            let message = format!("{} に問題があります:\n{}", path.display(), e.message);
            Error {
                message,
                hint: e.hint,
            }
        })
    }

    /// 設定を読んで確かめる。プロンプトのパスは `base` からの相対パスとして解決する。
    pub fn parse(text: &str, base: &Path) -> Result<Self> {
        let mut config: Config = toml::from_str(text).map_err(|e| {
            Error::new(e.to_string().trim_end().to_string())
                .with_hint("TOMLの書き方と項目名を確かめてください")
        })?;
        for cell in &mut config.cells {
            cell.prompt = base.join(&cell.prompt);
        }
        let problems = config.problems();
        if problems.is_empty() {
            Ok(config)
        } else {
            let list = problems
                .iter()
                .map(|p| format!("  - {p}"))
                .collect::<Vec<_>>()
                .join("\n");
            Err(Error::new(list).with_hint("上の項目を直してから、もう一度実行してください"))
        }
    }

    pub fn cell(&self, name: &str) -> Option<&Cell> {
        self.cells.iter().find(|c| c.name == name)
    }

    /// 見つかった問題をすべて返す。1つずつ直しては実行し直す手間を減らすため、最初の1つで止めない。
    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();

        let mut product_names = HashSet::new();
        for p in &self.products {
            if !is_valid_name(&p.name) {
                problems.push(format!(
                    "productの名前「{}」に使えない文字があります（英小文字・数字・-）",
                    p.name
                ));
            }
            if !product_names.insert(p.name.as_str()) {
                problems.push(format!("productの名前「{}」が重複しています", p.name));
            }
            if p.repos.is_empty() {
                problems.push(format!(
                    "product「{}」の repos が空です。作業するリポジトリを書いてください",
                    p.name
                ));
            }
            if p.cells.is_empty() {
                problems.push(format!(
                    "product「{}」の cells が空です。動かすcellを書いてください",
                    p.name
                ));
            }
            for name in &p.cells {
                let Some(cell) = self.cell(name) else {
                    problems.push(format!(
                        "product「{}」のcell「{name}」が [[cell]] に定義されていません",
                        p.name
                    ));
                    continue;
                };
                // 遷移先のcellも、同じproductで動いていないと拾われない。
                for next in &cell.next {
                    if self.cell(next).is_some() && !p.cells.contains(next) {
                        problems.push(format!(
                            "product「{}」のcell「{name}」は「{next}」に進めますが、「{next}」がこのproductの cells にありません",
                            p.name
                        ));
                    }
                }
            }
        }

        let mut cell_names = HashSet::new();
        let mut cell_labels = HashSet::new();
        for c in &self.cells {
            if !is_valid_name(&c.name) {
                problems.push(format!(
                    "cellの名前「{}」に使えない文字があります（英小文字・数字・-）",
                    c.name
                ));
            }
            if !cell_names.insert(c.name.as_str()) {
                problems.push(format!("cellの名前「{}」が重複しています", c.name));
            }
            if transition::is_reserved(&c.name) {
                problems.push(format!(
                    "cellの名前「{}」は遷移先として予約されています。別の名前にしてください",
                    c.name
                ));
            }
            for next in &c.next {
                if next == &c.name {
                    problems.push(format!("cell「{}」の next に自分自身があります", c.name));
                } else if !transition::is_reserved(next) && self.cell(next).is_none() {
                    problems.push(format!(
                        "cell「{}」の next の「{next}」が [[cell]] に定義されていません（cellの名前か {} を書きます）",
                        c.name,
                        transition::DONE
                    ));
                }
            }
            if !c.label.starts_with(labels::PREFIX) || c.label.len() == labels::PREFIX.len() {
                problems.push(format!(
                    "cell「{}」のラベル「{}」は {} で始めてください",
                    c.name,
                    c.label,
                    labels::PREFIX
                ));
            }
            if labels::is_system(&c.label) {
                problems.push(format!(
                    "cell「{}」のラベル「{}」はagentcellsが使うラベルです。別の名前にしてください",
                    c.name, c.label
                ));
            }
            if !cell_labels.insert(c.label.as_str()) {
                problems.push(format!(
                    "ラベル「{}」を複数のcellが拾おうとしています",
                    c.label
                ));
            }
            if !c.prompt.is_file() {
                problems.push(format!(
                    "cell「{}」のプロンプト {} がありません",
                    c.name,
                    c.prompt.display()
                ));
            }
            if c.on.is_empty() {
                problems.push(format!(
                    "cell「{}」の on が空です。issue か pull_request を書いてください",
                    c.name
                ));
            }
            if !(1..=MAX_ROUNDS_LIMIT).contains(&c.max_rounds) {
                problems.push(format!(
                    "cell「{}」の max_rounds は1から{MAX_ROUNDS_LIMIT}までにしてください",
                    c.name
                ));
            }
            if c.timeout_minutes == 0 {
                problems.push(format!(
                    "cell「{}」の timeout_minutes は1以上にしてください",
                    c.name
                ));
            }
        }
        problems
    }
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[[product]]
name = "agentcells"
issues = "y-hirakaw/agentcells"
repos = ["y-hirakaw/agentcells"]
cells = ["triage", "review"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
prompt = "cells/triage.md"
next = ["review", "done"]

[[cell]]
name = "review"
on = "pull_request"
label = "ac:review"
prompt = "cells/review.md"
max_rounds = 2
timeout_minutes = 10
"#;

    #[test]
    fn on_accepts_a_list() {
        let text = VALID.replace(
            "on = \"pull_request\"",
            "on = [\"issue\", \"pull_request\"]",
        );
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(&text, dir.path()).unwrap();
        assert_eq!(
            config.cell("review").unwrap().on,
            vec![Target::Issue, Target::PullRequest]
        );
        let empty = VALID.replace("on = \"pull_request\"", "on = []");
        assert!(problems_of(&empty).contains("on が空"));
    }

    #[test]
    fn next_defaults_to_empty() {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(VALID, dir.path()).unwrap();
        assert_eq!(config.cell("triage").unwrap().next, vec!["review", "done"]);
        assert!(config.cell("review").unwrap().next.is_empty());
    }

    #[test]
    fn max_rounds_is_capped_at_three() {
        let ok = VALID.replace("max_rounds = 2", "max_rounds = 3");
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        assert!(Config::parse(&ok, dir.path()).is_ok());
        let over = VALID.replace("max_rounds = 2", "max_rounds = 4");
        assert!(problems_of(&over).contains("1から3まで"));
    }

    #[test]
    fn rejects_bad_next() {
        let text = VALID.replace(
            "next = [\"review\", \"done\"]",
            "next = [\"triage\", \"deploy\", \"done\", \"needs-human\"]",
        );
        let message = problems_of(&text);
        assert!(message.contains("自分自身"), "{message}");
        assert!(message.contains("「deploy」が [[cell]]"), "{message}");
        assert!(!message.contains("「done」"), "{message}");
        assert!(!message.contains("「needs-human」"), "{message}");
    }

    #[test]
    fn rejects_next_outside_the_product() {
        let text = VALID.replace("cells = [\"triage\", \"review\"]", "cells = [\"triage\"]");
        let message = problems_of(&text);
        assert!(
            message.contains("「review」がこのproductの cells にありません"),
            "{message}"
        );
    }

    #[test]
    fn rejects_reserved_cell_names() {
        let text = VALID
            .replace("name = \"review\"", "name = \"done\"")
            .replace("\"review\", \"done\"", "\"done\"")
            .replace(
                "cells = [\"triage\", \"review\"]",
                "cells = [\"triage\", \"done\"]",
            );
        assert!(problems_of(&text).contains("予約されています"));
    }

    fn base_with_prompts(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cells")).unwrap();
        for name in names {
            std::fs::write(dir.path().join("cells").join(name), "prompt").unwrap();
        }
        dir
    }

    fn problems_of(text: &str) -> String {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        Config::parse(text, dir.path()).unwrap_err().message
    }

    #[test]
    fn parses_products_and_cells() {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(VALID, dir.path()).unwrap();

        let product = &config.products[0];
        assert_eq!(product.issues.to_string(), "y-hirakaw/agentcells");
        assert_eq!(product.cells, vec!["triage", "review"]);

        let triage = config.cell("triage").unwrap();
        assert_eq!(triage.on, vec![Target::Issue]);
        assert_eq!(triage.max_rounds, DEFAULT_MAX_ROUNDS);
        assert_eq!(triage.timeout_minutes, DEFAULT_TIMEOUT_MINUTES);
        assert_eq!(triage.prompt, dir.path().join("cells/triage.md"));

        let review = config.cell("review").unwrap();
        assert_eq!(review.on, vec![Target::PullRequest]);
        assert_eq!(review.max_rounds, 2);
    }

    #[test]
    fn template_is_a_valid_empty_config() {
        let config = Config::parse(TEMPLATE, Path::new("/nonexistent")).unwrap();
        assert!(config.products.is_empty());
        assert!(config.cells.is_empty());
    }

    #[test]
    fn reports_unknown_fields_with_hint() {
        let err = Config::parse("[[cell]]\nnmae = \"x\"\n", Path::new("/")).unwrap_err();
        assert!(err.message.contains("nmae"), "{}", err.message);
        assert!(err.hint.is_some());
    }

    #[test]
    fn reports_invalid_repo_names() {
        let text = VALID.replace("\"y-hirakaw/agentcells\"]", "\"agentcells\"]");
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let err = Config::parse(&text, dir.path()).unwrap_err();
        assert!(
            err.message.contains("リポジトリ名の形式"),
            "{}",
            err.message
        );
    }

    #[test]
    fn reports_all_problems_at_once() {
        let text = VALID
            .replace(
                "cells = [\"triage\", \"review\"]",
                "cells = [\"triage\", \"deploy\"]",
            )
            .replace("label = \"ac:review\"", "label = \"review\"")
            .replace("max_rounds = 2", "max_rounds = 0");
        let message = problems_of(&text);
        assert!(message.contains("deploy"), "{message}");
        assert!(message.contains("ac: で始めて"), "{message}");
        assert!(message.contains("max_rounds"), "{message}");
    }

    #[test]
    fn rejects_duplicate_names_and_labels() {
        let text = VALID
            .replace("name = \"review\"", "name = \"triage\"")
            .replace("label = \"ac:review\"", "label = \"ac:triage\"");
        let message = problems_of(&text);
        assert!(message.contains("cellの名前「triage」が重複"), "{message}");
        assert!(message.contains("ラベル「ac:triage」を複数"), "{message}");
    }

    #[test]
    fn rejects_system_labels_for_cells() {
        let text = VALID.replace("label = \"ac:review\"", "label = \"ac:working\"");
        assert!(problems_of(&text).contains("agentcellsが使うラベル"));
    }

    #[test]
    fn rejects_missing_prompt_file() {
        let dir = base_with_prompts(&["triage.md"]);
        let err = Config::parse(VALID, dir.path()).unwrap_err();
        assert!(err.message.contains("review.md"), "{}", err.message);
        assert!(!err.message.contains("triage.md"), "{}", err.message);
    }

    #[test]
    fn rejects_empty_repos_and_bad_names() {
        let text = VALID
            .replace("repos = [\"y-hirakaw/agentcells\"]", "repos = []")
            .replace("name = \"agentcells\"", "name = \"Agent Cells\"");
        let message = problems_of(&text);
        assert!(message.contains("repos が空"), "{message}");
        assert!(message.contains("使えない文字"), "{message}");
    }

    #[test]
    fn load_reports_missing_file_with_setup_hint() {
        let dir = tempfile::tempdir().unwrap();
        let err = Config::load(&dir.path().join("config.toml")).unwrap_err();
        assert!(err.hint.unwrap().contains("agentcells setup"));
    }
}
