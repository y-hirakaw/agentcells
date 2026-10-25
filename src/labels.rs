//! agentcellsが使うGitHubのラベル。

use std::collections::{BTreeMap, HashSet};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::github::{self, Credentials, Purpose};
use crate::home::RepoName;
use crate::probe::Probe;

/// agentcellsのラベルは、すべてこれで始める。
pub const PREFIX: &str = "ac:";
/// 処理中。同じ仕事を二重に拾わないために付ける。
pub const WORKING: &str = "ac:working";
/// 人間の回答や検品を待っている。
pub const NEEDS_HUMAN: &str = "ac:needs-human";

const SYSTEM: &[(&str, &str, &str)] = &[
    (WORKING, "fbca04", "agentcellsが処理中"),
    (NEEDS_HUMAN, "d93f0b", "人間の回答や検品を待っている"),
];
const CELL_COLOR: &str = "1d76db";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub color: String,
    pub description: String,
}

pub fn is_system(name: &str) -> bool {
    SYSTEM.iter().any(|(n, _, _)| n.eq_ignore_ascii_case(name))
}

/// リポジトリごとに必要なラベル。Issueのリポジトリと作業するリポジトリの両方に、
/// agentcellsのラベルと、そのproductのcellが拾うラベルを置く。
pub fn required(config: &Config) -> BTreeMap<RepoName, Vec<Label>> {
    let mut by_repo: BTreeMap<RepoName, Vec<Label>> = BTreeMap::new();
    for product in &config.products {
        let mut labels: Vec<Label> = SYSTEM
            .iter()
            .map(|(name, color, description)| Label {
                name: name.to_string(),
                color: color.to_string(),
                description: description.to_string(),
            })
            .collect();
        labels.extend(
            product
                .cells
                .iter()
                .filter_map(|name| config.cell(name))
                .map(|cell| Label {
                    name: cell.label.clone(),
                    color: CELL_COLOR.to_string(),
                    description: format!("cell「{}」が拾う", cell.name),
                }),
        );
        let repos = std::iter::once(&product.issues).chain(&product.repos);
        for repo in repos {
            let entry = by_repo.entry(repo.clone()).or_default();
            for label in &labels {
                if !entry.iter().any(|l| l.name == label.name) {
                    entry.push(label.clone());
                }
            }
        }
    }
    by_repo
}

/// 足りないラベルを作る。すでにあるラベルは、色や説明を人が変えているかもしれないので触らない。
/// 作った（`dry_run` なら作る予定の）ラベルを、リポジトリごとに返す。
pub fn sync(
    config: &Config,
    probe: &dyn Probe,
    dry_run: bool,
) -> Result<Vec<(RepoName, Vec<Label>)>> {
    let mut result = Vec::new();
    for (repo, labels) in required(config) {
        let existing = existing_labels(&repo, probe)?;
        let missing: Vec<Label> = labels
            .into_iter()
            .filter(|l| !existing.contains(&l.name.to_lowercase()))
            .collect();
        if !dry_run && !missing.is_empty() {
            // 読むのはghのアカウントで、作るのは書き込みのアカウント（GitHub Appがあればそれ）で行う
            let credentials = github::credentials(config, probe, &repo, Purpose::Labels)?;
            for label in &missing {
                create_label(&repo, label, probe, &credentials)?;
            }
        }
        result.push((repo, missing));
    }
    Ok(result)
}

/// GitHubのラベル名は大文字と小文字を区別しないので、小文字にそろえて返す。
/// 足りないラベルがあるリポジトリと、そのラベル。`doctor` と `tick` が知らせるのに使う（作りはしない）。
pub fn missing(config: &Config, probe: &dyn Probe) -> Result<Vec<(RepoName, Vec<Label>)>> {
    Ok(sync(config, probe, true)?
        .into_iter()
        .filter(|(_, labels)| !labels.is_empty())
        .collect())
}

