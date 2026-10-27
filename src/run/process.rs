//! claudeとその子孫のプロセスを見つけて止める。止めきれなかったものは `stuck.json` に残し、なくなるまで次のtickを止める。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;

use crate::home::Home;

/// 止めきれなかったプロセス。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct Leftover {
    pub pid: u32,
    /// 始まった時刻（`ps -o lstart`）。PIDの使い回しと見分ける
    pub started: String,
}

/// 止めきれなかったプロセスの目印（`~/.agentcells/stuck.json`）。これがある間、tickは何も動かさない。
/// 次の仕事の書き込みのトークンを、残ったプロセスに読ませないため。
///
/// claudeが動いている間も、tick自身と、それまでに見つけたclaudeの子孫を書いておく。tickが殺されると
/// （launchdの登録し直し、ログアウト、Ctrl-Cなど）、後片付けをせずに終わり、別のプロセスグループで動く
/// claudeは残る。そのときも、残ったプロセスがなくなるまで、次のtickが動かないようにするため。
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub(super) struct Stuck {
    /// どの仕事か（`<owner>/<repo>/<番号>`）
    pub(super) job: String,
    pub(super) processes: Vec<Leftover>,
    /// claudeが動いている間に書いたときの、tick自身。これが生きていれば、まだ動いている途中で、残ったものではない
    #[serde(default)]
    pub(super) tick: Option<Leftover>,
}

/// そのプロセス（PIDと始まった時刻が同じもの）が、まだ動いているか。
pub(crate) fn is_alive_leftover(p: &Leftover) -> bool {
    process_table()
        .iter()
        .any(|r| r.0 == p.pid && r.3 == p.started)
}

/// 今のプロセス（tick自身）。
pub(super) fn current_process() -> Option<Leftover> {
    let me = std::process::id();
    process_table()
        .into_iter()
        .find(|r| r.0 == me)
        .map(|r| Leftover {
            pid: me,
            started: r.3,
        })
}

pub(super) fn stuck_path(home: &Home) -> PathBuf {
    home.root().join("stuck.json")
}

/// 前の実行で止めきれなかったプロセスが、まだ残っていれば、その説明。
/// どれも残っていなければ（同じPIDと始まった時刻のプロセスがなければ）、目印を消して `None`。
pub fn stuck(home: &Home) -> Option<String> {
    let path = stuck_path(home);
    let text = std::fs::read_to_string(&path).ok()?;
    let Ok(stuck) = serde_json::from_str::<Stuck>(&text) else {
        return Some(format!(
            "前の実行の目印 {} を読めません。確かめてから消してください",
            path.display()
        ));
    };
    let rows = process_table();
    let running = |p: &Leftover| rows.iter().any(|r| r.0 == p.pid && r.3 == p.started);
    // 書いたtickがまだ動いていれば、cellが動いている途中（次のtickは、tickのロックで止まる）
    if stuck.tick.as_ref().is_some_and(running) {
        return None;
    }
    let alive: Vec<&Leftover> = stuck.processes.iter().filter(|p| running(p)).collect();
    if alive.is_empty() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let pids: Vec<String> = alive.iter().map(|p| p.pid.to_string()).collect();
    Some(format!(
        "前の実行（{}）でAIが動かしたプロセスが残っています（PID: {}）。止めてから、そのIssue/PRの ac:working を外し、ラベルを付け直してください。プロセスがなくなれば、次のtickから動きます",
        stuck.job,
        pids.join(", ")
    ))
}

/// claudeの子孫に付ける、実行ごとの目印の環境変数。
pub(super) const RUN_MARK_ENV: &str = "AGENTCELLS_RUN";

/// claudeの子孫のプロセス。PIDは使い回されるので、始まった時刻と組にして覚え、同じものだけを止める。
#[derive(Default)]
pub(super) struct Processes {
    /// PIDと、始まった時刻（`ps -o lstart`）
    pub(super) known: BTreeMap<u32, String>,
    pub(super) groups: BTreeSet<u32>,
    /// `AGENTCELLS_RUN=<目印>`。環境変数にこれを持つプロセスも止める（Linuxだけ。macOSでは、
    /// sandboxの中のプロセスの環境変数を読めない）
    pub(super) mark: String,
    /// この仕事の場所。作業のディレクトリ（cwd）がこの中にあるプロセスも止める
    pub(super) dirs: Vec<PathBuf>,
}

/// 作業のディレクトリ（cwd）が `dirs` のどれかの中にあるプロセス。
fn working_in(dirs: &[PathBuf]) -> Vec<u32> {
    let dirs: Vec<PathBuf> = dirs.iter().filter_map(|d| d.canonicalize().ok()).collect();
    if dirs.is_empty() {
        return Vec::new();
    }
    let inside = |path: &Path| dirs.iter().any(|d| path.starts_with(d));
    if cfg!(target_os = "linux") {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        return entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| {
                std::fs::read_link(format!("/proc/{pid}/cwd"))
                    .map(|cwd| inside(&cwd))
                    .unwrap_or(false)
            })
            .collect();
    }
    // `-F pn` は、プロセスごとに `p<PID>`、`fcwd`、`n<パス>` の行を出す
    let Ok(out) = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-F", "pn"])
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut pid = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse::<u32>().ok();
        } else if let (Some(path), Some(p)) = (line.strip_prefix('n'), pid)
            && inside(Path::new(path))
        {
            found.push(p);
        }
    }
    found
}

