//! betterleaksが当てたものを、人が確かめて通すときに `allow_secrets` に書く項目を、手元にだけ出す。
//!
//! 項目には、当たった行の中身のハッシュが入る。betterleaksの種類には、パスワードのような短い値の行を当てるものも
//! あり、行の形が分かれば、ハッシュから値を総当たりで確かめられる。そこで、公開のコメントには場所と種類だけを
//! 書き、項目は手元（tickの出力）に出す（[`crate::blocked`] と同じ扱い）。
//!
//! `agentcells cell commit` が断ったときの項目は、claudeの出力（コマンドの結果）から拾う。AIも同じ形の文を
//! 印字できるので、拾ったものは、人が中身を確かめてから書く材料にすぎない。

use std::path::Path;

use crate::config::AllowSecret;

/// 拾う数の上限。
const MAX_ENTRIES: usize = 10;

/// 項目の前に付ける印。`agentcells cell commit` のエラーに書き、claudeの出力から拾うときの目印にする。
pub const MARK: &str = "allow_secrets に書く項目: ";

/// 人間に回すコメント（公開）に足す文。項目は書かない。
pub const COMMENT: &str = "betterleaksが秘密らしいものとして当てたものがあります。見誤りなら、通すときに設定（allow_secrets）に書く項目を、agentcellsを動かしている人の手元（tickの出力。定期実行なら `~/.agentcells/logs/tick.log`）に出しています。中身を確かめてから書いてください";

/// claudeの出力（`claude.jsonl`）のコマンドの結果から、項目を出てきた順に拾う。書き方として正しいものだけ。
pub fn entries(log: &Path) -> Vec<AllowSecret> {
    let Ok(text) = std::fs::read_to_string(log) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for line in text.lines().filter(|l| l.contains(r#""tool_result""#)) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        for content in value["message"]["content"].as_array().into_iter().flatten() {
            let mut texts = Vec::new();
            match &content["content"] {
                serde_json::Value::String(s) => texts.push(s.as_str()),
                serde_json::Value::Array(parts) => {
                    texts.extend(parts.iter().filter_map(|p| p["text"].as_str()))
                }
                _ => {}
            }
            for text in texts {
                for (i, _) in text.match_indices(MARK) {
                    let rest = &text[i + MARK.len()..];
                    let item = rest.lines().next().unwrap_or("");
                    if let Some(entry) = parse(item)
                        && !found.contains(&entry)
                    {
                        found.push(entry);
                        if found.len() >= MAX_ENTRIES {
                            return found;
                        }
                    }
                }
            }
        }
    }
    found
}

/// `{ rule = "...", path = "...", sha256 = "..." }` を読む。後ろに続く文は見ない。
fn parse(item: &str) -> Option<AllowSecret> {
    #[derive(serde::Deserialize)]
    struct Entry {
        x: AllowSecret,
    }
    let end = item.find(" }")? + 2;
    let entry: Entry = toml::from_str(&format!("x = {}", &item[..end])).ok()?;
    let x = entry.x;
    let valid = x.sha256.len() == crate::config::SECRET_DIGEST_LEN
        && x.sha256.chars().all(|c| c.is_ascii_hexdigit())
        && !x.rule.is_empty()
        && !x.path.is_empty();
    valid.then_some(x)
}

/// 手元（tickの出力）に出す文。項目がなければ `None`。
pub fn note(entries: &[AllowSecret]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let list: Vec<String> = entries
        .iter()
        .map(|e| format!("  {}", e.to_toml()))
        .collect();
    Some(format!(
        "betterleaksが秘密らしいものとして当てたもの（コミットしていません）。見誤りなら、中身を確かめてから、config.toml のproductの allow_secrets に足すと、次から通ります:\n{}",
        list.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_valid_entries_from_command_results() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("claude.jsonl");
        let item = r#"{ rule = "fake", path = "a.txt", sha256 = "2e7ebf112d677b99" }"#;
        let result = |text: &str| {
            serde_json::json!({
                "type": "user",
                "message": {"content": [{"type": "tool_result", "content": text}]}
            })
            .to_string()
        };
        let lines = [
            result(&format!(
                "コミットしませんでした:\n  - a.txt:1（fake）。{MARK}{item} と書けば通ります\n  - b.txt:2（fake）。{MARK}{{ rule = \"fake\", path = \"b.txt\", sha256 = \"short\" }}"
            )),
            // 同じものは1つに
            result(&format!("{MARK}{item}")),
            // AIの発言（コマンドの結果でないもの）は拾わない
            serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": format!("{MARK}{{ rule = \"x\", path = \"c\", sha256 = \"0123456789abcdef\" }}")}]}}).to_string(),
        ];
        std::fs::write(&log, lines.join("\n")).unwrap();
        let found = entries(&log);
        assert_eq!(
            found,
            [AllowSecret {
                rule: "fake".to_string(),
                path: "a.txt".to_string(),
                sha256: "2e7ebf112d677b99".to_string(),
            }]
        );
        let note = note(&found).unwrap();
        assert!(note.contains(item), "{note}");
        assert!(super::note(&[]).is_none());
        assert!(entries(&dir.path().join("missing")).is_empty());
    }
}
