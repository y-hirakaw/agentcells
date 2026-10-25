//! 1件の仕事を、cellとして `claude -p` で動かす。
//!
//! 流れ:
//! 1. `ac:working` を付ける（ほかのtickや人間に、処理中だと示す）
//! 2. Issue/PRを読み、もう一度 [`guard`] を通す（確かめてから読むまでの間の書き換えを見逃さない）
//! 3. 作業の場所を作る（`repos/` のbareのcloneを最新にし、そこから仕事ごとの `--shared` のcloneを作る）
//! 4. 今の仕事のファイルと、`--settings` で渡すsandboxの設定を書く
//! 5. `claude -p` を、時間の上限とプロセスグループを付けて動かす
//! 6. 結果を読み、AIが実行中にラベルを変えていないかを確かめる
//! 7. コメントをbetterleaksで調べてから、目印を付けて投稿する
//! 8. ラベルを付け替える（付けてから外す）。どこかで失敗したら、人間に回す
//!
//! 今は、コミットを要らないIssueのcell（triageなど）だけを動かす。implementとreviewは、pushとPRの作成、
//! レビューの投稿をつなぐときに動かす。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::actions::{self, Context, Identity};
use crate::caches;
use crate::config::{Cell, Config, Product, Target};
use crate::error::{Error, Result};
use crate::files;
use crate::github::{self, Credentials, ISOLATED_GIT_ENV, Purpose};
use crate::guard::{self, Actors};
use crate::home::{Home, RepoName};
use crate::labels::{NEEDS_HUMAN, WORKING};
use crate::marker;
use crate::probe::Probe;
use crate::prompt;
use crate::settings::{self, RunPaths};
use crate::tick::Work;
use crate::transition::{self, Destination, Finished, Outcome};

/// 1回の実行の結果。
#[derive(Debug)]
pub struct Report {
    /// 進めた先（`implement`、`done`、`needs-human` など）
    pub destination: String,
    /// 人間に回したときの理由
    pub reason: Option<String>,
}

/// 今は動かせない仕事なら、その理由。
pub fn unsupported(work: &Work, cell: &Cell) -> Option<String> {
    if work.item != Target::Issue {
        return Some("PRの仕事は、まだ動かせません".to_string());
    }
    if cell.name == "implement" {
        return Some("implementは、pushとPRの作成をつなぐまで動かせません".to_string());
    }
    None
}

/// 実行に使うもの。テストでは、claudeを偽のスクリプトにする。
pub struct Runner<'a> {
    pub home: &'a Home,
    pub config: &'a Config,
    pub probe: &'a dyn Probe,
    pub actors: &'a Actors,
    /// `claude` のプログラム
    pub claude: &'a Path,
    /// `agentcells` のプログラム（AIが `agentcells cell` で呼ぶ）
    pub agentcells: &'a Path,
    /// 秘密を探す `betterleaks` のプログラム
    pub scanner: &'a Path,
    /// `claude -p` の `--model`。なければ既定のもの
    pub model: Option<&'a str>,
}

