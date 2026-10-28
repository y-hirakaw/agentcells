//! inspectorの2段目（concept.md）。cellの実行の中身（claudeの出力）まで読み、1段目（記録の規則）では
//! 見つからないこと（sandboxで止められた、テストのコマンドが合っていない、同じ失敗をくり返した、など）を、
//! 手元に残す。公開の場所には出さない。使う人が聞いたら（`agentcells inspect --notes`）見せる。
//!
//! claudeの出力（`runs/<owner>/<repo>/<番号>/claude.jsonl`）は、同じIssue/PRの次の実行で上書きされる。
//! そこで、実行のたびにCLIが短い要約（使ったツール、ツールのエラー、許されなかった操作、結果）を作って
//! `inspector/digests/` に残し、2段目はそれを読む。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, Inspector};
use crate::display::printable;
use crate::error::{Error, Result};
use crate::home::Home;
use crate::probe::Probe;
use crate::tick::Work;

const SYSTEM: &str = include_str!("../prompts/inspector-notes.md");
/// 1回の実行の要約の、いちばん長い行数。長ければ、始めと終わりを残す。
const MAX_LINES: usize = 300;
/// 1行の長さ。
const MAX_LINE: usize = 300;
/// 読んだ要約と、書いたメモを残す長さ。
const KEEP: u64 = 14 * 24 * 3600;
/// 1回に読ませる要約の数。2段目が失敗し続けてたまっても、時間切れや使用量が増え続けないように。
/// 多ければ、新しいものから読み、古いものは読まずに済ませる。
const MAX_DIGESTS: usize = 50;

fn digests_dir(home: &Home) -> PathBuf {
    home.root().join("inspector/digests")
}

fn notes_dir(home: &Home) -> PathBuf {
    home.root().join("inspector/notes")
}

fn shorten(s: &str) -> String {
    let s = printable(&s.replace('\n', " "));
    if s.chars().count() <= MAX_LINE {
        return s;
    }
    let cut: String = s.chars().take(MAX_LINE - 1).collect();
    format!("{cut}…")
}

