//! 定期実行で呼ぶ `agentcells tick`。仕事を探し、1件ずつcellを動かす。
//!
//! 仕事は、cellのラベルが付いていて、処理中（`ac:working`）でも人間待ち（`ac:needs-human`）でもない、
//! 開いているIssue/PR。Issueはproductの `issues` のリポジトリから、PRは `repos` のリポジトリから探す。
//! そのうち、cellを使う人（operator）が自分で決めたものだけを動かす（[`guard`]）。

use std::collections::HashMap;
use std::fs::File;

use serde::Deserialize;

use crate::config::{Config, Target};
use crate::error::{Error, Result};
use crate::github::Credentials;
use crate::guard::{self, Actors};
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

/// 見つかった仕事と、見送ったもの（理由つき）。
#[derive(Debug, Default)]
pub struct Found {
    pub work: Vec<Work>,
    pub skipped: Vec<(Work, String)>,
}

#[derive(Deserialize)]
struct Listed {
    number: u64,
    title: String,
    #[serde(default)]
    labels: Vec<ListedLabel>,
    #[serde(default)]
    assignees: Vec<ListedUser>,
    #[serde(default)]
    author: Option<ListedUser>,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
struct ListedLabel {
    name: String,
}

#[derive(Deserialize)]
struct ListedUser {
    login: String,
}

/// 仕事を探す。productとcellを設定の順に見て、同じcellの中では番号の小さい順に並べる。
///
/// `credentials_for` は、リポジトリを読むための認証を返す（ghの設定はAIが書き換えられるので、
/// 読むときもトークンと空の設定のghを使う）。
/// Assigneesにoperatorがいないものと、[`guard::check`] が止めたものは、理由を付けて見送る。
pub fn find(
    config: &Config,
    probe: &dyn Probe,
    credentials_for: &dyn Fn(&RepoName) -> Result<Credentials>,
    actors: &Actors,
) -> Result<Found> {
    let mut found = Found::default();
    // Appのトークンはリポジトリごとなので、1回のtickの中ではリポジトリごとに1つだけ作って使い回す
    let mut cache: HashMap<RepoName, Credentials> = HashMap::new();
    for product in &config.products {
        // 元のIssueのリポジトリの分も、先に作っておく
        for repo in std::iter::once(&product.issues).chain(&product.repos) {
            if !cache.contains_key(repo) {
                cache.insert(repo.clone(), credentials_for(repo)?);
            }
        }
        let cell_labels: Vec<String> = product
            .cells
            .iter()
            .filter_map(|name| config.cell(name))
            .map(|c| c.label.clone())
            .collect();
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
                    let env = cache[repo].gh_env()?;
                    let mut items = list(probe, &env, item, repo, &cell.label)?;
                    items.sort_by_key(|i| i.number);
                    for i in items {
                        // GitHubのラベル名は大文字・小文字を区別しない
                        if i.labels.iter().any(|l| {
                            l.name.eq_ignore_ascii_case(WORKING)
                                || l.name.eq_ignore_ascii_case(NEEDS_HUMAN)
                        }) {
                            continue;
                        }
                        let work = Work {
                            product: product.name.clone(),
                            cell: cell.name.clone(),
                            item,
                            repo: repo.clone(),
                            number: i.number,
                            title: i.title,
                        };
                        if !i
                            .assignees
                            .iter()
                            .any(|a| a.login.eq_ignore_ascii_case(&actors.operator))
                        {
                            let reason = format!("Assignees に {} がいません", actors.operator);
                            found.skipped.push((work, reason));
                            continue;
                        }
                        let author = i
                            .author
                            .as_ref()
                            .map(|a| crate::pr::app_login(&a.login))
                            .unwrap_or_else(|| "ghost".to_string());
                        let original = match item {
                            Target::PullRequest => crate::prompt::closes(&i.body, repo),
                            Target::Issue => None,
                        };
                        let original_env = match &original {
                            Some((original_repo, _)) => match cache.get(original_repo) {
                                Some(c) => Some(c.gh_env()?),
                                None => {
                                    let reason = format!(
                                        "元のIssueのリポジトリ {original_repo} が、このproductにありません"
                                    );
                                    found.skipped.push((work, reason));
                                    continue;
                                }
                            },
                            None => None,
                        };
                        let request = guard::Request {
                            kind: item,
                            subject: guard::Subject {
                                repo,
                                number: i.number,
                                env: &env,
                            },
                            author: &author,
                            label: &cell.label,
                            cell_labels: &cell_labels,
                            original: original.as_ref().zip(original_env.as_ref()).map(
                                |((original_repo, number), env)| guard::Subject {
                                    repo: original_repo,
                                    number: *number,
                                    env,
                                },
                            ),
                        };
                        // 1件の履歴を読めなくても、ほかの仕事は進める（読めなかったものは動かさない）
                        match guard::check(probe, &request, actors) {
                            Ok(None) => found.work.push(work),
                            Ok(Some(reason)) => found.skipped.push((work, reason)),
                            Err(e) => found
                                .skipped
                                .push((work, format!("履歴を読めません: {}", e.message))),
                        }
                    }
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
        "number,title,labels,assignees,author,body",
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
    use crate::guard::fake::{NO_EDITS, assigned, labeled, with_history};
    use crate::probe::fake::FakeProbe;

    fn actors() -> Actors {
        Actors {
            operator: "me".to_string(),
            writer: "bot[bot]".to_string(),
        }
    }

    const ME: &str = r#""assignees":[{"login":"me"}]"#;

    /// operatorが自分を割り当て、ラベルを付けた履歴。
    fn approved(probe: FakeProbe, repo: &str, number: u64, label: &str) -> FakeProbe {
        let events = [
            assigned("me", "me", "2026-10-25T01:00:00Z"),
            labeled("me", label, "2026-10-25T02:00:00Z"),
        ];
        with_history(probe, repo, number, &events, NO_EDITS)
    }

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"
operator = "me"

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
                "gh issue list --repo octo/issues --label ac:triage --state open --json number,title,labels,assignees,author,body --limit 100",
                true,
                &format!(r#"[{{"number":9,"title":"後","labels":[{{"name":"ac:triage"}}],{ME}}},
                    {{"number":3,"title":"先","labels":[{{"name":"ac:triage"}}],{ME}}},
                    {{"number":4,"title":"処理中","labels":[{{"name":"ac:triage"}},{{"name":"ac:working"}}],{ME}}},
                    {{"number":5,"title":"人間待ち","labels":[{{"name":"ac:triage"}},{{"name":"AC:Needs-Human"}}],{ME}}},
                    {{"number":6,"title":"他人の","labels":[{{"name":"ac:triage"}}],"assignees":[{{"login":"other"}}]}},
                    {{"number":7,"title":"勝手に","labels":[{{"name":"ac:triage"}}],{ME}}}]"#),
            )
            .with(
                "gh pr list --repo octo/app --label ac:review --state open --json number,title,labels,assignees,author,body --limit 100",
                true,
                &format!(r#"[{{"number":12,"title":"PR","labels":[{{"name":"ac:review"}}],{ME}}}]"#),
            )
            .with(
                "gh pr list --repo octo/web --label ac:review --state open --json number,title,labels,assignees,author,body --limit 100",
                true,
                "[]",
            );
        let probe = approved(probe, "octo/issues", 9, "ac:triage");
        let probe = approved(probe, "octo/issues", 3, "ac:triage");
        let probe = approved(probe, "octo/app", 12, "ac:review");
        // ほかの人がラベルを付けた
        let eager = [
            assigned("me", "me", "2026-10-25T01:00:00Z"),
            labeled("eager", "ac:triage", "2026-10-25T02:00:00Z"),
        ];
        let probe = with_history(probe, "octo/issues", 7, &eager, NO_EDITS);
        let found = find(&config, &probe, &token, &actors()).unwrap();
        let skipped: Vec<String> = found
            .skipped
            .iter()
            .map(|(w, reason)| format!("{} {reason}", w.number))
            .collect();
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(
            skipped[0].starts_with("6 Assignees に me がいません"),
            "{skipped:?}"
        );
        assert!(
            skipped[1].starts_with("7 ラベル「ac:triage」を最後に付けたのが eager"),
            "{skipped:?}"
        );
        let work = found.work;
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
            // 一覧は --repo、履歴は /repos/ のパス、編集の履歴はGraphQLの name でリポジトリを渡す
            let rest = ["--repo octo/", "/repos/octo/", "-F name="]
                .iter()
                .find_map(|key| call.split(key).nth(1))
                .unwrap();
            let name = rest.split([' ', '/']).next().unwrap();
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
                    "gh {kind} list --repo octo/issues --label {label} --state open --json number,title,labels,assignees,author,body --limit 100"
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
        assert!(
            find(&config, &probe, &counting, &actors())
                .unwrap()
                .work
                .is_empty()
        );
        assert_eq!(probe.calls.borrow().len(), 3);
        assert_eq!(made.get(), 1);
    }

    #[test]
    fn prs_made_by_agentcells_run_from_the_issue_approval_and_unreadable_items_are_skipped() {
        let (_dir, config) = config();
        let list = |kind: &str, repo: &str, label: &str| {
            format!(
                "gh {kind} list --repo {repo} --label {label} --state open --json number,title,labels,assignees,author,body --limit 100"
            )
        };
        let probe = FakeProbe::default()
            .with(
                &list("issue", "octo/issues", "ac:triage"),
                true,
                &format!(r#"[{{"number":5,"title":"読めない","labels":[{{"name":"ac:triage"}}],{ME}}}]"#),
            )
            .with(
                &list("pr", "octo/app", "ac:review"),
                true,
                &format!(
                    r#"[{{"number":12,"title":"PR","labels":[{{"name":"ac:review"}}],{ME},"author":{{"login":"app/bot"}},"body":"Closes octo/issues#3"}}]"#
                ),
            )
            .with(&list("pr", "octo/web", "ac:review"), true, "[]");
        // agentcellsが作ったPRに、agentcellsがoperatorを割り当て、reviewのラベルを付けた
        let pr_events = [
            assigned("bot[bot]", "me", "2026-10-25T03:00:00Z"),
            labeled("bot[bot]", "ac:review", "2026-10-25T03:00:00Z"),
        ];
        let probe = with_history(probe, "octo/app", 12, &pr_events, NO_EDITS);
        // 元のIssueは、operatorが承認している
        let probe = approved(probe, "octo/issues", 3, "ac:triage");
        // octo/issues#5 の履歴は、応答を用意していないので読めない
        let found = find(&config, &probe, &token, &actors()).unwrap();
        let work: Vec<String> = found.work.iter().map(Work::describe).collect();
        assert_eq!(work, vec!["review PR octo/app#12「PR」"]);
        assert_eq!(found.skipped.len(), 1);
        assert!(
            found.skipped[0].1.starts_with("履歴を読めません"),
            "{:?}",
            found.skipped
        );
    }

    #[test]
    fn list_failures_point_to_doctor() {
        let (_dir, config) = config();
        let probe = FakeProbe::default().with_fallback(false);
        let err = find(&config, &probe, &token, &actors()).unwrap_err();
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
