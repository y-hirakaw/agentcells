//! gitignoreされていてビルドに要るファイル（keystoreやapplication.ymlなど）をworktreeに置く。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};
use crate::home::{Home, RepoName};

/// Finderが勝手に作るファイル。原本の置き場所にあっても配らない。
const SKIPPED_NAMES: &[&str] = &[".DS_Store"];

/// `files/<owner>/<repo>/` の下にあるファイルを、同じパスでworktreeにコピーする。
///
/// 1つでもgitignoreされていないパスがあれば、何も置かずにエラーにする（秘密のファイルをコミットさせない）。
/// 置いたファイルの、worktreeからの相対パスを返す。
pub fn apply(home: &Home, repo: &RepoName, worktree: &Path) -> Result<Vec<PathBuf>> {
    let source = home.files_for(repo);
    if !source.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect(&source, Path::new(""), &mut files)?;
    files.sort();
    if files.is_empty() {
        return Ok(files);
    }

    let ignored = ignored_paths(worktree, &files)?;
    let not_ignored: Vec<_> = files.iter().filter(|f| !ignored.contains(f)).collect();
    if !not_ignored.is_empty() {
        let list = not_ignored
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(Error::new(format!(
            "gitignoreされていないパスがあるので、ファイルを置きませんでした:\n{list}"
        ))
        .with_hint(format!(
            "{repo} の .gitignore にこれらのパスを足すか、{} から外してください",
            source.display()
        )));
    }

    for rel in &files {
        copy_private(&source.join(rel), &worktree.join(rel))?;
    }
    Ok(files)
}

fn collect(dir: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir)
        .map_err(|e| Error::new(format!("{} を読めません: {e}", dir.display())))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| Error::new(format!("{} を読めません: {e}", dir.display())))?;
        let name = entry.file_name();
        if SKIPPED_NAMES.iter().any(|s| name == *s) {
            continue;
        }
        let path = entry.path();
        let rel = rel.join(&name);
        // シンボリックリンクはたどった先の中身を配る。
        let meta = fs::metadata(&path)
            .map_err(|e| Error::new(format!("{} を読めません: {e}", path.display())))?;
        if meta.is_dir() {
            collect(&path, &rel, out)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

/// `git check-ignore` で、worktreeでgitignoreされているパスを返す。追跡されているファイルは含まれない。
fn ignored_paths(worktree: &Path, files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let spawn_error = |e: std::io::Error| {
        Error::new(format!("gitを実行できません: {e}"))
            .with_hint("`agentcells doctor` でgitが入っているか確かめてください")
    };
    let mut child = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["check-ignore", "--stdin", "-z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    // パスはNUL区切りで標準入力から渡す。出力を読む前に書き込みで詰まらないよう、別スレッドで書く。
    let mut input = Vec::new();
    for f in files {
        input.extend_from_slice(f.to_string_lossy().as_bytes());
        input.push(0);
    }
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().map_err(spawn_error)?;
    // gitが先に終了したときは書き込みが失敗するが、その場合は終了コードで判断する。
    let _ = writer.join();
    // 0: 1つ以上ignoreされている、1: どれもignoreされていない、それ以外: エラー
    match out.status.code() {
        Some(0) | Some(1) => Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()))
            .collect()),
        _ => Err(Error::new(format!(
            "{} でgitignoreを確かめられません: {}",
            worktree.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
        .with_hint("置き先がgitのリポジトリ（worktree）か確かめてください")),
    }
}

/// 親ディレクトリを作ってコピーし、本人だけが読み書きできる権限にする。
fn copy_private(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| Error::new(format!("{} を作れません: {e}", parent.display())))?;
    }
    fs::copy(from, to).map_err(|e| {
        Error::new(format!(
            "{} を {} にコピーできません: {e}",
            from.display(),
            to.display()
        ))
    })?;
    set_owner_only(to, 0o600)
}

#[cfg(unix)]
pub(crate) fn set_owner_only(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| Error::new(format!("{} の権限を変えられません: {e}", path.display())))
}

