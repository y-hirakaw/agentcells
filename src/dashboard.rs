//! `agentcells dashboard`。productごとに、扱っているIssue/PRの状態を並べ、動いているcellの様子を、
//! そのIssue/PRの行の下に広げて出す。画面を消して書き直し続ける（読むだけ）。
//!
//! 動いているcellの様子は手元のファイル（[`crate::running`]）から読むので、2秒ごとに読み直す。
//! GitHubは、読む回数を抑えるため、決めた間隔ごとに読み直す（[`crate::status`] と同じものを読む）。

use crate::running::{self, Running};
use crate::status::{Line, Report, State};

/// GitHubから読んだ、productごとの状態。
#[derive(Debug, Default)]
pub struct Snapshot {
    /// productの名前と、その状態
    pub products: Vec<(String, Report)>,
    /// 最後に読めた時刻（UNIXの秒）。まだ読めていなければ `None`
    pub read_at: Option<u64>,
    /// 最後に読もうとして、設定などが読めなかったときのエラー
    pub error: Option<String>,
}

/// 動いているcellの、最近のしたことを何件出すか。
const RECENT: usize = 10;

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
        (_, Some(error)) => out.push(format!("エラー: GitHubを読めません: {error}")),
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
    // 動いているcellの行が、どこかのproductにあれば、その下に広げる。なければ上にまとめて出す
    let placed = detail.as_ref().is_some_and(|(r, _)| {
        snapshot
            .products
            .iter()
            .any(|(_, report)| report.lines.iter().any(|l| runs_on(l, r)))
    });
    if !placed {
        out.push(String::new());
        out.push("動いているcell".to_string());
        match &detail {
            Some((_, lines)) => out.extend(lines.iter().map(|l| format!("  {l}"))),
            None => out.push("  なし".to_string()),
        }
    }

    if snapshot.read_at.is_some() && snapshot.products.is_empty() {
        out.push(String::new());
        out.push("productがないので、扱っているIssueはありません".to_string());
    }
    for (name, report) in &snapshot.products {
        out.push(String::new());
        out.push(format!("■ {name}  {}", summary(report)));
        for error in &report.errors {
            out.push(format!("  エラー: {error}"));
        }
        for note in &report.notes {
            out.push(format!("  注意: {note}"));
        }
        for line in &report.lines {
            let detail = detail.as_ref().filter(|(r, _)| runs_on(line, r));
            let mark = if detail.is_some() { "▶" } else { " " };
            out.push(format!(" {mark}{}", line.describe(now)));
            // 同じIssueが2行に出ても、広げるのは作業中の行だけ
            if let Some((_, lines)) = detail
                && matches!(line.state, State::Working { .. })
            {
                out.extend(lines.iter().map(|l| format!("      {l}")));
            }
        }
    }
    out
}

/// 状態ごとの件数を、1行で書く。
fn summary(report: &Report) -> String {
    if report.lines.is_empty() {
        return "扱っているIssue/PRはありません".to_string();
    }
    let count = |f: fn(&State) -> bool| report.lines.iter().filter(|l| f(&l.state)).count();
    let counts = [
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

/// その行のIssueかPRで、cellが動いているか。IssueとPRは、同じリポジトリで番号を分け合うので、番号で見る。
fn runs_on(line: &Line, running: &Running) -> bool {
    [&line.issue, &line.pr].into_iter().flatten().any(|item| {
        item.number == running.number && item.repo.to_string().eq_ignore_ascii_case(&running.repo)
    })
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
            error: None,
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
            "エラー: GitHubを読めません: config.toml がありません"
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
