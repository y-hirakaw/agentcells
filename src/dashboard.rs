//! `agentcells dashboard`。productごとに、扱っているIssue/PRの状態を並べ、動いているcellの様子を、
//! そのIssue/PRの行の下に広げて出す。画面を消して書き直し続ける（読むだけ）。
//!
//! 動いているcellの様子は手元のファイル（[`crate::running`]）から読むので、2秒ごとに読み直す。
//! GitHubは、読む回数を抑えるため、決めた間隔ごとに読み直す（[`crate::status`] と同じものを読む）。

use crate::recent::{self, Run};
use crate::running::{self, Running};
use crate::status::{Line, Report, State};

/// GitHubから読んだ、productごとの状態。
#[derive(Debug, Default)]
pub struct Snapshot {
    /// productの名前と、その状態
    pub products: Vec<(String, Report)>,
    /// 最後に読めた時刻（UNIXの秒）。まだ読めていなければ `None`
    pub read_at: Option<u64>,
    /// 最後に読めたものを、読み始めた時刻（UNIXのミリ秒）
    pub read_started: Option<u64>,
    /// 動いているcellが変わったことに気づいた時刻（UNIXのミリ秒）。これより前に読み始めたGitHubの状態は、
    /// 終わったcellの `ac:working` が残って見えることがあるので、止まったままかどうかを決めない
    /// （変わると読み直しを起こすので、すぐ後に読み始めたものは、ミリ秒で比べれば後になる）
    pub running_changed: Option<u64>,
    /// 最後に読もうとして、設定などが読めなかったときのエラー
    pub error: Option<String>,
    /// 最近の実行（古い順。手元の記録から、GitHubを読み直すときと、動いているcellが変わったときに読む）
    pub runs: Vec<Run>,
}

/// 動いているcellの、最近のしたことを何件出すか。
const RECENT: usize = 10;

/// 最近の実行を何件出すか。
const RECENT_RUNS: usize = 3;

/// 画面に出す行。`running` は、動いているcellと、それを動かしているtickがまだ動いているか。
/// `notices` は、tickが動かない理由など。`interval` はGitHubを読み直す間隔（秒）、`now` はUNIXの秒。
pub fn render(
    snapshot: &Snapshot,
    running: Option<(&Running, bool)>,
    notices: &[String],
    interval: u64,
    now: u64,
) -> Vec<String> {
    let mut out = vec![format!(
        "agentcells ダッシュボード（cellの様子は2秒ごと、GitHubは{interval}秒ごとに読み直します。Ctrl-Cで終わる）"
    )];
    match (snapshot.read_at, &snapshot.error) {
        (_, Some(error)) => out.push(format!("エラー: 状態を読めません: {error}")),
        (Some(at), None) => out.push(format!(
            "GitHubを最後に読んだのは{}前",
            running::elapsed(now.saturating_sub(at))
        )),
        (None, None) => out.push("GitHubを読んでいます...".to_string()),
    }
    for notice in notices {
        out.push(format!("※{notice}"));
    }

    let detail = running.map(|(r, alive)| (r, running::describe_running(r, alive, now, RECENT)));
    // GitHubを読み始めたあとに動いているcellが変わっていなければ、止まったままかを決める
    let judge_stopped = match (snapshot.read_started, snapshot.running_changed) {
        (Some(started), Some(changed)) => started > changed,
        _ => true,
    };
    // 動いているcellの行が、どこかのproductにあれば、その下に広げる。なければ上にまとめて出す
    let expand_at = detail
        .as_ref()
        .and_then(|(r, _)| expand_at(&snapshot.products, r));
    if expand_at.is_none() {
        out.push(String::new());
        out.push("動いているcell".to_string());
        match &detail {
            Some((_, lines)) => out.extend(lines.iter().map(|l| format!("  {l}"))),
            None => out.push("  なし".to_string()),
        }
    }

    if !snapshot.runs.is_empty() {
        out.push(String::new());
        out.push("最近の実行".to_string());
        out.extend(
            recent::latest(&snapshot.runs, RECENT_RUNS).map(|r| format!("  {}", r.describe(now))),
        );
    }

    if snapshot.read_at.is_some() && snapshot.products.is_empty() {
        out.push(String::new());
        out.push("productがないので、扱っているIssueはありません".to_string());
    }
    for (p, (name, report)) in snapshot.products.iter().enumerate() {
        out.push(String::new());
        let lines: Vec<Line> = report
            .lines
            .iter()
            .cloned()
            .map(|mut l| {
                if judge_stopped {
                    l.check_stopped(&Vec::from_iter(running), now);
                }
                l
            })
            .collect();
        out.push(format!("■ {name}  {}", summary(&lines)));
        for error in &report.errors {
            out.push(format!("  エラー: {error}"));
        }
        for note in &report.notes {
            out.push(format!("  注意: {note}"));
        }
        if lines
            .iter()
            .any(|l| matches!(l.state, State::Stopped { .. }))
        {
            out.push(format!("  ※{}", crate::status::STOPPED_HINT));
        }
        for (i, line) in lines.iter().enumerate() {
            let detail = detail.as_ref().filter(|(r, _)| line.runs(r));
            let mark = if detail.is_some() { "▶" } else { " " };
            out.push(format!(" {mark}{}", line.describe(now)));
            if let Some((_, lines)) = detail
                && expand_at == Some((p, i))
            {
                out.extend(lines.iter().map(|l| format!("      {l}")));
            } else if !matches!(line.state, State::Working { .. })
                && let Some(run) = recent::last_for(&snapshot.runs, line)
            {
                out.push(format!("      {}", run.describe_last(now)));
            }
        }
    }
    out
}

