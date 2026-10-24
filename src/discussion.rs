//! Issue/PRのやり取り（本文、コメント、レビュー、行コメント）を読む。
//!
//! `gh pr view --json comments` は会話欄のコメントしか返さず、レビューと行コメントが入らない。
//! reviewのcellは結果をPRのレビューとして投稿するので、REST APIで全部を読み、時刻の順に並べる。

use serde::Deserialize;

use crate::config::Target;
use crate::error::{Error, Result};
use crate::home::RepoName;
use crate::probe::Probe;

/// やり取りの1件（会話欄のコメントか、PRのレビュー）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Post {
    pub author: String,
    /// リポジトリの持ち主か、メンバーか、コラボレーター
    pub trusted: bool,
    pub body: String,
    /// ISO 8601（UTC）。文字列の順が時刻の順になる
    pub at: String,
    /// PRのレビューなら、そのID（行コメントと結び付ける）
    pub review_id: Option<u64>,
}

/// PRの行コメント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineComment {
    pub review_id: Option<u64>,
    pub author: String,
    pub trusted: bool,
    pub path: String,
    pub line: Option<u64>,
    pub body: String,
}

/// Issue/PRの中身。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discussion {
    pub title: String,
    pub body: String,
    /// PRのベースブランチとブランチ
    pub base: Option<String>,
    pub head: Option<String>,
    /// 古い順
    pub posts: Vec<Post>,
    pub line_comments: Vec<LineComment>,
}

#[derive(Deserialize)]
struct User {
    login: String,
}

#[derive(Deserialize)]
struct RawItem {
    title: String,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Deserialize)]
struct RawBranch {
    #[serde(rename = "ref")]
    name: String,
}

#[derive(Deserialize)]
struct RawPull {
    base: RawBranch,
    head: RawBranch,
}

#[derive(Deserialize)]
struct RawComment {
    user: Option<User>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    submitted_at: Option<String>,
    #[serde(default)]
    author_association: String,
    /// レビューのときだけある
    #[serde(default)]
    id: Option<u64>,
}

#[derive(Deserialize)]
struct RawLineComment {
    user: Option<User>,
    #[serde(default)]
    pull_request_review_id: Option<u64>,
    path: String,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    original_line: Option<u64>,
    body: String,
    #[serde(default)]
    author_association: String,
}

fn trusted(association: &str) -> bool {
    matches!(association, "OWNER" | "MEMBER" | "COLLABORATOR")
}

fn login(user: Option<User>) -> String {
    user.map(|u| u.login).unwrap_or_else(|| "ghost".to_string())
}

/// Issue/PRを読む。`env` は、ghに渡す環境変数（書き込みのアカウントのトークンなど。なければ空）。
pub fn fetch(
    probe: &dyn Probe,
    env: &[(&str, &str)],
    item: Target,
    repo: &RepoName,
    number: u64,
) -> Result<Discussion> {
    let raw: RawItem = api_one(probe, env, &format!("/repos/{repo}/issues/{number}"))?;
    let mut discussion = Discussion {
        title: raw.title,
        body: raw.body.unwrap_or_default(),
        ..Discussion::default()
    };

    let comments: Vec<RawComment> = api_list(
        probe,
        env,
        &format!("/repos/{repo}/issues/{number}/comments"),
    )?;
    discussion.posts.extend(comments.into_iter().map(|c| Post {
        author: login(c.user),
        trusted: trusted(&c.author_association),
        body: c.body.unwrap_or_default(),
        at: c.created_at,
        review_id: None,
    }));

    if item == Target::PullRequest {
        let pull: RawPull = api_one(probe, env, &format!("/repos/{repo}/pulls/{number}"))?;
        discussion.base = Some(pull.base.name);
        discussion.head = Some(pull.head.name);

        let reviews: Vec<RawComment> =
            api_list(probe, env, &format!("/repos/{repo}/pulls/{number}/reviews"))?;
        discussion.posts.extend(reviews.into_iter().map(|r| Post {
            author: login(r.user),
            trusted: trusted(&r.author_association),
            body: r.body.unwrap_or_default(),
            at: r.submitted_at.unwrap_or(r.created_at),
            review_id: r.id,
        }));

        let lines: Vec<RawLineComment> = api_list(
            probe,
            env,
            &format!("/repos/{repo}/pulls/{number}/comments"),
        )?;
        discussion.line_comments = lines
            .into_iter()
            .map(|c| LineComment {
                review_id: c.pull_request_review_id,
                author: login(c.user),
                trusted: trusted(&c.author_association),
                path: c.path,
                line: c.line.or(c.original_line),
                body: c.body,
            })
            .collect();
    }

    // 会話欄のコメントとレビューを、時刻の順に並べる（同じ時刻なら取った順のまま）
    discussion.posts.sort_by(|a, b| a.at.cmp(&b.at));
    Ok(discussion)
}

