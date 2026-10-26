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
use crate::labels::{IN_REVIEW, NEEDS_HUMAN, WORKING};
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

/// claudeの結果から、失敗の種類を決まった語で返す。claudeの `subtype` は、英小文字と _ だけを使う。
fn failure_kind(result: &serde_json::Value) -> String {
    if result["is_error"].as_bool() == Some(true) {
        let subtype: String = result["subtype"]
            .as_str()
            .unwrap_or("unknown")
            .chars()
            .filter(|c| c.is_ascii_lowercase() || *c == '_')
            .take(40)
            .collect();
        // 使用量の上限などは、APIの状態の番号（429など）で見分けられる
        match result["api_error_status"].as_u64() {
            Some(status) => format!("error:{subtype}:{status}"),
            None => format!("error:{subtype}"),
        }
    } else if result["structured_output"].is_null() {
        "no_output".to_string()
    } else {
        "bad_output".to_string()
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
struct Stuck {
    /// どの仕事か（`<owner>/<repo>/<番号>`）
    job: String,
    processes: Vec<Leftover>,
    /// claudeが動いている間に書いたときの、tick自身。これが生きていれば、まだ動いている途中で、残ったものではない
    #[serde(default)]
    tick: Option<Leftover>,
}

/// そのプロセス（PIDと始まった時刻が同じもの）が、まだ動いているか。
pub(crate) fn is_alive_leftover(p: &Leftover) -> bool {
    process_table()
        .iter()
        .any(|r| r.0 == p.pid && r.3 == p.started)
}

/// 今のプロセス（tick自身）。
fn current_process() -> Option<Leftover> {
    let me = std::process::id();
    process_table()
        .into_iter()
        .find(|r| r.0 == me)
        .map(|r| Leftover {
            pid: me,
            started: r.3,
        })
}

fn stuck_path(home: &Home) -> PathBuf {
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
                    pr: None,
                    model: None,
                    usage: Usage::default(),
                });
            }
            Prepared::Ready(ready) => ready,
        };

        // 動いている間、`agentcells status` で見えるようにする
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        crate::running::start(
            self.home,
            &crate::running::Running {
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
                log: place.run.join(CLAUDE_LOG),
                tick: current_process(),
                phase: None,
            },
        );
        // 抜けるときに（失敗でも）消す。push や投稿の間も、動いているものとして見せる
        let _running = crate::running::Guard(self.home);
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
            placed_blobs: placed_blobs.clone(),
            identity: self.identity(item_read)?,
            scanner: self.scanner.to_path_buf(),
            tmp: place.tmp.clone(),
            caches: cache_dirs.clone(),
            base: branch.as_ref().map(|b| b.base.clone()),
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
        let settings = settings::runtime_settings(self.home, self.config, product, &run_paths)?;
        let settings_path = place.run.join("settings.json");
        write_json(&settings_path, &settings)?;

        let extra = match &cell.prompt {
            Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
                Error::new(format!("追加の指示 {} を読めません: {e}", path.display()))
            })?),
            None => None,
        };
        let asked = crate::discussion::asked_by(&task.discussion.posts, &task.writer, &cell.name);
        let replied_in = transition::replied_in(work.waiting, work.conflict);
        Ok(Prepared::Ready(Box::new(Ready {
            user: prompt::user_prompt(cell, extra.as_deref(), Some(&task)),
            system: prompt::system_prompt(cell, replied_in, asked),
            schema: transition::output_schema(cell, replied_in, asked),
            settings_path,
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
            task,
        })))
    }

    /// 作業の場所で、仕事のブランチを用意する（claudeを動かす前の、CLIが作ったばかりのcloneで行う）。
    /// - Issue: `ac/<owner>-<repo>-<番号>`。前の実行でpushしていれば、その続きから始める
    /// - PR: PRのブランチ。フォークからのPRは動かさない
    /// - レビューのcellも、PRのブランチを見る（pushはしない）
    fn checkout(&self, work: &Work, task: &prompt::Task, place: &Place) -> Result<Option<Branch>> {
        let wt = path_str(&place.worktree)?;
        let (name, base) = match work.item {
            Target::Issue => {
                let base = git_out(self.probe, &["-C", &wt, "symbolic-ref", "--short", "HEAD"])?;
                (
                    crate::pr::branch_name(&work.repo, work.number),
                    base.trim().to_string(),
                )
            }
            Target::PullRequest => {
                let d = &task.discussion;
                let (Some(head), Some(base)) = (&d.head, &d.base) else {
                    return Err(Error::new("PRのブランチを読めません"));
                };
                if d.head_repo.as_deref() != Some(&work.repo.to_string()) {
                    return Err(Error::new(format!(
                        "フォーク（{}）からのPRは、動かせません",
                        d.head_repo.as_deref().unwrap_or("不明")
                    )));
                }
                (head.clone(), base.clone())
            }
        };
        let remote = format!("refs/remotes/origin/{name}");
        let exists = git_out(
            self.probe,
            &["-C", &wt, "rev-parse", "-q", "--verify", &remote],
        )
        .is_ok();
        if work.item == Target::PullRequest && !exists {
            return Err(Error::new(format!("PRのブランチ {name} が見つかりません")));
        }
        let mut args = vec![
            "-C",
            wt.as_str(),
            "-c",
            "core.hooksPath=/dev/null",
            "checkout",
            "-q",
            "-B",
            name.as_str(),
        ];
        let origin = format!("origin/{name}");
        if exists {
            args.push(origin.as_str());
        }
        git_out(self.probe, &args)?;
        let start = git_out(self.probe, &["-C", &wt, "rev-parse", "HEAD"])?
            .trim()
            .to_string();
        Ok(Some(Branch { name, base, start }))
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
                Pushed::Refused(reason) => overridden = Some(reason),
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
        if let Some(reason) = &reason {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("人間に回しました: {reason}"));
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
            pr: pr_number,
            model: self.model.or(cell.model.as_deref()).map(str::to_string),
            usage,
        })
    }

    /// pushしなかったAIの作業（コミット）を、手元に残し、取り出し方を返す。実行の記録の場所は、同じIssue/PRの
    /// 次の実行で空になるので、別の場所（`kept/`）に、時刻を付けて残す。
    fn keep_work(&self, place: &Place, staging: &Path, branch: &Branch) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let kept = self.home.root().join("kept");
        prune_kept(&kept, now);
        let name = format!("{}/{now}.bundle", place.relative.display());
        match keep_bundle(self.probe, staging, branch, &kept, &kept.join(&name)) {
            Ok(()) => format!(
                "AIの作業（コミット）は、agentcellsを動かしている人の手元の ~/.agentcells/kept/{name} に残しました（{KEPT_DAYS}日で消します）。作業するリポジトリで `git fetch ~/.agentcells/kept/{name} {}:ai-work` を実行すると、ブランチ ai-work として取り出せます",
                branch.name
            ),
            Err(e) => format!(
                "AIの作業（コミット）を手元に残せませんでした: {}",
                e.message
            ),
        }
    }

    /// pushしないで終わるとき、AIが足したコミットがあれば手元に残し、取り出し方を返す。なければ `None`。
    fn keep_unpushed(&self, place: &Place, branch: &Branch) -> Option<String> {
        let staging = crate::pr::stage(self.probe, &place.worktree, &branch.name).ok()?;
        let staging_path = path_str(staging.path()).ok()?;
        let range = format!("{}..refs/heads/{}", branch.start, branch.name);
        let count = git_out(
            self.probe,
            &["-C", &staging_path, "rev-list", "--count", &range],
        )
        .ok()?;
        (count.trim() != "0").then(|| self.keep_work(place, staging.path(), branch))
    }

    /// AIが足したコミットを確かめてpushする。IssueからでPRに進むなら、PRを作る（あれば、それを使う）。
    /// 確かめもpushも、CLIがその場で作ったbareリポジトリから行う（AIが書き換えられる .git/config を読まない）。
    #[allow(clippy::too_many_arguments)]
    fn push(
        &self,
        work: &Work,
        cell: &Cell,
        task: &prompt::Task,
        place: &Place,
        ready: &Ready,
        branch: &Branch,
        outcome: &Outcome,
        round: u32,
        write: &Credentials,
    ) -> Result<Pushed> {
        let staging = crate::pr::stage(self.probe, &place.worktree, &branch.name)?;
        let staging_path = path_str(staging.path())?;
        let mut env = ISOLATED_GIT_ENV.to_vec();
        env.push(("GIT_ALLOW_PROTOCOL", "file"));
        let base_ref = format!("+refs/heads/{0}:refs/heads/{0}", branch.base);
        git(
            self.probe,
            &[
                "-C",
                &staging_path,
                "fetch",
                "-q",
                "--no-tags",
                &path_str(&place.cache)?,
                &base_ref,
            ],
            &env,
        )?;
        let range = format!("{}..refs/heads/{}", branch.start, branch.name);
        let count = git_out(
            self.probe,
            &["-C", &staging_path, "rev-list", "--count", &range],
        )?;
        if count.trim() == "0" {
            return Ok(Pushed::Nothing);
        }
        // IssueからPRに進まないcell（triageなど）が足したコミットは、pushしない
        let wants_pr = work.item == Target::Issue
            && self
                .config
                .cell(&outcome.next)
                .is_some_and(|next| !next.on.contains(&Target::Issue));
        // PRを作るcell（implementなど。PRを扱うcellに進めるもの）が、途中で人間に回すときは、PRは作らずに
        // ブランチだけをpushする。pushしないと、作業場所ごと消えて、次の実行が最初からやり直しになるため
        let keeps_branch = work.item == Target::Issue
            && outcome.next == transition::NEEDS_HUMAN
            && cell.next.iter().any(|n| {
                self.config
                    .cell(n)
                    .is_some_and(|next| !next.on.contains(&Target::Issue))
            });
        if work.item == Target::Issue && !wants_pr && !keeps_branch {
            return Ok(Pushed::Nothing);
        }
        if let Some(refusal) = crate::pr::verify(
            self.probe,
            staging.path(),
            &branch.start,
            &format!("refs/heads/{}", branch.base),
            &branch.name,
            &ready.placed_blobs,
            self.scanner,
        )? {
            // pushしない作業も、人が読んだり取り出したりできるように、手元に残す（作業場所は消えるため）
            let text = format!(
                "pushしませんでした: {}\n\n次にすること: {}\n\n{}",
                refusal.reason,
                refusal.next,
                self.keep_work(place, staging.path(), branch)
            );
            return Ok(Pushed::Refused(text));
        }
        crate::pr::push_staged(self.probe, write, staging.path(), &work.repo, &branch.name)?;
        if work.item == Target::PullRequest {
            return Ok(Pushed::Done(None));
        }
        if keeps_branch {
            // 同じブランチのPRが開いていれば（人がPRのあるIssueに付け直したときなど）、そのPRに足したことになる
            let open = crate::pr::find_open(
                self.probe,
                write,
                &work.repo,
                &branch.name,
                &self.actors.writer,
            )?;
            return Ok(Pushed::Branch(branch.name.clone(), open));
        }
        let number = match crate::pr::find_open(
            self.probe,
            write,
            &work.repo,
            &branch.name,
            &self.actors.writer,
        )? {
            Some(number) => number,
            None => {
                let body = crate::pr::body(
                    &cell.name,
                    round,
                    outcome.comment.trim(),
                    &work.repo,
                    work.number,
                );
                let found = actions::scan_text(self.scanner, &body)?;
                if !found.is_empty() {
                    return Ok(Pushed::Refused(format!(
                        "PRの本文に秘密らしいものがあったので、PRを作りませんでした（ブランチ {} はpushしました）。agentcellsを動かしている人は、手元の記録（{}）を確かめてください",
                        branch.name,
                        place.run_name()
                    )));
                }
                let args = crate::pr::create_args(
                    &work.repo,
                    &branch.base,
                    &branch.name,
                    &task.discussion.title,
                    &body,
                    &self.actors.operator,
                );
                crate::pr::create(self.probe, write, &args)?
            }
        };
        Ok(Pushed::Done(Some(number)))
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
    /// 仕事のブランチ
    branch: Option<Branch>,
    /// 置いた秘密のファイルの中身のハッシュ（pushの前の確かめに使う）
    placed_blobs: Vec<String>,
}

