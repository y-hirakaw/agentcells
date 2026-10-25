//! 定期実行で呼ぶ `agentcells tick`。仕事を探し、1件ずつcellを動かす。仕事が残っていれば、
//! 上限（時間と回数）まで、続けて探して動かす（[`Session`]）。
//!
//! 仕事は、開いているIssue/PRのうち、処理中（`ac:working`）でないもので、次のどちらか。
//! Issueはproductの `issues` のリポジトリから、PRは `repos` のリポジトリから探す。
//! - cellのラベルが付いている
//! - 人間を待っている（`ac:needs-human`、`ac:in-review`）ところに、cellを使う人が返事をした。
//!   人に返事をするのと同じように、コメントするだけで続きが動く
//!
//! そのうち、cellを使う人（operator）が自分で決めたものだけを動かす（[`guard`]）。

use std::collections::{BTreeMap, HashMap};
use std::fs::File;

use serde::{Deserialize, Serialize};

use crate::config::{Config, Product, Target};
use crate::discussion;
use crate::error::{Error, Result};
use crate::github::Credentials;
use crate::guard::{self, Actors};
use crate::home::{Home, RepoName};
use crate::labels::WORKING;
use crate::probe::Probe;
use crate::transition::Waiting;

/// 1件の仕事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Work {
    pub product: String,
    pub cell: String,
    pub item: Target,
    pub repo: RepoName,
    pub number: u64,
    pub title: String,
    /// 人間を待っていたところから、cellを使う人の返事で動くなら、その状態
    pub waiting: Option<Waiting>,
}

impl Work {
    pub fn describe(&self) -> String {
        let kind = match self.item {
            Target::Issue => "Issue",
            Target::PullRequest => "PR",
        };
        let reply = if self.waiting.is_some() {
            "（返事を受けて）"
        } else {
            ""
        };
        // タイトルはほかの人が書けるので、制御文字を置き換える
        format!(
            "{}{reply} {kind} {}#{}「{}」",
            self.cell,
            self.repo,
            self.number,
            crate::display::printable(&self.title)
        )
    }

    /// 仕事を拾ったラベル。動かす前にもう一度確かめ、終わったら外す。
    pub fn label<'a>(&self, cell: &'a crate::config::Cell) -> &'a str {
        match self.waiting {
            Some(waiting) => waiting.label(),
            None => &cell.label,
        }
    }
}

/// 1回のtickで動かす回数の上限。時間の上限より先に、思わぬくり返しを止めるため。
pub const MAX_RUNS: usize = 20;

/// 1回のtickの中で、続けて動かすかを決める。
///
/// - 1件は必ず動かす。そのあとは、始めてから `max` がたつか、[`MAX_RUNS`] 件動かしたら、新しく始めない
///   （動いているcellは止めない）
/// - 見送ったもの（動かす前の確かめで止めたもの）は、このtickではもう拾わない。ラベルが残るので、
///   そのままだと同じものを拾い続けるため。進んだものは、また拾ってよい（triageからimplementへ、など。
///   AIどうしの往復は、cellの `max_rounds` で止まる）
#[derive(Debug)]
pub struct Session {
    started: std::time::Instant,
    max: std::time::Duration,
    runs: usize,
    skipped: Vec<(Target, RepoName, u64)>,
    /// これ以上続けない（AIの実行が失敗したなど）
    halted: bool,
}

impl Session {
    pub fn new(max: std::time::Duration) -> Self {
        Session {
            started: std::time::Instant::now(),
            max,
            runs: 0,
            skipped: Vec::new(),
            halted: false,
        }
    }

    /// これ以上続けない。AIの実行そのものが失敗したら（使用量の上限、ログインの切れなど）、
    /// ほかの仕事も同じように失敗して、たまっていた仕事がまとめて人間に回ってしまうため。
    pub fn halt(&mut self) {
        self.halted = true;
    }

    pub fn halted(&self) -> bool {
        self.halted
    }

    pub fn runs(&self) -> usize {
        self.runs
    }

    /// もう新しく始めないか。
    pub fn exhausted(&self) -> bool {
        self.runs > 0
            && (self.halted || self.runs >= MAX_RUNS || self.started.elapsed() >= self.max)
    }

    /// 次に動かすもの。このtickで見送ったものは飛ばす。
    pub fn next<'a>(&self, work: &'a [Work]) -> Option<&'a Work> {
        work.iter().find(|w| {
            !self.skipped.iter().any(|(item, repo, number)| {
                *item == w.item && repo == &w.repo && *number == w.number
            })
        })
    }

    /// 動かした。`skipped` なら、このtickではもう拾わない。
    pub fn ran(&mut self, work: &Work, skipped: bool) {
        self.runs += 1;
        if skipped {
            self.skipped
                .push((work.item, work.repo.clone(), work.number));
        }
    }
}

