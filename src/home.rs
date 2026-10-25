use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// agentcellsの作業データを置く場所（既定は `~/.agentcells`）。
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 環境変数から場所を決める。`AGENTCELLS_HOME` があればそれを、なければ `$HOME/.agentcells` を使う。
    pub fn from_env() -> Result<Self> {
        Self::resolve(
            std::env::var_os("AGENTCELLS_HOME"),
            std::env::var_os("HOME"),
        )
    }

    pub fn resolve(agentcells_home: Option<OsString>, home: Option<OsString>) -> Result<Self> {
        if let Some(dir) = agentcells_home.filter(|v| !v.is_empty()) {
            return Ok(Self::new(dir));
        }
        match home.filter(|v| !v.is_empty()) {
            Some(home) => Ok(Self::new(PathBuf::from(home).join(".agentcells"))),
            None => Err(Error::new("ホームディレクトリがわかりません")
                .with_hint("環境変数 HOME か AGENTCELLS_HOME を設定してください")),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.root.join("repos")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.root.join("worktrees")
    }

    /// productごとの、ビルドツールのキャッシュ（`caches/<product>/<ツール>`）
    pub fn caches_dir(&self) -> PathBuf {
        self.root.join("caches")
    }

    pub fn files_dir(&self) -> PathBuf {
        self.root.join("files")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// worktreeに置くファイルの原本を置く場所。
    pub fn files_for(&self, repo: &RepoName) -> PathBuf {
        self.files_dir().join(&repo.owner).join(&repo.name)
    }
}

/// `owner/repo` 形式のリポジトリ名。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct RepoName {
    pub owner: String,
    pub name: String,
}

impl RepoName {
    pub fn parse(s: &str) -> Result<Self> {
        let invalid = || {
            Error::new(format!("リポジトリ名の形式が正しくありません: {s}"))
                .with_hint("owner/repo の形で指定してください（例: y-hirakaw/agentcells）")
        };
        let (owner, name) = s.split_once('/').ok_or_else(invalid)?;
        if !is_valid_part(owner) || !is_valid_part(name) {
            return Err(invalid());
        }
        Ok(Self {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

impl TryFrom<String> for RepoName {
    type Error = Error;

    fn try_from(s: String) -> Result<Self> {
        Self::parse(&s)
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

// GitHubのowner/repoに使える文字だけを許す。パスとして使うので `..` などは通さない。
fn is_valid_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agentcells_home_takes_precedence() {
        let home = Home::resolve(Some("/tmp/ac".into()), Some("/Users/me".into())).unwrap();
        assert_eq!(home.root(), Path::new("/tmp/ac"));
    }

    #[test]
    fn defaults_to_dot_agentcells_under_home() {
        let home = Home::resolve(None, Some("/Users/me".into())).unwrap();
        assert_eq!(home.root(), Path::new("/Users/me/.agentcells"));
    }

    #[test]
    fn empty_agentcells_home_is_ignored() {
        let home = Home::resolve(Some("".into()), Some("/Users/me".into())).unwrap();
        assert_eq!(home.root(), Path::new("/Users/me/.agentcells"));
    }

    #[test]
    fn fails_with_hint_without_any_home() {
        let err = Home::resolve(None, None).unwrap_err();
        assert!(err.hint.is_some());
    }

    #[test]
    fn layout_is_under_root() {
        let home = Home::new("/r");
        assert_eq!(home.bin_dir(), Path::new("/r/bin"));
        assert_eq!(home.repos_dir(), Path::new("/r/repos"));
        assert_eq!(home.worktrees_dir(), Path::new("/r/worktrees"));
        assert_eq!(home.logs_dir(), Path::new("/r/logs"));
        let repo = RepoName::parse("octo/app").unwrap();
        assert_eq!(home.files_for(&repo), Path::new("/r/files/octo/app"));
    }

    #[test]
    fn parses_owner_and_repo() {
        let repo = RepoName::parse("y-hirakaw/agentcells").unwrap();
        assert_eq!(repo.owner, "y-hirakaw");
        assert_eq!(repo.name, "agentcells");
        assert_eq!(repo.to_string(), "y-hirakaw/agentcells");
    }

    #[test]
    fn rejects_malformed_repo_names() {
        for s in [
            "",
            "agentcells",
            "/repo",
            "owner/",
            "a/b/c",
            "../x",
            "x/..",
            "o w/r",
            "o/r\n",
        ] {
            assert!(RepoName::parse(s).is_err(), "{s:?} should be rejected");
        }
    }
}
