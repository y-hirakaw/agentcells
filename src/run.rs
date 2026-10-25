//! 1件の仕事を、cellとして `claude -p` で動かす。
//!
//! 流れ:
//! 1. `ac:working` を付ける（ほかのtickや人間に、処理中だと示す）
//! 2. Issue/PRを読み、もう一度 [`guard`] を通す（確かめてから読むまでの間の書き換えを見逃さない）
//! 3. 作業の場所を作る（`repos/` のbareのcloneを最新にし、そこから仕事ごとの `--shared` のcloneを作る）
//! 4. 今の仕事のファイルと、`--settings` で渡すsandboxの設定を書く
//! 5. ここまでに作ったGitHub Appのトークンを取り消す（AIの動かすものに読まれても使えないように）
//! 6. `claude -p` を、時間の上限を付けて動かす。終わったら、claudeとその子孫のプロセスをすべて止める
//! 7. 止めたことを確かめてから、新しいトークンを作る。結果を読み、AIが実行中にラベルを変えていないかを確かめる
//! 8. コメントをbetterleaksで調べてから、目印を付けて投稿する
//! 9. ラベルを付け替える（付けてから外す）。どこかで失敗したら、人間に回す
//!
//! 今は、コミットを要らないIssueのcell（triageなど）だけを動かす。implementとreviewは、pushとPRの作成、
//! レビューの投稿をつなぐときに動かす。

use std::collections::{BTreeMap, BTreeSet};
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
    /// 進めた先（`implement`、`done`、`needs-human`、見送ったときは `skipped`）
    pub destination: String,
    /// 人間に回したときや、見送ったときの理由
    pub reason: Option<String>,
}

