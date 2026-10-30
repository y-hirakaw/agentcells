//! 最近の実行の結果。`status` と `dashboard` で、cellがどこへ進めたか、人間に回した理由、失敗、時間切れを見せる。
//!
//! tickの記録（[`crate::record`]）の `ran` と `failed` から読む。手元のファイルだけを読み、GitHubは読まない。
//! 人間に回った理由を知るのに、`tick.log` を開いたり、GitHubのコメントをたどったりしなくて済むように。

use crate::home::Home;
use crate::record::{self, Event, Subject};
use crate::status::Line;
use crate::transition::{DONE, NEEDS_HUMAN, STOP};

/// 理由やエラーを、何字まで出すか。
const REASON_LIMIT: usize = 80;

/// 1回の実行。
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// 終わった時刻（UNIXの秒）
    pub at: u64,
    pub subject: Subject,
    /// かかった秒
    pub seconds: u64,
    pub outcome: Outcome,
}

/// どうなったか。
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// 次のcellか、`done`、`stop` に進めた
    Moved { destination: String },
    /// 人間に回した。`requested` はAIが選んだ先（CLIが替えたときは違う）、`reason` はCLIが回した理由、
    /// `failure` はAIの実行が失敗したときの種類
    Human {
        requested: Option<String>,
        reason: Option<String>,
        failure: Option<String>,
    },
    /// 始める直前の確かめで見送った
    Skipped { reason: Option<String> },
    /// 使用量の上限に当たったので、ラベルを戻した（上限が戻ったら、また動く）
    Paused,
    /// 途中でエラーになった（tickも止まった）
    Failed { error: String },
}

/// 記録を読み、実行を古い順に返す。
pub fn read(home: &Home) -> Vec<Run> {
    runs(record::read(home))
}

fn runs(lines: Vec<record::Line>) -> Vec<Run> {
    lines
        .into_iter()
        .filter_map(|line| {
            let (subject, seconds, outcome) = match line.event {
                Event::Ran {
                    subject,
                    seconds,
                    requested,
                    destination,
                    reason,
                    failure,
                    ..
                } => {
                    let outcome = match destination.as_str() {
                        NEEDS_HUMAN => Outcome::Human {
                            requested,
                            reason,
                            failure,
                        },
                        "skipped" => Outcome::Skipped { reason },
                        "paused" => Outcome::Paused,
                        _ => Outcome::Moved { destination },
                    };
                    (subject, seconds, outcome)
                }
                Event::Failed {
                    subject,
                    seconds,
                    error,
                    ..
                } => (subject, seconds, Outcome::Failed { error }),
                _ => return None,
            };
            Some(Run {
                at: line.at,
                subject,
                seconds,
                outcome,
            })
        })
        .collect()
}

/// その行のIssueかPRで、最後に動いた実行。
pub fn last_for<'a>(runs: &'a [Run], line: &Line) -> Option<&'a Run> {
    runs.iter().rev().find(|run| {
        [&line.issue, &line.pr].into_iter().flatten().any(|item| {
            item.number == run.subject.number
                && item
                    .repo
                    .to_string()
                    .eq_ignore_ascii_case(&run.subject.repo)
        })
    })
}

/// 新しい順に `count` 件。
pub fn latest(runs: &[Run], count: usize) -> impl Iterator<Item = &Run> {
    runs.iter().rev().take(count)
}

impl Run {
    /// どうなったかを、1行で書く（`implement → review` など）。理由やエラーは、AIや gh が書いた文が入りうるので、
    /// 制御文字を置き換えて短くする。
    pub fn result(&self) -> String {
        let cell = &self.subject.cell;
        let text = match &self.outcome {
            Outcome::Moved { destination } => match destination.as_str() {
                DONE => format!("{cell} → done"),
                STOP => format!("{cell} → stop（止めた）"),
                next => format!("{cell} → {next}"),
            },
            Outcome::Human {
                requested,
                reason,
                failure,
            } => {
                let why = match (failure.as_deref(), reason) {
                    (Some("timeout"), _) => "時間切れ".to_string(),
                    (Some(kind), Some(reason)) => {
                        format!("AIの実行が失敗（{kind}）: {}", shorten(reason))
                    }
                    (Some(kind), None) => format!("AIの実行が失敗（{kind}）"),
                    (None, Some(reason)) => format!("CLIが回した: {}", shorten(reason)),
                    (None, None) if requested.as_deref() == Some(NEEDS_HUMAN) => {
                        "AIが選んだ。理由はコメントに".to_string()
                    }
                    (None, None) => "CLIが回した".to_string(),
                };
                format!("{cell} → 人間に回した（{why}）")
            }
            Outcome::Skipped { reason } => match reason {
                Some(reason) => format!("{cell} を見送った: {}", shorten(reason)),
                None => format!("{cell} を見送った"),
            },
            Outcome::Paused => format!("{cell} は使用量の上限で止め、ラベルを戻した"),
            Outcome::Failed { error } => {
                format!("{cell} がエラーで止まった: {}", shorten(error))
            }
        };
        crate::display::printable(&text)
    }

    /// 最近の実行の一覧の1行。`now` は今の時刻（UNIXの秒）。
    pub fn describe(&self, now: u64) -> String {
        let s = &self.subject;
        let kind = if s.kind == "pull_request" { "PR " } else { "" };
        format!(
            "{}前  {kind}{}#{}  {}（{}）",
            crate::status::rough_elapsed(now.saturating_sub(self.at)),
            s.repo,
            s.number,
            self.result(),
            crate::running::elapsed(self.seconds)
        )
    }