impl Runner<'_> {
    /// 1件の仕事を動かす。`ac:working` を付けたあとに失敗したら、人間に回してから、エラーを返す。
    pub fn run(&self, work: &Work) -> Result<Report> {
        let product = self
            .config
            .products
            .iter()
            .find(|p| p.name == work.product)
            .ok_or_else(|| Error::new(format!("product「{}」がありません", work.product)))?;
        let cell = self
            .config
            .cell(&work.cell)
            .ok_or_else(|| Error::new(format!("cell「{}」がありません", work.cell)))?;
        let write = github::credentials(self.config, self.probe, &work.repo, Purpose::Work)?;
        let issue = Issue {
            probe: self.probe,
            repo: &work.repo,
            number: work.number,
            credentials: &write,
        };

        issue.edit_labels(&[WORKING], &[])?;
        match self.run_working(work, product, cell, &issue) {
            Ok(report) => Ok(report),
            Err(e) => {
                // 動かせなかったことを、人間に伝えて回す。round 0 の目印は、往復の回数に数えない
                let round = 0;
                let text = format!(
                    "実行できませんでした: {}{}\n\n確かめて、ラベル「{}」を付け直すと、もう一度動きます。",
                    e.message,
                    e.hint
                        .as_ref()
                        .map(|h| format!("（{h}）"))
                        .unwrap_or_default(),
                    cell.label
                );
                let _ = issue.comment(&marker::comment(&cell.name, round, &text));
                let _ = issue.edit_labels(&[NEEDS_HUMAN], &[&cell.label, WORKING]);
                Err(e)
            }
        }
    }

    fn run_working(
        &self,
        work: &Work,
        product: &Product,
        cell: &Cell,
        issue: &Issue,
    ) -> Result<Report> {
        let read =
            |repo: &RepoName| github::credentials(self.config, self.probe, repo, Purpose::Read);
        let mut task = prompt::fetch_task(
            self.probe,
            &read,
            work.item,
            &work.repo,
            work.number,
            self.actors.writer.clone(),
            self.actors.operator.clone(),
        )?;

        // 確かめてから読むまでの間に書き換えられていないか、読んだあとにもう一度確かめる
        let read_credentials = read(&work.repo)?;
        let env = read_credentials.gh_env()?;
        let cell_labels: Vec<String> = product
            .cells
            .iter()
            .filter_map(|name| self.config.cell(name))
            .map(|c| c.label.clone())
            .collect();
        let request = guard::Request {
            kind: work.item,
            subject: guard::Subject {
                repo: &work.repo,
                number: work.number,
                env: &env,
            },
            author: &task.discussion.author,
            label: &cell.label,
            cell_labels: &cell_labels,
            original: None,
        };
        if let Some(reason) = guard::check(self.probe, &request, self.actors)? {
            // 勝手に始めようとした人に反応しないよう、コメントはせずに ac:working だけ外す
            issue.edit_labels(&[], &[WORKING])?;
            return Ok(Report {
                destination: "skipped".to_string(),
                reason: Some(reason),
            });
        }

        let round = task.round(&cell.name);
        let work_repo = product.repos.first().ok_or_else(|| {
            Error::new(format!("product「{}」に repos がありません", product.name))
        })?;
        let mut place = Place::new(self.home, &work.repo, work.number);
        place.prepare(self.probe, self.config, self.home, work_repo)?;
        task.worktree = Some(place.worktree.clone());

        // gitignoreされたファイルを置き、その中身のハッシュを、コミットさせないために覚えておく
        let placed = files::apply(self.home, work_repo, &place.worktree)?;
        let placed_blobs = placed
            .iter()
            .map(|p| actions::blob_hash(&place.worktree, p))
            .collect::<Result<Vec<_>>>()?;
        let cache_dirs = caches::dirs(
            &self.home.caches_dir(),
            &product.name,
            &product.sandbox.caches,
        );
        for dir in cache_dirs.values() {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::new(format!("{} を作れません: {e}", dir.display())))?;
        }
        let context = Context {
            worktree: place.worktree.clone(),
            commands: product.commands.clone(),
            placed: placed.clone(),
            placed_blobs,
            identity: self.identity(&read_credentials)?,
            scanner: self.scanner.to_path_buf(),
            tmp: place.tmp.clone(),
            caches: cache_dirs.clone(),
        };
        let context_path = place.run.join("context.json");
        context.save(&context_path)?;

        let placed_abs: Vec<PathBuf> = placed.iter().map(|p| place.worktree.join(p)).collect();
        let programs = [self.agentcells.to_path_buf(), self.scanner.to_path_buf()];
        let cache_list: Vec<PathBuf> = cache_dirs.values().cloned().collect();
        let objects = place.cache.join("objects");
        let run_paths = RunPaths {
            worktree: &place.worktree,
            git_dir: &place.git_dir,
            shared_objects: &objects,
            context: &context_path,
            placed: &placed_abs,
            programs: &programs,
            tmp: &place.tmp,
            caches: &cache_list,
        };
        let settings = settings::runtime_settings(self.home, self.config, product, &run_paths)?;
        let settings_path = place.run.join("settings.json");
        write_json(&settings_path, &settings)?;

        let extra = match &cell.prompt {
            Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
                Error::new(format!("追加の指示 {} を読めません: {e}", path.display()))
            })?),
            None => None,
        };
        let user = prompt::user_prompt(cell, extra.as_deref(), Some(&task));
        let system = prompt::system_prompt(cell);
        let schema = transition::output_schema(cell);

        let labels_before = issue.labels()?;
        let claude = ClaudeRun {
            program: self.claude,
            cwd: &place.worktree,
            settings: &settings_path,
            system: &system,
            schema: &schema,
            prompt: &user,
            context: &context_path,
            path_prefix: self.agentcells.parent(),
            model: self.model,
            timeout: Duration::from_secs(u64::from(cell.timeout_minutes) * 60),
            log_dir: &place.run,
        };
        let finished = claude.run()?;
        let labels_after = issue.labels()?;

        // AIが決めた結果を読む。読めなければ、人間に回す
        let mut overridden: Option<String> = None;
        let outcome = match finished {
            ClaudeEnd::TimedOut => Err(format!(
                "時間の上限（{}分）を超えたので、止めました",
                cell.timeout_minutes
            )),
            ClaudeEnd::Exited(stdout) => match serde_json::from_str::<serde_json::Value>(&stdout) {
                Ok(value) => Outcome::from_result(&value),
                Err(e) => Err(format!("AIの出力を読めません: {e}")),
            },
        };
        let (requested, mut text) = match outcome {
            Ok(outcome) => (outcome.next, outcome.comment),
            Err(reason) => {
                overridden = Some(reason);
                (transition::NEEDS_HUMAN.to_string(), String::new())
            }
        };
        if let Some(changed) = transition::tampered(&labels_before, &labels_after) {
            overridden = Some(changed);
        }

        // 投稿する前に、秘密らしいものがないかを調べる。あれば、AIの文章は投稿しない
        let found = actions::scan_text(self.scanner, &text)?;
        if !found.is_empty() {
            let rules: Vec<String> = found.iter().map(|(_, rule)| rule.clone()).collect();
            overridden = Some(format!(
                "AIのコメントに秘密らしいもの（{}）があったので、投稿しませんでした。実行の記録（{}）を確かめてください",
                rules.join("、"),
                place.run.display()
            ));
            text = String::new();
        }

        let requested = if overridden.is_some() {
            transition::NEEDS_HUMAN.to_string()
        } else {
            requested
        };
        let plan = transition::plan(
            self.config,
            product,
            &Finished {
                cell,
                item: work.item,
                created_pr: false,
                requested: &requested,
            },
            &|name: &str| task.round(name).saturating_sub(1),
        );
        let reason = overridden.or(plan.overridden.clone());
        let mut body = text.trim().to_string();
        if let Some(reason) = &reason {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&format!("人間に回しました: {reason}"));
        }
        issue.comment(&marker::comment(&cell.name, round, &body))?;

        // 付けてから外す（guardが、ac:working が付いている間のラベルをagentcellsの付け替えとみなすため）
        let add: Vec<&str> = plan.add.iter().map(String::as_str).collect();
        let remove: Vec<&str> = plan.remove.iter().map(String::as_str).collect();
        issue.edit_labels(&add, &[])?;
        issue.edit_labels(&[], &remove)?;
        if plan.close {
            issue.close()?;
        }
        place.clean();
        Ok(Report {
            destination: match &plan.destination {
                Destination::Cell(name) => name.clone(),
                Destination::Done => transition::DONE.to_string(),
                Destination::NeedsHuman => transition::NEEDS_HUMAN.to_string(),
            },
            reason,
        })
    }

    /// コミットの作者。書き込みのアカウントの名前と、GitHubのnoreplyのアドレス。
    fn identity(&self, read: &Credentials) -> Result<Identity> {
        let login = &self.actors.writer;
        let path = format!("/users/{login}");
        let out = self
            .probe
            .run_with_env("gh", &["api", &path, "--jq", ".id"], &read.gh_env()?)
            .ok_or_else(|| Error::new("ghが見つかりません"))?;
        let id = out.stdout.trim();
        if !out.success || id.is_empty() {
            return Err(Error::new(format!(
                "{login} のGitHubのIDを読めません: {}",
                out.stderr.trim()
            )));
        }
        Ok(Identity {
            name: login.clone(),
            email: format!("{id}+{login}@users.noreply.github.com"),
        })
    }
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| Error::new(format!("{} を書けません: {e}", path.display())))?;
    std::fs::write(path, text)
        .map_err(|e| Error::new(format!("{} を書けません: {e}", path.display())))
}

