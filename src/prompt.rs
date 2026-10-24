//! cellとして動くAIに渡すプロンプトを組み立てる。
//!
//! 3つの層を重ねる。
//! - 標準: agentcellsが用意する。全cellに共通の決まりと、標準のcell（triage、implement、review）の役割。
//!   `prompts/` のmdをバイナリに埋め込む。`claude -p` には `--append-system-prompt` で渡す
//! - 追加: 使う人が `prompt` に書いたファイル。プロダクトごとの方針などを足す
//! - 今回の仕事: CLIが、Issue/PRの内容と、前回のagentcellsのコメントより後の人間のコメントを入れる
//!
//! 使う人から標準のプロンプトが見えないと困るので、`agentcells prompt show` で組み立てた結果を表示できる。

use std::path::PathBuf;

use serde::Deserialize;

use crate::config::{Cell, Config, Target};
use crate::error::{Error, Result};
use crate::home::RepoName;
use crate::marker;
use crate::probe::Probe;
use crate::transition::{self, DONE, NEEDS_HUMAN};

const COMMON: &str = include_str!("../prompts/common.md");

/// 標準のcellの役割。
const BUILTIN: &[(&str, &str)] = &[
    ("triage", include_str!("../prompts/triage.md")),
    ("implement", include_str!("../prompts/implement.md")),
    ("review", include_str!("../prompts/review.md")),
];

/// 標準のcellなら、その役割のプロンプトを返す。
pub fn builtin(name: &str) -> Option<&'static str> {
    BUILTIN.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
}

/// Issue/PRへのコメント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub author: String,
    pub body: String,
}

/// 今回の仕事。
#[derive(Debug, Clone)]
pub struct Task {
    pub item: Target,
    pub repo: RepoName,
    pub number: u64,
    pub title: String,
    pub body: String,
    /// 古い順
    pub comments: Vec<Comment>,
    /// このcellが動くのは何回目か
    pub round: u32,
    /// 作業するworktree（あれば）
    pub worktree: Option<PathBuf>,
    /// 作業するブランチ（あれば）
    pub branch: Option<String>,
    /// PRのベースブランチ（PRなら）
    pub base: Option<String>,
}

/// システムプロンプト（`--append-system-prompt` で渡す）。標準の決まり、cellの役割、進め先を書く。
pub fn system_prompt(cell: &Cell) -> String {
    let mut prompt = COMMON.replace("{cell}", &cell.name).trim_end().to_string();
    if let Some(role) = builtin(&cell.name) {
        prompt.push_str("\n\n");
        prompt.push_str(role.trim_end());
    }
    prompt.push_str("\n\n## 進め先（next）\n\n次のどれかを選んでください。\n");
    for choice in transition::choices(cell) {
        let meaning = match choice.as_str() {
            DONE => "これ以上することがない（Issueなら閉じる。PRなら人間に検品を頼む）".to_string(),
            NEEDS_HUMAN => "人間の答えや作業が要る".to_string(),
            name => format!("cell「{name}」に回す"),
        };
        prompt.push_str(&format!("\n- `{choice}`: {meaning}"));
    }
    prompt.push('\n');
    prompt
}

/// 指示の本文。使う人の追加の指示と、今回の仕事を書く。
pub fn user_prompt(cell: &Cell, extra: Option<&str>, task: Option<&Task>) -> String {
    let mut sections = Vec::new();
    if let Some(extra) = extra.map(str::trim).filter(|e| !e.is_empty()) {
        sections.push(format!("## このプロダクトでの追加の指示\n\n{extra}"));
    }
    match task {
        Some(task) => sections.push(task_section(cell, task)),
        None => sections.push(
            "## 今回の仕事\n\n（実行するときに、Issue/PRの内容がここに入ります）".to_string(),
        ),
    }
    let mut prompt = sections.join("\n\n");
    prompt.push('\n');
    prompt
}

