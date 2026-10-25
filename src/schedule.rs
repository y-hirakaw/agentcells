//! `agentcells schedule`。launchd（macOS）に、`agentcells tick` を決まった間隔で動かすように登録する。
//!
//! 仕事があれば、tickがcellを1件動かす。cellが動いている間に次の時刻が来ても、二重には動かない。
//! - launchdは、同じジョブがまだ動いていれば、次を起動しない
//! - tickは、cellが終わるまで `~/.agentcells/tick.lock` を持つ。取れなければ何もせずに終わる
//! - 動いているIssue/PRには `ac:working` が付いていて、tickはそれを拾わない
//!
//! 登録し直すときと外すときは、tickのロックを持ってからlaunchdを操作する。launchdは、ジョブを外すときに、
//! 動いているtickを止める。cellが動いている途中で止めると、別のプロセスグループで動くclaudeが残るため。
//!
//! launchdから動くtickは、ログインしたシェルの設定を読まない。そこで、登録したときのPATHを
//! plistに書く（gh、git、claude、betterleaksを、手元で動かすときと同じ場所から使うため）。

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::home::Home;
use crate::probe::Probe;

/// launchdのジョブの名前。
pub const LABEL: &str = "dev.agentcells.tick";

/// tickが使うプログラム。登録するPATHで、すべて見つかること。
const PROGRAMS: &[&str] = &["gh", "git", "claude", "betterleaks"];

/// 間隔（分）の範囲。
pub const MIN_EVERY: u32 = 1;
pub const MAX_EVERY: u32 = 24 * 60;

/// plistの置き場所（`~/Library/LaunchAgents/dev.agentcells.tick.plist`）。ログインのたびに、launchdが読み込む。
pub fn plist_path(user_home: &Path) -> PathBuf {
    user_home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// tickの出力を残すファイル。
pub fn log_path(home: &Home) -> PathBuf {
    home.root().join("logs/tick.log")
}

/// 登録する内容。
pub struct Job<'a> {
    /// 動かすagentcells（絶対パス）
    pub program: &'a Path,
    /// 間隔（分）
    pub every: u32,
    /// tickに渡すPATH
    pub path_env: &'a str,
    /// `AGENTCELLS_HOME` を設定して登録したときは、それも渡す
    pub agentcells_home: Option<&'a Path>,
    pub log: &'a Path,
}

impl Job<'_> {
    /// launchdのplist。ログを読みやすくするため、tickの前に時刻を書く（`/bin/sh` の `$0` にagentcellsを渡す）。
    pub fn plist(&self) -> String {
        let mut env = format!(
            "    <key>PATH</key>\n    <string>{}</string>\n",
            xml(self.path_env)
        );
        if let Some(home) = self.agentcells_home {
            env.push_str(&format!(
                "    <key>AGENTCELLS_HOME</key>\n    <string>{}</string>\n",
                xml(&home.to_string_lossy())
            ));
        }
        let log = xml(&self.log.to_string_lossy());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>-c</string>
    <string>echo "--- $(date '+%Y-%m-%d %H:%M:%S')"; exec "$0" tick</string>
    <string>{program}</string>
  </array>
  <key>StartInterval</key>
  <integer>{seconds}</integer>
  <key>RunAtLoad</key>
  <false/>
  <key>EnvironmentVariables</key>
  <dict>
{env}  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
            program = xml(&self.program.to_string_lossy()),
            seconds = self.every * 60,
        )
    }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// PATHから、相対パスの要素（`.` や空の要素）を外す。launchdでは作業のディレクトリが `/` なので、
/// 登録したときと同じものを指さないため。
pub fn absolute_path(path_env: &str) -> String {
    let dirs: Vec<PathBuf> = std::env::split_paths(path_env)
        .filter(|dir| dir.is_absolute())
        .collect();
    std::env::join_paths(dirs)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// PATHの中から、実行できるプログラムを探す。
fn find_in(path_env: &str, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path_env)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// ログがこれより大きくなったら、`tick.log.1` に回す（前の `.1` は消える）。
const LOG_LIMIT: u64 = 1024 * 1024;

/// ログが大きくなっていたら回す。tickの最初に呼ぶ。回せなくても、tickは止めない。
pub fn rotate_log(home: &Home) {
    let log = log_path(home);
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = std::fs::rename(&log, log.with_extension("log.1"));
    }
}

