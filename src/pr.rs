//! IssueからPRを作る。
//!
//! AIはworktreeの中でコミットするところまでで、ブランチのpushとPRの作成はCLIがする。
//! ブランチ名とPRの本文の形をCLIが決めるので、目印やIssueへのリンクがそろう。

use std::path::Path;

use tempfile::TempDir;

use crate::error::{Error, Result};
use crate::github::{Credentials, ISOLATED_GIT_ENV};
use crate::home::RepoName;
use crate::marker;
use crate::probe::Probe;

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

/// GitHubのリポジトリのURL。
pub fn remote_url(repo: &RepoName) -> String {
    format!("https://github.com/{repo}.git")
}

/// worktreeのブランチをGitHubにpushする。
///
/// AIはworktreeの `.git/config` を書き換えられる。`http.<URL>.sslVerify` やproxyは `-c` でも上書きできないので、
/// トークンを使うpushはworktreeからはしない。CLIがその場で作ったbareリポジトリにトークンなしでブランチを移し、
/// そこからpushする。
pub fn push_branch(
    probe: &dyn Probe,
    credentials: &Credentials,
    worktree: &Path,
    repo: &RepoName,
    branch: &str,
) -> Result<()> {
    let staging = stage(probe, worktree, branch)?;
    push_staged(probe, credentials, staging.path(), repo, branch)
}

/// worktreeのブランチを、新しく作ったbareリポジトリに移す。トークンは渡さない。
fn stage(probe: &dyn Probe, worktree: &Path, branch: &str) -> Result<TempDir> {
    let staging = tempfile::Builder::new()
        .prefix("agentcells-push-")
        .tempdir()
        .map_err(|e| Error::new(format!("pushのための一時ディレクトリを作れません: {e}")))?;
    let mut env = ISOLATED_GIT_ENV.to_vec();
    env.push(("GIT_ALLOW_PROTOCOL", "file"));
    let staging_path = path_str(staging.path())?;
    let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
    git(probe, &["init", "-q", "--bare", staging_path], &env)?;
    git(
        probe,
        &[
            "-C",
            staging_path,
            "fetch",
            "-q",
            "--no-tags",
            path_str(worktree)?,
            &refspec,
        ],
        &env,
    )?;
    Ok(staging)
}

/// bareリポジトリに移したブランチを、トークンを使ってGitHubにpushする。
fn push_staged(
    probe: &dyn Probe,
    credentials: &Credentials,
    staging: &Path,
    repo: &RepoName,
    branch: &str,
) -> Result<()> {
    let url = remote_url(repo);
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let git_args = credentials.git_args();
    let mut args: Vec<&str> = vec!["-C", path_str(staging)?];
    args.extend(git_args.iter().map(String::as_str));
    args.extend(["push", "-q", &url, &refspec]);
    git(probe, &args, &credentials.git_env())
}

fn git(probe: &dyn Probe, args: &[&str], env: &[(&str, &str)]) -> Result<()> {
    let out = probe.run_with_env("git", args, env).ok_or_else(|| {
        Error::new("gitが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if out.success {
        Ok(())
    } else {
        Err(Error::new(format!(
            "gitが失敗しました: {}",
            out.stderr.trim()
        )))
    }
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::new(format!("パスに使えない文字があります: {}", path.display())))
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

    /// 本物のgitで、AIが書き換えたworktreeの設定がpushに効かないことを確かめる。
    #[cfg(unix)]
    mod push {
        use super::*;
        use crate::probe::SystemProbe;
        use std::process::Command;

        fn git_in(dir: &Path, args: &[&str]) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn token() -> Credentials {
            Credentials::App {
                token: "ghs_test".to_string(),
                gh_config: tempfile::tempdir().unwrap(),
            }
        }

        /// トークンを環境変数から書き出すスクリプト。これが動いたら漏れたことになる。
        fn leak_script(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
            use std::os::unix::fs::PermissionsExt;
            let marker = dir.join("leaked");
            let script = dir.join("leak.sh");
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\necho \"$AGENTCELLS_GITHUB_TOKEN\" > '{}'\nexit 1\n",
                    marker.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            (script, marker)
        }

        /// AIが書き換えたかもしれない設定を持つworktree。
        fn tampered_worktree(dir: &Path) -> std::path::PathBuf {
            let work = dir.join("work");
            git_in(dir, &["init", "-q", "work"]);
            git_in(&work, &["checkout", "-q", "-b", "ac/octo-app-1"]);
            git_in(&work, &["commit", "-q", "--allow-empty", "-m", "x"]);
            let (script, _) = leak_script(dir);
            for (key, value) in [
                ("http.https://github.com/octo/app.git.sslVerify", "false"),
                (
                    "http.https://github.com/octo/app.git.proxy",
                    "http://127.0.0.1:9",
                ),
                ("protocol.ext.allow", "always"),
                (
                    &format!("url.ext::{}.insteadOf", script.display()),
                    "https://github.com/",
                ),
            ] {
                git_in(&work, &["config", key, value]);
            }
            work
        }

        #[test]
        fn stage_copies_only_the_branch_not_the_config() {
            let dir = tempfile::tempdir().unwrap();
            let work = tampered_worktree(dir.path());

            let staging = stage(&SystemProbe, &work, "ac/octo-app-1").unwrap();
            assert_eq!(
                git_in(staging.path(), &["rev-parse", "refs/heads/ac/octo-app-1"]),
                git_in(&work, &["rev-parse", "HEAD"])
            );
            let config = std::fs::read_to_string(staging.path().join("config")).unwrap();
            for word in ["sslVerify", "proxy", "insteadOf", "ext"] {
                assert!(!config.contains(word), "{word} in {config}");
            }
        }

        #[test]
        fn push_does_not_run_ext_commands_even_if_configured() {
            // stageしたリポジトリは毎回新しいので本来はこの設定を持たないが、持っていても漏れないことを確かめる
            let dir = tempfile::tempdir().unwrap();
            let work = tampered_worktree(dir.path());
            let staging = dir.path().join("work/.git");

            let err = push_staged(
                &SystemProbe,
                &token(),
                &staging,
                &RepoName::parse("octo/app").unwrap(),
                "ac/octo-app-1",
            )
            .unwrap_err();
            assert!(err.message.contains("not allowed"), "{}", err.message);
            assert!(!dir.path().join("leaked").exists());
            drop(work);
        }
    }
}
