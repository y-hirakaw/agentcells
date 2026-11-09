//! tickとinspectorが動かすプログラム（`claude`、`betterleaks`、`agentcells` 自身）の場所。
//! 本番ではPATHから探し、テストでは偽物を渡す。

use std::path::PathBuf;

use crate::error::{Error, Result};

pub enum Programs {
    /// 動いている `agentcells` と、PATHにある `claude` と `betterleaks`。使うたびに探す
    /// （tickの途中でプログラムを入れ替えても、次の1件から新しいものを使うため）。
    /// 見つからなくても、使うときまでエラーにしない（仕事がなければ要らないため）
    FromEnv,
    Fixed {
        agentcells: PathBuf,
        claude: PathBuf,
        scanner: PathBuf,
    },
}

impl Programs {
    pub fn agentcells(&self) -> Result<PathBuf> {
        match self {
            Self::FromEnv => std::env::current_exe()
                .map_err(|e| Error::new(format!("agentcellsの場所がわかりません: {e}"))),
            Self::Fixed { agentcells, .. } => Ok(agentcells.clone()),
        }
    }

    pub fn claude(&self) -> Result<PathBuf> {
        match self {
            Self::FromEnv => find_program("claude").ok_or_else(|| {
                Error::new("claudeが見つかりません")
                    .with_hint("`agentcells doctor` で確かめてください")
            }),
            Self::Fixed { claude, .. } => Ok(claude.clone()),
        }
    }

    /// 秘密を探す `betterleaks`
    pub fn scanner(&self) -> Result<PathBuf> {
        match self {
            Self::FromEnv => find_program("betterleaks").ok_or_else(|| {
                Error::new("betterleaksが見つかりません")
                    .with_hint("`agentcells doctor` で確かめてください")
            }),
            Self::Fixed { scanner, .. } => Ok(scanner.clone()),
        }
    }
}

/// PATHから、プログラムの絶対パスを探す。AIに別のものを動かされないよう、sandboxの外で決めておく。
fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
}