/// 登録するPATHで見つからないプログラム。
pub fn missing_programs(path_env: &str) -> Vec<&'static str> {
    PROGRAMS
        .iter()
        .copied()
        .filter(|name| find_in(path_env, name).is_none())
        .collect()
}

/// launchdの、このユーザーの領域（`gui/<uid>`）。
fn domain(probe: &dyn Probe) -> Result<String> {
    let out = probe
        .run("id", &["-u"])
        .filter(|o| o.success)
        .ok_or_else(|| Error::new("ユーザーのIDがわかりません（`id -u` が失敗しました）"))?;
    Ok(format!("gui/{}", out.stdout.trim()))
}

fn launchctl(probe: &dyn Probe, args: &[&str]) -> Result<()> {
    let out = probe
        .run("launchctl", args)
        .ok_or_else(|| Error::new("launchctlが見つかりません（launchdはmacOSだけで使えます）"))?;
    if !out.success {
        return Err(Error::new(format!(
            "launchctl {} が失敗しました: {}",
            args.first().unwrap_or(&""),
            format!("{} {}", out.stdout.trim(), out.stderr.trim()).trim()
        )));
    }
    Ok(())
}

/// tickが動いていれば（cellが動いているかもしれない）、断る。
fn lock(home: &Home) -> Result<crate::tick::Lock> {
    crate::tick::lock(home)?.ok_or_else(|| {
        Error::new("tickが動いています（cellが動いているかもしれません）")
            .with_hint("終わってから、もう一度実行してください。`agentcells status` で、作業中のものがわかります")
    })
}

/// plistを書いて、launchdに登録する。すでに登録してあれば、入れ替える。
pub fn install(home: &Home, probe: &dyn Probe, plist: &Path, job: &Job) -> Result<()> {
    if !(MIN_EVERY..=MAX_EVERY).contains(&job.every) {
        return Err(Error::new(format!(
            "間隔は {MIN_EVERY}〜{MAX_EVERY} 分で指定してください"
        )));
    }
    let missing = missing_programs(job.path_env);
    if !missing.is_empty() {
        return Err(Error::new(format!(
            "今のPATHに {} が見つかりません。登録するtickは、今のPATHでプログラムを探します",
            missing.join(", ")
        ))
        .with_hint("`agentcells doctor` で確かめ、見つかるシェルから、もう一度実行してください"));
    }
    if let Some(dir) = job.log.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::new(format!("{} を作れません: {e}", dir.display())))?;
    }
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::new(format!("{} を作れません: {e}", dir.display())))?;
    }
    let domain = domain(probe)?;
    // 外すときに、動いているtickを止めないように、終わるまでロックを持つ
    let _lock = lock(home)?;
    std::fs::write(plist, job.plist())
        .map_err(|e| Error::new(format!("{} を書けません: {e}", plist.display())))?;
    // 前の登録があれば外す（なければ失敗するが、かまわない）
    let replaced = launchctl(probe, &["bootout", &format!("{domain}/{LABEL}")]).is_ok();
    // 外した直後は、外れきっておらず読み込めないことがあるので、少し待ってやり直す
    let plist = plist.to_string_lossy();
    let mut result = launchctl(probe, &["bootstrap", &domain, &plist]);
    for _ in 0..5 {
        if result.is_ok() || !replaced {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        result = launchctl(probe, &["bootstrap", &domain, &plist]);
    }
    result
}

