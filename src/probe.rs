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

#[cfg(test)]
pub(crate) mod fake {
    use super::{Output, Probe};
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// コマンドと引数の組に、決まった結果を返す。呼ばれたコマンドは `calls` に残る。
    /// 登録していないコマンドは、`fallback` があればそれを、なければ見つからない扱いにする。
    #[derive(Default)]
    pub struct FakeProbe {
        pub outputs: HashMap<String, Output>,
        pub fallback: Option<Output>,
        pub calls: RefCell<Vec<String>>,
    }

    impl FakeProbe {
        pub fn with(mut self, command: &str, success: bool, stdout: &str) -> Self {
            self.outputs.insert(
                command.to_string(),
                Output {
                    success,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                },
            );
            self
        }

        pub fn with_fallback(mut self, success: bool) -> Self {
            self.fallback = Some(Output {
                success,
                ..Output::default()
            });
            self
        }
    }

    impl Probe for FakeProbe {
        fn run(&self, program: &str, args: &[&str]) -> Option<Output> {
            let key = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.borrow_mut().push(key.clone());
            self.outputs
                .get(&key)
                .cloned()
                .or_else(|| self.fallback.clone())
        }
    }
}