/// GitHubのIssueへの書き込み（書き込みのアカウントで）。
struct Issue<'a> {
    probe: &'a dyn Probe,
    repo: &'a RepoName,
    number: u64,
    credentials: &'a Credentials,
}

impl Issue<'_> {
    fn gh(&self, args: &[&str], input: Option<&str>) -> Result<String> {
        let env = self.credentials.gh_env()?;
        let out = self
            .probe
            .run_with_input("gh", args, &env, input)
            .ok_or_else(|| {
                Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
            })?;
        if !out.success {
            return Err(Error::new(format!(
                "gh {} が失敗しました: {}",
                args.first().unwrap_or(&""),
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }

    fn number(&self) -> String {
        self.number.to_string()
    }

    fn edit_labels(&self, add: &[&str], remove: &[&str]) -> Result<()> {
        if add.is_empty() && remove.is_empty() {
            return Ok(());
        }
        let repo = self.repo.to_string();
        let number = self.number();
        let add = add.join(",");
        let remove = remove.join(",");
        let mut args = vec!["issue", "edit", number.as_str(), "--repo", repo.as_str()];
        if !add.is_empty() {
            args.extend(["--add-label", add.as_str()]);
        }
        if !remove.is_empty() {
            args.extend(["--remove-label", remove.as_str()]);
        }
        self.gh(&args, None).map(|_| ())
    }

    fn comment(&self, body: &str) -> Result<()> {
        let repo = self.repo.to_string();
        let number = self.number();
        self.gh(
            &[
                "issue",
                "comment",
                number.as_str(),
                "--repo",
                repo.as_str(),
                "--body-file",
                "-",
            ],
            Some(body),
        )
        .map(|_| ())
    }

    fn close(&self) -> Result<()> {
        let repo = self.repo.to_string();
        let number = self.number();
        self.gh(
            &["issue", "close", number.as_str(), "--repo", repo.as_str()],
            None,
        )
        .map(|_| ())
    }

    fn labels(&self) -> Result<Vec<String>> {
        let repo = self.repo.to_string();
        let number = self.number();
        let out = self.gh(
            &[
                "issue",
                "view",
                number.as_str(),
                "--repo",
                repo.as_str(),
                "--json",
                "labels",
            ],
            None,
        )?;
        #[derive(Deserialize)]
        struct Labels {
            labels: Vec<Named>,
        }
        #[derive(Deserialize)]
        struct Named {
            name: String,
        }
        let labels: Labels = serde_json::from_str(&out)
            .map_err(|e| Error::new(format!("ラベルを読めません: {e}")))?;
        Ok(labels.labels.into_iter().map(|l| l.name).collect())
    }
}

/// 1件の仕事の場所。`<種類>/<owner>/<repo>/<番号>` の形にする（番号だけだと、別のリポジトリの同じ番号とぶつかる）。
struct Place {
    /// `repos/<owner>/<repo>`（作業するリポジトリのbareのclone）
    cache: PathBuf,
    worktree: PathBuf,
    git_dir: PathBuf,
    tmp: PathBuf,
    /// 今の仕事のファイル、sandboxの設定、AIの出力を置く（実行のあとも残す）
    run: PathBuf,
}

impl Place {
    fn new(home: &Home, repo: &RepoName, number: u64) -> Self {
        let rel = Path::new(&repo.owner)
            .join(&repo.name)
            .join(number.to_string());
        Place {
            cache: PathBuf::new(),
            worktree: home.worktrees_dir().join(&rel),
            git_dir: home.root().join("gitdirs").join(&rel),
            tmp: home.root().join("tmp").join(&rel),
            run: home.root().join("runs").join(&rel),
        }
    }

    /// 作業するリポジトリのcloneを最新にし、仕事ごとの場所を空にして作り直す。
    fn prepare(
        &mut self,
        probe: &dyn Probe,
        config: &Config,
        home: &Home,
        work_repo: &RepoName,
    ) -> Result<()> {
        self.cache = update_cache(probe, config, home, work_repo)?;
        self.clean();
        for dir in [&self.tmp, &self.run] {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::new(format!("{} を作れません: {e}", dir.display())))?;
        }
        for dir in [&self.worktree, &self.git_dir] {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::new(format!("{} を作れません: {e}", parent.display())))?;
            }
        }
        // 元のリポジトリのオブジェクトは共有して読むだけにし、.git の実体は仕事ごとに作業場所の外に置く
        let mut env = ISOLATED_GIT_ENV.to_vec();
        env.push(("GIT_ALLOW_PROTOCOL", "file"));
        let git_dir_arg = format!("--separate-git-dir={}", self.git_dir.display());
        git(
            probe,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "clone",
                "-q",
                "--shared",
                &git_dir_arg,
                &path_str(&self.cache)?,
                &path_str(&self.worktree)?,
            ],
            &env,
        )
    }

    /// 作業の場所を消す（実行の記録は残す）。
    fn clean(&self) {
        for dir in [&self.worktree, &self.git_dir, &self.tmp] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// `repos/<owner>/<repo>` のbareのcloneを作るか、最新にする。CLIだけが書き、sandboxからは読むだけにする。
fn update_cache(
    probe: &dyn Probe,
    config: &Config,
    home: &Home,
    repo: &RepoName,
) -> Result<PathBuf> {
    let cache = home.repos_dir().join(&repo.owner).join(&repo.name);
    let credentials = github::credentials(config, probe, repo, Purpose::Read)?;
    let git_args = credentials.git_args();
    let env = credentials.git_env();
    let url = crate::pr::remote_url(repo);
    let mut args: Vec<String> = git_args.clone();
    if cache.join("HEAD").exists() {
        args.extend(
            [
                "-C",
                &path_str(&cache)?,
                "fetch",
                "-q",
                "--prune",
                &url,
                "+refs/heads/*:refs/heads/*",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    } else {
        if let Some(parent) = cache.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::new(format!("{} を作れません: {e}", parent.display())))?;
        }
        args.extend(
            ["clone", "-q", "--bare", &url, &path_str(&cache)?]
                .iter()
                .map(|s| s.to_string()),
        );
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git(probe, &args, &env)?;
    Ok(cache)
}

fn git(probe: &dyn Probe, args: &[&str], env: &[(&str, &str)]) -> Result<()> {
    let out = probe.run_with_env("git", args, env).ok_or_else(|| {
        Error::new("gitが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if out.success {
        Ok(())
    } else {
        Err(Error::new(format!(
            "gitが失敗しました: {}",
            out.stderr.trim()
        )))
    }
}

fn path_str(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| Error::new(format!("パスに使えない文字があります: {}", path.display())))
}

/// `claude -p` の1回の実行。
struct ClaudeRun<'a> {
    program: &'a Path,
    cwd: &'a Path,
    settings: &'a Path,
    system: &'a str,
    schema: &'a serde_json::Value,
    prompt: &'a str,
    context: &'a Path,
    /// AIが呼ぶ `agentcells` を、PATHの先頭で見つけさせる
    path_prefix: Option<&'a Path>,
    model: Option<&'a str>,
    timeout: Duration,
    /// 標準出力と標準エラーを残す場所
    log_dir: &'a Path,
}

enum ClaudeEnd {
    Exited(String),
    TimedOut,
}

/// AIに渡さない環境変数。トークンを、使う人の環境から持ち込ませない。
const STRIPPED_ENV: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

impl ClaudeRun<'_> {
    fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            "--output-format",
            "json",
            "--settings",
            &self.settings.display().to_string(),
            // 使う人のグローバルの設定は読まない。作業するリポジトリの設定（project、local）の上に重ねる
            "--setting-sources",
            "project,local",
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
        args
    }

    fn run(&self) -> Result<ClaudeEnd> {
        let log = |name: &str| -> Result<std::fs::File> {
            let path = self.log_dir.join(name);
            std::fs::File::create(&path)
                .map_err(|e| Error::new(format!("{} を作れません: {e}", path.display())))
        };
        let stdout_path = self.log_dir.join("claude.json");
        let path_env = match (self.path_prefix, std::env::var_os("PATH")) {
            (Some(prefix), Some(path)) => {
                let mut paths = vec![prefix.to_path_buf()];
                paths.extend(std::env::split_paths(&path));
                std::env::join_paths(paths).ok()
            }
            (_, path) => path,
        };
        let mut command = Command::new(self.program);
        command
            .args(self.args())
            .current_dir(self.cwd)
            .env(actions::CONTEXT_ENV, self.context)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log("claude.json")?))
            .stderr(Stdio::from(log("claude.err")?));
        if let Some(path) = path_env {
            command.env("PATH", path);
        }
        for key in STRIPPED_ENV {
            command.env_remove(key);
        }
        // AIが残したプロセスもまとめて止められるよう、新しいプロセスグループで動かす
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
        let deadline = Instant::now() + self.timeout;
        let timed_out = loop {
            match child.try_wait() {
                Ok(Some(_)) => break false,
                Ok(None) if Instant::now() >= deadline => break true,
                Ok(None) => std::thread::sleep(Duration::from_millis(500)),
                Err(e) => return Err(Error::new(format!("claudeを待てません: {e}"))),
            }
        };
        // 時間切れでも、終わったあとでも、AIが残したプロセスをまとめて止める
        kill_group(pid, timed_out);
        let _ = child.wait();
        if timed_out {
            return Ok(ClaudeEnd::TimedOut);
        }
        let stdout = std::fs::read_to_string(&stdout_path)
            .map_err(|e| Error::new(format!("claudeの出力を読めません: {e}")))?;
        Ok(ClaudeEnd::Exited(stdout))
    }
}