/// 環境変数に `mark` を持つプロセス。macOSは `ps -E`、Linuxは `/proc/<pid>/environ` で読む。
/// 使う人のほかのプロセスの環境変数は、読むだけで、どこにも残さない。
fn marked(mark: &str) -> Vec<u32> {
    if mark.is_empty() {
        return Vec::new();
    }
    if cfg!(target_os = "linux") {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        return entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| {
                std::fs::read(format!("/proc/{pid}/environ"))
                    .map(|env| env.split(|b| *b == 0).any(|kv| kv == mark.as_bytes()))
                    .unwrap_or(false)
            })
            .collect();
    }
    let Ok(out) = Command::new("ps")
        .args(["-A", "-ww", "-E", "-o", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.split_whitespace().any(|w| w == mark))
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

/// 今のプロセスの一覧（PID、親のPID、プロセスグループ、始まった時刻）。
pub(super) fn process_table() -> Vec<(u32, u32, u32, String)> {
    let Ok(out) = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid=,lstart="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let pid = f.next()?.parse().ok()?;
            let ppid = f.next()?.parse().ok()?;
            let pgid = f.next()?.parse().ok()?;
            let started = f.collect::<Vec<_>>().join(" ");
            Some((pid, ppid, pgid, started))
        })
        .collect()
}

impl Processes {
    /// 覚えたプロセスを、動いている間の目印として書く。書けなくても、cellは止めない。
    pub(super) fn record(&self, path: &Path, job: &str, tick: Option<Leftover>) {
        let stuck = Stuck {
            job: job.to_string(),
            processes: self
                .known
                .iter()
                .map(|(pid, started)| Leftover {
                    pid: *pid,
                    started: started.clone(),
                })
                .collect(),
            tick,
        };
        // 書いている途中で殺されても壊れた目印が残らないよう、別のファイルに書いてから置き換える
        if let Ok(text) = serde_json::to_string_pretty(&stuck) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    /// 今のプロセスの一覧から、`root` の子孫（覚えているものの子孫も）と、覚えているグループのものを足す。
    /// 今の一覧を返す。
    pub(super) fn collect(&mut self, root: u32) -> Vec<(u32, u32, u32, String)> {
        let rows = process_table();
        let me = std::process::id();
        if let Some((_, _, _, started)) = rows.iter().find(|r| r.0 == root) {
            self.known.entry(root).or_insert_with(|| started.clone());
        }
        self.groups.insert(root);
        // 子孫を、増えなくなるまでたどる
        loop {
            let before = (self.known.len(), self.groups.len());
            for (pid, ppid, pgid, started) in &rows {
                if *pid == me {
                    continue;
                }
                let parent_known = self.same(&rows, *ppid);
                // グループの番号は使い回されるので、覚えたプロセスがまだいるグループだけを見る
                if parent_known || (self.groups.contains(pgid) && self.valid_group(&rows, *pgid)) {
                    self.known.entry(*pid).or_insert_with(|| started.clone());
                    // 自分のグループを作ったもの（Claude CodeのBashなど）は、そのグループも覚える
                    if *pgid > 1 {
                        self.groups.insert(*pgid);
                    }
                }
            }
            if (self.known.len(), self.groups.len()) == before {
                break;
            }
        }
        rows
    }

    /// グループに、覚えているものと同じプロセスがまだいるか（いなければ、番号が使い回されたかもしれない）。
    fn valid_group(&self, rows: &[(u32, u32, u32, String)], group: u32) -> bool {
        rows.iter().any(|r| r.2 == group && self.same(rows, r.0))
    }

    /// `pid` が、覚えているものと同じプロセス（始まった時刻も同じ）か。
    fn same(&self, rows: &[(u32, u32, u32, String)], pid: u32) -> bool {
        match self.known.get(&pid) {
            Some(started) => rows.iter().any(|r| r.0 == pid && &r.3 == started),
            None => false,
        }
    }

    /// 覚えたグループとプロセス、目印を持つプロセスをすべて止め、残っていないことを確かめる。
    /// 止めきれなければ、残ったものを返す。
    pub(super) fn stop_all(&mut self, root: u32) -> std::result::Result<(), Vec<Leftover>> {
        let mut last: Vec<Leftover> = Vec::new();
        let me = std::process::id();
        for _ in 0..10 {
            let rows = self.collect(root);
            let mut alive: Vec<u32> = self
                .known
                .keys()
                .copied()
                .filter(|pid| self.same(&rows, *pid) && is_alive(*pid))
                .collect();
            for pid in marked(&self.mark).into_iter().chain(working_in(&self.dirs)) {
                if pid != me && !alive.contains(&pid) && is_alive(pid) {
                    alive.push(pid);
                }
            }
            if alive.is_empty() {
                return Ok(());
            }
            last = alive
                .iter()
                .map(|pid| Leftover {
                    pid: *pid,
                    started: rows
                        .iter()
                        .find(|r| r.0 == *pid)
                        .map(|r| r.3.clone())
                        .unwrap_or_default(),
                })
                .collect();
            for group in self.groups.iter().filter(|g| self.valid_group(&rows, **g)) {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{group}")])
                    .stderr(Stdio::null())
                    .status();
            }
            for pid in alive {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status();
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        // 最後に止めたあとも、まだ残っているかを確かめる
        let rows = process_table();
        last.retain(|p| rows.iter().any(|r| r.0 == p.pid && r.3 == p.started) && is_alive(p.pid));
        if last.is_empty() { Ok(()) } else { Err(last) }
    }
}

/// プロセスのコマンド（止めきれなかったものを、人に伝えるため）。
pub(super) fn command_of(pid: u32) -> String {
    Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// 生きているか。ゾンビ（終わったが、親がまだ回収していない）は、生きているとみなさない。
pub(super) fn is_alive(pid: u32) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| {
            let stat = String::from_utf8_lossy(&o.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        })
        .unwrap_or(false)
}
