//! IssueからPRを作る。
//!
//! AIはworktreeの中でコミットするところまでで、ブランチのpushとPRの作成はCLIがする。
//! ブランチ名とPRの本文の形をCLIが決めるので、目印やIssueへのリンクがそろう。

use crate::home::RepoName;
use crate::marker;

/// Issueの作業に使うブランチ名。Issueのリポジトリと作業するリポジトリが違っても、ぶつからないようにする。
pub fn branch_name(issues: &RepoName, number: u64) -> String {
    format!("ac/{}-{}-{number}", issues.owner, issues.name)
}

/// PRの本文。1行目に目印を書き、Issueが閉じられるように `Closes owner/repo#番号` を入れる。
pub fn body(cell: &str, round: u32, summary: &str, issues: &RepoName, number: u64) -> String {
    format!(
        "{}\n\nCloses {issues}#{number}",
        marker::comment(cell, round, summary)
    )
}

/// `gh pr create` の引数。
pub fn create_args(
    repo: &RepoName,
    base: &str,
    branch: &str,
    title: &str,
    body: &str,
) -> Vec<String> {
    [
        "pr",
        "create",
        "--repo",
        &repo.to_string(),
        "--base",
        base,
        "--head",
        branch,
        "--title",
        title,
        "--body",
        body,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(s: &str) -> RepoName {
        RepoName::parse(s).unwrap()
    }

    #[test]
    fn branch_name_includes_the_issue_repo() {
        assert_eq!(
            branch_name(&repo("y-hirakaw/agentcells"), 12),
            "ac/y-hirakaw-agentcells-12"
        );
    }

    #[test]
    fn body_starts_with_marker_and_closes_the_issue() {
        let b = body(
            "implement",
            1,
            "  ログインの不具合を直しました\n",
            &repo("octo/issues"),
            7,
        );
        assert!(b.starts_with("[agentcells] implement round 1\n\nログインの不具合を直しました"));
        assert!(b.ends_with("Closes octo/issues#7"));
        assert_eq!(marker::parse(&b), Some(("implement".to_string(), 1)));
    }

    #[test]
    fn create_args_target_the_work_repo() {
        let args = create_args(
            &repo("octo/app"),
            "main",
            "ac/octo-issues-7",
            "タイトル",
            "本文",
        );
        assert_eq!(
            args,
            vec![
                "pr",
                "create",
                "--repo",
                "octo/app",
                "--base",
                "main",
                "--head",
                "ac/octo-issues-7",
                "--title",
                "タイトル",
                "--body",
                "本文"
            ]
        );
    }
}
