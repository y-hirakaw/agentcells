//! PRへのレビューの投稿。
//!
//! レビュー本文の1行目に目印（`[agentcells] review round <n>`）を書き、
//! 指摘は1件ずつ、コードの行へのコメントとしてそのレビューに付ける。
//! GitHubは差分の外の行にはコメントできないので、そうした指摘は本文にまとめる。

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::json;

use crate::marker;

/// コードへの指摘1件。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Finding {
    pub path: String,
    /// 指摘する行（変更後のファイルの行番号）
    pub line: u32,
    /// 複数行にまたがるときの最初の行
    #[serde(default)]
    pub start_line: Option<u32>,
    pub body: String,
}

/// `claude -p --json-schema` で、指摘を返させるためのスキーマ。
pub fn findings_schema() -> serde_json::Value {
    json!({
        "type": "array",
        "description": "コードへの指摘。1件につき1つ。指摘がなければ空にする",
        "items": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "リポジトリのルートからのパス"},
                "line": {"type": "integer", "minimum": 1, "description": "変更後のファイルの行番号"},
                "start_line": {"type": "integer", "minimum": 1, "description": "複数行にまたがるときの最初の行"},
                "body": {"type": "string", "description": "指摘の内容"}
            },
            "required": ["path", "line", "body"],
            "additionalProperties": false
        }
    })
}

/// PRの差分のうち、行コメントを付けられる行（変更後のファイルの、追加行と前後の行）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiffLines(HashMap<String, HashSet<u32>>);

impl DiffLines {
    /// `gh pr diff` の出力（unified diff）を読む。
    pub fn parse(diff: &str) -> Self {
        let mut lines: HashMap<String, HashSet<u32>> = HashMap::new();
        let mut path: Option<String> = None;
        let mut next_line: Option<u32> = None;
        for line in diff.lines() {
            if line.starts_with("diff --git ") {
                path = None;
                next_line = None;
            } else if let Some(p) = line.strip_prefix("+++ ") {
                // 消したファイルは "+++ /dev/null" になり、行コメントは付けられない
                path = unquote(p).strip_prefix("b/").map(str::to_string);
                next_line = None;
            } else if line.starts_with("@@") {
                next_line = hunk_new_start(line);
            } else if let (Some(p), Some(n)) = (&path, next_line) {
                match line.as_bytes().first() {
                    Some(b'+') | Some(b' ') => {
                        lines.entry(p.clone()).or_default().insert(n);
                        next_line = Some(n + 1);
                    }
                    // 空の前後行は、空白まで削られて空行になっていることがある
                    None => {
                        lines.entry(p.clone()).or_default().insert(n);
                        next_line = Some(n + 1);
                    }
                    // '-' は変更前の行、'\' は "No newline at end of file"
                    _ => {}
                }
            }
        }
        Self(lines)
    }