/// claudeの出力から、1回の実行の要約を作る。ツールの結果は、エラーのときだけ、始めの部分を残す。
pub fn digest(log: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log).ok()?;
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value["type"].as_str() {
            Some("assistant") => {
                for content in value["message"]["content"].as_array().into_iter().flatten() {
                    match content["type"].as_str() {
                        Some("tool_use") => {
                            let name = content["name"].as_str().unwrap_or("?");
                            let input = &content["input"];
                            // 結果を返すところは、選んだ先と、人へのコメントの始めを残す（なぜそこへ進めたかを読めるように）
                            if name == "StructuredOutput" {
                                lines.push(shorten(&format!(
                                    "[outcome] next={} comment={}",
                                    input["next"].as_str().unwrap_or("?"),
                                    input["comment"].as_str().unwrap_or("")
                                )));
                                continue;
                            }
                            let detail = ["command", "file_path", "pattern", "description", "url"]
                                .iter()
                                .find_map(|k| input[k].as_str())
                                .unwrap_or("");
                            lines.push(shorten(&format!("[tool] {name} {detail}")));
                        }
                        Some("text") => {
                            let text = content["text"].as_str().unwrap_or("").trim();
                            if !text.is_empty() {
                                lines.push(shorten(&format!("[text] {text}")));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for content in value["message"]["content"].as_array().into_iter().flatten() {
                    if content["type"] == "tool_result" && content["is_error"] == true {
                        let body = match &content["content"] {
                            Value::String(s) => s.clone(),
                            Value::Array(parts) => parts
                                .iter()
                                .filter_map(|p| p["text"].as_str())
                                .collect::<Vec<_>>()
                                .join(" "),
                            _ => String::new(),
                        };
                        lines.push(shorten(&format!("[error] {body}")));
                    }
                }
            }
            Some("result") => {
                let denied: Vec<String> = value["permission_denials"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|d| d["tool_name"].as_str().map(str::to_string))
                    .collect();
                lines.push(shorten(&format!(
                    "[result] is_error={} subtype={} turns={} seconds={} denied={}",
                    value["is_error"].as_bool().unwrap_or(false),
                    value["subtype"].as_str().unwrap_or("?"),
                    value["num_turns"].as_u64().unwrap_or(0),
                    value["duration_ms"].as_u64().unwrap_or(0) / 1000,
                    denied.join(",")
                )));
            }
            _ => {}
        }
    }
    if lines.len() > MAX_LINES {
        let omitted = lines.len() - MAX_LINES;
        let tail = lines.split_off(lines.len() - MAX_LINES * 2 / 3);
        lines.truncate(MAX_LINES / 3);
        lines.push(format!("（{omitted}行を省きました）"));
        lines.extend(tail);
    }
    Some(lines.join("\n"))
}

/// 1回の実行の要約を残す（[inspector] を書いたときだけ）。書けなくても、tickは止めない。
/// `started` より前に書かれた出力は、前の実行のもの（claudeを動かす前に止まった）なので、残さない。
pub fn keep_digest(
    home: &Home,
    work: &Work,
    destination: &str,
    round: u32,
    started: u64,
    now: u64,
) {
    let log = crate::run::claude_log(home, &work.repo, work.number);
    let written = std::fs::metadata(&log)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    // claudeは終わるときにも書くので、本当の実行なら、始めた秒より後になる
    if written.is_none_or(|at| at <= started) {
        return;
    }
    let Some(body) = digest(&log) else {
        return;
    };
    let dir = digests_dir(home);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let name = format!(
        "{now}-{}-{}-{}-{}.txt",
        work.repo.owner, work.repo.name, work.number, work.cell
    );
    let head = format!(
        "product: {}\ncell: {}\n{}: {}#{}\nround: {round}\n進めた先: {destination}\n---\n",
        work.product,
        work.cell,
        work.item.as_str(),
        work.repo,
        work.number
    );
    let _ = write_private(&dir.join(name), &format!("{head}{body}\n"));
}

/// AIが返すもの。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Notes {
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Note {
    /// どこを直すものか（`config`、`product`、`prompt`、`agentcells`）
    pub kind: String,
    pub title: String,
    pub detail: String,
    /// 直し方の案
    pub suggestion: String,
    /// 根拠にした要約のファイル名
    pub evidence: Vec<String>,
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "notes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["config", "product", "prompt", "agentcells"] },
                        "title": { "type": "string" },
                        "detail": { "type": "string" },
                        "suggestion": { "type": "string" },
                        "evidence": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["kind", "title", "detail", "suggestion", "evidence"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["notes"],
        "additionalProperties": false
    })
}

/// 設定のうち、2段目の判断に役立つところ（productのコマンドとsandbox、cellの進め先とモデル）。
/// 秘密になりうるもの（Appの鍵の場所など）は渡さない。
fn config_summary(config: &Config) -> String {
    let mut out = Vec::new();
    for product in &config.products {
        out.push(format!("[product {}]", product.name));
        out.push(format!("cells = {:?}", product.cells));
        for (name, command) in &product.commands {
            out.push(format!("commands.{name} = {command:?}"));
        }
        let sandbox = &product.sandbox;
        out.push(format!(
            "sandbox: read={:?} caches={:?} network={:?} bind_localhost={}",
            sandbox.read, sandbox.caches, sandbox.network, sandbox.bind_localhost
        ));
        for (cell, next) in &product.next {
            out.push(format!("next.{cell} = {next:?}"));
        }
    }
    for cell in &config.cells {
        out.push(format!(
            "[cell {}] on={:?} next={:?} commands={:?} web_search={} model={:?} max_rounds={} timeout_minutes={} 追加の指示={}",
            cell.name,
            cell.on.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            cell.next,
            cell.commands,
            cell.web_search,
            cell.model,
            cell.max_rounds,
            cell.timeout_minutes,
            if cell.prompt.is_some() {
                "あり"
            } else {
                "なし"
            }
        ));
    }
    out.join("\n")
}

/// 動かすのに要るもの。
pub struct Context<'a> {
    pub home: &'a Home,
    pub config: &'a Config,
    pub inspector: &'a Inspector,
    pub probe: &'a dyn Probe,
    pub claude: &'a Path,
    pub scanner: &'a Path,
    pub now: u64,
}