/// 仕事のブランチ。
struct Branch {
    name: String,
    /// PRのベース（Issueなら、既定のブランチ）
    base: String,
    /// 動かす前のブランチの先（ここから先が、AIが足したコミット）
    start: String,
}

/// pushの結果。
enum Pushed {
    /// 新しいコミットがない（か、pushしないcell）
    Nothing,
    /// 確かめで止めた。人間に回す理由
    Refused(String),
    /// pushした。IssueからならPRの番号
    Done(Option<u64>),
    /// 人間に回すので、PRは作らずに、ブランチだけをpushした（次の実行は、そこから続ける）。
    /// そのブランチのPRが開いていれば、その番号
    Branch(String, Option<u64>),
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
            tools: None,
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

/// GitHubのIssue/PRへの読み書き。トークンは、そのたびに渡す（作る時を、AIが動いていないときに限るため）。
struct Issue<'a> {
    probe: &'a dyn Probe,
    repo: &'a RepoName,
    number: u64,
    kind: Target,
}

impl Issue<'_> {
    /// `gh issue` か `gh pr`。
    fn sub(&self) -> &'static str {
        match self.kind {
            Target::Issue => "issue",
            Target::PullRequest => "pr",
        }
    }

    /// PRのレビューを投稿する（`POST /repos/{owner}/{repo}/pulls/{number}/reviews`）。
    fn review(&self, credentials: &Credentials, payload: &serde_json::Value) -> Result<()> {
        let path = format!("repos/{}/pulls/{}/reviews", self.repo, self.number);
        self.gh(
            credentials,
            &["api", "-X", "POST", &path, "--input", "-", "--silent"],
            Some(&payload.to_string()),
        )
        .map(|_| ())
    }

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
        let mut args = vec![self.sub(), "edit", number.as_str(), "--repo", repo.as_str()];
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
                self.sub(),
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
                self.sub(),
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
            run: run_dir(home, repo, number),
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
            // 記録には、AIが読んだファイルの中身やコマンドの出力も入るので、ほかのユーザーに読ませない
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
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

