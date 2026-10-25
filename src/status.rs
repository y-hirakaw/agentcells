//! `agentcells status`。agentcellsが扱っているIssue/PRごとに、今どこまで進んでいるかを1行で出す。
//!
//! ラベルは、次に動くもの（Issueか、そこから作ったPR）に付いている。PRができると、Issueにはラベルが
//! なくなるので、次のように出す。
//! - ラベルのあるPRは、どれも1行ずつ出す。つながった開いているIssueがあれば、それと並べる
//! - Issueにラベルがあれば、それも出す（つながったPRにもラベルがあれば、同じIssueが2行に出る）
//! - どちらにもなく、agentcellsが作ったPRがあれば、「ラベルなし」（cellは動かない）
//!
//! PRとIssueのつながりは、PRの本文の `Closes`、GitHubが覚えているつながり（`closingIssuesReferences`）、
//! agentcellsのブランチ名（`ac/<owner>-<repo>-<番号>`）のどれかで見る。Issueと作業するリポジトリが
//! 別のときに、GitHubがつながりを返すかを確かめていないので、ブランチ名でも見る。
//!
//! 読むだけで、何も書き換えない。リポジトリごとに、GraphQLで1回だけ読む。

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::config::{Config, Product};
use crate::error::{Error, Result};
use crate::github::Credentials;
use crate::guard::Actors;
use crate::home::RepoName;
use crate::labels::{IN_REVIEW, NEEDS_HUMAN, WORKING};
use crate::probe::Probe;

/// Issueの状態。並べる順に書く。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// cellが動いている（`ac:working`）
    Working { cell: Option<String> },
    /// 人間の回答や作業を待っている（`ac:needs-human`）
    NeedsHuman,
    /// 人間の検品を待っている（`ac:in-review`）
    InReview,
    /// cellのラベルが付いていて、tickで拾われるのを待っている
    Queued { cell: String },
    /// agentcellsが作ったPRはあるが、ラベルがない。cellは動かない
    Unlabeled,
}

impl State {
    fn describe(&self) -> String {
        match self {
            State::Working { cell: Some(cell) } => format!("作業中 {cell}"),
            State::Working { cell: None } => "作業中".to_string(),
            State::NeedsHuman => "返事待ち".to_string(),
            State::InReview => "検品待ち".to_string(),
            State::Queued { cell } => format!("順番待ち {cell}"),
            State::Unlabeled => "ラベルなし".to_string(),
        }
    }
}

/// 1行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub state: State,
    /// 元のIssue。人が作ったPRで、つながったIssueがなければ `None`
    pub issue: Option<Item>,
    /// つながったPR（状態をPRから読んだときは、そのPR）
    pub pr: Option<Item>,
    /// 状態を決めたラベルが付いた時刻（GitHubの時刻）
    pub since: Option<String>,
    /// 動かない理由など（目安。動くかどうかはtickが決める）
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub repo: RepoName,
    pub number: u64,
    pub title: String,
}

/// productごとの結果。
#[derive(Debug, Default)]
pub struct Report {
    pub lines: Vec<Line>,
    /// 読めなかったリポジトリ
    pub errors: Vec<String>,
    /// 多すぎて全部を見ていないことなど
    pub notes: Vec<String>,
}

/// 1回に読む件数。これより多ければ、更新の新しい順にこれだけ見る。[`QUERY`] の `first` も同じ数にする。
const LIMIT: usize = 100;

/// 開いているIssueとPRを、ラベル、Assignees、ラベルを付けた記録、つながったIssue、PRの作者と一緒に読む。
pub(crate) const QUERY: &str = "query($owner: String!, $name: String!, $issues: Boolean!, $prs: Boolean!) { repository(owner: $owner, name: $name) { issues(states: OPEN, first: 100, orderBy: {field: UPDATED_AT, direction: DESC}) @include(if: $issues) { totalCount nodes { number title labels(first: 20) { nodes { name } } assignees(first: 10) { nodes { login } } timelineItems(itemTypes: [LABELED_EVENT], last: 50) { nodes { ... on LabeledEvent { createdAt actor { __typename login } label { name } } } } } } pullRequests(states: OPEN, first: 100, orderBy: {field: UPDATED_AT, direction: DESC}) @include(if: $prs) { totalCount nodes { number title body headRefName author { __typename login } labels(first: 20) { nodes { name } } assignees(first: 10) { nodes { login } } timelineItems(itemTypes: [LABELED_EVENT], last: 50) { nodes { ... on LabeledEvent { createdAt actor { __typename login } label { name } } } } closingIssuesReferences(first: 10) { nodes { number repository { nameWithOwner } } } } } } }";

