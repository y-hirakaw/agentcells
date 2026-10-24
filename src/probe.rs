use std::io::Write;
use std::process::{Command, Stdio};

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
    fn run(&self, program: &str, args: &[&str]) -> Option<Output> {
        self.run_with_env(program, args, &[])
    }

    /// 環境変数を足して実行する。トークンは引数ではなく環境変数で渡す。
    fn run_with_env(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Option<Output> {
        self.run_with_input(program, args, env, None)
    }

    /// 環境変数と標準入力を渡して実行する。引数や環境変数で渡せない秘密（curlのヘッダーなど）は標準入力で渡す。
    fn run_with_input(
        &self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        input: Option<&str>,
    ) -> Option<Output>;
}

pub struct SystemProbe;

impl Probe for SystemProbe {
    fn run_with_input(
        &self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        input: Option<&str>,
    ) -> Option<Output> {
        let mut child = Command::new(program)
            .args(args)
            .envs(env.iter().copied())
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            // 読まずに終わるコマンドもあるので、書けなくてもエラーにしない
            let _ = stdin.write_all(input.as_bytes());
        }
        let out = child.wait_with_output().ok()?;
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

    /// コマンドと引数の組に、決まった結果を返す。呼ばれたコマンドは `calls` に、
    /// 渡された環境変数は `envs` に、標準入力は `inputs` に残る。
    /// 登録していないコマンドは、`fallback` があればそれを、なければ見つからない扱いにする。
    #[derive(Default)]
    pub struct FakeProbe {
        pub outputs: HashMap<String, Output>,
        pub fallback: Option<Output>,
        pub calls: RefCell<Vec<String>>,
        pub envs: RefCell<Vec<Vec<(String, String)>>>,
        pub inputs: RefCell<Vec<Option<String>>>,
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

        pub fn with_output(mut self, command: String, output: Output) -> Self {
            self.outputs.insert(command, output);
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
        fn run_with_input(
            &self,
            program: &str,
            args: &[&str],
            env: &[(&str, &str)],
            input: Option<&str>,
        ) -> Option<Output> {
            let key = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.borrow_mut().push(key.clone());
            self.envs.borrow_mut().push(
                env.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            );
            self.inputs.borrow_mut().push(input.map(str::to_string));
            self.outputs
                .get(&key)
                .cloned()
                .or_else(|| self.fallback.clone())
        }
    }
}
