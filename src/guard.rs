//! Issue/PRを動かしてよいかを確かめる。cellを使う人（operator）が自分で決めたものだけを動かす。
//!
//! 公開リポジトリでも、ラベルやAssigneesを変えられるのは書き込みの権限を持つ人だけだが、
//! その中のほかの人が「早くやってほしい」とラベルとAssigneesを変えて、勝手に始めさせないようにする。
//! Issue/PRの履歴（events API）と編集の履歴（GraphQL）は、書いた人をあとから変えられないので、それで確かめる。
//!
//! 確かめる起点は、operatorが「承認」した時刻（operator自身が、productのどれかのcellのラベルを最後に付けた時刻）。
//! agentcellsが次のcellのラベルを付けた時刻を起点にすると、cellが動いている間のほかの人の書き換えを見逃すため。
//!
//! Issueの仕事:
//! - Assigneesにoperatorがいて、operatorを最後に割り当てたのがoperator自身
//! - 今のcellのラベルを最後に付けたのが、operatorか、agentcells自身（書き込みのアカウント）
//! - operatorが承認していて、そのあとにほかの人がタイトル・本文・operatorのコメントを書き換えていない
//!
//! PRの仕事:
//! - agentcellsが作ったPR（作者が書き込みのアカウント）は、agentcellsがoperatorを割り当てる。
//!   元のIssue（本文の `Closes`）が、Issueの仕事と同じ確かめを通ること。PRそのものは、operatorがPRで承認して
//!   いればその時刻から、していなければ作られてからずっと、ほかの人が書き換えていないこと
//! - 人が作ったPRは、Issueの仕事と同じ確かめをPRにする。元のIssueがあれば、PRの承認のあとに
//!   ほかの人が書き換えていないことも確かめる（元のIssueの中身もプロンプトに入るため）
//!
//! 確かめきれないとき（履歴が多すぎるなど）は、動かさない。

use serde::Deserialize;

use crate::config::Target;
use crate::discussion;
use crate::error::{Error, Result};
use crate::home::RepoName;
use crate::probe::Probe;

/// 動かしてよい人。
#[derive(Debug, Clone)]
pub struct Actors {
    /// cellを使う人
    pub operator: String,
    /// 書き込みのアカウント（Appなら `<slug>[bot]`）
    pub writer: String,
}

impl Actors {
    fn is_operator(&self, login: &str) -> bool {
        login.eq_ignore_ascii_case(&self.operator)
    }

    fn is_writer(&self, login: &str) -> bool {
        login.eq_ignore_ascii_case(&self.writer)
    }

    fn is_trusted(&self, login: &str) -> bool {
        self.is_operator(login) || self.is_writer(login)
    }
}

/// 確かめるIssue/PR。
pub struct Subject<'a> {
    pub repo: &'a RepoName,
    pub number: u64,
    /// ghに渡す、読み取り用のトークンなど
    pub env: &'a [(&'a str, &'a str)],
}

/// 確かめること。
pub struct Request<'a> {
    pub kind: Target,
    pub subject: Subject<'a>,
    /// 作った人
    pub author: &'a str,
    /// 今のcellのラベル
    pub label: &'a str,
    /// productのcellのラベルすべて。operatorがどれかを付けたことを、承認とみなす
    pub cell_labels: &'a [String],
    /// PRの本文の `Closes` で示された元のIssue
    pub original: Option<Subject<'a>>,
}

#[derive(Deserialize)]
struct User {
    login: String,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
struct Event {
    event: String,
    actor: Option<User>,
    #[serde(default)]
    label: Option<Label>,
    #[serde(default)]
    assignee: Option<User>,
    #[serde(default)]
    created_at: String,
}

fn login(user: &Option<User>) -> &str {
    user.as_ref().map(|u| u.login.as_str()).unwrap_or("ghost")
}

/// 編集の履歴を読むGraphQL。
/// - `userContentEdits` は新しい順に返るので（cli/cli#1 で確かめた）、`first` で新しいものから読む
/// - コメントとレビューは古い順に返るので、`last` で新しいものから読む
/// - どれも、読みきれなければ（`hasNextPage` / `hasPreviousPage`）動かさない
pub const EDITS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issueOrPullRequest(number: $number) { ... on Issue { userContentEdits(first: 100) { ...edits } comments(last: 100) { pageInfo { hasPreviousPage } nodes { author { login } userContentEdits(first: 20) { ...edits } } } } ... on PullRequest { userContentEdits(first: 100) { ...edits } comments(last: 100) { pageInfo { hasPreviousPage } nodes { author { login } userContentEdits(first: 20) { ...edits } } } reviews(last: 100) { pageInfo { hasPreviousPage } nodes { author { login } userContentEdits(first: 20) { ...edits } comments(first: 100) { pageInfo { hasNextPage } nodes { author { login } userContentEdits(first: 20) { ...edits } } } } } } } } } fragment edits on UserContentEditConnection { pageInfo { hasNextPage } nodes { editedAt editor { login } } }";