/// launchdから外して、plistを消す。登録していなければ `false`。
pub fn remove(home: &Home, probe: &dyn Probe, plist: &Path) -> Result<bool> {
    let domain = domain(probe)?;
    let _lock = lock(home)?;
    let loaded = launchctl(probe, &["bootout", &format!("{domain}/{LABEL}")]).is_ok();
    let existed = plist.exists();
    if existed {
        std::fs::remove_file(plist)
            .map_err(|e| Error::new(format!("{} を消せません: {e}", plist.display())))?;
    }
    Ok(loaded || existed)
}

/// 登録の様子。`doctor` で出す。
#[derive(Debug, PartialEq, Eq)]
pub enum State {
    /// plistがない
    NotInstalled,
    /// plistはあるが、launchdに読み込まれていない
    NotLoaded,
    /// plistにあるagentcellsがない
    MissingProgram(PathBuf),
    /// 動いている。間隔（分）
    Loaded { every: Option<u32> },
}

pub fn state(probe: &dyn Probe, plist: &Path) -> State {
    let Ok(text) = std::fs::read_to_string(plist) else {
        return State::NotInstalled;
    };
    if let Some(program) = program_in(&text)
        && !program.is_file()
    {
        return State::MissingProgram(program);
    }
    let loaded = domain(probe)
        .and_then(|d| launchctl(probe, &["print", &format!("{d}/{LABEL}")]))
        .is_ok();
    if !loaded {
        return State::NotLoaded;
    }
    State::Loaded {
        every: every_in(&text),
    }
}

/// plistの、ProgramArgumentsの最後（agentcells）。
fn program_in(plist: &str) -> Option<PathBuf> {
    let args = plist.split("<key>ProgramArguments</key>").nth(1)?;
    let args = args.split("</array>").next()?;
    let last = args.rsplit("<string>").next()?.split("</string>").next()?;
    Some(PathBuf::from(
        last.replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    ))
}

