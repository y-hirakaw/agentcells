//! Issue/PRのやり取り（本文、コメント、レビュー、行コメント）を読む。
//!
//! `gh pr view --json comments` は会話欄のコメントしか返さず、レビューと行コメントが入らない。
//! reviewのcellは結果をPRのレビューとして投稿するので、REST APIで全部を読み、時刻の順に並べる。

use serde::Deserialize;

use crate::config::Target;
use crate::error::{Error, Result};
use crate::home::RepoName;
use crate::marker;
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
    /// 本文を書いた人
    pub author: String,
    /// 本文を書いた人が、リポジトリの持ち主か、メンバーか、コラボレーターか
    pub author_trusted: bool,
    /// 作られた時刻（ISO 8601、UTC）
    pub created_at: String,
    /// PRのベースブランチとブランチ
    pub base: Option<String>,
    pub head: Option<String>,
    /// PRのブランチがあるリポジトリ（`owner/repo`）。フォークからのPRなら、作業するリポジトリと違う
    pub head_repo: Option<String>,
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
    user: Option<User>,
    #[serde(default)]
    author_association: String,
    #[serde(default)]
    created_at: String,
}

#[derive(Deserialize)]
struct RawBranch {
    #[serde(rename = "ref")]
    name: String,
    #[serde(default)]
    repo: Option<RawRepo>,
}

#[derive(Deserialize)]
struct RawRepo {
    full_name: String,
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
        author: login(raw.user),
        author_trusted: trusted(&raw.author_association),
        created_at: raw.created_at,
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
        discussion.head_repo = pull.head.repo.map(|r| r.full_name);

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

/// 書き込みのアカウントが書いた、目印付きの投稿か（目印は誰でも書けるので、書いた人も見る）。
pub fn is_marker(post: &Post, writer: &str) -> bool {
    marker::parse(&post.body).is_some() && post.author == writer
}

/// 最後の目印付きの投稿の位置。
pub fn last_marker(posts: &[Post], writer: &str) -> Option<usize> {
    posts.iter().rposition(|p| is_marker(p, writer))
}

/// cellがまだ読んでいない投稿（目印は除く）。最後の目印に書いた「読んだところ」より後のもの。
/// その行がなければ（動かせなかったときなど）、最後の目印より後のもの。目印がなければ、すべて。
/// cellが動いている間に書かれたコメントは、目印より前に並ぶが、読んでいないものとして扱う。
pub fn unread<'a>(posts: &'a [Post], writer: &str) -> Vec<&'a Post> {
    let boundary = last_marker(posts, writer).map(|i| {
        let posted = posts[i].at.as_str();
        // 目印の投稿より後を「読んだ」とは書けない
        match marker::read_until(&posts[i].body) {
            Some(at) if at < posted => at,
            _ => posted,
        }
    });
    posts
        .iter()
        .filter(|p| !is_marker(p, writer))
        .filter(|p| boundary.is_none_or(|b| p.at.as_str() > b))
        .collect()
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
pub(crate) fn api_list<T: for<'de> Deserialize<'de>>(
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

    fn post(author: &str, body: &str, at: &str) -> Post {
        Post {
            author: author.to_string(),
            trusted: true,
            body: body.to_string(),
            at: at.to_string(),
            review_id: None,
        }
    }

    fn bodies<'a>(posts: &[&'a Post]) -> Vec<&'a str> {
        posts.iter().map(|p| p.body.as_str()).collect()
    }

    #[test]
    fn unread_posts_start_where_the_last_marker_read_until() {
        let marker = "[agentcells] triage round 1\n\n質問\n\n<!-- agentcells read-until 2026-10-25T03:00:00Z -->";
        let posts = vec![
            post("me", "読んだ", "2026-10-25T02:00:00Z"),
            // cellが動いている間に書かれた（目印より前に並ぶ）
            post("me", "動いている間", "2026-10-25T03:01:00Z"),
            post("bot[bot]", marker, "2026-10-25T03:05:00Z"),
            // ほかの人が書いた目印は、目印として扱わない
            post(
                "other",
                "[agentcells] triage round 9",
                "2026-10-25T03:06:00Z",
            ),
            post("me", "あと", "2026-10-25T03:10:00Z"),
        ];
        assert_eq!(
            bodies(&unread(&posts, "bot[bot]")),
            vec!["動いている間", "[agentcells] triage round 9", "あと"]
        );
        assert_eq!(last_marker(&posts, "bot[bot]"), Some(2));
        // 目印がなければ、すべて読んでいない
        assert_eq!(unread(&posts[..2], "bot[bot]").len(), 2);
    }

    #[test]
    fn unread_posts_without_read_until_or_with_a_later_one_start_at_the_marker() {
        for marker in [
            // 動かせなかったとき（round 0）は、読んだところを書かない
            "[agentcells] triage round 0\n\n実行できませんでした".to_string(),
            // 目印の投稿より後を「読んだ」とは書けない
            "[agentcells] triage round 1\n\n<!-- agentcells read-until 2099-01-01T00:00:00Z -->"
                .to_string(),
        ] {
            let posts = vec![
                post("me", "前", "2026-10-25T03:01:00Z"),
                post("bot[bot]", &marker, "2026-10-25T03:05:00Z"),
                post("me", "あと", "2026-10-25T03:10:00Z"),
            ];
            assert_eq!(
                bodies(&unread(&posts, "bot[bot]")),
                vec!["あと"],
                "{marker}"
            );
        }
    }

    #[test]
    fn issue_reads_body_and_comments_with_trust() {
        let probe = fake::with_item(
            FakeProbe::default(),
            "octo/app",
            3,
            r#"{"title":"ログインできない","body":null,"user":{"login":"stranger"},"author_association":"NONE"}"#,
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
        assert_eq!(d.author, "stranger");
        assert!(!d.author_trusted);
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
