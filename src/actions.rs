//! cellとして動くAIが使う `agentcells cell` のコマンド。
//!
//! AIに許すのは、読むことと、worktreeの中のファイルの作成・更新（Edit、Write）だけ。
//! それ以外の書き換え（削除、移動、コミット）と、gitを読むこと、ビルドやテストの実行は、このコマンドを通す。
//! CLIが、worktreeの外を触らないか、危ないオプションがないかを確かめてから行う。
//!
//! どのworktreeで、どのコマンドを使えるかは、`tick` が書いた「今の仕事」のファイルで決まる。
//! その場所は環境変数 `AGENTCELLS_CONTEXT` で渡す（AIは環境変数を変えて呼べない。
//! `Bash(agentcells cell:*)` は先頭が `agentcells cell` のコマンドだけを許すため）。

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const CONTEXT_ENV: &str = "AGENTCELLS_CONTEXT";

/// 今の仕事。`tick` が書き、`agentcells cell` が読む。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    /// 作業するworktree（絶対パス）
    pub worktree: PathBuf,
    /// `agentcells cell run <名前>` で実行できるコマンド
    #[serde(default)]
    pub commands: BTreeMap<String, String>,
    /// worktreeに置いた秘密のファイル（worktreeからの相対パス）。コミットさせない
    #[serde(default)]
    pub placed: Vec<PathBuf>,
}

impl Context {
    pub fn load_from_env() -> Result<Self> {
        let path = std::env::var_os(CONTEXT_ENV).ok_or_else(|| {
            Error::new("`agentcells cell` は、cellとして動くAIが使うコマンドです")
                .with_hint("agentcellsがAIを起動したときだけ使えます")
        })?;
        let text = std::fs::read_to_string(&path).map_err(|e| {
            Error::new(format!(
                "今の仕事のファイル {} を読めません: {e}",
                Path::new(&path).display()
            ))
        })?;
        serde_json::from_str(&text)
            .map_err(|e| Error::new(format!("今の仕事のファイルを読めません: {e}")))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| Error::new(format!("今の仕事を書けません: {e}")))?;
        std::fs::write(path, text)
            .map_err(|e| Error::new(format!("{} に書けません: {e}", path.display())))
    }

    /// `path`（`cwd` からの相対パスか絶対パス）を、worktreeの中のパスとして解く。
    /// worktreeの外や、`.git` の中や、worktreeそのものは断る。
    /// シンボリックリンクはたどらない（親ディレクトリだけを解く）。リンク先を消したり動かしたりさせないため。
    pub fn inside(&self, cwd: &Path, path: &Path) -> Result<PathBuf> {
        let joined = normalize(&cwd.join(path));
        let worktree = self.worktree.canonicalize().map_err(|e| {
            Error::new(format!(
                "worktree {} がありません: {e}",
                self.worktree.display()
            ))
        })?;
        let parent = joined
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .ok_or_else(|| {
                Error::new(format!("{} の親ディレクトリがありません", path.display()))
            })?;
        let name = joined
            .file_name()
            .ok_or_else(|| Error::new(format!("{} はファイルを指していません", path.display())))?;
        let resolved = parent.join(name);
        let relative = resolved.strip_prefix(&worktree).map_err(|_| {
            Error::new(format!("{} はworktreeの外です", path.display()))
                .with_hint("worktreeの中のファイルだけを扱えます")
        })?;
        if relative.as_os_str().is_empty() {
            return Err(Error::new("worktreeそのものは扱えません"));
        }
        if relative.components().next() == Some(Component::Normal(".git".as_ref())) {
            return Err(Error::new(format!("{} は .git の中です", path.display())));
        }
        Ok(resolved)
    }
}

impl Context {
    /// あるディレクトリが、worktreeそのものか、その中（`.git` を除く）かを確かめる。
    fn inside_dir(&self, dir: &Path) -> Result<()> {
        let worktree = self
            .worktree
            .canonicalize()
            .map_err(|e| Error::new(format!("worktreeがありません: {e}")))?;
        let dir = dir
            .canonicalize()
            .map_err(|e| Error::new(format!("{} がありません: {e}", dir.display())))?;
        let relative = dir.strip_prefix(&worktree).map_err(|_| {
            Error::new(format!("{} はworktreeの外です", dir.display()))
                .with_hint("worktreeの中のファイルだけを扱えます")
        })?;
        if relative.components().next() == Some(Component::Normal(".git".as_ref())) {
            return Err(Error::new(format!("{} は .git の中です", dir.display())));
        }
        Ok(())
    }
}

