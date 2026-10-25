//! 閉じたIssue/PRに残ったagentcellsのラベルを外す（tickのたびに、仕事を探す前にする）。
//!
//! 検品を待っていたPRをマージしたり、人間を待っていたIssueを閉じたりすると、`ac:in-review` や
//! `ac:needs-human` が残る。動きには関係しない（tickもstatusも、開いているものしか見ない）が、
//! ラベルで一覧を絞ったときに、終わったものが混ざるため。
//!
//! 外すのは、`ac:working`、人間を待っているラベル、そのproductのcellのラベルだけ。tickのロックを持って
//! いる間にするので、動いているcellのラベルを外すことはない。

use serde::Deserialize;

use crate::config::{Config, Product, Target};
use crate::error::{Error, Result};
use crate::github::Credentials;
use crate::home::RepoName;
use crate::labels::{IN_REVIEW, NEEDS_HUMAN, WORKING};
use crate::probe::Probe;

/// 閉じていて、agentcellsのラベルが残っているもの。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub repo: RepoName,
    pub kind: Target,
    pub number: u64,
    /// 外すラベル（GitHubにある名前のまま）
    pub labels: Vec<String>,
}

impl Leftover {
    pub fn describe(&self) -> String {
        let kind = match self.kind {
            Target::Issue => "Issue",
            Target::PullRequest => "PR",
        };
        format!(
            "{kind} {}#{} から {}",
            self.repo,
            self.number,
            self.labels.join(", ")
        )
    }
}

/// 1回に見る件数。片付けたものは次から出てこないので、多ければ次のtickで続きを片付ける。
pub(crate) const QUERY: &str = "query($owner: String!, $name: String!, $labels: [String!], $issues: Boolean!, $prs: Boolean!) { repository(owner: $owner, name: $name) { issues(states: CLOSED, labels: $labels, first: 50, orderBy: {field: UPDATED_AT, direction: DESC}) @include(if: $issues) { nodes { number labels(first: 20) { nodes { name } } } } pullRequests(states: [CLOSED, MERGED], labels: $labels, first: 50, orderBy: {field: UPDATED_AT, direction: DESC}) @include(if: $prs) { nodes { number labels(first: 20) { nodes { name } } } } } }";

#[derive(Deserialize)]
struct Response {
    data: Data,
}

