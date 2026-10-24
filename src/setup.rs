use std::fs;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::files::set_owner_only;
use crate::home::Home;

/// setupでしたこと。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub created: Vec<PathBuf>,
    pub existing: Vec<PathBuf>,
}

/// 作業データの置き場所を作る。何度実行しても同じ結果になり、途中で失敗してももう一度実行すれば続きから進む。
pub fn run(home: &Home) -> Result<Report> {
    let mut report = Report::default();
    let dirs = [
        home.root().to_path_buf(),
        home.bin_dir(),
        home.repos_dir(),
        home.worktrees_dir(),
        home.files_dir(),
        home.logs_dir(),
    ];
    for dir in dirs {
        if dir.is_dir() {
            report.existing.push(dir);
            continue;
        }
        if dir.exists() {
            return Err(
                Error::new(format!("{} はディレクトリではありません", dir.display())).with_hint(
                    "中身を確かめて、別の場所に移すか消してから、もう一度実行してください",
                ),
            );
        }
        fs::create_dir_all(&dir).map_err(|e| {
            Error::new(format!("{} を作れません: {e}", dir.display()))
                .with_hint("親ディレクトリの権限を確かめてください")
        })?;
        report.created.push(dir);
    }
    // 秘密のファイルの原本を置くので、本人だけが読めるようにする。
    set_owner_only(&home.files_dir(), 0o700)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_layout_then_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join(".agentcells"));

        let first = run(&home).unwrap();
        assert_eq!(first.created.len(), 6);
        assert!(first.existing.is_empty());
        assert!(home.worktrees_dir().is_dir());

        let second = run(&home).unwrap();
        assert!(second.created.is_empty());
        assert_eq!(second.existing.len(), 6);
    }

    #[test]
    fn resumes_when_partially_created() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join(".agentcells"));
        fs::create_dir_all(home.bin_dir()).unwrap();

        let report = run(&home).unwrap();
        assert_eq!(
            report.existing,
            vec![home.root().to_path_buf(), home.bin_dir()]
        );
        assert_eq!(report.created.len(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn files_dir_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join(".agentcells"));
        run(&home).unwrap();
        let mode = fs::metadata(home.files_dir()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn fails_with_hint_when_a_file_blocks_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join(".agentcells"));
        fs::create_dir_all(home.root()).unwrap();
        fs::write(home.logs_dir(), "not a dir").unwrap();

        let err = run(&home).unwrap_err();
        assert!(err.message.contains("logs"));
        assert!(err.hint.is_some());
    }
}
