//! `claude -p --settings` に渡す、cellとして動くエージェントの設定。

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::home::Home;

/// AIにIssue/PRのラベルや状態を変えさせないためのdeny。ラベルの付け替えはCLIだけがする。
/// コメントとレビューもCLIが目印（`[agentcells] <cell> round <n>`）を付けて投稿するので、AIには書かせない。
/// ブランチのpushとPRの作成もCLIがするので、AIはworktreeの中でコミットするところまでにする。
/// `gh api` は何でもできてしまうので、まとめて止める。
/// `gh auth` は、ghのアカウントで動くときに `gh auth token` でトークンを取り出せるので止める。
pub const DENY_GH: &[&str] = &[
    "Bash(gh auth:*)",
    "Bash(git push:*)",
    "Bash(gh pr create:*)",
    "Bash(gh issue comment:*)",
    "Bash(gh pr comment:*)",
    "Bash(gh pr review:*)",
    "Bash(gh issue edit:*)",
    "Bash(gh issue close:*)",
    "Bash(gh issue reopen:*)",
    "Bash(gh issue delete:*)",
    "Bash(gh pr edit:*)",
    "Bash(gh pr close:*)",
    "Bash(gh pr reopen:*)",
    "Bash(gh pr merge:*)",
    "Bash(gh label:*)",
    "Bash(gh api:*)",
];

/// AIに許すツール。読むのは自由にし、書き換えは次の2つの道だけにする。
/// - ファイルの作成と更新: Edit・Writeを、その仕事のworktreeの中だけに許す（`runtime_settings` で足す）
/// - それ以外（削除、移動、コミット、gitを読む、ビルドやテスト）: `agentcells cell` を通す。CLIが範囲を確かめる
///
/// Bashは `agentcells cell` だけを許す。`ls` や `git diff` のようなコマンドでも、オプションでファイルに
/// 書けるものがある（`git diff --output` など）ので許さない。Claude Codeは、許したコマンドに
/// `>`、`;`、`&&`、`$(...)` で別の操作を付け足すことを止める（2.1.310で確かめた）。
pub const ALLOW_BASE: &[&str] = &["Read", "Glob", "Grep", "Bash(agentcells cell:*)"];

/// 1回の実行で渡す設定を作る。
///
/// `worktree` の中だけに、ファイルの作成と更新（Edit、Write）を許す。ただし `.git` と、置いた秘密のファイルは除く。
///
/// 次のファイルはAIに読ませない。
/// - `placed_files`: worktreeに置いた秘密のファイル（絶対パス）
/// - `files/` の下: worktreeに置くファイルの元
/// - GitHub Appの秘密鍵: 読まれると、インストール先のリポジトリへのトークンをいくらでも作れる
/// - `config.toml`: 秘密鍵の場所などが書いてある
/// - ghの設定ディレクトリ: keyringを使わない環境では、`hosts.yml` にトークンが平文で入っている
///
/// 信頼していないディレクトリではprojectの設定のallowが無視されるので、cellに要るallowもここで渡す。
pub fn runtime_settings(
    home: &Home,
    config: &Config,
    worktree: &Path,
    placed_files: &[PathBuf],
) -> Result<serde_json::Value> {
    let mut allow: Vec<String> = ALLOW_BASE.iter().map(|s| s.to_string()).collect();
    for tool in ["Edit", "Write"] {
        rule(&mut allow, tool, worktree, "/**")?;
    }

    let mut deny: Vec<String> = DENY_GH.iter().map(|s| s.to_string()).collect();
    // .git（linked worktreeではリポジトリを指すファイル）を書き換えさせない
    for tool in ["Edit", "Write"] {
        rule(&mut deny, tool, &worktree.join(".git"), "")?;
        rule(&mut deny, tool, &worktree.join(".git"), "/**")?;
    }
    for path in placed_files {
        for tool in ["Read", "Edit", "Write"] {
            rule(&mut deny, tool, path, "")?;
        }
    }
    rule(&mut deny, "Read", &home.files_dir(), "/**")?;
    rule(&mut deny, "Read", &home.config_path(), "")?;
    if let Some(app) = &config.github_app {
        rule(&mut deny, "Read", &app.private_key, "")?;
    }
    if let Some(dir) = gh_config_dir(|key| std::env::var_os(key)) {
        rule(&mut deny, "Read", &dir, "/**")?;
    }
    Ok(json!({ "permissions": { "allow": allow, "deny": deny } }))
}

/// ghの設定ディレクトリ。ghと同じく `GH_CONFIG_DIR`、`XDG_CONFIG_HOME/gh`、`~/.config/gh` の順に探す。
fn gh_config_dir(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let absolute = |p: PathBuf| p.is_absolute().then_some(p);
    if let Some(dir) = var("GH_CONFIG_DIR").map(PathBuf::from) {
        return absolute(dir);
    }
    if let Some(dir) = var("XDG_CONFIG_HOME").map(PathBuf::from) {
        return absolute(dir.join("gh"));
    }
    var("HOME")
        .map(PathBuf::from)
        .and_then(|home| absolute(home.join(".config/gh")))
}

/// `Read(//絶対パス)` の形で、ツールとパスの決まりを足す（先頭の / を重ねる）。
///
/// Claude Codeが比べるパスと形が違うと効かないので、書いたままのパスと、
/// シンボリックリンクや `..` を解いたパス（macOSの /var → /private/var など）の両方を足す。
fn rule(rules: &mut Vec<String>, tool: &str, path: &Path, suffix: &str) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::new(format!(
            "許可や禁止に書くパスは絶対パスにしてください: {}",
            path.display()
        )));
    }
    let mut paths = vec![path.to_path_buf()];
    if let Some(resolved) = resolve(path)
        && resolved != path
    {
        paths.push(resolved);
    }
    for p in paths {
        let rule = format!("{tool}(/{}{suffix})", p.display());
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    }
    Ok(())
}

