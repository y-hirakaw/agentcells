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

/// AIに許すツール。読むのはその仕事のworktreeの中だけにし、書き換えは次の2つの道だけにする。
/// - ファイルの作成と更新: Edit・Writeを、その仕事のworktreeの中だけに許す
/// - それ以外（削除、移動、コミット、gitを読む、ビルドやテスト）: `agentcells cell` を通す。CLIが範囲を確かめる
///
/// Bashは `agentcells cell` だけを許す。`ls` や `git diff` のようなコマンドでも、オプションでファイルに
/// 書けるものがある（`git diff --output` など）ので許さない。Claude Codeは、許したコマンドに
/// `>`、`;`、`&&`、`$(...)` で別の操作を付け足すことを止める（2.1.310で確かめた）。
pub const ALLOW_BASH: &str = "Bash(agentcells cell:*)";

/// 使う人のホームの下で、秘密がよく置かれる場所。AIが動かすコマンド（sandboxの中）に読ませない。
const SECRET_PLACES: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".netrc",
    ".git-credentials",
    ".docker",
    ".kube",
    ".npmrc",
    ".pypirc",
    ".config/gcloud",
    "Library/Keychains",
];

/// 1回の実行の場所。
pub struct RunPaths<'a> {
    /// 作業するworktree
    pub worktree: &'a Path,
    /// gitの共通のディレクトリ（linked worktreeなら、元のリポジトリの `.git`）。コミットで書き込む
    pub git_dir: &'a Path,
    /// 今の仕事のファイル（`AGENTCELLS_CONTEXT`）。`agentcells cell` が読む
    pub context: &'a Path,
    /// worktreeに置いた秘密のファイル（絶対パス）
    pub placed: &'a [PathBuf],
}

