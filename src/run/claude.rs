//! `claude -p` を、決まった引数と環境変数で動かす（cellと、ファイルを読むだけのinspector）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::Failure;
use super::process::{Processes, RUN_MARK_ENV, Stuck, current_process, stuck_path};
use crate::actions;
use crate::error::{Error, Result};
use crate::home::Home;

/// claudeの結果から、失敗の種類を決まった語で返す。claudeの `subtype` は、英小文字と _ だけを使う。
pub(super) fn failure_kind(result: &serde_json::Value) -> String {
    if result["is_error"].as_bool() == Some(true) {
        let subtype: String = result["subtype"]
            .as_str()
            .unwrap_or("unknown")
            .chars()
            .filter(|c| c.is_ascii_lowercase() || *c == '_')
            .take(40)
            .collect();
        // 使用量の上限などは、APIの状態の番号（429など）で見分けられる
        match result["api_error_status"].as_u64() {
            Some(status) => format!("error:{subtype}:{status}"),
            None => format!("error:{subtype}"),
        }
    } else if result["structured_output"].is_null() {
        "no_output".to_string()
    } else {
        "bad_output".to_string()
    }
}

/// `claude -p` の1回の実行。
pub(super) struct ClaudeRun<'a> {
    pub(super) program: &'a Path,
    pub(super) cwd: &'a Path,
    pub(super) settings: &'a Path,
    pub(super) system: &'a str,
    pub(super) schema: &'a serde_json::Value,
    pub(super) prompt: &'a str,
    pub(super) context: &'a Path,
    /// AIが呼ぶ `agentcells` を、PATHの先頭で見つけさせる
    pub(super) path_prefix: Option<&'a Path>,
    pub(super) model: Option<&'a str>,
    /// 使えるツール（`--tools`）。`None` なら標準のまま（設定のallowとdenyで絞る）
    pub(super) tools: Option<&'a str>,
    pub(super) timeout: Duration,
    /// 標準出力と標準エラーを残す場所
    pub(super) log_dir: &'a Path,
    /// この仕事の場所（worktree、一時ディレクトリ、キャッシュ）。ここで動いているプロセスは、終わったら止める
    pub(super) job_dirs: &'a [PathBuf],
    /// 動いている間、見つけたプロセスを書いておく目印（`stuck.json`）と、どの仕事か（[`Stuck`]）
    pub(super) record: Option<(PathBuf, String)>,
}

/// claudeの終わり方。プロセスを止めきれなかったときは、ここには来ない（`Failure::Unstopped`）。
/// 書き込みの道に入るのは、止めたことを確かめたときだけにするため。
pub(super) enum ClaudeEnd {
    Exited(String),
    TimedOut,
}

/// claudeに渡す環境変数。使う人の環境変数（`SSH_AUTH_SOCK`、`AWS_*`、ほかのツールのトークンなど）は、
/// AIとビルドから使えてしまうので、要るものだけを渡す。claudeの認証は、macOSならキーチェーンを使う。
/// `ANTHROPIC_API_KEY` で認証しているときは渡すしかなく、AIの動かすコマンドからも見える（docs/design.md）。
const PASSED_ENV: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TERM",
    "TMPDIR",
    "CLAUDE_CONFIG_DIR",
    "ANTHROPIC_API_KEY",
    // 社内のproxyやCAの下で、claudeがAPIにつながるように（URLに資格情報を書いていれば、コマンドからも見える）
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
];

