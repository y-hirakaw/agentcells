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
//! コミットしたcell（implement）は、CLIがそのブランチ全体を確かめてからpushし、IssueからならPRを作る。
//! レビューのcell（`review = true`）は、指摘をPRのレビューとして、コードの行ごとに投稿する。
//!
//! 分けたもの:
//! - `claude`: `claude -p` を、決まった引数と環境変数で動かす
//! - `process`: claudeの子孫のプロセスを見つけて止める。止めきれなければ `stuck.json` に残す
//! - `place`: 作業の場所（clone、実行の記録）
//! - `branch`: 仕事のブランチを用意し、ベースとのぶつかりを調べ、確かめてpushする。pushしない作業はbundleに残す
//! - `issue`: Issue/PRへの書き込み（ラベル、コメント、レビュー）

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::actions::{self, Context, Identity};
use crate::caches;
use crate::config::{Cell, Config, Product, Target};
use crate::error::{Error, Result};
use crate::files;
use crate::github::{self, Credentials, ISOLATED_GIT_ENV, Purpose};
use crate::guard::{self, Actors};
use crate::home::{Home, RepoName};
use crate::labels::{IN_REVIEW, NEEDS_HUMAN, WORKING};
use crate::marker;
use crate::probe::Probe;
use crate::prompt;
use crate::settings::{self, RunPaths};
use crate::tick::Work;
use crate::transition::{self, Destination, Finished, Outcome};

mod branch;
mod claude;
mod issue;
mod place;
mod process;

use branch::{BaseCheck, Branch, Pushed, check_base};
pub use claude::Reader;
use claude::{CLAUDE_LOG, ClaudeEnd, ClaudeRun, failure_kind};
use issue::{Issue, present};
use place::Place;
pub use place::claude_log;
#[cfg(test)]
pub(crate) use process::inside_cell;
pub(crate) use process::is_alive_leftover;
pub use process::{Leftover, stuck};
use process::{Stuck, command_of, current_process, stuck_path};

/// 1回の実行の結果。
#[derive(Debug)]
pub struct Report {
    /// 進めた先（`implement`、`done`、`needs-human`、見送ったときは `skipped`）
    pub destination: String,
    /// 人間に回したときや、見送ったときの理由
    pub reason: Option<String>,
    /// AIの実行そのものが失敗した（結果が返らなかった、時間切れ）。使用量の上限やログインの切れでも
    /// こうなり、ほかの仕事も同じように失敗しやすいので、tickはそこで続けるのをやめる
    pub ai_failed: bool,
    /// 何回目か（見送ったときは0）
    pub round: u32,
    /// AIが選んだ先（結果が返らなかったときは `None`）
    pub requested: Option<String>,
    /// AIの実行が失敗したときの種類（`timeout`、`unreadable`、`error:<claudeのsubtype>`、`no_output`、`bad_output`、
    /// 使用量の上限なら `rate_limit:<種類>`）
    pub failure: Option<String>,
    /// 使用量の上限に当たったときの、戻る時刻（UNIXの秒）
    pub limited_until: Option<u64>,
    /// 上限には当たっていないが、使用量が設定（`[usage]` の `pause_at`）を超えた
    pub near_limit: Option<crate::limit::Near>,
    /// sandboxが止めた通信の先（手元にだけ出す。[`crate::blocked`]）
    pub blocked: Vec<String>,
    /// betterleaksが当てたものを、通すときに `allow_secrets` に書く項目（手元にだけ出す。[`crate::allowed`]）
    pub allow_secrets: Vec<crate::config::AllowSecret>,
    /// 実行の中で作ったか、使ったPRの番号（IssueからPRに進んだとき）
    pub pr: Option<u64>,
    /// 使ったモデル（指定しなかったら `None`）
    pub model: Option<String>,
    pub usage: Usage,
}

/// claudeが結果と一緒に返す、やり取りの回数と費用の目安。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub turns: Option<u64>,
    pub cost_usd: Option<f64>,
    /// 実際に使ったモデル（`modelUsage` のキー）
    pub models: Vec<String>,
}