/// 動かしている途中の失敗。
enum Failure {
    /// ふつうの失敗。人間に回すコメントとラベルを書く
    Error(Error),
    /// AIが動かしたプロセスを止めきれなかった。残ったプロセスにトークンを読まれうるので、
    /// トークンを作らず、GitHubには何も書かない（`ac:working` は付いたまま。人が片付ける）
    Unstopped(Error),
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Error(e)
    }
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
        let issue = Issue {
            probe: self.probe,
            repo: &work.repo,
            number: work.number,
        };
        {
            let write = self.write_credentials(&work.repo)?;
            let added = issue.edit_labels(&write, &[WORKING], &[]);
            write.revoke(self.probe);
            added?;
        }

        let mut place = Place::new(self.home, &work.repo, work.number);
        let result = self.run_working(work, product, cell, &issue, &mut place);
        // 置いた秘密のファイルを残さないよう、うまくいってもいかなくても作業の場所を消す（実行の記録は残す）
        place.clean();
        match result {
            Ok(report) => Ok(report),
            Err(failure) => Err(self.fail(failure, cell, &issue, &place)),
        }
    }

    /// 失敗したときの片付け。詳しいことは手元の記録（`error.txt`）とtickの出力に残し、
    /// 公開のIssueには、決まった短い文だけを書いて人間に回す。
    /// プロセスを止めきれなかったときは、トークンを作らず、GitHubには何も書かない。
    fn fail(&self, failure: Failure, cell: &Cell, issue: &Issue, place: &Place) -> Error {
        let (e, write_to_github) = match failure {
            Failure::Error(e) => (e, true),
            Failure::Unstopped(e) => (e, false),
        };
        let _ = std::fs::create_dir_all(&place.run);
        let _ = std::fs::write(
            place.run.join("error.txt"),
            format!("{}\n{}\n", e.message, e.hint.clone().unwrap_or_default()),
        );
        if !write_to_github {
            return e;
        }
        let text = format!(
            "実行できませんでした。agentcellsを動かしている人は、手元の記録（{}）を確かめてください。\n\n直したら、ラベル「{}」を付け直すと、もう一度動きます。",
            place.run_name(),
            cell.label
        );
        // round 0 の目印は、往復の回数に数えない
        if let Ok(write) = self.write_credentials(issue.repo) {
            let _ = issue.comment(&write, &marker::comment(&cell.name, 0, &text));
            let _ = issue.edit_labels(&write, &[NEEDS_HUMAN], &[&cell.label, WORKING]);
            write.revoke(self.probe);
        }
        e
    }

    fn write_credentials(&self, repo: &RepoName) -> Result<Credentials> {
        github::credentials(self.config, self.probe, repo, Purpose::Work)
    }

    fn run_working(
        &self,
        work: &Work,
        product: &Product,
        cell: &Cell,
        issue: &Issue,
        place: &mut Place,
    ) -> std::result::Result<Report, Failure> {
        let work_repo = place_repo(product)?;
        // claudeを動かす前に作ったトークンは、動かす前にすべて取り消す
        let issue_read = github::credentials(self.config, self.probe, &work.repo, Purpose::Read)?;
        let work_read = github::credentials(self.config, self.probe, &work_repo, Purpose::Read)?;
        let result = self.prepare(work, product, cell, place, &issue_read, &work_read, issue);
        issue_read.revoke(self.probe);
        work_read.revoke(self.probe);
        let ready = match result? {
            Prepared::Skipped(reason) => {
                // 勝手に始めようとした人に反応しないよう、コメントはせずに ac:working だけ外す。
                // 外せなくても、コメントはしない（tickの出力に出す）
                let removed = self.write_credentials(&work.repo).and_then(|write| {
                    let removed = issue.edit_labels(&write, &[], &[WORKING]);
                    write.revoke(self.probe);
                    removed
                });
                let note = match removed {
                    Ok(()) => reason,
                    Err(e) => format!("{reason}（ac:working を外せませんでした: {}）", e.message),
                };
                return Ok(Report {
                    destination: "skipped".to_string(),
                    reason: Some(note),
                });
            }
            Prepared::Ready(ready) => ready,
        };

        let finished = match ready.claude(self, cell, place).run()? {
            ClaudeEnd::Unstopped(e) => return Err(Failure::Unstopped(e)),
            other => other,
        };

        // AIの動かしたものをすべて止めたあとで、書き込みのトークンを作る
        let write = self.write_credentials(&work.repo)?;
        let result = self.finish(work, product, cell, issue, place, &ready, finished, &write);
        write.revoke(self.probe);
        Ok(result?)
    }

    /// claudeを動かす前の用意。ガードで見送るときは `Skipped`。
    #[allow(clippy::too_many_arguments)]
    fn prepare(
        &self,
        work: &Work,
        product: &Product,
        cell: &Cell,
        place: &mut Place,
        issue_read: &Credentials,
        work_read: &Credentials,
        issue: &Issue,
    ) -> Result<Prepared> {
        let work_repo = place_repo(product)?;
        let read = |repo: &RepoName| {
            if repo == &work.repo {
                issue_read.reuse()
            } else if repo == &work_repo {
                work_read.reuse()
            } else {
                Err(Error::new(format!(
                    "{repo} は、この仕事で読むリポジトリではありません"
                )))
            }
        };
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
        let env = issue_read.gh_env()?;
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
            return Ok(Prepared::Skipped(reason));
        }

        place.prepare(self.probe, work_read, self.home, &work_repo)?;
        task.worktree = Some(place.worktree.clone());

        // gitignoreされたファイルを置き、その中身のハッシュを、コミットさせないために覚えておく
        let placed = files::apply(self.home, &work_repo, &place.worktree)?;
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
            identity: self.identity(issue_read)?,
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
        Ok(Prepared::Ready(Box::new(Ready {
            user: prompt::user_prompt(cell, extra.as_deref(), Some(&task)),
            system: prompt::system_prompt(cell),
            schema: transition::output_schema(cell),
            settings_path,
            context_path,
            labels_before: issue.labels(issue_read)?,
            job_dirs: [
                place.worktree.clone(),
                place.git_dir.clone(),
                place.tmp.clone(),
            ]
            .into_iter()
            .chain(cache_list)
            .collect(),
            task,
        })))
    }

    /// claudeが終わったあと。結果を読んで投稿し、ラベルを付け替える。
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        work: &Work,
        product: &Product,
        cell: &Cell,
        issue: &Issue,
        place: &Place,
        ready: &Ready,
        finished: ClaudeEnd,
        write: &Credentials,
    ) -> Result<Report> {
        let labels_after = issue.labels(write)?;
        let task = &ready.task;
        let round = task.round(&cell.name);
        let runs = |name: &str| task.round(name).saturating_sub(1);

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
            ClaudeEnd::Unstopped(e) => return Err(e),
        };
        let (requested, text) = match outcome {
            Ok(outcome) => (outcome.next, outcome.comment),
            Err(reason) => {
                overridden = Some(reason);
                (transition::NEEDS_HUMAN.to_string(), String::new())
            }
        };
        if let Some(changed) = transition::tampered(&ready.labels_before, &labels_after) {
            overridden = Some(changed);
        }
        let requested = if overridden.is_some() {
            transition::NEEDS_HUMAN.to_string()
        } else {
            requested
        };
        let finished_cell = |requested: &str| {
            transition::plan(
                self.config,
                product,
                &Finished {
                    cell,
                    item: work.item,
                    created_pr: false,
                    requested,
                },
                &runs,
            )
        };
        let mut plan = finished_cell(&requested);
        let mut reason = overridden.or(plan.overridden.clone());
        let mut body = text.trim().to_string();
        if let Some(reason) = &reason {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&format!("人間に回しました: {reason}"));
        }
        let mut body = marker::comment(&cell.name, round, &body);

        // 投稿する本文の全体を、秘密らしいものがないか調べる。あれば、決まった文だけにして人間に回す
        let found = actions::scan_text(self.scanner, &body)?;
        if !found.is_empty() {
            let rules: Vec<String> = found.iter().map(|(_, rule)| rule.clone()).collect();
            let note = format!(
                "コメントに秘密らしいもの（{}）があったので、投稿しませんでした。agentcellsを動かしている人は、手元の記録（{}）を確かめてください",
                rules.join("、"),
                place.run_name()
            );
            body = marker::comment(&cell.name, round, &format!("人間に回しました: {note}"));
            plan = finished_cell(transition::NEEDS_HUMAN);
            reason = Some(note);
        }
        issue.comment(write, &body)?;

        // 付けてから外す（guardが、ac:working が付いている間のラベルをagentcellsの付け替えとみなすため）
        let add: Vec<&str> = plan.add.iter().map(String::as_str).collect();
        let remove: Vec<&str> = plan.remove.iter().map(String::as_str).collect();
        issue.edit_labels(write, &add, &[])?;
        issue.edit_labels(write, &[], &remove)?;
        if plan.close {
            issue.close(write)?;
        }
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

