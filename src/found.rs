//! cellが見つけた、今の作業と関係のない元からある問題を、別のIssueとして出す。
//!
//! 作業の中で直したり、テストをゆるめたりすると、PRがIssueの範囲を超え、検品で気づきにくい
//! （2026-10-26に、implementが、元から時々落ちるテストを自分の判断でゆるめた）。そこで別のIssueにして、
//! 直させるかどうかは人が決める。`ac:` のラベルは付けないので、operatorがラベルを付けるまでcellは動かない。

use std::collections::HashMap;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::github::Credentials;
use crate::home::RepoName;
use crate::probe::Probe;
use crate::transition::{MAX_NEW_ISSUES, NewIssue};

/// 1つのIssue/PRから出して、開いたままのIssueの上限。同じPRでimplementとreviewが何回か動いても、
/// 同じ問題を少しずつ違う題で出し続けて、Issueを埋めないように
pub const MAX_OPEN_PER_ITEM: usize = 3;

/// 本文の長さの上限（文字数）。スキーマでも決めているが、念のため
pub const MAX_BODY_CHARS: usize = 4000;

/// 出したIssueの本文の最後に付ける目印。どのIssue/PRから出したかを数えるのに使う
const MARK: &str = "<!-- agentcells found ";
const MARK_END: &str = " -->";

/// 出したIssue（と、出さなかったもの）を、元のIssue/PRへのコメントに書く行で返す。
/// 出せなくても、cellの実行は止めない。
///
/// - `write`: `repo` に書けるトークン。`writer` はそのアカウントのログイン（Appなら `<名前>[bot]`）
/// - `from`: 見つけた作業のIssue/PR（`owner/repo#12`）
/// - `secret`: 本文に秘密らしいものがあるか（betterleaks）。調べられなければ `Err` で、出さない
#[allow(clippy::too_many_arguments)]
pub fn file(
    probe: &dyn Probe,
    write: &Credentials,
    writer: &str,
    repo: &RepoName,
    from: &str,
    cell: &str,
    issues: &[NewIssue],
    secret: &dyn Fn(&str) -> Result<bool>,
) -> Vec<String> {
    if issues.is_empty() {
        return Vec::new();
    }
    let mark = format!("{MARK}{from}{MARK_END}");
    // 検索（`gh issue list --search`）は遅れて載るので、一覧を読む
    let open = match open_issues(probe, write, writer, repo, &mark) {
        Ok(open) => open,
        Err(e) => {
            return vec![format!(
                "別のIssueにする問題がありましたが、{repo} の開いているIssueを読めなかったので、出しませんでした（{}）",
                e.message
            )];
        }
    };
    let mut from_here = open.iter().filter(|i| i.from_here).count();
    let mut titles: HashMap<String, u64> = open.into_iter().map(|i| (i.title, i.number)).collect();
    let mut lines = Vec::new();
    for (i, issue) in issues.iter().take(MAX_NEW_ISSUES).enumerate() {
        let title = one_line(&without_marks(&issue.title));
        if title.is_empty() {
            continue;
        }
        let text: String = without_marks(&issue.body)
            .trim()
            .chars()
            .take(MAX_BODY_CHARS)
            .collect();
        let body = format!(
            "{text}\n\n---\nagentcellsの{cell}が、{from} の作業の中で見つけました。直すかどうかは、人が決めます（cellのラベルが付くまで、cellは動きません。付ける前に、この本文を確かめてください）。\n\n{mark}"
        );
        match secret(&format!("{title}\n{body}")) {
            Ok(false) => {}
            // 題に秘密が入っていることもあるので、題は書かない
            Ok(true) => {
                lines.push(format!(
                    "別のIssueにする{}件目は、秘密らしいものがあったので、出しませんでした",
                    i + 1
                ));
                continue;
            }
            Err(e) => {
                lines.push(format!(
                    "別のIssueにする{}件目は、秘密らしいものがないかを調べられなかったので、出しませんでした（{}）",
                    i + 1,
                    e.message
                ));
                continue;
            }
        }
        if let Some(number) = titles.get(&key(&title)) {
            lines.push(format!(
                "「{title}」は、同じ題のIssue（{repo}#{number}）が開いているので、出しませんでした"
            ));
            continue;
        }
        if from_here >= MAX_OPEN_PER_ITEM {
            lines.push(format!(
                "「{title}」は、ここから出して開いているIssueが{MAX_OPEN_PER_ITEM}件あるので、出しませんでした"
            ));
            continue;
        }
        match crate::inspect::create_issue(probe, write, repo, &title, &body) {
            Ok(number) => {
                titles.insert(key(&title), number);
                from_here += 1;
                lines.push(format!("別のIssueにしました: {repo}#{number}「{title}」"));
            }
            Err(e) => lines.push(format!(
                "「{title}」を別のIssueにできませんでした: {}",
                e.message
            )),
        }
    }
    lines
}