fn task_section(cell: &Cell, task: &Task) -> String {
    let kind = match task.item {
        Target::Issue => "Issue",
        Target::PullRequest => "PR",
    };
    let mut lines = vec![
        "## 今回の仕事".to_string(),
        String::new(),
        format!(
            "- 対象: {kind} {}#{}「{}」",
            task.repo, task.number, task.title
        ),
        format!(
            "- この cell が動くのは {} 回目です（上限 {} 回）",
            task.round, cell.max_rounds
        ),
    ];
    if let Some(worktree) = &task.worktree {
        lines.push(format!("- 作業場所: {}", worktree.display()));
    }
    if let Some(branch) = &task.branch {
        lines.push(format!("- ブランチ: {branch}"));
    }
    if let Some(base) = &task.base {
        lines.push(format!(
            "- ベースブランチ: {base}（差分は `git diff origin/{base}...HEAD` で見られます）"
        ));
    }
    lines.push(String::new());
    lines.push("### 本文".to_string());
    lines.push(String::new());
    lines.push(non_empty(&task.body, "（本文はありません）"));
    lines.push(String::new());
    lines.push("### 前回の agentcells のコメントより後の、人間のコメント".to_string());
    lines.push(String::new());
    let comments = human_comments_since_last_marker(&task.comments);
    if comments.is_empty() {
        lines.push("（ありません）".to_string());
    } else {
        for c in comments {
            lines.push(format!("@{}:\n{}\n", c.author, c.body.trim()));
        }
    }
    lines.join("\n").trim_end().to_string()
}

fn non_empty(text: &str, fallback: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        fallback.to_string()
    } else {
        text.to_string()
    }
}

/// 最後にagentcellsの目印が付いたコメントより後のコメント。目印がなければ全部。
/// 人間への質問の答えや、検品のあとの指示を、次の実行に渡すため。
pub fn human_comments_since_last_marker(comments: &[Comment]) -> Vec<&Comment> {
    let start = comments
        .iter()
        .rposition(|c| marker::parse(&c.body).is_some())
        .map(|i| i + 1)
        .unwrap_or(0);
    comments[start..]
        .iter()
        .filter(|c| marker::parse(&c.body).is_none())
        .collect()
}

/// `owner/repo#12` を読む。
pub fn parse_ref(text: &str) -> Result<(RepoName, u64)> {
    let invalid = || {
        Error::new(format!("「{text}」を読めません"))
            .with_hint("owner/repo#12 の形で書いてください")
    };
    let (repo, number) = text.split_once('#').ok_or_else(invalid)?;
    let number: u64 = number.parse().map_err(|_| invalid())?;
    Ok((RepoName::parse(repo)?, number))
}

#[derive(Deserialize)]
struct Viewed {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    comments: Vec<ViewedComment>,
    #[serde(default, rename = "baseRefName")]
    base: Option<String>,
    #[serde(default, rename = "headRefName")]
    head: Option<String>,
}