impl Usage {
    fn from_result(result: &serde_json::Value) -> Self {
        Usage {
            turns: result["num_turns"].as_u64(),
            cost_usd: result["total_cost_usd"].as_f64(),
            models: result["modelUsage"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default(),
        }
    }
}

/// 動かしている途中の失敗。
enum Failure {
    /// ふつうの失敗。人間に回すコメントとラベルを書く
    Error(Error),
    /// AIが動かしたプロセスを止めきれなかった。残ったプロセスにトークンを読まれうるので、
    /// トークンを作らず、GitHubには何も書かない（`ac:working` は付いたまま。人が片付ける）
    Unstopped(Vec<Leftover>),
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Error(e)
    }
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
            .cell_in(product, &work.cell)
            .ok_or_else(|| Error::new(format!("cell「{}」がありません", work.cell)))?;
        let cell = &cell;
        let issue = Issue {
            probe: self.probe,
            repo: &work.repo,
            number: work.number,
            kind: work.item,
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
            Err(failure) => Err(self.fail(failure, cell, work, &issue, &place)),
        }
    }

    /// 失敗したときの片付け。詳しいことは手元の記録（`error.txt`）とtickの出力に残し、
    /// 公開のIssueには、決まった短い文だけを書いて人間に回す。
    /// プロセスを止めきれなかったときは、トークンを作らず、GitHubには何も書かない。
    fn fail(
        &self,
        failure: Failure,
        cell: &Cell,
        work: &Work,
        issue: &Issue,
        place: &Place,
    ) -> Error {
        let _ = std::fs::create_dir_all(&place.run);
        let e = match failure {
            Failure::Error(e) => e,
            Failure::Unstopped(leftovers) => {
                let e = Error::new(format!(
                    "AIが動かしたプロセスを止めきれませんでした（PID: {}）。GitHubには何も書かずに止めます",
                    leftovers
                        .iter()
                        .map(|p| p.pid.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .with_hint(
                    "残ったプロセスを止めてから、Issue/PRの ac:working を外し、ラベルを付け直してください。それまで、tickはほかの仕事も動かしません",
                );
                let detail: Vec<String> = leftovers
                    .iter()
                    .map(|p| format!("{} {} {}", p.pid, p.started, command_of(p.pid)))
                    .collect();
                let _ = std::fs::write(
                    place.run.join("error.txt"),
                    format!(
                        "{}\n{}\n\n{}\n",
                        e.message,
                        e.hint.clone().unwrap_or_default(),
                        detail.join("\n")
                    ),
                );
                let stuck = Stuck {
                    job: place.relative.display().to_string(),
                    processes: leftovers,
                    tick: None,
                };
                if let Ok(text) = serde_json::to_string_pretty(&stuck) {
                    let _ = std::fs::write(stuck_path(self.home), text);
                }
                return e;
            }
        };
        let _ = std::fs::write(
            place.run.join("error.txt"),
            format!("{}\n{}\n", e.message, e.hint.clone().unwrap_or_default()),
        );
        let text = format!(
            "実行できませんでした。agentcellsを動かしている人は、手元の記録（{}）を確かめてください。\n\n直したら、ここにコメントすると、もう一度動きます。",
            place.run_name(),
        );
        // round 0 の目印は、往復の回数に数えない。読んだところは書かない（このあとのコメントで動かす）
        if let Ok(write) = self.write_credentials(issue.repo) {
            // 目印を書けなければ、ラベルは変えない（ac:working が付いたままになり、人が片付けるまで止まる）。
            // 待っているラベルにだけ戻すと、きっかけになった返事を、次のtickでまた読んでいないものとして拾い、
            // 同じ失敗をくり返すため
            if issue
                .comment(&write, &marker::comment(&cell.name, 0, &text))
                .is_err()
            {
                write.revoke(self.probe);
                return e;
            }
            // 付いているものだけ外す。ラベルを読めなければ、今のcellのラベルと ac:working を外す
            let remove = match issue.labels(&write) {
                Ok(labels) => present(
                    &labels,
                    &[cell.label.as_str(), WORKING, IN_REVIEW, work.label(cell)],
                    &[NEEDS_HUMAN],
                ),
                Err(_) => vec![cell.label.as_str(), WORKING],
            };
            let _ = issue.edit_labels(&write, &[NEEDS_HUMAN], &remove);
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
        // 動いている間、`agentcells status` で見えるようにする。用意（cloneなど）に時間がかかっても、
        // ac:working が付いたものを「止まったまま」と見せないよう、用意の前から書く
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        // 用意している間は、claudeの出力の場所を空にする（前の実行の claude.jsonl が残っていて、
        // 今の様子として見えないように。用意の中で消す）
        let running = |phase: Option<&str>, log: PathBuf| crate::running::Running {
            cell: cell.name.clone(),
            kind: match work.item {
                Target::Issue => "Issue",
                Target::PullRequest => "PR",
            }
            .to_string(),
            repo: work.repo.to_string(),
            number: work.number,
            title: work.title.clone(),
            started,
            log,
            tick: current_process(),
            phase: phase.map(str::to_string),
        };
        crate::running::start(self.home, &running(Some("用意しています"), PathBuf::new()));
        // 抜けるときに（見送りや失敗でも）消す。push や投稿の間も、動いているものとして見せる
        let _running = crate::running::Guard(self.home);
        let running = running(None, place.run.join(CLAUDE_LOG));
        // claudeを動かす前に作ったトークンは、動かす前にすべて取り消す。
        // 読むリポジトリ（仕事のリポジトリ、作業するリポジトリ、Issueのリポジトリ）ごとに1つ作る
        let mut reads: Vec<(RepoName, Credentials)> = Vec::new();
        for repo in [&work.repo, &work_repo, &product.issues] {
            if !reads.iter().any(|(r, _)| r == repo) {
                let credentials =
                    github::credentials(self.config, self.probe, repo, Purpose::Read)?;
                reads.push((repo.clone(), credentials));
            }
        }
        let result = self.prepare(work, product, cell, place, &reads, issue);
        for (_, credentials) in &reads {
            credentials.revoke(self.probe);
        }
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
                    ai_failed: false,
                    round: 0,
                    requested: None,
                    failure: None,
                    limited_until: None,
                    near_limit: None,
                    blocked: Vec::new(),
                    allow_secrets: Vec::new(),
                    pr: None,
                    model: None,
                    usage: Usage::default(),
                });
            }
            Prepared::Ready(ready) => ready,
        };

        crate::running::start(self.home, &running);
        let finished = ready.claude(self, cell, place).run()?;
        crate::running::set_phase(self.home, "結果をGitHubに書いています");

        // AIの動かしたものをすべて止めたあとで、書き込みのトークンを作る
        let write = self.write_credentials(&work.repo)?;
        let result = self.finish(work, product, cell, issue, place, &ready, finished, &write);
        write.revoke(self.probe);
        Ok(result?)
    }

    /// claudeを動かす前の用意。ガードで見送るときは `Skipped`。
    fn prepare(
        &self,
        work: &Work,
        product: &Product,
        cell: &Cell,
        place: &mut Place,
        reads: &[(RepoName, Credentials)],
        issue: &Issue,
    ) -> Result<Prepared> {
        let work_repo = place_repo(product)?;
        let read_for = |repo: &RepoName| -> Result<&Credentials> {
            reads
                .iter()
                .find(|(r, _)| r == repo)
                .map(|(_, c)| c)
                .ok_or_else(|| {
                    Error::new(format!("{repo} は、この仕事で読むリポジトリではありません"))
                })
        };
        let item_read = read_for(&work.repo)?;
        let work_read = read_for(&work_repo)?;
        let read = |repo: &RepoName| read_for(repo)?.reuse();
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
        let env = item_read.gh_env()?;
        let cell_labels: Vec<String> = product
            .cells
            .iter()
            .filter_map(|name| self.config.cell(name))
            .map(|c| c.label.clone())
            .collect();
        let original = match work.item {
            Target::PullRequest => prompt::closes(&task.discussion.body, &work.repo),
            Target::Issue => None,
        };
        let original_env = match &original {
            Some((repo, _)) => match read_for(repo) {
                Ok(credentials) => Some(credentials.gh_env()?),
                Err(_) => {
                    return Ok(Prepared::Skipped(format!(
                        "元のIssueのリポジトリ {repo} が、このproductにありません"
                    )));
                }
            },
            None => None,
        };
        let request = guard::Request {
            kind: work.item,
            subject: guard::Subject {
                repo: &work.repo,
                number: work.number,
                env: &env,
            },
            author: &task.discussion.author,
            label: work.label(cell),
            cell_labels: &cell_labels,
            original: original
                .as_ref()
                .zip(original_env.as_ref())
                .map(|((repo, number), env)| guard::Subject {
                    repo,
                    number: *number,
                    env,
                }),
        };
        if let Some(reason) = guard::check(self.probe, &request, self.actors)? {
            return Ok(Prepared::Skipped(reason));
        }

        place.prepare(self.probe, work_read, self.home, &work_repo)?;
        let branch = self.checkout(work, &task, place)?;
        task.worktree = Some(place.worktree.clone());
        if let Some(branch) = &branch {
            match check_base(self.probe, &place.worktree, branch) {
                BaseCheck::Conflict(conflict) => task.conflict = Some(conflict),
                // ぶつかりを直すために動かすときに、もうぶつかっていなければ（GitHubの判定が古いなど）、動かさない。
                // PRかベースの先が変わるまで、ぶつかりで動かし直さないように覚えておく
                BaseCheck::Clean { base } if work.conflict => {
                    let key = format!("{}#{}", work.repo, work.number);
                    let now = crate::github::now().unwrap_or_default();
                    crate::tick::remember_no_conflict(self.home, &key, &branch.start, &base, now);
                    return Ok(Prepared::Skipped(
                        "ベースのブランチとのぶつかりは、もうありません".to_string(),
                    ));
                }
                // GitHubがぶつかっているとしたのに、手元で調べられなければ、ファイルは分からないまま、AIに任せる
                BaseCheck::Unknown if work.conflict => {
                    task.conflict = Some(prompt::Conflict {
                        base: branch.base.clone(),
                        files: Vec::new(),
                    });
                }
                _ => {}
            }
        }

        // gitignoreされたファイルを置き、その中身のハッシュを、コミットさせないために覚えておく
        let placed = files::apply(self.home, &work_repo, &place.worktree)?;
        let base = branch
            .as_ref()
            .map_or("HEAD".to_string(), |b| format!("origin/{}", b.base));
        let wt_str = path_str(&place.worktree)?;
        let git = |args: &[&str]| {
            let mut all = vec!["-C", wt_str.as_str()];
            all.extend(args);
            git_out(self.probe, &all).ok()
        };
        task.guides = Some(prompt::read_guides(&git, &base));
        // 使うのはレビューのcellだけ。読めなければ止める（守るファイルを見落としたまま進まないように）
        if cell.review && work.item == Target::PullRequest && !product.protected.is_empty() {
            let Some(branch) = &branch else {
                return Err(Error::new("PRのブランチを読めません"));
            };
            task.protected = protected_changes(
                self.probe,
                &place.cache,
                &format!("refs/heads/{}", branch.base),
                &branch.start,
                product,
            )?;
        }
        let commands = product.commands_for(cell);
        task.commands = Some(commands.keys().cloned().collect());
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
            commands,
            placed: placed.clone(),
            placed_blobs: placed_blobs.clone(),
            identity: self.identity(item_read)?,
            scanner: self.scanner.to_path_buf(),
            tmp: place.tmp.clone(),
            caches: cache_dirs.clone(),
            base: branch.as_ref().map(|b| b.base.clone()),
            network: product.sandbox.network.clone(),
            commands_limited: cell.commands.is_some(),
            allow_secrets: product.allow_secrets.clone(),
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
            agentcells: self.agentcells,
            tmp: &place.tmp,
            caches: &cache_list,
        };
        let settings =
            settings::runtime_settings(self.home, self.config, product, cell, &run_paths)?;
        let settings_path = place.run.join("settings.json");
        write_json(&settings_path, &settings)?;

        let extra = cell.extra_prompt()?;
        let asked = crate::discussion::asked_by(&task.discussion.posts, &task.writer, &cell.name);
        let replied_in = transition::replied_in(work.waiting, work.conflict);
        Ok(Prepared::Ready(Box::new(Ready {
            user: prompt::user_prompt(cell, extra.as_deref(), Some(&task)),
            system: prompt::system_prompt(self.config, cell, replied_in, asked),
            schema: transition::output_schema(cell, replied_in, asked),
            settings_path,
            tools: settings::cell_tools(cell),
            context_path,
            labels_before: issue.labels(item_read)?,
            job_dirs: [
                place.worktree.clone(),
                place.git_dir.clone(),
                place.tmp.clone(),
            ]
            .into_iter()
            .chain(cache_list)
            .collect(),
            branch,
            placed_blobs,
            allow_secrets: product.allow_secrets.clone(),
            task,
        })))
    }

    /// claudeが終わったあと。結果を読み、pushとPRの作成、投稿をして、ラベルを付け替える。
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
        let asked = crate::discussion::asked_by(&task.discussion.posts, &task.writer, &cell.name);
        let round = task.round(&cell.name);
        let runs = |name: &str| task.runs_since_operator(name);
        let read_until = task.read_until();

        // AIが決めた結果を読む。読めなければ、人間に回す
        let mut overridden: Option<String> = None;
        let mut ai_failed = false;
        let mut usage = Usage::default();
        // AIの実行が失敗したときの種類（決まった語だけ。inspectorが、自由な文を読まずに数えるため）
        let mut failure = None;
        let outcome = match finished {
            ClaudeEnd::TimedOut => {
                failure = Some("timeout".to_string());
                Err(format!(
                    "時間の上限（{}分）を超えたので、止めました",
                    cell.timeout_minutes
                ))
            }
            ClaudeEnd::Exited(stdout) => match serde_json::from_str::<serde_json::Value>(&stdout) {
                Ok(value) => {
                    usage = Usage::from_result(&value);
                    let outcome = Outcome::from_result(&value);
                    if outcome.is_err() {
                        failure = Some(failure_kind(&value));
                    }
                    outcome
                }
                Err(e) => {
                    failure = Some("unreadable".to_string());
                    Err(format!("AIの出力を読めません: {e}"))
                }
            },
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(reason) => {
                overridden = Some(reason);
                ai_failed = true;
                Outcome {
                    next: transition::NEEDS_HUMAN.to_string(),
                    comment: String::new(),
                    findings: Vec::new(),
                    issues: Vec::new(),
                }
            }
        };
        // 使用量の上限に当たったなら、人間には回さない。ac:working だけを外して元のラベルに戻し、
        // 上限が戻ってから、同じ仕事をまた動かす（目印のコメントを書かないので、往復の回数にも数えない）
        let limit = crate::limit::last_event(&place.run.join(CLAUDE_LOG));
        // claudeがAPIの状態の番号として429を返したときも、上限とみなす
        let too_many = failure.as_deref().is_some_and(|f| f.ends_with(":429"));
        if ai_failed && (limit.as_ref().is_some_and(|l| l.rejected()) || too_many) {
            let kind: String = limit
                .as_ref()
                .and_then(|l| l.kind.as_deref())
                .unwrap_or("unknown")
                .chars()
                .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
                .take(40)
                .collect();
            // 動いている間に人が外していれば、外さない（外すのに失敗して、人間に回らないように）
            issue.edit_labels(write, &[], &present(&labels_after, &[WORKING], &[]))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            return Ok(Report {
                destination: "paused".to_string(),
                reason: Some(format!("使用量の上限（{kind}）に当たりました")),
                ai_failed: true,
                round,
                requested: None,
                failure: Some(format!("rate_limit:{kind}")),
                limited_until: Some(crate::limit::until(limit.and_then(|l| l.resets_at), now)),
                near_limit: None,
                blocked: Vec::new(),
                allow_secrets: Vec::new(),
                pr: None,
                model: self.model.or(cell.model.as_deref()).map(str::to_string),
                usage,
            });
        }
        if let Some(changed) = transition::tampered(&ready.labels_before, &labels_after) {
            overridden = Some(changed);
        }

        // コミットを確かめてpushし、IssueからならPRを作る（レビューのcellと、着手を確かめるcellはpushしない）
        let mut pr_number: Option<u64> = None;
        let mut kept_branch: Option<(String, Option<u64>)> = None;
        let mut refused_allow: Vec<crate::config::AllowSecret> = Vec::new();
        // CLIが人間に回すとき（時間切れ、ラベルの変化など）は、pushしない。それでも、AIの作業が作業場所ごと
        // 消えないように、コミットがあれば手元に残す
        if overridden.is_some()
            && !cell.review
            && !cell.confirms()
            && let Some(branch) = &ready.branch
            && let Some(note) = self.keep_unpushed(place, branch)
            && let Some(reason) = &mut overridden
        {
            reason.push_str(&format!("\n\n{note}"));
        }
        if overridden.is_none()
            && !cell.review
            && !cell.confirms()
            && let Some(branch) = &ready.branch
        {
            match self.push(
                work, cell, task, place, ready, branch, &outcome, round, write,
            )? {
                Pushed::Nothing => {
                    if work.item == Target::PullRequest
                        && outcome.next != transition::NEEDS_HUMAN
                        && outcome.next != transition::DONE
                        && outcome.next != transition::STOP
                    {
                        overridden = Some(format!(
                            "新しいコミットがないので「{}」には進めません（同じものを何度もレビューしないため）。直すことがなかったのなら、ここにそう書くか、ラベルを付け替えてください。直してほしいことがあれば、コメントで頼むと、また動きます",
                            outcome.next
                        ));
                    }
                }
                Pushed::Refused(reason, allow) => {
                    overridden = Some(reason);
                    refused_allow = allow;
                }
                Pushed::Done(number) => pr_number = number,
                Pushed::Branch(name, open) => kept_branch = Some((name, open)),
            }
        }

        let requested = if overridden.is_some() {
            transition::NEEDS_HUMAN.to_string()
        } else {
            outcome.next.clone()
        };
        let plan_for = |requested: &str, created_pr: bool| {
            transition::plan(
                self.config,
                product,
                &Finished {
                    cell,
                    item: work.item,
                    created_pr,
                    requested,
                    waiting: work.waiting,
                    conflict: work.conflict,
                    asked,
                },
                &runs,
            )
        };
        let mut plan = plan_for(&requested, pr_number.is_some());
        let mut reason = overridden.or(plan.overridden.clone());
        let mut text = outcome.comment.trim().to_string();
        if let Some(number) = pr_number
            && work.item == Target::Issue
        {
            text.push_str(&format!("\n\nPR: #{number}"));
        }
        match &kept_branch {
            Some((name, Some(number))) => text.push_str(&format!(
                "\n\nここまでの作業は、ブランチ `{name}`（PR #{number}）にpushしました。次に動くときは、そこから続けます"
            )),
            Some((name, None)) => text.push_str(&format!(
                "\n\nここまでの作業は、ブランチ `{name}` にpushしました（PRはまだ作っていません）。次に動くときは、そこから続けます"
            )),
            None => {}
        }
        // 作業と関係のない、元からある問題は、別のIssueにする（直すかどうかは人が決める）
        let filed = self.file_found(product, work, cell, &task.writer, &outcome.issues, write);
        if !filed.is_empty() {
            text.push_str("\n\n");
            text.push_str(&filed.join("\n"));
        }
        // sandboxが止めた通信の先を拾い、手元にだけ出す。人間に回すときは、公開のコメントに止めたことだけを書く
        let blocked = crate::blocked::hosts(&place.run.join(CLAUDE_LOG));
        if !blocked.is_empty() && plan.destination == transition::Destination::NeedsHuman {
            text.push_str("\n\n");
            text.push_str(crate::blocked::COMMENT);
        }
        // betterleaksが当てたものを通すときに書く項目も、手元にだけ出す（中身のハッシュが入るため）
        let mut allow_secrets = refused_allow;
        for entry in crate::allowed::entries(&place.run.join(CLAUDE_LOG)) {
            if !allow_secrets.contains(&entry) {
                allow_secrets.push(entry);
            }
        }
        if !allow_secrets.is_empty() && plan.destination == transition::Destination::NeedsHuman {
            text.push_str("\n\n");
            text.push_str(crate::allowed::COMMENT);
        }
        if let Some(reason) = &reason {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("人間に回しました: {reason}"));
        }
        // 守るファイルを変えたPRを検品に回すときは、AIのレビューだけで済ませないよう、CLIが書き足す
        if cell.review
            && reason.is_none()
            && plan.destination == Destination::Done
            && let Some(note) = protected_note(&task.protected)
        {
            text.push_str("\n\n");
            text.push_str(&note);
        }

        // 着手を確かめるcellが、自分で人間に確かめたなら、そう印を付ける（このあとの返事でだけ先へ進める）
        let asking = cell.confirms()
            && reason.is_none()
            && plan.destination == transition::Destination::NeedsHuman;
        // レビューのcellは、指摘を行ごとにPRのレビューとして投稿する。それ以外はコメントにする
        // 最後の行に、読んだやり取りがどこまでかを書く（動いている間に届いたコメントで、また動かすため）
        let review = (cell.review && work.item == Target::PullRequest && reason.is_none())
            .then(|| self.review_payload(cell, round, &text, &outcome.findings, task, place))
            .transpose()?
            .map(|mut payload| {
                let body = payload["body"].as_str().unwrap_or_default();
                payload["body"] = marker::with_read_until(body, &read_until, asking).into();
                payload
            });
        let posted = match &review {
            Some(payload) => payload.to_string(),
            None => marker::with_read_until(
                &marker::comment(&cell.name, round, &text),
                &read_until,
                asking,
            ),
        };
        // 投稿する本文の全体を、秘密らしいものがないか調べる。あれば、決まった文だけにして人間に回す
        let found = actions::scan_text(self.scanner, &posted)?;
        if found.is_empty() {
            match &review {
                Some(payload) => issue.review(write, payload)?,
                None => issue.comment(write, &posted)?,
            }
        } else {
            let rules: Vec<String> = found.iter().map(|(_, rule)| rule.clone()).collect();
            let note = format!(
                "コメントに秘密らしいもの（{}）があったので、投稿しませんでした。agentcellsを動かしている人は、手元の記録（{}）を確かめてください",
                rules.join("、"),
                place.run_name()
            );
            let body = marker::with_read_until(
                &marker::comment(&cell.name, round, &format!("人間に回しました: {note}")),
                &read_until,
                false,
            );
            issue.comment(write, &body)?;
            plan = plan_for(transition::NEEDS_HUMAN, pr_number.is_some());
            reason = Some(note);
        }

        // 付けてから外す（guardが、ac:working が付いている間のラベルをagentcellsの付け替えとみなすため）
        if let Some(number) = pr_number
            && !plan.add_to_created_pr.is_empty()
        {
            let created = Issue {
                probe: self.probe,
                repo: &work.repo,
                number,
                kind: Target::PullRequest,
            };
            let add: Vec<&str> = plan.add_to_created_pr.iter().map(String::as_str).collect();
            created.edit_labels(write, &add, &[])?;
        }
        let add: Vec<&str> = plan.add.iter().map(String::as_str).collect();
        // 人間を待っていたラベルが残っていれば（cellのラベルを付け直して動かしたときなど）、それも外す
        let mut remove: Vec<&str> = plan.remove.iter().map(String::as_str).collect();
        remove.extend([NEEDS_HUMAN, IN_REVIEW]);
        let remove = present(&labels_after, &remove, &add);
        issue.edit_labels(write, &add, &[])?;
        issue.edit_labels(write, &[], &remove)?;
        if plan.close {
            issue.close(write)?;
        }
        Ok(Report {
            destination: match &plan.destination {
                Destination::Cell(name) => name.clone(),
                Destination::Done => transition::DONE.to_string(),
                Destination::Stop => transition::STOP.to_string(),
                Destination::NeedsHuman => transition::NEEDS_HUMAN.to_string(),
            },
            reason,
            ai_failed,
            round,
            requested: (!ai_failed).then(|| outcome.next.clone()),
            failure,
            limited_until: None,
            blocked,
            allow_secrets,
            near_limit: limit.as_ref().and_then(|l| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or_default();
                crate::limit::near(l, self.config.usage.pause_at, now)
            }),
            pr: pr_number,
            model: self.model.or(cell.model.as_deref()).map(str::to_string),
            usage,
        })
    }

    /// AIが見つけた、作業と関係のない問題を、productのIssueのリポジトリに出す（[`crate::found`]）。
    /// 書き込みのトークンは仕事のリポジトリだけに絞って作るので、Issueのリポジトリが違えば、別に作って取り消す。
    fn file_found(
        &self,
        product: &Product,
        work: &Work,
        cell: &Cell,
        writer: &str,
        issues: &[transition::NewIssue],
        write: &Credentials,
    ) -> Vec<String> {
        if issues.is_empty() {
            return Vec::new();
        }
        let other = (product.issues != work.repo).then(|| self.write_credentials(&product.issues));
        let credentials = match &other {
            None => write,
            Some(Ok(credentials)) => credentials,
            Some(Err(e)) => {
                return vec![format!(
                    "別のIssueにする問題がありましたが、{} に書き込めないので、出しませんでした（{}）",
                    product.issues, e.message
                )];
            }
        };
        let lines = crate::found::file(
            self.probe,
            credentials,
            writer,
            &product.issues,
            &format!("{}#{}", work.repo, work.number),
            &cell.name,
            issues,
            &|text| actions::scan_text(self.scanner, text).map(|found| !found.is_empty()),
        );
        if let Some(Ok(credentials)) = &other {
            credentials.revoke(self.probe);
        }
        lines
    }

    /// PRのレビューとして送る内容。差分の中の行への指摘は、行コメントにする。
    fn review_payload(
        &self,
        cell: &Cell,
        round: u32,
        summary: &str,
        findings: &[crate::review::Finding],
        task: &prompt::Task,
        place: &Place,
    ) -> Result<serde_json::Value> {
        let d = &task.discussion;
        let (Some(base), Some(head)) = (&d.base, &d.head) else {
            return Err(Error::new("PRのブランチを読めません"));
        };
        // CLIだけが書くbareのキャッシュで、差分を読む
        let range = format!("refs/heads/{base}...refs/heads/{head}");
        let diff = git_out(
            self.probe,
            &[
                "--git-dir",
                &path_str(&place.cache)?,
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                &range,
            ],
        )?;
        let lines = crate::review::DiffLines::parse(&diff);
        Ok(crate::review::payload(
            &cell.name, round, summary, findings, &lines,
        ))
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

/// PRが変えた（足した、消した、名前を変えた）ファイルのうち、守るもの。差分は、CLIだけが書くbareのキャッシュで、
/// ベースと、worktreeに取り出したコミット（`head`。AIが見るもの）の間で読む。
fn protected_changes(
    probe: &dyn Probe,
    cache: &Path,
    base: &str,
    head: &str,
    product: &Product,
) -> Result<Vec<String>> {
    let range = format!("{base}...{head}");
    // 名前を変えたときに、元の名前も出す（守るファイルを動かして外すのも、変えたうちに入れる）
    let out = git_out(
        probe,
        &[
            "--git-dir",
            &path_str(cache)?,
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            &range,
        ],
    )?;
    Ok(out
        .split('\0')
        .filter(|path| !path.is_empty() && product.protects(path))
        .map(str::to_string)
        .collect())
}

/// 守るファイルを変えたPRを検品に回すときに、CLIが書き足す文。変えていなければ `None`。
fn protected_note(files: &[String]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let listed: Vec<String> = files
        .iter()
        .map(|f| format!("`{}`", crate::display::printable(f).replace('`', "'")))
        .collect();
    Some(format!(
        "このPRは、守るファイル（agentcellsの設定の protected）を変えています: {}。AIのレビューだけで済ませず、マージの前に人が差分を読んでください",
        listed.join("、")
    ))
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
    /// 使えるツール（`--tools`）
    tools: String,
    context_path: PathBuf,
    labels_before: Vec<String>,
    task: prompt::Task,
    /// worktree、一時ディレクトリ、キャッシュ
    job_dirs: Vec<PathBuf>,
    /// 仕事のブランチ
    branch: Option<Branch>,
    /// 置いた秘密のファイルの中身のハッシュ（pushの前の確かめに使う）
    placed_blobs: Vec<String>,
    /// betterleaksが当てても通すもの（productの `allow_secrets`）。pushの前の確かめに使う
    allow_secrets: Vec<crate::config::AllowSecret>,
}

impl Ready {
    fn claude<'a>(&'a self, runner: &'a Runner, cell: &'a Cell, place: &'a Place) -> ClaudeRun<'a> {
        ClaudeRun {
            program: runner.claude,
            cwd: &place.worktree,
            settings: &self.settings_path,
            system: &self.system,
            schema: &self.schema,
            prompt: &self.user,
            context: &self.context_path,
            path_prefix: runner.agentcells.parent(),
            // 試すときの指定（AGENTCELLS_MODEL）を、cellの設定より先にする
            model: runner.model.or(cell.model.as_deref()),
            tools: Some(&self.tools),
            timeout: Duration::from_secs(u64::from(cell.timeout_minutes) * 60),
            log_dir: &place.run,
            job_dirs: &self.job_dirs,
            record: Some((
                stuck_path(runner.home),
                place.relative.display().to_string(),
            )),
        }
    }
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| Error::new(format!("{} を書けません: {e}", path.display())))?;
    std::fs::write(path, text)
        .map_err(|e| Error::new(format!("{} を書けません: {e}", path.display())))
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

/// gitを、使う人とシステムの設定を読まずに動かし、標準出力を返す。
fn git_out(probe: &dyn Probe, args: &[&str]) -> Result<String> {
    let out = probe
        .run_with_env("git", args, ISOLATED_GIT_ENV)
        .ok_or_else(|| {
            Error::new("gitが見つかりません").with_hint("`agentcells doctor` で確かめてください")
        })?;
    if out.success {
        Ok(out.stdout)
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

#[cfg(all(test, unix))]
mod tests {
    use super::process::process_table;
    use super::*;

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
            kind: Target::Issue,
        };
        let place = Place::new(&home, &repo, 3);
        let cell = Cell {
            name: "triage".to_string(),
            on: vec![Target::Issue],
            label: "ac:triage".to_string(),
            description: None,
            prompt: None,
            next: vec![],
            review: false,
            max_rounds: 3,
            timeout_minutes: 30,
            model: None,
            confirm: None,
            commands: None,
            web_search: false,
            from_file: false,
        };
        let work = Work {
            product: "app".to_string(),
            cell: "triage".to_string(),
            item: Target::Issue,
            repo: repo.clone(),
            number: 3,
            title: "t".to_string(),
            waiting: None,
            conflict: false,
        };
        // 止めきれなかったものとして、今動いているこのテストのプロセスを渡す
        let me = std::process::id();
        // cellのsandboxの中では、プロセスの一覧（ps）が見えないので飛ばす。外で見えなければ、壊れたとして落とす
        let Some(started) = process_table().into_iter().find(|r| r.0 == me).map(|r| r.3) else {
            assert!(
                inside_cell(),
                "ps の一覧に、このテストのプロセスが見えません"
            );
            eprintln!("cellのsandboxでは、プロセスの一覧が見えないので、飛ばします");
            return;
        };
        let e = runner.fail(
            Failure::Unstopped(vec![Leftover {
                pid: me,
                started: started.clone(),
            }]),
            &cell,
            &work,
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
        // 残っている間は、tickが動かないよう目印を残す
        let note = stuck(&home).unwrap();
        assert!(note.contains(&me.to_string()), "{note}");
        assert!(note.contains("octo/app/3"), "{note}");
        // 残ったプロセスがもうなければ、目印は消える
        std::fs::write(
            stuck_path(&home),
            r#"{"job":"octo/app/3","processes":[{"pid":1,"started":"never"}]}"#,
        )
        .unwrap();
        assert!(stuck(&home).is_none());
        assert!(!stuck_path(&home).exists());
        // 動いている間の目印は、書いたtickが動いていれば、残ったものとみなさない。tickが殺されていれば、止める
        let alive = format!(r#"{{"pid":{me},"started":"{started}"}}"#);
        let running =
            |tick: &str| format!(r#"{{"job":"octo/app/3","processes":[{alive}],"tick":{tick}}}"#);
        std::fs::write(stuck_path(&home), running(&alive)).unwrap();
        assert!(stuck(&home).is_none());
        assert!(stuck_path(&home).exists());
        std::fs::write(stuck_path(&home), running(r#"{"pid":1,"started":"never"}"#)).unwrap();
        assert!(stuck(&home).unwrap().contains(&me.to_string()));
        std::fs::remove_file(stuck_path(&home)).unwrap();

        // ふつうの失敗なら、人間に回すコメントとラベルを書く
        runner.fail(
            Failure::Error(Error::new("失敗")),
            &cell,
            &work,
            &issue,
            &place,
        );
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
    fn protected_files_a_pr_changes_are_found() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let cache = dir.path().join("cache.git");
        let run = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::create_dir_all(work.join("prompts")).unwrap();
        for f in [
            "src/hook.rs",
            "src/main.rs",
            "prompts/common.md",
            "prompts.md",
        ] {
            std::fs::write(work.join(f), "a\n").unwrap();
        }
        run(dir.path(), &["init", "-q", "-b", "main", "work"]);
        run(&work, &["add", "."]);
        run(&work, &["commit", "-qm", "base"]);
        run(&work, &["checkout", "-qb", "ac/1"]);
        // 名前を変えて守りから外す、ディレクトリの下を変える、守らないものを変える
        run(&work, &["mv", "src/hook.rs", "src/hook_old.rs"]);
        std::fs::write(work.join("prompts/common.md"), "b\n").unwrap();
        std::fs::write(work.join("prompts.md"), "b\n").unwrap();
        std::fs::write(work.join("src/main.rs"), "b\n").unwrap();
        run(&work, &["commit", "-qam", "change"]);
        run(dir.path(), &["clone", "-q", "--bare", "work", "cache.git"]);

        let mut product: Product = toml::from_str(
            "name = \"app\"\nissues = \"octo/app\"\nrepos = [\"octo/app\"]\ncells = []\nprotected = [\"src/hook.rs\", \"prompts/\"]",
        )
        .unwrap();
        let probe = crate::probe::SystemProbe;
        let changes = |product: &Product| {
            protected_changes(
                &probe,
                &cache,
                "refs/heads/main",
                "refs/heads/ac/1",
                product,
            )
            .unwrap()
        };
        assert_eq!(changes(&product), vec!["prompts/common.md", "src/hook.rs"]);
        // 大文字と小文字は区別しない（macOSでは同じファイルになる）。/ がなくてもディレクトリに当たる
        product.protected = vec!["SRC/Main.rs/".to_string(), "Prompts".to_string()];
        assert_eq!(changes(&product), vec!["prompts/common.md", "src/main.rs"]);
    }

    #[test]
    fn the_note_lists_protected_files() {
        assert_eq!(protected_note(&[]), None);
        let note = protected_note(&["src/hook.rs".to_string(), "a\u{1b}b".to_string()]).unwrap();
        assert!(note.contains("`src/hook.rs`、`a"), "{note}");
        assert!(note.contains("人が差分を読んで"), "{note}");
        assert!(!note.contains('\u{1b}'), "{note}");
    }
}
