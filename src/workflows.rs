//! 作業するリポジトリのGitHub Actionsのworkflowのうち、AIが書いたコードをsecrets付きで動かしうるものを探す。
//!
//! cellは `ac/...` のブランチをpushし、PRを作る。workflowがそのpushやPRで動き、secretsを使っていると、
//! AIが書いたコード（Issueに仕込まれた指示に従ったものも）が、secrets付きでCIで動く。
//! `doctor` で知らせ、ブランチを絞るか、secretsを使うjobを environment の承認で守るかを、使う人が決める。
//!
//! YAMLとしては読まず、文字列から探す（多めに知らせる向き。知らせるだけで、止めはしない）。

use serde_json::Value;

use crate::error::{Error, Result};
use crate::home::RepoName;
use crate::probe::Probe;

/// 既定のブランチの `.github/workflows` の、ファイルの名前と中身。
pub(crate) const QUERY: &str = "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { object(expression: \"HEAD:.github/workflows\") { ... on Tree { entries { name object { ... on Blob { text } } } } } } }";

/// 気をつけたいworkflow。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Risk {
    pub file: String,
    pub why: String,
}

/// `repo` の既定のブランチのworkflowを読み、気をつけたいものを返す。
pub fn risks(probe: &dyn Probe, repo: &RepoName) -> Result<Vec<Risk>> {
    let query = format!("query={QUERY}");
    let owner = format!("owner={}", repo.owner);
    let name = format!("name={}", repo.name);
    let out = probe
        .run_with_env(
            "gh",
            &["api", "graphql", "-f", &query, "-f", &owner, "-f", &name],
            &[],
        )
        .ok_or_else(|| Error::new("ghが見つかりません"))?;
    if !out.success {
        return Err(Error::new(format!(
            "{repo} のworkflowを読めません: {}",
            out.stderr.trim()
        )));
    }
    let value: Value = serde_json::from_str(&out.stdout)
        .map_err(|e| Error::new(format!("{repo} のworkflowを読めません: {e}")))?;
    let entries = value["data"]["repository"]["object"]["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut risks: Vec<Risk> = entries
        .iter()
        .filter_map(|entry| {
            let file = entry["name"].as_str()?;
            if !(file.ends_with(".yml") || file.ends_with(".yaml")) {
                return None;
            }
            // 大きいものやバイナリは、中身が返らない
            let why = match entry["object"]["text"].as_str() {
                Some(text) => assess(text)?,
                None => "中身を読めませんでした（大きいか、文字でない）。手で確かめてください"
                    .to_string(),
            };
            Some(Risk {
                file: file.to_string(),
                why,
            })
        })
        .collect();
    // 管理の権限がないと読めないことがある。読めなくても、見つけたworkflowの知らせは捨てない
    let setting = match default_token_writes(probe, repo) {
        Ok(true) => Some("GITHUB_TOKEN の既定の権限が書き込みです（cellのブランチで動くworkflowで、AIのコードがほかのブランチやタグに書き込める）".to_string()),
        Ok(false) => None,
        Err(e) => Some(format!("GITHUB_TOKEN の既定の権限を読めませんでした（リポジトリの管理の権限が要ります）: {}", e.message)),
    };
    if let Some(why) = setting {
        risks.push(Risk {
            file: "リポジトリの設定".to_string(),
            why,
        });
    }
    Ok(risks)
}

/// リポジトリの設定で、workflowの `GITHUB_TOKEN` の既定の権限が書き込みか。
fn default_token_writes(probe: &dyn Probe, repo: &RepoName) -> Result<bool> {
    let path = format!("repos/{repo}/actions/permissions/workflow");
    let out = probe
        .run_with_env("gh", &["api", &path], &[])
        .ok_or_else(|| Error::new("ghが見つかりません"))?;
    if !out.success {
        return Err(Error::new(format!(
            "{repo} のworkflowの権限を読めません: {}",
            out.stderr.trim()
        )));
    }
    let value: Value = serde_json::from_str(&out.stdout)
        .map_err(|e| Error::new(format!("{repo} のworkflowの権限を読めません: {e}")))?;
    Ok(value["default_workflow_permissions"] == "write")
}

/// cellのブランチのpushやPR、cellが書くレビューやコメントで、そのブランチのコードを動かしうるきっかけ。
const TRIGGERS: &[&str] = &[
    "push",
    "pull_request",
    "pull_request_review",
    "pull_request_review_comment",
    "create",
];

/// AIのコードに渡ると困る、`GITHUB_TOKEN` の書き込みの権限。`id-token` は、OIDCでクラウドの資格を取れる。
const WRITE_SCOPES: &[&str] = &["contents", "id-token", "packages", "actions", "deployments"];

/// 1つのworkflowを見て、気をつけたい理由を返す。
fn assess(text: &str) -> Option<String> {
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect();
    let has = |word: &str| words.contains(&word);
    // フォークや、ほかのworkflowの結果から、書き込みの権限とsecrets付きで動く
    if has("pull_request_target") {
        return Some(
            "pull_request_target で動きます（PRのコードを書き込みの権限とsecrets付きで動かしうる）"
                .to_string(),
        );
    }
    if has("workflow_run") {
        return Some(
            "workflow_run で動きます（PRで動いたworkflowのあとに、secrets付きで動く）".to_string(),
        );
    }
    if !TRIGGERS.iter().any(|t| has(t)) {
        return None;
    }
    let mut why = Vec::new();
    if text.contains("secrets: inherit") {
        why.push("呼び出すworkflowにsecretsをすべて渡しています（secrets: inherit）".to_string());
    }
    let (names, other) = secret_uses(text);
    if !names.is_empty() {
        why.push(format!("secrets（{}）を使っています", names.join("、")));
    }
    if other {
        why.push("secretsを、名前を組み立てるなどして使っています".to_string());
    }
    // 空白や引用符の書き方に左右されないように、取り除いてから探す
    let compact: String = text
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\'' | '"'))
        .collect();
    let writes = compact.contains("write-all")
        || WRITE_SCOPES
            .iter()
            .any(|scope| compact.contains(&format!("{scope}:write")));
    if writes {
        why.push("GITHUB_TOKEN に書き込みの権限（OIDCのトークンを含む）を付けています".to_string());
    }
    if why.is_empty() {
        return None;
    }
    Some(format!(
        "cellのブランチのpushやPRで動きうるうえ、{}",
        why.join("。また、")
    ))
}