fn every_in(plist: &str) -> Option<u32> {
    let rest = plist.split("<key>StartInterval</key>").nth(1)?;
    let seconds: u32 = rest
        .split("<integer>")
        .nth(1)?
        .split("</integer>")
        .next()?
        .trim()
        .parse()
        .ok()?;
    Some(seconds / 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::fake::FakeProbe;

    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, "").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn bin_with_programs() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in PROGRAMS.iter().chain(&["agentcells"]) {
            executable(&dir.path().join(name));
        }
        dir
    }

    #[test]
    fn programs_must_be_executable_and_in_absolute_directories() {
        use std::os::unix::fs::PermissionsExt;
        let bin = bin_with_programs();
        let gh = bin.path().join("gh");
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path_env = format!(".::{}", bin.path().display());
        assert_eq!(absolute_path(&path_env), bin.path().display().to_string());
        assert_eq!(missing_programs(&path_env), ["gh"]);
    }

    #[test]
    fn rotates_large_logs() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().to_path_buf());
        let log = log_path(&home);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "x").unwrap();
        rotate_log(&home);
        assert!(log.exists());
        std::fs::write(&log, vec![b'x'; LOG_LIMIT as usize + 1]).unwrap();
        rotate_log(&home);
        assert!(!log.exists());
        assert!(log.with_extension("log.1").exists());
    }

    #[test]
    fn installs_a_plist_that_runs_tick_with_the_current_path() {
        let bin = bin_with_programs();
        let home = tempfile::tempdir().unwrap();
        let plist = plist_path(home.path());
        let program = bin.path().join("agentcells");
        let path_env = format!("{}:/usr/bin", bin.path().display());
        let log = home.path().join(".agentcells/logs/tick.log");
        let job = Job {
            program: &program,
            every: 10,
            path_env: &path_env,
            agentcells_home: None,
            log: &log,
        };
        let probe = FakeProbe::default()
            .with("id -u", true, "501\n")
            .with(&format!("launchctl bootout gui/501/{LABEL}"), false, "")
            .with(
                &format!("launchctl bootstrap gui/501 {}", plist.display()),
                true,
                "",
            )
            .with(&format!("launchctl print gui/501/{LABEL}"), true, "");

        let agentcells = Home::new(home.path().join(".agentcells"));
        std::fs::create_dir_all(agentcells.root()).unwrap();
        // tickが動いている間は、launchdを触らずに断る
        {
            let _tick = crate::tick::lock(&agentcells).unwrap().unwrap();
            let err = install(&agentcells, &probe, &plist, &job).unwrap_err();
            assert!(
                err.message.contains("tickが動いています"),
                "{}",
                err.message
            );
            assert!(remove(&agentcells, &probe, &plist).is_err());
            assert!(!plist.exists());
            assert!(
                !probe
                    .calls
                    .borrow()
                    .iter()
                    .any(|c| c.starts_with("launchctl"))
            );
        }
        // ほかのテストが子のプロセスを作ると、ロックがすぐには外れないことがある（tickのロックのテストと同じ）。少し待つ
        let installed = (0..50).any(|_| {
            let ok = install(&agentcells, &probe, &plist, &job).is_ok();
            if !ok {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            ok
        });
        assert!(installed);
        let text = std::fs::read_to_string(&plist).unwrap();
        assert!(text.contains("<integer>600</integer>"));
        assert!(text.contains(&format!("<string>{path_env}</string>")));
        assert!(!text.contains("AGENTCELLS_HOME"));
        assert!(log.parent().unwrap().is_dir());
        assert_eq!(state(&probe, &plist), State::Loaded { every: Some(10) });
        assert_eq!(program_in(&text), Some(program.clone()));

        // agentcellsを消すと、doctorで気づける
        std::fs::remove_file(&program).unwrap();
        assert_eq!(state(&probe, &plist), State::MissingProgram(program));
    }

    #[test]
    fn refuses_when_programs_are_not_in_the_path_or_the_interval_is_wrong() {
        let bin = tempfile::tempdir().unwrap();
        executable(&bin.path().join("git"));
        let home = tempfile::tempdir().unwrap();
        let plist = plist_path(home.path());
        let path_env = bin.path().display().to_string();
        let log = home.path().join("tick.log");
        let mut job = Job {
            program: Path::new("/x/agentcells"),
            every: 10,
            path_env: &path_env,
            agentcells_home: None,
            log: &log,
        };
        let probe = FakeProbe::default();
        let agentcells = Home::new(home.path().to_path_buf());
        let err = install(&agentcells, &probe, &plist, &job).unwrap_err();
        assert!(
            err.message.contains("gh, claude, betterleaks"),
            "{}",
            err.message
        );
        job.every = 0;
        assert!(install(&agentcells, &probe, &plist, &job).is_err());
        assert!(!plist.exists());
        assert!(probe.calls.borrow().is_empty());
    }

    #[test]
    fn escapes_paths_and_passes_agentcells_home() {
        let job = Job {
            program: Path::new("/a&b/agentcells"),
            every: 5,
            path_env: "/usr/bin",
            agentcells_home: Some(Path::new("/h<1>")),
            log: Path::new("/l/tick.log"),
        };
        let text = job.plist();
        assert!(text.contains("<string>/a&amp;b/agentcells</string>"));
        assert!(text.contains("<string>/h&lt;1&gt;</string>"));
        assert!(text.contains("<integer>300</integer>"));
        assert_eq!(program_in(&text), Some(PathBuf::from("/a&b/agentcells")));
    }

    #[test]
    fn removes_the_job_and_the_plist() {
        let home = tempfile::tempdir().unwrap();
        let plist = plist_path(home.path());
        let probe = FakeProbe::default().with("id -u", true, "501\n").with(
            &format!("launchctl bootout gui/501/{LABEL}"),
            false,
            "",
        );
        let agentcells = Home::new(home.path().to_path_buf());
        assert!(!remove(&agentcells, &probe, &plist).unwrap());
        assert_eq!(state(&probe, &plist), State::NotInstalled);
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "").unwrap();
        assert_eq!(state(&probe, &plist), State::NotLoaded);
        assert!(remove(&agentcells, &probe, &plist).unwrap());
        assert!(!plist.exists());
    }
}
