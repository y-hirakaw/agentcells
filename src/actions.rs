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
use crate::github::ISOLATED_GIT_ENV;

pub const CONTEXT_ENV: &str = "AGENTCELLS_CONTEXT";

/// 今の仕事。`tick` が書き、`agentcells cell` が読む。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    /// 作業するworktree（絶対パス）
    pub worktree: PathBuf,
    /// `agentcells cell run <名前>` で実行できるコマンド
    #[serde(default)]
    pub commands: BTreeMap<String, String>,
    /// worktreeに置いた秘密のファイル（worktreeからの相対パス）。消したり動かしたりさせない
    #[serde(default)]
    pub placed: Vec<PathBuf>,
    /// 置いた秘密のファイルの中身（gitのblobのハッシュ）。名前を変えたり写したりしても、コミットさせない。
    /// `placed` と同じ数だけ要る（書き忘れると守りが黙って弱くなるので、`commit` が断る）
    #[serde(default)]
    pub placed_blobs: Vec<String>,
    /// コミットの作者。使う人のgitの設定は読まないので、ここで渡す
    pub identity: Identity,
    /// 秘密を探すbetterleaks（絶対パス）。`tick` がsandboxの外で探して渡す。
    /// AIは環境変数を変えて呼べないが、PATHで探すと別のものを動かされうるので、場所を決めておく
    pub scanner: PathBuf,
    /// ビルドやテストの一時ディレクトリ（仕事ごと）。Claude Codeがsandboxの中で使わせる `/tmp/claude-<uid>` は、
    /// 使う人のほかのセッションと共有なので、`run` のコマンドにはこちらを `TMPDIR` で渡す
    pub tmp: PathBuf,
    /// ビルドツールのキャッシュ（`gradle` など）と、その場所（productごと、agentcellsが持つ）
    #[serde(default)]
    pub caches: BTreeMap<String, PathBuf>,
    /// ベースのブランチ（PRのベース。Issueなら既定のブランチ）。`merge` が `origin/<base>` を取り込む
    #[serde(default)]
    pub base: Option<String>,
    /// `fetch` で開いてよいホスト（productの `[product.sandbox]` の `network`）
    #[serde(default)]
    pub network: Vec<String>,
    /// cellの `commands` で、使えるコマンドを絞っているか（使えないときに、どこで絞っているかを伝えるため）
    #[serde(default)]
    pub commands_limited: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub email: String,
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
        if is_git_entry(&worktree, relative) {
            return Err(Error::new(format!("{} は .git の中です", path.display())));
        }
        Ok(resolved)
    }

    /// 置いた秘密のファイルか、それを含むディレクトリなら断る。
    fn not_placed(&self, resolved: &Path) -> Result<()> {
        let worktree = self
            .worktree
            .canonicalize()
            .map_err(|e| Error::new(format!("worktreeがありません: {e}")))?;
        let Ok(relative) = resolved.strip_prefix(&worktree) else {
            return Ok(());
        };
        // 大文字・小文字を区別しないファイルシステムでは LOCAL.properties も同じファイルを指すので、
        // 名前を大文字・小文字を問わず比べ、さらに同じファイルかも確かめる
        let lower = relative.to_string_lossy().to_lowercase();
        if let Some(placed) = self.placed.iter().find(|p| {
            let placed_lower = p.to_string_lossy().to_lowercase();
            placed_lower == lower
                || placed_lower.starts_with(&format!("{lower}/"))
                || same_file(&worktree.join(p), resolved)
                || p.ancestors()
                    .skip(1)
                    .any(|a| !a.as_os_str().is_empty() && same_file(&worktree.join(a), resolved))
        }) {
            return Err(Error::new(format!(
                "{} は置いた秘密のファイル（{}）なので、消したり動かしたりできません",
                relative.display(),
                placed.display()
            )));
        }
        Ok(())
    }
}