/// 足りないラベルを、1つのリポジトリにつき1行で並べる。
pub fn describe_missing(missing: &[(RepoName, Vec<Label>)]) -> String {
    missing
        .iter()
        .map(|(repo, labels)| {
            let names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
            format!("{repo}: {}", names.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn existing_labels(repo: &RepoName, probe: &dyn Probe) -> Result<HashSet<String>> {
    let repo_arg = repo.to_string();
    let args = [
        "label", "list", "--repo", &repo_arg, "--json", "name", "--limit", "1000",
    ];
    let out = run_gh(probe, &args, repo, &[])?;
    let names: Vec<serde_json::Value> = serde_json::from_str(&out).map_err(|e| {
        Error::new(format!("{repo} のラベル一覧を読めません: {e}"))
            .with_hint("ghのバージョンが古い可能性があります。ghを新しくしてください")
    })?;
    Ok(names
        .iter()
        .filter_map(|v| v["name"].as_str())
        .map(str::to_lowercase)
        .collect())
}

fn create_label(
    repo: &RepoName,
    label: &Label,
    probe: &dyn Probe,
    credentials: &Credentials,
) -> Result<()> {
    let repo_arg = repo.to_string();
    let args = [
        "label",
        "create",
        &label.name,
        "--repo",
        &repo_arg,
        "--color",
        &label.color,
        "--description",
        &label.description,
    ];
    run_gh(probe, &args, repo, &credentials.gh_env()?).map(|_| ())
}

fn run_gh(
    probe: &dyn Probe,
    args: &[&str],
    repo: &RepoName,
    env: &[(&str, &str)],
) -> Result<String> {
    let out = probe.run_with_env("gh", args, env).ok_or_else(|| {
        Error::new("GitHub CLI（gh）が見つかりません")
            .with_hint("`agentcells doctor` で直し方を確かめてください")
    })?;
    if out.success {
        Ok(out.stdout)
    } else {
        Err(Error::new(format!(
            "`gh {}` が失敗しました: {}",
            args.join(" "),
            out.stderr.trim()
        ))
        .with_hint(format!(
            "{repo} があるか、書き込みのアカウントにそのリポジトリへの権限があるかを `agentcells doctor` で確かめてください"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::fake::FakeProbe;
    use std::path::Path;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        for name in ["triage.md", "review.md"] {
            std::fs::write(dir.path().join(name), "prompt").unwrap();
        }
        let text = r#"
operator = "me"

[[product]]
name = "a"
issues = "octo/issues"
repos = ["octo/app", "octo/issues"]
cells = ["triage"]

[[product]]
name = "b"
issues = "octo/issues"
repos = ["octo/web"]
cells = ["review"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
prompt = "triage.md"

[[cell]]
name = "review"
on = "pull_request"
label = "ac:review"
prompt = "review.md"
"#;
        let config = Config::parse(text, dir.path()).unwrap();
        (dir, config)
    }

    fn names(labels: &[Label]) -> Vec<&str> {
        labels.iter().map(|l| l.name.as_str()).collect()
    }

    #[test]
    fn required_labels_per_repo_are_merged_across_products() {
        let (_dir, config) = config();
        let required = required(&config);

        let issues = &required[&RepoName::parse("octo/issues").unwrap()];
        assert_eq!(
            names(issues),
            vec![WORKING, NEEDS_HUMAN, "ac:triage", "ac:review"]
        );
        let app = &required[&RepoName::parse("octo/app").unwrap()];
        assert_eq!(names(app), vec![WORKING, NEEDS_HUMAN, "ac:triage"]);
        let web = &required[&RepoName::parse("octo/web").unwrap()];
        assert_eq!(names(web), vec![WORKING, NEEDS_HUMAN, "ac:review"]);
    }

    #[test]
    fn system_labels_are_case_insensitive() {
        assert!(is_system("ac:working"));
        assert!(is_system("AC:Needs-Human"));
        assert!(!is_system("ac:triage"));
    }

    #[test]
    fn dry_run_lists_missing_labels_without_creating() {
        let (_dir, config) = config();
        let probe = FakeProbe::default().with_fallback(true).with(
            "gh label list --repo octo/issues --json name --limit 1000",
            true,
            r#"[{"name":"AC:Working"},{"name":"bug"}]"#,
        );
        let probe = with_empty_list(probe, &["octo/app", "octo/web"]);

        let result = sync(&config, &probe, true).unwrap();

        let issues = result
            .iter()
            .find(|(r, _)| r.to_string() == "octo/issues")
            .unwrap();
        assert_eq!(
            names(&issues.1),
            vec![NEEDS_HUMAN, "ac:triage", "ac:review"]
        );
        assert!(
            !probe
                .calls
                .borrow()
                .iter()
                .any(|c| c.contains("label create"))
        );
    }

    #[test]
    fn sync_creates_only_missing_labels() {
        let (_dir, config) = config();
        let probe = FakeProbe::default()
            .with_fallback(true)
            .with("gh auth token --hostname github.com", true, "gho_me\n")
            .with(
                "gh label list --repo octo/app --json name --limit 1000",
                true,
                r#"[{"name":"ac:working"},{"name":"ac:needs-human"}]"#,
            );
        let probe = with_empty_list(probe, &["octo/issues", "octo/web"]);

        sync(&config, &probe, false).unwrap();

        // Appがなくても、ghのアカウントのトークンを環境変数で渡して作る
        for (call, env) in probe.calls.borrow().iter().zip(probe.envs.borrow().iter()) {
            if call.starts_with("gh label create") {
                assert!(env.contains(&("GH_TOKEN".to_string(), "gho_me".to_string())));
            }
        }
        let creates: Vec<String> = probe
            .calls
            .borrow()
            .iter()
            .filter(|c| c.contains("label create"))
            .cloned()
            .collect();
        let app_creates: Vec<_> = creates
            .iter()
            .filter(|c| c.contains("--repo octo/app "))
            .collect();
        assert_eq!(app_creates.len(), 1);
        assert!(app_creates[0].starts_with("gh label create ac:triage "));
        // octo/issues に4つ、octo/app に1つ、octo/web に3つ
        assert_eq!(creates.len(), 8);
    }

    #[test]
    fn gh_failure_mentions_permissions() {
        let (_dir, config) = config();
        let probe = FakeProbe::default().with_fallback(false);
        let err = sync(&config, &probe, true).unwrap_err();
        assert!(err.hint.unwrap().contains("agentcells doctor"));
    }

    #[test]
    fn sync_creates_labels_as_the_github_app() {
        let (_dir, mut config) = config();
        config.github_app = Some(crate::config::GithubApp {
            id: "Iv23liTEST".to_string(),
            private_key: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("testdata/github-app-test-key.pem"),
        });
        let mut probe = with_empty_list(
            FakeProbe::default().with_fallback(true),
            &["octo/app", "octo/issues", "octo/web"],
        );
        for (i, repo) in ["octo/app", "octo/issues", "octo/web"].iter().enumerate() {
            let name = repo.split('/').nth(1).unwrap();
            probe = crate::github::fake::with_installation(
                probe,
                repo,
                i as u64,
                Purpose::Labels,
                &format!("ghs_{name}"),
            );
        }

        sync(&config, &probe, false).unwrap();

        let calls = probe.calls.borrow();
        let envs = probe.envs.borrow();
        let mut creates = 0;
        for (call, env) in calls.iter().zip(envs.iter()) {
            if call.starts_with("gh label create") {
                creates += 1;
                let repo = call.split("--repo octo/").nth(1).unwrap();
                let name = repo.split(' ').next().unwrap();
                assert!(
                    env.contains(&("GH_TOKEN".to_string(), format!("ghs_{name}"))),
                    "{call}: {env:?}"
                );
            } else if call.starts_with("gh label list") {
                // 読むのはghのアカウントのまま
                assert!(env.is_empty(), "{call}: {env:?}");
            }
        }
        // ラベルがまだ1つもないので、リポジトリごとに要るラベルをすべて作る
        let expected: usize = required(&config).values().map(Vec::len).sum();
        assert_eq!(creates, expected);
    }

    #[test]
    fn missing_gh_points_to_doctor() {
        let (_dir, config) = config();
        let err = sync(&config, &FakeProbe::default(), true).unwrap_err();
        assert!(err.hint.unwrap().contains("agentcells doctor"));
    }

    #[test]
    fn missing_lists_only_repos_that_lack_labels() {
        let (_dir, config) = config();
        let probe = with_empty_list(FakeProbe::default(), &["octo/app", "octo/web"]).with(
            "gh label list --repo octo/issues --json name --limit 1000",
            true,
            r#"[{"name":"ac:working"},{"name":"ac:needs-human"},{"name":"ac:triage"},{"name":"ac:implement"},{"name":"ac:review"}]"#,
        );
        let missing = missing(&config, &probe).unwrap();
        let repos: Vec<String> = missing.iter().map(|(r, _)| r.to_string()).collect();
        assert!(!repos.contains(&"octo/issues".to_string()), "{repos:?}");
        assert!(describe_missing(&missing).contains("ac:working"));
    }

    #[test]
    fn empty_config_needs_nothing() {
        let config = Config::parse("", Path::new("/")).unwrap();
        assert!(required(&config).is_empty());
    }

    fn with_empty_list(mut probe: FakeProbe, repos: &[&str]) -> FakeProbe {
        for repo in repos {
            probe = probe.with(
                &format!("gh label list --repo {repo} --json name --limit 1000"),
                true,
                "[]",
            );
        }
        probe
    }
}
