//! 動いているcellの様子。`agentcells status` で見せる。
//!
//! cellが動いている間、`~/.agentcells/running.json` に、どのcellがどのIssue/PRで動いているかと、
//! claudeの出力（`stream-json`）を書いているファイルを置く。終わったら消す。
//! 出力から、AIが今どのツールを使っているかを読む。GitHubは読まないので、何度見てもよい。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::home::Home;
use crate::run::Leftover;

/// 動いているcell。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Running {
    pub cell: String,
    /// `Issue` か `PR`
    pub kind: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    /// 始まった時刻（UNIXの秒）
    pub started: u64,
    /// claudeの出力（`claude.jsonl`）
    pub log: PathBuf,
    /// 動かしているtick。これが動いていなければ、tickが途中で止まった
    pub tick: Option<Leftover>,
    /// claudeが終わったあとの段階（結果をGitHubに書いているなど）。claudeが動いている間は `None`
    #[serde(default)]
    pub phase: Option<String>,
}

fn path(home: &Home) -> PathBuf {
    home.root().join("running.json")
}

/// 動き始めたことを書く。書けなくても、cellは止めない。
pub fn start(home: &Home, running: &Running) {
    if let Ok(text) = serde_json::to_string_pretty(running) {
        let path = path(home);
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// 終わったら消す。
pub fn finish(home: &Home) {
    let _ = std::fs::remove_file(path(home));
}

/// claudeが終わったあとの段階を書く。
pub fn set_phase(home: &Home, phase: &str) {
    if let Some((mut running, _)) = load(home) {
        running.phase = Some(phase.to_string());
        start(home, &running);
    }
}

/// 抜けるときに（失敗で途中で返っても）、動いている印を消す。
pub struct Guard<'a>(pub &'a Home);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        finish(self.0);
    }
}

/// 動いているcellと、それを動かしているtickがまだ動いているか。
pub fn load(home: &Home) -> Option<(Running, bool)> {
    let text = std::fs::read_to_string(path(home)).ok()?;
    let running: Running = serde_json::from_str(&text).ok()?;
    let alive = running
        .tick
        .as_ref()
        .is_some_and(crate::run::is_alive_leftover);
    Some((running, alive))
}

/// AIがしたことの1つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// ツールを使った（名前と、何に使ったか）
    Tool { name: String, detail: String },
    /// AIが書いた文（最初の行）
    Text(String),
    /// 終わった
    Done,
}

impl Event {
    /// AIが書いたものなので、制御文字は置き換える。
    pub fn describe(&self) -> String {
        crate::display::printable(&self.describe_raw())
    }

    fn describe_raw(&self) -> String {
        match self {
            Event::Tool { name, detail } if detail.is_empty() => name.clone(),
            Event::Tool { name, detail } => format!("{name}  {detail}"),
            Event::Text(text) => format!("（発言）{text}"),
            Event::Done => "（終わり）".to_string(),
        }
    }
}

