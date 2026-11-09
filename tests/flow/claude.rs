//! 偽の `claude -p`。テストのバイナリが `--fake-claude <場面のフォルダ> <claudeの引数...>` で動く。
//!
//! 場面のフォルダ（`claude/`）:
//! - `replies/<cell>/<n>.json`: そのcellの n 回目（1から）に返す結果（`structured_output`）
//! - `calls/<cell>-<n>/`: 受け取ったもの（`prompt.txt`、`system.txt`、`schema.json`、`cwd.txt`）
//! - `problems/`: 場面の誤り（返す結果を用意していない、スキーマに合わない）。場面が確かめて失敗にする

use std::io::Read;
use std::path::Path;
use std::process::ExitCode;

use serde_json::{Value, json};

pub fn main(args: &[String]) -> ExitCode {
    let Some((dir, args)) = args.split_first() else {
        eprintln!("--fake-claude <場面のフォルダ> が要ります");
        return ExitCode::from(2);
    };
    let dir = Path::new(dir);
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_default()
    };
    let system = flag("--append-system-prompt");
    let schema: Value = serde_json::from_str(&flag("--json-schema")).unwrap_or(Value::Null);
    let mut prompt = String::new();
    let _ = std::io::stdin().read_to_string(&mut prompt);

    let cell = cell_of(&system).unwrap_or_else(|| "unknown".to_string());
    let calls = dir.join("calls");
    let round = (1..)
        .find(|n| !calls.join(format!("{cell}-{n}")).exists())
        .unwrap_or(1);
    let saved = calls.join(format!("{cell}-{round}"));
    let _ = std::fs::create_dir_all(&saved);
    let _ = std::fs::write(saved.join("prompt.txt"), &prompt);
    let _ = std::fs::write(saved.join("system.txt"), &system);
    let _ = std::fs::write(saved.join("schema.json"), schema.to_string());
    if let Ok(cwd) = std::env::current_dir() {
        let _ = std::fs::write(saved.join("cwd.txt"), cwd.display().to_string());
    }

    // 同じ回に2つ起きても、前のものを消さないように書き足す
    let problem = |text: String| {
        use std::io::Write;
        let problems = dir.join("problems");
        let _ = std::fs::create_dir_all(&problems);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(problems.join(format!("{cell}-{round}.txt")))
        {
            let _ = writeln!(file, "{text}");
        }
    };
    // 本物のclaudeは、知らない引数があると失敗する
    if let Some(why) = unknown_arg(args) {
        problem(format!("{cell} の {round} 回目の引数: {why}"));
    }
    let reply = dir
        .join("replies")
        .join(&cell)
        .join(format!("{round}.json"));
    let output = match std::fs::read_to_string(&reply) {
        Ok(text) => serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
        Err(_) => {
            problem(format!("{cell} の {round} 回目に返す結果がありません"));
            Value::Null
        }
    };
    // 本物のclaudeは、スキーマに合わない結果を返せない（structured_output が null になる）
    let output = match mismatch(&schema, &output) {
        Some(why) if !output.is_null() => {
            problem(format!(
                "{cell} の {round} 回目の結果がスキーマに合いません: {why}"
            ));
            Value::Null
        }
        _ => output,
    };
    println!("{}", json!({ "type": "system", "subtype": "init" }));
    println!(
        "{}",
        json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "num_turns": 1,
            "total_cost_usd": 0.0,
            "structured_output": output,
        })
    );
    ExitCode::SUCCESS
}

/// CLIが渡す引数（`run/claude.rs`）のほかのものがあれば、その説明。
fn unknown_arg(args: &[String]) -> Option<String> {
    const FLAGS: &[&str] = &["-p", "--verbose", "--strict-mcp-config"];
    const WITH_VALUE: &[&str] = &[
        "--output-format",
        "--settings",
        "--setting-sources",
        "--append-system-prompt",
        "--json-schema",
        "--model",
        "--tools",
    ];
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if FLAGS.contains(&arg.as_str()) {
            continue;
        }
        if !WITH_VALUE.contains(&arg.as_str()) {
            return Some(format!("{arg} を知りません"));
        }
        if args.next().is_none() {
            return Some(format!("{arg} の値がありません"));
        }
    }
    None
}

/// system promptの1行目「あなたは agentcells の cell「triage」として動いています。」から、cellの名前を読む。
fn cell_of(system: &str) -> Option<String> {
    let line = system.lines().next()?;
    let start = line.find("cell「")? + "cell「".len();
    let end = line[start..].find('」')? + start;
    Some(line[start..end].to_string())
}

/// スキーマに合わなければ、その理由。CLIが渡すスキーマで使う形（object、required、
/// additionalProperties、enum、type、maxLength、配列）だけを見る。
fn mismatch(schema: &Value, value: &Value) -> Option<String> {
    if let Some(choices) = schema["enum"].as_array()
        && !choices.contains(value)
    {
        return Some(format!(
            "{value} は {} のどれでもありません",
            Value::Array(choices.clone())
        ));
    }
    match schema["type"].as_str() {
        Some("object") => {
            let Some(object) = value.as_object() else {
                return Some("objectではありません".to_string());
            };
            let properties = schema["properties"].as_object();
            for name in schema["required"].as_array().into_iter().flatten() {
                let name = name.as_str().unwrap_or_default();
                if !object.contains_key(name) {
                    return Some(format!("{name} がありません"));
                }
            }
            for (name, v) in object {
                match properties.and_then(|p| p.get(name)) {
                    Some(s) => {
                        if let Some(why) = mismatch(s, v) {
                            return Some(format!("{name}: {why}"));
                        }
                    }
                    None if schema["additionalProperties"] == json!(false) => {
                        return Some(format!("{name} はスキーマにありません"));
                    }
                    None => {}
                }
            }
            None
        }
        Some("array") => {
            let Some(items) = value.as_array() else {
                return Some("配列ではありません".to_string());
            };
            if let Some(max) = schema["maxItems"].as_u64()
                && items.len() as u64 > max
            {
                return Some(format!("{} 件は多すぎます（{max} 件まで）", items.len()));
            }
            items.iter().find_map(|v| mismatch(&schema["items"], v))
        }
        Some("string") => {
            let Some(s) = value.as_str() else {
                return Some("文字列ではありません".to_string());
            };
            match schema["maxLength"].as_u64() {
                Some(max) if s.chars().count() as u64 > max => Some(format!(
                    "{} 字は長すぎます（{max} 字まで）",
                    s.chars().count()
                )),
                _ => None,
            }
        }
        _ => None,
    }
}
