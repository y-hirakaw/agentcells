//! 仕事ごとの作業の場所（`repos/` のbareのclone、仕事ごとのclone、実行の記録）。

use std::path::{Path, PathBuf};

use super::claude::CLAUDE_LOG;
use super::{git, path_str};
use crate::error::{Error, Result};
use crate::github::{Credentials, ISOLATED_GIT_ENV};
use crate::home::{Home, RepoName};
use crate::probe::Probe;

/// 1件の仕事の場所。`<種類>/<owner>/<repo>/<番号>` の形にする（番号だけだと、別のリポジトリの同じ番号とぶつかる）。
pub(super) struct Place {
    /// `repos/<owner>/<repo>`（作業するリポジトリのbareのclone）
    pub(super) cache: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) git_dir: PathBuf,
    pub(super) tmp: PathBuf,
    /// 今の仕事のファイル、sandboxの設定、AIの出力を置く（実行のあとも残す）
    pub(super) run: PathBuf,
    /// `<owner>/<repo>/<番号>`
    pub(super) relative: PathBuf,
}

impl Place {
    pub(super) fn new(home: &Home, repo: &RepoName, number: u64) -> Self {
        let relative = Path::new(&repo.owner)
            .join(&repo.name)
            .join(number.to_string());
        Place {
            cache: PathBuf::new(),
            worktree: home.worktrees_dir().join(&relative),
            git_dir: home.root().join("gitdirs").join(&relative),
            tmp: home.root().join("tmp").join(&relative),
            run: run_dir(home, repo, number),
            relative,
        }
    }

    /// 公開のコメントに書く、実行の記録の場所（手元の絶対パスは書かない）。
    pub(super) fn run_name(&self) -> String {
        format!("~/.agentcells/runs/{}", self.relative.display())
    }

    /// 作業するリポジトリのcloneを最新にし、仕事ごとの場所を空にして作り直す。
    pub(super) fn prepare(
        &mut self,
        probe: &dyn Probe,
        read: &Credentials,
        home: &Home,
        work_repo: &RepoName,
    ) -> Result<()> {
        self.cache = update_cache(probe, read, home, work_repo)?;
        self.clean();
        let _ = std::fs::remove_dir_all(&self.run);
        for dir in [&self.tmp, &self.run] {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::new(format!("{} を作れません: {e}", dir.display())))?;
            // 記録には、AIが読んだファイルの中身やコマンドの出力も入るので、ほかのユーザーに読ませない
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        for dir in [&self.worktree, &self.git_dir] {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::new(format!("{} を作れません: {e}", parent.display())))?;
            }
        }
        // 元のリポジトリのオブジェクトは共有して読むだけにし、.git の実体は仕事ごとに作業場所の外に置く
        let mut env = ISOLATED_GIT_ENV.to_vec();
        env.push(("GIT_ALLOW_PROTOCOL", "file"));
        let git_dir_arg = format!("--separate-git-dir={}", self.git_dir.display());
        git(
            probe,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "clone",
                "-q",
                "--shared",
                &git_dir_arg,
                &path_str(&self.cache)?,
                &path_str(&self.worktree)?,
            ],
            &env,
        )
    }

    /// 作業の場所を消す（実行の記録は残す）。
    pub(super) fn clean(&self) {
        for dir in [&self.worktree, &self.git_dir, &self.tmp] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// `repos/<owner>/<repo>` のbareのcloneを作るか、最新にする。CLIだけが書き、sandboxからは読むだけにする。
fn update_cache(
    probe: &dyn Probe,
    read: &Credentials,
    home: &Home,
    repo: &RepoName,
) -> Result<PathBuf> {
    let cache = home.repos_dir().join(&repo.owner).join(&repo.name);
    let url = crate::pr::remote_url(repo);
    let mut args: Vec<String> = read.git_args();
    if cache.join("HEAD").exists() {
        args.extend(
            [
                "-C",
                &path_str(&cache)?,
                "fetch",
                "-q",
                "--prune",
                &url,
                "+refs/heads/*:refs/heads/*",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    } else {
        if let Some(parent) = cache.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::new(format!("{} を作れません: {e}", parent.display())))?;
        }
        args.extend(
            ["clone", "-q", "--bare", &url, &path_str(&cache)?]
                .iter()
                .map(|s| s.to_string()),
        );
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git(probe, &args, &read.git_env())?;
    Ok(cache)
}

/// Issue/PRごとの、実行の記録の場所（`runs/<owner>/<repo>/<番号>`）。
fn run_dir(home: &Home, repo: &RepoName, number: u64) -> PathBuf {
    home.root()
        .join("runs")
        .join(&repo.owner)
        .join(&repo.name)
        .join(number.to_string())
}

/// Issue/PRの、最後の実行のclaudeの出力。同じIssue/PRの次の実行で上書きされる。
pub fn claude_log(home: &Home, repo: &RepoName, number: u64) -> PathBuf {
    run_dir(home, repo, number).join(CLAUDE_LOG)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn public_run_names_do_not_include_local_paths() {
        let home = Home::new(PathBuf::from("/Users/someone/.agentcells"));
        let place = Place::new(&home, &RepoName::parse("octo/app").unwrap(), 3);
        assert_eq!(place.run_name(), "~/.agentcells/runs/octo/app/3");
    }
}