/// 外すラベルのうち、付いているもので、付けるものに入っていないもの（大文字・小文字を区別しない）。
fn present<'a>(labels: &[String], remove: &[&'a str], add: &[&str]) -> Vec<&'a str> {
    let mut out: Vec<&'a str> = Vec::new();
    for r in remove {
        if labels.iter().any(|l| l.eq_ignore_ascii_case(r))
            && !add.iter().any(|a| a.eq_ignore_ascii_case(r))
            && !out.iter().any(|o| o.eq_ignore_ascii_case(r))
        {
            out.push(r);
        }
    }
    out
}

/// ブランチが、ベースのブランチとぶつかるかを、手元で調べた結果。
#[derive(Debug, PartialEq, Eq)]
enum BaseCheck {
    /// ぶつからない（`base` は、調べたときのベースのブランチの先）
    Clean {
        base: String,
    },
    Conflict(prompt::Conflict),
    /// 調べられなかった（gitが古い、ベースのブランチを読めないなど）
    Unknown,
}

/// ブランチが、ベースのブランチ（`origin/<base>`）とぶつかるかを、`git merge-tree` で調べる。
fn check_base(probe: &dyn Probe, worktree: &Path, branch: &Branch) -> BaseCheck {
    let Ok(wt) = path_str(worktree) else {
        return BaseCheck::Unknown;
    };
    let base = format!("origin/{}", branch.base);
    let Ok(tip) = git_out(probe, &["-C", &wt, "rev-parse", "--verify", &base]) else {
        return BaseCheck::Unknown;
    };
    let Some(out) = probe.run_with_env(
        "git",
        &[
            "-C",
            &wt,
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            "--no-messages",
            &base,
            "HEAD",
        ],
        ISOLATED_GIT_ENV,
    ) else {
        return BaseCheck::Unknown;
    };
    // ぶつかりがなければ0、あれば1で終わる。木のあとに、ぶつかったファイルが並ぶ（NULで区切る。
    // 区切らないと、日本語などの名前が引用符と8進数になる）
    match out.code {
        Some(0) => BaseCheck::Clean {
            base: tip.trim().to_string(),
        },
        Some(1) => {
            let files: Vec<String> = out
                .stdout
                .split('\0')
                .skip(1)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            BaseCheck::Conflict(prompt::Conflict {
                base: branch.base.clone(),
                files,
            })
        }
        _ => BaseCheck::Unknown,
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
    /// 使えるツール（`--tools`）。`None` なら標準のまま（設定のallowとdenyで絞る）
    tools: Option<&'a str>,
    timeout: Duration,
    /// 標準出力と標準エラーを残す場所
    log_dir: &'a Path,
    /// この仕事の場所（worktree、一時ディレクトリ、キャッシュ）。ここで動いているプロセスは、終わったら止める
    job_dirs: &'a [PathBuf],
    /// 動いている間、見つけたプロセスを書いておく目印（`stuck.json`）と、どの仕事か（[`Stuck`]）
    record: Option<(PathBuf, String)>,
}

/// claudeの終わり方。プロセスを止めきれなかったときは、ここには来ない（`Failure::Unstopped`）。
/// 書き込みの道に入るのは、止めたことを確かめたときだけにするため。
enum ClaudeEnd {
    Exited(String),
    TimedOut,
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
            // 動いている間の様子を見られるように、出来事を1行ずつ書かせる（最後の行が結果）
            "--output-format",
            "stream-json",
            "--verbose",
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
        if let Some(tools) = self.tools {
            args.extend(["--tools".to_string(), tools.to_string()]);
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

    fn run(&self) -> std::result::Result<ClaudeEnd, Failure> {
        let log = |name: &str| -> Result<std::fs::File> {
            let path = self.log_dir.join(name);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            // AIが読んだファイルの中身やコマンドの出力も入るので、ほかのユーザーに読ませない
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(&path)
                .map_err(|e| Error::new(format!("{} を作れません: {e}", path.display())))
        };
        let stdout_path = self.log_dir.join(CLAUDE_LOG);
        let mark = self.mark();
        let mut command = Command::new(self.program);
        command
            .args(self.args())
            .current_dir(self.cwd)
            .env_clear()
            .envs(self.env())
            .env(RUN_MARK_ENV, &mark)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log(CLAUDE_LOG)?))
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
        let tick = current_process();
        let mut recorded = 0;
        let timed_out = loop {
            seen.collect(pid);
            // 見つけたものが増えたら書き直す（tickが殺されても、次のtickが残ったものに気づけるように）
            if let Some((path, job)) = &self.record
                && seen.known.len() != recorded
            {
                recorded = seen.known.len();
                seen.record(path, job, tick.clone());
            }
            match child.try_wait() {
                Ok(Some(_)) => break false,
                Ok(None) if Instant::now() >= deadline => break true,
                // Claude CodeのBashのシェルはすぐ終わるので、短い間隔で見る
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(Error::new(format!("claudeを待てません: {e}")).into()),
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
        if let Err(leftovers) = stopped {
            return Err(Failure::Unstopped(leftovers));
        }
        // すべて止めたので、動いている間の目印は要らない
        if let Some((path, _)) = &self.record {
            let _ = std::fs::remove_file(path);
        }
        if timed_out {
            return Ok(ClaudeEnd::TimedOut);
        }
        let stdout = std::fs::read_to_string(&stdout_path)
            .map_err(|e| Error::new(format!("claudeの出力を読めません: {e}")))?;
        Ok(ClaudeEnd::Exited(result_line(&stdout).to_string()))
    }
}

/// cellの外で、ファイルを読むだけのclaudeを動かす（inspector）。使えるツールは Read、Grep、Glob だけで、
/// 読めるのは `cwd` の中だけ（`-p` では、作業場所の外を読むには許しが要り、許していないので止まる）。
pub struct Reader<'a> {
    pub home: &'a Home,
    pub claude: &'a Path,
    pub cwd: &'a Path,
    /// `--settings` に渡すファイル
    pub settings: &'a Path,
    pub system: &'a str,
    pub schema: &'a serde_json::Value,
    pub prompt: &'a str,
    pub model: Option<&'a str>,
    pub timeout: Duration,
    /// 出力（`claude.jsonl`）を残す場所
    pub log_dir: &'a Path,
}

impl Reader<'_> {
    /// 動かして、結果の行（`"type":"result"`）を返す。時間切れなら `None`。止めきれなかったプロセスがあれば、
    /// `stuck.json` を残してエラーを返す（cellと同じく、それが残っている間は次のtickが動かない）。
    pub fn run(&self) -> Result<Option<serde_json::Value>> {
        let context = self.log_dir.join("context.json");
        let job_dirs = [self.cwd.to_path_buf()];
        let run = ClaudeRun {
            program: self.claude,
            cwd: self.cwd,
            settings: self.settings,
            system: self.system,
            schema: self.schema,
            prompt: self.prompt,
            context: &context,
            path_prefix: None,
            model: self.model,
            tools: Some("Read,Grep,Glob"),
            timeout: self.timeout,
            log_dir: self.log_dir,
            job_dirs: &job_dirs,
            record: Some((stuck_path(self.home), "inspector".to_string())),
        };
        match run.run() {
            Ok(ClaudeEnd::Exited(line)) => serde_json::from_str(&line)
                .map(Some)
                .map_err(|e| Error::new(format!("AIの出力を読めません: {e}"))),
            Ok(ClaudeEnd::TimedOut) => Ok(None),
            Err(Failure::Error(e)) => Err(e),
            Err(Failure::Unstopped(leftovers)) => {
                // tickを書かずに残し、tickが動いていても残ったものとして扱わせる（cellの失敗と同じ）
                let stuck = Stuck {
                    job: "inspector".to_string(),
                    processes: leftovers,
                    tick: None,
                };
                if let Ok(text) = serde_json::to_string_pretty(&stuck) {
                    let _ = std::fs::write(stuck_path(self.home), text);
                }
                Err(Error::new(
                    "claudeが動かしたプロセスを止めきれませんでした。残っている間は、次のtickは動きません",
                )
                .with_hint("`agentcells doctor` で確かめてください"))
            }
        }
    }
}

