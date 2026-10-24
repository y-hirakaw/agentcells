use std::process::Command;

/// 外部コマンドを実行した結果。
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// 外部コマンドを実行する。テストでは差し替える。
pub trait Probe {
    /// コマンドが見つからないときは `None` を返す。
    fn run(&self, program: &str, args: &[&str]) -> Option<Output>;
}

pub struct SystemProbe;

impl Probe for SystemProbe {
    fn run(&self, program: &str, args: &[&str]) -> Option<Output> {
        let out = Command::new(program).args(args).output().ok()?;
        Some(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}
