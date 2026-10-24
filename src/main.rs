use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use agentcells::config::{Config, Target};
use agentcells::doctor::{self, Status};
use agentcells::error::{Error, Result};
use agentcells::files;
use agentcells::home::{Home, RepoName};
use agentcells::labels;
use agentcells::probe::SystemProbe;
use agentcells::prompt;
use agentcells::setup;

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