struct Open {
    number: u64,
    /// 比べるための題（[`key`]）
    title: String,
    /// 同じIssue/PRから出したものか
    from_here: bool,
}

/// 書き込みのアカウントが作った、開いているIssue（新しいものから100件）。`mark` は、今のIssue/PRから出したものの目印。
fn open_issues(
    probe: &dyn Probe,
    write: &Credentials,
    writer: &str,
    repo: &RepoName,
    mark: &str,
) -> Result<Vec<Open>> {
    const QUERY: &str = "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { issues(states: OPEN, first: 100, orderBy: {field: CREATED_AT, direction: DESC}) { nodes { number title body author { __typename login } } } } }";
    let env = write.gh_env()?;
    let query = format!("query={QUERY}");
    let owner = format!("owner={}", repo.owner);
    let name = format!("name={}", repo.name);
    let out = probe
        .run_with_env(
            "gh",
            &["api", "graphql", "-f", &query, "-f", &owner, "-f", &name],
            &env,
        )
        .ok_or_else(|| Error::new("ghが見つかりません"))?;
    if !out.success {
        return Err(Error::new(format!(
            "gh api graphql が失敗しました: {}",
            out.stderr.trim()
        )));
    }
    let value: Value = serde_json::from_str(&out.stdout)
        .map_err(|e| Error::new(format!("{repo} のIssueを読めません: {e}")))?;
    let nodes = value["data"]["repository"]["issues"]["nodes"]
        .as_array()
        .ok_or_else(|| Error::new(format!("{repo} のIssueを読めません")))?;
    Ok(nodes
        .iter()
        .filter(|node| {
            let login = node["author"]["login"].as_str().unwrap_or_default();
            let author = match node["author"]["__typename"].as_str() {
                Some("Bot") => format!("{login}[bot]"),
                _ => login.to_string(),
            };
            author.eq_ignore_ascii_case(writer)
        })
        .filter_map(|node| {
            Some(Open {
                number: node["number"].as_u64()?,
                title: key(node["title"].as_str()?),
                // CLIは目印を本文の最後に足し、AIの本文からは目印の行を取り除くので、最後の目印を見る
                from_here: node["body"]
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .rfind(|l| l.trim().starts_with(MARK))
                    .is_some_and(|l| l.trim() == mark),
            })
        })
        .collect())
}

