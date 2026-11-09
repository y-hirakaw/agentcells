//! 偽のGitHub。ghの引数を解いて、手元の状態を読み書きする（`Probe` として渡す）。
//! gitは本物を動かし、`https://github.com/<owner>/<repo>.git` を手元のbareリポジトリに書き換える。
//!
//! GitHubについて確かめた事実のうち、CLIが頼っているものは、偽物でも同じにする。
//! - ラベル名は大文字・小文字を区別しない
//! - 時刻は秒までのISO 8601（UTC）で、文字列の順が時刻の順になる
//! - `userContentEdits` は新しい順、コメントとレビューは古い順に返る

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentcells::config::Target;
use agentcells::probe::{Output, Probe, SystemProbe};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Default, Serialize)]
pub struct State {
    /// 偽の時計（UNIXの秒）。書き込み（ghの呼び出しか、人の操作）1回ごとに1秒進める
    pub clock: u64,
    /// 止めている間は、時計を進めない（同じ秒の書き込みを試す）
    pub frozen: bool,
    /// コメントのID
    next_id: u64,
    /// トークン → ログイン名
    pub tokens: BTreeMap<String, String>,
    /// `gh auth token` が返すトークン（ghにログインしているアカウント）
    pub gh_token: String,
    pub repos: BTreeMap<String, Repo>,
    /// 解けなかった呼び出し。tickのたびに0件であること
    pub unknown: Vec<String>,
    /// 偽物が失敗を返した呼び出し（ないラベル、知らないトークンなど）。場面が予期しなければ0件であること
    pub failed: Vec<String>,
    /// 書き込みの記録（誰が、何をしたか）
    pub writes: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Repo {
    /// 持ち主（`author_association` が `OWNER`）
    pub owner: String,
    /// 持ち主のorganizationのメンバー（`MEMBER`）
    pub members: Vec<String>,
    /// リポジトリに招かれた人（`COLLABORATOR`）
    pub collaborators: Vec<String>,
    /// リポジトリにあるラベル。この書き方（大文字・小文字）で返す
    pub labels: Vec<String>,
    pub items: BTreeMap<u64, Item>,
    /// 手元のbareリポジトリ
    pub bare: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub number: u64,
    pub pr: bool,
    pub title: String,
    pub body: String,
    pub author: String,
    pub open: bool,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub comments: Vec<Comment>,
    pub events: Vec<Event>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub event: String,
    pub actor: String,
    pub label: Option<String>,
    pub assignee: Option<String>,
    pub created_at: String,
}

impl State {
    /// 書き込み1回分の時刻。止めていなければ、時計を1秒進める。
    pub fn now(&mut self) -> String {
        if !self.frozen {
            self.clock += 1;
        }
        iso(self.clock)
    }

    pub fn repo(&mut self, repo: &str) -> &mut Repo {
        self.repos
            .get_mut(repo)
            .unwrap_or_else(|| panic!("偽のGitHubに {repo} がありません"))
    }

    pub fn item(&mut self, repo: &str, number: u64) -> &mut Item {
        self.repo(repo)
            .items
            .get_mut(&number)
            .unwrap_or_else(|| panic!("偽のGitHubに {repo}#{number} がありません"))
    }

    /// Issueを作る。番号を返す。
    pub fn open_issue(&mut self, repo: &str, author: &str, title: &str, body: &str) -> u64 {
        let at = self.now();
        let r = self.repo(repo);
        let number = r.items.keys().max().copied().unwrap_or(0) + 1;
        r.items.insert(
            number,
            Item {
                number,
                pr: false,
                title: title.to_string(),
                body: body.to_string(),
                author: author.to_string(),
                open: true,
                labels: Vec::new(),
                assignees: Vec::new(),
                comments: Vec::new(),
                events: Vec::new(),
                created_at: at.clone(),
                updated_at: at,
            },
        );
        number
    }

    /// ラベルを付ける。リポジトリにないラベルがあれば、何も変えずに失敗する（ghと同じ）。
    /// 付けるのは、リポジトリにある書き方。もう付いていれば、出来事を残さない。
    pub fn add_labels(
        &mut self,
        repo: &str,
        number: u64,
        actor: &str,
        labels: &[&str],
        at: &str,
    ) -> Result<(), String> {
        let existing = &self.repo(repo).labels;
        let names = labels
            .iter()
            .map(|label| {
                existing
                    .iter()
                    .find(|l| l.eq_ignore_ascii_case(label))
                    .cloned()
                    .ok_or_else(|| format!("could not add label: '{label}' not found"))
            })
            .collect::<Result<Vec<String>, String>>()?;
        let item = self.item(repo, number);
        for name in names {
            if item.labels.iter().any(|l| l == &name) {
                continue;
            }
            item.labels.push(name.clone());
            item.events.push(Event {
                event: "labeled".into(),
                actor: actor.into(),
                label: Some(name),
                assignee: None,
                created_at: at.to_string(),
            });
            item.updated_at = at.to_string();
        }
        Ok(())
    }