/// 作業するリポジトリ（今は product の repos の最初）。
fn place_repo(product: &Product) -> Result<RepoName> {
    product
        .repos
        .first()
        .cloned()
        .ok_or_else(|| Error::new(format!("product「{}」に repos がありません", product.name)))
}

enum Prepared {
    Skipped(String),
    Ready(Box<Ready>),
}

/// claudeを動かす用意ができたもの。
struct Ready {
    user: String,
    system: String,
    schema: serde_json::Value,
    settings_path: PathBuf,
    context_path: PathBuf,
    labels_before: Vec<String>,
    task: prompt::Task,
    /// worktree、一時ディレクトリ、キャッシュ
    job_dirs: Vec<PathBuf>,
}

impl Ready {
    fn claude<'a>(&'a self, runner: &'a Runner, cell: &Cell, place: &'a Place) -> ClaudeRun<'a> {
        ClaudeRun {
            program: runner.claude,
            cwd: &place.worktree,
            settings: &self.settings_path,
            system: &self.system,
            schema: &self.schema,
            prompt: &self.user,
            context: &self.context_path,
            path_prefix: runner.agentcells.parent(),
            model: runner.model,
            timeout: Duration::from_secs(u64::from(cell.timeout_minutes) * 60),
            log_dir: &place.run,
            job_dirs: &self.job_dirs,
        }
    }
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| Error::new(format!("{} を書けません: {e}", path.display())))?;
    std::fs::write(path, text)
        .map_err(|e| Error::new(format!("{} を書けません: {e}", path.display())))
}

/// GitHubのIssueへの読み書き。トークンは、そのたびに渡す（作る時を、AIが動いていないときに限るため）。
struct Issue<'a> {
    probe: &'a dyn Probe,
    repo: &'a RepoName,
    number: u64,
}