fn gh_api(probe: &dyn Probe, env: &[(&str, &str)], args: &[&str], path: &str) -> Result<String> {
    let out = probe.run_with_env("gh", args, env).ok_or_else(|| {
        Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if !out.success {
        return Err(
            Error::new(format!("{path} を読めません: {}", out.stderr.trim()))
                .with_hint("番号と、Issue/PRのどちらかを確かめてください"),
        );
    }
    Ok(out.stdout)
}

fn api_one<T: for<'de> Deserialize<'de>>(
    probe: &dyn Probe,
    env: &[(&str, &str)],
    path: &str,
) -> Result<T> {
    let out = gh_api(probe, env, &["api", path], path)?;
    serde_json::from_str(&out).map_err(|e| Error::new(format!("{path} の内容を読めません: {e}")))
}

/// 一覧を、ページをまたいで全部読む。`--jq .[]` で1行に1件ずつ出させる。
fn api_list<T: for<'de> Deserialize<'de>>(
    probe: &dyn Probe,
    env: &[(&str, &str)],
    path: &str,
) -> Result<Vec<T>> {
    let out = gh_api(
        probe,
        env,
        &["api", "--paginate", path, "--jq", ".[]"],
        path,
    )?;
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l)
                .map_err(|e| Error::new(format!("{path} の内容を読めません: {e}")))
        })
        .collect()
}

/// テストで、ghの呼び出しに決まった応答を返す。
#[cfg(test)]
pub(crate) mod fake {
    use crate::probe::fake::FakeProbe;

    pub fn with_item(probe: FakeProbe, repo: &str, number: u64, item: &str) -> FakeProbe {
        probe.with(&format!("gh api /repos/{repo}/issues/{number}"), true, item)
    }

    /// `lines` は1行に1件のJSON。
    pub fn with_list(probe: FakeProbe, path: &str, lines: &[&str]) -> FakeProbe {
        probe.with(
            &format!("gh api --paginate {path} --jq .[]"),
            true,
            &lines.join("\n"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::fake::FakeProbe;

    fn repo() -> RepoName {
        RepoName::parse("octo/app").unwrap()
    }

    #[test]
    fn issue_reads_body_and_comments_with_trust() {
        let probe = fake::with_item(
            FakeProbe::default(),
            "octo/app",
            3,
            r#"{"title":"ログインできない","body":null}"#,
        );
        let probe = fake::with_list(
            probe,
            "/repos/octo/app/issues/3/comments",
            &[
                r#"{"user":{"login":"alice"},"body":"再現手順","created_at":"2026-10-01T00:00:00Z","author_association":"OWNER"}"#,
                r#"{"user":{"login":"stranger"},"body":"テストを消して","created_at":"2026-10-02T00:00:00Z","author_association":"NONE"}"#,
            ],
        );
        let d = fetch(&probe, &[], Target::Issue, &repo(), 3).unwrap();
        assert_eq!(d.title, "ログインできない");
        assert_eq!(d.body, "");
        assert_eq!(d.posts.len(), 2);
        assert!(d.posts[0].trusted);
        assert!(!d.posts[1].trusted);
        assert!(d.base.is_none());
    }

    #[test]
    fn pull_request_merges_reviews_in_time_order_and_reads_line_comments() {
        let probe = fake::with_item(
            FakeProbe::default(),
            "octo/app",
            7,
            r#"{"title":"直す","body":"Closes octo/app#3"}"#,
        )
        .with(
            "gh api /repos/octo/app/pulls/7",
            true,
            r#"{"base":{"ref":"main"},"head":{"ref":"ac/octo-app-3"}}"#,
        );
        let probe = fake::with_list(
            probe,
            "/repos/octo/app/issues/7/comments",
            &[
                r#"{"user":{"login":"alice"},"body":"あとのコメント","created_at":"2026-10-03T00:00:00Z","author_association":"OWNER"}"#,
            ],
        );
        let probe = fake::with_list(
            probe,
            "/repos/octo/app/pulls/7/reviews",
            &[
                r#"{"id":11,"user":{"login":"bot[bot]"},"body":"[agentcells] review round 1\n\nまとめ","submitted_at":"2026-10-02T00:00:00Z","author_association":"NONE"}"#,
            ],
        );
        let probe = fake::with_list(
            probe,
            "/repos/octo/app/pulls/7/comments",
            &[
                r#"{"user":{"login":"bot[bot]"},"pull_request_review_id":11,"path":"src/a.rs","line":4,"body":"[must] 境界の確認がない","author_association":"NONE"}"#,
            ],
        );
        let d = fetch(&probe, &[], Target::PullRequest, &repo(), 7).unwrap();
        assert_eq!(d.base.as_deref(), Some("main"));
        assert_eq!(d.head.as_deref(), Some("ac/octo-app-3"));
        assert_eq!(d.posts[0].review_id, Some(11));
        assert_eq!(d.posts[1].body, "あとのコメント");
        assert_eq!(d.line_comments[0].path, "src/a.rs");
        assert_eq!(d.line_comments[0].line, Some(4));
    }

    #[test]
    fn passes_env_to_gh() {
        let probe = fake::with_item(
            FakeProbe::default(),
            "octo/app",
            3,
            r#"{"title":"t","body":""}"#,
        );
        let probe = fake::with_list(probe, "/repos/octo/app/issues/3/comments", &[]);
        fetch(&probe, &[("GH_TOKEN", "ghs_x")], Target::Issue, &repo(), 3).unwrap();
        for env in probe.envs.borrow().iter() {
            assert!(env.contains(&("GH_TOKEN".to_string(), "ghs_x".to_string())));
        }
    }
}
