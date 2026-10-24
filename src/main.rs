use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use agentcells::doctor::{self, Status};
use agentcells::error::{Error, Result};
use agentcells::files;
use agentcells::home::{Home, RepoName};
use agentcells::probe::SystemProbe;
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
        println!("{mark} {:<12} {}", check.name, check.detail);
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