impl ClaudeRun<'_> {
    pub(super) fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            // 動いている間の様子を見られるように、出来事を1行ずつ書かせる（最後の行が結果）
            "--output-format",
            "stream-json",
            "--verbose",
            "--settings",
            &self.settings.display().to_string(),
            // 使う人の設定も、作業するリポジトリの設定（.claude/settings.json）も読まない。
            // リポジトリの設定のhooksやenvは、AIがブランチに書けるうえ、hooksはsandboxの外で動く
            "--setting-sources",
            "",
            // 作業するリポジトリの .mcp.json のMCPサーバーを動かさない
            "--strict-mcp-config",
            "--append-system-prompt",
            self.system,
            "--json-schema",
            &self.schema.to_string(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(model) = self.model {
            args.extend(["--model".to_string(), model.to_string()]);
        }
        if let Some(tools) = self.tools {
            args.extend(["--tools".to_string(), tools.to_string()]);
        }
        args
    }

    pub(super) fn env(&self) -> Vec<(String, std::ffi::OsString)> {
        let mut env: Vec<(String, std::ffi::OsString)> = std::env::vars_os()
            .filter_map(|(k, v)| {
                let k = k.to_str()?.to_string();
                (PASSED_ENV.contains(&k.as_str()) || k.starts_with("LC_")).then_some((k, v))
            })
            .collect();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths: Vec<PathBuf> = self
            .path_prefix
            .map(Path::to_path_buf)
            .into_iter()
            .collect();
        paths.extend(std::env::split_paths(&path));
        if let Ok(path) = std::env::join_paths(paths) {
            env.push(("PATH".to_string(), path));
        }
        env.push((actions::CONTEXT_ENV.to_string(), self.context.into()));
        env
    }

    /// この実行の目印。claudeの子孫は環境変数を受け継ぐので、親子のつながりが切れたもの
    /// （親が先に終わって付け替えられたもの）も、これで見つけて止める。
    pub(super) fn mark(&self) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("{:x}{:x}", std::process::id(), nanos)
    }

    pub(super) fn run(&self) -> std::result::Result<ClaudeEnd, Failure> {
        let log = |name: &str| -> Result<std::fs::File> {
            let path = self.log_dir.join(name);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            // AIが読んだファイルの中身やコマンドの出力も入るので、ほかのユーザーに読ませない
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(&path)
                .map_err(|e| Error::new(format!("{} を作れません: {e}", path.display())))
        };
        let stdout_path = self.log_dir.join(CLAUDE_LOG);
        let mark = self.mark();
        let mut command = Command::new(self.program);
        command
            .args(self.args())
            .current_dir(self.cwd)
            .env_clear()
            .envs(self.env())
            .env(RUN_MARK_ENV, &mark)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log(CLAUDE_LOG)?))
            .stderr(Stdio::from(log("claude.err")?));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|e| {
            Error::new(format!("{} を動かせません: {e}", self.program.display()))
                .with_hint("`agentcells doctor` で、claudeを確かめてください")
        })?;
        let pid = child.id();
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().expect("stdin is piped");
            // claudeは標準入力を読み終えてから動くので、書き終えてから待つ（大きくても止まらない）
            let _ = stdin.write_all(self.prompt.as_bytes());
        }
        // Claude Codeは、Bashのコマンドを別のプロセスグループで動かす。そのため、動いている間に
        // claudeの子孫を集めておき、終わったらすべて止める（親が終わって付け替えられた子も、先に覚えておけば止められる）
        let mut seen = Processes {
            mark: format!("{RUN_MARK_ENV}={mark}"),
            dirs: self.job_dirs.to_vec(),
            ..Processes::default()
        };
        let deadline = Instant::now() + self.timeout;
        let tick = current_process();
        let mut recorded = 0;
        let timed_out = loop {
            seen.collect(pid);
            // 見つけたものが増えたら書き直す（tickが殺されても、次のtickが残ったものに気づけるように）
            if let Some((path, job)) = &self.record
                && seen.known.len() != recorded
            {
                recorded = seen.known.len();
                seen.record(path, job, tick.clone());
            }
            match child.try_wait() {
                Ok(Some(_)) => break false,
                Ok(None) if Instant::now() >= deadline => break true,
                // Claude CodeのBashのシェルはすぐ終わるので、短い間隔で見る
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(Error::new(format!("claudeを待てません: {e}")).into()),
            }
        };
        if timed_out {
            let _ = Command::new("kill")
                .args(["-TERM", "--", &format!("-{pid}")])
                .stderr(Stdio::null())
                .status();
            std::thread::sleep(Duration::from_secs(3));
        }
        let stopped = seen.stop_all(pid);
        let _ = child.wait();
        if let Err(leftovers) = stopped {
            return Err(Failure::Unstopped(leftovers));
        }
        // すべて止めたので、動いている間の目印は要らない
        if let Some((path, _)) = &self.record {
            let _ = std::fs::remove_file(path);
        }
        if timed_out {
            return Ok(ClaudeEnd::TimedOut);
        }
        let stdout = std::fs::read_to_string(&stdout_path)
            .map_err(|e| Error::new(format!("claudeの出力を読めません: {e}")))?;
        Ok(ClaudeEnd::Exited(result_line(&stdout).to_string()))
    }
}

/// cellの外で、ファイルを読むだけのclaudeを動かす（inspector）。使えるツールは Read、Grep、Glob だけで、
/// 読めるのは `cwd` の中だけ（`-p` では、作業場所の外を読むには許しが要り、許していないので止まる）。
pub struct Reader<'a> {
    pub home: &'a Home,
    pub claude: &'a Path,
    pub cwd: &'a Path,
    /// `--settings` に渡すファイル
    pub settings: &'a Path,
    pub system: &'a str,
    pub schema: &'a serde_json::Value,
    pub prompt: &'a str,
    pub model: Option<&'a str>,
    pub timeout: Duration,
    /// 出力（`claude.jsonl`）を残す場所
    pub log_dir: &'a Path,
}