    /// ラベルを外す。リポジトリにないラベルがあれば、何も変えずに失敗する（ghと同じ）。付いていなければ何もしない。
    pub fn remove_labels(
        &mut self,
        repo: &str,
        number: u64,
        actor: &str,
        labels: &[&str],
        at: &str,
    ) -> Result<(), String> {
        let existing = &self.repo(repo).labels;
        if let Some(missing) = labels
            .iter()
            .find(|label| !existing.iter().any(|l| l.eq_ignore_ascii_case(label)))
        {
            return Err(format!("could not remove label: '{missing}' not found"));
        }
        let item = self.item(repo, number);
        for label in labels {
            let Some(i) = item
                .labels
                .iter()
                .position(|l| l.eq_ignore_ascii_case(label))
            else {
                continue;
            };
            let name = item.labels.remove(i);
            item.events.push(Event {
                event: "unlabeled".into(),
                actor: actor.into(),
                label: Some(name),
                assignee: None,
                created_at: at.to_string(),
            });
            item.updated_at = at.to_string();
        }
        Ok(())
    }

    pub fn assign(&mut self, repo: &str, number: u64, actor: &str, assignee: &str) {
        let at = self.now();
        let item = self.item(repo, number);
        if !item.assignees.iter().any(|a| a == assignee) {
            item.assignees.push(assignee.to_string());
        }
        item.events.push(Event {
            event: "assigned".into(),
            actor: actor.into(),
            label: None,
            assignee: Some(assignee.to_string()),
            created_at: at.clone(),
        });
        item.updated_at = at;
    }

    pub fn comment(&mut self, repo: &str, number: u64, author: &str, body: &str) {
        let at = self.now();
        self.next_id += 1;
        let id = self.next_id;
        let item = self.item(repo, number);
        item.comments.push(Comment {
            id,
            author: author.to_string(),
            body: body.to_string(),
            created_at: at.clone(),
        });
        item.updated_at = at;
    }

    pub fn close(&mut self, repo: &str, number: u64, actor: &str) {
        let at = self.now();
        let item = self.item(repo, number);
        item.open = false;
        item.events.push(Event {
            event: "closed".into(),
            actor: actor.into(),
            label: None,
            assignee: None,
            created_at: at.clone(),
        });
        item.updated_at = at;
    }

    fn association(&self, repo: &str, login: &str) -> &'static str {
        let r = &self.repos[repo];
        let is = |list: &[String]| list.iter().any(|m| m.eq_ignore_ascii_case(login));
        if r.owner.eq_ignore_ascii_case(login) {
            "OWNER"
        } else if is(&r.members) {
            "MEMBER"
        } else if is(&r.collaborators) {
            "COLLABORATOR"
        } else {
            "NONE"
        }
    }
}

/// 偽のGitHubに向いた `Probe`。
pub struct FakeGithub {
    pub state: RefCell<State>,
}

impl FakeGithub {
    pub fn new(clock: u64) -> Self {
        FakeGithub {
            state: RefCell::new(State {
                clock,
                ..State::default()
            }),
        }
    }

    fn gh(&self, args: &[&str], env: &[(&str, &str)], input: Option<&str>) -> Option<Output> {
        let token = env
            .iter()
            .find(|(k, _)| *k == "GH_TOKEN")
            .map(|(_, v)| v.to_string());
        let mut state = self.state.borrow_mut();
        // トークンがなければ、ghにログインしているアカウントで動く
        let token = token.unwrap_or_else(|| state.gh_token.clone());
        let out = match state.tokens.get(&token).cloned() {
            None => fail("HTTP 401: Bad credentials"),
            Some(login) => match handle(&mut state, &login, args, input) {
                Some(out) => out,
                None => {
                    state.unknown.push(format!("gh {}", args.join(" ")));
                    fail("偽のGitHubが知らない呼び出しです")
                }
            },
        };
        if !out.success {
            state
                .failed
                .push(format!("gh {}: {}", args.join(" "), out.stderr));
        }
        Some(out)
    }