/// claudeの出力（`stream-json`）を書くファイル。
const CLAUDE_LOG: &str = "claude.jsonl";

/// 残したAIの作業のうち、古いもの（`KEPT_DAYS` より前に作ったもの）を消す。
fn prune_kept(kept: &Path, now: u64) {
    let limit = KEPT_DAYS * 24 * 3600;
    let Ok(owners) = std::fs::read_dir(kept) else {
        return;
    };
    // kept/<owner>/<repo>/<番号>/<時刻>.bundle
    let dirs = |dir: std::path::PathBuf| -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .map(|e| e.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    };
    for owner in owners.flatten().map(|e| e.path()) {
        for repo in dirs(owner.clone()) {
            for number in dirs(repo.clone()) {
                for bundle in dirs(number.clone()) {
                    let at = bundle
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(|s| s.parse::<u64>().ok());
                    if at.is_some_and(|at| at + limit < now) {
                        let _ = std::fs::remove_file(bundle);
                    }
                }
                // 空になった場所も消す（空でなければ消えない）
                let _ = std::fs::remove_dir(&number);
            }
            let _ = std::fs::remove_dir(&repo);
        }
        let _ = std::fs::remove_dir(&owner);
    }
}

/// 残したAIの作業を消すまでの日数。
const KEPT_DAYS: u64 = 30;