impl Issue<'_> {
    fn gh(&self, credentials: &Credentials, args: &[&str], input: Option<&str>) -> Result<String> {
        let env = credentials.gh_env()?;
        let out = self
            .probe
            .run_with_input("gh", args, &env, input)
            .ok_or_else(|| {
                Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
            })?;
        if !out.success {
            return Err(Error::new(format!(
                "gh {} {} が失敗しました: {}",
                args.first().unwrap_or(&""),
                args.get(1).unwrap_or(&""),
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }

    fn edit_labels(&self, credentials: &Credentials, add: &[&str], remove: &[&str]) -> Result<()> {
        if add.is_empty() && remove.is_empty() {
            return Ok(());
        }
        let repo = self.repo.to_string();
        let number = self.number.to_string();
        let add = add.join(",");
        let remove = remove.join(",");
        let mut args = vec!["issue", "edit", number.as_str(), "--repo", repo.as_str()];
        if !add.is_empty() {
            args.extend(["--add-label", add.as_str()]);
        }
        if !remove.is_empty() {
            args.extend(["--remove-label", remove.as_str()]);
        }
        self.gh(credentials, &args, None).map(|_| ())
    }

    fn comment(&self, credentials: &Credentials, body: &str) -> Result<()> {
        let repo = self.repo.to_string();
        let number = self.number.to_string();
        self.gh(
            credentials,
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

    fn close(&self, credentials: &Credentials) -> Result<()> {
        let repo = self.repo.to_string();
        let number = self.number.to_string();
        self.gh(
            credentials,
            &["issue", "close", number.as_str(), "--repo", repo.as_str()],
            None,
        )
        .map(|_| ())
    }

    fn labels(&self, credentials: &Credentials) -> Result<Vec<String>> {
        let repo = self.repo.to_string();
        let number = self.number.to_string();
        let out = self.gh(
            credentials,
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
    /// `<owner>/<repo>/<番号>`
    relative: PathBuf,
}

impl Place {
    fn new(home: &Home, repo: &RepoName, number: u64) -> Self {
        let relative = Path::new(&repo.owner)
            .join(&repo.name)
            .join(number.to_string());
        Place {
            cache: PathBuf::new(),
            worktree: home.worktrees_dir().join(&relative),
            git_dir: home.root().join("gitdirs").join(&relative),
            tmp: home.root().join("tmp").join(&relative),
            run: home.root().join("runs").join(&relative),
            relative,
        }
    }

    /// 公開のコメントに書く、実行の記録の場所（手元の絶対パスは書かない）。
    fn run_name(&self) -> String {
        format!("~/.agentcells/runs/{}", self.relative.display())
    }

    /// 作業するリポジトリのcloneを最新にし、仕事ごとの場所を空にして作り直す。
    fn prepare(
        &mut self,
        probe: &dyn Probe,
        read: &Credentials,
        home: &Home,
        work_repo: &RepoName,
    ) -> Result<()> {
        self.cache = update_cache(probe, read, home, work_repo)?;
        self.clean();
        let _ = std::fs::remove_dir_all(&self.run);
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
    read: &Credentials,
    home: &Home,
    repo: &RepoName,
) -> Result<PathBuf> {
    let cache = home.repos_dir().join(&repo.owner).join(&repo.name);
    let url = crate::pr::remote_url(repo);
    let mut args: Vec<String> = read.git_args();
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
    git(probe, &args, &read.git_env())?;
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
    /// この仕事の場所（worktree、一時ディレクトリ、キャッシュ）。ここで動いているプロセスは、終わったら止める
    job_dirs: &'a [PathBuf],
}

enum ClaudeEnd {
    Exited(String),
    TimedOut,
    /// AIが動かしたプロセスを止めきれなかった
    Unstopped(Error),
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
    fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            "--output-format",
            "json",
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
        args
    }

    fn env(&self) -> Vec<(String, std::ffi::OsString)> {
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
    fn mark(&self) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("{:x}{:x}", std::process::id(), nanos)
    }

    fn run(&self) -> Result<ClaudeEnd> {
        let log = |name: &str| -> Result<std::fs::File> {
            let path = self.log_dir.join(name);
            std::fs::File::create(&path)
                .map_err(|e| Error::new(format!("{} を作れません: {e}", path.display())))
        };
        let stdout_path = self.log_dir.join("claude.json");
        let mark = self.mark();
        let mut command = Command::new(self.program);
        command
            .args(self.args())
            .current_dir(self.cwd)
            .env_clear()
            .envs(self.env())
            .env(RUN_MARK_ENV, &mark)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log("claude.json")?))
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
        let timed_out = loop {
            seen.collect(pid);
            match child.try_wait() {
                Ok(Some(_)) => break false,
                Ok(None) if Instant::now() >= deadline => break true,
                // Claude CodeのBashのシェルはすぐ終わるので、短い間隔で見る
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(Error::new(format!("claudeを待てません: {e}"))),
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
        if let Err(e) = stopped {
            return Ok(ClaudeEnd::Unstopped(e));
        }
        if timed_out {
            return Ok(ClaudeEnd::TimedOut);
        }
        let stdout = std::fs::read_to_string(&stdout_path)
            .map_err(|e| Error::new(format!("claudeの出力を読めません: {e}")))?;
        Ok(ClaudeEnd::Exited(stdout))
    }
}

/// claudeの子孫に付ける、実行ごとの目印の環境変数。
const RUN_MARK_ENV: &str = "AGENTCELLS_RUN";

/// claudeの子孫のプロセス。PIDは使い回されるので、始まった時刻と組にして覚え、同じものだけを止める。
#[derive(Default)]
struct Processes {
    /// PIDと、始まった時刻（`ps -o lstart`）
    known: BTreeMap<u32, String>,
    groups: BTreeSet<u32>,
    /// `AGENTCELLS_RUN=<目印>`。環境変数にこれを持つプロセスも止める（Linuxだけ。macOSでは、
    /// sandboxの中のプロセスの環境変数を読めない）
    mark: String,
    /// この仕事の場所。作業のディレクトリ（cwd）がこの中にあるプロセスも止める
    dirs: Vec<PathBuf>,
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
fn process_table() -> Vec<(u32, u32, u32, String)> {
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
    /// 今のプロセスの一覧から、`root` の子孫（覚えているものの子孫も）と、覚えているグループのものを足す。
    /// 今の一覧を返す。
    fn collect(&mut self, root: u32) -> Vec<(u32, u32, u32, String)> {
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
    fn stop_all(&mut self, root: u32) -> Result<()> {
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
        Err(Error::new(
            "AIが動かしたプロセスを止めきれませんでした。GitHubには何も書かずに止めます",
        )
        .with_hint(
            "残ったプロセスを止めてから、Issue/PRの ac:working を外し、ラベルを付け直してください",
        ))
    }
}

/// 生きているか。ゾンビ（終わったが、親がまだ回収していない）は、生きているとみなさない。
fn is_alive(pid: u32) -> bool {
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
            job_dirs: &[],
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
            r#"cat > prompt.txt; printf '%s\n' "$@" > args.txt; echo "$PATH" > path.txt; echo "$AGENTCELLS_CONTEXT" > context.txt; env > env.txt; echo '{"structured_output":{"next":"done","comment":"ok"}}'"#,
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
        let args: Vec<String> = read("args.txt").lines().map(str::to_string).collect();
        let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(pair("--setting-sources", ""), "{args:?}");
        assert!(pair("--settings", "/s.json"), "{args:?}");
        assert!(pair("--model", "haiku"), "{args:?}");
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
            .unwrap();
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
            .unwrap();
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
            timeout: Duration::from_secs(300),
            log_dir: dir.path(),
            job_dirs: &job_dirs,
        };
        let end = run.run().unwrap();
        assert!(matches!(end, ClaudeEnd::Exited(_)));
        std::thread::sleep(Duration::from_millis(500));
        let left = Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .unwrap();
        let left = String::from_utf8_lossy(&left.stdout).trim().to_string();
        let log = std::fs::read_to_string(dir.path().join("claude.json")).unwrap_or_default();
        assert!(left.is_empty(), "left: {left}\n{log}");
        assert!(log.contains("started") || log.contains("done"), "{log}");
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

    #[test]
    fn unstopped_processes_mean_nothing_is_written_to_github() {
        use crate::probe::fake::FakeProbe;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join(".agentcells"));
        let config = Config::default();
        let probe = FakeProbe::default()
            .with("gh auth token --hostname github.com", true, "gho_test\n")
            .with_fallback(true);
        let actors = Actors {
            operator: "me".to_string(),
            writer: "me".to_string(),
        };
        let runner = Runner {
            home: &home,
            config: &config,
            probe: &probe,
            actors: &actors,
            claude: Path::new("/x"),
            agentcells: Path::new("/x"),
            scanner: Path::new("/x"),
            model: None,
        };
        let repo = RepoName::parse("octo/app").unwrap();
        let issue = Issue {
            probe: &probe,
            repo: &repo,
            number: 3,
        };
        let place = Place::new(&home, &repo, 3);
        let cell = Cell {
            name: "triage".to_string(),
            on: vec![Target::Issue],
            label: "ac:triage".to_string(),
            prompt: None,
            next: vec![],
            review: false,
            max_rounds: 3,
            timeout_minutes: 30,
        };
        let e = runner.fail(
            Failure::Unstopped(Error::new("止めきれませんでした")),
            &cell,
            &issue,
            &place,
        );
        assert!(e.message.contains("止めきれません"));
        // トークンを作るghもgitも、何も動かしていない
        assert!(
            probe.calls.borrow().is_empty(),
            "{:?}",
            probe.calls.borrow()
        );
        assert!(place.run.join("error.txt").exists());

        // ふつうの失敗なら、人間に回すコメントとラベルを書く
        runner.fail(Failure::Error(Error::new("失敗")), &cell, &issue, &place);
        let calls = probe.calls.borrow();
        assert!(
            calls.iter().any(|c| c.contains("issue comment 3")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.contains("--add-label ac:needs-human")),
            "{calls:?}"
        );
    }

    #[test]
    fn public_run_names_do_not_include_local_paths() {
        let home = Home::new(PathBuf::from("/Users/someone/.agentcells"));
        let place = Place::new(&home, &RepoName::parse("octo/app").unwrap(), 3);
        assert_eq!(place.run_name(), "~/.agentcells/runs/octo/app/3");
    }
}
