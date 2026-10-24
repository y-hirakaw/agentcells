use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use agentcells::actions;
use agentcells::config::{Config, Target};
use agentcells::doctor::{self, Status};
use agentcells::error::{Error, Result};
use agentcells::files;
use agentcells::github::{self, Purpose};
use agentcells::home::{Home, RepoName};
use agentcells::labels;
use agentcells::probe::SystemProbe;
use agentcells::prompt;
use agentcells::setup;
use agentcells::tick;

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
        Commands::Tick { dry_run } => {
            let Some(_lock) = tick::lock(&home)? else {
                println!("ほかのtickが動いているので、何もしません");
                return Ok(ExitCode::SUCCESS);
            };
            let config = Config::load(&home.config_path())?;
            let credentials_for =
                |repo: &RepoName| github::credentials(&config, &SystemProbe, repo, Purpose::Read);
            let work = tick::find(&config, &SystemProbe, &credentials_for)?;
            if work.is_empty() {
                println!("仕事はありません");
                return Ok(ExitCode::SUCCESS);
            }
            println!("仕事: {}件", work.len());
            for w in &work {
                println!("  {}", w.describe());
            }
            println!("\n次に動かすもの: {}", work[0].describe());
            if !dry_run {
                return Err(
                    Error::new("cellを動かす部分は、まだ作っていません").with_hint(
                        "今は `agentcells tick --dry-run` で、見つかる仕事を確かめられます",
                    ),
                );
            }
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
    }
}

fn print_doctor(home: &Home) -> ExitCode {
    let path = std::env::var_os("PATH");
    let checks = doctor::run(home, path.as_deref(), &SystemProbe);
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