/// tickを同時に2つ動かさないためのロック。持っている間だけ有効。
pub struct Lock(#[allow(dead_code)] File);

/// ロックを取る。ほかのtickが持っていれば `None`。
pub fn lock(home: &Home) -> Result<Option<Lock>> {
    let path = home.root().join("tick.lock");
    let file = File::create(&path)
        .map_err(|e| Error::new(format!("{} を作れません: {e}", path.display())))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(Lock(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(Error::new(format!(
            "{} をロックできません: {e}",
            path.display()
        ))),
    }
}

/// 見つかった仕事と、見送ったもの（理由つき）。
#[derive(Debug, Default)]
pub struct Found {
    pub work: Vec<Work>,
    pub skipped: Vec<(Work, String)>,
}

#[derive(Deserialize)]
struct Listed {
    number: u64,
    title: String,
    #[serde(default)]
    labels: Nodes<ListedLabel>,
    #[serde(default)]
    assignees: Nodes<ListedUser>,
    #[serde(default)]
    author: Option<ListedAuthor>,
    #[serde(default)]
    body: String,
    #[serde(default, rename = "updatedAt")]
    updated_at: String,
}

impl Listed {
    /// GitHubのラベル名は大文字・小文字を区別しない。
    fn has_label(&self, label: &str) -> bool {
        self.labels
            .nodes
            .iter()
            .any(|l| l.name.eq_ignore_ascii_case(label))
    }

    fn is_assigned(&self, actors: &Actors) -> bool {
        self.assignees
            .nodes
            .iter()
            .any(|a| a.login.eq_ignore_ascii_case(&actors.operator))
    }

    /// 作った人。GraphQLはAppを `[bot]` なしのloginで返すので、RESTと同じ形にする。
    fn author_login(&self) -> String {
        match &self.author {
            Some(a) if a.typename == "Bot" => format!("{}[bot]", a.login),
            Some(a) => a.login.clone(),
            None => "ghost".to_string(),
        }
    }
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

impl<T> Default for Nodes<T> {
    fn default() -> Self {
        Nodes { nodes: Vec::new() }
    }
}

#[derive(Deserialize)]
struct ListedAuthor {
    #[serde(rename = "__typename")]
    typename: String,
    login: String,
}

#[derive(Deserialize)]
struct ListedLabel {
    name: String,
}

#[derive(Deserialize)]
struct ListedUser {
    login: String,
}

/// 仕事を探す。人間を待っていたもので、cellを使う人が返事をしたものを先に、そのあとに、流れの後ろの工程
/// （[`depths`] の深いcell）から、同じ工程ならPRをIssueより先に並べる（仕掛かっているものを先に片付ける）。
/// それも同じなら、productとcellの設定の順、番号の小さい順。
///
/// `credentials_for` は、リポジトリを読むための認証を返す（ghの設定はAIが書き換えられるので、
/// 読むときもトークンと空の設定のghを使う）。
/// Assigneesにoperatorがいないものと、[`guard::check`] が止めたものは、理由を付けて見送る。
pub fn find(
    config: &Config,
    probe: &dyn Probe,
    credentials_for: &dyn Fn(&RepoName) -> Result<Credentials>,
    actors: &Actors,
    quiet: &mut Quiet,
) -> Result<Found> {
    let mut found = Found::default();
    let mut replied = Found::default();
    // Appのトークンはリポジトリごとなので、1回のtickの中ではリポジトリごとに1つだけ作って使い回す
    let mut cache: HashMap<RepoName, Credentials> = HashMap::new();
    for product in &config.products {
        // 元のIssueのリポジトリの分も、先に作っておく
        for repo in std::iter::once(&product.issues).chain(&product.repos) {
            if !cache.contains_key(repo) {
                cache.insert(repo.clone(), credentials_for(repo)?);
            }
        }
        let cell_labels: Vec<String> = product
            .cells
            .iter()
            .filter_map(|name| config.cell(name))
            .map(|c| c.label.clone())
            .collect();
        let checker = Checker {
            probe,
            cache: &cache,
            cell_labels: &cell_labels,
            actors,
        };

        for name in &product.cells {
            let Some(cell) = config.cell(name) else {
                continue;
            };
            for &item in &cell.on {
                for repo in repos_for(product, item) {
                    let env = cache[repo].gh_env()?;
                    let mut items = list(probe, &env, item, repo, &cell.label)?;
                    items.sort_by_key(|i| i.number);
                    for i in items {
                        // GitHubのラベル名は大文字・小文字を区別しない。人間を待っているものでも、
                        // cellのラベルを付け直したなら、そのcellで動かす（待っていたラベルは、終わったら外す）
                        if i.has_label(WORKING) {
                            continue;
                        }
                        let work = Work {
                            product: product.name.clone(),
                            cell: cell.name.clone(),
                            item,
                            repo: repo.clone(),
                            number: i.number,
                            title: i.title.clone(),
                            waiting: None,
                        };
                        checker.check(&mut found, work, &i, &cell.label)?;
                    }
                }
            }
        }

        // 人間を待っていたもので、cellを使う人が、cellが読んだあとに書いたもの（コメント、レビュー）があれば、また動かす
        for waiting in Waiting::ALL {
            for item in [Target::Issue, Target::PullRequest] {
                if !waiting.applies_to(item) || !product_handles(config, product, item) {
                    continue;
                }
                for repo in repos_for(product, item) {
                    let env = cache[repo].gh_env()?;
                    let mut items = list(probe, &env, item, repo, waiting.label())?;
                    items.sort_by_key(|i| i.number);
                    for i in items {
                        // cellのラベルが付いていれば、上でそのcellの仕事として見ている
                        if i.has_label(WORKING) || cell_labels.iter().any(|l| i.has_label(l)) {
                            continue;
                        }
                        // 自分を外したものは、cellを使う人がやめたものなので、黙って飛ばす
                        if !i.is_assigned(actors) {
                            continue;
                        }
                        let mut work = Work {
                            product: product.name.clone(),
                            cell: String::new(),
                            item,
                            repo: repo.clone(),
                            number: i.number,
                            title: i.title.clone(),
                            waiting: Some(waiting),
                        };
                        // 前のtickで返事がなく、そのあと何も変わっていなければ、読み直さない
                        let key = format!("{repo}#{}", i.number);
                        if quiet.still_quiet(&key, &i.updated_at) {
                            continue;
                        }
                        let resumed = discussion::fetch(probe, &env, item, repo, i.number)
                            .map_err(|e| format!("やり取りを読めません: {}", e.message))
                            .and_then(|d| resumed_cell(config, product, item, waiting, &d, actors));
                        match resumed {
                            Ok(None) => quiet.remember(key, &i.updated_at),
                            Ok(Some(cell)) => {
                                work.cell = cell;
                                checker.check(&mut replied, work, &i, waiting.label())?;
                            }
                            Err(reason) => replied.skipped.push((work, reason)),
                        }
                    }
                }
            }
        }
    }
    // 流れの後ろの工程から動かす。新しいIssueの triage が、前からあるPRの review を待たせないように
    order(config, &mut found.work);
    // 返事を待っていた人を、先に相手にする
    replied.work.append(&mut found.work);
    replied.skipped.append(&mut found.skipped);
    Ok(replied)
}

/// 流れの後ろの工程（[`depths`] の深いcell）から並べ、同じ深さならPRをIssueより先にする。
/// それ以外は、今の順（productとcellの設定の順、番号の小さい順）のまま。
fn order(config: &Config, work: &mut [Work]) {
    let depths: HashMap<String, HashMap<String, usize>> = config
        .products
        .iter()
        .map(|p| (p.name.clone(), depths(config, p)))
        .collect();
    work.sort_by_key(|w| {
        let depth = depths
            .get(&w.product)
            .and_then(|d| d.get(&w.cell))
            .copied()
            .unwrap_or(0);
        (std::cmp::Reverse(depth), w.item == Target::Issue)
    });
}

/// productの中での、cellの深さ（流れの入口から、いくつ進んだところか）。入口は、ほかのcellの `next` に
/// ないcell（triage など）で、深さ0。そこから `next` をたどった、いちばん短い数（差し戻しの輪があっても止まる）。
/// 入口がなかったり、入口からたどれないcell（別の輪）があれば、そのうち設定で先のcellを入口にして、またたどる。
pub fn depths(config: &Config, product: &Product) -> HashMap<String, usize> {
    let cells: Vec<_> = product
        .cells
        .iter()
        .filter_map(|name| config.cell_in(product, name))
        .collect();
    let in_product = |name: &String| product.cells.contains(name);
    let pointed: Vec<&String> = cells
        .iter()
        .flat_map(|c| c.next.iter().filter(|n| *n != &c.name && in_product(n)))
        .collect();
    let mut queue: std::collections::VecDeque<String> = cells
        .iter()
        .filter(|c| !pointed.contains(&&c.name))
        .map(|c| c.name.clone())
        .collect();
    let mut depths: HashMap<String, usize> = queue.iter().map(|n| (n.clone(), 0)).collect();
    loop {
        while let Some(name) = queue.pop_front() {
            let depth = depths[&name];
            let Some(cell) = cells.iter().find(|c| c.name == name) else {
                continue;
            };
            for next in cell.next.iter().filter(|n| in_product(n)) {
                if !depths.contains_key(next) {
                    depths.insert(next.clone(), depth + 1);
                    queue.push_back(next.clone());
                }
            }
        }
        // まだ深さのないcell（入口からたどれない輪）があれば、設定で先のものを入口にする
        match cells.iter().find(|c| !depths.contains_key(&c.name)) {
            Some(cell) => {
                depths.insert(cell.name.clone(), 0);
                queue.push_back(cell.name.clone());
            }
            None => break,
        }
    }
    depths
}

/// 仕事を探すリポジトリ。Issueはproductの `issues`、PRは `repos`。
fn repos_for(product: &Product, item: Target) -> Vec<&RepoName> {
    match item {
        Target::Issue => vec![&product.issues],
        Target::PullRequest => product.repos.iter().collect(),
    }
}

/// productのどれかのcellが、Issue（かPR）を扱うか。
fn product_handles(config: &Config, product: &Product, item: Target) -> bool {
    product
        .cells
        .iter()
        .filter_map(|name| config.cell(name))
        .any(|c| c.on.contains(&item))
}

/// 人間を待っていたIssue/PRで、また動かすcell。cellを使う人が、cellが読んだあとに何も書いていなければ `None`。
///
/// - `ac:needs-human`: 最後に目印を書いたcell（質問したcellや、動かせなかったcell）
/// - `ac:in-review`: 最後に目印を書いたのがレビューのcellなら、その `next` のうち、PRを直すcell。
///   直すcellが返事だけをして検品待ちに戻したのなら、そのcell
fn resumed_cell(
    config: &Config,
    product: &Product,
    item: Target,
    waiting: Waiting,
    d: &discussion::Discussion,
    actors: &Actors,
) -> std::result::Result<Option<String>, String> {
    let replied = discussion::unread(&d.posts, &actors.writer)
        .iter()
        .any(|p| p.author.eq_ignore_ascii_case(&actors.operator));
    if !replied {
        return Ok(None);
    }
    let in_product = |name: &str| {
        product
            .cells
            .iter()
            .any(|c| c == name)
            .then(|| config.cell_in(product, name))
            .flatten()
    };
    let last = discussion::last_marker(&d.posts, &actors.writer)
        .and_then(|i| crate::marker::parse(&d.posts[i].body))
        .map(|(cell, _)| cell)
        .ok_or_else(|| {
            "どのcellが返事を待っていたのか分かりません（agentcellsのコメントがありません）"
                .to_string()
        })?;
    let mut cell = in_product(&last)
        .ok_or_else(|| format!("返事を待っていたcell「{last}」が、このproductにありません"))?;
    if waiting == Waiting::InReview && cell.review {
        cell = cell
            .next
            .iter()
            .filter_map(|n| in_product(n))
            .find(|c| !c.review && c.on.contains(&Target::PullRequest))
            .ok_or_else(|| format!("cell「{}」の next に、PRを直すcellがありません", cell.name))?;
    }
    if !cell.on.contains(&item) {
        return Err(format!(
            "cell「{}」は{}を扱えません",
            cell.name,
            item.as_str()
        ));
    }
    Ok(Some(cell.name.clone()))
}

/// 見つけた仕事を、[`guard::check`] で確かめる。
struct Checker<'a> {
    probe: &'a dyn Probe,
    cache: &'a HashMap<RepoName, Credentials>,
    cell_labels: &'a [String],
    actors: &'a Actors,
}

impl Checker<'_> {
    /// 動かしてよければ `found.work` に、だめなら理由を付けて `found.skipped` に入れる。
    /// `label` は、仕事を拾ったラベル（cellのラベルか、人間を待っているラベル）。
    fn check(&self, found: &mut Found, work: Work, i: &Listed, label: &str) -> Result<()> {
        let actors = self.actors;
        if !i.is_assigned(actors) {
            let reason = format!("Assignees に {} がいません", actors.operator);
            found.skipped.push((work, reason));
            return Ok(());
        }
        let author = i.author_login();
        let original = match work.item {
            Target::PullRequest => crate::prompt::closes(&i.body, &work.repo),
            Target::Issue => None,
        };
        let original_env = match &original {
            Some((original_repo, _)) => match self.cache.get(original_repo) {
                Some(c) => Some(c.gh_env()?),
                None => {
                    let reason = format!(
                        "元のIssueのリポジトリ {original_repo} が、このproductにありません"
                    );
                    found.skipped.push((work, reason));
                    return Ok(());
                }
            },
            None => None,
        };
        let env = self.cache[&work.repo].gh_env()?;
        let request = guard::Request {
            kind: work.item,
            subject: guard::Subject {
                repo: &work.repo,
                number: work.number,
                env: &env,
            },
            author: &author,
            label,
            cell_labels: self.cell_labels,
            original: original.as_ref().zip(original_env.as_ref()).map(
                |((original_repo, number), env)| guard::Subject {
                    repo: original_repo,
                    number: *number,
                    env,
                },
            ),
        };
        // 1件の履歴を読めなくても、ほかの仕事は進める（読めなかったものは動かさない）
        match guard::check(self.probe, &request, actors) {
            Ok(None) => found.work.push(work),
            Ok(Some(reason)) => found.skipped.push((work, reason)),
            Err(e) => found
                .skipped
                .push((work, format!("履歴を読めません: {}", e.message))),
        }
        Ok(())
    }
}

/// 一覧で読む項目。
const FIELDS: &str = "number title body updatedAt author { __typename login } labels(first: 100) { nodes { name } } assignees(first: 100) { nodes { login } }";

/// 1回に読む件数。ラベルで絞った、開いているものだけなので、足りなくなることはまずない。
const LIST_LIMIT: u32 = 100;

/// ラベルの付いた、開いているIssue/PRを読む問い合わせ。
///
/// `gh issue list --label` などは、GitHubの検索を使う。検索は、ラベルを変えたり閉じたりしてから
/// しばらくは、前のままで見つかる（cellが進めた直後に、同じtickで前のcellの仕事として拾い直し、
/// 同じcellが2回動いた）。そのため、検索を使わずに、リポジトリのIssue/PRをラベルで絞って読む。
fn list_query(item: Target) -> String {
    let connection = match item {
        Target::Issue => "issues",
        Target::PullRequest => "pullRequests",
    };
    format!(
        "query($owner: String!, $name: String!, $label: String!) {{ repository(owner: $owner, name: $name) {{ {connection}(states: OPEN, labels: [$label], first: {LIST_LIMIT}) {{ nodes {{ {FIELDS} }} }} }} }}"
    )
}

#[derive(Deserialize)]
struct ListResponse {
    data: ListData,
}

#[derive(Deserialize)]
struct ListData {
    repository: Option<ListRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListRepository {
    issues: Option<Nodes<Listed>>,
    pull_requests: Option<Nodes<Listed>>,
}

/// 人間を待っているもので、返事がなかったときの、Issue/PRの更新時刻（`~/.agentcells/waiting.json`）。
/// 更新時刻が変わっていなければ（コメントもレビューも増えていなければ）、やり取りを読み直さない。
/// 待っているものはたまっていくので、tickのたびに全部を読むと、APIを使いすぎるため。
///
/// 手元の目安なので、取りこぼしても遅れるだけにする。
/// - 読んでから [`QUIET_FOR`] が過ぎたら、更新時刻が同じでも読み直す。更新時刻は秒までなので、同じ秒に
///   届いた返事は時刻が変わらず、どの操作で更新時刻が変わるかを、GitHubが保証してもいないため
/// - 読めなかったり壊れていたりすれば、全部を読み直す
#[derive(Debug, Default)]
pub struct Quiet {
    /// 前のtickで覚えたもの（`owner/repo#番号` → 覚えたこと）
    checked: BTreeMap<String, Checked>,
    /// このtickで覚えるもの。前のものは引き継がない（待たなくなったものを残さないため）
    next: BTreeMap<String, Checked>,
    /// 今の時刻（UNIXの秒）
    now: u64,
}

/// 返事がないと読んだときのこと。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Checked {
    /// そのときのIssue/PRの更新時刻（GitHubの時刻）
    updated_at: String,
    /// 読んだ時刻（手元の時計。UNIXの秒）
    read_at: u64,
}

/// 返事がないと読んだものを、読み直さずにおく長さ（秒）。
const QUIET_FOR: u64 = 30 * 60;

impl Quiet {
    fn path(home: &Home) -> std::path::PathBuf {
        home.root().join("waiting.json")
    }

    pub fn load(home: &Home) -> Self {
        let checked = std::fs::read_to_string(Self::path(home))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        Quiet {
            checked,
            next: BTreeMap::new(),
            now,
        }
    }

    /// このtickで覚えたものを書く。書けなくても、次のtickで読み直すだけなので、エラーにしない。
    pub fn save(&self, home: &Home) {
        if let Ok(text) = serde_json::to_string_pretty(&self.next) {
            let _ = std::fs::write(Self::path(home), text);
        }
    }

    /// 前に返事がないと読んでから、更新されておらず、時間もたっていなければ、覚えたことを引き継いで `true`。
    fn still_quiet(&mut self, key: &str, updated_at: &str) -> bool {
        let Some(checked) = self.checked.get(key) else {
            return false;
        };
        let quiet = !updated_at.is_empty()
            && checked.updated_at == updated_at
            && self.now.saturating_sub(checked.read_at) < QUIET_FOR
            // 手元の時計が戻ったときも、読み直す
            && checked.read_at <= self.now;
        if quiet {
            self.next.insert(key.to_string(), checked.clone());
        }
        quiet
    }

    /// 今読んで、返事がなかった。
    fn remember(&mut self, key: String, updated_at: &str) {
        if !updated_at.is_empty() {
            let checked = Checked {
                updated_at: updated_at.to_string(),
                read_at: self.now,
            };
            self.next.insert(key, checked);
        }
    }
}

fn list(
    probe: &dyn Probe,
    env: &[(&str, &str)],
    item: Target,
    repo: &RepoName,
    label: &str,
) -> Result<Vec<Listed>> {
    let query = format!("query={}", list_query(item));
    // -F は数字だけの名前を数に変えるので、-f で渡す
    let owner = format!("owner={}", repo.owner);
    let name = format!("name={}", repo.name);
    let label_arg = format!("label={label}");
    let args = [
        "api",
        "graphql",
        "-f",
        query.as_str(),
        "-f",
        owner.as_str(),
        "-f",
        name.as_str(),
        "-f",
        label_arg.as_str(),
    ];
    let out = probe.run_with_env("gh", &args, env).ok_or_else(|| {
        Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if !out.success {
        return Err(Error::new(format!(
            "{repo} の {label} の一覧を読めません: {}",
            out.stderr.trim()
        ))
        .with_hint("`agentcells doctor` で、リポジトリと書き込みのアカウントを確かめてください"));
    }
    let response: ListResponse = serde_json::from_str(&out.stdout)
        .map_err(|e| Error::new(format!("{repo} の一覧を読めません: {e}")))?;
    let repository = response
        .data
        .repository
        .ok_or_else(|| Error::new(format!("{repo} が見つかりません")))?;
    let items = match item {
        Target::Issue => repository.issues,
        Target::PullRequest => repository.pull_requests,
    };
    // 絞り込みはGitHubに任せているが、探したラベルが付いていないものを仕事にしないよう、念のため確かめる
    Ok(items
        .map(|n| n.nodes)
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.has_label(label))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Source;
    use crate::guard::fake::{NO_EDITS, assigned, labeled, with_history};
    use crate::probe::fake::FakeProbe;

    fn actors() -> Actors {
        Actors {
            operator: "me".to_string(),
            writer: "bot[bot]".to_string(),
        }
    }

    const ME: &str = r#""assignees":[{"login":"me"}]"#;

    /// operatorが自分を割り当て、ラベルを付けた履歴。
    fn approved(probe: FakeProbe, repo: &str, number: u64, label: &str) -> FakeProbe {
        let events = [
            assigned("me", "me", "2026-10-25T01:00:00Z"),
            labeled("me", label, "2026-10-25T02:00:00Z"),
        ];
        with_history(probe, repo, number, &events, NO_EDITS)
    }

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"
operator = "me"

[[product]]
name = "app"
issues = "octo/issues"
repos = ["octo/app", "octo/web"]
cells = ["triage", "review"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
next = ["done"]

[[cell]]
name = "review"
on = "pull_request"
label = "ac:review"
next = ["done"]
"#;
        let config = Config::parse(text, dir.path()).unwrap();
        (dir, config)
    }

    fn list_command(kind: &str, repo: &str, label: &str) -> String {
        let item = match kind {
            "issue" => Target::Issue,
            _ => Target::PullRequest,
        };
        let (owner, name) = repo.split_once('/').unwrap();
        format!(
            "gh api graphql -f query={} -f owner={owner} -f name={name} -f label={label}",
            list_query(item)
        )
    }

    /// ghの `--json` の形で書いた一覧（`labels` と `assignees` は配列、Appの `author` は `app/<名前>`）を、
    /// GraphQLの答えの形にする。
    fn response(kind: &str, items: &str) -> String {
        let mut items: Vec<serde_json::Value> = serde_json::from_str(items).unwrap();
        for item in &mut items {
            for key in ["labels", "assignees"] {
                if let Some(list) = item.get(key).cloned() {
                    item[key] = serde_json::json!({ "nodes": list });
                }
            }
            if let Some(login) = item["author"]["login"].as_str().map(str::to_string) {
                item["author"] = match login.strip_prefix("app/") {
                    Some(bot) => serde_json::json!({"__typename": "Bot", "login": bot}),
                    None => serde_json::json!({"__typename": "User", "login": login}),
                };
            }
        }
        let connection = if kind == "issue" {
            "issues"
        } else {
            "pullRequests"
        };
        serde_json::json!({"data": {"repository": {connection: {"nodes": items}}}}).to_string()
    }

    trait WithList {
        /// ラベルの付いた一覧を読んだら、`items`（ghの `--json` の形）を返す。
        fn with_list(self, kind: &str, repo: &str, label: &str, items: &str) -> Self;
    }

    impl WithList for FakeProbe {
        fn with_list(self, kind: &str, repo: &str, label: &str, items: &str) -> Self {
            self.with(
                &list_command(kind, repo, label),
                true,
                &response(kind, items),
            )
        }
    }

    /// 人間を待っているものはない。
    fn nothing_waiting(mut probe: FakeProbe, issues: &[&str], prs: &[&str]) -> FakeProbe {
        for repo in issues {
            probe = probe.with_list("issue", repo, "ac:needs-human", "[]");
        }
        for repo in prs {
            for label in ["ac:needs-human", "ac:in-review"] {
                probe = probe.with_list("pr", repo, label, "[]");
            }
        }
        probe
    }

    fn token(repo: &RepoName) -> Result<Credentials> {
        Credentials::new(Source::App, format!("ghs_{}", repo.name))
    }

    #[test]
    fn finds_open_items_with_cell_labels_skipping_working() {
        let (_dir, config) = config();
        let probe = FakeProbe::default()
            .with_list("issue", "octo/issues", "ac:triage", &format!(r#"[{{"number":9,"title":"後","labels":[{{"name":"ac:triage"}}],{ME}}},
                    {{"number":3,"title":"先","labels":[{{"name":"ac:triage"}}],{ME}}},
                    {{"number":4,"title":"処理中","labels":[{{"name":"ac:triage"}},{{"name":"ac:working"}}],{ME}}},
                    {{"number":5,"title":"人間待ち","labels":[{{"name":"ac:triage"}},{{"name":"AC:Needs-Human"}}],{ME}}},
                    {{"number":6,"title":"他人の","labels":[{{"name":"ac:triage"}}],"assignees":[{{"login":"other"}}]}},
                    {{"number":7,"title":"勝手に","labels":[{{"name":"ac:triage"}}],{ME}}},
                    {{"number":8,"title":"もう進んだ","labels":[{{"name":"ac:review"}}],{ME}}}]"#),
            )
            .with_list("pr", "octo/app", "ac:review", &format!(r#"[{{"number":12,"title":"PR","labels":[{{"name":"ac:review"}}],{ME}}}]"#),
            )
            .with_list("pr", "octo/web", "ac:review", "[]",
            );
        let probe = nothing_waiting(probe, &["octo/issues"], &["octo/app", "octo/web"]);
        let probe = approved(probe, "octo/issues", 9, "ac:triage");
        let probe = approved(probe, "octo/issues", 3, "ac:triage");
        // 人間を待っていても、cellのラベルを付け直したなら動かす
        let probe = approved(probe, "octo/issues", 5, "ac:triage");
        let probe = approved(probe, "octo/app", 12, "ac:review");
        // ほかの人がラベルを付けた
        let eager = [
            assigned("me", "me", "2026-10-25T01:00:00Z"),
            labeled("eager", "ac:triage", "2026-10-25T02:00:00Z"),
        ];
        let probe = with_history(probe, "octo/issues", 7, &eager, NO_EDITS);
        // 8は、一覧に返っても、探したラベルが付いていないので拾わない
        let probe = approved(probe, "octo/issues", 8, "ac:triage");
        let found = find(&config, &probe, &token, &actors(), &mut Quiet::default()).unwrap();
        let skipped: Vec<String> = found
            .skipped
            .iter()
            .map(|(w, reason)| format!("{} {reason}", w.number))
            .collect();
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(
            skipped[0].starts_with("6 Assignees に me がいません"),
            "{skipped:?}"
        );
        assert!(
            skipped[1].starts_with("7 ラベル「ac:triage」を最後に付けたのが eager"),
            "{skipped:?}"
        );
        let work = found.work;
        let described: Vec<String> = work.iter().map(Work::describe).collect();
        // どちらも入口のcell（深さ0）なので、PRを先に、同じcellの中では番号の小さい順
        assert_eq!(
            described,
            vec![
                "review PR octo/app#12「PR」",
                "triage Issue octo/issues#3「先」",
                "triage Issue octo/issues#5「人間待ち」",
                "triage Issue octo/issues#9「後」",
            ]
        );
        // リポジトリごとのトークンで、空のghの設定を使って読む
        for (call, env) in probe.calls.borrow().iter().zip(probe.envs.borrow().iter()) {
            // 一覧は --repo、履歴は /repos/ のパス、編集の履歴はGraphQLの name でリポジトリを渡す
            let rest = ["--repo octo/", "/repos/octo/", "-f name="]
                .iter()
                .find_map(|key| call.split(key).nth(1))
                .unwrap();
            let name = rest.split([' ', '/']).next().unwrap();
            assert!(
                env.contains(&("GH_TOKEN".to_string(), format!("ghs_{name}"))),
                "{call}: {env:?}"
            );
            assert!(env.iter().any(|(k, _)| k == "GH_CONFIG_DIR"));
        }
    }

    #[test]
    fn makes_one_token_per_repo() {
        let (_dir, mut config) = config();
        // 同じリポジトリを、2つのcellで、IssueとしてもPRとしても見る
        config.products[0].repos = vec![RepoName::parse("octo/issues").unwrap()];
        config.cells[1].on = vec![Target::Issue, Target::PullRequest];
        let mut probe = FakeProbe::default();
        for (kind, label) in [
            ("issue", "ac:triage"),
            ("issue", "ac:review"),
            ("pr", "ac:review"),
        ] {
            probe = probe.with_list(kind, "octo/issues", label, "[]");
        }
        let probe = nothing_waiting(probe, &["octo/issues"], &["octo/issues"]);
        let made = std::cell::Cell::new(0);
        let counting = |repo: &RepoName| {
            made.set(made.get() + 1);
            token(repo)
        };
        assert!(
            find(&config, &probe, &counting, &actors(), &mut Quiet::default())
                .unwrap()
                .work
                .is_empty()
        );
        // cellのラベルで3つと、人間を待っているもので3つ
        assert_eq!(probe.calls.borrow().len(), 6);
        assert_eq!(made.get(), 1);
    }

    #[test]
    fn prs_made_by_agentcells_run_from_the_issue_approval_and_unreadable_items_are_skipped() {
        let (_dir, config) = config();
        let probe = FakeProbe::default()
            .with_list("issue", "octo/issues", "ac:triage", &format!(r#"[{{"number":5,"title":"読めない","labels":[{{"name":"ac:triage"}}],{ME}}}]"#),
            )
            .with_list("pr", "octo/app", "ac:review", &format!(
                    r#"[{{"number":12,"title":"PR","labels":[{{"name":"ac:review"}}],{ME},"author":{{"login":"app/bot"}},"body":"Closes octo/issues#3"}}]"#
                ),
            )
            .with_list("pr", "octo/web", "ac:review", "[]");
        let probe = nothing_waiting(probe, &["octo/issues"], &["octo/app", "octo/web"]);
        // agentcellsが作ったPRに、agentcellsがoperatorを割り当て、reviewのラベルを付けた
        let pr_events = [
            assigned("bot[bot]", "me", "2026-10-25T03:00:00Z"),
            labeled("bot[bot]", "ac:review", "2026-10-25T03:00:00Z"),
        ];
        let probe = with_history(probe, "octo/app", 12, &pr_events, NO_EDITS);
        // 元のIssueは、operatorが承認している
        let probe = approved(probe, "octo/issues", 3, "ac:triage");
        // octo/issues#5 の履歴は、応答を用意していないので読めない
        let found = find(&config, &probe, &token, &actors(), &mut Quiet::default()).unwrap();
        let work: Vec<String> = found.work.iter().map(Work::describe).collect();
        assert_eq!(work, vec!["review PR octo/app#12「PR」"]);
        assert_eq!(found.skipped.len(), 1);
        assert!(
            found.skipped[0].1.starts_with("履歴を読めません"),
            "{:?}",
            found.skipped
        );
    }

    #[test]
    fn replies_from_the_operator_resume_waiting_work_first() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"
operator = "me"

[[product]]
name = "app"
issues = "octo/issues"
repos = ["octo/app"]
cells = ["triage", "implement", "review"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
next = ["implement", "done"]

[[cell]]
name = "implement"
on = ["issue", "pull_request"]
label = "ac:implement"
next = ["review"]

[[cell]]
name = "review"
on = "pull_request"
label = "ac:review"
review = true
next = ["implement", "done"]
"#;
        let config = Config::parse(text, dir.path()).unwrap();
        let comment = |user: &str, body: &str, at: &str| {
            serde_json::json!({"user": {"login": user}, "body": body, "created_at": at, "author_association": "OWNER"})
                .to_string()
        };
        let review = |user: &str, body: &str, at: &str| {
            serde_json::json!({"id": 1, "user": {"login": user}, "body": body, "submitted_at": at, "author_association": "OWNER"})
                .to_string()
        };
        let asked = "[agentcells] triage round 1\n\nAとBのどちらですか\n\n<!-- agentcells read-until 2026-10-25T03:00:00Z -->";
        let probe = FakeProbe::default()
            .with_list("issue", "octo/issues", "ac:triage", "[]")
            .with_list("issue", "octo/issues", "ac:implement", "[]")
            .with_list("pr", "octo/app", "ac:implement", "[]")
            .with_list("pr", "octo/app", "ac:review", "[]")
            .with_list("issue", "octo/issues", "ac:needs-human", &format!(
                    r#"[{{"number":21,"title":"まだ","labels":[{{"name":"ac:needs-human"}}],{ME}}},
                    {{"number":20,"title":"質問","labels":[{{"name":"ac:needs-human"}}],{ME}}},
                    {{"number":22,"title":"やめた","labels":[{{"name":"ac:needs-human"}}],"assignees":[]}},
                    {{"number":23,"title":"もう待っていない","labels":[{{"name":"bug"}}],{ME}}}]"#
                ),
            )
            .with_list("pr", "octo/app", "ac:needs-human", "[]")
            .with_list("pr", "octo/app", "ac:in-review", &format!(
                    r#"[{{"number":30,"title":"検品","labels":[{{"name":"ac:in-review"}}],{ME},"author":{{"login":"me"}}}}]"#
                ),
            );
        // #23: 一覧に返っても、待っているラベルが付いていないので読まない
        // #20: 質問のあとに、operatorが答えた
        let probe = discussion::fake::with_item(
            probe,
            "octo/issues",
            20,
            r#"{"title":"質問","user":{"login":"me"}}"#,
        );
        let probe = discussion::fake::with_list(
            probe,
            "/repos/octo/issues/issues/20/comments",
            &[
                &comment("bot[bot]", asked, "2026-10-25T03:05:00Z"),
                &comment("me", "Bです", "2026-10-25T03:10:00Z"),
            ],
        );
        // #21: 質問のあとは、ほかの人しか書いていない（動かさない）
        let probe = discussion::fake::with_item(
            probe,
            "octo/issues",
            21,
            r#"{"title":"まだ","user":{"login":"me"}}"#,
        );
        let probe = discussion::fake::with_list(
            probe,
            "/repos/octo/issues/issues/21/comments",
            &[
                &comment("bot[bot]", asked, "2026-10-25T03:05:00Z"),
                &comment("other", "Aがいい", "2026-10-25T03:10:00Z"),
            ],
        );
        // #30: レビューが動いている間に、operatorがコメントした（目印より前に並ぶが、読んでいない）
        let probe = discussion::fake::with_item(
            probe,
            "octo/app",
            30,
            r#"{"title":"検品","user":{"login":"me"}}"#,
        )
        .with(
            "gh api /repos/octo/app/pulls/30",
            true,
            r#"{"base":{"ref":"main"},"head":{"ref":"fix"}}"#,
        );
        let probe = discussion::fake::with_list(
            probe,
            "/repos/octo/app/issues/30/comments",
            &[&comment("me", "ついでにREADMEも", "2026-10-25T04:02:00Z")],
        );
        let probe = discussion::fake::with_list(
            probe,
            "/repos/octo/app/pulls/30/reviews",
            &[&review(
                "bot[bot]",
                "[agentcells] review round 1\n\n問題なし\n\n<!-- agentcells read-until 2026-10-25T04:00:00Z -->",
                "2026-10-25T04:05:00Z",
            )],
        );
        let probe = discussion::fake::with_list(probe, "/repos/octo/app/pulls/30/comments", &[]);
        let issue_events = [
            assigned("me", "me", "2026-10-25T01:00:00Z"),
            labeled("me", "ac:triage", "2026-10-25T02:00:00Z"),
            labeled("bot[bot]", "ac:needs-human", "2026-10-25T03:05:00Z"),
        ];
        let probe = with_history(probe, "octo/issues", 20, &issue_events, NO_EDITS);
        let pr_events = [
            assigned("me", "me", "2026-10-25T01:00:00Z"),
            labeled("me", "ac:review", "2026-10-25T02:00:00Z"),
            labeled("bot[bot]", "ac:in-review", "2026-10-25T04:05:00Z"),
        ];
        let probe = with_history(probe, "octo/app", 30, &pr_events, NO_EDITS);

        let found = find(&config, &probe, &token, &actors(), &mut Quiet::default()).unwrap();
        let work: Vec<String> = found.work.iter().map(Work::describe).collect();
        assert_eq!(
            work,
            vec![
                "triage（返事を受けて） Issue octo/issues#20「質問」",
                "implement（返事を受けて） PR octo/app#30「検品」",
            ],
            "{:?}",
            found.skipped
        );
        assert_eq!(found.work[0].waiting, Some(Waiting::NeedsHuman));
        assert_eq!(found.work[1].waiting, Some(Waiting::InReview));
        assert!(found.skipped.is_empty(), "{:?}", found.skipped);
    }

    #[test]
    fn replies_are_guarded_like_labels() {
        let (_dir, config) = config();
        let probe = FakeProbe::default()
            .with_list("issue", "octo/issues", "ac:triage", "[]")
            .with_list("pr", "octo/app", "ac:review", "[]")
            .with_list("pr", "octo/web", "ac:review", "[]")
            .with_list("issue", "octo/issues", "ac:needs-human", &format!(
                    r#"[{{"number":20,"title":"質問","labels":[{{"name":"ac:needs-human"}}],{ME}}}]"#
                ),
            );
        let probe = nothing_waiting(probe, &[], &["octo/app", "octo/web"]);
        let probe = discussion::fake::with_item(
            probe,
            "octo/issues",
            20,
            r#"{"title":"質問","user":{"login":"me"}}"#,
        );
        let probe = discussion::fake::with_list(
            probe,
            "/repos/octo/issues/issues/20/comments",
            &[
                r#"{"user":{"login":"bot[bot]"},"body":"[agentcells] triage round 1\n\n質問","created_at":"2026-10-25T03:05:00Z"}"#,
                r#"{"user":{"login":"me"},"body":"答え","created_at":"2026-10-25T03:10:00Z"}"#,
            ],
        );
        // ほかの人が、ac:needs-human を付け直した
        let events = [
            assigned("me", "me", "2026-10-25T01:00:00Z"),
            labeled("me", "ac:triage", "2026-10-25T02:00:00Z"),
            labeled("eager", "ac:needs-human", "2026-10-25T03:06:00Z"),
        ];
        let probe = with_history(probe, "octo/issues", 20, &events, NO_EDITS);
        let found = find(&config, &probe, &token, &actors(), &mut Quiet::default()).unwrap();
        assert!(found.work.is_empty());
        assert_eq!(found.skipped.len(), 1);
        assert!(
            found.skipped[0]
                .1
                .contains("ac:needs-human」を最後に付けたのが eager"),
            "{:?}",
            found.skipped
        );
    }

    #[test]
    fn waiting_items_without_changes_are_not_read_again() {
        let (_dir, config) = config();
        let home_dir = tempfile::tempdir().unwrap();
        let home = Home::new(home_dir.path().to_path_buf());
        let listed = |updated: &str| {
            format!(
                r#"[{{"number":21,"title":"まだ","labels":[{{"name":"ac:needs-human"}}],{ME},"updatedAt":"{updated}"}}]"#
            )
        };
        let probe_with = |updated: &str| {
            let probe = FakeProbe::default()
                .with_list("issue", "octo/issues", "ac:triage", "[]")
                .with_list("pr", "octo/app", "ac:review", "[]")
                .with_list("pr", "octo/web", "ac:review", "[]")
                .with_list("issue", "octo/issues", "ac:needs-human", &listed(updated));
            let probe = nothing_waiting(probe, &[], &["octo/app", "octo/web"]);
            let probe = discussion::fake::with_item(
                probe,
                "octo/issues",
                21,
                r#"{"title":"まだ","user":{"login":"me"}}"#,
            );
            discussion::fake::with_list(
                probe,
                "/repos/octo/issues/issues/21/comments",
                &[
                    r#"{"user":{"login":"bot[bot]"},"body":"[agentcells] triage round 1\n\n質問","created_at":"2026-10-25T03:05:00Z"}"#,
                ],
            )
        };
        let reads = |probe: &FakeProbe| {
            probe
                .calls
                .borrow()
                .iter()
                .filter(|c| c.contains("/issues/21/comments"))
                .count()
        };
        let now = std::cell::Cell::new(1_000_000);
        let tick = |probe: &FakeProbe| {
            let mut quiet = Quiet::load(&home);
            quiet.now = now.get();
            let found = find(&config, probe, &token, &actors(), &mut quiet).unwrap();
            quiet.save(&home);
            assert!(found.work.is_empty() && found.skipped.is_empty());
        };

        // 返事がないことを読んで、覚える
        let probe = probe_with("2026-10-25T03:05:00Z");
        tick(&probe);
        assert_eq!(reads(&probe), 1);
        // 変わっていなければ、読み直さない（覚えたものは引き継ぐ）
        let probe = probe_with("2026-10-25T03:05:00Z");
        tick(&probe);
        assert_eq!(reads(&probe), 0);
        now.set(now.get() + QUIET_FOR - 1);
        let probe = probe_with("2026-10-25T03:05:00Z");
        tick(&probe);
        assert_eq!(reads(&probe), 0);
        // 読んでから時間がたったら、変わっていなくても読み直す（同じ秒に届いた返事を、取りこぼしたままにしない）
        now.set(now.get() + 1);
        let probe = probe_with("2026-10-25T03:05:00Z");
        tick(&probe);
        assert_eq!(reads(&probe), 1);
        // 更新されたら、読み直す
        let probe = probe_with("2026-10-25T03:10:00Z");
        tick(&probe);
        assert_eq!(reads(&probe), 1);
    }

    #[test]
    fn later_steps_in_the_flow_are_deeper() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"
operator = "me"

[[product]]
name = "app"
issues = "octo/app"
repos = ["octo/app"]
cells = ["triage", "implement", "review", "assess"]

[[product]]
name = "loop"
issues = "octo/loop"
repos = ["octo/loop"]
cells = ["implement", "review"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
next = ["implement", "done"]

[[cell]]
name = "implement"
on = ["issue", "pull_request"]
label = "ac:implement"
next = ["review"]

[[cell]]
name = "review"
on = "pull_request"
label = "ac:review"
next = ["implement", "done"]

[[cell]]
name = "assess"
on = "issue"
label = "ac:assess"
next = ["implement", "done"]
"#;
        let config = Config::parse(text, dir.path()).unwrap();
        let app = depths(&config, &config.products[0]);
        assert_eq!(app["triage"], 0);
        assert_eq!(app["assess"], 0);
        assert_eq!(app["implement"], 1);
        // 差し戻しの輪（review → implement）があっても、いちばん短い数
        assert_eq!(app["review"], 2);
        // 入口がない（すべてが輪）なら、最初のcellを入口にする
        let looped = depths(&config, &config.products[1]);
        assert_eq!(looped["implement"], 0);
        assert_eq!(looped["review"], 1);
        // 入口からたどれない輪があっても、どのcellにも深さが付く
        let mut apart = config.clone();
        apart.products[0]
            .next
            .insert("triage".into(), vec!["done".into()]);
        apart.products[0]
            .next
            .insert("assess".into(), vec!["done".into()]);
        let d = depths(&apart, &apart.products[0]);
        assert_eq!((d["triage"], d["implement"], d["review"]), (0, 0, 1));

        // 後ろの工程から、同じ工程ならPRを先に、それ以外は今の順のまま並べる
        let w = |cell: &str, item: Target, number: u64| Work {
            product: "app".into(),
            cell: cell.into(),
            item,
            repo: RepoName::parse("octo/app").unwrap(),
            number,
            title: String::new(),
            waiting: None,
        };
        let mut work = vec![
            w("triage", Target::Issue, 1),
            w("assess", Target::Issue, 2),
            w("implement", Target::Issue, 3),
            w("implement", Target::PullRequest, 4),
            w("review", Target::PullRequest, 5),
            w("triage", Target::Issue, 6),
        ];
        order(&config, &mut work);
        let numbers: Vec<u64> = work.iter().map(|w| w.number).collect();
        assert_eq!(numbers, [5, 4, 3, 1, 2, 6]);
    }

    #[test]
    fn list_failures_point_to_doctor() {
        let (_dir, config) = config();
        let probe = FakeProbe::default().with_fallback(false);
        let err = find(&config, &probe, &token, &actors(), &mut Quiet::default()).unwrap_err();
        assert!(err.hint.unwrap().contains("agentcells doctor"));
    }

    fn work(cell: &str, number: u64) -> Work {
        Work {
            product: "app".to_string(),
            cell: cell.to_string(),
            item: Target::Issue,
            repo: RepoName::parse("octo/issues").unwrap(),
            number,
            title: String::new(),
            waiting: None,
        }
    }

    #[test]
    fn a_session_runs_until_time_or_runs_run_out_and_skips_what_was_skipped() {
        let mut session = Session::new(std::time::Duration::from_secs(3600));
        let found = [work("triage", 1), work("triage", 2)];
        assert!(!session.exhausted());
        assert_eq!(session.next(&found).map(|w| w.number), Some(1));
        // 進んだものは、また拾ってよい
        session.ran(&found[0], false);
        assert_eq!(session.next(&found).map(|w| w.number), Some(1));
        // 見送ったものは、このtickでは拾わない
        session.ran(&found[0], true);
        assert_eq!(session.next(&found).map(|w| w.number), Some(2));
        session.ran(&found[1], true);
        assert!(session.next(&found).is_none());
        assert!(!session.exhausted());
        for _ in session.runs()..MAX_RUNS {
            session.ran(&found[1], false);
        }
        assert!(session.exhausted());

        // AIの実行が失敗したら、続けない
        let mut halted = Session::new(std::time::Duration::from_secs(3600));
        halted.ran(&found[0], false);
        assert!(!halted.exhausted());
        halted.halt();
        assert!(halted.exhausted());

        // 時間の上限が0でも、1件は動かす
        let mut once = Session::new(std::time::Duration::ZERO);
        assert!(!once.exhausted());
        once.ran(&found[0], false);
        assert!(once.exhausted());
    }

    #[test]
    fn only_one_tick_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().to_path_buf());
        let first = lock(&home).unwrap();
        assert!(first.is_some());
        assert!(lock(&home).unwrap().is_none());
        drop(first);
        // ほかのテストが同時に子のプロセスを作ると、forkしてからexecするまでの間、子もこのファイルを
        // 開いたままになり、ロックがすぐには外れないことがある。少し待って確かめる
        let again = (0..50).find_map(|_| {
            let lock = lock(&home).unwrap();
            if lock.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            lock
        });
        assert!(again.is_some());
    }
}