/// 1回の実行で渡す設定を作る。2つの守りを重ねる。
///
/// Claude Codeのツールの権限:
/// - 読む（Read）のは、worktreeの中だけ
/// - ファイルの作成と更新（Edit、Write）は、worktreeの中だけ。`.git` と置いた秘密のファイルは除く
/// - Bashは `agentcells cell` だけ
///
/// sandbox（Bashで動くもの。`agentcells cell run` のビルドやテストも含む）:
/// - 書けるのは、worktreeと、gitの共通のディレクトリだけ
/// - `~/.agentcells`（App の秘密鍵、`files/`、`config.toml`、ほかの仕事のworktree）、ghの設定、
///   秘密がよく置かれる場所は読ませない。ただし今の仕事のworktree、gitの共通のディレクトリ、今の仕事のファイルは読める
/// - sandboxの外でコマンドを動かすことは許さない
///
/// 信頼していないディレクトリではprojectの設定のallowが無視されるので、cellに要るallowもここで渡す。
pub fn runtime_settings(home: &Home, config: &Config, run: &RunPaths) -> Result<serde_json::Value> {
    let worktree = run.worktree;
    let mut allow = vec![ALLOW_BASH.to_string()];
    for tool in ["Read", "Edit", "Write"] {
        rule(&mut allow, tool, worktree, "/**")?;
    }

    let mut deny: Vec<String> = DENY_GH.iter().map(|s| s.to_string()).collect();
    // .git（linked worktreeではリポジトリを指すファイル）を書き換えさせない
    for tool in ["Edit", "Write"] {
        rule(&mut deny, tool, &worktree.join(".git"), "")?;
        rule(&mut deny, tool, &worktree.join(".git"), "/**")?;
    }
    for path in run.placed {
        for tool in ["Read", "Edit", "Write"] {
            rule(&mut deny, tool, path, "")?;
        }
    }

    let mut deny_read = forms(home.root())?;
    if let Some(app) = &config.github_app {
        deny_read.extend(forms(&app.private_key)?);
    }
    if let Some(dir) = gh_config_dir(|key| std::env::var_os(key)) {
        deny_read.extend(forms(&dir)?);
    }
    if let Some(user_home) = std::env::var_os("HOME").map(PathBuf::from)
        && user_home.is_absolute()
    {
        for place in SECRET_PLACES {
            deny_read.extend(forms(&user_home.join(place))?);
        }
    }
    for path in run.placed {
        deny_read.extend(forms(path)?);
    }
    let mut allow_read = forms(worktree)?;
    allow_read.extend(forms(run.git_dir)?);
    allow_read.extend(forms(run.context)?);

    Ok(json!({
        "permissions": { "allow": allow, "deny": deny },
        "sandbox": {
            "enabled": true,
            "allowUnsandboxedCommands": false,
            "filesystem": {
                "allowWrite": forms(run.git_dir)?,
                "denyRead": dedup(deny_read),
                "allowRead": dedup(allow_read),
            }
        }
    }))
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
    v
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
fn rule(rules: &mut Vec<String>, tool: &str, path: &Path, suffix: &str) -> Result<()> {
    for p in path_forms(path)? {
        let rule = format!("{tool}(/{}{suffix})", p.display());
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    }
    Ok(())
}

/// sandboxに書くパス。
fn forms(path: &Path) -> Result<Vec<String>> {
    Ok(path_forms(path)?
        .iter()
        .map(|p| p.display().to_string())
        .collect())
}

/// Claude Codeが比べるパスと形が違うと効かないので、書いたままのパスと、
/// シンボリックリンクや `..` を解いたパス（macOSの /var → /private/var など）の両方を返す。
fn path_forms(path: &Path) -> Result<Vec<PathBuf>> {
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
    Ok(paths)
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

    fn settings_of(config: &Config, placed: &[PathBuf]) -> serde_json::Value {
        let run = RunPaths {
            worktree: Path::new("/w/octo/app/12"),
            git_dir: Path::new("/home/me/.agentcells/repos/octo/app/.git"),
            context: Path::new("/home/me/.agentcells/runs/1/context.json"),
            placed,
        };
        runtime_settings(&home(), config, &run).unwrap()
    }

    fn strings(v: &serde_json::Value) -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap().to_string())
            .collect()
    }

    fn rules_of(config: &Config, placed: &[PathBuf], kind: &str) -> Vec<String> {
        strings(&settings_of(config, placed)["permissions"][kind])
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
                "Bash(agentcells cell:*)",
                "Read(//w/octo/app/12/**)",
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
            "Read(//w/octo/app/12/key.jks)",
            "Edit(//w/octo/app/12/key.jks)",
            "Write(//w/octo/app/12/key.jks)",
        ] {
            assert!(deny.contains(&rule.to_string()), "{rule} should be denied");
        }
    }

    #[test]
    fn sandbox_writes_only_to_git_dir_and_hides_agentcells_home() {
        let config = Config {
            github_app: Some(GithubApp {
                id: "Iv23li".to_string(),
                private_key: PathBuf::from("/keys/app.pem"),
            }),
            ..Config::default()
        };
        let settings = settings_of(&config, &[PathBuf::from("/w/octo/app/12/key.jks")]);
        let sandbox = &settings["sandbox"];
        assert_eq!(sandbox["enabled"], true);
        // sandboxの外でコマンドを動かさせない
        assert_eq!(sandbox["allowUnsandboxedCommands"], false);
        let fs = &sandbox["filesystem"];
        assert_eq!(
            strings(&fs["allowWrite"]),
            vec!["/home/me/.agentcells/repos/octo/app/.git"]
        );
        let deny_read = strings(&fs["denyRead"]);
        for path in [
            "/home/me/.agentcells",
            "/keys/app.pem",
            "/w/octo/app/12/key.jks",
        ] {
            assert!(
                deny_read.contains(&path.to_string()),
                "{path}: {deny_read:?}"
            );
        }
        let allow_read = strings(&fs["allowRead"]);
        for path in [
            "/w/octo/app/12",
            "/home/me/.agentcells/repos/octo/app/.git",
            "/home/me/.agentcells/runs/1/context.json",
        ] {
            assert!(
                allow_read.contains(&path.to_string()),
                "{path}: {allow_read:?}"
            );
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
    fn denies_placed_secret_files_by_absolute_path() {
        let deny = deny_of(
            &Config::default(),
            &[PathBuf::from("/work/app/local.properties")],
        );
        assert!(deny.contains(&"Read(//work/app/local.properties)".to_string()));
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
        let run = RunPaths {
            worktree: &link,
            git_dir: &link,
            context: &link,
            placed: &[],
        };
        let settings = runtime_settings(&home, &config, &run).unwrap();
        let deny_read = strings(&settings["sandbox"]["filesystem"]["denyRead"]);

        let resolved = real.canonicalize().unwrap();
        for path in [resolved.join("app.pem"), resolved.clone(), link.clone()] {
            let path = path.display().to_string();
            assert!(
                deny_read.contains(&path),
                "{path} should be denied: {deny_read:?}"
            );
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
        let placed = [PathBuf::from("local.properties")];
        let run = RunPaths {
            worktree: Path::new("/w"),
            git_dir: Path::new("/w/.git"),
            context: Path::new("/c.json"),
            placed: &placed,
        };
        assert!(runtime_settings(&home(), &Config::default(), &run).is_err());
    }
}