/// AIが足したコミット（動かす前のブランチの先から）を、gitのbundleとして `path`（`kept` の下）に残す。
/// 秘密らしいもので断った作業も入るので、`kept` から下はほかのユーザーに読ませない。
fn keep_bundle(
    probe: &dyn Probe,
    staging: &Path,
    branch: &Branch,
    kept: &Path,
    path: &Path,
) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let private = |dir: &Path| {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .and_then(|()| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)))
            .map_err(|e| Error::new(format!("{} を作れません: {e}", dir.display())))
    };
    private(kept)?;
    if let Some(dir) = path.parent() {
        private(dir)?;
    }
    let staging = path_str(staging)?;
    let out = path_str(path)?;
    let head = format!("refs/heads/{}", branch.name);
    let start = format!("^{}", branch.start);
    git(
        probe,
        &[
            "-C", &staging, "bundle", "create", "-q", &out, &head, &start,
        ],
        ISOLATED_GIT_ENV,
    )?;
    // AIが読んだファイルの中身も入りうるので、ほかのユーザーに読ませない
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::new(format!("{} の権限を変えられません: {e}", path.display())))
}

/// Issue/PRごとの、実行の記録の場所（`runs/<owner>/<repo>/<番号>`）。
fn run_dir(home: &Home, repo: &RepoName, number: u64) -> PathBuf {
    home.root()
        .join("runs")
        .join(&repo.owner)
        .join(&repo.name)
        .join(number.to_string())
}