/// 2段目を動かす。まだ読んでいない要約があれば、AIに読ませ、見つけたことを手元に残す。
/// `reported` は、1段目で見つけたこと（目印）。同じことは書かせない。
/// 書いたメモの数と、その場所を返す（読む要約がなければ `None`）。
pub fn run(ctx: &Context, reported: &[&str]) -> Result<Option<(usize, PathBuf)>> {
    let dir = digests_dir(ctx.home);
    let read_dir = dir.join("read");
    prune(&dir, ctx.now);
    prune(&read_dir, ctx.now);
    prune(&notes_dir(ctx.home), ctx.now);
    let mut digests: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "txt"))
                .collect()
        })
        .unwrap_or_default();
    if digests.is_empty() {
        return Ok(None);
    }
    digests.sort();
    // 多すぎれば、新しいものから読む。読まないものは、読んだものと同じところへ移す
    let skipped = digests.len().saturating_sub(MAX_DIGESTS);
    let older: Vec<PathBuf> = digests.drain(..skipped).collect();

    // AIには、要約と設定の写しだけを置いた一時ディレクトリを読ませる
    let work = tempfile::Builder::new()
        .prefix("agentcells-notes-")
        .tempdir()
        .map_err(|e| Error::new(format!("一時ディレクトリを作れません: {e}")))?;
    let mut names = Vec::new();
    for path in &digests {
        let name = path.file_name().unwrap_or_default().to_owned();
        std::fs::copy(path, work.path().join(&name))
            .map_err(|e| Error::new(format!("{} を写せません: {e}", path.display())))?;
        names.push(name.to_string_lossy().to_string());
    }
    write_private(
        &work.path().join("config.txt"),
        &masked(ctx.scanner, &config_summary(ctx.config))?,
    )?;

    let run_dir = ctx
        .home
        .root()
        .join("inspector/runs")
        .join(format!("{}-notes", ctx.now));
    std::fs::create_dir_all(&run_dir)
        .map_err(|e| Error::new(format!("{} を作れません: {e}", run_dir.display())))?;
    let settings_path = run_dir.join("settings.json");
    let settings = crate::settings::reader_settings(ctx.home, ctx.config)?;
    write_private(
        &settings_path,
        &serde_json::to_string_pretty(&settings).unwrap_or_default(),
    )?;
    let prompt = format!(
        "このディレクトリに、cellの実行の要約が {} 件（{}）と、設定の写し（config.txt）があります。\n\n1段目ですでに見つけていること（書かなくてよい）: {}\n",
        names.len(),
        names.join("、"),
        if reported.is_empty() {
            "なし".to_string()
        } else {
            reported.join("、")
        }
    );
    let schema = schema();
    let reader = crate::run::Reader {
        home: ctx.home,
        claude: ctx.claude,
        cwd: work.path(),
        settings: &settings_path,
        system: SYSTEM,
        schema: &schema,
        prompt: &prompt,
        model: ctx.inspector.model.as_deref(),
        timeout: Duration::from_secs(u64::from(ctx.inspector.timeout_minutes) * 60),
        log_dir: &run_dir,
    };
    let result = reader
        .run()?
        .ok_or_else(|| Error::new("inspectorの2段目のAIが時間内に終わりませんでした"))?;
    if result["is_error"].as_bool() == Some(true) {
        return Err(Error::new(format!(
            "inspectorの2段目のAIの実行が失敗しました（{}）",
            result["subtype"].as_str().unwrap_or("理由不明")
        )));
    }
    let notes: Notes = serde_json::from_value(result["structured_output"].clone())
        .map_err(|e| Error::new(format!("inspectorの2段目のAIの結果を読めません: {e}")))?;

    let out_dir = notes_dir(ctx.home);
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| Error::new(format!("{} を作れません: {e}", out_dir.display())))?;
    let path = out_dir.join(format!("{}.md", ctx.now));
    write_private(&path, &render(&notes, &names, older.len()))?;

    // 読んだ要約は、しばらく残す（聞かれたときに、根拠をたどれるように）
    let _ = std::fs::create_dir_all(&read_dir);
    for digest in digests.iter().chain(&older) {
        if let Some(name) = digest.file_name() {
            let _ = std::fs::rename(digest, read_dir.join(name));
        }
    }
    Ok(Some((notes.notes.len(), path)))
}