/// worktreeからの相対パスの先頭が `.git` か。大文字・小文字を区別しないファイルシステム（macOSの標準）では
/// `.GIT` も同じものを指すので、名前を大文字・小文字を問わず比べ、さらに同じファイルかも確かめる。
fn is_git_entry(worktree: &Path, relative: &Path) -> bool {
    let Some(Component::Normal(first)) = relative.components().next() else {
        return false;
    };
    if first.to_string_lossy().eq_ignore_ascii_case(".git") {
        return true;
    }
    same_file(&worktree.join(first), &worktree.join(".git"))
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(_a: &Path, _b: &Path) -> bool {
    false
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
        if is_git_entry(&worktree, relative) {
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
        context.not_placed(target)?;
    }
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
    context.not_placed(&from)?;
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
    context.not_placed(&to)?;
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
        let mut hint = format!("使えるのは git {} です", GIT_READ.join(" / git "));
        // 代わりのツールがあるものは、それを案内する（git grep は外のコマンドを動かす -O があるので許さない）
        match subcommand {
            "grep" => hint.push_str("。ファイルの中を探すには Grep ツールを使ってください"),
            "ls-files" => hint.push_str("。ファイルを探すには Glob ツールを使ってください"),
            _ => {}
        }
        return Err(Error::new(format!("git {subcommand} は使えません")).with_hint(hint));
    }
    for arg in args {
        let lower = arg.to_ascii_lowercase();
        // --no-index はworktreeの外のファイルを読めるので断る
        if lower.starts_with("--output")
            || lower.starts_with("--ext-diff")
            || lower.starts_with("--no-index")
            || lower == "-o"
        {
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
        .envs(ISOLATED_GIT_ENV.iter().copied())
        .status()
        .map_err(|e| Error::new(format!("gitを動かせません: {e}")))
}

pub fn git_read(context: &Context, subcommand: &str, args: &[String]) -> Result<i32> {
    let args = git_read_args(subcommand, args)?;
    Ok(git_in(&context.worktree, &args)?.code().unwrap_or(1))
}

/// 書き換えのためのgit。使う人とシステムのgitの設定は読まず、fsmonitorとhooksは動かさない。
fn git_output(worktree: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .envs(ISOLATED_GIT_ENV.iter().copied())
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

/// `merge` の結果。
#[derive(Debug, PartialEq, Eq)]
pub enum Merged {
    /// ぶつからずに取り込み、マージのコミットを作った（取り込むものがなかったときも）
    Clean,
    /// ぶつかった。直してから `commit` でマージのコミットを作る
    Conflicts(Vec<String>),
}

/// ベースのブランチ（`origin/<base>`）を取り込む。ぶつかりを直すための、ただ1つのマージ。
/// push の前の確かめ（[`crate::pr::verify`]）は、ベースのブランチ以外を取り込んだマージを断る。
pub fn merge(context: &Context) -> Result<Merged> {
    let base = context.base.as_deref().ok_or_else(|| {
        Error::new("ベースのブランチが分かりません")
            .with_hint("人間に回してください（needs-human）")
    })?;
    let wt = &context.worktree;
    let upstream = format!("origin/{base}");
    let name = format!("user.name={}", context.identity.name);
    let email = format!("user.email={}", context.identity.email);
    let args = [
        "-c",
        name.as_str(),
        "-c",
        email.as_str(),
        "merge",
        "--no-edit",
        "--no-stat",
        "-q",
        upstream.as_str(),
    ];
    match git_output(wt, &args) {
        Ok(_) => Ok(Merged::Clean),
        Err(e) => {
            let conflicts: Vec<String> =
                git_output(wt, &["diff", "-z", "--name-only", "--diff-filter=U"])?
                    .split('\0')
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
            // ぶつかった以外の理由（コミットしていない変更があるなど）なら、そのまま返す
            if conflicts.is_empty() {
                return Err(e);
            }
            Ok(Merged::Conflicts(conflicts))
        }
    }
}

/// マージの途中なら、自動でマージした結果の木と、ぶつかったファイル。マージの途中でなければ `None`。
///
/// マージのコミットを調べるときは、1つ目の親（`HEAD`）ではなく、この木と比べる。`HEAD` と比べると、
/// ベースのブランチで人が足した行まで「足した行」になり、それで断ってしまうため（pushの前の確かめ
/// [`crate::pr::verify`] の `--remerge-diff` と同じ範囲にする）。
fn auto_merge(worktree: &Path) -> Result<Option<(String, Vec<String>)>> {
    if git_output(worktree, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).is_err() {
        return Ok(None);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args([
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            "--no-messages",
            "HEAD",
            "MERGE_HEAD",
        ])
        .envs(ISOLATED_GIT_ENV.iter().copied())
        .output()
        .map_err(|e| Error::new(format!("gitを動かせません: {e}")))?;
    // ぶつかりがなければ0、あれば1で終わる。木のあとに、ぶつかったファイルが並ぶ（どれもNULで区切る。
    // 区切らないと、日本語などの名前が引用符と8進数になり、その名前ではファイルを読めない）
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(Error::new(format!(
            "自動のマージを作れません: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.split('\0').filter(|l| !l.is_empty());
    let tree = lines
        .next()
        .ok_or_else(|| Error::new("自動のマージを作れません"))?
        .to_string();
    Ok(Some((tree, lines.map(str::to_string).collect())))
}

/// ぶつかったファイルに、ぶつかりの印（`<<<<<<<`、`>>>>>>>`）が残っていないかを見る。残したまま
/// コミットすると、壊れたファイルがマージのコミットに入るため。ほかのファイルは見ない（ベースの
/// ブランチの文書やテストのデータに、同じ形の行がありうるため）。
fn unresolved(worktree: &Path, conflicted: &[String]) -> Result<()> {
    let mut left = Vec::new();
    for path in conflicted {
        let Ok(text) = git_output(worktree, &["show", &format!(":{path}")]) else {
            // 消して直したファイルは、ステージにない。あるのに読めなければ、調べずに通さない
            let staged = git_output(
                worktree,
                &["--literal-pathspecs", "ls-files", "-z", "--", path],
            )?;
            if staged.is_empty() {
                continue;
            }
            return Err(Error::new(format!(
                "ぶつかったファイル {path} を読めないので、コミットしませんでした"
            ))
            .with_hint("人間に回してください（needs-human）"));
        };
        for (i, line) in text.lines().enumerate() {
            if line.starts_with("<<<<<<< ") || line.starts_with(">>>>>>> ") {
                left.push(format!("  - {path}:{}", i + 1));
            }
        }
    }
    if left.is_empty() {
        return Ok(());
    }
    Err(Error::new(format!(
        "ぶつかりの印が残っているので、コミットしませんでした:\n{}",
        left.join("\n")
    ))
    .with_hint("ぶつかったところを直してから、もう一度コミットしてください"))
}

/// 人が開いたときに自動で動くファイル（[`crate::pr::autorun`]）の変更、シンボリックリンク、submoduleを、
/// コミットの時点で断る。pushの前の確かめ（[`crate::pr::verify`]）でも断るが、そこで断られると、その実行の
/// 作業はまるごと人間に回る。ここで断れば、AIがその変更を外して続けられる。
/// `against` はマージの途中なら自動でマージした結果の木で、それと比べる（ベースのブランチから来た変更では断らない）。
fn guarded_changes(worktree: &Path, against: Option<&str>) -> Result<()> {
    let raw = git_output(
        worktree,
        &[
            "diff",
            "--cached",
            "--raw",
            "--no-renames",
            "-z",
            against.unwrap_or("HEAD"),
        ],
    )?;
    // ":<古いmode> <新しいmode> <古いhash> <新しいhash> <状態>\0<パス>\0" が並ぶ
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    let mut refused = Vec::new();
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        let new_mode = meta.split_whitespace().nth(1);
        let why = if let Some(guarded) = crate::pr::autorun(path) {
            format!("人が開いたときに自動で動くファイル（{guarded}）")
        } else if new_mode == Some("120000") {
            "シンボリックリンク".to_string()
        } else if new_mode == Some("160000") {
            "submodule".to_string()
        } else {
            continue;
        };
        refused.push(format!("  - {path}: {why}"));
    }
    if refused.is_empty() {
        return Ok(());
    }
    Err(Error::new(format!(
        "次の変更は、agentcellsではコミットできません:\n{}",
        refused.join("\n")
    ))
    .with_hint(
        "その変更を元に戻してから（足したものは `agentcells cell rm`、変えたものは Edit で元の中身に）、もう一度コミットしてください。作業にどうしても要るなら、ほかの変更をコミットしてから needs-human にし、何をどう変えてほしいかを comment に書いてください（人が中身を確かめて入れます）",
    ))
}

/// 途中のマージをやめて、取り込む前に戻す。
pub fn merge_abort(context: &Context) -> Result<()> {
    git_output(&context.worktree, &["merge", "--abort"]).map(|_| ())
}

/// worktreeの変更をすべてコミットする。置いた秘密のファイルの中身が入っていたら、コミットせずに断る。
/// 名前で比べると、名前を変えたり写したりしたものを見逃すので、中身（blobのハッシュ）で比べる。
pub fn commit(context: &Context, message: &str) -> Result<String> {
    if message.trim().is_empty() {
        return Err(Error::new("コミットのメッセージが空です").with_hint("-m で書いてください"));
    }
    if context.placed_blobs.len() != context.placed.len() {
        return Err(Error::new(
            "今の仕事のファイルに、置いた秘密のファイルの中身のハッシュがそろっていません",
        )
        .with_hint("agentcellsの不具合です。人間に回してください（needs-human）"));
    }
    let wt = &context.worktree;
    git_output(wt, &["add", "-A"])?;
    let staged = staged_blobs(wt)?;
    if staged.is_empty() {
        return Err(Error::new("コミットする変更がありません"));
    }
    let leaked: Vec<&str> = staged
        .iter()
        .filter(|(blob, path)| {
            context.placed_blobs.contains(blob)
                || context.placed.iter().any(|p| p == Path::new(path))
        })
        .map(|(_, path)| path.as_str())
        .collect();
    if !leaked.is_empty() {
        // 入れてしまったものを外してから断る
        let mut args = vec!["reset", "-q", "--"];
        args.extend(leaked.iter().copied());
        git_output(wt, &args)?;
        return Err(Error::new(format!(
            "置いた秘密のファイルと同じ中身のファイルは、コミットできません: {}",
            leaked.join(", ")
        )));
    }
    // マージの途中なら、自動でマージした結果と比べる（ぶつかりの直しと、足した変更だけを見る）
    let merging = auto_merge(wt)?;
    if let Some((_, conflicted)) = &merging {
        unresolved(wt, conflicted)?;
    }
    guarded_changes(wt, merging.as_ref().map(|(tree, _)| tree.as_str()))?;
    scan_secrets(context, merging.as_ref().map(|(tree, _)| tree.as_str()))?;
    let mut args: Vec<String> = vec![
        "-c".to_string(),
        format!("user.name={}", context.identity.name),
        "-c".to_string(),
        format!("user.email={}", context.identity.email),
    ];
    args.extend(
        ["commit", "-q", "-m", message]
            .iter()
            .map(|s| s.to_string()),
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git_output(wt, &args)?;
    Ok(git_output(wt, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string())
}

/// betterleaksの設定を選ぶ環境変数。worktreeの設定と同じく、AIに書き換えられうるので使わせない。
const SCANNER_CONFIG_ENV: &[&str] = &[
    "BETTERLEAKS_CONFIG",
    "GITLEAKS_CONFIG",
    "BETTERLEAKS_CONFIG_TOML",
    "GITLEAKS_CONFIG_TOML",
];

#[derive(Deserialize)]
struct Finding {
    #[serde(rename = "RuleID")]
    rule: String,
    #[serde(rename = "StartLine")]
    line: usize,
}

/// コミットに足す行を、betterleaksで調べる。秘密らしいものがあれば断る（入れたものはステージに残す）。
///
/// betterleaksは、調べる場所の `.betterleaks.toml` や `.betterleaksignore`、行の `betterleaks:allow` で
/// 見逃すものを決められる。どれもAIが書けるので、既定の決まりだけで調べる:
/// - 足した行だけを標準入力で渡し、AIが書けない `/` で動かす（設定と無視のファイルを読ませない）
/// - 設定を選ぶ環境変数を消し、`betterleaks:allow` を無視させる
///
/// `against` は比べる相手（マージの途中なら、自動でマージした結果の木）。なければ `HEAD`。
fn scan_secrets(context: &Context, against: Option<&str>) -> Result<()> {
    let diff = git_output(
        &context.worktree,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--cached",
            against.unwrap_or("HEAD"),
            "--no-color",
            "--no-ext-diff",
            // AIが書ける .git/config で、見出しの形を変えさせない（変わっても調べる中身は同じで、出す名前だけがずれる）
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--no-textconv",
            "--text",
            "--no-renames",
            "--unified=0",
        ],
    )?;
    let lines = added_lines(&diff);
    if lines.is_empty() {
        return Ok(());
    }
    let input: String = lines.iter().map(|l| format!("{}\n", l.text)).collect();
    let hint = "コミットできません。人間に回してください（needs-human）";
    let found = scan_text(&context.scanner, &input).map_err(|e| e.with_hint(hint))?;
    if found.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = found
        .iter()
        .map(|(line, rule)| match lines.get(line.wrapping_sub(1)) {
            Some(l) => format!("  - {}:{}（{rule}）", l.path, l.line),
            None => format!("  - 場所不明（{rule}）"),
        })
        .collect();
    Err(Error::new(format!(
        "コミットに秘密らしいものが入っているので、コミットしませんでした:\n{}",
        list.join("\n")
    ))
    .with_hint(
        "秘密を消してから、もう一度コミットしてください。秘密でないなら、人間に回してください（needs-human）",
    ))
}

/// 文章を、betterleaksの既定の決まりだけで調べる。見つかったものの行（1から）と種類を返す。
/// コミットに足す行のほか、AIが書いてGitHubに投稿する文章にも使う。
///
/// betterleaksは、調べる場所の設定や無視のファイル、行の `betterleaks:allow` で見逃すものを決められる。
/// どれもAIが書けるので、AIが書けない `/` で動かし、設定を選ぶ環境変数を消し、allowのコメントを無視させる。
/// 動かないときや、結果が読めないときはエラーにする（調べられなかったものを通さない）。
pub fn scan_text(scanner: &Path, text: &str) -> Result<Vec<(usize, String)>> {
    let mut command = Command::new(scanner);
    command
        .args([
            "stdin",
            "--no-banner",
            "--no-color",
            "--redact",
            "--ignore-gitleaks-allow",
            // 標準のre2は、ホームの下（~/Library/Caches）にキャッシュを作ろうとし、sandboxの中では落ちる
            "--regex-engine",
            "stdlib",
            "--gitleaks-ignore-path",
            "/",
            "--report-format",
            "json",
            "--report-path",
            "-",
            "--exit-code",
            "1",
        ])
        .current_dir("/")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for key in SCANNER_CONFIG_ENV {
        command.env_remove(key);
    }
    let unavailable = |e: std::io::Error| {
        Error::new(format!(
            "秘密を探すbetterleaks（{}）を動かせません: {e}",
            scanner.display()
        ))
    };
    let mut child = command.spawn().map_err(unavailable)?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = text.to_string();
    // 書き終わる前に読み始めないと、出力が多いときに互いを待って止まる
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        stdin.write_all(input.as_bytes())
    });
    let out = child.wait_with_output().map_err(unavailable)?;
    // betterleaksが入力を読み切らずに終わることがあるので、書けなかったことは問わない
    let _ = writer.join();
    match out.status.code() {
        Some(0) => Ok(Vec::new()),
        Some(1) => {
            let findings: Vec<Finding> = serde_json::from_slice(&out.stdout)
                .map_err(|e| Error::new(format!("betterleaksの結果を読めません: {e}")))?;
            Ok(findings.into_iter().map(|f| (f.line, f.rule)).collect())
        }
        _ => Err(Error::new(format!(
            "betterleaksが失敗しました: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
    }
}

/// 足した1行。
pub(crate) struct AddedLine {
    pub(crate) path: String,
    /// 新しいファイルでの行番号
    pub(crate) line: usize,
    pub(crate) text: String,
}

/// `git diff --unified=0` から、足した行を取り出す。
pub(crate) fn added_lines(diff: &str) -> Vec<AddedLine> {
    let mut lines = Vec::new();
    let mut path = String::new();
    let mut next = 0;
    // ファイルの見出し（`diff --git` から最初の `@@` まで）の中だけで `+++ ` を名前として読む。
    // 足した行が `++ ` で始まると `+++ ` に見えるので、見出しの外では行として扱う
    let mut in_header = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_header = true;
        } else if in_header && let Some(name) = line.strip_prefix("+++ ") {
            path = name.strip_prefix("b/").unwrap_or(name).to_string();
        } else if let Some(hunk) = line.strip_prefix("@@ ") {
            in_header = false;
            // "@@ -a,b +c,d @@"
            next = hunk
                .split_whitespace()
                .find_map(|part| part.strip_prefix('+'))
                .and_then(|range| range.split(',').next())
                .and_then(|start| start.parse().ok())
                .unwrap_or(0);
        } else if !in_header && let Some(text) = line.strip_prefix('+') {
            lines.push(AddedLine {
                path: path.clone(),
                line: next,
                text: text.to_string(),
            });
            next += 1;
        }
    }
    lines
}

/// コミットに入る（消すもの以外の）ファイルの、新しい中身のハッシュとパス。
fn staged_blobs(worktree: &Path) -> Result<Vec<(String, String)>> {
    // 形: ":古いmode 新しいmode 古いhash 新しいhash 状態\0パス\0"（名前の変更は検出しない）
    let raw = git_output(
        worktree,
        &[
            "diff",
            "--cached",
            "--raw",
            "-z",
            "--no-abbrev",
            "--no-renames",
        ],
    )?;
    let mut fields = raw.split('\0').filter(|s| !s.is_empty());
    let mut staged = Vec::new();
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if let Some(blob) = parts.get(3) {
            staged.push((blob.to_string(), path.to_string()));
        }
    }
    Ok(staged)
}

/// ファイルの中身のハッシュ（gitのblob）。`tick` が、置いた秘密のファイルについて求める。
pub fn blob_hash(worktree: &Path, file: &Path) -> Result<String> {
    let file = file
        .to_str()
        .ok_or_else(|| Error::new(format!("パスに使えない文字があります: {}", file.display())))?;
    Ok(git_output(worktree, &["hash-object", "--", file])?
        .trim()
        .to_string())
}

/// 設定に書いたコマンドを、worktreeで実行する。標準出力と標準エラーはそのまま流し、終了コードを返す。
///
/// ビルドツールのキャッシュは、環境変数でagentcellsが持つ場所（`Context::caches`）に向ける。
/// sandboxに止められた、ホームの下の場所が出力にあれば、最後に `[product.sandbox]` に足すとよい書き方を出す。
/// 使う人がsandboxの仕組みを知らなくても、動かしてみれば、何を足せばよいかがわかるようにするため。
pub fn run(context: &Context, name: &str) -> Result<i32> {
    let command = context.commands.get(name).ok_or_else(|| {
        let names: Vec<&str> = context.commands.keys().map(String::as_str).collect();
        Error::new(format!("コマンド「{name}」はありません")).with_hint(if context.commands_limited {
            if names.is_empty() {
                "このcellでは、コマンドを使えません（cellの設定で決めています）".to_string()
            } else {
                format!("このcellで使えるのは {} です（cellの設定で決めています）", names.join(", "))
            }
        } else if names.is_empty() {
            "このproductには、実行できるコマンドがありません。必要なら、人間に設定を頼んでください（needs-human）"
                .to_string()
        } else {
            format!("使えるのは {} です", names.join(", "))
        })
    })?;
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    let (code, blocked) = run_command(context, command, user_home.as_deref())?;
    if let Some(user_home) = &user_home {
        let suggestions = suggestions(&blocked, user_home, &context.worktree);
        if !suggestions.is_empty() {
            eprintln!(
                "\nagentcells: sandboxで、ホームの下の次の場所が使えませんでした（コマンドの出力から拾ったものです。本当に要るかを確かめてから足してください）:\n{}\n\
                 ビルドやテストに要るなら、needs-human にして、config.toml の [product.sandbox] に足すよう人間に頼んでください。足すかどうかは人間が決めます",
                suggestions
                    .iter()
                    .map(|s| format!("  - {s}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
    Ok(code)
}

/// コマンドが終わったあと、出力を読み切るのを待つ時間。コマンドがバックグラウンドに残したプロセス
/// （テストのサーバーやコンパイラのデーモン）が出力を握ったままでも、ここで見切る。
const OUTPUT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// コマンドを動かし、出力をそのまま流しながら、sandboxに止められたホームの下の場所を集める。
fn run_command(
    context: &Context,
    command: &str,
    user_home: Option<&Path>,
) -> Result<(i32, Vec<PathBuf>)> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(&context.worktree)
        .env("TMPDIR", &context.tmp)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (name, dir) in &context.caches {
        crate::caches::clean(name, dir)?;
        cmd.envs(crate::caches::env(name, dir, |key| std::env::var(key).ok()));
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::new(format!("「{command}」を実行できません: {e}")))?;
    let (sender, receiver) = std::sync::mpsc::channel();
    let relay = |from: Box<dyn std::io::Read + Send>, to: fn() -> Box<dyn std::io::Write>| {
        let user_home = user_home.map(Path::to_path_buf);
        let sender = sender.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(from);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
                let mut out = to();
                let _ = out.write_all(&line);
                let _ = out.flush();
                if let Some(user_home) = &user_home {
                    let places = blocked_places(&String::from_utf8_lossy(&line), user_home);
                    if !places.is_empty() {
                        let _ = sender.send(Some(places));
                    }
                }
                line.clear();
            }
            let _ = sender.send(None);
        });
    };
    relay(
        Box::new(child.stdout.take().expect("stdout is piped")),
        || Box::new(std::io::stdout()),
    );
    relay(
        Box::new(child.stderr.take().expect("stderr is piped")),
        || Box::new(std::io::stderr()),
    );
    drop(sender);
    let status = child
        .wait()
        .map_err(|e| Error::new(format!("「{command}」を待てません: {e}")))?;
    // 2つのスレッドが読み終えるか、見切るまで集める
    let mut blocked = Vec::new();
    let mut finished = 0;
    let deadline = std::time::Instant::now() + OUTPUT_GRACE;
    while finished < 2 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match receiver.recv_timeout(left) {
            Ok(Some(places)) => {
                for place in places {
                    if !blocked.contains(&place) {
                        blocked.push(place);
                    }
                }
            }
            Ok(None) => finished += 1,
            Err(_) => break,
        }
    }
    Ok((status.code().unwrap_or(1), blocked))
}

/// sandboxに止められたときの言い方（大文字・小文字は問わない）。Nodeは `EPERM: operation not permitted` と出す。
const BLOCKED_MARKERS: &[&str] = &[
    "operation not permitted",
    "accessdeniedexception",
    "permission denied",
    "eperm",
    "eacces",
];

/// 1行の出力から、sandboxに止められた、ホームの下のパスを取り出す。
fn blocked_places(line: &str, user_home: &Path) -> Vec<PathBuf> {
    let lower = line.to_lowercase();
    if !BLOCKED_MARKERS.iter().any(|m| lower.contains(m)) {
        return Vec::new();
    }
    let prefix = format!("{}/", user_home.display());
    let mut places = Vec::new();
    for (start, _) in line.match_indices(&prefix) {
        let rest = &line[start + prefix.len()..];
        // 引用符で囲まれていれば、閉じる引用符まで（空白を含むパスがある）。なければ空白や区切りまで
        let quote = line[..start]
            .chars()
            .last()
            .filter(|c| *c == '\'' || *c == '"');
        let end = match quote {
            Some(q) => rest.find(q),
            None => rest.find(|c: char| c.is_whitespace() || "'\":),;`".contains(c)),
        }
        .unwrap_or(rest.len());
        let relative = rest[..end].trim_end_matches('/');
        if relative.is_empty() {
            continue;
        }
        let place = user_home.join(relative);
        if !places.contains(&place) {
            places.push(place);
        }
    }
    places
}

/// 止められた場所から、`[product.sandbox]` に足すとよい書き方を作る。
/// - 使う人のビルドツールのキャッシュなら、agentcellsが持つキャッシュ（`caches`）を勧める
/// - それ以外は、止められたパスそのもの（`bin/` の下なら、その `bin`）を `read` で勧める。広く丸めると、
///   同じ場所にある秘密（`~/.cargo/credentials.toml`、`~/.android/adbkey` など）まで開けることになるため
///   （止められたのが読むことか書くことかは、出力からはわからない。書かせると手元のツールに効くので、まず読ませる）
/// - 開けられない場所（秘密がよく置かれる場所、書かせない場所、agentcellsの置き場所）は勧めない
fn suggestions(blocked: &[PathBuf], user_home: &Path, worktree: &Path) -> Vec<String> {
    let worktree = worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf());
    let guarded = |relative: &Path| {
        crate::settings::SECRET_PLACES
            .iter()
            .chain(crate::settings::WRITE_PROTECTED_PLACES)
            .chain(&[".config/gh"])
            .any(|p| {
                let p = p.to_lowercase();
                let r = relative.to_string_lossy().to_lowercase();
                r == p || r.starts_with(&format!("{p}/"))
            })
    };
    let mut out = Vec::new();
    for path in blocked {
        let Ok(relative) = path.strip_prefix(user_home) else {
            continue;
        };
        let mut place = PathBuf::new();
        for part in relative.components() {
            place.push(part);
            if part.as_os_str() == "bin" {
                break;
            }
        }
        if place.as_os_str().is_empty() {
            continue;
        }
        // 作業の場所と同じ、ホームのすぐ下の場所（~/.agentcells）の中は開けられない
        let top: PathBuf = relative.components().take(1).collect();
        let top = user_home
            .join(&top)
            .canonicalize()
            .unwrap_or_else(|_| user_home.join(&top));
        if worktree.starts_with(&top) || guarded(relative) {
            continue;
        }
        let suggestion = match crate::caches::for_place(relative) {
            Some(cache) => format!(
                "~/{} → caches = [\"{cache}\"]（agentcellsが持つキャッシュに向けます。手元のキャッシュには書かせません）",
                relative.display()
            ),
            None => format!(
                "~/{} → read = [\"~/{}\"]（読むだけ）",
                relative.display(),
                place.display()
            ),
        };
        let key = suggestion.split(" → ").nth(1).unwrap_or("").to_string();
        if !out.iter().any(|s: &String| s.ends_with(&key)) {
            out.push(suggestion);
        }
    }
    out
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
        let scanner = fake_scanner(dir.path());
        let tmp = dir.path().join("tmp");
        std::fs::create_dir(&tmp).unwrap();
        let context = Context {
            worktree: work,
            commands: [(
                "hello".to_string(),
                "echo \"$TMPDIR\" > made.txt; exit 3".to_string(),
            )]
            .into_iter()
            .collect(),
            placed: vec![PathBuf::from("local.properties")],
            placed_blobs: vec!["0000000000000000000000000000000000000000".to_string()],
            identity: Identity {
                name: "t".to_string(),
                email: "t@example.com".to_string(),
            },
            scanner,
            tmp,
            caches: BTreeMap::new(),
            base: Some("main".to_string()),
            network: Vec::new(),
            commands_limited: false,
        };
        (dir, context)
    }

    #[test]
    fn merge_takes_in_the_base_and_reports_conflicts() {
        let (_dir, ctx) = setup();
        let wt = ctx.worktree.clone();
        let start = git(&wt, &["rev-parse", "HEAD"]);
        // ベースのブランチ（origin/main）が先に進んだ
        let advance = |file: &str, content: &str| {
            git(
                &wt,
                &["checkout", "-q", "--detach", "refs/remotes/origin/main"],
            );
            std::fs::write(wt.join(file), content).unwrap();
            git(&wt, &["commit", "-qam", "main"]);
            let tip = git(&wt, &["rev-parse", "HEAD"]);
            git(&wt, &["update-ref", "refs/remotes/origin/main", &tip]);
            git(&wt, &["checkout", "-q", "-"]);
        };
        git(&wt, &["update-ref", "refs/remotes/origin/main", &start]);
        git(&wt, &["checkout", "-q", "-b", "ac/x"]);
        std::fs::write(wt.join(".gitignore"), "local.properties\nmine\n").unwrap();
        git(&wt, &["commit", "-qam", "pr"]);
        advance("a.txt", "from main");
        assert_eq!(merge(&ctx).unwrap(), Merged::Clean);
        assert_eq!(
            git(&wt, &["show", "-s", "--format=%P"]).split(' ').count(),
            2
        );

        // 同じところを変えていれば、ぶつかる。直してコミットすると、マージのコミットになる。
        // ベースで人が足した行（秘密らしいものや、ぶつかりの印に似たもの）では断らない
        std::fs::write(wt.join("a.txt"), "from pr").unwrap();
        git(&wt, &["commit", "-qam", "pr2"]);
        std::fs::write(wt.join("doc.md"), "x").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "doc"]);
        git(
            &wt,
            &["checkout", "-q", "--detach", "refs/remotes/origin/main"],
        );
        std::fs::write(
            wt.join("fixture.txt"),
            "token = FAKE-SECRET\n<<<<<<< example\n",
        )
        .unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "fixture"]);
        let tip = git(&wt, &["rev-parse", "HEAD"]);
        git(&wt, &["update-ref", "refs/remotes/origin/main", &tip]);
        git(&wt, &["checkout", "-q", "-"]);
        advance("a.txt", "from main again");
        assert_eq!(
            merge(&ctx).unwrap(),
            Merged::Conflicts(vec!["a.txt".to_string()])
        );
        // ぶつかりの印を残したままでは、コミットしない
        let err = commit(&ctx, "Merge main").unwrap_err();
        assert!(err.message.contains("a.txt:1"), "{}", err.message);
        // 直しに入れた秘密は断る
        std::fs::write(wt.join("a.txt"), "both FAKE-SECRET").unwrap();
        let err = commit(&ctx, "Merge main").unwrap_err();
        assert!(err.message.contains("秘密"), "{}", err.message);
        std::fs::write(wt.join("a.txt"), "both").unwrap();
        commit(&ctx, "Merge main").unwrap();
        assert_eq!(
            git(&wt, &["show", "-s", "--format=%P"]).split(' ').count(),
            2
        );

        // やめれば、取り込む前に戻る
        std::fs::write(wt.join("a.txt"), "pr3").unwrap();
        git(&wt, &["commit", "-qam", "pr3"]);
        let before = git(&wt, &["rev-parse", "HEAD"]);
        advance("a.txt", "main3");
        assert!(matches!(merge(&ctx).unwrap(), Merged::Conflicts(_)));
        merge_abort(&ctx).unwrap();
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]), before);
        assert_eq!(git(&wt, &["status", "--porcelain"]), "");

        // 日本語の名前のファイルも、ぶつかりの印を見る（a.txt は、やめたときのまま、またぶつかる）
        std::fs::write(wt.join("日本.txt"), "pr").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "jp"]);
        git(
            &wt,
            &["checkout", "-q", "--detach", "refs/remotes/origin/main"],
        );
        std::fs::write(wt.join("日本.txt"), "main").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "jp main"]);
        let tip = git(&wt, &["rev-parse", "HEAD"]);
        git(&wt, &["update-ref", "refs/remotes/origin/main", &tip]);
        git(&wt, &["checkout", "-q", "-"]);
        assert_eq!(
            merge(&ctx).unwrap(),
            Merged::Conflicts(vec!["a.txt".to_string(), "日本.txt".to_string()])
        );
        let err = commit(&ctx, "Merge main").unwrap_err();
        assert!(err.message.contains("日本.txt:1"), "{}", err.message);
    }

    /// 偽のbetterleaks。「FAKE-SECRET」を含む行を見つけたことにし、動いた場所と引数を書き残す。
    fn fake_scanner(dir: &Path) -> PathBuf {
        let path = dir.join("fake-betterleaks");
        let script = format!(
            r#"#!/bin/sh
pwd > '{dir}/scanner-cwd'
echo "$@" > '{dir}/scanner-args'
n=$(grep -n FAKE-SECRET | head -1 | cut -d: -f1)
if [ -n "$n" ]; then echo "[{{\"RuleID\":\"fake\",\"StartLine\":$n}}]"; exit 1; fi
echo "[]"
"#,
            dir = dir.display()
        );
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
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
        for bad in [
            "--output=/tmp/x",
            "--OUTPUT",
            "--ext-diff",
            "-o",
            "--no-index",
        ] {
            assert!(git_read_args("diff", &[bad.to_string()]).is_err(), "{bad}");
        }
        assert!(git_read_args("push", &[]).is_err());
        assert!(git_read_args("config", &[]).is_err());
        let hint = |sub: &str| git_read_args(sub, &[]).unwrap_err().hint.unwrap();
        assert!(hint("grep").contains("Grep ツール"), "{}", hint("grep"));
        assert!(
            hint("ls-files").contains("Glob ツール"),
            "{}",
            hint("ls-files")
        );
        assert!(!hint("push").contains("ツール"), "{}", hint("push"));
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
        ctx.placed_blobs
            .push("0000000000000000000000000000000000000001".to_string());
        let err = commit(&ctx, "leak").unwrap_err();
        assert!(err.message.contains("secret.env"), "{}", err.message);
        assert_eq!(git(&wt, &["diff", "--cached", "--name-only"]), "");
        assert_eq!(git(&wt, &["log", "-1", "--format=%s"]), "Add b");
    }

    #[test]
    fn commit_refuses_files_that_run_when_a_person_opens_the_branch() {
        let (_dir, ctx) = setup();
        let wt = ctx.worktree.clone();
        std::fs::create_dir_all(wt.join(".vscode")).unwrap();
        std::fs::write(wt.join(".vscode/tasks.json"), "{}").unwrap();
        // 大文字・小文字を変えても、下のディレクトリに置いても断る
        std::fs::create_dir_all(wt.join(".Claude")).unwrap();
        std::fs::write(wt.join(".Claude/settings.json"), "{}").unwrap();
        std::fs::create_dir_all(wt.join("sub")).unwrap();
        std::fs::write(wt.join("sub/.envrc"), "echo hi").unwrap();
        std::os::unix::fs::symlink("a.txt", wt.join("link")).unwrap();
        std::fs::write(wt.join("ok.txt"), "ok").unwrap();
        let err = commit(&ctx, "x").unwrap_err();
        for path in [
            ".vscode/tasks.json",
            ".Claude/settings.json",
            "sub/.envrc",
            "link",
        ] {
            assert!(err.message.contains(path), "{path}: {}", err.message);
        }
        assert!(!err.message.contains("ok.txt"), "{}", err.message);
        assert!(err.hint.unwrap().contains("needs-human"));
        assert_eq!(git(&wt, &["log", "-1", "--format=%s"]), "base");
        // 外せば、ほかの変更はコミットできる
        for path in [".vscode", ".Claude", "sub", "link"] {
            remove(&ctx, &wt, &[PathBuf::from(path)]).unwrap();
        }
        commit(&ctx, "Add ok").unwrap();
        assert_eq!(git(&wt, &["log", "-1", "--format=%s"]), "Add ok");
    }

    #[test]
    fn guarded_files_from_the_base_do_not_block_a_merge_commit() {
        let (_dir, ctx) = setup();
        let wt = ctx.worktree.clone();
        let start = git(&wt, &["rev-parse", "HEAD"]);
        git(&wt, &["update-ref", "refs/remotes/origin/main", &start]);
        git(&wt, &["checkout", "-q", "-b", "ac/x"]);
        std::fs::write(wt.join("a.txt"), "from pr").unwrap();
        git(&wt, &["commit", "-qam", "pr"]);
        // ベースのブランチで、人がworkflowを足し、同じ行を変えた
        git(&wt, &["checkout", "-q", "--detach", &start]);
        std::fs::create_dir_all(wt.join(".github/workflows")).unwrap();
        std::fs::write(wt.join(".github/workflows/ci.yml"), "on: push").unwrap();
        std::fs::write(wt.join("a.txt"), "from main").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "main"]);
        let tip = git(&wt, &["rev-parse", "HEAD"]);
        git(&wt, &["update-ref", "refs/remotes/origin/main", &tip]);
        git(&wt, &["checkout", "-q", "ac/x"]);
        assert_eq!(
            merge(&ctx).unwrap(),
            Merged::Conflicts(vec!["a.txt".to_string()])
        );
        std::fs::write(wt.join("a.txt"), "both").unwrap();
        commit(&ctx, "Merge main").unwrap();
        assert_eq!(
            git(&wt, &["show", "-s", "--format=%P"]).split(' ').count(),
            2
        );
    }

    #[test]
    fn placed_secrets_cannot_be_renamed_or_committed_under_another_name() {
        let (_dir, mut ctx) = setup();
        let wt = ctx.worktree.clone();
        std::fs::write(wt.join("local.properties"), "SECRET=1\n").unwrap();
        ctx.placed_blobs = vec![blob_hash(&wt, &wt.join("local.properties")).unwrap()];

        let err = rename(
            &ctx,
            &wt,
            Path::new("local.properties"),
            Path::new("leaked.txt"),
        )
        .unwrap_err();
        assert!(err.message.contains("置いた秘密"), "{}", err.message);
        assert!(remove(&ctx, &wt, &[PathBuf::from("local.properties")]).is_err());

        // Writeで写したものも、中身で見分けて断る
        std::fs::write(wt.join("copy.txt"), "SECRET=1\n").unwrap();
        let err = commit(&ctx, "leak").unwrap_err();
        assert!(err.message.contains("copy.txt"), "{}", err.message);
        assert_eq!(git(&wt, &["diff", "--cached", "--name-only"]), "");
    }

    #[test]
    fn uppercase_git_is_still_git() {
        let (_dir, ctx) = setup();
        let wt = ctx.worktree.clone();
        for bad in [".GIT", ".Git/config", ".GIT/HEAD"] {
            assert!(ctx.inside(&wt, Path::new(bad)).is_err(), "{bad}");
        }
        assert!(rename(&ctx, &wt, Path::new("a.txt"), Path::new(".GIT/x")).is_err());
    }

    #[test]
    fn commit_uses_the_given_identity_without_user_config() {
        let (_dir, mut ctx) = setup();
        let wt = ctx.worktree.clone();
        ctx.identity = Identity {
            name: "agentcells-yh[bot]".to_string(),
            email: "1+agentcells-yh[bot]@users.noreply.github.com".to_string(),
        };
        std::fs::write(wt.join("d.txt"), "d").unwrap();
        commit(&ctx, "Add d").unwrap();
        assert_eq!(
            git(&wt, &["log", "-1", "--format=%an <%ae>"]),
            "agentcells-yh[bot] <1+agentcells-yh[bot]@users.noreply.github.com>"
        );
    }

    #[test]
    fn commit_refuses_when_placed_hashes_are_missing() {
        let (_dir, mut ctx) = setup();
        ctx.placed_blobs.clear();
        std::fs::write(ctx.worktree.join("e.txt"), "e").unwrap();
        let err = commit(&ctx, "Add e").unwrap_err();
        assert!(err.message.contains("ハッシュがそろっていません"));
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
        // 一時ディレクトリは、ほかのセッションと共有しない仕事ごとの場所にする
        let made = std::fs::read_to_string(ctx.worktree.join("made.txt")).unwrap();
        assert_eq!(made.trim(), ctx.tmp.display().to_string());
        let err = run(&ctx, "rm-everything").unwrap_err();
        assert!(err.hint.unwrap().contains("hello"));
    }

    #[test]
    fn blocked_places_are_found_in_command_output() {
        let home = Path::new("/Users/me");
        assert_eq!(
            blocked_places(
                "cat: /Users/me/.gradle/caches/x.lock: Operation not permitted",
                home
            ),
            vec![PathBuf::from("/Users/me/.gradle/caches/x.lock")]
        );
        // Nodeは小文字で、パスを引用符で囲んで出す
        assert_eq!(
            blocked_places(
                "Error: EPERM: operation not permitted, mkdir '/Users/me/.npm/_cacache'",
                home
            ),
            vec![PathBuf::from("/Users/me/.npm/_cacache")]
        );
        // 引用符の中の空白を含むパス
        assert_eq!(
            blocked_places(
                "EACCES: permission denied, open \"/Users/me/Library/Application Support/x.json\"",
                home
            ),
            vec![PathBuf::from(
                "/Users/me/Library/Application Support/x.json"
            )]
        );
        // 止められたことを言っていない行や、ホームの外は拾わない
        assert!(blocked_places("reading /Users/me/.gradle/caches", home).is_empty());
        assert!(blocked_places("/opt/x: Operation not permitted", home).is_empty());
    }

    #[test]
    fn suggestions_prefer_agentcells_caches_and_skip_guarded_places() {
        let home = Path::new("/Users/me");
        let worktree = Path::new("/Users/me/.agentcells/worktrees/octo/app/3");
        let blocked: Vec<PathBuf> = [
            ".gradle/caches/x.lock",
            ".gradle/wrapper/dists",
            "Library/Android/sdk/licenses",
            ".ssh/id_ed25519",
            ".zshrc",
            ".agentcells/files/octo/app/key.jks",
            ".cargo/bin/cargo",
            ".android/debug.keystore",
            ".cargo/credentials.toml",
        ]
        .iter()
        .map(|p| home.join(p))
        .collect();
        let out = suggestions(&blocked, home, worktree);
        // 広く丸めない（~/.cargo や ~/.android には、秘密も置かれている）
        assert_eq!(out.len(), 4, "{out:?}");
        assert!(out[0].contains("caches = [\"gradle\"]"), "{out:?}");
        assert!(
            out[1].contains("read = [\"~/Library/Android/sdk/licenses\"]"),
            "{out:?}"
        );
        assert!(out[2].contains("read = [\"~/.cargo/bin\"]"), "{out:?}");
        assert!(
            out[3].contains("read = [\"~/.android/debug.keystore\"]"),
            "{out:?}"
        );
    }

    #[test]
    fn run_collects_blocked_places_and_points_caches_at_agentcells() {
        let (dir, mut ctx) = setup();
        let home = dir.path();
        ctx.caches
            .insert("gradle".to_string(), dir.path().join("caches/app/gradle"));
        let command = format!(
            "echo '{h}/.gradle/a: Operation not permitted' >&2; echo \"$GRADLE_USER_HOME\" > gradle.txt",
            h = home.display(),
        );
        let (code, blocked) = run_command(&ctx, &command, Some(home)).unwrap();
        assert_eq!(code, 0);
        assert_eq!(blocked, vec![home.join(".gradle/a")]);
        let gradle = std::fs::read_to_string(ctx.worktree.join("gradle.txt")).unwrap();
        assert_eq!(
            gradle.trim(),
            dir.path().join("caches/app/gradle").display().to_string()
        );
    }

    #[test]
    fn run_does_not_wait_for_background_processes() {
        let (_dir, ctx) = setup();
        let started = std::time::Instant::now();
        let (code, _) = run_command(&ctx, "sleep 8 & echo started", None).unwrap();
        assert_eq!(code, 0);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn context_round_trips_through_a_file() {
        let (dir, ctx) = setup();
        let path = dir.path().join("context.json");
        ctx.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(serde_json::from_str::<Context>(&text).unwrap(), ctx);
    }

    #[test]
    fn commit_refuses_secrets_found_by_the_scanner() {
        let (dir, ctx) = setup();
        let wt = ctx.worktree.clone();
        std::fs::write(wt.join("a.txt"), "a\nok\nFAKE-SECRET\n").unwrap();
        let err = commit(&ctx, "leak").unwrap_err();
        assert!(err.message.contains("a.txt:3（fake）"), "{}", err.message);
        assert_eq!(git(&wt, &["log", "-1", "--format=%s"]), "base");
        // AIが設定や無視のファイルを置けない場所で、allowのコメントを無視させて動かす
        let cwd = std::fs::read_to_string(dir.path().join("scanner-cwd")).unwrap();
        assert_eq!(cwd.trim(), "/");
        let args = std::fs::read_to_string(dir.path().join("scanner-args")).unwrap();
        assert!(args.contains("--ignore-gitleaks-allow"), "{args}");
        assert!(args.contains("--gitleaks-ignore-path /"), "{args}");

        std::fs::write(wt.join("a.txt"), "a\nok\n").unwrap();
        commit(&ctx, "fine").unwrap();
    }

    #[test]
    fn commit_refuses_when_the_scanner_cannot_run() {
        let (dir, mut ctx) = setup();
        ctx.scanner = dir.path().join("missing-betterleaks");
        std::fs::write(ctx.worktree.join("b.txt"), "b").unwrap();
        let err = commit(&ctx, "Add b").unwrap_err();
        assert!(err.message.contains("betterleaks"), "{}", err.message);
        assert!(err.hint.unwrap().contains("needs-human"));
    }

    #[test]
    fn added_lines_track_paths_and_line_numbers() {
        let diff = "diff --git a/x.txt b/x.txt\n\
index 1..2 100644\n\
--- a/x.txt\n\
+++ b/x.txt\n\
@@ -0,0 +1,2 @@\n\
+one\n\
+++ not a header\n\
@@ -5 +7 @@\n\
-old\n\
+seven\n\
diff --git a/y.txt b/y.txt\n\
--- a/y.txt\n\
+++ /dev/null\n\
@@ -1 +0,0 @@\n\
-gone\n";
        let lines: Vec<(String, usize, String)> = added_lines(diff)
            .into_iter()
            .map(|l| (l.path, l.line, l.text))
            .collect();
        assert_eq!(
            lines,
            vec![
                ("x.txt".to_string(), 1, "one".to_string()),
                // 足した行が「++ 」で始まっても、名前と取り違えずに調べる
                ("x.txt".to_string(), 2, "++ not a header".to_string()),
                ("x.txt".to_string(), 7, "seven".to_string()),
            ]
        );
    }

    /// 本物のbetterleaksで、worktreeに置いた設定や allow のコメントで見逃させられないことを確かめる。
    /// betterleaksが入っていなければ確かめない。
    #[test]
    fn real_betterleaks_ignores_settings_in_the_worktree() {
        let Some(scanner) = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join("betterleaks"))
                .find(|p| p.is_file())
        }) else {
            eprintln!("betterleaksがないので確かめません");
            return;
        };
        let (_dir, mut ctx) = setup();
        ctx.scanner = scanner;
        let wt = ctx.worktree.clone();
        std::fs::write(
            wt.join(".betterleaks.toml"),
            "[extend]\nuseDefault = false\n",
        )
        .unwrap();
        std::fs::write(wt.join(".gitleaks.toml"), "[extend]\nuseDefault = false\n").unwrap();
        std::fs::write(wt.join(".betterleaksignore"), "*\n").unwrap();
        // テストのたびに作る、本物らしいトークン（ソースに秘密らしい文字列を置かない）
        let body: String = (0..36)
            .map(|i| {
                let n = (std::process::id() as usize * 31 + i * 7919) % 62;
                b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"[n] as char
            })
            .collect();
        std::fs::write(
            wt.join("config.txt"),
            format!("token = \"ghp_{body}\" # betterleaks:allow gitleaks:allow\n"),
        )
        .unwrap();
        let err = commit(&ctx, "leak").unwrap_err();
        assert!(err.message.contains("config.txt:1"), "{}", err.message);
    }
}