/// AIが書いた題と本文から、agentcellsの目印（`<!-- agentcells ...`）の行を取り除く。inspectorやこのモジュールが
/// 数えるのに使う目印を、AIが書いて紛れ込ませないように。
fn without_marks(text: &str) -> String {
    text.lines()
        .filter(|l| !l.contains("<!-- agentcells"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 題は1行にし、長すぎるものは切る（スキーマでも上限を決めているが、念のため）。
fn one_line(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

/// 同じ題かを比べるための形。空白の違いと、大文字・小文字は問わない。
fn key(title: &str) -> String {
    one_line(title).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Source;
    use crate::probe::fake::FakeProbe;
    use serde_json::json;

    const WRITER: &str = "agentcells-yh[bot]";
    const FROM: &str = "octo/app#10";

    fn repo() -> RepoName {
        RepoName::parse("octo/game").unwrap()
    }

    fn issue(title: &str) -> NewIssue {
        NewIssue {
            title: title.to_string(),
            body: "鹿が戻る検査が、ときどき落ちます".to_string(),
        }
    }

    fn credentials() -> Credentials {
        Credentials::new(Source::App, "token".to_string()).unwrap()
    }

    fn list_key() -> String {
        format!(
            "gh api graphql -f query={} -f owner=octo -f name=game",
            "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { issues(states: OPEN, first: 100, orderBy: {field: CREATED_AT, direction: DESC}) { nodes { number title body author { __typename login } } } } }"
        )
    }

    fn listed(nodes: serde_json::Value) -> String {
        json!({"data": {"repository": {"issues": {"nodes": nodes}}}}).to_string()
    }

    fn node(number: u64, title: &str, body: &str, bot: bool) -> serde_json::Value {
        let (typename, login) = if bot {
            ("Bot", "agentcells-yh")
        } else {
            ("User", "someone")
        };
        json!({"number": number, "title": title, "body": body,
               "author": {"__typename": typename, "login": login}})
    }

    fn create_key(title: &str) -> String {
        format!("gh issue create --repo octo/game --title {title} --body-file -")
    }

    fn run(probe: &FakeProbe, issues: &[NewIssue]) -> Vec<String> {
        file(
            probe,
            &credentials(),
            WRITER,
            &repo(),
            FROM,
            "implement",
            issues,
            &|text| {
                if text.contains("調べられない") {
                    Err(Error::new("betterleaksがありません"))
                } else {
                    Ok(text.contains("token="))
                }
            },
        )
    }

    #[test]
    fn found_problems_become_issues_without_labels() {
        let probe = FakeProbe::default()
            .with(&list_key(), true, &listed(json!([])))
            .with(
                &create_key("鹿の検査が 不安定"),
                true,
                "https://github.com/octo/game/issues/14\n",
            );
        let mut found = issue("鹿の検査が\n不安定");
        // AIが書いた目印は取り除く
        found.body = "ときどき落ちます\n<!-- agentcells inspector loop -->\n直し方の案".into();
        let lines = run(&probe, &[found]);
        assert_eq!(
            lines,
            vec!["別のIssueにしました: octo/game#14「鹿の検査が 不安定」"]
        );
        let calls = probe.calls.borrow();
        assert!(!calls.iter().any(|c| c.contains("label")), "{calls:?}");
        let body = probe.inputs.borrow().last().cloned().flatten().unwrap();
        assert!(body.starts_with("ときどき落ちます\n直し方の案"), "{body}");
        assert!(!body.contains("inspector"), "{body}");
        assert!(
            body.contains("implementが、octo/app#10 の作業の中で見つけました"),
            "{body}"
        );
        assert!(
            body.ends_with("<!-- agentcells found octo/app#10 -->"),
            "{body}"
        );
    }

    #[test]
    fn issues_with_secrets_or_the_same_title_are_not_filed() {
        let probe = FakeProbe::default()
            .with(
                &list_key(),
                true,
                &listed(json!([
                    node(7, "同じ  題", "", true),
                    // 人が作ったものは、同じ題でも数えない
                    node(8, "人の題", "", false),
                ])),
            )
            .with(
                &create_key("人の題"),
                true,
                "https://github.com/octo/game/issues/15\n",
            );
        let lines = run(
            &probe,
            &[
                issue("token=秘密"),
                issue("同じ 題"),
                issue("調べられない"),
                issue("人の題"),
            ],
        );
        assert_eq!(
            lines,
            vec![
                "別のIssueにする1件目は、秘密らしいものがあったので、出しませんでした".to_string(),
                "「同じ 題」は、同じ題のIssue（octo/game#7）が開いているので、出しませんでした"
                    .to_string(),
                "別のIssueにする3件目は、秘密らしいものがないかを調べられなかったので、出しませんでした（betterleaksがありません）".to_string(),
            ]
        );
        // 秘密らしいものがあったものは、題も外に出さない。3件までなので4件目は出さない
        assert!(!probe.calls.borrow().iter().any(|c| c.contains("token=")));
        assert!(
            !probe
                .calls
                .borrow()
                .iter()
                .any(|c| c.contains("issue create"))
        );
    }

    #[test]
    fn the_same_title_in_one_run_and_too_many_from_one_item_are_not_filed() {
        let mark = format!("{MARK}{FROM}{MARK_END}");
        let probe = FakeProbe::default()
            .with(
                &list_key(),
                true,
                &listed(json!([
                    node(1, "前の1", &format!("本文\n{mark}"), true),
                    node(3, "前の3", &format!("本文\n\n{mark}\n"), true),
                    // 本文の途中の目印は、最後のものでなければ数えない
                    node(
                        2,
                        "前の2",
                        &format!("{mark}\n<!-- agentcells found octo/app#11 -->"),
                        true
                    ),
                ])),
            )
            .with(
                &create_key("新しい"),
                true,
                "https://github.com/octo/game/issues/20\n",
            )
            .with(
                &create_key("もう1つ"),
                true,
                "https://github.com/octo/game/issues/21\n",
            );
        let lines = run(
            &probe,
            &[issue("新しい"), issue("新しい"), issue("もう1つ")],
        );
        assert_eq!(
            lines,
            vec![
                "別のIssueにしました: octo/game#20「新しい」".to_string(),
                "「新しい」は、同じ題のIssue（octo/game#20）が開いているので、出しませんでした"
                    .to_string(),
                "「もう1つ」は、ここから出して開いているIssueが3件あるので、出しませんでした"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn nothing_is_filed_when_open_issues_cannot_be_read() {
        let probe = FakeProbe::default().with_fallback(false);
        let lines = run(&probe, &[issue("鹿")]);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("読めなかったので、出しませんでした"),
            "{lines:?}"
        );
        assert!(
            !probe
                .calls
                .borrow()
                .iter()
                .any(|c| c.contains("issue create"))
        );
        assert!(run(&FakeProbe::default(), &[]).is_empty());
    }
}