#[derive(Deserialize)]
struct Response {
    data: Data,
}

#[derive(Deserialize)]
struct Data {
    repository: Option<Repository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    #[serde(default)]
    issues: Option<Connection>,
    #[serde(default)]
    pull_requests: Option<Connection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection {
    total_count: usize,
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Node {
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    head_ref_name: String,
    /// PRの作者（消えたアカウントなら `None`）
    #[serde(default)]
    author: Option<Author>,
    labels: Nodes<Name>,
    assignees: Nodes<Login>,
    timeline_items: Nodes<Labeled>,
    #[serde(default)]
    closing_issues_references: Option<Nodes<Reference>>,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct Name {
    name: String,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct Author {
    login: String,
    #[serde(rename = "__typename", default)]
    typename: Option<String>,
}

/// ラベルを付けた記録。`itemTypes` で絞っても、型の合わないものが空で入ることがあるので、どれも省略できる。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Labeled {
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    actor: Option<Author>,
    #[serde(default)]
    label: Option<Name>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reference {
    number: u64,
    repository: RepositoryName,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryName {
    name_with_owner: String,
}

impl Node {
    fn has_label(&self, label: &str) -> bool {
        self.labels
            .nodes
            .iter()
            .any(|l| l.name.eq_ignore_ascii_case(label))
    }

    fn is_assigned(&self, operator: &str) -> bool {
        self.assignees
            .nodes
            .iter()
            .any(|a| a.login.eq_ignore_ascii_case(operator))
    }

    /// 最後にそのラベルを付けた記録。
    fn last_labeled(&self, label: &str) -> Option<&Labeled> {
        self.timeline_items
            .nodes
            .iter()
            .filter(|e| {
                !e.created_at.is_empty()
                    && e.label
                        .as_ref()
                        .is_some_and(|l| l.name.eq_ignore_ascii_case(label))
            })
            .max_by(|a, b| a.created_at.cmp(&b.created_at))
    }

    fn is_by_writer(&self, actors: &Actors) -> bool {
        self.author
            .as_ref()
            .is_some_and(|a| actors.is_writer_graphql(&a.login, a.typename.as_deref()))
    }

    /// ラベルから読んだ状態と、それを決めたラベル。tickと同じく、cellのラベルは人間を待っているラベルより強い。
    fn state(&self, cells: &[(String, String)]) -> Option<(State, String)> {
        let cell = cells
            .iter()
            .find(|(label, _)| self.has_label(label))
            .map(|(label, name)| (label.clone(), name.clone()));
        if self.has_label(WORKING) {
            let state = State::Working {
                cell: cell.map(|(_, name)| name),
            };
            return Some((state, WORKING.to_string()));
        }
        if let Some((label, name)) = cell {
            return Some((State::Queued { cell: name }, label));
        }
        if self.has_label(NEEDS_HUMAN) {
            return Some((State::NeedsHuman, NEEDS_HUMAN.to_string()));
        }
        if self.has_label(IN_REVIEW) {
            return Some((State::InReview, IN_REVIEW.to_string()));
        }
        None
    }

    fn item(&self, repo: &RepoName) -> Item {
        Item {
            repo: repo.clone(),
            number: self.number,
            title: self.title.clone(),
        }
    }
}

/// PRが、productのIssueのリポジトリのどのIssueにつながっているか。
///
/// ブランチ名は誰でも付けられるので、agentcellsが作ったPRのときだけ見る。
fn linked_issues(pr: &Node, repo: &RepoName, issues: &RepoName, actors: &Actors) -> BTreeSet<u64> {
    let mut linked: BTreeSet<u64> = closes(pr, repo, issues).into_iter().collect();
    let issues_name = issues.to_string();
    for r in pr
        .closing_issues_references
        .iter()
        .flat_map(|c| &c.nodes)
        .filter(|r| {
            r.repository
                .name_with_owner
                .eq_ignore_ascii_case(&issues_name)
        })
    {
        linked.insert(r.number);
    }
    if pr.is_by_writer(actors)
        && let Some(number) = branch_issue(&pr.head_ref_name, issues)
    {
        linked.insert(number);
    }
    linked
}

/// PRの本文の `Closes` が指す、productのIssueの番号。ガード（tick）が元のIssueとして確かめるのは、これだけ。
fn closes(pr: &Node, repo: &RepoName, issues: &RepoName) -> Option<u64> {
    crate::prompt::closes(&pr.body, repo)
        .filter(|(closes_repo, _)| closes_repo == issues)
        .map(|(_, number)| number)
}

/// agentcellsのブランチ名（[`crate::pr::branch_name`]）から、Issueの番号を読む。
fn branch_issue(branch: &str, issues: &RepoName) -> Option<u64> {
    let prefix = crate::pr::branch_name(issues, 0);
    let prefix = prefix.strip_suffix('0')?;
    branch.strip_prefix(prefix)?.parse().ok()
}

fn fetch(
    probe: &dyn Probe,
    credentials: &Credentials,
    repo: &RepoName,
    issues: bool,
    prs: bool,
) -> Result<Repository> {
    let env = credentials.gh_env()?;
    let query = format!("query={QUERY}");
    let owner = format!("owner={}", repo.owner);
    let name = format!("name={}", repo.name);
    // -F は数字だけの名前を数に変えるので、名前は -f で渡す。真偽値だけ -F で渡す
    let issues = format!("issues={issues}");
    let prs = format!("prs={prs}");
    let args = [
        "api",
        "graphql",
        "-f",
        query.as_str(),
        "-f",
        owner.as_str(),
        "-f",
        name.as_str(),
        "-F",
        issues.as_str(),
        "-F",
        prs.as_str(),
    ];
    let out = probe.run_with_env("gh", &args, &env).ok_or_else(|| {
        Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if !out.success {
        return Err(Error::new(format!(
            "{repo} を読めません: {}",
            out.stderr.trim()
        )));
    }
    let response: Response = serde_json::from_str(&out.stdout)
        .map_err(|e| Error::new(format!("{repo} を読めません: {e}")))?;
    response
        .data
        .repository
        .ok_or_else(|| Error::new(format!("{repo} が見つかりません")))
}

/// productの、Issueごとの状態を読む。読めなかったリポジトリは、`errors` に書いて飛ばす。
///
/// `credentials_for` は、リポジトリを読むための認証を返す（tickと同じく、トークンと空の設定のghで読む）。
/// 動かない理由の注記は目安で、動くかどうかはtickが決める（`tick --dry-run` の見送りの理由）。
pub fn product(
    config: &Config,
    product: &Product,
    probe: &dyn Probe,
    credentials_for: &dyn Fn(&RepoName) -> Result<Credentials>,
    actors: &Actors,
) -> Report {
    let mut report = Report::default();
    let cells: Vec<(String, String)> = product
        .cells
        .iter()
        .filter_map(|name| config.cell(name))
        .map(|c| (c.label.clone(), c.name.clone()))
        .collect();

    // Issueのリポジトリが作業するリポジトリでもあれば、1回で両方を読む
    let mut repos: Vec<&RepoName> = vec![&product.issues];
    repos.extend(product.repos.iter().filter(|r| *r != &product.issues));
    let mut issues: Vec<Node> = Vec::new();
    let mut prs: Vec<(RepoName, Node)> = Vec::new();
    for repo in repos {
        let want_issues = repo == &product.issues;
        let want_prs = product.repos.contains(repo);
        let read =
            credentials_for(repo).and_then(|c| fetch(probe, &c, repo, want_issues, want_prs));
        let read = match read {
            Ok(read) => read,
            Err(e) => {
                report.errors.push(e.message);
                continue;
            }
        };
        let mut take = |connection: Option<Connection>, kind: &str| {
            let connection = connection?;
            if connection.total_count > LIMIT {
                report.notes.push(format!(
                    "{repo} の開いている{kind}は {} 件あるので、更新の新しい {LIMIT} 件だけを見ています",
                    connection.total_count
                ));
            }
            Some(connection.nodes)
        };
        if let Some(nodes) = take(read.issues, "Issue") {
            issues.extend(nodes);
        }
        if let Some(nodes) = take(read.pull_requests, "PR") {
            prs.extend(nodes.into_iter().map(|n| (repo.clone(), n)));
        }
    }

    let operator = actors.operator.as_str();
    let not_assigned =
        |what: &str| format!("{what}の Assignees に {operator} がいないので、動きません");
    let find_issue = |number: u64| issues.iter().find(|i| i.number == number);
    let links: Vec<BTreeSet<u64>> = prs
        .iter()
        .map(|(repo, pr)| linked_issues(pr, repo, &product.issues, actors))
        .collect();
    // ラベルのあるPRから、どれかの行に出たIssue
    let mut covered: BTreeSet<u64> = BTreeSet::new();

    // ラベルのあるPRは、どれも1行ずつ出す。つながった開いているIssueがあれば、それと並べる
    for (i, (repo, pr)) in prs.iter().enumerate() {
        let Some((state, label)) = pr.state(&cells) else {
            continue;
        };
        let closes = closes(pr, repo, &product.issues);
        // 本文の Closes が指すIssueを先に（ガードが確かめるのは、それだけなので）
        let issue = closes
            .into_iter()
            .chain(links[i].iter().copied())
            .find_map(find_issue);
        if let Some(issue) = issue {
            covered.insert(issue.number);
        }
        let note = if !pr.is_assigned(operator) {
            Some(not_assigned("PR"))
        } else {
            match closes.map(|n| (n, find_issue(n))) {
                Some((_, Some(original))) if !original.is_assigned(operator) => {
                    Some(not_assigned("元のIssue"))
                }
                Some((n, None)) => Some(format!(
                    "元のIssue #{n} が閉じているか、読んだ範囲の外にあります"
                )),
                _ => labeler_note(&state, pr.last_labeled(&label), actors),
            }
        };
        report.lines.push(Line {
            state,
            issue: issue.map(|issue| issue.item(&product.issues)),
            pr: Some(pr.item(repo)),
            since: pr.last_labeled(&label).map(|e| e.created_at.clone()),
            note,
        });
    }

    for issue in &issues {
        // agentcellsが作った、つながったPR
        let ours = (0..prs.len())
            .find(|&i| links[i].contains(&issue.number) && prs[i].1.is_by_writer(actors));
        let (state, labeled) = if let Some((state, label)) = issue.state(&cells) {
            (state, issue.last_labeled(&label))
        } else if covered.contains(&issue.number) || ours.is_none() {
            // PRの行に出したか、agentcellsが扱っていない
            continue;
        } else {
            (State::Unlabeled, None)
        };
        let note = if issue.is_assigned(operator) {
            labeler_note(&state, labeled, actors)
        } else {
            Some(not_assigned("Issue"))
        };
        report.lines.push(Line {
            state,
            issue: Some(issue.item(&product.issues)),
            pr: ours.map(|i| prs[i].1.item(&prs[i].0)),
            since: labeled.map(|e| e.created_at.clone()),
            note,
        });
    }

    report.lines.sort_by(|a, b| {
        let number = |l: &Line| l.issue.as_ref().or(l.pr.as_ref()).map(|i| i.number);
        a.state
            .cmp(&b.state)
            .then_with(|| a.issue.is_none().cmp(&b.issue.is_none()))
            .then_with(|| number(a).cmp(&number(b)))
    });
    report
}

/// 順番待ちで、cellのラベルを最後に付けたのが、operatorでもagentcellsでもなければ、tickは拾わない（guard）。
fn labeler_note(state: &State, labeled: Option<&Labeled>, actors: &Actors) -> Option<String> {
    if !matches!(state, State::Queued { .. }) {
        return None;
    }
    let Some(actor) = labeled?.actor.as_ref() else {
        // 消えたアカウント。ガードは信頼しない
        return Some("ラベルを付けた人がわからないので、動きません".to_string());
    };
    (!actors.is_trusted_graphql(&actor.login, actor.typename.as_deref()))
        .then(|| format!("ラベルを付けたのが {} なので、動きません", actor.login))
}

impl Line {
    /// 1行で書く。`now` は今の時刻（UNIXの秒）で、ラベルが付いてからの長さを出すのに使う。
    pub fn describe(&self, now: u64) -> String {
        let mut text = self.state.describe();
        match (&self.issue, &self.pr) {
            (Some(issue), pr) => {
                text.push_str(&format!(
                    "  {}#{}「{}」",
                    issue.repo, issue.number, issue.title
                ));
                if let Some(pr) = pr {
                    if pr.repo == issue.repo {
                        text.push_str(&format!(" → PR #{}", pr.number));
                    } else {
                        text.push_str(&format!(" → PR {}#{}", pr.repo, pr.number));
                    }
                }
            }
            (None, Some(pr)) => {
                text.push_str(&format!("  PR {}#{}「{}」", pr.repo, pr.number, pr.title))
            }
            (None, None) => {}
        }
        if let Some(elapsed) = self
            .since
            .as_deref()
            .and_then(unix_seconds)
            .map(|at| elapsed(now.saturating_sub(at)))
        {
            text.push_str(&format!("（{elapsed}前から）"));
        }
        if let Some(note) = &self.note {
            text.push_str(&format!("  ※{note}"));
        }
        text
    }
}

/// 経過した長さを、おおまかに書く。
fn elapsed(seconds: u64) -> String {
    match seconds {
        s if s < 60 => "1分足らず".to_string(),
        s if s < 60 * 60 => format!("{}分", s / 60),
        s if s < 24 * 60 * 60 => format!("{}時間", s / 3600),
        s => format!("{}日", s / 86400),
    }
}

/// GitHubの時刻（`2026-10-25T03:05:00Z`）を、UNIXの秒にする。
fn unix_seconds(at: &str) -> Option<u64> {
    let at = at.strip_suffix('Z')?;
    let (date, time) = at.split_once('T')?;
    let mut date = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (date.next()??, date.next()??, date.next()??);
    let mut time = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, min, s) = (time.next()??, time.next()??, time.next()??);
    // 1970-01-01からの日数（Howard Hinnantのdays_from_civil）
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + h * 3600 + min * 60 + s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Source;
    use crate::probe::fake::FakeProbe;

    fn config() -> (tempfile::TempDir, Config) {
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
next = ["implement"]

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
        (dir, config)
    }

    fn actors() -> Actors {
        Actors {
            operator: "me".to_string(),
            writer: "bot[bot]".to_string(),
        }
    }

    /// agentcellsが作ったPR（GraphQLは、Appを `[bot]` なしで返す）
    const OURS: &str = r#","author":{"__typename":"Bot","login":"bot"}"#;

    fn token(repo: &RepoName) -> Result<Credentials> {
        Credentials::new(Source::App, format!("ghs_{}", repo.name))
    }

    fn command(repo: &str, issues: bool, prs: bool) -> String {
        let (owner, name) = repo.split_once('/').unwrap();
        format!(
            "gh api graphql -f query={QUERY} -f owner={owner} -f name={name} -F issues={issues} -F prs={prs}"
        )
    }

    fn node(number: u64, title: &str, labels: &[&str], assigned: bool, extra: &str) -> String {
        let labels: Vec<String> = labels
            .iter()
            .map(|l| format!(r#"{{"name":"{l}"}}"#))
            .collect();
        // 型の合わない記録は、空で入る
        let events: Vec<String> = std::iter::once("{}".to_string())
            .chain(
                labels
                    .iter()
                    .map(|l| format!(r#"{{"createdAt":"2026-10-25T01:00:00Z","actor":{{"__typename":"User","login":"me"}},"label":{l}}}"#)),
            )
            .collect();
        let assignees = if assigned { r#"{"login":"me"}"# } else { "" };
        format!(
            r#"{{"number":{number},"title":"{title}","labels":{{"nodes":[{}]}},"assignees":{{"nodes":[{assignees}]}},"timelineItems":{{"nodes":[{}]}}{extra}}}"#,
            labels.join(","),
            events.join(",")
        )
    }

    fn connection(key: &str, nodes: &[String]) -> String {
        format!(
            r#""{key}":{{"totalCount":{},"nodes":[{}]}}"#,
            nodes.len(),
            nodes.join(",")
        )
    }

    fn response(parts: &[String]) -> String {
        format!(r#"{{"data":{{"repository":{{{}}}}}}}"#, parts.join(","))
    }

    #[test]
    fn issues_without_labels_take_the_state_of_their_pull_request() {
        let (_dir, config) = config();
        let issues = [
            node(1, "PRを作った", &[], true, ""),
            node(2, "質問した", &["ac:needs-human"], true, ""),
            node(3, "待っている", &["ac:triage"], false, ""),
            node(4, "関係ない", &[], true, ""),
            node(5, "人のPRだけ", &[], true, ""),
            node(6, "止めた", &[], true, ""),
            node(7, "両方", &["ac:needs-human"], true, ""),
        ];
        let prs = [
            // 本文の Closes でつながる
            node(
                10,
                "検品して",
                &["ac:in-review"],
                true,
                r#","body":"[agentcells] review round 1\n\nCloses octo/issues#1","headRefName":"feature""#,
            ),
            // agentcellsのPRは、ブランチ名でもつながる。ラベルがない
            node(
                11,
                "止めたPR",
                &[],
                true,
                &format!(r#","body":"","headRefName":"ac/octo-issues-6"{OURS}"#),
            ),
            // 人のPRは、ブランチ名ではつながらない
            node(
                15,
                "まねたPR",
                &[],
                true,
                r#","body":"","headRefName":"ac/octo-issues-4","author":{"__typename":"User","login":"alice"}"#,
            ),
            // Issueにラベルがあっても、つながったPRのラベルも出す
            node(
                14,
                "直している",
                &["ac:implement", "ac:working"],
                true,
                r#","body":"Closes octo/issues#7","headRefName":"fix","author":{"__typename":"User","login":"alice"}"#,
            ),
            // ほかの人がラベルを付けたものは、tickが拾わない
            node(
                16,
                "人が付けた",
                &["ac:review"],
                true,
                r#","body":"","headRefName":"z""#,
            )
            .replace(r#""login":"me"},"label""#, r#""login":"alice"},"label""#),
            // GitHubのつながりだけ。ラベルがない人のPRは出さない
            node(
                12,
                "人のPR",
                &[],
                true,
                r#","body":"","headRefName":"x","closingIssuesReferences":{"nodes":[{"number":5,"repository":{"nameWithOwner":"octo/issues"}}]}"#,
            ),
            // Issueのない、人のPR
            node(
                13,
                "見て",
                &["ac:review", "ac:working"],
                true,
                r#","body":"","headRefName":"y""#,
            ),
        ];
        let probe = FakeProbe::default()
            .with(
                &command("octo/issues", true, false),
                true,
                &response(&[connection("issues", &issues)]),
            )
            .with(
                &command("octo/app", false, true),
                true,
                &response(&[connection("pullRequests", &prs)]),
            );

        let report = product(&config, &config.products[0], &probe, &token, &actors());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.notes.is_empty(), "{:?}", report.notes);
        let now = unix_seconds("2026-10-25T04:30:00Z").unwrap();
        let lines: Vec<String> = report.lines.iter().map(|l| l.describe(now)).collect();
        assert_eq!(
            lines,
            [
                "作業中 implement  octo/issues#7「両方」 → PR octo/app#14（3時間前から）",
                "作業中 review  PR octo/app#13「見て」（3時間前から）",
                "返事待ち  octo/issues#2「質問した」（3時間前から）",
                "返事待ち  octo/issues#7「両方」（3時間前から）",
                "検品待ち  octo/issues#1「PRを作った」 → PR octo/app#10（3時間前から）",
                "順番待ち review  PR octo/app#16「人が付けた」（3時間前から）  ※ラベルを付けたのが alice なので、動きません",
                "順番待ち triage  octo/issues#3「待っている」（3時間前から）  ※Issueの Assignees に me がいないので、動きません",
                "ラベルなし  octo/issues#6「止めた」 → PR octo/app#11",
            ]
        );
    }

    #[test]
    fn a_repository_used_for_both_is_read_once_and_failures_are_noted() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"
operator = "me"

[[product]]
name = "app"
issues = "octo/app"
repos = ["octo/app", "octo/web"]
cells = ["review"]

[[cell]]
name = "review"
on = "pull_request"
label = "ac:review"
next = ["done"]
"#;
        let config = Config::parse(text, dir.path()).unwrap();
        let issues = [node(1, "直して", &[], true, "")];
        let prs = [node(
            2,
            "直した",
            &["ac:review"],
            true,
            r#","body":"Closes #1","headRefName":"z""#,
        )];
        let probe = FakeProbe::default()
            .with(
                &command("octo/app", true, true),
                true,
                &response(&[
                    connection("issues", &issues),
                    connection("pullRequests", &prs),
                ]),
            )
            .with(&command("octo/web", false, true), false, "");

        let report = product(&config, &config.products[0], &probe, &token, &actors());
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("octo/web"), "{:?}", report.errors);
        assert_eq!(report.lines.len(), 1);
        let line = &report.lines[0];
        assert_eq!(
            line.state,
            State::Queued {
                cell: "review".into()
            }
        );
        assert_eq!(line.pr.as_ref().map(|p| p.number), Some(2));
        assert_eq!(probe.calls.borrow().len(), 2);
    }

    #[test]
    fn reads_github_times() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2026-10-25T03:05:00Z"), Some(1_792_897_500));
        assert_eq!(unix_seconds("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(unix_seconds("2026-10-25"), None);
        assert_eq!(elapsed(30), "1分足らず");
        assert_eq!(elapsed(3 * 86400 + 5), "3日");
    }
}