#[cfg(not(unix))]
pub(crate) fn set_owner_only(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        home: Home,
        repo: RepoName,
        worktree: PathBuf,
    }

    impl Fixture {
        fn new(gitignore: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let home = Home::new(dir.path().join("home"));
            let worktree = dir.path().join("worktree");
            fs::create_dir_all(&worktree).unwrap();
            let status = Command::new("git")
                .arg("-C")
                .arg(&worktree)
                .args(["init", "-q"])
                .status()
                .unwrap();
            assert!(status.success());
            fs::write(worktree.join(".gitignore"), gitignore).unwrap();
            Self {
                _dir: dir,
                home,
                repo: RepoName::parse("octo/app").unwrap(),
                worktree,
            }
        }

        fn source(&self, rel: &str, content: &str) {
            let path = self.home.files_for(&self.repo).join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn apply(&self) -> Result<Vec<PathBuf>> {
            apply(&self.home, &self.repo, &self.worktree)
        }
    }

    #[test]
    fn places_ignored_files_at_the_same_paths() {
        let fx = Fixture::new("*.jks\n/src/main/resources/application.yml\n");
        fx.source("app/release.jks", "keystore");
        fx.source("src/main/resources/application.yml", "db: secret");

        let placed = fx.apply().unwrap();

        assert_eq!(
            placed,
            vec![
                PathBuf::from("app/release.jks"),
                PathBuf::from("src/main/resources/application.yml"),
            ]
        );
        let yml = fx.worktree.join("src/main/resources/application.yml");
        assert_eq!(fs::read_to_string(yml).unwrap(), "db: secret");
    }

    #[cfg(unix)]
    #[test]
    fn placed_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new("local.properties\n");
        fx.source("local.properties", "sdk.dir=/x");
        fx.apply().unwrap();
        let mode = fs::metadata(fx.worktree.join("local.properties"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn refuses_to_place_anything_if_one_path_is_not_ignored() {
        let fx = Fixture::new("*.jks\n");
        fx.source("app/release.jks", "keystore");
        fx.source("config/secret.yml", "token: x");

        let err = fx.apply().unwrap_err();

        assert!(err.message.contains("config/secret.yml"));
        assert!(!err.message.contains("release.jks"));
        assert!(err.hint.is_some());
        assert!(!fx.worktree.join("app/release.jks").exists());
    }

    #[test]
    fn refuses_tracked_files_even_if_pattern_matches() {
        let fx = Fixture::new("*.yml\n");
        fs::write(fx.worktree.join("tracked.yml"), "tracked").unwrap();
        let add = Command::new("git")
            .arg("-C")
            .arg(&fx.worktree)
            .args(["add", "-f", "tracked.yml"])
            .status()
            .unwrap();
        assert!(add.success());
        fx.source("tracked.yml", "secret");

        assert!(fx.apply().is_err());
        assert_eq!(
            fs::read_to_string(fx.worktree.join("tracked.yml")).unwrap(),
            "tracked"
        );
    }

    #[test]
    fn overwrites_existing_file_with_the_source() {
        let fx = Fixture::new("local.properties\n");
        fx.source("local.properties", "new");
        fs::write(fx.worktree.join("local.properties"), "old").unwrap();
        fx.apply().unwrap();
        assert_eq!(
            fs::read_to_string(fx.worktree.join("local.properties")).unwrap(),
            "new"
        );
    }

    #[test]
    fn skips_ds_store() {
        let fx = Fixture::new("local.properties\n");
        fx.source("local.properties", "x");
        fx.source(".DS_Store", "finder");
        assert_eq!(fx.apply().unwrap(), vec![PathBuf::from("local.properties")]);
        assert!(!fx.worktree.join(".DS_Store").exists());
    }

    #[test]
    fn nothing_to_do_without_source_dir() {
        let fx = Fixture::new("");
        assert!(fx.apply().unwrap().is_empty());
    }

    #[test]
    fn fails_with_hint_outside_git_repository() {
        let fx = Fixture::new("x\n");
        fx.source("x", "x");
        fs::remove_dir_all(fx.worktree.join(".git")).unwrap();
        let err = fx.apply().unwrap_err();
        assert!(err.hint.is_some());
    }
}