impl Reader<'_> {
    /// 動かして、結果の行（`"type":"result"`）を返す。時間切れなら `None`。止めきれなかったプロセスがあれば、
    /// `stuck.json` を残してエラーを返す（cellと同じく、それが残っている間は次のtickが動かない）。
    pub fn run(&self) -> Result<Option<serde_json::Value>> {
        let context = self.log_dir.join("context.json");
        let job_dirs = [self.cwd.to_path_buf()];
        let run = ClaudeRun {
            program: self.claude,
            cwd: self.cwd,
            settings: self.settings,
            system: self.system,
            schema: self.schema,
            prompt: self.prompt,
            context: &context,
            path_prefix: None,
            model: self.model,
            tools: Some("Read,Grep,Glob"),
            timeout: self.timeout,
            log_dir: self.log_dir,
            job_dirs: &job_dirs,
            record: Some((stuck_path(self.home), "inspector".to_string())),
        };
        match run.run() {
            Ok(ClaudeEnd::Exited(line)) => serde_json::from_str(&line)
                .map(Some)
                .map_err(|e| Error::new(format!("AIの出力を読めません: {e}"))),
            Ok(ClaudeEnd::TimedOut) => Ok(None),
            Err(Failure::Error(e)) => Err(e),
            Err(Failure::Unstopped(leftovers)) => {
                // tickを書かずに残し、tickが動いていても残ったものとして扱わせる（cellの失敗と同じ）
                let stuck = Stuck {
                    job: "inspector".to_string(),
                    processes: leftovers,
                    tick: None,
                };
                if let Ok(text) = serde_json::to_string_pretty(&stuck) {
                    let _ = std::fs::write(stuck_path(self.home), text);
                }
                Err(Error::new(
                    "claudeが動かしたプロセスを止めきれませんでした。残っている間は、次のtickは動きません",
                )
                .with_hint("`agentcells doctor` で確かめてください"))
            }
        }
    }
}

/// claudeの出力（`stream-json`）を書くファイル。
pub(super) const CLAUDE_LOG: &str = "claude.jsonl";

