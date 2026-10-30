use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use agentcells::actions;
use agentcells::config::{Config, Target};
use agentcells::doctor::{self, Status};
use agentcells::error::{Error, Result};
use agentcells::files;
use agentcells::flow;
use agentcells::github::{self, Purpose};
use agentcells::guard;
use agentcells::home::{Home, RepoName};
use agentcells::inspect;
use agentcells::labels;
use agentcells::probe::SystemProbe;
use agentcells::prompt;
use agentcells::record;
use agentcells::run;
use agentcells::schedule;
use agentcells::setup;
use agentcells::status;
use agentcells::tick;
use agentcells::tidy;
use agentcells::view;

#[derive(Parser)]
#[command(version, about = "Issueを作ると、AIエージェントのcellが仕事を進める")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 動かすのに必要なものがそろっているかを確かめ、だめなら直し方を出す
    Doctor,
    /// 作業データの置き場所を用意する（何度実行してもよい）
    Setup,
    /// 設定ファイル（config.toml）を扱う
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// gitignoreされたファイルを扱う
    Files {
        #[command(subcommand)]
        command: FilesCommands,
    },
    /// GitHubのラベルを扱う
    Labels {
        #[command(subcommand)]
        command: LabelsCommands,
    },
    /// cellとして動くAIに渡すプロンプトを扱う
    Prompt {
        #[command(subcommand)]
        command: PromptCommands,
    },
    /// 仕事を探して、cellを動かす（定期実行で呼ぶ）
    Tick {
        /// 動かさずに、見つかった仕事と、次に動かすものだけを表示する
        #[arg(long)]
        dry_run: bool,
        /// 仕事が残っていれば、始めてからこの分数がたつまで、続けて動かす（0なら1件だけ）
        #[arg(long, default_value_t = 30)]
        max_minutes: u64,
        /// 使用量の上限などで止まっている間でも動かす
        #[arg(long)]
        ignore_pause: bool,
    },
    /// tickの記録を調べてラインの不具合を見つけ、原因と直し方の案を書く（[inspector] を書いたとき。
    /// tickも、1日に1回、最後にこれを動かす）
    Inspect {
        /// AIは動かさずに、規則で見つけたもの（AIに渡す、伏せ字にした記録）だけを表示する
        #[arg(long)]
        dry_run: bool,
        /// 動かさずに、2段目が手元に残した最近のメモを表示する
        #[arg(long)]
        notes: bool,
    },
    /// 動いているcellと、agentcellsが扱っているIssueごとに、今どこまで進んでいるかを出す（読むだけ）
    Status {
        /// 動いているcellの様子だけを、2秒ごとに出し直す（GitHubは読まない。Ctrl-Cで終わる）
        #[arg(long)]
        watch: bool,
    },
    /// productごとのIssue/PRの状態と、動いているcellの様子を、1つの画面で出し続ける（読むだけ。Ctrl-Cで終わる）
    Dashboard {
        /// GitHubを読み直す間隔（秒。10より短くはしない）。cellの様子は2秒ごとに読み直す
        #[arg(long, default_value_t = 60)]
        interval: u64,
    },
    /// 定期実行（macOSのlaunchd）を扱う
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommands,
    },
    /// cellとして動くAIが使うコマンド（agentcellsがAIを起動したときだけ使える）
    Cell {
        #[command(subcommand)]
        command: CellCommands,
    },
}