    fn git(&self, args: &[&str], env: &[(&str, &str)]) -> Option<Output> {
        let state = self.state.borrow();
        let mut remote = false;
        let args: Vec<String> = args
            .iter()
            .map(|a| match local_remote(&state, a) {
                Some(path) => {
                    remote = true;
                    path
                }
                None => a.to_string(),
            })
            .collect();
        drop(state);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        // 手元のbareリポジトリを読むので、その呼び出しだけ file を許す
        let mut env: Vec<(&str, &str)> = env
            .iter()
            .map(|&(k, v)| {
                if remote && k == "GIT_ALLOW_PROTOCOL" {
                    (k, "file")
                } else {
                    (k, v)
                }
            })
            .collect();
        // 使う人のgitの設定（hooks、署名など）を読まない。CLIが渡していなくても、テストを手元の設定から切り離す
        for isolated in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ] {
            if !env.iter().any(|(k, _)| *k == isolated.0) {
                env.push(isolated);
            }
        }
        SystemProbe.run_with_input("git", &args, &env, None)
    }
}

impl Probe for FakeGithub {
    fn run_with_input(
        &self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        input: Option<&str>,
    ) -> Option<Output> {
        match program {
            "gh" => self.gh(args, env, input),
            "git" => self.git(args, env),
            _ => {
                self.state
                    .borrow_mut()
                    .unknown
                    .push(format!("{program} {}", args.join(" ")));
                None
            }
        }
    }
}

/// `https://github.com/<owner>/<repo>.git` なら、手元のbareリポジトリの場所。
fn local_remote(state: &State, arg: &str) -> Option<String> {
    let repo = arg
        .strip_prefix("https://github.com/")?
        .strip_suffix(".git")?;
    let bare = &state.repos.get(repo)?.bare;
    Some(bare.display().to_string())
}