/// `..` と `.` を、ファイルシステムを見ずに解く。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// ファイルかディレクトリを消す。
pub fn remove(context: &Context, cwd: &Path, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    // 先に全部確かめてから消す（途中で止まって、半分だけ消えないように）
    let targets: Vec<PathBuf> = paths
        .iter()
        .map(|p| context.inside(cwd, p))
        .collect::<Result<_>>()?;
    for target in &targets {
        let meta = std::fs::symlink_metadata(target)
            .map_err(|e| Error::new(format!("{} がありません: {e}", target.display())))?;
        let result = if meta.is_dir() {
            std::fs::remove_dir_all(target)
        } else {
            std::fs::remove_file(target)
        };
        result.map_err(|e| Error::new(format!("{} を消せません: {e}", target.display())))?;
    }
    Ok(targets)
}

/// ファイルかディレクトリを動かす。移す先のディレクトリがなければ作る。
pub fn rename(context: &Context, cwd: &Path, from: &Path, to: &Path) -> Result<()> {
    let from = context.inside(cwd, from)?;
    // 移す先の親がまだないときは、いちばん近くにあるディレクトリがworktreeの中かを確かめてから作る
    let to_joined = normalize(&cwd.join(to));
    if let Some(parent) = to_joined.parent()
        && !parent.exists()
    {
        let mut existing = parent;
        while !existing.exists() {
            existing = existing
                .parent()
                .ok_or_else(|| Error::new(format!("{} を作れません", parent.display())))?;
        }
        context.inside_dir(existing)?;
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::new(format!("{} を作れません: {e}", parent.display())))?;
    }
    let to = context.inside(cwd, to)?;
    if to.exists() {
        return Err(Error::new(format!("{} はすでにあります", to.display())));
    }
    std::fs::rename(&from, &to).map_err(|e| {
        Error::new(format!(
            "{} を {} に動かせません: {e}",
            from.display(),
            to.display()
        ))
    })
}

/// 読むためのgitのサブコマンド。
const GIT_READ: &[&str] = &["status", "diff", "log", "show", "blame"];