#[derive(Subcommand)]
enum CellCommands {
    /// gitを読む（status / diff / log / show / blame）
    Git {
        subcommand: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// worktreeの中のファイルやディレクトリを消す
    Rm {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// worktreeの中で、ファイルやディレクトリを動かす
    Mv { from: PathBuf, to: PathBuf },
    /// worktreeの変更をすべてコミットする（置いた秘密のファイルはコミットしない）
    Commit {
        #[arg(short, long)]
        message: String,
    },
    /// 設定（product.commands）に書いたコマンドを実行する
    Run { name: String },
    /// ベースのブランチ（origin/<ベース>）を取り込む。ぶつかったら、直してから commit する
    Merge {
        /// 途中のマージをやめて、取り込む前に戻す
        #[arg(long)]
        abort: bool,
    },
    /// Webのページを読む（httpsのGET）。開けるのは、productの [product.sandbox] の network にあるホストだけ
    Fetch {
        url: String,
        /// 何バイト目から出すか（長いページの続きを読むとき）
        #[arg(long, default_value_t = 0)]
        from: usize,
    },
    /// Bashを使う前にClaude Codeが動かすhook。`agentcells cell` 以外のコマンドを、代わりに使うものを添えて断る
    #[command(hide = true)]
    Hook,
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// productとcellのつながりを、工場のラインとして描いたHTMLファイルに書き出す（読むだけ。開くのは自分で）
    View {
        /// 書き出す先（省略すると ~/.agentcells/config.html）
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ScheduleCommands {
    /// `agentcells tick` を決まった間隔で動かすように、launchdに登録する（登録してあれば入れ替える）。
    /// 仕事があれば、tickがcellを1件動かす。cellが動いている間は、次のtickは何もしない
    Install {
        /// 間隔（分）
        #[arg(long, default_value_t = 10)]
        every: u32,
    },
    /// 定期実行の登録を外す
    Remove,
}

#[derive(Subcommand)]
enum PromptCommands {
    /// 標準のプロンプト、追加の指示、今回の仕事を組み立てて表示する
    Show {
        /// cellの名前
        cell: String,
        /// このIssueを今回の仕事にする（owner/repo#12）
        #[arg(long, conflicts_with = "pr")]
        issue: Option<String>,
        /// このPRを今回の仕事にする（owner/repo#12）
        #[arg(long)]
        pr: Option<String>,
    },
}

#[derive(Subcommand)]
enum LabelsCommands {
    /// 設定ファイルのproductとcellに必要なラベルのうち、足りないものを作る（あるものは触らない）
    Sync {
        /// 作らずに、作る予定のラベルだけを表示する
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum FilesCommands {
    /// files/<owner>/<repo>/ の下のファイルを、worktreeの同じパスに置く
    Apply {
        /// 作業するリポジトリ（owner/repo）
        repo: String,
        /// ファイルを置くworktree
        worktree: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("エラー: {err}");
            if let Some(hint) = &err.hint {
                eprintln!("直し方: {hint}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run(command: Commands) -> Result<ExitCode> {
    let home = Home::from_env()?;
    match command {
        Commands::Doctor => Ok(print_doctor(&home)),
        Commands::Setup => {
            let report = setup::run(&home)?;
            for dir in &report.created {
                println!("作成: {}", dir.display());
            }
            if report.created.is_empty() {
                println!("置き場所はそろっています: {}", home.root().display());
            }
            println!();
            Ok(print_doctor(&home))
        }
        Commands::Config {
            command: ConfigCommands::View { out },
        } => {
            let config = Config::load(&home.config_path())?;
            let factory = flow::read(&config);
            let out = out.unwrap_or_else(|| home.root().join("config.html"));
            std::fs::write(&out, view::render(&factory)).map_err(|e| {
                Error::new(format!("{} に書き出せません: {e}", out.display()))
                    .with_hint("--out で、書き込める場所を指定してください")
            })?;
            println!("書き出しました: {}", out.display());
            let notes = factory.notes();
            if notes.is_empty() {
                println!("注意はありません");
            } else {
                println!("注意: {}件", notes.len());
                for note in &notes {
                    println!("  - {note}");
                }
            }
            println!("ブラウザで開いて見てください");
            Ok(ExitCode::SUCCESS)
        }
        Commands::Files {
            command: FilesCommands::Apply { repo, worktree },
        } => {
            let repo = RepoName::parse(&repo)?;
            if !worktree.is_dir() {
                return Err(Error::new(format!("{} がありません", worktree.display()))
                    .with_hint("worktreeのパスを確かめてください"));
            }
            let placed = files::apply(&home, &repo, &worktree)?;
            if placed.is_empty() {
                println!(
                    "置くファイルはありません（{}）",
                    home.files_for(&repo).display()
                );
            }
            for rel in placed {
                println!("配置: {}", rel.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Labels {
            command: LabelsCommands::Sync { dry_run },
        } => {
            let config = Config::load(&home.config_path())?;
            let result = labels::sync(&config, &SystemProbe, dry_run)?;
            if result.is_empty() {
                println!("productがないので、作るラベルはありません");
            }
            let verb = if dry_run { "作る予定" } else { "作成" };
            for (repo, created) in result {
                if created.is_empty() {
                    println!("{repo}: そろっています");
                }
                for label in created {
                    println!("{repo}: {verb} {}", label.name);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Cell { command } => run_cell(command),
        Commands::Schedule { command } => {
            let plist = schedule_plist().ok_or_else(|| {
                Error::new("定期実行の登録は、macOS（launchd）だけで使えます").with_hint(
                    "ほかの環境では、cronなどで `agentcells tick` を定期的に動かしてください",
                )
            })?;
            match command {
                ScheduleCommands::Install { every } => {
                    let program = std::env::current_exe()
                        .and_then(|p| p.canonicalize())
                        .map_err(|e| Error::new(format!("agentcellsの場所がわかりません: {e}")))?;
                    let path_env =
                        schedule::absolute_path(&std::env::var("PATH").unwrap_or_default());
                    let agentcells_home = std::env::var_os("AGENTCELLS_HOME")
                        .filter(|v| !v.is_empty())
                        .map(PathBuf::from);
                    let log = schedule::log_path(&home);
                    let job = schedule::Job {
                        program: &program,
                        every,
                        path_env: &path_env,
                        agentcells_home: agentcells_home.as_deref(),
                        log: &log,
                    };
                    schedule::install(&home, &SystemProbe, &plist, &job)?;
                    println!(
                        "登録しました: {every}分ごとに `{} tick` を動かします（最初は{every}分後）",
                        program.display()
                    );
                    println!("ログ: {}", log.display());
                    println!(
                        "PATH: 今のPATHを渡します（gh、git、claude、betterleaksを同じ場所から使うため）"
                    );
                    let bin_dir = home.bin_dir().canonicalize().unwrap_or(home.bin_dir());
                    if !program.starts_with(&bin_dir) {
                        println!(
                            "注意: {} の外のagentcellsを登録しました。install.sh で入れたもの（{}）から登録すると、更新しても登録し直さずに済みます",
                            home.bin_dir().display(),
                            home.bin_dir().join("agentcells").display()
                        );
                    }
                    println!("外すときは `agentcells schedule remove` を実行してください");
                }
                ScheduleCommands::Remove => {
                    if schedule::remove(&home, &SystemProbe, &plist)? {
                        println!("定期実行の登録を外しました");
                    } else {
                        println!("定期実行は登録されていません");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Status { watch: true } => loop {
            // 画面を消して、上から書き直す
            print!("\x1b[2J\x1b[H");
            println!("動いているcell（2秒ごとに更新。Ctrl-Cで終わる）\n");
            for line in agentcells::running::describe(&home, unix_now(), 20) {
                println!("{line}");
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        },
        Commands::Status { watch: false } => {
            println!("動いているcell");
            for line in agentcells::running::describe(&home, unix_now(), 5) {
                println!("  {line}");
            }
            for notice in tick_notices(&home, unix_now()) {
                println!("  ※{notice}");
            }
            let runs = agentcells::recent::read(&home);
            println!("\n最近の実行（新しい順）");
            if runs.is_empty() {
                println!("  まだありません");
            }
            for run in agentcells::recent::latest(&runs, 5) {
                println!("  {}", run.describe(unix_now()));
            }
            println!();
            let mut reports = read_reports(&home)?;
            let now = unix_now();
            let running = agentcells::running::load(&home);
            let mut stopped = false;
            for line in reports.iter_mut().flat_map(|(_, r)| r.lines.iter_mut()) {
                line.check_stopped(running.as_ref().map(|(r, alive)| (r, *alive)), now);
                stopped |= matches!(line.state, status::State::Stopped { .. });
            }
            if reports.is_empty() {
                println!("productがないので、扱っているIssueはありません");
            }
            let mut failed = false;
            for (i, (name, report)) in reports.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                println!("{name}");
                for error in &report.errors {
                    println!("  エラー: {error}");
                }
                failed |= !report.errors.is_empty();
                for note in &report.notes {
                    println!("  注意: {note}");
                }
                if report.lines.is_empty() {
                    println!("  扱っているIssue/PRはありません");
                }
                for line in &report.lines {
                    println!("  {}", line.describe(now));
                    // 動いているものは、前回の結果より今の様子（上の「動いているcell」）を見る
                    if !matches!(line.state, status::State::Working { .. })
                        && let Some(run) = agentcells::recent::last_for(&runs, line)
                    {
                        println!("      {}", run.describe_last(now));
                    }
                }
            }
            if stopped {
                println!("\n※{}", status::STOPPED_HINT);
            }
            // スクリプトから呼んだときに、読めなかったことに気づけるように
            Ok(if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Commands::Dashboard { interval } => {
            let interval = interval.max(10);
            let snapshot = std::sync::Arc::new(std::sync::Mutex::new(
                agentcells::dashboard::Snapshot::default(),
            ));
            // 動いているcellが変わったら、間隔を待たずにGitHubを読み直すよう、読むスレッドを起こす
            let (wake, woken) = std::sync::mpsc::channel::<()>();
            // GitHubは時間がかかるので、別のスレッドで読み直す。画面は、その間も書き直す
            {
                let snapshot = std::sync::Arc::clone(&snapshot);
                let home = home.clone();
                std::thread::spawn(move || {
                    loop {
                        let read = read_reports(&home);
                        let runs = agentcells::recent::read(&home);
                        {
                            let mut s = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                            s.runs = runs;
                            match read {
                                Ok(products) => {
                                    s.products = products;
                                    s.read_at = Some(unix_now());
                                    s.error = None;
                                }
                                Err(e) => s.error = Some(e.message),
                            }
                        }
                        // 起こされるか、間隔が過ぎるまで待つ
                        let _ = woken.recv_timeout(std::time::Duration::from_secs(interval));
                    }
                });
            }
            let mut last_running = None;
            loop {
                let now = unix_now();
                let running = agentcells::running::load(&home);
                let current = running
                    .as_ref()
                    .map(|(r, _)| (r.repo.clone(), r.number, r.started));
                if last_running.as_ref().is_some_and(|last| *last != current) {
                    let _ = wake.send(());
                }
                last_running = Some(current);
                let notices = tick_notices(&home, now);
                let lines = {
                    let s = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                    agentcells::dashboard::render(
                        &s,
                        running.as_ref().map(|(r, alive)| (r, *alive)),
                        &notices,
                        interval,
                        now,
                    )
                };
                // 画面を消して、上から書き直す
                print!("\x1b[2J\x1b[H");
                for line in lines {
                    println!("{line}");
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
        Commands::Tick {
            dry_run,
            max_minutes,
            ignore_pause,
        } => {
            schedule::rotate_log(&home);
            let Some(_lock) = tick::lock(&home)? else {
                println!("ほかのtickが動いているので、何もしません");
                return Ok(ExitCode::SUCCESS);
            };
            // ロックを取れたので、動いているcellはない。殺されたtickが残した印を片付ける
            agentcells::running::finish(&home);
            // 使用量の上限などで止まっている間は、新しく始めない（試すだけなら、知らせて続ける）
            if let Some(pause) = agentcells::limit::active(&home, unix_now())
                && !ignore_pause
            {
                println!(
                    "{}のため、あと{}は新しく始めません（急ぐときは `agentcells tick --ignore-pause`）",
                    pause.reason,
                    agentcells::running::elapsed(pause.until.saturating_sub(unix_now()))
                );
                if !dry_run {
                    return Ok(ExitCode::SUCCESS);
                }
            }
            // 動いたことを記録する（inspectorの材料）。試すだけ（--dry-run）なら記録しない
            record::rotate(&home);
            let recorder = (!dry_run).then(|| record::Recorder::new(&home, unix_now()));
            let note = |event: record::Event| {
                if let Some(recorder) = &recorder {
                    recorder.write(unix_now(), event);
                }
            };
            note(record::Event::Started {
                version: env!("CARGO_PKG_VERSION").to_string(),
                max_minutes,
            });
            // 終わった理由を記録する。ここから先でエラーになったら error とその中身
            let mut why = "error";
            let mut runs = 0;
            let result = (|| -> Result<Config> {
                // 前の実行でAIのプロセスを止めきれなかったら、それが残っている間は何も動かさない
                // （次の仕事の書き込みのトークンを、残ったプロセスに読ませないため）
                if let Some(note_text) = run::stuck(&home) {
                    why = "stuck";
                    return Err(Error::new(note_text).with_hint(
                        "`agentcells doctor` でも確かめられます。プロセスがなくなれば、次のtickから動きます",
                    ));
                }
                let config = Config::load(&home.config_path())?;
                let credentials_for = |repo: &RepoName| {
                    github::credentials(&config, &SystemProbe, repo, Purpose::Read)
                };
                let actors = guard::Actors {
                    // 設定を読むときに、productがあれば operator は必ずある
                    operator: config.operator.clone().unwrap_or_default(),
                    writer: prompt::writer_login(&config, &SystemProbe)?,
                };
                // ラベルが足りないと、仕事が見つからなかったり、ラベルを付け替えられなかったりする
                match labels::missing(&config, &SystemProbe) {
                    Ok(missing) if !missing.is_empty() => println!(
                        "注意: 足りないラベルがあります（`agentcells labels sync` で作れます）:\n{}\n",
                        labels::describe_missing(&missing)
                    ),
                    Ok(_) => {}
                    Err(e) => println!("注意: ラベルを確かめられません: {}\n", e.message),
                }
                // 閉じたものに残ったラベルを片付ける。できなくても、仕事は進める
                let (leftovers, errors) = tidy::find(&config, &SystemProbe, &credentials_for);
                for error in &errors {
                    println!("注意: 片付けるものを探せません: {error}");
                }
                if dry_run {
                    for l in &leftovers {
                        println!("片付ける予定: {} を外す", l.describe());
                    }
                } else if !leftovers.is_empty() {
                    let write_for = |repo: &RepoName| {
                        github::credentials(&config, &SystemProbe, repo, Purpose::Work)
                    };
                    let results = tidy::remove(&SystemProbe, &leftovers, &write_for);
                    for (l, result) in leftovers.iter().zip(results) {
                        note(record::Event::tidied(
                            l.kind,
                            &l.repo.to_string(),
                            l.number,
                            &l.labels,
                            result.as_ref().err().map(|e| e.message.clone()),
                        ));
                        match result {
                            Ok(()) => println!("片付け: {} を外しました", l.describe()),
                            Err(e) => {
                                println!("注意: {} を外せません: {}", l.describe(), e.message)
                            }
                        }
                    }
                }
                if !errors.is_empty() || !leftovers.is_empty() {
                    println!();
                }
                let agentcells = std::env::current_exe()
                    .map_err(|e| Error::new(format!("agentcellsの場所がわかりません: {e}")))?;
                // 試すときに、安いモデルで動かせるようにする
                let model = std::env::var("AGENTCELLS_MODEL").ok();
                // 仕事が残っていれば、上限まで続けて探して動かす
                let mut session = tick::Session::new(std::time::Duration::from_secs(
                    max_minutes.saturating_mul(60),
                ));
                // 見送りは、読み直すたびに同じものが出るので、1回のtickでは1度だけ出す
                let mut told: std::collections::HashSet<String> = std::collections::HashSet::new();
                loop {
                    if session.runs() > 0 {
                        // 止めきれなかったプロセスが残ったら、ほかの仕事も動かさない
                        if let Some(note_text) = run::stuck(&home) {
                            why = "stuck";
                            return Err(Error::new(note_text).with_hint(
                                "`agentcells doctor` でも確かめられます。プロセスがなくなれば、次のtickから動きます",
                            ));
                        }
                        if let Some(halted) = session.halted() {
                            println!(
                                "\n{}ので、残りは次のtickで動かします",
                                if halted == "usage" {
                                    "使用量が設定を超えた"
                                } else {
                                    "AIの実行が失敗した"
                                }
                            );
                            why = halted;
                            break;
                        }
                        if session.exhausted() {
                            println!(
                                "\nこのtickの上限（{max_minutes}分、{}件）に達したので、残りは次のtickで動かします",
                                tick::MAX_RUNS
                            );
                            why = "limit";
                            break;
                        }
                        println!(
                            "\n--- 続けて、仕事を探します（このtickで{}件目のあと）",
                            session.runs()
                        );
                    }
                    let mut quiet = tick::Quiet::load(&home);
                    let found =
                        tick::find(&config, &SystemProbe, &credentials_for, &actors, &mut quiet)?;
                    quiet.save(&home);
                    for (w, reason) in &found.skipped {
                        let line = format!("見送り: {}（{reason}）", w.describe());
                        if told.insert(line.clone()) {
                            println!("{line}");
                            note(record::Event::Skipped {
                                subject: record::Subject::of(w),
                                reason: reason.clone(),
                            });
                        }
                    }
                    let work = found.work;
                    let Some(next) = session.next(&work) else {
                        if session.runs() == 0 {
                            println!("仕事はありません");
                        } else {
                            println!("ほかに仕事はありません");
                        }
                        why = "no_work";
                        break;
                    };
                    println!("仕事: {}件", work.len());
                    for w in &work {
                        println!("  {}", w.describe());
                    }
                    println!("\n次に動かすもの: {}", next.describe());
                    if dry_run {
                        why = "dry_run";
                        break;
                    }
                    let scanner = find_program("betterleaks").ok_or_else(|| {
                        Error::new("betterleaksが見つかりません")
                            .with_hint("`agentcells doctor` で確かめてください")
                    })?;
                    let claude = find_program("claude").ok_or_else(|| {
                        Error::new("claudeが見つかりません")
                            .with_hint("`agentcells doctor` で確かめてください")
                    })?;
                    let runner = run::Runner {
                        home: &home,
                        config: &config,
                        probe: &SystemProbe,
                        actors: &actors,
                        claude: &claude,
                        agentcells: &agentcells,
                        scanner: &scanner,
                        model: model.as_deref(),
                    };
                    println!("動かしています...");
                    let started = unix_now();
                    let report = match runner.run(next) {
                        Ok(report) => report,
                        Err(e) => {
                            let now = unix_now();
                            note(record::Event::Failed {
                                subject: record::Subject::of(next),
                                started,
                                seconds: now.saturating_sub(started),
                                error: e.message.clone(),
                            });
                            // claudeを動かしたあとの失敗なら、その中身も読めるように残す
                            if config.inspector.is_some() {
                                agentcells::notes::keep_digest(
                                    &home, next, "failed", 0, started, now,
                                );
                            }
                            return Err(e);
                        }
                    };
                    note(record::Event::ran(next, started, unix_now(), &report));
                    // inspectorの2段目のために、実行の中身の要約を残す（claudeの出力は、次の実行で上書きされる）
                    if config.inspector.is_some() && report.destination != "skipped" {
                        agentcells::notes::keep_digest(
                            &home,
                            next,
                            &report.destination,
                            report.round,
                            started,
                            unix_now(),
                        );
                    }
                    let skipped = report.destination == "skipped";
                    match &report.reason {
                        Some(reason) if skipped => println!("見送りました: {reason}"),
                        Some(reason) if report.destination == "paused" => {
                            println!(
                                "止めました: {reason}（人間には回さず、元のラベルに戻しました）"
                            )
                        }
                        Some(reason) => println!("{} に進めました（{reason}）", report.destination),
                        None => println!("{} に進めました", report.destination),
                    }
                    if let Some(note) = agentcells::blocked::note(&report.blocked) {
                        println!("{note}");
                    }
                    session.ran(next, skipped);
                    runs = session.runs();
                    // 使用量が設定を超えたら、上限に当たる前に止まる（使う人が自分で使う分を残すため）
                    if let Some(near) = report.near_limit.as_ref().filter(|_| !report.ai_failed) {
                        session.halt("usage");
                        let reason = near.reason();
                        agentcells::limit::pause(&home, near.until, &reason);
                        println!(
                            "{reason}のため、あと{}は新しく始めません（急ぐときは `agentcells tick --ignore-pause`。超えている間は、1件動かすたびにまた止まります）",
                            agentcells::running::elapsed(near.until.saturating_sub(unix_now()))
                        );
                    }
                    if report.ai_failed {
                        session.halt("ai_failed");
                        // 使用量の上限なら戻る時刻まで、ほかの仕事も同じように失敗しそうなら（ログインの切れなど）
                        // しばらく、新しく始めない
                        let now = unix_now();
                        let until = report.limited_until.or_else(|| {
                            agentcells::limit::pauses_after(report.failure.as_deref())
                                .then_some(now + agentcells::limit::AFTER_FAILURE)
                        });
                        if let Some(until) = until {
                            let reason = if report.limited_until.is_some() {
                                "使用量の上限"
                            } else {
                                "AIの実行の失敗"
                            };
                            agentcells::limit::pause(&home, until, reason);
                            println!(
                                "{reason}のため、あと{}は新しく始めません",
                                agentcells::running::elapsed(until.saturating_sub(now))
                            );
                        }
                    }
                }
                Ok(config)
            })();
            note(record::Event::Ended {
                runs,
                why: why.to_string(),
                error: result.as_ref().err().map(|e| e.message.clone()),
            });
            let config = result?;
            // 1日に1回（設定した間隔ごとに）、最後にinspectorを動かす。AIの失敗や残ったプロセスで止まったときは動かさない
            if let Some(inspector) = &config.inspector
                && matches!(why, "no_work" | "limit")
                && inspect::due(&home, inspector, unix_now())
            {
                println!("\n--- inspector");
                match inspect_now(&home, &config, inspector, &note) {
                    Ok(summary) => print_inspection(&summary),
                    Err(e) => println!("注意: inspectorが動けませんでした: {}", e.message),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Inspect { dry_run, notes } => {
            if notes {
                let recent = agentcells::notes::recent(&home, 3);
                if recent.is_empty() {
                    println!("まだメモはありません（inspectorの2段目が動くと、ここに残ります）");
                }
                for (path, text) in recent.iter().rev() {
                    println!("--- {}\n{text}", path.display());
                }
                return Ok(ExitCode::SUCCESS);
            }
            if dry_run {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&inspect::preview(&home)).unwrap_or_default()
                );
                return Ok(ExitCode::SUCCESS);
            }
            let config = Config::load(&home.config_path())?;
            let Some(inspector) = &config.inspector else {
                return Err(Error::new("config.toml に [inspector] がありません")
                    .with_hint("雛形のコメント（`agentcells setup` で作ったもの）を見て、[inspector] を書いてください"));
            };
            // tickと重ならないように、同じロックを取る
            let Some(_lock) = tick::lock(&home)? else {
                println!("tickが動いているので、終わってからもう一度実行してください");
                return Ok(ExitCode::SUCCESS);
            };
            if let Some(note_text) = run::stuck(&home) {
                return Err(
                    Error::new(note_text).with_hint("`agentcells doctor` でも確かめられます")
                );
            }
            let recorder = record::Recorder::new(&home, unix_now());
            let note = |event: record::Event| recorder.write(unix_now(), event);
            let summary = inspect_now(&home, &config, inspector, &note)?;
            print_inspection(&summary);
            Ok(ExitCode::SUCCESS)
        }
        Commands::Prompt {
            command: PromptCommands::Show { cell, issue, pr },
        } => {
            let config = Config::load(&home.config_path())?;
            let target = match (&issue, &pr) {
                (Some(issue), _) => Some((Target::Issue, issue.as_str())),
                (_, Some(pr)) => Some((Target::PullRequest, pr.as_str())),
                _ => None,
            };
            print!("{}", prompt::show(&config, &SystemProbe, &cell, target)?);
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn run_cell(command: CellCommands) -> Result<ExitCode> {
    if let CellCommands::Hook = command {
        // 読めなくても断る答えを出せるように、今の仕事のファイルは読めたときだけ使う
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
            .map_err(|e| Error::new(format!("hookの入力を読めません: {e}")))?;
        let context = actions::Context::load_from_env().ok();
        if let Some(out) = agentcells::hook::pre_tool_use(&input, context.as_ref()) {
            println!("{out}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let context = actions::Context::load_from_env()?;
    let cwd = std::env::current_dir()
        .map_err(|e| Error::new(format!("今のディレクトリが分かりません: {e}")))?;
    let code = |code: i32| ExitCode::from(u8::try_from(code).unwrap_or(1));
    match command {
        CellCommands::Git { subcommand, args } => {
            Ok(code(actions::git_read(&context, &subcommand, &args)?))
        }
        CellCommands::Rm { paths } => {
            for removed in actions::remove(&context, &cwd, &paths)? {
                println!("消しました: {}", removed.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        CellCommands::Mv { from, to } => {
            actions::rename(&context, &cwd, &from, &to)?;
            println!("動かしました: {} → {}", from.display(), to.display());
            Ok(ExitCode::SUCCESS)
        }
        CellCommands::Commit { message } => {
            let sha = actions::commit(&context, &message)?;
            println!("コミットしました: {sha}");
            Ok(ExitCode::SUCCESS)
        }
        CellCommands::Run { name } => Ok(code(actions::run(&context, &name)?)),
        CellCommands::Fetch { url, from } => {
            Ok(code(agentcells::fetch::fetch(&context, &url, from)?))
        }
        CellCommands::Merge { abort: true } => {
            actions::merge_abort(&context)?;
            println!("マージをやめて、取り込む前に戻しました");
            Ok(ExitCode::SUCCESS)
        }
        CellCommands::Hook => unreachable!("hookは上で扱う"),
        CellCommands::Merge { abort: false } => match actions::merge(&context)? {
            actions::Merged::Clean => {
                println!("ベースのブランチを取り込みました（ぶつかりはありません）");
                Ok(ExitCode::SUCCESS)
            }
            actions::Merged::Conflicts(files) => {
                println!("ぶつかりました:");
                for file in files {
                    println!("  {}", agentcells::display::printable(&file));
                }
                println!(
                    "ぶつかったところを直してから `agentcells cell commit -m <メッセージ>` でマージのコミットを作ってください。やめるときは `agentcells cell merge --abort` です"
                );
                Ok(ExitCode::from(1))
            }
        },
    }
}

fn print_doctor(home: &Home) -> ExitCode {
    let path = std::env::var_os("PATH");
    let plist = schedule_plist();
    let checks = doctor::run(home, path.as_deref(), plist.as_deref(), &SystemProbe);
    for check in &checks {
        let mark = match check.status {
            Status::Ok => "✓",
            Status::Warn => "!",
            Status::Fail => "✗",
        };
        // 複数行の説明は、2行目以降を説明の列にそろえる。
        let detail = check.detail.replace('\n', &format!("\n  {:<12} ", ""));
        println!("{mark} {:<12} {detail}", check.name);
        if let Some(fix) = &check.fix {
            println!("  {:<12} 直し方: {fix}", "");
        }
    }
    if doctor::has_failure(&checks) {
        println!("\n✗ の項目を直してから、もう一度 `agentcells doctor` を実行してください");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// productごとに、扱っているIssue/PRの状態をGitHubから読む（`status` と `dashboard`）。
fn read_reports(home: &Home) -> Result<Vec<(String, status::Report)>> {
    let config = Config::load(&home.config_path())?;
    let credentials_for =
        |repo: &RepoName| github::credentials(&config, &SystemProbe, repo, Purpose::Read);
    let actors = guard::Actors {
        operator: config.operator.clone().unwrap_or_default(),
        writer: prompt::writer_login(&config, &SystemProbe)?,
    };
    Ok(config
        .products
        .iter()
        .map(|product| {
            let report = status::product(&config, product, &SystemProbe, &credentials_for, &actors);
            (product.name.clone(), report)
        })
        .collect())
}

/// tickが新しく始めない理由（`status` と `dashboard`）。
fn tick_notices(home: &Home, now: u64) -> Vec<String> {
    let mut notices = Vec::new();
    // 止めきれなかったプロセスがあれば、次のtickは動かない
    if let Some(note) = run::stuck(home) {
        notices.push(note);
    }
    if let Some(pause) = agentcells::limit::active(home, now) {
        notices.push(format!(
            "{}のため、あと{}は、tickが新しく始めません",
            pause.reason,
            agentcells::running::elapsed(pause.until.saturating_sub(now))
        ));
    }
    notices
}

/// 今の時刻（UNIXの秒）。
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// 定期実行のplistの場所。launchdのない環境では `None`。
fn schedule_plist() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| schedule::plist_path(Path::new(&home)))
}

/// inspectorを動かし、記録に残す。
fn inspect_now(
    home: &Home,
    config: &Config,
    inspector: &agentcells::config::Inspector,
    note: &dyn Fn(record::Event),
) -> Result<inspect::Summary> {
    let result = (|| -> Result<inspect::Summary> {
        let claude = find_program("claude").ok_or_else(|| {
            Error::new("claudeが見つかりません").with_hint("`agentcells doctor` で確かめてください")
        })?;
        let scanner = find_program("betterleaks").ok_or_else(|| {
            Error::new("betterleaksが見つかりません")
                .with_hint("`agentcells doctor` で確かめてください")
        })?;
        let writer = prompt::writer_login(config, &SystemProbe)?;
        let now = unix_now();
        let mut summary = inspect::run(&inspect::Context {
            home,
            config,
            inspector,
            probe: &SystemProbe,
            claude: &claude,
            scanner: &scanner,
            writer: &writer,
            now,
        })?;
        // 2段目: 実行の中身まで読み、1段目で見つけたこと以外を、手元に残す
        let reported = summary.fingerprints.clone();
        let reported: Vec<&str> = reported.iter().map(String::as_str).collect();
        let context = agentcells::notes::Context {
            home,
            config,
            inspector,
            probe: &SystemProbe,
            claude: &claude,
            scanner: &scanner,
            now,
        };
        match agentcells::notes::run(&context, &reported) {
            Ok(Some((count, path))) => {
                summary.observations = count;
                summary.notes_path = Some(path);
            }
            Ok(None) => {}
            Err(e) => {
                summary
                    .notes
                    .push(format!("2段目が動けませんでした: {}", e.message));
                summary.notes_error = Some(e.message);
            }
        }
        Ok(summary)
    })();
    note(match &result {
        Ok(s) => record::Event::Inspected {
            findings: s.findings,
            created: s.created.clone(),
            commented: s.commented.clone(),
            kept: s.kept,
            observations: s.observations,
            notes_error: s.notes_error.clone(),
            error: None,
        },
        Err(e) => record::Event::Inspected {
            findings: 0,
            created: Vec::new(),
            commented: Vec::new(),
            kept: 0,
            observations: 0,
            notes_error: None,
            error: Some(e.message.clone()),
        },
    });
    result
}

fn print_inspection(summary: &inspect::Summary) {
    if summary.findings == 0 {
        println!("1段目（記録の規則）: 気になることはありませんでした");
    } else {
        println!("1段目（記録の規則）: 気になること {}件", summary.findings);
    }
    for number in &summary.created {
        println!("  Issueを作りました: #{number}");
    }
    for number in &summary.commented {
        println!("  また起きたとコメントしました: #{number}");
    }
    if summary.kept > 0 {
        println!("  出さずに手元に残した案: {}件", summary.kept);
    }
    if let Some(report) = &summary.report {
        println!("  記録: {}", report.display());
    }
    if let Some(path) = &summary.notes_path {
        println!(
            "2段目（実行の中身）: 手元に残したメモ {}件（`agentcells inspect --notes` で見られます）",
            summary.observations
        );
        println!("  メモ: {}", path.display());
    }
    for note in &summary.notes {
        println!("注意: {note}");
    }
}

/// PATHから、プログラムの絶対パスを探す。AIに別のものを動かされないよう、sandboxの外で決めておく。
fn find_program(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
}