#[derive(Deserialize)]
struct Data {
    repository: Option<Repository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    #[serde(default)]
    issues: Option<Nodes<Node>>,
    #[serde(default)]
    pull_requests: Option<Nodes<Node>>,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct Node {
    number: u64,
    labels: Nodes<Name>,
}

#[derive(Deserialize)]
struct Name {
    name: String,
}

/// productで外すラベル。
fn ours(config: &Config, product: &Product) -> Vec<String> {
    let mut labels: Vec<String> = [WORKING, NEEDS_HUMAN, IN_REVIEW]
        .iter()
        .map(|l| l.to_string())
        .collect();
    labels.extend(
        product
            .cells
            .iter()
            .filter_map(|name| config.cell(name))
            .map(|c| c.label.clone()),
    );
    labels
}

/// 閉じていて、agentcellsのラベルが残っているものを探す。読めなかったリポジトリは、理由を付けて返す。
///
/// `credentials_for` は、リポジトリを読むための認証を返す（tickと同じく、トークンと空の設定のghで読む）。
pub fn find(
    config: &Config,
    probe: &dyn Probe,
    credentials_for: &dyn Fn(&RepoName) -> Result<Credentials>,
) -> (Vec<Leftover>, Vec<String>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    for product in &config.products {
        let labels = ours(config, product);
        let mut repos: Vec<&RepoName> = vec![&product.issues];
        repos.extend(product.repos.iter().filter(|r| *r != &product.issues));
        for repo in repos {
            let want_issues = repo == &product.issues;
            let want_prs = product.repos.contains(repo);
            let read = credentials_for(repo)
                .and_then(|c| fetch(probe, &c, repo, &labels, want_issues, want_prs));
            let read = match read {
                Ok(read) => read,
                Err(e) => {
                    errors.push(e.message);
                    continue;
                }
            };
            for (kind, nodes) in [
                (Target::Issue, read.issues),
                (Target::PullRequest, read.pull_requests),
            ] {
                for node in nodes.into_iter().flat_map(|n| n.nodes) {
                    let remove: Vec<String> = node
                        .labels
                        .nodes
                        .into_iter()
                        .map(|l| l.name)
                        .filter(|name| labels.iter().any(|l| l.eq_ignore_ascii_case(name)))
                        .collect();
                    // 同じリポジトリを2つのproductで使っていれば、先に見つけた方だけにする
                    let seen = found.iter().any(|f: &Leftover| {
                        &f.repo == repo && f.kind == kind && f.number == node.number
                    });
                    if !remove.is_empty() && !seen {
                        found.push(Leftover {
                            repo: repo.clone(),
                            kind,
                            number: node.number,
                            labels: remove,
                        });
                    }
                }
            }
        }
    }
    (found, errors)
}

fn fetch(
    probe: &dyn Probe,
    credentials: &Credentials,
    repo: &RepoName,
    labels: &[String],
    issues: bool,
    prs: bool,
) -> Result<Repository> {
    let env = credentials.gh_env()?;
    let mut args: Vec<String> = vec![
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={QUERY}"),
        // -F は数字だけの名前を数に変えるので、名前は -f で渡す。真偽値だけ -F で渡す
        "-f".into(),
        format!("owner={}", repo.owner),
        "-f".into(),
        format!("name={}", repo.name),
        "-F".into(),
        format!("issues={issues}"),
        "-F".into(),
        format!("prs={prs}"),
    ];
    for label in labels {
        args.extend(["-f".into(), format!("labels[]={label}")]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = probe.run_with_env("gh", &args, &env).ok_or_else(|| {
        Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if !out.success {
        return Err(Error::new(format!(
            "{repo} の閉じたものを読めません: {}",
            out.stderr.trim()
        )));
    }
    let response: Response = serde_json::from_str(&out.stdout)
        .map_err(|e| Error::new(format!("{repo} の閉じたものを読めません: {e}")))?;
    response
        .data
        .repository
        .ok_or_else(|| Error::new(format!("{repo} が見つかりません")))
}

/// ラベルを外す。1件ずつ結果を返す（外せなくても、ほかのものは続ける）。
///
/// `write_for` は、書き込みの認証を返す。リポジトリごとに1つ作り、使い終わったら取り消す。
pub fn remove(
    probe: &dyn Probe,
    leftovers: &[Leftover],
    write_for: &dyn Fn(&RepoName) -> Result<Credentials>,
) -> Vec<Result<()>> {
    let mut results: Vec<Option<Result<()>>> = leftovers.iter().map(|_| None).collect();
    let mut repos: Vec<&RepoName> = leftovers.iter().map(|l| &l.repo).collect();
    repos.dedup();
    for repo in repos {
        let indices: Vec<usize> = (0..leftovers.len())
            .filter(|&i| &leftovers[i].repo == repo && results[i].is_none())
            .collect();
        if indices.is_empty() {
            continue;
        }
        let write = match write_for(repo) {
            Ok(write) => write,
            Err(e) => {
                for i in indices {
                    let mut error = Error::new(e.message.clone());
                    error.hint = e.hint.clone();
                    results[i] = Some(Err(error));
                }
                continue;
            }
        };
        for i in indices {
            results[i] = Some(edit(probe, &write, &leftovers[i]));
        }
        write.revoke(probe);
    }
    results.into_iter().map(|r| r.unwrap_or(Ok(()))).collect()
}

fn edit(probe: &dyn Probe, write: &Credentials, leftover: &Leftover) -> Result<()> {
    let env = write.gh_env()?;
    let number = leftover.number.to_string();
    let repo = leftover.repo.to_string();
    let remove = leftover.labels.join(",");
    let sub = match leftover.kind {
        Target::Issue => "issue",
        Target::PullRequest => "pr",
    };
    let args = [
        sub,
        "edit",
        number.as_str(),
        "--repo",
        repo.as_str(),
        "--remove-label",
        remove.as_str(),
    ];
    let out = probe.run_with_env("gh", &args, &env).ok_or_else(|| {
        Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if !out.success {
        return Err(Error::new(out.stderr.trim().to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Source;
    use crate::probe::fake::FakeProbe;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"
operator = "me"

[[product]]
name = "app"
issues = "octo/app"
repos = ["octo/app", "octo/web"]
cells = ["review"]

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

    fn command(repo: &str, issues: bool, prs: bool) -> String {
        let (owner, name) = repo.split_once('/').unwrap();
        format!(
            "gh api graphql -f query={QUERY} -f owner={owner} -f name={name} -F issues={issues} -F prs={prs} -f labels[]=ac:working -f labels[]=ac:needs-human -f labels[]=ac:in-review -f labels[]=ac:review"
        )
    }

    #[test]
    fn finds_and_removes_only_agentcells_labels_from_closed_items() {
        let (_dir, config) = config();
        let probe = FakeProbe::default()
            .with(
                &command("octo/app", true, true),
                true,
                r#"{"data":{"repository":{
                    "issues":{"nodes":[{"number":1,"labels":{"nodes":[{"name":"bug"},{"name":"AC:Needs-Human"}]}}]},
                    "pullRequests":{"nodes":[{"number":2,"labels":{"nodes":[{"name":"ac:in-review"},{"name":"ac:other"}]}}]}}}}"#,
            )
            .with(&command("octo/web", false, true), false, "")
            .with(
                "gh issue edit 1 --repo octo/app --remove-label AC:Needs-Human",
                true,
                "",
            )
            .with_fallback(false);

        let (found, errors) = find(&config, &probe, &token);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("octo/web"), "{errors:?}");
        let described: Vec<String> = found.iter().map(Leftover::describe).collect();
        assert_eq!(
            described,
            [
                "Issue octo/app#1 から AC:Needs-Human",
                "PR octo/app#2 から ac:in-review"
            ]
        );

        // 1件外せなくても、ほかは続け、トークンは1つ作って取り消す
        let results = remove(&probe, &found, &token);
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
        let calls = probe.calls.borrow();
        assert!(
            calls.contains(&"gh pr edit 2 --repo octo/app --remove-label ac:in-review".to_string())
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.contains("DELETE /installation/token"))
                .count(),
            1
        );
    }
}