/// シンボリックリンクと `..` を解く。まだないファイルは、親ディレクトリを解いてつなぐ。
fn resolve(path: &Path) -> Option<PathBuf> {
    if let Ok(p) = path.canonicalize() {
        return Some(p);
    }
    let parent = path.parent()?.canonicalize().ok()?;
    Some(parent.join(path.file_name()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GithubApp;

    fn home() -> Home {
        Home::new(PathBuf::from("/home/me/.agentcells"))
    }

    fn rules_of(config: &Config, placed: &[PathBuf], kind: &str) -> Vec<String> {
        runtime_settings(&home(), config, Path::new("/w/octo/app/12"), placed).unwrap()
            ["permissions"][kind]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap().to_string())
            .collect()
    }

    fn deny_of(config: &Config, placed: &[PathBuf]) -> Vec<String> {
        rules_of(config, placed, "deny")
    }

    #[test]
    fn allows_reading_and_writing_only_inside_the_worktree() {
        let allow = rules_of(&Config::default(), &[], "allow");
        assert_eq!(
            allow,
            vec![
                "Read",
                "Glob",
                "Grep",
                "Bash(agentcells cell:*)",
                "Edit(//w/octo/app/12/**)",
                "Write(//w/octo/app/12/**)",
            ]
        );
        let deny = deny_of(
            &Config::default(),
            &[PathBuf::from("/w/octo/app/12/key.jks")],
        );
        for rule in [
            "Edit(//w/octo/app/12/.git)",
            "Write(//w/octo/app/12/.git/**)",
            "Edit(//w/octo/app/12/key.jks)",
            "Write(//w/octo/app/12/key.jks)",
        ] {
            assert!(deny.contains(&rule.to_string()), "{rule} should be denied");
        }
    }

    #[test]
    fn denies_label_and_state_changes_through_gh() {
        let deny = deny_of(&Config::default(), &[]);
        for rule in [
            "Bash(gh issue edit:*)",
            "Bash(gh pr edit:*)",
            "Bash(gh label:*)",
            "Bash(gh api:*)",
            "Bash(gh pr comment:*)",
            "Bash(gh pr review:*)",
            "Bash(gh pr create:*)",
            "Bash(git push:*)",
            "Bash(gh auth:*)",
        ] {
            assert!(deny.iter().any(|d| d == rule), "{rule} should be denied");
        }
        // 読むだけのコマンドは止めない
        assert!(!deny.iter().any(|d| d.contains("pr view")));
    }

    #[test]
    fn denies_reading_secret_files_by_absolute_path() {
        let deny = deny_of(
            &Config::default(),
            &[PathBuf::from("/work/app/local.properties")],
        );
        assert!(deny.contains(&"Read(//work/app/local.properties)".to_string()));
    }

    #[test]
    fn denies_reading_the_app_key_config_and_file_sources() {
        let config = Config {
            github_app: Some(GithubApp {
                id: "Iv23li".to_string(),
                private_key: PathBuf::from("/home/me/.agentcells/github-app.pem"),
            }),
            ..Config::default()
        };
        let deny = deny_of(&config, &[]);
        for rule in [
            "Read(//home/me/.agentcells/github-app.pem)",
            "Read(//home/me/.agentcells/config.toml)",
            "Read(//home/me/.agentcells/files/**)",
        ] {
            assert!(deny.contains(&rule.to_string()), "{rule} should be denied");
        }
    }

    #[cfg(unix)]
    #[test]
    fn denies_both_the_written_and_the_resolved_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("app.pem"), "key").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let config = Config {
            github_app: Some(GithubApp {
                id: "Iv23li".to_string(),
                // config.toml からの相対パスで書くと .. が残る
                private_key: link.join("../link/app.pem"),
            }),
            ..Config::default()
        };
        let home = Home::new(link.clone());
        let deny: Vec<String> =
            runtime_settings(&home, &config, &link, &[]).unwrap()["permissions"]["deny"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d.as_str().unwrap().to_string())
                .collect();

        let resolved = real.canonicalize().unwrap();
        for rule in [
            format!("Read(/{}/app.pem)", resolved.display()),
            format!("Read(/{}/files/**)", resolved.display()),
            format!("Read(/{}/config.toml)", resolved.display()),
            format!("Read(/{}/config.toml)", link.display()),
        ] {
            assert!(deny.contains(&rule), "{rule} should be denied: {deny:?}");
        }
    }

    #[test]
    fn finds_the_gh_config_dir_like_gh_does() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }
        };
        assert_eq!(
            gh_config_dir(env(&[("GH_CONFIG_DIR", "/x/gh"), ("HOME", "/home/me")])),
            Some(PathBuf::from("/x/gh"))
        );
        assert_eq!(
            gh_config_dir(env(&[
                ("XDG_CONFIG_HOME", "/x/config"),
                ("HOME", "/home/me")
            ])),
            Some(PathBuf::from("/x/config/gh"))
        );
        assert_eq!(
            gh_config_dir(env(&[("HOME", "/home/me")])),
            Some(PathBuf::from("/home/me/.config/gh"))
        );
        assert_eq!(gh_config_dir(env(&[("GH_CONFIG_DIR", "relative")])), None);
    }

    #[test]
    fn rejects_relative_secret_paths() {
        assert!(
            runtime_settings(
                &home(),
                &Config::default(),
                Path::new("/w"),
                &[PathBuf::from("local.properties")]
            )
            .is_err()
        );
    }
}