#[derive(Deserialize)]
struct EditsResponse {
    data: EditsData,
}

#[derive(Deserialize)]
struct EditsData {
    repository: Option<EditsRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditsRepository {
    issue_or_pull_request: Option<Content>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    #[serde(default)]
    has_next_page: bool,
    #[serde(default)]
    has_previous_page: bool,
}

impl PageInfo {
    fn more(&self) -> bool {
        self.has_next_page || self.has_previous_page
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    #[serde(default)]
    page_info: PageInfo,
    nodes: Vec<Option<T>>,
}

impl<T> Connection<T> {
    fn items(&self) -> impl Iterator<Item = &T> {
        self.nodes.iter().flatten()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Edit {
    edited_at: String,
    editor: Option<User>,
}

/// 本文、コメント、レビュー、レビューのコメント。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Content {
    #[serde(default)]
    author: Option<User>,
    #[serde(default)]
    user_content_edits: Option<Connection<Edit>>,
    #[serde(default)]
    comments: Option<Connection<Content>>,
    #[serde(default)]
    reviews: Option<Connection<Content>>,
}

/// 1つのIssue/PRの履歴。
struct History {
    events: Vec<Event>,
    content: Content,
}

impl History {
    fn fetch(probe: &dyn Probe, subject: &Subject) -> Result<Self> {
        let Subject { repo, number, env } = subject;
        let events =
            discussion::api_list(probe, env, &format!("/repos/{repo}/issues/{number}/events"))?;
        Ok(History {
            events,
            content: fetch_edits(probe, env, repo, *number)?,
        })
    }

    fn last_assigned_operator(&self, actors: &Actors) -> Option<&Event> {
        self.events.iter().rev().find(|e| {
            e.event == "assigned"
                && e.assignee
                    .as_ref()
                    .is_some_and(|a| actors.is_operator(&a.login))
        })
    }

    fn last_labeled(&self, label: &str) -> Option<&Event> {
        // GitHubのラベル名は大文字・小文字を区別しない
        self.events.iter().rev().find(|e| {
            e.event == "labeled"
                && e.label
                    .as_ref()
                    .is_some_and(|l| l.name.eq_ignore_ascii_case(label))
        })
    }

    /// operatorが、cellのラベルのどれかを最後に付けた時刻。
    fn approved_at(&self, cell_labels: &[String], actors: &Actors) -> Option<&str> {
        self.events
            .iter()
            .rev()
            .find(|e| {
                e.event == "labeled"
                    && actors.is_operator(login(&e.actor))
                    && e.label.as_ref().is_some_and(|l| {
                        cell_labels.iter().any(|c| c.eq_ignore_ascii_case(&l.name))
                    })
            })
            .map(|e| e.created_at.as_str())
    }