fn ok(stdout: impl Into<String>) -> Output {
    Output {
        success: true,
        code: Some(0),
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

fn fail(stderr: &str) -> Output {
    Output {
        success: false,
        code: Some(1),
        stdout: String::new(),
        stderr: stderr.to_string(),
    }
}

/// ghの引数を解く。解けなければ `None`。
fn handle(state: &mut State, login: &str, args: &[&str], input: Option<&str>) -> Option<Output> {
    match args {
        ["auth", "token", "--hostname", "github.com"] => Some(ok(format!("{}\n", state.gh_token))),
        ["api", "user", "--jq", ".login"] => Some(ok(format!("{login}\n"))),
        ["api", path, "--jq", ".id"] => {
            let user = path.strip_prefix("/users/")?;
            // ログイン名から決まった数を作る（noreplyのアドレスに使うだけ）
            let id: u64 = user.bytes().map(u64::from).sum();
            Some(ok(format!("{id}\n")))
        }
        [
            "label",
            "list",
            "--repo",
            repo,
            "--json",
            "name",
            "--limit",
            _,
        ] => {
            let labels: Vec<Value> = state
                .repos
                .get(*repo)?
                .labels
                .iter()
                .map(|l| json!({ "name": l }))
                .collect();
            Some(ok(Value::Array(labels).to_string()))
        }
        ["api", "graphql", rest @ ..] => graphql(state, rest),
        ["api", path] => rest_one(state, path),
        ["api", "--paginate", path, "--jq", ".[]"] => rest_list(state, path),
        [
            kind @ ("issue" | "pr"),
            "edit",
            number,
            "--repo",
            repo,
            rest @ ..,
        ] => {
            let number: u64 = number.parse().ok()?;
            check_kind(state, repo, number, kind)?;
            let (mut add, mut remove): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
            let mut rest = rest;
            while let [flag, value, tail @ ..] = rest {
                match *flag {
                    "--add-label" => add.extend(value.split(',')),
                    "--remove-label" => remove.extend(value.split(',')),
                    _ => return None,
                }
                rest = tail;
            }
            if !rest.is_empty() {
                return None;
            }
            // 1回の呼び出しの付け外しは、同じ時刻になる（ghは続けて送るので、本物でもふつう同じ秒になる）
            let at = state.now();
            // ghは、変える前にすべてのラベルを探す。どれかがなければ、何も変えない
            if let Some(missing) = remove.iter().find(|r| {
                !state
                    .repo(repo)
                    .labels
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case(r))
            }) {
                return Some(fail(&format!(
                    "could not remove label: '{missing}' not found"
                )));
            }
            if let Err(e) = state.add_labels(repo, number, login, &add, &at) {
                return Some(fail(&e));
            }
            if let Err(e) = state.remove_labels(repo, number, login, &remove, &at) {
                return Some(fail(&e));
            }
            state.writes.push(format!(
                "{login}: {kind} edit {repo}#{number} {}",
                args[5..].join(" ")
            ));
            Some(ok(""))
        }
        [
            kind @ ("issue" | "pr"),
            "comment",
            number,
            "--repo",
            repo,
            "--body-file",
            "-",
        ] => {
            let number: u64 = number.parse().ok()?;
            check_kind(state, repo, number, kind)?;
            state.comment(repo, number, login, input?);
            state
                .writes
                .push(format!("{login}: {kind} comment {repo}#{number}"));
            Some(ok(""))
        }
        ["issue", "close", number, "--repo", repo] => {
            let number: u64 = number.parse().ok()?;
            check_kind(state, repo, number, "issue")?;
            state.close(repo, number, login);
            state
                .writes
                .push(format!("{login}: issue close {repo}#{number}"));
            Some(ok(""))
        }
        [
            kind @ ("issue" | "pr"),
            "view",
            number,
            "--repo",
            repo,
            "--json",
            "labels",
        ] => {
            let number: u64 = number.parse().ok()?;
            check_kind(state, repo, number, kind)?;
            let labels: Vec<Value> = state
                .item(repo, number)
                .labels
                .iter()
                .map(|l| json!({ "name": l }))
                .collect();
            Some(ok(json!({ "labels": labels }).to_string()))
        }
        _ => None,
    }
}

/// `gh issue` をPRに（`gh pr` をIssueに）使っていないか。GitHubは区別しないが、CLIの誤りを見逃さないように。
fn check_kind(state: &mut State, repo: &str, number: u64, kind: &str) -> Option<()> {
    let item = state.repos.get(repo)?.items.get(&number)?;
    (item.pr == (kind == "pr")).then_some(())
}

/// REST APIで1件を読む。
fn rest_one(state: &mut State, path: &str) -> Option<Output> {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match parts.as_slice() {
        ["repos", owner, name, "issues", number] => {
            let repo = format!("{owner}/{name}");
            let item = state
                .repos
                .get(&repo)?
                .items
                .get(&number.parse().ok()?)?
                .clone();
            Some(ok(json!({
                "number": item.number,
                "title": item.title,
                "body": item.body,
                "user": { "login": item.author },
                "author_association": state.association(&repo, &item.author),
                "created_at": item.created_at,
                "state": if item.open { "open" } else { "closed" },
            })
            .to_string()))
        }
        _ => None,
    }
}

/// REST APIで一覧を読む（`--jq .[]` なので、1行に1件）。
fn rest_list(state: &mut State, path: &str) -> Option<Output> {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let (repo, number, what) = match parts.as_slice() {
        ["repos", owner, name, "issues", number, what] => (
            format!("{owner}/{name}"),
            number.parse::<u64>().ok()?,
            *what,
        ),
        _ => return None,
    };
    let item = state.repos.get(&repo)?.items.get(&number)?.clone();
    let lines: Vec<Value> = match what {
        "comments" => item
            .comments
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "user": { "login": c.author },
                    "body": c.body,
                    "created_at": c.created_at,
                    "author_association": state.association(&repo, &c.author),
                })
            })
            .collect(),
        "events" => item
            .events
            .iter()
            .map(|e| {
                let mut v = json!({
                    "event": e.event,
                    "actor": { "login": e.actor },
                    "created_at": e.created_at,
                });
                if let Some(label) = &e.label {
                    v["label"] = json!({ "name": label });
                }
                if let Some(assignee) = &e.assignee {
                    v["assignee"] = json!({ "login": assignee });
                }
                v
            })
            .collect(),
        _ => return None,
    };
    Some(ok(lines
        .iter()
        .map(|l| format!("{l}\n"))
        .collect::<String>()))
}

/// `gh api graphql` の引数（`-f query=...`、`-f name=value`、`-F name=value`、`-f name[]=value`）を解く。
fn variables(args: &[&str]) -> Option<(String, BTreeMap<String, Value>)> {
    let mut query = None;
    let mut vars: BTreeMap<String, Value> = BTreeMap::new();
    let mut rest = args;
    while let [flag, value, tail @ ..] = rest {
        let (name, value) = value.split_once('=')?;
        match (*flag, name) {
            ("-f", "query") => query = Some(value.to_string()),
            ("-f", name) if name.ends_with("[]") => {
                let list = vars
                    .entry(name.trim_end_matches("[]").to_string())
                    .or_insert_with(|| json!([]));
                list.as_array_mut()?.push(json!(value));
            }
            ("-f", name) => {
                vars.insert(name.to_string(), json!(value));
            }
            // -F は、数と真偽値を型にする
            ("-F", name) => {
                let typed = match value {
                    "true" => json!(true),
                    "false" => json!(false),
                    v => v.parse::<u64>().map(|n| json!(n)).unwrap_or(json!(v)),
                };
                vars.insert(name.to_string(), typed);
            }
            _ => return None,
        }
        rest = tail;
    }
    rest.is_empty().then_some(())?;
    Some((query?, vars))
}

