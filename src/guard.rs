//! Issue/PRを動かしてよいかを確かめる。cellを使う人（operator）が自分で決めたものだけを動かす。
//!
//! 公開リポジトリでも、ラベルやAssigneesを変えられるのは書き込みの権限を持つ人だけだが、
//! その中のほかの人が「早くやってほしい」とラベルとAssigneesを変えて、勝手に始めさせないようにする。
//! Issue/PRの履歴（events API）と編集の履歴（GraphQL）は、書いた人をあとから変えられないので、それで確かめる:
//! - Assigneesにoperatorがいて、operatorを割り当てたのがoperator自身
//! - cellのラベルを最後に付けたのが、operatorか、agentcells自身（書き込みのアカウント）。
//!   agentcellsが付けるのは、正しく始まった仕事を次のcellに進めるときだけ
//! - ラベルを付けたあとに、ほかの人がタイトルや本文、operatorのコメントを書き換えていない
//!   （書き換えられたら、operatorが見直してラベルを付け直すと、また動く）

use serde::Deserialize;

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

    fn is_trusted(&self, login: &str) -> bool {
        self.is_operator(login) || login.eq_ignore_ascii_case(&self.writer)
    }
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

/// 本文とコメントの編集の履歴を読むGraphQL。コメントは新しいものから100件まで見る。
pub const EDITS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issueOrPullRequest(number: $number) { ... on Issue { userContentEdits(last: 100) { nodes { editedAt editor { login } } } comments(last: 100) { nodes { author { login } userContentEdits(last: 20) { nodes { editedAt editor { login } } } } } } ... on PullRequest { userContentEdits(last: 100) { nodes { editedAt editor { login } } } comments(last: 100) { nodes { author { login } userContentEdits(last: 20) { nodes { editedAt editor { login } } } } } } } } }";

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
    issue_or_pull_request: Option<Edited>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Edited {
    #[serde(default)]
    user_content_edits: Option<Nodes<Edit>>,
    #[serde(default)]
    comments: Option<Nodes<Comment>>,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<Option<T>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Edit {
    edited_at: String,
    editor: Option<User>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    author: Option<User>,
    #[serde(default)]
    user_content_edits: Option<Nodes<Edit>>,
}

fn edits(nodes: Option<Nodes<Edit>>) -> Vec<Edit> {
    nodes
        .map(|n| n.nodes.into_iter().flatten().collect())
        .unwrap_or_default()
}

/// 動かしてよければ `None`、だめなら見送る理由を返す。理由には `@` を付けない（そのまま表示しても通知が飛ばない）。
/// `env` は、ghに渡す読み取り用のトークンなど。
pub fn check(
    probe: &dyn Probe,
    env: &[(&str, &str)],
    repo: &RepoName,
    number: u64,
    label: &str,
    actors: &Actors,
) -> Result<Option<String>> {
    let events: Vec<Event> =
        discussion::api_list(probe, env, &format!("/repos/{repo}/issues/{number}/events"))?;

    // operatorを最後に割り当てたのが、operator自身か
    match events.iter().rev().find(|e| {
        e.event == "assigned"
            && e.assignee
                .as_ref()
                .is_some_and(|a| actors.is_operator(&a.login))
    }) {
        None => {
            return Ok(Some(format!(
                "{} を割り当てた記録がありません",
                actors.operator
            )));
        }
        Some(e) if !actors.is_operator(login(&e.actor)) => {
            return Ok(Some(format!(
                "{} を割り当てたのが {} です（自分で割り当てたものだけ動かします）",
                actors.operator,
                login(&e.actor)
            )));
        }
        Some(_) => {}
    }

    // cellのラベルを最後に付けたのが、operatorかagentcellsか（GitHubのラベル名は大文字・小文字を区別しない）
    let Some(labeled) = events.iter().rev().find(|e| {
        e.event == "labeled"
            && e.label
                .as_ref()
                .is_some_and(|l| l.name.eq_ignore_ascii_case(label))
    }) else {
        return Ok(Some(format!("ラベル「{label}」を付けた記録がありません")));
    };
    if !actors.is_trusted(login(&labeled.actor)) {
        return Ok(Some(format!(
            "ラベル「{label}」を最後に付けたのが {} です（{} か agentcells が付けたものだけ動かします）",
            login(&labeled.actor),
            actors.operator
        )));
    }
    let since = labeled.created_at.as_str();

    // ラベルを付けたあとに、ほかの人がタイトルを変えていないか
    if let Some(e) = events.iter().find(|e| {
        e.event == "renamed" && e.created_at.as_str() > since && !actors.is_trusted(login(&e.actor))
    }) {
        return Ok(Some(format!(
            "ラベルを付けたあとに、{} がタイトルを変えました（見直して、ラベルを付け直すと動きます）",
            login(&e.actor)
        )));
    }

    // ラベルを付けたあとに、ほかの人が本文やoperatorのコメントを書き換えていないか
    let edited = fetch_edits(probe, env, repo, number)?;
    let changed_by_other =
        |edit: &Edit| edit.edited_at.as_str() > since && !actors.is_trusted(login(&edit.editor));
    if let Some(edit) = edits(edited.user_content_edits)
        .iter()
        .find(|e| changed_by_other(e))
    {
        return Ok(Some(format!(
            "ラベルを付けたあとに、{} が本文を書き換えました（見直して、ラベルを付け直すと動きます）",
            login(&edit.editor)
        )));
    }
    for comment in edited
        .comments
        .map(|c| c.nodes.into_iter().flatten().collect::<Vec<_>>())
        .unwrap_or_default()
    {
        if !actors.is_operator(login(&comment.author)) {
            continue;
        }
        if let Some(edit) = edits(comment.user_content_edits)
            .iter()
            .find(|e| changed_by_other(e))
        {
            return Ok(Some(format!(
                "ラベルを付けたあとに、{} が {} のコメントを書き換えました（見直して、ラベルを付け直すと動きます）",
                login(&edit.editor),
                actors.operator
            )));
        }
    }
    Ok(None)
}

fn fetch_edits(
    probe: &dyn Probe,
    env: &[(&str, &str)],
    repo: &RepoName,
    number: u64,
) -> Result<Edited> {
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
        ))
        .with_hint("`agentcells doctor` で、リポジトリと書き込みのアカウントを確かめてください"));
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
        events: &[&str],
        edits: &str,
    ) -> FakeProbe {
        let (owner, name) = repo.split_once('/').unwrap();
        let probe = discussion::fake::with_list(
            probe,
            &format!("/repos/{repo}/issues/{number}/events"),
            events,
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

    pub const NO_EDITS: &str = r#"{"userContentEdits":{"nodes":[]},"comments":{"nodes":[]}}"#;
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

    fn verdict(events: &[String], edits: &str) -> Option<String> {
        let events: Vec<&str> = events.iter().map(String::as_str).collect();
        let probe = with_history(FakeProbe::default(), "octo/app", 3, &events, edits);
        let repo = RepoName::parse("octo/app").unwrap();
        check(&probe, &[], &repo, 3, "ac:triage", &actors()).unwrap()
    }

    const T1: &str = "2026-10-25T01:00:00Z";
    const T2: &str = "2026-10-25T02:00:00Z";
    const T3: &str = "2026-10-25T03:00:00Z";

    #[test]
    fn runs_when_the_operator_assigned_and_labeled() {
        let events = [
            assigned("y-hirakaw", "y-hirakaw", T1),
            labeled("Y-Hirakaw", "AC:Triage", T2),
        ];
        assert_eq!(verdict(&events, NO_EDITS), None);
        // 次のcellへ進めるラベルは、agentcellsが付ける
        let events = [
            assigned("y-hirakaw", "y-hirakaw", T1),
            labeled("agentcells-yh[bot]", "ac:triage", T2),
        ];
        assert_eq!(verdict(&events, NO_EDITS), None);
    }

    #[test]
    fn skips_when_someone_else_assigned_or_labeled() {
        let reason = verdict(
            &[
                assigned("eager", "y-hirakaw", T1),
                labeled("y-hirakaw", "ac:triage", T2),
            ],
            NO_EDITS,
        )
        .unwrap();
        assert!(reason.contains("割り当てたのが eager"), "{reason}");

        let reason = verdict(
            &[
                assigned("y-hirakaw", "y-hirakaw", T1),
                labeled("eager", "ac:triage", T2),
            ],
            NO_EDITS,
        )
        .unwrap();
        assert!(reason.contains("最後に付けたのが eager"), "{reason}");
        assert!(!reason.contains('@'));

        // 一度operatorが付けても、そのあとほかの人が付け直したら動かさない
        let reason = verdict(
            &[
                assigned("y-hirakaw", "y-hirakaw", T1),
                labeled("y-hirakaw", "ac:triage", T2),
                labeled("eager", "ac:triage", T3),
            ],
            NO_EDITS,
        )
        .unwrap();
        assert!(reason.contains("eager"), "{reason}");

        assert!(
            verdict(&[labeled("y-hirakaw", "ac:triage", T2)], NO_EDITS)
                .unwrap()
                .contains("割り当てた記録がありません")
        );
    }

    #[test]
    fn skips_when_someone_else_changed_it_after_labeling() {
        let base = vec![
            assigned("y-hirakaw", "y-hirakaw", T1),
            labeled("y-hirakaw", "ac:triage", T2),
        ];
        let mut renamed = base.clone();
        renamed.push(format!(
            r#"{{"event":"renamed","actor":{{"login":"eager"}},"created_at":"{T3}"}}"#
        ));
        assert!(verdict(&renamed, NO_EDITS).unwrap().contains("タイトル"));

        let body_edit = format!(
            r#"{{"userContentEdits":{{"nodes":[{{"editedAt":"{T3}","editor":{{"login":"eager"}}}}]}},"comments":{{"nodes":[]}}}}"#
        );
        assert!(
            verdict(&base, &body_edit)
                .unwrap()
                .contains("本文を書き換えました")
        );

        let comment_edit = format!(
            r#"{{"userContentEdits":{{"nodes":[]}},"comments":{{"nodes":[{{"author":{{"login":"y-hirakaw"}},"userContentEdits":{{"nodes":[{{"editedAt":"{T3}","editor":{{"login":"eager"}}}}]}}}}]}}}}"#
        );
        assert!(
            verdict(&base, &comment_edit)
                .unwrap()
                .contains("y-hirakaw のコメントを書き換えました")
        );

        // ラベルを付ける前の編集や、operator自身の編集は問わない
        let earlier = format!(
            r#"{{"userContentEdits":{{"nodes":[{{"editedAt":"{T1}","editor":{{"login":"eager"}}}},{{"editedAt":"{T3}","editor":{{"login":"y-hirakaw"}}}}]}},"comments":{{"nodes":[]}}}}"#
        );
        assert_eq!(verdict(&base, &earlier), None);
    }
}