/// プロセスグループをまとめて止める。時間切れなら、まずTERMで止まる時間を与える。
fn kill_group(pid: u32, gentle: bool) {
    let group = format!("-{pid}");
    if gentle {
        let _ = Command::new("kill").args(["-TERM", "--", &group]).status();
        std::thread::sleep(Duration::from_secs(5));
    }
    let _ = Command::new("kill")
        .args(["-KILL", "--", &group])
        .stderr(Stdio::null())
        .status();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
            timeout,
            log_dir: dir,
        }
    }

    #[test]
    fn passes_the_prompt_on_stdin_and_keeps_settings_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            r#"cat > prompt.txt; echo "$@" > args.txt; echo "$PATH" > path.txt; echo "$AGENTCELLS_CONTEXT" > context.txt; echo "${GH_TOKEN:-none}" > token.txt; echo '{"structured_output":{"next":"done","comment":"ok"}}'"#,
        );
        let schema = serde_json::json!({"type": "object"});
        let end = claude_run(dir.path(), &program, &schema, Duration::from_secs(30))
            .run()
            .unwrap();
        let ClaudeEnd::Exited(stdout) = end else {
            panic!("timed out");
        };
        assert!(stdout.contains("\"done\""));
        let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(read("prompt.txt"), "user prompt");
        let args = read("args.txt");
        for arg in [
            "-p",
            "--setting-sources project,local",
            "--strict-mcp-config",
            "--settings /s.json",
            "--model haiku",
        ] {
            assert!(args.contains(arg), "{arg}: {args}");
        }
        assert!(read("path.txt").starts_with("/opt/agentcells/bin:"));
        assert_eq!(read("context.txt").trim(), "/c.json");
        assert_eq!(read("token.txt").trim(), "none");
    }

    #[test]
    fn stops_the_whole_process_group_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // バックグラウンドに子を残してから、待ち続ける
        let program = script(
            dir.path(),
            r#"cat > /dev/null; sleep 30 & echo $! > child.pid; sleep 30"#,
        );
        let schema = serde_json::json!({});
        let started = Instant::now();
        let end = claude_run(dir.path(), &program, &schema, Duration::from_secs(1))
            .run()
            .unwrap();
        assert!(matches!(end, ClaudeEnd::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(15));
        std::thread::sleep(Duration::from_millis(300));
        // 残した子も止まっている
        let child = std::fs::read_to_string(dir.path().join("child.pid")).unwrap();
        let alive = Command::new("kill")
            .args(["-0", child.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!alive.success(), "child {} is still alive", child.trim());
    }

    #[test]
    fn only_issue_cells_other_than_implement_run_for_now() {
        let work = |item, cell: &str| Work {
            product: "app".to_string(),
            cell: cell.to_string(),
            item,
            repo: RepoName::parse("octo/app").unwrap(),
            number: 1,
            title: "t".to_string(),
        };
        let cell = |name: &str| Cell {
            name: name.to_string(),
            on: vec![Target::Issue],
            label: format!("ac:{name}"),
            prompt: None,
            next: vec![],
            review: false,
            max_rounds: 3,
            timeout_minutes: 30,
        };
        assert!(unsupported(&work(Target::Issue, "triage"), &cell("triage")).is_none());
        assert!(unsupported(&work(Target::Issue, "implement"), &cell("implement")).is_some());
        assert!(unsupported(&work(Target::PullRequest, "review"), &cell("review")).is_some());
    }
}