/// Issue/PRの、最後の実行のclaudeの出力。同じIssue/PRの次の実行で上書きされる。
pub fn claude_log(home: &Home, repo: &RepoName, number: u64) -> PathBuf {
    run_dir(home, repo, number).join(CLAUDE_LOG)
}

/// `stream-json` の出力から、結果の行（`"type":"result"`）を読む。なければ空。
fn result_line(stdout: &str) -> &str {
    stdout
        .lines()
        .rev()
        .find(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|v| v["type"].as_str() == Some("result"))
        })
        .unwrap_or("")
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
    /// 覚えたプロセスを、動いている間の目印として書く。書けなくても、cellは止めない。
    fn record(&self, path: &Path, job: &str, tick: Option<Leftover>) {
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
    /// 止めきれなければ、残ったものを返す。
    fn stop_all(&mut self, root: u32) -> std::result::Result<(), Vec<Leftover>> {
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
fn command_of(pid: u32) -> String {
    Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
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
            tools: None,
            timeout,
            log_dir: dir,
            job_dirs: &[],
            record: None,
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
            r#"cat > prompt.txt; printf '%s\n' "$@" > args.txt; echo "$PATH" > path.txt; echo "$AGENTCELLS_CONTEXT" > context.txt; env > env.txt; echo '{"type":"system"}'; echo '{"type":"result","structured_output":{"next":"done","comment":"ok"}}'"#,
        );
        let schema = serde_json::json!({"type": "object"});
        let end = claude_run(dir.path(), &program, &schema, Duration::from_secs(30))
            .run()
            .unwrap_or_else(|_| panic!("failed"));
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
            .unwrap_or_else(|_| panic!("failed"));
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
            .unwrap_or_else(|_| panic!("failed"));
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
            tools: None,
            timeout: Duration::from_secs(300),
            log_dir: dir.path(),
            job_dirs: &job_dirs,
            record: None,
        };
        let end = run.run().unwrap_or_else(|_| panic!("failed"));
        let ClaudeEnd::Exited(result) = end else {
            panic!("timed out");
        };
        // stream-json の最後の結果の行を読む
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["type"], "result", "{result}");
        std::thread::sleep(Duration::from_millis(500));
        let left = Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .unwrap();
        let left = String::from_utf8_lossy(&left.stdout).trim().to_string();
        let log = std::fs::read_to_string(dir.path().join(CLAUDE_LOG)).unwrap_or_default();
        assert!(left.is_empty(), "left: {left}\n{log}");
        assert!(log.contains("started") || log.contains("done"), "{log}");
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
            kind: Target::Issue,
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
            model: None,
            confirm: None,
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
        let started = process_table()
            .into_iter()
            .find(|r| r.0 == me)
            .map(|r| r.3)
            .unwrap();
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
    fn finds_files_that_conflict_with_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("日本.txt"), "a").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "ac/x"]);
        std::fs::write(repo.join("日本.txt"), "pr").unwrap();
        git(&["commit", "-qam", "pr"]);
        git(&["checkout", "-q", "main"]);
        std::fs::write(repo.join("b.txt"), "b").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "main"]);
        let main = git(&["rev-parse", "HEAD"]);
        git(&["update-ref", "refs/remotes/origin/main", &main]);
        git(&["checkout", "-q", "ac/x"]);
        let branch = Branch {
            name: "ac/x".into(),
            base: "main".into(),
            start: String::new(),
        };
        let probe = crate::probe::SystemProbe;
        // 別のファイルを変えただけなら、ぶつからない
        assert_eq!(
            check_base(&probe, &repo, &branch),
            BaseCheck::Clean { base: main.clone() }
        );
        git(&["checkout", "-q", "main"]);
        std::fs::write(repo.join("日本.txt"), "main").unwrap();
        git(&["commit", "-qam", "main2"]);
        let main = git(&["rev-parse", "HEAD"]);
        git(&["update-ref", "refs/remotes/origin/main", &main]);
        git(&["checkout", "-q", "ac/x"]);
        assert_eq!(
            check_base(&probe, &repo, &branch),
            BaseCheck::Conflict(prompt::Conflict {
                base: "main".into(),
                files: vec!["日本.txt".into()],
            })
        );
        // ベースのブランチを読めなければ、分からない
        let gone = Branch {
            base: "gone".into(),
            ..branch
        };
        assert_eq!(check_base(&probe, &repo, &gone), BaseCheck::Unknown);
    }

    #[test]
    fn refused_work_can_be_fetched_from_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let out = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let start = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["checkout", "-q", "-b", "ac/x"]);
        std::fs::write(repo.join("b.txt"), "b").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "ai work"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let branch = Branch {
            name: "ac/x".into(),
            base: "main".into(),
            start: start.clone(),
        };
        let kept = dir.path().join("kept");
        let bundle = kept.join("octo/app/3/100.bundle");
        keep_bundle(&crate::probe::SystemProbe, &repo, &branch, &kept, &bundle).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&bundle), 0o600);
        assert_eq!(mode(&kept), 0o700);
        assert_eq!(mode(&kept.join("octo/app/3")), 0o700);
        // 人の手元のclone（AIの作業はない）に、書いたとおりのコマンドで取り出せる
        let human = dir.path().join("human");
        let repo_arg = repo.to_str().unwrap();
        git(
            dir.path(),
            &["clone", "-q", "-b", "main", repo_arg, "human"],
        );
        git(
            &human,
            &["fetch", "-q", bundle.to_str().unwrap(), "ac/x:ai-work"],
        );
        assert_eq!(git(&human, &["rev-parse", "ai-work"]), head);

        // 古いものは消す
        prune_kept(&kept, 100 + KEPT_DAYS * 24 * 3600 - 1);
        assert!(bundle.exists());
        prune_kept(&kept, 100 + KEPT_DAYS * 24 * 3600 + 1);
        assert!(!bundle.exists());
        assert!(!kept.join("octo").exists());
        assert!(kept.exists());
    }

    #[test]
    fn only_present_labels_are_removed() {
        let labels = vec!["ac:Working".to_string(), "ac:needs-human".to_string()];
        assert_eq!(
            present(
                &labels,
                &["ac:triage", "ac:working", "ac:needs-human", "ac:in-review"],
                &["ac:needs-human"]
            ),
            vec!["ac:working"]
        );
    }

    #[test]
    fn public_run_names_do_not_include_local_paths() {
        let home = Home::new(PathBuf::from("/Users/someone/.agentcells"));
        let place = Place::new(&home, &RepoName::parse("octo/app").unwrap(), 3);
        assert_eq!(place.run_name(), "~/.agentcells/runs/octo/app/3");
    }
}
