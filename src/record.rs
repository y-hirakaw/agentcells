//! tickが動いたことの記録。inspector（concept.md）の材料にする。
//!
//! `~/.agentcells/logs/events.jsonl` に、1行に1件のJSONで足していく。tickの出力（`tick.log`）は人が読むための
//! もので、あとから機械で数えにくいため、別に残す。手元だけに置き、リポジトリの名前と番号は書くが、
//! Issue/PRのタイトルや本文、AIの出力は書かない。書けなくても、tickは止めない。
//!
//! ただし、理由やエラー（`reason`、`error`）は自由な文で、リポジトリの中のパス、ほかの人のログイン、gh や git
//! のエラーが入りうる。公開の場所に出すときは、そのまま出さない。

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Target;
use crate::home::Home;
use crate::run::Report;
use crate::tick::Work;

/// これより大きくなったら、tickの始めに `events.jsonl.1` に回す。
const LIMIT: u64 = 5 * 1024 * 1024;

pub fn path(home: &Home) -> PathBuf {
    home.root().join("logs/events.jsonl")
}

/// 大きくなった記録を `events.jsonl.1` に回す（前の `.1` は消える）。
pub fn rotate(home: &Home) {
    let path = path(home);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LIMIT) {
        let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
    }
}

/// 1行分。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    /// 書いた時刻（UNIXの秒）
    pub at: u64,
    /// どのtickか（始めた時刻とpid）。同じtickの行をまとめるため
    pub tick: String,
    #[serde(flatten)]
    pub event: Event,
}

/// 仕事を指すもの。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub product: String,
    pub cell: String,
    /// `issue` か `pull_request`
    pub kind: String,
    pub repo: String,
    pub number: u64,
    /// 人間を待っていたところから、返事で動いたなら、待っていたラベル
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
}