/// 読むためのgitの引数を作る。ファイルに書けるオプションや、外のコマンドを動かすオプションは断る。
pub fn git_read_args(subcommand: &str, args: &[String]) -> Result<Vec<String>> {
    if !GIT_READ.contains(&subcommand) {
        return Err(Error::new(format!("git {subcommand} は使えません"))
            .with_hint(format!("使えるのは git {} です", GIT_READ.join(" / git "))));
    }
    for arg in args {
        let lower = arg.to_ascii_lowercase();
        if lower.starts_with("--output") || lower.starts_with("--ext-diff") || lower == "-o" {
            return Err(Error::new(format!("{arg} は使えません")).with_hint(
                "ファイルに書くオプションや、外のコマンドを動かすオプションは使えません",
            ));
        }
    }
    let mut out: Vec<String> = [
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.pager=cat",
        "--no-pager",
        subcommand,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if matches!(subcommand, "diff" | "log" | "show") {
        out.extend(["--no-ext-diff".to_string(), "--no-textconv".to_string()]);
    }
    out.extend(args.iter().cloned());
    Ok(out)
}

/// gitを、worktreeで動かす。標準出力と標準エラーはそのまま流す。
fn git_in(worktree: &Path, args: &[String]) -> Result<std::process::ExitStatus> {
    Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .env("GIT_PAGER", "cat")
        .status()
        .map_err(|e| Error::new(format!("gitを動かせません: {e}")))
}

pub fn git_read(context: &Context, subcommand: &str, args: &[String]) -> Result<i32> {
    let args = git_read_args(subcommand, args)?;
    Ok(git_in(&context.worktree, &args)?.code().unwrap_or(1))
}

fn git_output(worktree: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()
        .map_err(|e| Error::new(format!("gitを動かせません: {e}")))?;
    if !out.status.success() {
        return Err(Error::new(format!(
            "git {} が失敗しました: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// worktreeの変更をすべてコミットする。置いた秘密のファイルが入っていたら、コミットせずに断る。
/// hooksは動かさない（AIが置いたhookを動かさないため）。
pub fn commit(context: &Context, message: &str) -> Result<String> {
    if message.trim().is_empty() {
        return Err(Error::new("コミットのメッセージが空です").with_hint("-m で書いてください"));
    }
    let wt = &context.worktree;
    git_output(wt, &["add", "-A"])?;
    let staged = git_output(wt, &["diff", "--cached", "--name-only", "-z"])?;
    let staged: Vec<&str> = staged.split('\0').filter(|s| !s.is_empty()).collect();
    if staged.is_empty() {
        return Err(Error::new("コミットする変更がありません"));
    }
    let secrets: Vec<&str> = staged
        .iter()
        .copied()
        .filter(|s| context.placed.iter().any(|p| p == Path::new(s)))
        .collect();
    if !secrets.is_empty() {
        // 入れてしまったものを外してから断る
        let mut args = vec!["reset", "-q", "--"];
        args.extend(secrets.iter().copied());
        git_output(wt, &args)?;
        return Err(Error::new(format!(
            "置いた秘密のファイルはコミットできません: {}",
            secrets.join(", ")
        )));
    }
    git_output(
        wt,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-q",
            "-m",
            message,
        ],
    )?;
    Ok(git_output(wt, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string())
}

/// 設定に書いたコマンドを、worktreeで実行する。標準出力と標準エラーはそのまま流し、終了コードを返す。
pub fn run(context: &Context, name: &str) -> Result<i32> {
    let command = context.commands.get(name).ok_or_else(|| {
        let names: Vec<&str> = context.commands.keys().map(String::as_str).collect();
        Error::new(format!("コマンド「{name}」はありません")).with_hint(if names.is_empty() {
            "このproductには、実行できるコマンドがありません。必要なら、人間に設定を頼んでください（needs-human）"
                .to_string()
        } else {
            format!("使えるのは {} です", names.join(", "))
        })
    })?;
    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&context.worktree)
        .status()
        .map_err(|e| Error::new(format!("「{command}」を実行できません: {e}")))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// worktreeとして使うリポジトリと、その外のファイルを作る。
    fn setup() -> (tempfile::TempDir, Context) {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "work"]);
        let work = dir.path().join("work").canonicalize().unwrap();
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["config", "user.email", "t@example.com"]);
        std::fs::write(work.join(".gitignore"), "local.properties\n").unwrap();
        std::fs::write(work.join("a.txt"), "a").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "base"]);
        std::fs::write(dir.path().join("outside.txt"), "keep").unwrap();
        let context = Context {
            worktree: work,
            commands: [(
                "hello".to_string(),
                "echo hi > made.txt; exit 3".to_string(),
            )]
            .into_iter()
            .collect(),
            placed: vec![PathBuf::from("local.properties")],
        };
        (dir, context)
    }

    #[test]
    fn inside_rejects_paths_outside_the_worktree() {
        let (dir, ctx) = setup();
        let wt = ctx.worktree.clone();
        assert_eq!(
            ctx.inside(&wt, Path::new("a.txt")).unwrap(),
            wt.join("a.txt")
        );
        assert_eq!(
            ctx.inside(&wt.join("sub"), Path::new("../a.txt")).unwrap(),
            wt.join("a.txt")
        );
        for bad in ["../outside.txt", "/etc/hosts", ".git/config", ".git", "."] {
            assert!(ctx.inside(&wt, Path::new(bad)).is_err(), "{bad}");
        }
        // worktreeの外を指すシンボリックリンクの中も、外として断る
        std::os::unix::fs::symlink(dir.path(), wt.join("link")).unwrap();
        assert!(ctx.inside(&wt, Path::new("link/outside.txt")).is_err());
    }

    #[test]
    fn remove_deletes_only_inside_and_not_link_targets() {
        let (dir, ctx) = setup();
        let wt = ctx.worktree.clone();
        std::os::unix::fs::symlink(dir.path().join("outside.txt"), wt.join("link")).unwrap();
        std::fs::create_dir(wt.join("sub")).unwrap();
        std::fs::write(wt.join("sub/b.txt"), "b").unwrap();

        remove(&ctx, &wt, &[PathBuf::from("link"), PathBuf::from("sub")]).unwrap();
        assert!(!wt.join("link").exists());
        assert!(!wt.join("sub").exists());
        // リンク先は消えない
        assert_eq!(
            std::fs::read_to_string(dir.path().join("outside.txt")).unwrap(),
            "keep"
        );
        // 1つでも外を指していたら、何も消さない
        assert!(
            remove(
                &ctx,
                &wt,
                &[PathBuf::from("a.txt"), PathBuf::from("../outside.txt")]
            )
            .is_err()
        );
        assert!(wt.join("a.txt").exists());
    }

    #[test]
    fn rename_moves_inside_and_creates_parents() {
        let (_dir, ctx) = setup();
        let wt = ctx.worktree.clone();
        rename(&ctx, &wt, Path::new("a.txt"), Path::new("docs/deep/a.md")).unwrap();
        assert!(wt.join("docs/deep/a.md").exists());
        rename(
            &ctx,
            &wt,
            Path::new("docs/deep/a.md"),
            Path::new("docs/a.md"),
        )
        .unwrap();
        assert!(rename(&ctx, &wt, Path::new("docs/a.md"), Path::new("../moved.md")).is_err());
        assert!(rename(&ctx, &wt, Path::new("docs/a.md"), Path::new("../new/x.md")).is_err());
        assert!(!wt.parent().unwrap().join("new").exists());
    }

    #[test]
    fn git_read_rejects_writing_options() {
        let args = git_read_args("diff", &["origin/main...HEAD".to_string()]).unwrap();
        assert!(args.contains(&"--no-ext-diff".to_string()));
        assert_eq!(args.last().unwrap(), "origin/main...HEAD");
        for bad in ["--output=/tmp/x", "--OUTPUT", "--ext-diff", "-o"] {
            assert!(git_read_args("diff", &[bad.to_string()]).is_err(), "{bad}");
        }
        assert!(git_read_args("push", &[]).is_err());
        assert!(git_read_args("config", &[]).is_err());
    }

    #[test]
    fn commit_commits_changes_but_not_placed_secrets() {
        let (_dir, mut ctx) = setup();
        let wt = ctx.worktree.clone();
        std::fs::write(wt.join("b.txt"), "b").unwrap();
        let sha = commit(&ctx, "Add b").unwrap();
        assert!(!sha.is_empty());
        assert_eq!(git(&wt, &["log", "-1", "--format=%s"]), "Add b");
        assert!(commit(&ctx, "nothing").is_err());

        // gitignoreされていない場所に置いた秘密は、コミットせずに外して断る
        std::fs::write(wt.join("secret.env"), "TOKEN=x").unwrap();
        ctx.placed.push(PathBuf::from("secret.env"));
        let err = commit(&ctx, "leak").unwrap_err();
        assert!(err.message.contains("secret.env"), "{}", err.message);
        assert_eq!(git(&wt, &["diff", "--cached", "--name-only"]), "");
        assert_eq!(git(&wt, &["log", "-1", "--format=%s"]), "Add b");
    }

    #[test]
    fn commit_does_not_run_hooks() {
        let (dir, ctx) = setup();
        let wt = ctx.worktree.clone();
        let hooks = wt.join(".git/hooks");
        let marker = dir.path().join("hook-ran");
        std::fs::write(
            hooks.join("pre-commit"),
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                hooks.join("pre-commit"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        std::fs::write(wt.join("c.txt"), "c").unwrap();
        commit(&ctx, "Add c").unwrap();
        assert!(!marker.exists());
    }

    #[test]
    fn run_executes_only_configured_commands_in_the_worktree() {
        let (_dir, ctx) = setup();
        assert_eq!(run(&ctx, "hello").unwrap(), 3);
        assert!(ctx.worktree.join("made.txt").exists());
        let err = run(&ctx, "rm-everything").unwrap_err();
        assert!(err.hint.unwrap().contains("hello"));
    }

    #[test]
    fn context_round_trips_through_a_file() {
        let (dir, ctx) = setup();
        let path = dir.path().join("context.json");
        ctx.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(serde_json::from_str::<Context>(&text).unwrap(), ctx);
    }
}