    /// Issue/PRの行の下に添える1行。
    pub fn describe_last(&self, now: u64) -> String {
        format!(
            "前回（{}前）: {}",
            crate::status::rough_elapsed(now.saturating_sub(self.at)),
            self.result()
        )
    }
}

/// 1行で読める長さにする（改行は空白にする）。
fn shorten(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= REASON_LIMIT {
        return s;
    }
    let cut: String = s.chars().take(REASON_LIMIT - 1).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::RepoName;
    use crate::status::{Item, State};

    fn subject(cell: &str, kind: &str, repo: &str, number: u64) -> Subject {
        Subject {
            product: "app".to_string(),
            cell: cell.to_string(),
            kind: kind.to_string(),
            repo: repo.to_string(),
            number,
            reply: None,
        }
    }

    fn ran(at: u64, subject: Subject, destination: &str) -> record::Line {
        record::Line {
            at,
            tick: "t".to_string(),
            event: Event::Ran {
                subject,
                started: at - 90,
                seconds: 90,
                round: 1,
                requested: Some(destination.to_string()),
                destination: destination.to_string(),
                pr: None,
                reason: None,
                ai_failed: false,
                failure: None,
                model: None,
                models: Vec::new(),
                turns: None,
                cost_usd: None,
            },
        }
    }

    fn with(line: record::Line, f: impl FnOnce(&mut Event)) -> record::Line {
        let mut line = line;
        f(&mut line.event);
        line
    }

    fn result(line: record::Line) -> String {
        runs(vec![line])[0].result()
    }

    #[test]
    fn tells_where_the_cell_went_and_why_it_went_to_a_human() {
        let issue = || subject("implement", "issue", "octo/app", 3);
        assert_eq!(result(ran(1000, issue(), "review")), "implement → review");
        assert_eq!(result(ran(1000, issue(), "done")), "implement → done");
        assert_eq!(
            result(ran(1000, issue(), NEEDS_HUMAN)),
            "implement → 人間に回した（AIが選んだ。理由はコメントに）"
        );
        let timeout = with(ran(1000, issue(), NEEDS_HUMAN), |e| {
            if let Event::Ran {
                requested,
                failure,
                reason,
                ai_failed,
                ..
            } = e
            {
                *requested = None;
                *failure = Some("timeout".to_string());
                *reason = Some("時間の上限（30分）を超えたので、止めました".to_string());
                *ai_failed = true;
            }
        });
        assert_eq!(result(timeout), "implement → 人間に回した（時間切れ）");
        let overridden = with(ran(1000, issue(), NEEDS_HUMAN), |e| {
            if let Event::Ran {
                requested, reason, ..
            } = e
            {
                *requested = Some("review".to_string());
                *reason = Some(format!("新しいコミットが\nありません{}", "あ".repeat(100)));
            }
        });
        let text = result(overridden);
        assert!(
            text.starts_with("implement → 人間に回した（CLIが回した: 新しいコミットが ありません"),
            "{text}"
        );
        assert!(text.ends_with("あ…）"), "{text}");
        let failed = record::Line {
            at: 1000,
            tick: "t".to_string(),
            event: Event::Failed {
                subject: issue(),
                started: 900,
                seconds: 100,
                error: "gh が\u{1b}[31m失敗".to_string(),
            },
        };
        let text = result(failed);
        assert!(
            text.starts_with("implement がエラーで止まった: gh が"),
            "{text}"
        );
        assert!(!text.contains('\u{1b}'), "{text}");
    }

    #[test]
    fn finds_the_last_run_of_a_line() {
        let all = runs(vec![
            ran(
                1000,
                subject("triage", "issue", "octo/issues", 7),
                "implement",
            ),
            ran(
                2000,
                subject("implement", "issue", "octo/issues", 7),
                "review",
            ),
            ran(
                3000,
                subject("review", "pull_request", "Octo/App", 14),
                NEEDS_HUMAN,
            ),
            ran(4000, subject("triage", "issue", "octo/issues", 8), "done"),
        ]);
        let item = |repo: &str, number| Item {
            repo: RepoName::parse(repo).unwrap(),
            number,
            title: "t".to_string(),
        };
        let line = |issue: Option<Item>, pr: Option<Item>| Line {
            state: State::NeedsHuman,
            issue,
            pr,
            since: None,
            note: None,
        };
        // PRの実行のほうが新しければ、それ
        let both = line(Some(item("octo/issues", 7)), Some(item("octo/app", 14)));
        assert_eq!(last_for(&all, &both).unwrap().at, 3000);
        let issue = line(Some(item("octo/issues", 7)), None);
        assert_eq!(last_for(&all, &issue).unwrap().at, 2000);
        assert!(last_for(&all, &line(Some(item("octo/issues", 9)), None)).is_none());
        // 最近の一覧は新しい順
        let at: Vec<u64> = latest(&all, 2).map(|r| r.at).collect();
        assert_eq!(at, [4000, 3000]);
        assert_eq!(
            all[2].describe(3000 + 300),
            "5分前  PR Octo/App#14  review → 人間に回した（AIが選んだ。理由はコメントに）（1分30秒）"
        );
        assert_eq!(
            all[1].describe_last(2000 + 30),
            "前回（1分足らず前）: implement → review"
        );
        // tickの始まりなど、実行でない記録は読まない
        let started = record::Line {
            at: 1,
            tick: "t".to_string(),
            event: Event::Started {
                version: "0".to_string(),
                max_minutes: 1,
            },
        };
        assert!(runs(vec![started]).is_empty());
    }
}