    pub fn contains(&self, path: &str, line: u32) -> bool {
        self.0.get(path).is_some_and(|ls| ls.contains(&line))
    }
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

/// "@@ -a,b +c,d @@" の c を返す。
fn hunk_new_start(header: &str) -> Option<u32> {
    let plus = header.split_whitespace().find(|w| w.starts_with('+'))?;
    plus[1..].split(',').next()?.parse().ok()
}

/// `POST /repos/{owner}/{repo}/pulls/{number}/reviews` に送る内容を作る。
///
/// 自分のPRには「変更を要求」も「承認」もできないので、種類はCOMMENTにする。
pub fn payload(
    cell: &str,
    round: u32,
    summary: &str,
    findings: &[Finding],
    diff: &DiffLines,
) -> serde_json::Value {
    let (inline, outside): (Vec<&Finding>, Vec<&Finding>) = findings
        .iter()
        .partition(|f| diff.contains(&f.path, f.line));

    let mut body = marker::comment(cell, round, summary);
    if !outside.is_empty() {
        body.push_str("\n\n差分の外への指摘（行コメントを付けられないので、ここに書きます）:\n");
        for f in &outside {
            body.push_str(&format!("\n- `{}:{}` {}", f.path, f.line, f.body.trim()));
        }
    }

    let comments: Vec<serde_json::Value> = inline
        .iter()
        .map(|f| {
            let mut c = json!({
                "path": f.path,
                "line": f.line,
                "side": "RIGHT",
                "body": f.body,
            });
            // 範囲の最初の行も差分の中にあるときだけ、複数行のコメントにする
            if let Some(start) = f.start_line
                && start < f.line
                && diff.contains(&f.path, start)
            {
                c["start_line"] = json!(start);
                c["start_side"] = json!("RIGHT");
            }
            c
        })
        .collect();

    json!({
        "event": "COMMENT",
        "body": body,
        "comments": comments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,4 +10,5 @@ fn main() {
     let a = 1;
-    let b = 2;
+    let b = 3;
+    let c = 4;

     println!(\"{a}\");
diff --git a/old.txt b/old.txt
deleted file mode 100644
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-gone
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
\\ No newline at end of file
";

    fn finding(path: &str, line: u32, body: &str) -> Finding {
        Finding {
            path: path.to_string(),
            line,
            start_line: None,
            body: body.to_string(),
        }
    }

    #[test]
    fn diff_lines_are_added_and_context_lines_on_the_new_side() {
        let diff = DiffLines::parse(DIFF);
        // 10: let a（前後の行）、11: let b = 3（追加）、12: let c（追加）、13: 空行、14: println
        for line in 10..=14 {
            assert!(diff.contains("src/main.rs", line), "line {line}");
        }
        assert!(!diff.contains("src/main.rs", 9));
        assert!(!diff.contains("src/main.rs", 15));
        assert!(diff.contains("new.txt", 1));
        assert!(diff.contains("new.txt", 2));
        assert!(!diff.contains("new.txt", 3));
        assert!(!diff.contains("old.txt", 1));
    }

    #[test]
    fn payload_starts_with_marker_and_comments_one_per_finding() {
        let diff = DiffLines::parse(DIFF);
        let findings = [
            finding("src/main.rs", 11, "b を変えた理由をコメントに"),
            finding("new.txt", 2, "末尾に改行がありません"),
        ];
        let p = payload("review", 1, "指摘は2件です", &findings, &diff);

        assert_eq!(p["event"], "COMMENT");
        let body = p["body"].as_str().unwrap();
        assert!(body.starts_with("[agentcells] review round 1\n\n指摘は2件です"));
        assert_eq!(marker::parse(body), Some(("review".to_string(), 1)));

        let comments = p["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0]["path"], "src/main.rs");
        assert_eq!(comments[0]["line"], 11);
        assert_eq!(comments[0]["side"], "RIGHT");
        assert_eq!(comments[1]["body"], "末尾に改行がありません");
    }

    #[test]
    fn findings_outside_the_diff_go_into_the_body() {
        let diff = DiffLines::parse(DIFF);
        let findings = [
            finding("src/main.rs", 11, "差分の中"),
            finding("src/lib.rs", 3, "差分の外のファイル"),
            finding("src/main.rs", 40, "差分の外の行"),
        ];
        let p = payload("review", 2, "まとめ", &findings, &diff);

        assert_eq!(p["comments"].as_array().unwrap().len(), 1);
        let body = p["body"].as_str().unwrap();
        assert!(
            body.contains("- `src/lib.rs:3` 差分の外のファイル"),
            "{body}"
        );
        assert!(body.contains("- `src/main.rs:40` 差分の外の行"), "{body}");
    }

    #[test]
    fn no_findings_means_body_only() {
        let p = payload("review", 1, "問題ありません", &[], &DiffLines::default());
        assert!(p["comments"].as_array().unwrap().is_empty());
        assert!(!p["body"].as_str().unwrap().contains("差分の外"));
    }

    #[test]
    fn multi_line_only_when_start_is_in_the_diff() {
        let diff = DiffLines::parse(DIFF);
        let mut in_diff = finding("src/main.rs", 12, "2行まとめて");
        in_diff.start_line = Some(11);
        let mut out_of_diff = finding("src/main.rs", 12, "始まりが差分の外");
        out_of_diff.start_line = Some(5);

        let p = payload("review", 1, "s", &[in_diff, out_of_diff], &diff);
        let comments = p["comments"].as_array().unwrap();
        assert_eq!(comments[0]["start_line"], 11);
        assert_eq!(comments[0]["start_side"], "RIGHT");
        assert!(comments[1].get("start_line").is_none());
    }

    #[test]
    fn findings_are_read_from_json() {
        let findings: Vec<Finding> = serde_json::from_str(
            r#"[{"path":"a.rs","line":3,"body":"x"},{"path":"b.rs","line":9,"start_line":7,"body":"y"}]"#,
        )
        .unwrap();
        assert_eq!(findings[0].start_line, None);
        assert_eq!(findings[1].start_line, Some(7));
    }
}
