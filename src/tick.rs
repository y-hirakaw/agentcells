//! 定期実行で呼ぶ `agentcells tick`。仕事を探し、1件ずつcellを動かす。
//!
//! 仕事は、cellのラベルが付いていて、処理中（`ac:working`）でも人間待ち（`ac:needs-human`）でもない、
//! 開いているIssue/PR。Issueはproductの `issues` のリポジトリから、PRは `repos` のリポジトリから探す。

use std::collections::HashMap;
use std::fs::File;

use serde::Deserialize;

use crate::config::{Config, Target};
use crate::error::{Error, Result};
use crate::github::Credentials;
use crate::home::{Home, RepoName};
use crate::labels::{NEEDS_HUMAN, WORKING};
use crate::probe::Probe;

/// 1件の仕事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Work {
    pub product: String,
    pub cell: String,
    pub item: Target,
    pub repo: RepoName,
    pub number: u64,
    pub title: String,
}

impl Work {
    pub fn describe(&self) -> String {
        let kind = match self.item {
            Target::Issue => "Issue",
            Target::PullRequest => "PR",
        };
        format!(
            "{} {kind} {}#{}「{}」",
            self.cell, self.repo, self.number, self.title
        )
    }
}

/// tickを同時に2つ動かさないためのロック。持っている間だけ有効。
pub struct Lock(#[allow(dead_code)] File);

/// ロックを取る。ほかのtickが持っていれば `None`。
pub fn lock(home: &Home) -> Result<Option<Lock>> {
    let path = home.root().join("tick.lock");
    let file = File::create(&path)
        .map_err(|e| Error::new(format!("{} を作れません: {e}", path.display())))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(Lock(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(Error::new(format!(
            "{} をロックできません: {e}",
            path.display()
        ))),
    }
}

#[derive(Deserialize)]
struct Listed {
    number: u64,
    title: String,
    #[serde(default)]
    labels: Vec<ListedLabel>,
}

#[derive(Deserialize)]
struct ListedLabel {
    name: String,
}

/// 仕事を探す。productとcellを設定の順に見て、同じcellの中では番号の小さい順に並べる。
///
/// `credentials_for` は、リポジトリを読むための認証を返す（ghの設定はAIが書き換えられるので、
/// 読むときもトークンと空の設定のghを使う）。
pub fn find(
    config: &Config,
    probe: &dyn Probe,
    credentials_for: &dyn Fn(&RepoName) -> Result<Credentials>,
) -> Result<Vec<Work>> {
    let mut found = Vec::new();
    // Appのトークンはリポジトリごとなので、1回のtickの中ではリポジトリごとに1つだけ作って使い回す
    let mut cache: HashMap<RepoName, Credentials> = HashMap::new();
    for product in &config.products {
        for name in &product.cells {
            let Some(cell) = config.cell(name) else {
                continue;
            };
            for &item in &cell.on {
                let repos: Vec<&RepoName> = match item {
                    Target::Issue => vec![&product.issues],
                    Target::PullRequest => product.repos.iter().collect(),
                };
                for repo in repos {
                    if !cache.contains_key(repo) {
                        cache.insert(repo.clone(), credentials_for(repo)?);
                    }
                    let env = cache[repo].gh_env()?;
                    let mut items = list(probe, &env, item, repo, &cell.label)?;
                    items.sort_by_key(|i| i.number);
                    found.extend(
                        items
                            .into_iter()
                            .filter(|i| {
                                // GitHubのラベル名は大文字・小文字を区別しない
                                !i.labels.iter().any(|l| {
                                    l.name.eq_ignore_ascii_case(WORKING)
                                        || l.name.eq_ignore_ascii_case(NEEDS_HUMAN)
                                })
                            })
                            .map(|i| Work {
                                product: product.name.clone(),
                                cell: cell.name.clone(),
                                item,
                                repo: repo.clone(),
                                number: i.number,
                                title: i.title,
                            }),
                    );
                }
            }
        }
    }
    Ok(found)
}

fn list(
    probe: &dyn Probe,
    env: &[(&str, &str)],
    item: Target,
    repo: &RepoName,
    label: &str,
) -> Result<Vec<Listed>> {
    let kind = match item {
        Target::Issue => "issue",
        Target::PullRequest => "pr",
    };
    let repo_arg = repo.to_string();
    let args = [
        kind,
        "list",
        "--repo",
        repo_arg.as_str(),
        "--label",
        label,
        "--state",
        "open",
        "--json",
        "number,title,labels",
        "--limit",
        "100",
    ];
    let out = probe.run_with_env("gh", &args, env).ok_or_else(|| {
        Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if !out.success {
        return Err(Error::new(format!(
            "{repo} の {label} の一覧を読めません: {}",
            out.stderr.trim()
        ))
        .with_hint("`agentcells doctor` で、リポジトリと書き込みのアカウントを確かめてください"));
    }
    serde_json::from_str(&out.stdout)
        .map_err(|e| Error::new(format!("{repo} の一覧を読めません: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Source;
    use crate::probe::fake::FakeProbe;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"
[[product]]
name = "app"
issues = "octo/issues"
repos = ["octo/app", "octo/web"]
cells = ["triage", "review"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
next = ["done"]

[[cell]]
name = "review"
on = "pull_request"
label = "ac:review"
next = ["done"]
"#;
        let config = Config::parse(text, dir.path()).unwrap();
        (dir, config)
    }

    fn token(repo: &RepoName) -> Result<Credentials> {
        Credentials::new(Source::App, format!("ghs_{}", repo.name))
    }

    #[test]
    fn finds_open_items_with_cell_labels_skipping_working_and_waiting() {
        let (_dir, config) = config();
        let probe = FakeProbe::default()
            .with(
                "gh issue list --repo octo/issues --label ac:triage --state open --json number,title,labels --limit 100",
                true,
                r#"[{"number":9,"title":"後","labels":[{"name":"ac:triage"}]},
                    {"number":3,"title":"先","labels":[{"name":"ac:triage"}]},
                    {"number":4,"title":"処理中","labels":[{"name":"ac:triage"},{"name":"ac:working"}]},
                    {"number":5,"title":"人間待ち","labels":[{"name":"ac:triage"},{"name":"AC:Needs-Human"}]}]"#,
            )
            .with(
                "gh pr list --repo octo/app --label ac:review --state open --json number,title,labels --limit 100",
                true,
                r#"[{"number":12,"title":"PR","labels":[{"name":"ac:review"}]}]"#,
            )
            .with(
                "gh pr list --repo octo/web --label ac:review --state open --json number,title,labels --limit 100",
                true,
                "[]",
            );
        let work = find(&config, &probe, &token).unwrap();
        let described: Vec<String> = work.iter().map(Work::describe).collect();
        assert_eq!(
            described,
            vec![
                "triage Issue octo/issues#3「先」",
                "triage Issue octo/issues#9「後」",
                "review PR octo/app#12「PR」",
            ]
        );
        // リポジトリごとのトークンで、空のghの設定を使って読む
        for (call, env) in probe.calls.borrow().iter().zip(probe.envs.borrow().iter()) {
            let repo = call.split("--repo octo/").nth(1).unwrap();
            let name = repo.split(' ').next().unwrap();
            assert!(
                env.contains(&("GH_TOKEN".to_string(), format!("ghs_{name}"))),
                "{call}: {env:?}"
            );
            assert!(env.iter().any(|(k, _)| k == "GH_CONFIG_DIR"));
        }
    }

    #[test]
    fn makes_one_token_per_repo() {
        let (_dir, mut config) = config();
        // 同じリポジトリを、2つのcellで、IssueとしてもPRとしても見る
        config.products[0].repos = vec![RepoName::parse("octo/issues").unwrap()];
        config.cells[1].on = vec![Target::Issue, Target::PullRequest];
        let mut probe = FakeProbe::default();
        for (kind, label) in [
            ("issue", "ac:triage"),
            ("issue", "ac:review"),
            ("pr", "ac:review"),
        ] {
            probe = probe.with(
                &format!(
                    "gh {kind} list --repo octo/issues --label {label} --state open --json number,title,labels --limit 100"
                ),
                true,
                "[]",
            );
        }
        let made = std::cell::Cell::new(0);
        let counting = |repo: &RepoName| {
            made.set(made.get() + 1);
            token(repo)
        };
        assert!(find(&config, &probe, &counting).unwrap().is_empty());
        assert_eq!(probe.calls.borrow().len(), 3);
        assert_eq!(made.get(), 1);
    }

    #[test]
    fn list_failures_point_to_doctor() {
        let (_dir, config) = config();
        let probe = FakeProbe::default().with_fallback(false);
        let err = find(&config, &probe, &token).unwrap_err();
        assert!(err.hint.unwrap().contains("agentcells doctor"));
    }

    #[test]
    fn only_one_tick_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().to_path_buf());
        let first = lock(&home).unwrap();
        assert!(first.is_some());
        assert!(lock(&home).unwrap().is_none());
        drop(first);
        assert!(lock(&home).unwrap().is_some());
    }
}