/// `stream-json` の出力から、結果の行（`"type":"result"`）を読む。なければ空。
fn result_line(stdout: &str) -> &str {
    stdout
        .lines()
        .rev()
        .find(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|v| v["type"].as_str() == Some("result"))
        })
        .unwrap_or("")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::run::process::is_alive;

    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-claude");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn claude_run<'a>(
        dir: &'a Path,
        program: &'a Path,
        schema: &'a serde_json::Value,
        timeout: Duration,
    ) -> ClaudeRun<'a> {
        ClaudeRun {
            program,
            cwd: dir,
            settings: Path::new("/s.json"),
            system: "system",
            schema,
            prompt: "user prompt",
            context: Path::new("/c.json"),
            path_prefix: Some(Path::new("/opt/agentcells/bin")),
            model: Some("haiku"),
            tools: Some(crate::settings::CELL_TOOLS),
            timeout,
            log_dir: dir,
            job_dirs: &[],
            record: None,
        }
    }

    fn alive_from_file(dir: &Path, name: &str) -> bool {
        let pid = std::fs::read_to_string(dir.join(name)).unwrap();
        is_alive(pid.trim().parse().unwrap())
    }

    #[test]
    fn passes_the_prompt_on_stdin_with_only_the_needed_env() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            r#"cat > prompt.txt; printf '%s\n' "$@" > args.txt; echo "$PATH" > path.txt; echo "$AGENTCELLS_CONTEXT" > context.txt; env > env.txt; echo '{"type":"system"}'; echo '{"type":"result","structured_output":{"next":"done","comment":"ok"}}'"#,
        );
        let schema = serde_json::json!({"type": "object"});
        let end = claude_run(dir.path(), &program, &schema, Duration::from_secs(30))
            .run()
            .unwrap_or_else(|_| panic!("failed"));
        let ClaudeEnd::Exited(stdout) = end else {
            panic!("timed out");
        };
        assert!(stdout.contains("\"done\""));
        let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(read("prompt.txt"), "user prompt");
        let args: Vec<String> = read("args.txt").lines().map(str::to_string).collect();
        let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(pair("--setting-sources", ""), "{args:?}");
        assert!(pair("--settings", "/s.json"), "{args:?}");
        assert!(pair("--model", "haiku"), "{args:?}");
        assert!(pair("--tools", crate::settings::CELL_TOOLS), "{args:?}");
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        assert!(read("path.txt").starts_with("/opt/agentcells/bin:"));
        assert_eq!(read("context.txt").trim(), "/c.json");
        // 使う人の環境変数は、要るもの以外を渡さない
        let names: Vec<String> = read("env.txt")
            .lines()
            .filter_map(|l| l.split('=').next().map(str::to_string))
            .collect();
        for name in &names {
            let allowed = PASSED_ENV.contains(&name.as_str())
                || name.starts_with("LC_")
                || [
                    "PATH",
                    "AGENTCELLS_CONTEXT",
                    RUN_MARK_ENV,
                    "PWD",
                    "SHLVL",
                    "_",
                    "OLDPWD",
                ]
                .contains(&name.as_str());
            assert!(allowed, "{name} should not be passed");
        }
    }

    #[test]
    fn stops_descendants_in_other_process_groups() {
        let dir = tempfile::tempdir().unwrap();
        // Claude CodeのBashのように、自分のプロセスグループを作った子を残して終わる
        let program = script(
            dir.path(),
            r#"cat > /dev/null; python3 -c 'import os,time; os.setsid(); open("child.pid","w").write(str(os.getpid())); time.sleep(60)' & sleep 2; echo '{}'"#,
        );
        let schema = serde_json::json!({});
        let end = claude_run(dir.path(), &program, &schema, Duration::from_secs(30))
            .run()
            .unwrap_or_else(|_| panic!("failed"));
        assert!(matches!(end, ClaudeEnd::Exited(_)));
        assert!(
            !alive_from_file(dir.path(), "child.pid"),
            "child is still alive"
        );
    }

    #[test]
    fn stops_everything_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            r#"cat > /dev/null; sleep 60 & echo $! > child.pid; sleep 60"#,
        );
        let schema = serde_json::json!({});
        let started = Instant::now();
        let end = claude_run(dir.path(), &program, &schema, Duration::from_secs(2))
            .run()
            .unwrap_or_else(|_| panic!("failed"));
        assert!(matches!(end, ClaudeEnd::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(
            !alive_from_file(dir.path(), "child.pid"),
            "child is still alive"
        );
    }

    /// 本物のclaudeで、AIがBashでバックグラウンドに残したプロセスも止まることを確かめる。
    /// claudeを動かすので、手で `cargo test -- --ignored real_claude` を実行したときだけ動く（haikuを使う）。
    #[test]
    #[ignore]
    fn real_claude_background_processes_are_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let marker = format!("agentcells-bg-{}", std::process::id());
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            serde_json::json!({
                "disableAllHooks": true,
                "permissions": { "allow": ["Bash(sh bg.sh)"] },
                "sandbox": { "enabled": true, "allowUnsandboxedCommands": false }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("bg.sh"),
            format!("nohup sh -c 'exec -a {marker} sleep 300' >/dev/null 2>&1 &\necho started\n"),
        )
        .unwrap();
        let job_dirs = [dir.path().to_path_buf()];
        let claude = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|d| d.join("claude"))
            .find(|p| p.is_file())
            .expect("claude is on PATH");
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "done": { "type": "boolean" } },
            "required": ["done"]
        });
        let run = ClaudeRun {
            program: &claude,
            cwd: dir.path(),
            settings: &settings,
            system: "You are a test helper.",
            schema: &schema,
            prompt: "Run exactly this Bash command once: sh bg.sh  Then return done=true.",
            context: Path::new("/nonexistent"),
            path_prefix: None,
            model: Some("haiku"),
            tools: None,
            timeout: Duration::from_secs(300),
            log_dir: dir.path(),
            job_dirs: &job_dirs,
            record: None,
        };
        let end = run.run().unwrap_or_else(|_| panic!("failed"));
        let ClaudeEnd::Exited(result) = end else {
            panic!("timed out");
        };
        // stream-json の最後の結果の行を読む
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["type"], "result", "{result}");
        std::thread::sleep(Duration::from_millis(500));
        let left = Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .unwrap();
        let left = String::from_utf8_lossy(&left.stdout).trim().to_string();
        let log = std::fs::read_to_string(dir.path().join(CLAUDE_LOG)).unwrap_or_default();
        assert!(left.is_empty(), "left: {left}\n{log}");
        assert!(log.contains("started") || log.contains("done"), "{log}");
    }
}