impl Subject {
    pub fn of(work: &Work) -> Self {
        Subject {
            product: work.product.clone(),
            cell: work.cell.clone(),
            kind: work.item.as_str().to_string(),
            repo: work.repo.to_string(),
            number: work.number,
            reply: work.waiting.map(|w| w.label().to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// tickが始まった（ロックを取れて、動かせる状態になった）
    Started { version: String, max_minutes: u64 },
    /// 閉じたものに残ったラベルを外した
    Tidied {
        kind: String,
        repo: String,
        number: u64,
        labels: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// 動かす前の確かめで見送った（1回のtickで、同じものは1度だけ）
    Skipped {
        #[serde(flatten)]
        subject: Subject,
        reason: String,
    },
    /// cellを1回動かした
    Ran {
        #[serde(flatten)]
        subject: Subject,
        /// 始めた時刻（UNIXの秒）と、かかった秒
        started: u64,
        seconds: u64,
        round: u32,
        /// AIが選んだ先（CLIが人間に回したときは、destination と違う）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested: Option<String>,
        /// 進めた先（`skipped` は、始める直前の確かめで見送った）
        destination: String,
        /// 実行の中で作ったか、使ったPRの番号（IssueからPRに進んだとき）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pr: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        ai_failed: bool,
        /// AIの実行が失敗したときの種類（決まった語）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// claudeが実際に使ったモデル（claudeの結果の `modelUsage`）
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        models: Vec<String>,
        /// claudeが返した、やり取りの回数と費用の目安
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turns: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
    },
    /// cellを動かしている途中で、エラーで止まった（そのtickも終わる）
    Failed {
        #[serde(flatten)]
        subject: Subject,
        started: u64,
        seconds: u64,
        error: String,
    },
    /// inspectorが動いた
    Inspected {
        /// 規則で見つけた気になることの数
        findings: usize,
        /// 作ったIssueと、また起きたとコメントしたIssueの番号
        created: Vec<u64>,
        commented: Vec<u64>,
        /// 出さずに手元に残した案の数
        kept: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// tickが終わった。`why` は `no_work`、`limit`、`ai_failed`、`stuck`、`error`（`error` と `stuck` ならその中身）
    Ended {
        runs: usize,
        why: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl Event {
    pub fn ran(work: &Work, started: u64, now: u64, report: &Report) -> Self {
        Event::Ran {
            subject: Subject::of(work),
            started,
            seconds: now.saturating_sub(started),
            round: report.round,
            requested: report.requested.clone(),
            destination: report.destination.clone(),
            pr: report.pr,
            reason: report.reason.clone(),
            ai_failed: report.ai_failed,
            failure: report.failure.clone(),
            model: report.model.clone(),
            models: report.usage.models.clone(),
            turns: report.usage.turns,
            cost_usd: report.usage.cost_usd,
        }
    }

    pub fn tidied(
        kind: Target,
        repo: &str,
        number: u64,
        labels: &[String],
        error: Option<String>,
    ) -> Self {
        Event::Tidied {
            kind: kind.as_str().to_string(),
            repo: repo.to_string(),
            number,
            labels: labels.to_vec(),
            error,
        }
    }
}

/// 1回のtickの記録を書く。
pub struct Recorder {
    path: PathBuf,
    tick: String,
}

impl Recorder {
    pub fn new(home: &Home, started: u64) -> Self {
        Recorder {
            path: path(home),
            tick: format!("{started}-{}", std::process::id()),
        }
    }

    /// 1行足す。書けなくても、tickは止めない。
    pub fn write(&self, at: u64, event: Event) {
        let line = Line {
            at,
            tick: self.tick.clone(),
            event,
        };
        let _ = append(&self.path, &line);
    }
}

fn append(path: &Path, line: &Line) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
    text.push('\n');
    // 1回の書き込みで1行を足す（途中で止まっても、行が混ざらないように）
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?
        .write_all(text.as_bytes())
}

/// 記録を読む（回した `events.jsonl.1` から順に）。読めない行は飛ばす。
pub fn read(home: &Home) -> Vec<Line> {
    let path = path(home);
    [path.with_extension("jsonl.1"), path]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|text| {
            text.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect::<Vec<Line>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    use crate::home::RepoName;
    use crate::run::Usage;
    use crate::transition::Waiting;

    #[test]
    fn writes_one_json_line_per_event_and_reads_them_back() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().to_path_buf());
        let recorder = Recorder::new(&home, 100);
        let work = Work {
            product: "app".into(),
            cell: "implement".into(),
            item: Target::PullRequest,
            repo: RepoName::parse("octo/app").unwrap(),
            number: 4,
            title: "書かない題".into(),
            waiting: Some(Waiting::InReview),
        };
        let report = Report {
            destination: "review".into(),
            reason: None,
            ai_failed: false,
            round: 2,
            requested: Some("review".into()),
            failure: None,
            limited_until: None,
            pr: None,
            model: Some("sonnet".into()),
            usage: Usage {
                turns: Some(12),
                cost_usd: Some(0.5),
                models: vec!["claude-sonnet-5-5".into()],
            },
        };
        recorder.write(
            100,
            Event::Started {
                version: "0.0.0".into(),
                max_minutes: 30,
            },
        );
        recorder.write(160, Event::ran(&work, 100, 160, &report));
        recorder.write(
            161,
            Event::Ended {
                runs: 1,
                why: "no_work".into(),
                error: None,
            },
        );

        let text = std::fs::read_to_string(path(&home)).unwrap();
        assert_eq!(text.lines().count(), 3);
        // タイトルは書かない
        assert!(!text.contains("書かない題"), "{text}");
        let ran: serde_json::Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        assert_eq!(ran["event"], "ran");
        assert_eq!(ran["tick"], format!("100-{}", std::process::id()));
        assert_eq!(ran["repo"], "octo/app");
        assert_eq!(ran["kind"], "pull_request");
        assert_eq!(ran["reply"], "ac:in-review");
        assert_eq!(ran["seconds"], 60);
        assert_eq!(ran["round"], 2);
        assert_eq!(ran["turns"], 12);
        assert_eq!(ran["requested"], "review");
        assert_eq!(ran["models"][0], "claude-sonnet-5-5");
        assert!(ran.get("pr").is_none());
        assert!(ran.get("reason").is_none());

        let lines = read(&home);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].event, Event::ran(&work, 100, 160, &report));
        assert_eq!(
            std::fs::metadata(path(&home)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
