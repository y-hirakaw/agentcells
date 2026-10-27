//! 使用量の上限。
//!
//! claudeの出力（`stream-json`）には、`rate_limit_event` の行があり、上限の状態（`status`）と、戻る時刻
//! （`resetsAt`、UNIXの秒）が入る。上限に当たったら（`status` が `rejected`）、戻る時刻を
//! `~/.agentcells/pause.json` に書き、それまでtickは新しく始めない。続けて動かすと、たまっていた仕事が
//! 次々に同じように失敗して、人間に回ってしまうため。
//!
//! 上限ごと（5時間、7日）の使用量（`unifiedWindows` の `utilization`。0〜1）も入る。設定（`[usage]` の
//! `pause_at`）を超えたら、上限に当たる前に、同じように止まる。使う人が自分でClaude Codeを使う分を残すため。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::home::Home;

/// AIの実行が失敗して、上限の印がないとき（ログインの切れなど）に止まる長さ。
pub const AFTER_FAILURE: u64 = 30 * 60;

/// claudeの出力にあった、最後の上限の状態。
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimit {
    /// `allowed`、`allowed_warning`、`rejected` など
    pub status: String,
    /// 戻る時刻（UNIXの秒）
    pub resets_at: Option<u64>,
    /// どの上限か（`five_hour`、`seven_day` など）
    pub kind: Option<String>,
    /// 上限ごとの使用量
    pub windows: Vec<Window>,
}

/// 1つの上限（5時間、7日など）の使用量。
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub kind: String,
    /// 使った割合（0〜1）
    pub utilization: f64,
    /// 戻る時刻（UNIXの秒）
    pub resets_at: Option<u64>,
}

impl Window {
    /// 使った割合（%）。浮動小数のずれ（0.57 * 100 が 56.99...）で、ちょうどの値を取りこぼさないように丸める
    fn percent(&self) -> f64 {
        (self.utilization * 100.0).round()
    }
}

impl RateLimit {
    /// 上限に当たったか。当たったときの値は `rejected` だが、確かめられていないので、`allowed` で始まらない
    /// ものはすべて当たったとみなす（外れても、人間に回さずに待つだけで、危ない側には倒れない）。
    pub fn rejected(&self) -> bool {
        !self.status.starts_with("allowed")
    }

    /// 使用量が `percent`（%）以上になった上限のうち、いちばん遅く戻るもの。
    pub fn over(&self, percent: u8) -> Option<&Window> {
        self.windows
            .iter()
            .filter(|w| w.percent() >= f64::from(percent))
            .max_by_key(|w| w.resets_at.unwrap_or(u64::MAX))
    }
}

fn seconds(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_f64().map(|f| f as u64))
}

/// 止まる時刻を、5分後から8日後（週の上限より少し長い）の間に収める。戻る時刻が過去（時計のずれ）なら、
/// すぐにまた上限に当たらないように、遠すぎれば（ミリ秒など）、黙って止まり続けないように。
pub fn until(resets_at: Option<u64>, now: u64) -> u64 {
    resets_at
        .unwrap_or(now + AFTER_FAILURE)
        .clamp(now + 5 * 60, now + 8 * 24 * 3600)
}

/// claudeの出力（`claude.jsonl`）から、最後の上限の状態を読む。
pub fn last_event(log: &Path) -> Option<RateLimit> {
    let text = std::fs::read_to_string(log).ok()?;
    text.lines()
        .rev()
        .filter(|line| line.contains(r#""type":"rate_limit_event""#))
        .find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            let info = &value["rate_limit_info"];
            let windows = info["unifiedWindows"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(kind, w)| {
                    Some(Window {
                        kind: kind.clone(),
                        utilization: w["utilization"].as_f64()?,
                        resets_at: seconds(&w["resetsAt"]),
                    })
                })
                .collect();
            Some(RateLimit {
                status: info["status"].as_str()?.to_string(),
                resets_at: seconds(&info["resetsAt"]),
                kind: info["rateLimitType"].as_str().map(str::to_string),
                windows,
            })
        })
}

/// 使用量が設定（`[usage]` の `pause_at`）を超えた上限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Near {
    /// どの上限か（`five_hour`、`seven_day` など。英小文字・数字・_ だけ）
    pub kind: String,
    /// 使った割合（%）
    pub percent: u8,
    /// 止まる時刻（UNIXの秒。[`until`] で収めたもの）
    pub until: u64,
}

impl Near {
    /// 止まる理由（「〜のため」に続く形）。
    pub fn reason(&self) -> String {
        format!("使用量（{}）が{}%", self.kind, self.percent)
    }
}

/// 使用量が `pause_at`（%）以上になった上限があれば、それが戻るまで止まる時刻。
pub fn near(limit: &RateLimit, pause_at: u8, now: u64) -> Option<Near> {
    let window = limit.over(pause_at)?;
    Some(Near {
        kind: window
            .kind
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
            .take(40)
            .collect(),
        percent: window.percent().clamp(0.0, 255.0) as u8,
        until: until(window.resets_at, now),
    })
}

/// 新しく始めないでいる間。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pause {
    /// この時刻（UNIXの秒）まで
    pub until: u64,
    pub reason: String,
}

fn path(home: &Home) -> PathBuf {
    home.root().join("pause.json")
}