#[derive(Deserialize)]
struct ViewedComment {
    #[serde(default)]
    author: Option<Author>,
    body: String,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

/// Issue/PRを読んで、今回の仕事を作る。読むのはghのアカウントで行う。
pub fn fetch_task(
    probe: &dyn Probe,
    cell: &Cell,
    item: Target,
    repo: &RepoName,
    number: u64,
) -> Result<Task> {
    let repo_arg = repo.to_string();
    let number_arg = number.to_string();
    let (kind, fields) = match item {
        Target::Issue => ("issue", "title,body,comments"),
        Target::PullRequest => ("pr", "title,body,comments,baseRefName,headRefName"),
    };
    let args = [
        kind,
        "view",
        number_arg.as_str(),
        "--repo",
        repo_arg.as_str(),
        "--json",
        fields,
    ];
    let out = probe.run("gh", &args).ok_or_else(|| {
        Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
    })?;
    if !out.success {
        return Err(Error::new(format!(
            "{repo}#{number} を読めません: {}",
            out.stderr.trim()
        ))
        .with_hint("番号と、Issue/PRのどちらかを確かめてください"));
    }
    let viewed: Viewed = serde_json::from_str(&out.stdout)
        .map_err(|e| Error::new(format!("{repo}#{number} の内容を読めません: {e}")))?;
    let comments: Vec<Comment> = viewed
        .comments
        .into_iter()
        .map(|c| Comment {
            author: c
                .author
                .map(|a| a.login)
                .unwrap_or_else(|| "ghost".to_string()),
            body: c.body,
        })
        .collect();
    let round = marker::rounds(comments.iter().map(|c| c.body.as_str()), &cell.name) + 1;
    Ok(Task {
        item,
        repo: repo.clone(),
        number,
        title: viewed.title,
        body: viewed.body,
        comments,
        round,
        worktree: None,
        branch: viewed.head,
        base: viewed.base,
    })
}

/// `agentcells prompt show` の出力。AIに渡るプロンプトを、層ごとに見出しを付けて並べる。
pub fn show(
    config: &Config,
    probe: &dyn Probe,
    cell_name: &str,
    target: Option<(Target, &str)>,
) -> Result<String> {
    let cell = config.cell(cell_name).ok_or_else(|| {
        let names: Vec<&str> = config.cells.iter().map(|c| c.name.as_str()).collect();
        Error::new(format!("cell「{cell_name}」は設定にありません")).with_hint(format!(
            "設定にあるcell: {}",
            if names.is_empty() {
                "（なし）".to_string()
            } else {
                names.join(", ")
            }
        ))
    })?;
    let extra = match &cell.prompt {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| Error::new(format!("{} を読めません: {e}", path.display())))?,
        ),
        None => None,
    };
    let task = match target {
        Some((item, text)) => {
            let (repo, number) = parse_ref(text)?;
            Some(fetch_task(probe, cell, item, &repo, number)?)
        }
        None => None,
    };
    let extra_note = match &cell.prompt {
        Some(path) => format!("追加の指示は {}", path.display()),
        None => "追加の指示のファイルはありません".to_string(),
    };
    Ok(format!(
        "===== システムプロンプト（agentcellsの標準。`--append-system-prompt` で渡す） =====\n\n{}\n===== 指示（{extra_note}） =====\n\n{}",
        system_prompt(cell),
        user_prompt(cell, extra.as_deref(), task.as_ref())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::fake::FakeProbe;

    fn cell(name: &str, next: &[&str]) -> Cell {
        Cell {
            name: name.to_string(),
            on: vec![Target::Issue],
            label: format!("ac:{name}"),
            prompt: None,
            next: next.iter().map(|s| s.to_string()).collect(),
            review: false,
            max_rounds: 3,
            timeout_minutes: 30,
        }
    }

    fn comment(author: &str, body: &str) -> Comment {
        Comment {
            author: author.to_string(),
            body: body.to_string(),
        }
    }

    #[test]
    fn builtin_cells_have_roles() {
        for name in ["triage", "implement", "review"] {
            let role = builtin(name).unwrap();
            assert!(role.contains(&format!("役割: {name}")), "{name}");
        }
        assert!(builtin("design").is_none());
    }

    #[test]
    fn system_prompt_has_rules_role_and_choices() {
        let prompt = system_prompt(&cell("triage", &["implement", "done"]));
        assert!(prompt.contains("cell「triage」として動いています"));
        assert!(!prompt.contains("{cell}"));
        assert!(prompt.contains("GitHub への書き込み"));
        assert!(prompt.contains("役割: triage"));
        assert!(prompt.contains("- `implement`: cell「implement」に回す"));
        assert!(prompt.contains("- `done`: "));
        // needs-human は書かなくても選べる
        assert!(prompt.contains("- `needs-human`: "));
    }

    #[test]
    fn custom_cells_get_only_the_common_rules() {
        let prompt = system_prompt(&cell("design", &["implement"]));
        assert!(prompt.contains("cell「design」"));
        assert!(!prompt.contains("役割:"));
    }

    #[test]
    fn user_prompt_adds_extra_and_task() {
        let task = Task {
            item: Target::Issue,
            repo: RepoName::parse("octo/app").unwrap(),
            number: 12,
            title: "ログインできない".to_string(),
            body: "本文です".to_string(),
            comments: vec![
                comment("alice", "最初の報告"),
                comment("bot", "[agentcells] triage round 1\n\n質問です"),
                comment("alice", "答えです"),
            ],
            round: 2,
            worktree: Some(PathBuf::from("/w/octo/app/12")),
            branch: Some("ac/octo-app-12".to_string()),
            base: None,
        };
        let prompt = user_prompt(
            &cell("triage", &["done"]),
            Some("質問は日本語で"),
            Some(&task),
        );
        assert!(prompt.starts_with("## このプロダクトでの追加の指示\n\n質問は日本語で"));
        assert!(prompt.contains("Issue octo/app#12「ログインできない」"));
        assert!(prompt.contains("2 回目です（上限 3 回）"));
        assert!(prompt.contains("/w/octo/app/12"));
        assert!(prompt.contains("@alice:\n答えです"));
        assert!(!prompt.contains("最初の報告"));
        assert!(!prompt.contains("質問です"));
    }

    #[test]
    fn user_prompt_without_task_shows_where_it_goes() {
        let prompt = user_prompt(&cell("triage", &["done"]), None, None);
        assert!(prompt.starts_with("## 今回の仕事"));
        assert!(!prompt.contains("追加の指示"));
    }

    #[test]
    fn parse_ref_reads_owner_repo_and_number() {
        let (repo, number) = parse_ref("octo/app#12").unwrap();
        assert_eq!(repo.to_string(), "octo/app");
        assert_eq!(number, 12);
        for bad in ["octo/app", "octo/app#x", "app#1"] {
            assert!(parse_ref(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn fetch_task_counts_rounds_from_markers() {
        let probe = FakeProbe::default().with(
            "gh pr view 7 --repo octo/app --json title,body,comments,baseRefName,headRefName",
            true,
            r#"{"title":"直す","body":"","baseRefName":"main","headRefName":"ac/octo-app-3",
                "comments":[{"author":{"login":"bot"},"body":"[agentcells] review round 1"},
                            {"author":{"login":"alice"},"body":"ここも見て"}]}"#,
        );
        let task = fetch_task(
            &probe,
            &cell("review", &["done"]),
            Target::PullRequest,
            &RepoName::parse("octo/app").unwrap(),
            7,
        )
        .unwrap();
        assert_eq!(task.round, 2);
        assert_eq!(task.base.as_deref(), Some("main"));
        assert_eq!(task.branch.as_deref(), Some("ac/octo-app-3"));
        let prompt = user_prompt(&cell("review", &["done"]), None, Some(&task));
        assert!(prompt.contains("git diff origin/main...HEAD"));
        assert!(prompt.contains("@alice:\nここも見て"));
    }

    #[test]
    fn show_lists_each_layer_and_reads_the_extra_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("triage.md"), "テストを必ず書く").unwrap();
        let config = Config::parse(
            r#"
[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
prompt = "triage.md"
next = ["done"]
"#,
            dir.path(),
        )
        .unwrap();
        let out = show(&config, &FakeProbe::default(), "triage", None).unwrap();
        assert!(out.contains("===== システムプロンプト"));
        assert!(out.contains("役割: triage"));
        assert!(out.contains("テストを必ず書く"));

        let err = show(&config, &FakeProbe::default(), "deploy", None).unwrap_err();
        assert!(err.hint.unwrap().contains("triage"));
    }

    #[test]
    fn all_comments_count_when_there_is_no_marker() {
        let comments = [comment("a", "1"), comment("b", "2")];
        assert_eq!(human_comments_since_last_marker(&comments).len(), 2);
        let ends_with_marker = [
            comment("a", "1"),
            comment("bot", "[agentcells] review round 1"),
        ];
        assert!(human_comments_since_last_marker(&ends_with_marker).is_empty());
    }
}
