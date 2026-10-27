//! sandboxが止めた通信の先を、claudeの出力から拾う。
//!
//! Claude Codeは、止めた通信の先を、コマンドの結果に `deny network-outbound <ホスト>:<ポート>` として書き足す。
//! 人間に回すときに、CLIが拾って、足すとよい `network` の行を、手元（tickの出力）にだけ出す。
//!
//! 公開のコメントには、先の名前を書かない（「止められた」とだけ書く）。claudeの出力には、ビルドやテストが印字した
//! ものも入るので、AIが書き換えたビルドが、秘密を埋めた名前を `deny network-outbound ...` として印字すれば、
//! そのまま公開されてしまう。攻撃がなくても、社内のプロキシやミラーの名前が出る。
//! 手元に出すものも、ホスト名として正しい形のものだけを、数を絞って拾う。

use std::path::Path;

/// 拾う数の上限。
const MAX_HOSTS: usize = 5;

const MARK: &str = "deny network-outbound ";

/// claudeの出力（`claude.jsonl`）から、止められた通信の先（`ホスト:ポート`）を、出てきた順に拾う。
pub fn hosts(log: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(log) else {
        return Vec::new();
    };
    let mut found: Vec<String> = Vec::new();
    for (i, _) in text.match_indices(MARK) {
        let token: String = text[i + MARK.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
            .take(300)
            .collect();
        let Some(target) = target(&token) else {
            continue;
        };
        if !found.contains(&target) {
            found.push(target);
            if found.len() >= MAX_HOSTS {
                break;
            }
        }
    }
    found
}

/// `ホスト:ポート` を確かめて、小文字にそろえる。ホスト名として正しくなければ `None`。
fn target(token: &str) -> Option<String> {
    let (host, port) = token.rsplit_once(':')?;
    if port.is_empty() || port.len() > 5 || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let valid = host.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| {
            (1..=63).contains(&l.len())
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    valid.then(|| format!("{host}:{port}"))
}

/// 人間に回すコメント（公開）に足す文。先の名前は書かない。
pub const COMMENT: &str = "sandboxが通信を止めました。止めた先と、足すとよい設定は、agentcellsを動かしている人の手元（tickの出力。定期実行なら `~/.agentcells/logs/tick.log`）に出しています";

/// 手元（tickの出力）に出す文。止められた先がなければ `None`。
pub fn note(hosts: &[String]) -> Option<String> {
    if hosts.is_empty() {
        return None;
    }
    let listed: Vec<String> = hosts.iter().map(|h| format!("`{h}`")).collect();
    let names: Vec<String> = hosts
        .iter()
        .map(|h| {
            format!(
                "\"{}\"",
                h.rsplit_once(':').map_or(h.as_str(), |(host, _)| host)
            )
        })
        .collect();
    Some(format!(
        "sandboxが止めた通信の先（claudeの出力から拾ったもの。ビルドが印字したものも混じりうる）: {}\nビルドやテストに要るなら、config.toml の [product.sandbox] の network に足してください（例: network = [{}]）。本当に要る先かを確かめてから決めてください",
        listed.join("、"),
        names.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_hosts_are_picked_from_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("claude.jsonl");
        assert!(hosts(&log).is_empty());
        std::fs::write(
            &log,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"error\n<sandbox_violations>deny network-outbound Services.Gradle.org:443 (curl)\ndeny network-outbound services.gradle.org:443\ndeny network-outbound dl.google.com:443</sandbox_violations>"}]}}
{"type":"user","message":{"content":"deny network-outbound localhost:8080 deny network-outbound -bad-.com:443 deny network-outbound x.com:99999999 deny network-outbound a..b:1"}}
"#,
        )
        .unwrap();
        assert_eq!(
            hosts(&log),
            vec!["services.gradle.org:443", "dl.google.com:443"]
        );
        let many: String = (0..10)
            .map(|i| format!("deny network-outbound h{i}.example.com:443\n"))
            .collect();
        std::fs::write(&log, many).unwrap();
        assert_eq!(hosts(&log).len(), MAX_HOSTS);
    }

    #[test]
    fn the_note_suggests_network_entries() {
        assert_eq!(note(&[]), None);
        let note = note(&[
            "dl.google.com:443".to_string(),
            "a.example.com:80".to_string(),
        ])
        .unwrap();
        assert!(
            note.contains("`dl.google.com:443`、`a.example.com:80`"),
            "{note}"
        );
        assert!(
            note.contains(r#"network = ["dl.google.com", "a.example.com"]"#),
            "{note}"
        );
        // 公開のコメントには、先の名前を書かない
        assert!(!COMMENT.contains("deny network-outbound"));
    }
}
