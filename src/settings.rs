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

/// 1回の実行で渡す設定を作る。
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
    placed_files: &[PathBuf],
) -> Result<serde_json::Value> {
    let mut deny: Vec<String> = DENY_GH.iter().map(|s| s.to_string()).collect();
    for path in placed_files {
        deny_read(&mut deny, path, "")?;
    }
    deny_read(&mut deny, &home.files_dir(), "/**")?;
    deny_read(&mut deny, &home.config_path(), "")?;
    if let Some(app) = &config.github_app {
        deny_read(&mut deny, &app.private_key, "")?;
    }
    if let Some(dir) = gh_config_dir(|key| std::env::var_os(key)) {
        deny_read(&mut deny, &dir, "/**")?;
    }
    Ok(json!({ "permissions": { "deny": deny } }))
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

/// `Read(//絶対パス)` の形でdenyを足す（先頭の / を重ねる）。
///
/// Claude Codeが比べるパスと形が違うと効かないので、書いたままのパスと、
/// シンボリックリンクや `..` を解いたパス（macOSの /var → /private/var など）の両方を足す。
fn deny_read(deny: &mut Vec<String>, path: &Path, suffix: &str) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::new(format!(
            "Readを禁止するパスは絶対パスにしてください: {}",
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
        let rule = format!("Read(/{}{suffix})", p.display());
        if !deny.contains(&rule) {
            deny.push(rule);
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

    fn deny_of(config: &Config, placed: &[PathBuf]) -> Vec<String> {
        runtime_settings(&home(), config, placed).unwrap()["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap().to_string())
            .collect()
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
            runtime_settings(&home, &config, &[]).unwrap()["permissions"]["deny"]
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
                &[PathBuf::from("local.properties")]
            )
            .is_err()
        );
    }
}
