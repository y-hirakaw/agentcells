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
pub const DENY_GH: &[&str] = &[
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
///
/// 信頼していないディレクトリではprojectの設定のallowが無視されるので、cellに要るallowもここで渡す。
pub fn runtime_settings(
    home: &Home,
    config: &Config,
    placed_files: &[PathBuf],
) -> Result<serde_json::Value> {
    let mut deny: Vec<String> = DENY_GH.iter().map(|s| s.to_string()).collect();
    for path in placed_files {
        deny.push(deny_read(path)?);
    }
    deny.push(deny_read(&home.files_dir().join("**"))?);
    deny.push(deny_read(&home.config_path())?);
    if let Some(app) = &config.github_app {
        deny.push(deny_read(&app.private_key)?);
    }
    Ok(json!({ "permissions": { "deny": deny } }))
}

/// 絶対パスは `Read(//絶対パス)` の形で書く（先頭の / を重ねる）。
fn deny_read(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        return Err(Error::new(format!(
            "Readを禁止するパスは絶対パスにしてください: {}",
            path.display()
        )));
    }
    Ok(format!("Read(/{})", path.display()))
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
