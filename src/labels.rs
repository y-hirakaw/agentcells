//! agentcellsが使うGitHubのラベル。

use std::collections::{BTreeMap, HashSet};

use crate::config::Config;
use crate::error::{Error, Result};
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
        if !dry_run {
            for label in &missing {
                create_label(&repo, label, probe)?;
            }
        }
        result.push((repo, missing));
    }
    Ok(result)
}

/// GitHubのラベル名は大文字と小文字を区別しないので、小文字にそろえて返す。
fn existing_labels(repo: &RepoName, probe: &dyn Probe) -> Result<HashSet<String>> {
    let repo_arg = repo.to_string();
    let args = [
        "label", "list", "--repo", &repo_arg, "--json", "name", "--limit", "1000",
    ];
    let out = run_gh(probe, &args, repo)?;
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

fn create_label(repo: &RepoName, label: &Label, probe: &dyn Probe) -> Result<()> {
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
    run_gh(probe, &args, repo).map(|_| ())
}

fn run_gh(probe: &dyn Probe, args: &[&str], repo: &RepoName) -> Result<String> {
    let out = probe.run("gh", args).ok_or_else(|| {
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
            "{repo} があるか、`gh auth status` のアカウントにそのリポジトリへの書き込み権限があるかを確かめてください"
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
        let probe = FakeProbe::default().with_fallback(true).with(
            "gh label list --repo octo/app --json name --limit 1000",
            true,
            r#"[{"name":"ac:working"},{"name":"ac:needs-human"}]"#,
        );
        let probe = with_empty_list(probe, &["octo/issues", "octo/web"]);

        sync(&config, &probe, false).unwrap();

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
        assert!(err.hint.unwrap().contains("gh auth status"));
    }

    #[test]
    fn missing_gh_points_to_doctor() {
        let (_dir, config) = config();
        let err = sync(&config, &FakeProbe::default(), true).unwrap_err();
        assert!(err.hint.unwrap().contains("agentcells doctor"));
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