/// 設定の写しのうち、秘密らしいもの（betterleaks）がある行を伏せる。コマンドに、使う人が
/// トークンを書いていることがあるため。調べられなければ、渡さない。
fn masked(scanner: &Path, text: &str) -> Result<String> {
    let found = crate::actions::scan_text(scanner, text)?;
    Ok(text
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if found.iter().any(|(n, _)| *n == i + 1) {
                "（秘密らしいものがあるので伏せました）"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// メモを、人が読める形にする。`skipped` は、多すぎて読まなかった古い要約の数。
fn render(notes: &Notes, read: &[String], skipped: usize) -> String {
    let mut out = vec![format!(
        "# inspectorのメモ（実行 {} 件を読んだ）",
        read.len()
    )];
    if skipped > 0 {
        out.push(format!(
            "\n多すぎたので、古い実行 {skipped} 件の要約は読みませんでした。"
        ));
    }
    if notes.notes.is_empty() {
        out.push("\n気になることはありませんでした。".to_string());
    }
    for note in &notes.notes {
        let kind = match note.kind.as_str() {
            "config" => "設定",
            "product" => "product（作業するリポジトリ）",
            "prompt" => "プロンプト",
            _ => "agentcells",
        };
        out.push(format!(
            "\n## {}\n\n- 直す場所: {kind}\n- 根拠: {}\n\n{}\n\n直し方の案: {}",
            printable(&note.title),
            printable(&note.evidence.join("、")),
            printable(note.detail.trim()),
            printable(note.suggestion.trim())
        ));
    }
    out.join("\n") + "\n"
}

/// 最近のメモ（新しいものから `count` 件）。
pub fn recent(home: &Home, count: usize) -> Vec<(PathBuf, String)> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(notes_dir(home))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "md"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
        .into_iter()
        .rev()
        .take(count)
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|t| (p, t)))
        .collect()
}