    /// `since` 以後（同じ秒も含む）に、ほかの人がタイトル、本文、operatorのコメントやレビューを書き換えたか。
    /// `what` はIssue/PRの呼び方（理由に使う）。
    fn changed_by_others(&self, since: &str, actors: &Actors, what: &str) -> Option<String> {
        if let Some(e) = self.events.iter().find(|e| {
            e.event == "renamed"
                && e.created_at.as_str() >= since
                && !actors.is_trusted(login(&e.actor))
        }) {
            return Some(format!(
                "{what}のタイトルを、{} が変えました",
                login(&e.actor)
            ));
        }
        if let Some(reason) = edited_by_others(&self.content.user_content_edits, since, actors) {
            return Some(format!("{what}の本文を、{reason}"));
        }
        let mut stack: Vec<(&Option<Connection<Content>>, &str)> = vec![
            (&self.content.comments, "コメント"),
            (&self.content.reviews, "レビュー"),
        ];
        while let Some((connection, kind)) = stack.pop() {
            let Some(connection) = connection else {
                continue;
            };
            if connection.page_info.more() {
                return Some(format!(
                    "{what}の{kind}が多すぎて、書き換えを確かめきれません"
                ));
            }
            for item in connection.items() {
                if actors.is_operator(login(&item.author))
                    && let Some(reason) = edited_by_others(&item.user_content_edits, since, actors)
                {
                    return Some(format!("{what}の {} の{kind}を、{reason}", actors.operator));
                }
                stack.push((&item.comments, "レビューのコメント"));
            }
        }
        None
    }
}

/// ほかの人が `since` 以後に書き換えていれば、「〇〇 が書き換えました」。読みきれなければ、そう言う。
fn edited_by_others(
    edits: &Option<Connection<Edit>>,
    since: &str,
    actors: &Actors,
) -> Option<String> {
    let edits = edits.as_ref()?;
    if let Some(edit) = edits
        .items()
        .find(|e| e.edited_at.as_str() >= since && !actors.is_trusted(login(&e.editor)))
    {
        return Some(format!("{} が書き換えました", login(&edit.editor)));
    }
    edits
        .page_info
        .more()
        .then(|| "書き換えが多すぎて確かめきれません".to_string())
}

/// 見送る理由に、「見直して、ラベルを付け直すと動きます」を足す。
fn retry(reason: String) -> Option<String> {
    Some(format!("{reason}（見直して、ラベルを付け直すと動きます）"))
}

/// 動かしてよければ `None`、だめなら見送る理由を返す。理由には `@` を付けない（そのまま表示しても通知が飛ばない）。
pub fn check(probe: &dyn Probe, request: &Request, actors: &Actors) -> Result<Option<String>> {
    let history = History::fetch(probe, &request.subject)?;
    let made_by_agentcells =
        request.kind == Target::PullRequest && actors.is_writer(request.author);

    // operatorを最後に割り当てたのが、operator自身か（agentcellsが作ったPRなら、agentcells）
    match history.last_assigned_operator(actors) {
        None => {
            return Ok(Some(format!(
                "{} を割り当てた記録がありません",
                actors.operator
            )));
        }
        Some(e)
            if !(actors.is_operator(login(&e.actor))
                || made_by_agentcells && actors.is_writer(login(&e.actor))) =>
        {
            return Ok(Some(format!(
                "{} を割り当てたのが {} です（自分で割り当てたものだけ動かします）",
                actors.operator,
                login(&e.actor)
            )));
        }
        Some(_) => {}
    }

    // 今のcellのラベルを最後に付けたのが、operatorかagentcellsか
    let label = request.label;
    let Some(labeled) = history.last_labeled(label) else {
        return Ok(Some(format!("ラベル「{label}」を付けた記録がありません")));
    };
    if !actors.is_trusted(login(&labeled.actor)) {
        return Ok(Some(format!(
            "ラベル「{label}」を最後に付けたのが {} です（{} か agentcells が付けたものだけ動かします）",
            login(&labeled.actor),
            actors.operator
        )));
    }

    // operatorの承認から、ほかの人が書き換えていないか
    let what = match request.kind {
        Target::Issue => "Issue",
        Target::PullRequest => "PR",
    };
    let since = match history.approved_at(request.cell_labels, actors) {
        Some(at) => at.to_string(),
        // agentcellsが作ったPRは、元のIssueの承認を引き継ぐ。PRそのものは、作られてからずっと見る
        None if made_by_agentcells => String::new(),
        None => {
            return Ok(Some(format!(
                "{} がcellのラベルを付けた記録がありません（{} が付けたものから始めます）",
                actors.operator, actors.operator
            )));
        }
    };
    if let Some(reason) = history.changed_by_others(&since, actors, what) {
        return Ok(retry(reason));
    }

    if request.kind == Target::PullRequest {
        let Some(original) = &request.original else {
            if made_by_agentcells {
                return Ok(Some(
                    "PRの本文に、元のIssue（Closes）が見つかりません".to_string(),
                ));
            }
            return Ok(None);
        };
        let original_history = History::fetch(probe, original)?;
        let issue_since = if made_by_agentcells {
            // 元のIssueが、Issueの仕事と同じ確かめを通ること
            match original_history.last_assigned_operator(actors) {
                Some(e) if actors.is_operator(login(&e.actor)) => {}
                _ => {
                    return Ok(Some(format!(
                        "元のIssueに、{} が自分を割り当てた記録がありません",
                        actors.operator
                    )));
                }
            }
            match original_history.approved_at(request.cell_labels, actors) {
                Some(at) => at.to_string(),
                None => {
                    return Ok(Some(format!(
                        "元のIssueに、{} がcellのラベルを付けた記録がありません",
                        actors.operator
                    )));
                }
            }
        } else {
            since
        };
        if let Some(reason) = original_history.changed_by_others(&issue_since, actors, "元のIssue")
        {
            return Ok(retry(reason));
        }
    }
    Ok(None)
}

fn fetch_edits(
    probe: &dyn Probe,
    env: &[(&str, &str)],
    repo: &RepoName,
    number: u64,
) -> Result<Content> {
    let query = format!("query={EDITS_QUERY}");
    let owner = format!("owner={}", repo.owner);
    let name = format!("name={}", repo.name);
    let number_arg = format!("number={number}");
    let args = [
        "api",
        "graphql",
        "-f",
        query.as_str(),
        "-F",
        owner.as_str(),
        "-F",
        name.as_str(),
        "-F",
        number_arg.as_str(),
    ];
    let out = probe.run_with_env("gh", &args, env).ok_or_else(|| {
        Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if !out.success {
        return Err(Error::new(format!(
            "{repo}#{number} の編集の履歴を読めません: {}",
            out.stderr.trim()
        )));
    }
    let response: EditsResponse = serde_json::from_str(&out.stdout)
        .map_err(|e| Error::new(format!("{repo}#{number} の編集の履歴を読めません: {e}")))?;
    response
        .data
        .repository
        .and_then(|r| r.issue_or_pull_request)
        .ok_or_else(|| Error::new(format!("{repo}#{number} が見つかりません")))
}

/// テストで、Issue/PRの履歴に決まった応答を返す。
#[cfg(test)]
pub(crate) mod fake {
    use super::EDITS_QUERY;
    use crate::discussion;
    use crate::probe::fake::FakeProbe;

    /// `events` は1行に1件のJSON。`edits` はGraphQLの `issueOrPullRequest` の中身。
    pub fn with_history(
        probe: FakeProbe,
        repo: &str,
        number: u64,
        events: &[String],
        edits: &str,
    ) -> FakeProbe {
        let (owner, name) = repo.split_once('/').unwrap();
        let events: Vec<&str> = events.iter().map(String::as_str).collect();
        let probe = discussion::fake::with_list(
            probe,
            &format!("/repos/{repo}/issues/{number}/events"),
            &events,
        );
        probe.with(
            &format!(
                "gh api graphql -f query={EDITS_QUERY} -F owner={owner} -F name={name} -F number={number}"
            ),
            true,
            &format!(r#"{{"data":{{"repository":{{"issueOrPullRequest":{edits}}}}}}}"#),
        )
    }

    pub fn assigned(actor: &str, assignee: &str, at: &str) -> String {
        format!(
            r#"{{"event":"assigned","actor":{{"login":"{actor}"}},"assignee":{{"login":"{assignee}"}},"created_at":"{at}"}}"#
        )
    }

    pub fn labeled(actor: &str, label: &str, at: &str) -> String {
        format!(
            r#"{{"event":"labeled","actor":{{"login":"{actor}"}},"label":{{"name":"{label}"}},"created_at":"{at}"}}"#
        )
    }

    pub fn body_edit(editor: &str, at: &str) -> String {
        format!(
            r#"{{"userContentEdits":{{"nodes":[{{"editedAt":"{at}","editor":{{"login":"{editor}"}}}}]}},"comments":{{"nodes":[]}}}}"#
        )
    }

    pub const NO_EDITS: &str =
        r#"{"userContentEdits":{"nodes":[]},"comments":{"nodes":[]},"reviews":{"nodes":[]}}"#;
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use crate::probe::fake::FakeProbe;

    fn actors() -> Actors {
        Actors {
            operator: "y-hirakaw".to_string(),
            writer: "agentcells-yh[bot]".to_string(),
        }
    }

    const T1: &str = "2026-10-25T01:00:00Z";
    const T2: &str = "2026-10-25T02:00:00Z";
    const T3: &str = "2026-10-25T03:00:00Z";
    const T4: &str = "2026-10-25T04:00:00Z";

    fn labels() -> Vec<String> {
        ["ac:triage", "ac:implement", "ac:review"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn issue(events: &[String], edits: &str, label: &str) -> Option<String> {
        let probe = with_history(FakeProbe::default(), "octo/app", 3, events, edits);
        let repo = RepoName::parse("octo/app").unwrap();
        let labels = labels();
        let request = Request {
            kind: Target::Issue,
            subject: Subject {
                repo: &repo,
                number: 3,
                env: &[],
            },
            author: "stranger",
            label,
            cell_labels: &labels,
            original: None,
        };
        check(&probe, &request, &actors()).unwrap()
    }

    /// agentcellsが作ったPR（octo/app#7、元のIssueは octo/app#3）。
    fn pr(
        pr_events: &[String],
        pr_edits: &str,
        issue_events: &[String],
        issue_edits: &str,
    ) -> Option<String> {
        let probe = with_history(FakeProbe::default(), "octo/app", 7, pr_events, pr_edits);
        let probe = with_history(probe, "octo/app", 3, issue_events, issue_edits);
        let repo = RepoName::parse("octo/app").unwrap();
        let labels = labels();
        let request = Request {
            kind: Target::PullRequest,
            subject: Subject {
                repo: &repo,
                number: 7,
                env: &[],
            },
            author: "agentcells-yh[bot]",
            label: "ac:review",
            cell_labels: &labels,
            original: Some(Subject {
                repo: &repo,
                number: 3,
                env: &[],
            }),
        };
        check(&probe, &request, &actors()).unwrap()
    }

    fn approved_issue() -> Vec<String> {
        vec![
            assigned("y-hirakaw", "y-hirakaw", T1),
            labeled("y-hirakaw", "ac:triage", T2),
        ]
    }

    #[test]
    fn runs_when_the_operator_assigned_and_labeled() {
        assert_eq!(issue(&approved_issue(), NO_EDITS, "AC:Triage"), None);
        // 次のcellへ進めるラベルは、agentcellsが付ける
        let mut events = approved_issue();
        events.push(labeled("agentcells-yh[bot]", "ac:implement", T3));
        assert_eq!(issue(&events, NO_EDITS, "ac:implement"), None);
    }

    #[test]
    fn skips_when_someone_else_assigned_or_labeled() {
        let events = [
            assigned("eager", "y-hirakaw", T1),
            labeled("y-hirakaw", "ac:triage", T2),
        ];
        let reason = issue(&events, NO_EDITS, "ac:triage").unwrap();
        assert!(reason.contains("割り当てたのが eager"), "{reason}");

        let events = [
            assigned("y-hirakaw", "y-hirakaw", T1),
            labeled("eager", "ac:triage", T2),
        ];
        let reason = issue(&events, NO_EDITS, "ac:triage").unwrap();
        assert!(reason.contains("最後に付けたのが eager"), "{reason}");
        assert!(!reason.contains('@'));

        // 一度operatorが付けても、そのあとほかの人が付け直したら動かさない
        let mut events = approved_issue();
        events.push(labeled("eager", "ac:triage", T3));
        assert!(
            issue(&events, NO_EDITS, "ac:triage")
                .unwrap()
                .contains("eager")
        );

        // agentcellsが付けたラベルだけで、operatorの承認がない
        let events = [
            assigned("y-hirakaw", "y-hirakaw", T1),
            labeled("agentcells-yh[bot]", "ac:implement", T2),
        ];
        assert!(
            issue(&events, NO_EDITS, "ac:implement")
                .unwrap()
                .contains("cellのラベルを付けた記録がありません")
        );
    }

    #[test]
    fn edits_while_a_cell_ran_are_caught_by_the_next_cell() {
        // T2 で承認、T3 にtriageが動いている間に書き換えられ、T4 にagentcellsが次のcellへ進めた
        let mut events = approved_issue();
        events.push(labeled("agentcells-yh[bot]", "ac:implement", T4));
        let reason = issue(&events, &body_edit("eager", T3), "ac:implement").unwrap();
        assert!(
            reason.contains("Issueの本文を、eager が書き換えました"),
            "{reason}"
        );
        // 同じ秒の書き換えも見る
        assert!(issue(&approved_issue(), &body_edit("eager", T2), "ac:triage").is_some());
        // 承認より前の書き換えや、operator自身の書き換えは問わない
        assert_eq!(
            issue(&approved_issue(), &body_edit("eager", T1), "ac:triage"),
            None
        );
        assert_eq!(
            issue(&approved_issue(), &body_edit("y-hirakaw", T3), "ac:triage"),
            None
        );
    }

    #[test]
    fn skips_renames_and_edits_of_the_operators_comments_and_reviews() {
        let mut renamed = approved_issue();
        renamed.push(format!(
            r#"{{"event":"renamed","actor":{{"login":"eager"}},"created_at":"{T3}"}}"#
        ));
        assert!(
            issue(&renamed, NO_EDITS, "ac:triage")
                .unwrap()
                .contains("タイトル")
        );

        let comment_edit = format!(
            r#"{{"userContentEdits":{{"nodes":[]}},"comments":{{"nodes":[{{"author":{{"login":"y-hirakaw"}},"userContentEdits":{{"nodes":[{{"editedAt":"{T3}","editor":{{"login":"eager"}}}}]}}}}]}}}}"#
        );
        assert!(
            issue(&approved_issue(), &comment_edit, "ac:triage")
                .unwrap()
                .contains("y-hirakaw のコメントを、eager が書き換えました")
        );

        // PRのレビューの行コメント
        let review_comment_edit = format!(
            r#"{{"userContentEdits":{{"nodes":[]}},"comments":{{"nodes":[]}},"reviews":{{"nodes":[{{"author":{{"login":"y-hirakaw"}},"userContentEdits":{{"nodes":[]}},"comments":{{"nodes":[{{"author":{{"login":"y-hirakaw"}},"userContentEdits":{{"nodes":[{{"editedAt":"{T3}","editor":{{"login":"eager"}}}}]}}}}]}}}}]}}}}"#
        );
        let pr_events = [
            assigned("agentcells-yh[bot]", "y-hirakaw", T2),
            labeled("agentcells-yh[bot]", "ac:review", T2),
        ];
        let reason = pr(
            &pr_events,
            &review_comment_edit,
            &approved_issue(),
            NO_EDITS,
        )
        .unwrap();
        assert!(reason.contains("レビューのコメントを、eager"), "{reason}");
    }

    #[test]
    fn skips_when_history_is_too_long_to_check() {
        let many = r#"{"userContentEdits":{"nodes":[]},"comments":{"pageInfo":{"hasPreviousPage":true},"nodes":[]}}"#;
        assert!(
            issue(&approved_issue(), many, "ac:triage")
                .unwrap()
                .contains("多すぎて")
        );
        let many_edits = r#"{"userContentEdits":{"pageInfo":{"hasNextPage":true},"nodes":[]},"comments":{"nodes":[]}}"#;
        assert!(
            issue(&approved_issue(), many_edits, "ac:triage")
                .unwrap()
                .contains("確かめきれません")
        );
    }

    #[test]
    fn prs_made_by_agentcells_inherit_the_issue_approval() {
        // implementが作ったPRに、agentcellsがoperatorを割り当て、reviewのラベルを付けた
        let pr_events = [
            assigned("agentcells-yh[bot]", "y-hirakaw", T3),
            labeled("agentcells-yh[bot]", "ac:review", T3),
        ];
        assert_eq!(pr(&pr_events, NO_EDITS, &approved_issue(), NO_EDITS), None);

        // 元のIssueが、承認のあとに書き換えられた
        let reason = pr(
            &pr_events,
            NO_EDITS,
            &approved_issue(),
            &body_edit("eager", T3),
        )
        .unwrap();
        assert!(reason.contains("元のIssueの本文を、eager"), "{reason}");

        // 元のIssueを、operatorが承認していない
        let unapproved = [assigned("y-hirakaw", "y-hirakaw", T1)];
        assert!(pr(&pr_events, NO_EDITS, &unapproved, NO_EDITS).is_some());

        // agentcellsが作ったPRは、作られてからずっと、ほかの人の書き換えを見る
        assert!(
            pr(
                &pr_events,
                &body_edit("eager", T1),
                &approved_issue(),
                NO_EDITS
            )
            .is_some()
        );
    }

    #[test]
    fn agentcells_may_assign_only_on_its_own_prs() {
        // 人が作ったIssueに、agentcellsが割り当てても認めない
        let events = [
            assigned("agentcells-yh[bot]", "y-hirakaw", T1),
            labeled("y-hirakaw", "ac:triage", T2),
        ];
        assert!(
            issue(&events, NO_EDITS, "ac:triage")
                .unwrap()
                .contains("割り当てたのが agentcells-yh[bot]")
        );
    }
}