fn graphql(state: &mut State, args: &[&str]) -> Option<Output> {
    let (query, vars) = variables(args)?;
    let repo = format!(
        "{}/{}",
        vars.get("owner")?.as_str()?,
        vars.get("name")?.as_str()?
    );
    let r = state.repos.get(&repo)?;
    let user = |login: &str| {
        // 書き込みのアカウントがAppなら Bot。偽物ではghのアカウントだけを扱う
        json!({ "__typename": "User", "login": login })
    };
    let labels = |item: &Item| json!({ "nodes": item.labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>() });
    if query == agentcells::tick::list_query(Target::Issue)
        || query == agentcells::tick::list_query(Target::PullRequest)
    {
        let pr = query == agentcells::tick::list_query(Target::PullRequest);
        let label = vars.get("label")?.as_str()?;
        let nodes: Vec<Value> = r
            .items
            .values()
            .filter(|i| i.pr == pr && i.open)
            .filter(|i| i.labels.iter().any(|l| l.eq_ignore_ascii_case(label)))
            .map(|i| {
                json!({
                    "number": i.number,
                    "title": i.title,
                    "body": i.body,
                    "updatedAt": i.updated_at,
                    "author": user(&i.author),
                    "labels": labels(i),
                    "assignees": { "nodes": i.assignees.iter().map(|a| json!({ "login": a })).collect::<Vec<_>>() },
                })
            })
            .collect();
        let connection = if pr { "pullRequests" } else { "issues" };
        return Some(ok(
            json!({ "data": { "repository": { connection: { "nodes": nodes } } } }).to_string(),
        ));
    }
    if query == agentcells::tidy::QUERY {
        let wanted: Vec<String> = vars
            .get("labels")?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_lowercase))
            .collect();
        let closed = |pr: bool| -> Vec<Value> {
            r.items
                .values()
                .filter(|i| i.pr == pr && !i.open)
                .filter(|i| i.labels.iter().any(|l| wanted.contains(&l.to_lowercase())))
                .map(|i| json!({ "number": i.number, "labels": labels(i) }))
                .collect()
        };
        let mut repository = json!({});
        if vars.get("issues")?.as_bool()? {
            repository["issues"] = json!({ "nodes": closed(false) });
        }
        if vars.get("prs")?.as_bool()? {
            repository["pullRequests"] = json!({ "nodes": closed(true) });
        }
        return Some(ok(
            json!({ "data": { "repository": repository } }).to_string()
        ));
    }
    if query == agentcells::guard::EDITS_QUERY {
        let number = vars.get("number")?.as_u64()?;
        let i = r.items.get(&number)?;
        let none = json!({ "pageInfo": { "hasNextPage": false }, "nodes": [] });
        let comments: Vec<Value> = i
            .comments
            .iter()
            .map(|c| json!({ "author": user(&c.author), "userContentEdits": none }))
            .collect();
        let mut content = json!({
            "state": if i.open { "OPEN" } else { "CLOSED" },
            "assignees": { "nodes": i.assignees.iter().map(|a| json!({ "login": a })).collect::<Vec<_>>() },
            "userContentEdits": none,
            "comments": { "pageInfo": { "hasPreviousPage": false }, "nodes": comments },
        });
        if i.pr {
            content["reviews"] = json!({ "pageInfo": { "hasPreviousPage": false }, "nodes": [] });
        }
        return Some(ok(
            json!({ "data": { "repository": { "issueOrPullRequest": content } } }).to_string(),
        ));
    }
    None
}

/// UNIXの秒を、GitHubの形の時刻（`2026-11-09T12:34:56Z`）にする。
pub fn iso(secs: u64) -> String {
    let days = secs / 86400;
    let rem = secs % 86400;
    // Howard Hinnantのcivil_from_days
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// 状態をJSONで書き出す（場面が失敗したときに読む）。
pub fn dump(state: &State, to: &Path) {
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(to, text);
    }
}
