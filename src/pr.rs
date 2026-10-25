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
    assignee: &str,
) -> Vec<String> {
    // cellを使う人を割り当てる。agentcellsが作ったPRは、割り当てたのがagentcellsでも、次のcellが動ける（guard）
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
        "--assignee",
        assignee,
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
/// 移した先の設定はCLIが作ったまっさらなものなので、push の前の確かめもここでする。
pub fn stage(probe: &dyn Probe, worktree: &Path, branch: &str) -> Result<TempDir> {
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
pub fn push_staged(
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

/// 人がそのブランチを開いたときに、自動で動くもの。AIのビルドが書いても、コミットに入れさせない。
/// `.github/workflows` は、GitHub Appに権限を付けていないうえ、pushされるとCIで動く。
pub const AUTORUN_PATHS: &[&str] = &[
    ".claude",
    ".mcp.json",
    ".vscode/tasks.json",
    ".vscode/settings.json",
    ".devcontainer",
    ".envrc",
    ".husky",
    ".github/workflows",
];

/// pushする前に、ブランチに足したコミット（`base` から `branch` まで）をすべて確かめる。
/// AIはsandboxの中から、`agentcells cell commit` を通さずにコミットを作れるので、ここで見る。
/// 問題があれば、その理由（秘密そのものは書かない）。
///
/// - 置いた秘密のファイルと同じ中身のファイル（名前を変えたり、途中のコミットだけに入れたりしても）
/// - 人が開いたときに自動で動くファイル（`AUTORUN_PATHS`）の変更
/// - betterleaksが見つけた秘密らしいもの（足した行。途中のコミットで足して消したものも）
pub fn verify(
    probe: &dyn Probe,
    staging: &Path,
    base: &str,
    branch: &str,
    placed_blobs: &[String],
    scanner: &Path,
) -> Result<Option<String>> {
    let staging_path = path_str(staging)?;
    let range = format!("{base}..refs/heads/{branch}");
    let objects = git_out(
        probe,
        &["-C", staging_path, "rev-list", "--objects", &range],
        ISOLATED_GIT_ENV,
    )?;
    for line in objects.lines() {
        let mut parts = line.splitn(2, ' ');
        let (Some(sha), path) = (parts.next(), parts.next()) else {
            continue;
        };
        if placed_blobs.iter().any(|b| b == sha) {
            return Ok(Some(format!(
                "置いた秘密のファイルと同じ中身のファイル（{}）がコミットに入っています",
                path.unwrap_or("?")
            )));
        }
    }
    let changed = git_out(
        probe,
        &[
            "-C",
            staging_path,
            "-c",
            "core.quotePath=false",
            "log",
            "--no-renames",
            "--name-only",
            "--format=",
            &range,
        ],
        ISOLATED_GIT_ENV,
    )?;
    for path in changed.lines().filter(|l| !l.is_empty()) {
        let lower = path.to_lowercase();
        if let Some(guarded) = AUTORUN_PATHS
            .iter()
            .find(|g| lower == **g || lower.starts_with(&format!("{g}/")))
        {
            return Ok(Some(format!(
                "人が開いたときに自動で動くファイル（{guarded}）を変えるコミットがあります: {path}"
            )));
        }
    }
    let patches = git_out(
        probe,
        &[
            "-C",
            staging_path,
            "-c",
            "core.quotePath=false",
            "log",
            "-p",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--text",
            "--no-renames",
            "--unified=0",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--format=",
            &range,
        ],
        ISOLATED_GIT_ENV,
    )?;
    let lines = crate::actions::added_lines(&patches);
    if !lines.is_empty() {
        let input: String = lines.iter().map(|l| format!("{}\n", l.text)).collect();
        let found = crate::actions::scan_text(scanner, &input)?;
        if let Some((line, rule)) = found.first() {
            let place = lines
                .get(line.wrapping_sub(1))
                .map(|l| format!("{}:{}", l.path, l.line))
                .unwrap_or_else(|| "場所不明".to_string());
            return Ok(Some(format!(
                "コミットに秘密らしいもの（{rule}、{place}）が入っています"
            )));
        }
    }
    Ok(None)
}

/// ブランチから開いているPRの番号。なければ `None`。
pub fn find_open(
    probe: &dyn Probe,
    credentials: &Credentials,
    repo: &RepoName,
    branch: &str,
) -> Result<Option<u64>> {
    let repo_arg = repo.to_string();
    let out = gh(
        probe,
        credentials,
        &[
            "pr",
            "list",
            "--repo",
            &repo_arg,
            "--head",
            branch,
            "--state",
            "open",
            "--json",
            "number",
            "--jq",
            ".[0].number",
        ],
    )?;
    let number = out.trim();
    if number.is_empty() || number == "null" {
        return Ok(None);
    }
    number
        .parse()
        .map(Some)
        .map_err(|e| Error::new(format!("PRの番号を読めません（{number}）: {e}")))
}

/// PRを作り、その番号を返す（`gh pr create` はPRのURLを出す）。
pub fn create(probe: &dyn Probe, credentials: &Credentials, args: &[String]) -> Result<u64> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = gh(probe, credentials, &args)?;
    out.trim()
        .rsplit('/')
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| Error::new(format!("作ったPRの番号を読めません: {}", out.trim())))
}

fn gh(probe: &dyn Probe, credentials: &Credentials, args: &[&str]) -> Result<String> {
    let env = credentials.gh_env()?;
    let out = probe.run_with_env("gh", args, &env).ok_or_else(|| {
        Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if !out.success {
        return Err(Error::new(format!(
            "gh {} {} が失敗しました: {}",
            args.first().unwrap_or(&""),
            args.get(1).unwrap_or(&""),
            out.stderr.trim()
        )));
    }
    Ok(out.stdout)
}

fn git_out(probe: &dyn Probe, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let out = probe.run_with_env("git", args, env).ok_or_else(|| {
        Error::new("gitが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if out.success {
        Ok(out.stdout)
    } else {
        Err(Error::new(format!(
            "gitが失敗しました: {}",
            out.stderr.trim()
        )))
    }
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
            "y-hirakaw",
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
                "本文",
                "--assignee",
                "y-hirakaw"
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
            Credentials::new(crate::github::Source::App, "ghs_test".to_string()).unwrap()
        }

        /// 偽のbetterleaks。「FAKE-SECRET」を含む行を見つけたことにする。
        fn fake_scanner(dir: &Path) -> std::path::PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join("fake-betterleaks");
            std::fs::write(
                &path,
                "#!/bin/sh\nn=$(grep -n FAKE-SECRET | head -1 | cut -d: -f1)\nif [ -n \"$n\" ]; then echo \"[{\\\"RuleID\\\":\\\"fake\\\",\\\"StartLine\\\":$n}]\"; exit 1; fi\necho '[]'\n",
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        /// 既定のブランチに1つ、仕事のブランチに `files` を足したコミットを1つ作り、bareリポジトリに移す。
        fn staged(dir: &Path, files: &[(&str, &str)]) -> (TempDir, String) {
            let work = dir.join("work");
            std::fs::create_dir_all(&work).unwrap();
            git_in(&work, &["init", "-q", "-b", "main"]);
            std::fs::write(work.join("a.txt"), "a").unwrap();
            git_in(&work, &["add", "-A"]);
            git_in(&work, &["commit", "-q", "-m", "base"]);
            let base = git_in(&work, &["rev-parse", "HEAD"]);
            git_in(&work, &["checkout", "-q", "-b", "ac/x"]);
            for (path, content) in files {
                let full = work.join(path);
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(full, content).unwrap();
            }
            git_in(&work, &["add", "-A"]);
            git_in(&work, &["commit", "-q", "-m", "change"]);
            (stage(&SystemProbe, &work, "ac/x").unwrap(), base)
        }

        #[test]
        fn verify_passes_ordinary_commits() {
            let dir = tempfile::tempdir().unwrap();
            let scanner = fake_scanner(dir.path());
            let (staging, base) = staged(dir.path(), &[("src/b.txt", "hello")]);
            let found = verify(&SystemProbe, staging.path(), &base, "ac/x", &[], &scanner).unwrap();
            assert_eq!(found, None);
        }

        #[test]
        fn verify_refuses_placed_secrets_autorun_files_and_leaks() {
            let dir = tempfile::tempdir().unwrap();
            let scanner = fake_scanner(dir.path());
            // 置いた秘密のファイルと同じ中身を、別の名前で
            let (staging, base) = staged(dir.path(), &[("copy.txt", "keystore-bytes")]);
            let hash = {
                let out = Command::new("git")
                    .args(["hash-object", "--stdin"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .and_then(|mut c| {
                        use std::io::Write;
                        c.stdin.take().unwrap().write_all(b"keystore-bytes")?;
                        c.wait_with_output()
                    })
                    .unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            let reason = verify(
                &SystemProbe,
                staging.path(),
                &base,
                "ac/x",
                &[hash],
                &scanner,
            )
            .unwrap()
            .unwrap();
            assert!(reason.contains("copy.txt"), "{reason}");

            for (path, content, expect) in [
                (".claude/settings.json", "{}", ".claude"),
                (".github/workflows/ci.yml", "on: push", ".github/workflows"),
                ("src/config.rs", "token = FAKE-SECRET", "fake"),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let scanner = fake_scanner(dir.path());
                let (staging, base) = staged(dir.path(), &[(path, content)]);
                let reason = verify(&SystemProbe, staging.path(), &base, "ac/x", &[], &scanner)
                    .unwrap()
                    .unwrap();
                assert!(reason.contains(expect), "{path}: {reason}");
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
        fn stage_works_from_a_linked_worktree_without_running_its_hooks() {
            let dir = tempfile::tempdir().unwrap();
            let main = dir.path().join("main");
            git_in(dir.path(), &["init", "-q", "main"]);
            git_in(&main, &["commit", "-q", "--allow-empty", "-m", "base"]);
            let linked = dir.path().join("linked");
            git_in(
                &main,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "ac/octo-app-2",
                    linked.to_str().unwrap(),
                ],
            );
            git_in(&linked, &["commit", "-q", "--allow-empty", "-m", "work"]);
            // fetchのときに動くかもしれない設定を、AIが書いたとする
            let marker = dir.path().join("ran");
            let command = format!("touch '{}'", marker.display());
            git_in(&main, &["config", "uploadpack.packObjectsHook", &command]);
            git_in(&main, &["config", "core.fsmonitor", &command]);

            let staging = stage(&SystemProbe, &linked, "ac/octo-app-2").unwrap();
            assert_eq!(
                git_in(staging.path(), &["rev-parse", "refs/heads/ac/octo-app-2"]),
                git_in(&linked, &["rev-parse", "HEAD"])
            );
            assert!(!marker.exists());
        }

        #[test]
        fn push_does_not_run_ext_commands_even_if_configured() {
            // stageしたリポジトリは毎回新しいので本来はこの設定を持たないが、持っていても漏れないことを確かめる
            let dir = tempfile::tempdir().unwrap();
            tampered_worktree(dir.path());
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
        }
    }
}
