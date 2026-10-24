//! `claude -p --settings` に渡す、cellとして動くエージェントの設定。

use std::path::PathBuf;

use serde_json::json;

use crate::error::{Error, Result};

/// AIにIssue/PRのラベルや状態を変えさせないためのdeny。ラベルの付け替えはCLIだけがする。
/// `gh api` は何でもできてしまうので、まとめて止める。
pub const DENY_GH: &[&str] = &[
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

/// 1回の実行で渡す設定を作る。`secret_files` はworktreeに置いた秘密のファイルの絶対パスで、AIに読ませない。
///
/// 信頼していないディレクトリではprojectの設定のallowが無視されるので、cellに要るallowもここで渡す。
pub fn runtime_settings(secret_files: &[PathBuf]) -> Result<serde_json::Value> {
    let mut deny: Vec<String> = DENY_GH.iter().map(|s| s.to_string()).collect();
    for path in secret_files {
        if !path.is_absolute() {
            return Err(Error::new(format!(
                "Readを禁止するパスは絶対パスにしてください: {}",
                path.display()
            )));
        }
        // 絶対パスは `Read(//絶対パス)` の形で書く（先頭の / を重ねる）。
        deny.push(format!("Read(/{})", path.display()));
    }
    Ok(json!({ "permissions": { "deny": deny } }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_label_and_state_changes_through_gh() {
        let settings = runtime_settings(&[]).unwrap();
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        for rule in [
            "Bash(gh issue edit:*)",
            "Bash(gh pr edit:*)",
            "Bash(gh label:*)",
            "Bash(gh api:*)",
        ] {
            assert!(deny.iter().any(|d| d == rule), "{rule} should be denied");
        }
        // PRを作ることやコメントは止めない
        assert!(
            !deny
                .iter()
                .any(|d| d.as_str().unwrap().contains("pr create"))
        );
    }

    #[test]
    fn denies_reading_secret_files_by_absolute_path() {
        let settings = runtime_settings(&[PathBuf::from("/work/app/local.properties")]).unwrap();
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        assert!(
            deny.iter()
                .any(|d| d == "Read(//work/app/local.properties)")
        );
    }

    #[test]
    fn rejects_relative_secret_paths() {
        assert!(runtime_settings(&[PathBuf::from("local.properties")]).is_err());
    }
}