/// 古いもの（名前の先頭の時刻が `KEEP` より前）を消す。
fn prune(dir: &Path, now: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let at = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split(['-', '.']).next())
            .and_then(|n| n.parse::<u64>().ok());
        if at.is_some_and(|at| at + KEEP < now) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| Error::new(format!("{} を書けません: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[{"type":"text","text":"テストを流します"},{"type":"tool_use","name":"Bash","input":{"command":"agentcells cell run test\nmore"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"Exit code 1\nres://tests/smoke_test.gd: File not found"}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"大きな結果は残さない"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"StructuredOutput","input":{"next":"needs-human","comment":"テストのコマンドが見つかりません\n設定を直してください"}}]}}
{"type":"result","is_error":false,"subtype":"success","num_turns":7,"duration_ms":65000,"permission_denials":[{"tool_name":"Bash"}]}
"#;

    #[test]
    fn digests_keep_tools_errors_and_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("claude.jsonl");
        std::fs::write(&log, LOG).unwrap();
        let digest = digest(&log).unwrap();
        assert_eq!(
            digest.lines().collect::<Vec<_>>(),
            [
                "[text] テストを流します",
                "[tool] Bash agentcells cell run test more",
                "[error] Exit code 1 res://tests/smoke_test.gd: File not found",
                "[outcome] next=needs-human comment=テストのコマンドが見つかりません 設定を直してください",
                "[result] is_error=false subtype=success turns=7 seconds=65 denied=Bash",
            ]
        );
    }

    #[test]
    fn long_digests_keep_the_start_and_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("claude.jsonl");
        let line = |i: usize| {
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{i}"}}]}}}}"#
            )
        };
        let text: Vec<String> = (0..1000).map(line).collect();
        std::fs::write(&log, text.join("\n")).unwrap();
        let digest = digest(&log).unwrap();
        let lines: Vec<&str> = digest.lines().collect();
        assert_eq!(lines.len(), MAX_LINES + 1);
        assert_eq!(lines[0], "[text] 0");
        assert!(lines[MAX_LINES / 3].contains("700行を省きました"));
        assert_eq!(lines[lines.len() - 1], "[text] 999");
    }

    #[test]
    fn keeps_a_digest_only_for_output_written_by_this_run() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().to_path_buf());
        let work = Work {
            product: "app".into(),
            cell: "implement".into(),
            item: crate::config::Target::Issue,
            repo: crate::home::RepoName::parse("octo/app").unwrap(),
            number: 3,
            title: String::new(),
            waiting: None,
            conflict: false,
        };
        let log = crate::run::claude_log(&home, &work.repo, 3);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, LOG).unwrap();
        let written = std::fs::metadata(&log)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // 始めたのが、出力を書いたより後なら、前の実行の出力なので残さない
        keep_digest(&home, &work, "failed", 0, written + 10, written + 20);
        assert!(std::fs::read_dir(digests_dir(&home)).is_err());
        keep_digest(&home, &work, "failed", 0, written, written + 20);
        assert!(std::fs::read_dir(digests_dir(&home)).is_err());
        keep_digest(&home, &work, "review", 1, written - 1, written + 20);
        let kept: Vec<_> = std::fs::read_dir(digests_dir(&home)).unwrap().collect();
        assert_eq!(kept.len(), 1);
        let text = std::fs::read_to_string(kept[0].as_ref().unwrap().path()).unwrap();
        assert!(text.starts_with("product: app\ncell: implement\nissue: octo/app#3\nround: 1\n"));
        assert!(text.contains("[outcome] next=needs-human"));
    }

    #[test]
    fn masks_only_the_lines_with_secrets() {
        let dir = tempfile::tempdir().unwrap();
        // 偽のbetterleaks。「ghp_」を含む行（1から数える）を見つけたことにする
        let scanner = dir.path().join("fake-betterleaks");
        std::fs::write(
            &scanner,
            r#"#!/bin/sh
n=$(grep -n ghp_ | head -1 | cut -d: -f1)
if [ -n "$n" ]; then echo "[{\"RuleID\":\"fake\",\"StartLine\":$n}]"; exit 1; fi
echo "[]"
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&scanner, std::fs::Permissions::from_mode(0o755)).unwrap();
        let text = "[product app]\ncommands.test = \"GITHUB_TOKEN=ghp_x npm test\"\ncells = []";
        assert_eq!(
            masked(&scanner, text).unwrap(),
            "[product app]\n（秘密らしいものがあるので伏せました）\ncells = []"
        );
    }

    #[test]
    fn old_files_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("100-octo-app-1-review.txt"), "").unwrap();
        std::fs::write(dir.path().join(format!("{}.md", KEEP + 50)), "").unwrap();
        prune(dir.path(), KEEP + 200);
        assert!(!dir.path().join("100-octo-app-1-review.txt").exists());
        assert!(dir.path().join(format!("{}.md", KEEP + 50)).exists());
    }
}