/// 止まっている間なら、その中身。過ぎていれば `None`。
pub fn active(home: &Home, now: u64) -> Option<Pause> {
    let text = std::fs::read_to_string(path(home)).ok()?;
    let pause: Pause = serde_json::from_str(&text).ok()?;
    (pause.until > now).then_some(pause)
}

/// `until` まで止まる。書けなくても、tickは止めない（そのtickは、もう続けない）。
pub fn pause(home: &Home, until: u64, reason: &str) {
    let pause = Pause {
        until,
        reason: reason.to_string(),
    };
    if let Ok(text) = serde_json::to_string_pretty(&pause) {
        let _ = std::fs::write(path(home), text);
    }
}

/// AIの実行が失敗したとき、しばらく止まるか。時間切れや、決まった形で返さなかったことは、その仕事だけの
/// ことが多いので止まらない。claudeが失敗を返したとき（ログインの切れなど）と、出力を読めないときは止まる。
pub fn pauses_after(failure: Option<&str>) -> bool {
    failure.is_some_and(|f| {
        f.starts_with("rate_limit") || f.starts_with("error:") || f == "unreadable"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_last_rate_limit_event() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("claude.jsonl");
        assert_eq!(last_event(&log), None);
        std::fs::write(
            &log,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":100,"rateLimitType":"five_hour"}}
{"type":"assistant","message":{"content":[{"type":"text","text":"\"type\":\"rate_limit_event\""}]}}
{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":200,"rateLimitType":"five_hour"}}
{"type":"result","is_error":true}
"#,
        )
        .unwrap();
        let limit = last_event(&log).unwrap();
        assert!(limit.rejected());
        assert_eq!(limit.resets_at, Some(200));
        assert_eq!(limit.kind.as_deref(), Some("five_hour"));
    }

    #[test]
    fn reads_the_usage_of_each_window() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("claude.jsonl");
        // 実際の出力（2.1.311）と同じ形
        std::fs::write(
            &log,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":100,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.83,"resetsAt":100,"status":"allowed"},"seven_day":{"utilization":0.13,"resetsAt":900.0,"status":"allowed"}}}}
"#,
        )
        .unwrap();
        let limit = last_event(&log).unwrap();
        assert!(!limit.rejected());
        assert_eq!(
            limit.windows,
            vec![
                Window {
                    kind: "five_hour".into(),
                    utilization: 0.83,
                    resets_at: Some(100)
                },
                Window {
                    kind: "seven_day".into(),
                    utilization: 0.13,
                    resets_at: Some(900)
                },
            ]
        );
        assert_eq!(limit.over(80).map(|w| w.kind.as_str()), Some("five_hour"));
        assert_eq!(limit.over(84), None);
        // 両方超えたら、遅く戻るほうまで止まる
        assert_eq!(limit.over(10).map(|w| w.kind.as_str()), Some("seven_day"));
        assert_eq!(limit.over(100), None);
        assert_eq!(
            near(&limit, 80, 0),
            Some(Near {
                kind: "five_hour".into(),
                percent: 83,
                until: 5 * 60
            })
        );
        assert_eq!(
            near(&limit, 80, 0).unwrap().reason(),
            "使用量（five_hour）が83%"
        );
        assert_eq!(near(&limit, 90, 0), None);
        // ちょうどの値を取りこぼさない
        let exact = RateLimit {
            status: "allowed".into(),
            resets_at: None,
            kind: None,
            windows: vec![Window {
                kind: "five_hour".into(),
                utilization: 0.57,
                resets_at: Some(1000),
            }],
        };
        assert_eq!(near(&exact, 57, 0).map(|n| n.percent), Some(57));
    }

    #[test]
    fn statuses_other_than_allowed_count_as_limited() {
        let limit = |status: &str| RateLimit {
            status: status.into(),
            resets_at: None,
            kind: None,
            windows: Vec::new(),
        };
        assert!(!limit("allowed").rejected());
        assert!(!limit("allowed_warning").rejected());
        assert!(limit("rejected").rejected());
        assert!(limit("something_new").rejected());
    }

    #[test]
    fn the_pause_stays_within_sane_bounds() {
        let now = 1_000_000;
        assert_eq!(until(Some(now + 3600), now), now + 3600);
        // 過去や、すぐ先なら5分後
        assert_eq!(until(Some(now - 10), now), now + 300);
        // ミリ秒のような遠い値なら8日後
        assert_eq!(until(Some(now * 1000), now), now + 8 * 24 * 3600);
        assert_eq!(until(None, now), now + AFTER_FAILURE);
    }

    #[test]
    fn pauses_until_the_time_passes() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().to_path_buf());
        assert_eq!(active(&home, 0), None);
        pause(&home, 100, "使用量の上限");
        assert_eq!(active(&home, 99).unwrap().reason, "使用量の上限");
        assert_eq!(active(&home, 100), None);
    }

    #[test]
    fn only_failures_that_hit_every_job_pause() {
        assert!(pauses_after(Some("rate_limit:five_hour")));
        assert!(pauses_after(Some("error:success:429")));
        assert!(pauses_after(Some("unreadable")));
        assert!(!pauses_after(Some("timeout")));
        assert!(!pauses_after(Some("no_output")));
        assert!(!pauses_after(None));
    }
}