/// claudeの出力から、AIがしたことを順に読む。読めない行は飛ばす。
pub fn events(log: &Path) -> Vec<Event> {
    let Ok(text) = std::fs::read_to_string(log) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for line in text.lines() {
        // ツールの結果（大きいことがある）は読まない
        if !line.contains(r#""type":"assistant""#) && !line.contains(r#""type":"result""#) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match value["type"].as_str() {
            Some("result") => events.push(Event::Done),
            Some("assistant") => {
                for content in value["message"]["content"].as_array().into_iter().flatten() {
                    match content["type"].as_str() {
                        Some("tool_use") => {
                            let name = content["name"].as_str().unwrap_or("?").to_string();
                            let detail = tool_detail(&name, &content["input"]);
                            events.push(Event::Tool { name, detail });
                        }
                        Some("text") => {
                            let first = content["text"]
                                .as_str()
                                .unwrap_or("")
                                .lines()
                                .find(|l| !l.trim().is_empty())
                                .unwrap_or("")
                                .trim();
                            if !first.is_empty() {
                                events.push(Event::Text(shorten(first)));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    events
}

/// ツールを何に使ったか。ツールごとに、いちばんわかりやすい引数を出す。
fn tool_detail(name: &str, input: &serde_json::Value) -> String {
    let field = |key: &str| input[key].as_str().map(str::to_string);
    let detail = match name {
        "Bash" => field("command").map(|c| c.lines().next().unwrap_or("").to_string()),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            field("file_path").or_else(|| field("notebook_path"))
        }
        "Grep" | "Glob" => field("pattern"),
        "Task" | "Agent" => field("description"),
        "WebFetch" => field("url"),
        "WebSearch" => field("query"),
        "TodoWrite" => Some("TODOを更新".to_string()),
        "StructuredOutput" => Some("結果を返す".to_string()),
        _ => None,
    };
    detail.map(|d| shorten(&d)).unwrap_or_default()
}

/// 長い文を、1行で読める長さにする。AIが書いた文なので、制御文字は置き換える。
fn shorten(s: &str) -> String {
    const MAX: usize = 100;
    let s = &crate::display::printable(s);
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX - 1).collect();
    format!("{cut}…")
}

/// 経過した長さ。
pub fn elapsed(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}時間{m}分")
    } else if m > 0 {
        format!("{m}分{s}秒")
    } else {
        format!("{s}秒")
    }
}

/// 動いているcellの様子を、何行かで書く。`recent` は、最近のしたことを何件出すか。`now` はUNIXの秒。
pub fn describe(home: &Home, now: u64, recent: usize) -> Vec<String> {
    let Some((running, alive)) = load(home) else {
        return vec!["動いているcellはありません".to_string()];
    };
    describe_running(&running, alive, now, recent)
}

/// [`describe`] の、動いているcellがあるときの中身。`alive` は、動かしているtickがまだ動いているか。
pub fn describe_running(running: &Running, alive: bool, now: u64, recent: usize) -> Vec<String> {
    let events = events(&running.log);
    // 結果を返すための呼び出しは、作業ではないので数えない
    let tools = events
        .iter()
        .filter(|e| matches!(e, Event::Tool { name, .. } if name != "StructuredOutput"))
        .count();
    let quiet = std::fs::metadata(&running.log)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| format!("、最後の出力は{}前", elapsed(d.as_secs())))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "{}  {} {}#{}「{}」  {}{}（ツール {tools}回{quiet}）",
        running.cell,
        running.kind,
        running.repo,
        running.number,
        crate::display::printable(&running.title),
        elapsed(now.saturating_sub(running.started)),
        running
            .phase
            .as_deref()
            .map(|p| format!("  {p}"))
            .unwrap_or_default(),
    )];
    if !alive {
        lines.push(
            "  ※動かしていたtickが止まっています。claudeが残っていれば、終わるまで次のtickは動きません（`agentcells doctor`）"
                .to_string(),
        );
    }
    let skip = events.len().saturating_sub(recent);
    lines.extend(events[skip..].iter().map(|e| format!("  {}", e.describe())));
    lines.push(format!("  記録: {}", running.log.display()));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[{"type":"text","text":"\n読みます\n続き"},{"type":"tool_use","name":"Read","input":{"file_path":"/w/src/main.rs"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"\"type\":\"assistant\" と書いてあるファイル"}]}}
{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"..."},{"type":"tool_use","name":"Bash","input":{"command":"cargo test\necho ok"}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Unknown","input":{}}]}}
壊れた行 "type":"assistant"
{"type":"result","subtype":"success","structured_output":{"next":"done"}}
"#;

    #[test]
    fn reads_what_the_ai_did_from_the_stream() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("claude.jsonl");
        std::fs::write(&log, LOG).unwrap();
        let described: Vec<String> = events(&log).iter().map(Event::describe).collect();
        assert_eq!(
            described,
            [
                "（発言）読みます",
                "Read  /w/src/main.rs",
                "Bash  cargo test",
                "Unknown",
                "（終わり）"
            ]
        );
    }

    #[test]
    fn shows_the_running_cell_until_it_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().to_path_buf());
        assert_eq!(describe(&home, 0, 3), ["動いているcellはありません"]);

        let log = dir.path().join("claude.jsonl");
        std::fs::write(&log, LOG).unwrap();
        let running = Running {
            cell: "implement".into(),
            kind: "Issue".into(),
            repo: "octo/app".into(),
            number: 3,
            title: "直す".into(),
            started: 1000,
            log: log.clone(),
            tick: None,
            phase: None,
        };
        start(&home, &running);
        let lines = describe(&home, 1000 + 65, 2);
        assert!(
            lines[0].starts_with(
                "implement  Issue octo/app#3「直す」  1分5秒（ツール 3回、最後の出力は"
            ),
            "{lines:?}"
        );
        // tickが記録されていない（止まった）ことを知らせる
        assert!(lines[1].contains("tickが止まっています"), "{lines:?}");
        assert_eq!(lines[2..4], ["  Unknown", "  （終わり）"]);
        set_phase(&home, "結果を書いています");
        assert!(describe(&home, 1065, 0)[0].contains("1分5秒  結果を書いています（"));
        {
            let _guard = Guard(&home);
        }
        assert_eq!(describe(&home, 0, 3), ["動いているcellはありません"]);
        start(&home, &running);
        finish(&home);
        assert_eq!(describe(&home, 0, 3), ["動いているcellはありません"]);
    }

    #[test]
    fn control_characters_from_the_ai_are_not_printed() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("claude.jsonl");
        std::fs::write(
            &log,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo \u001b]52;c;aGk=\u0007"}}]}}"#,
        )
        .unwrap();
        let described = events(&log)[0].describe();
        assert!(
            !described.contains('\u{1b}') && !described.contains('\u{7}'),
            "{described:?}"
        );
        assert!(described.contains("]52;c;aGk="), "{described:?}");
    }

    #[test]
    fn shortens_long_details_and_formats_elapsed_time() {
        assert_eq!(shorten(&"あ".repeat(120)).chars().count(), 100);
        assert_eq!(elapsed(5), "5秒");
        assert_eq!(elapsed(3725), "1時間2分");
    }
}