/// 使っているsecretsの名前（`secrets.NAME`、`secrets['NAME']`）と、名前の分からない使い方（`toJSON(secrets)`、
/// `secrets[format(...)]` など）があるか。`GITHUB_TOKEN` は除く（どのworkflowにもある）。`secrets:` の見出しは数えない。
fn secret_uses(text: &str) -> (Vec<String>, bool) {
    let mut names: Vec<String> = Vec::new();
    let mut other = false;
    for (i, _) in text.match_indices("secrets") {
        let before = text[..i].chars().next_back();
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let rest = &text[i + "secrets".len()..];
        if rest.starts_with(':')
            || rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        let named = rest
            .strip_prefix('.')
            .or_else(|| rest.strip_prefix("['"))
            .or_else(|| rest.strip_prefix("[\""));
        let name: String = named
            .unwrap_or_default()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(100)
            .collect();
        if name.is_empty() {
            other = true;
        } else if name != "GITHUB_TOKEN" && !names.contains(&name) {
            names.push(name);
        }
    }
    (names, other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflows_that_run_ai_code_with_secrets_are_found() {
        let ci = "on:\n  push:\n  pull_request:\npermissions:\n  contents: read\njobs:\n  test:\n    steps:\n      - run: cargo test\n        env:\n          TOKEN: ${{ secrets.GITHUB_TOKEN }}\n";
        assert_eq!(assess(ci), None);
        let deploy = "on: [push]\njobs:\n  d:\n    steps:\n      - run: deploy\n        env:\n          KEY: ${{ secrets.DEPLOY_KEY }}\n          B: ${{ secrets['NPM_TOKEN'] }}\n          C: ${{ secrets.DEPLOY_KEY }}\n";
        assert_eq!(
            assess(deploy).unwrap(),
            "cellのブランチのpushやPRで動きうるうえ、secrets（DEPLOY_KEY、NPM_TOKEN）を使っています"
        );
        assert!(
            assess("on: pull_request_target\n")
                .unwrap()
                .contains("pull_request_target")
        );
        assert!(
            assess("on:\n  workflow_run:\n    workflows: [ci]\n")
                .unwrap()
                .contains("workflow_run")
        );
        assert!(
            assess("on: pull_request\njobs:\n  a:\n    uses: o/r/.github/workflows/x.yml@v1\n    secrets: inherit\n")
                .unwrap()
                .contains("inherit")
        );
        // 名前を組み立てる使い方や、レビューで動くもの、書き込みの権限も知らせる
        let dump =
            assess("on: pull_request_review\nenv:\n  ALL: ${{ toJSON(secrets) }}\n").unwrap();
        assert!(dump.contains("名前を組み立てる"), "{dump}");
        let writes = assess("on: create\npermissions: write-all\n").unwrap();
        assert!(writes.contains("書き込みの権限"), "{writes}");
        for scope in ["contents:write", "id-token: write", "packages: 'write'"] {
            let text = format!("on: push\npermissions:\n  {scope}\n");
            assert!(assess(&text).is_some(), "{scope}");
        }
        // pushでもPRでも動かないものは、secretsを使っていても知らせない
        assert_eq!(
            assess("on:\n  release:\njobs:\n  a:\n    env:\n      K: ${{ secrets.K }}\n"),
            None
        );
        // 見出しの secrets: や、ほかの語の一部は数えない
        assert_eq!(
            secret_uses("on:\n  workflow_call:\n    secrets:\n      x:\nmysecrets.y\n"),
            (Vec::new(), false)
        );
    }
}