/// 動いているcellの様子を、どの行（productの番号、行の番号）の下に広げるか。
/// 同じIssueが2行に出ても、広げるのは1か所だけで、作業中の行を選ぶ。GitHubを読み直す前は、
/// cellが動き始めたIssue/PRが、まだ順番待ちや返事待ちのままのことがあるので、作業中の行がなければ、
/// 合った最初の行の下に広げる。
fn expand_at(products: &[(String, Report)], running: &Running) -> Option<(usize, usize)> {
    let matched = || {
        products.iter().enumerate().flat_map(|(p, (_, report))| {
            report
                .lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.runs(running))
                .map(move |(i, l)| ((p, i), l))
        })
    };
    matched()
        .find(|(_, l)| matches!(l.state, State::Working { .. }))
        .or_else(|| matched().next())
        .map(|(at, _)| at)
}

/// 状態ごとの件数を、1行で書く。
fn summary(lines: &[Line]) -> String {
    if lines.is_empty() {
        return "扱っているIssue/PRはありません".to_string();
    }
    let count = |f: fn(&State) -> bool| lines.iter().filter(|l| f(&l.state)).count();
    let counts = [
        (
            "止まったまま",
            count(|s| matches!(s, State::Stopped { .. })),
        ),
        ("作業中", count(|s| matches!(s, State::Working { .. }))),
        ("返事待ち", count(|s| matches!(s, State::NeedsHuman))),
        ("検品待ち", count(|s| matches!(s, State::InReview))),
        ("順番待ち", count(|s| matches!(s, State::Queued { .. }))),
        ("ラベルなし", count(|s| matches!(s, State::Unlabeled))),
    ];
    counts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(name, n)| format!("{name} {n}"))
        .collect::<Vec<_>>()
        .join(" / ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::RepoName;
    use crate::status::Item;

    fn item(repo: &str, number: u64, title: &str) -> Item {
        Item {
            repo: RepoName::parse(repo).unwrap(),
            number,
            title: title.to_string(),
        }
    }

    fn line(state: State, issue: Option<Item>, pr: Option<Item>) -> Line {
        Line {
            state,
            issue,
            pr,
            since: None,
            note: None,
        }
    }

    fn running(dir: &std::path::Path, repo: &str, number: u64) -> Running {
        let log = dir.join("claude.jsonl");
        std::fs::write(
            &log,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/w/src/main.rs"}}]}}"#,
        )
        .unwrap();
        Running {
            cell: "implement".into(),
            kind: "PR".into(),
            repo: repo.into(),
            number,
            title: "直す".into(),
            started: 1000,
            log,
            tick: None,
            phase: None,
        }
    }

    fn snapshot() -> Snapshot {
        let report = Report {
            lines: vec![
                line(
                    State::Working {
                        cell: Some("implement".into()),
                    },
                    Some(item("octo/issues", 7, "両方")),
                    Some(item("octo/app", 14, "直している")),
                ),
                line(
                    State::NeedsHuman,
                    Some(item("octo/issues", 7, "両方")),
                    None,
                ),
                line(
                    State::NeedsHuman,
                    Some(item("octo/issues", 2, "質問した")),
                    None,
                ),
            ],
            errors: vec!["octo/web を読めません".into()],
            notes: Vec::new(),
        };
        Snapshot {
            products: vec![("app".into(), report), ("web".into(), Report::default())],
            read_at: Some(1000),
            read_started: Some(990_000),
            running_changed: None,
            error: None,
            runs: Vec::new(),
        }
    }

    #[test]
    fn expands_the_running_cell_under_its_line() {
        let dir = tempfile::tempdir().unwrap();
        let running = running(dir.path(), "Octo/App", 14);
        let lines = render(&snapshot(), Some((&running, true)), &[], 60, 1065);
        assert_eq!(lines[1], "GitHubを最後に読んだのは1分5秒前");
        // 上にまとめては出さない
        assert!(!lines.contains(&"動いているcell".to_string()), "{lines:?}");
        let at = lines
            .iter()
            .position(|l| l == "■ app  作業中 1 / 返事待ち 2")
            .unwrap();
        assert_eq!(lines[at + 1], "  エラー: octo/web を読めません");
        assert_eq!(
            lines[at + 2],
            " ▶作業中 implement  octo/issues#7「両方」 → PR octo/app#14"
        );
        assert!(
            lines[at + 3]
                .starts_with("      implement  PR Octo/App#14「直す」  1分5秒（ツール 1回"),
            "{lines:?}"
        );
        assert_eq!(lines[at + 4], "        Read  /w/src/main.rs");
        assert!(lines[at + 5].contains("記録: "), "{lines:?}");
        // cellが動いているのはPRなので、同じIssueのPRのない行には印を付けない
        assert_eq!(lines[at + 6], "  返事待ち  octo/issues#7「両方」");
        assert_eq!(lines[at + 7], "  返事待ち  octo/issues#2「質問した」");
        assert_eq!(lines[at + 8], "");
        assert_eq!(lines[at + 9], "■ web  扱っているIssue/PRはありません");

        // 同じIssueが2行に出ても、広げるのは作業中の行だけ
        let running = Running {
            repo: "octo/issues".into(),
            number: 7,
            ..running
        };
        let lines = render(&snapshot(), Some((&running, true)), &[], 60, 1065);
        let marked: Vec<usize> = (0..lines.len())
            .filter(|&i| lines[i].starts_with(" ▶"))
            .collect();
        assert_eq!(marked.len(), 2, "{lines:?}");
        assert!(lines[marked[0] + 1].starts_with("      implement"));
        assert_eq!(lines[marked[1]], " ▶返事待ち  octo/issues#7「両方」");
        assert!(!lines[marked[1] + 1].starts_with("      "), "{lines:?}");
    }

    #[test]
    fn expands_under_a_stale_line_when_no_working_line_matches() {
        // GitHubを読み直す前で、cellが動き始めたIssueがまだ返事待ちのまま
        let dir = tempfile::tempdir().unwrap();
        let running = running(dir.path(), "octo/issues", 2);
        let lines = render(&snapshot(), Some((&running, true)), &[], 60, 1065);
        assert!(!lines.contains(&"動いているcell".to_string()), "{lines:?}");
        let at = lines
            .iter()
            .position(|l| l == " ▶返事待ち  octo/issues#2「質問した」")
            .unwrap();
        assert!(
            lines[at + 1].starts_with("      implement  PR octo/issues#2"),
            "{lines:?}"
        );

        // 作業中の行がなく、合う行が2つあれば、最初の行の下にだけ広げる
        let mut stale = snapshot();
        stale.products[0].1.lines[0].state = State::Queued {
            cell: "implement".into(),
        };
        let running = Running {
            number: 7,
            ..running
        };
        let lines = render(&stale, Some((&running, true)), &[], 60, 1065);
        let marked: Vec<usize> = (0..lines.len())
            .filter(|&i| lines[i].starts_with(" ▶"))
            .collect();
        assert_eq!(marked.len(), 2, "{lines:?}");
        assert!(
            lines[marked[0] + 1].starts_with("      implement"),
            "{lines:?}"
        );
        assert!(!lines[marked[1] + 1].starts_with("      "), "{lines:?}");
    }

    #[test]
    fn shows_the_running_cell_on_top_when_no_line_matches() {
        let dir = tempfile::tempdir().unwrap();
        let running = running(dir.path(), "octo/other", 3);
        let lines = render(
            &snapshot(),
            Some((&running, true)),
            &["使用量の上限のため".to_string()],
            60,
            1065,
        );
        assert_eq!(lines[2], "※使用量の上限のため");
        assert_eq!(lines[4], "動いているcell");
        assert!(
            lines[5].starts_with("  implement  PR octo/other#3"),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.starts_with(" ▶")), "{lines:?}");
    }

    #[test]
    fn working_lines_with_no_running_cell_are_shown_as_stopped() {
        let lines = render(&snapshot(), None, &[], 60, 1065);
        let at = lines
            .iter()
            .position(|l| l == "■ app  止まったまま 1 / 返事待ち 2")
            .unwrap();
        assert!(lines[at + 2].contains("ac:working"), "{lines:?}");
        assert_eq!(
            lines[at + 3],
            "  止まったまま implement  octo/issues#7「両方」 → PR octo/app#14"
        );

        // 読み始めたあとに、動いていたcellが終わった（GitHubの状態は古いまま）なら、作業中のまま
        let finished = Snapshot {
            running_changed: Some(995_000),
            ..snapshot()
        };
        let lines = render(&finished, None, &[], 60, 1065);
        assert!(
            lines.contains(&"■ app  作業中 1 / 返事待ち 2".to_string()),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains("ac:working")), "{lines:?}");
        // 同じ時刻なら決めない。読み直したあとなら決める
        let same = Snapshot {
            running_changed: Some(990_000),
            ..snapshot()
        };
        assert!(
            !render(&same, None, &[], 60, 1065)
                .iter()
                .any(|l| l.contains("止まったまま"))
        );
        let reread = Snapshot {
            running_changed: Some(989_999),
            ..snapshot()
        };
        assert!(
            render(&reread, None, &[], 60, 1065)
                .iter()
                .any(|l| l.contains("止まったまま"))
        );
    }

    #[test]
    fn shows_recent_runs_and_the_last_run_under_each_line() {
        let run = |at, cell: &str, kind: &str, repo: &str, number, outcome| Run {
            at,
            subject: crate::record::Subject {
                product: "app".into(),
                cell: cell.into(),
                kind: kind.into(),
                repo: repo.into(),
                number,
                reply: None,
            },
            seconds: 70,
            outcome,
        };
        let snapshot = Snapshot {
            runs: vec![
                run(
                    500,
                    "triage",
                    "issue",
                    "octo/issues",
                    2,
                    recent::Outcome::Human {
                        requested: Some("needs-human".into()),
                        reason: None,
                        failure: None,
                    },
                ),
                run(
                    900,
                    "review",
                    "pull_request",
                    "octo/app",
                    14,
                    recent::Outcome::Moved {
                        destination: "implement".into(),
                    },
                ),
            ],
            ..snapshot()
        };
        let dir = tempfile::tempdir().unwrap();
        let running = running(dir.path(), "octo/app", 14);
        let lines = render(&snapshot, Some((&running, true)), &[], 60, 1065);
        let at = lines.iter().position(|l| l == "最近の実行").unwrap();
        assert_eq!(
            lines[at + 1..at + 3],
            [
                "  2分前  PR octo/app#14  review → implement（1分10秒）",
                "  9分前  octo/issues#2  triage → 人間に回した（AIが選んだ。理由はコメントに）（1分10秒）",
            ]
        );
        // 返事待ちの行の下に前回の結果。作業中の行（動いているcellを広げる）には付けない
        let waiting = lines
            .iter()
            .position(|l| l == "  返事待ち  octo/issues#2「質問した」")
            .unwrap();
        assert_eq!(
            lines[waiting + 1],
            "      前回（9分前）: triage → 人間に回した（AIが選んだ。理由はコメントに）"
        );
        assert!(
            !lines.iter().any(|l| l.contains("前回（2分前）")),
            "{lines:?}"
        );
    }

    #[test]
    fn tells_when_github_is_not_read_yet_or_fails() {
        let lines = render(&Snapshot::default(), None, &[], 60, 0);
        assert_eq!(lines[1], "GitHubを読んでいます...");
        assert_eq!(lines[3..], ["動いているcell", "  なし"]);

        let failed = Snapshot {
            error: Some("config.toml がありません".into()),
            ..snapshot()
        };
        let lines = render(&failed, None, &[], 60, 0);
        assert_eq!(
            lines[1],
            "エラー: 状態を読めません: config.toml がありません"
        );
        // 前に読めたものは出し続ける
        assert!(lines.iter().any(|l| l.starts_with("■ app")), "{lines:?}");

        let empty = Snapshot {
            read_at: Some(0),
            ..Snapshot::default()
        };
        let lines = render(&empty, None, &[], 60, 0);
        assert_eq!(
            lines.last().unwrap(),
            "productがないので、扱っているIssueはありません"
        );
    }
}
