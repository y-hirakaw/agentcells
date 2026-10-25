//! cellが終わったあとの遷移。AIが選んだ先を、設定で許された先に限って通す。
//!
//! ラベルを付け替えるのはCLIだけで、AIは「次はどこか」を答えるだけにする。
//! 許されていない先や往復の上限を超えた先が選ばれたら、人間に回す。

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::json;

use crate::config::{Cell, Config, Product, Target};
use crate::labels;
use crate::review::{self, Finding};

/// このcellの仕事で終わり。
pub const DONE: &str = "done";
/// 人間に回す。どのcellからでも選べる。
pub const NEEDS_HUMAN: &str = "needs-human";
/// ここで止める。Issue/PRは閉じずに、agentcellsのラベルを外す（そのあとは人間が進める）。
pub const STOP: &str = "stop";

/// cellの名前に使えない、遷移先として予約された名前か。
pub fn is_reserved(name: &str) -> bool {
    name == DONE || name == NEEDS_HUMAN || name == STOP
}

/// 人間を待っている状態。cellを使う人がコメントすると、また動く（[`crate::tick`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiting {
    /// 人間の答えや作業を待っている（`ac:needs-human`）。待っていたcellが、また動く
    NeedsHuman,
    /// AIのレビューが終わったPRで、人間の検品を待っている（`ac:in-review`）。直すcellが動く
    InReview,
}

impl Waiting {
    pub const ALL: [Waiting; 2] = [Waiting::NeedsHuman, Waiting::InReview];

    pub fn label(self) -> &'static str {
        match self {
            Waiting::NeedsHuman => labels::NEEDS_HUMAN,
            Waiting::InReview => labels::IN_REVIEW,
        }
    }

    /// 待っている状態を置けるもの。検品を待つのはPRだけ。
    pub fn applies_to(self, item: Target) -> bool {
        self == Waiting::NeedsHuman || item == Target::PullRequest
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    Cell(String),
    Done,
    Stop,
    NeedsHuman,
}

/// AIが選べる先。設定の `next` に `needs-human` を足したもの。
/// 検品を待つPRへのコメントで動いたときは、`done`（返事だけして、検品待ちに戻す）も選べる。
/// 着手を確かめるcell（[`Cell::confirms`]）は、自分で人間に確かめたあとの返事で動いたとき（`asked`）だけ、
/// 次のcellと `stop` を選べる。
pub fn choices(cell: &Cell, waiting: Option<Waiting>, asked: bool) -> Vec<String> {
    let reply_only = (waiting == Some(Waiting::InReview) && !cell.review).then_some(DONE);
    let approved = !cell.confirms() || (waiting.is_some() && asked);
    let mut choices: Vec<String> = Vec::new();
    for next in cell
        .next
        .iter()
        .map(String::as_str)
        .filter(|next| approved || *next == DONE)
        .chain(reply_only)
        .chain([NEEDS_HUMAN])
    {
        if !choices.iter().any(|c| c == next) {
            choices.push(next.to_string());
        }
    }
    choices
}

/// 1回の実行の結果としてAIが返すもの。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Outcome {
    pub next: String,
    pub comment: String,
    /// レビューのcell（`review = true`）が返す、コードへの指摘
    #[serde(default)]
    pub findings: Vec<Finding>,
}

impl Outcome {
    /// `claude -p --output-format json --json-schema` の結果から読む。
    ///
    /// スキーマに合わない値（許されていない先など）をAIが返そうとすると `structured_output` が null になる。
    /// そのときは人間に回せるよう、理由を返す。
    pub fn from_result(result: &serde_json::Value) -> std::result::Result<Self, String> {
        if result["is_error"].as_bool() == Some(true) {
            return Err(format!(
                "AIの実行が失敗しました（{}）",
                result["subtype"].as_str().unwrap_or("理由不明")
            ));
        }
        match &result["structured_output"] {
            serde_json::Value::Null => Err("AIが決められた形で結果を返しませんでした".to_string()),
            output => serde_json::from_value(output.clone())
                .map_err(|e| format!("AIの結果を読めません: {e}")),
        }
    }
}

/// `claude -p --json-schema` に渡すスキーマ。`next` は選べる先しか出せないようにする。
/// レビューのcellには、コードへの指摘（`findings`）も返させる。
pub fn output_schema(cell: &Cell, waiting: Option<Waiting>, asked: bool) -> serde_json::Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "next": {
                "type": "string",
                "enum": choices(cell, waiting, asked),
                "description": "次に進める先。迷ったら needs-human を選び、comment に人間への質問を書く"
            },
            "comment": {
                "type": "string",
                "description": "Issue/PRに残すコメント。やったことと、次に進める理由を書く。needs-human のときは、人間に確かめたいこと（仕様の質問など）や、できなかったこととその結果、人間にやってほしいこと（環境のせいでビルドが通らないなど）を書く"
            }
        },
        "required": ["next", "comment"],
        "additionalProperties": false
    });
    if cell.review {
        schema["properties"]["findings"] = review::findings_schema();
        schema["required"] = json!(["next", "comment", "findings"]);
    }
    schema
}

/// 終わったcellの実行。
pub struct Finished<'a> {
    pub cell: &'a Cell,
    /// cellが扱っていたもの
    pub item: Target,
    /// 実行の中でPRが作られたか
    pub created_pr: bool,
    /// AIが選んだ先
    pub requested: &'a str,
    /// 人間を待っていたところから、コメントで動いたなら、その状態
    pub waiting: Option<Waiting>,
    /// 最後の目印が、このcellが自分で人間に確かめたものか（[`choices`]）
    pub asked: bool,
}

/// 遷移のためにCLIがすること。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub destination: Destination,
    /// 今のIssue/PRから外すラベル
    pub remove: Vec<String>,
    /// 今のIssue/PRに付けるラベル
    pub add: Vec<String>,
    /// 実行の中で作られたPRに付けるラベル
    pub add_to_created_pr: Vec<String>,
    /// 今のIssueを閉じるか
    pub close: bool,
    /// AIが選んだ先を人間に回したときの理由。コメントに書く
    pub overridden: Option<String>,
}

/// AIが選んだ先を確かめて、ラベルの付け替えを決める。
///
/// 付け替えるときは、`add` を付けてから `remove`（`ac:working` を含む）を外す順にする。guardは、
/// `ac:working` が付いている間に付いたcellのラベルを、agentcellsの付け替えとして承認から除くため。
///
/// `runs` は、そのcellがこのIssue/PRで、cellを使う人の最後のコメントから何回動いたかを返す（往復の上限に使う）。
pub fn plan(
    config: &Config,
    product: &Product,
    finished: &Finished,
    runs: &dyn Fn(&str) -> u32,
) -> Plan {
    let mut plan = plan_labels(config, product, finished, runs);
    // 待っていたラベル（あれば）も外す。また同じ状態で待つなら、付けたまま
    if let Some(waiting) = finished.waiting {
        plan.remove.push(waiting.label().to_string());
    }
    let add = plan.add.clone();
    plan.remove
        .retain(|r| !add.iter().any(|a| a.eq_ignore_ascii_case(r)));
    plan
}

fn plan_labels(
    config: &Config,
    product: &Product,
    finished: &Finished,
    runs: &dyn Fn(&str) -> u32,
) -> Plan {
    let cell = finished.cell;
    // どの先に進んでも、このcellのラベルと処理中のラベルは外す。
    let remove = || vec![cell.label.clone(), labels::WORKING.to_string()];
    let to_human = |reason: Option<String>| Plan {
        destination: Destination::NeedsHuman,
        remove: remove(),
        add: vec![labels::NEEDS_HUMAN.to_string()],
        add_to_created_pr: Vec::new(),
        close: false,
        overridden: reason,
    };

    let requested = finished.requested;
    let choices = choices(cell, finished.waiting, finished.asked);
    if !choices.iter().any(|c| c == requested) {
        return to_human(Some(format!(
            "cell「{}」から「{requested}」には進めません（選べる先: {}）",
            cell.name,
            choices.join(", ")
        )));
    }

    match requested {
        NEEDS_HUMAN => to_human(None),
        STOP => Plan {
            destination: Destination::Stop,
            remove: remove(),
            add: Vec::new(),
            add_to_created_pr: Vec::new(),
            close: false,
            overridden: None,
        },
        DONE => match finished.item {
            Target::Issue => Plan {
                destination: Destination::Done,
                remove: remove(),
                add: Vec::new(),
                add_to_created_pr: Vec::new(),
                close: true,
                overridden: None,
            },
            // 自動でマージするかの判断（dispatcher）ができるまでは、人間に検品を頼む。
            Target::PullRequest => Plan {
                destination: Destination::Done,
                remove: remove(),
                add: vec![labels::IN_REVIEW.to_string()],
                add_to_created_pr: Vec::new(),
                close: false,
                overridden: None,
            },
        },
        name => {
            let next = match config.cell(name) {
                Some(next) if product.cells.iter().any(|c| c == name) => next,
                _ => {
                    return to_human(Some(format!(
                        "「{name}」はproduct「{}」で動いていません",
                        product.name
                    )));
                }
            };
            let done_runs = runs(name);
            if done_runs >= next.max_rounds {
                return to_human(Some(format!(
                    "「{name}」はすでに{done_runs}回動いていて、上限（{}回）に達しました",
                    next.max_rounds
                )));
            }
            let mut plan = Plan {
                destination: Destination::Cell(name.to_string()),
                remove: remove(),
                add: Vec::new(),
                add_to_created_pr: Vec::new(),
                close: false,
                overridden: None,
            };
            if next.on.contains(&finished.item) {
                plan.add.push(next.label.clone());
            } else if finished.item == Target::Issue && next.on.contains(&Target::PullRequest) {
                if !finished.created_pr {
                    return to_human(Some(format!(
                        "PRが作られていないので「{name}」に進めません"
                    )));
                }
                plan.add_to_created_pr.push(next.label.clone());
            } else {
                return to_human(Some(format!(
                    "「{name}」は{}を扱えません",
                    finished.item.as_str()
                )));
            }
            plan
        }
    }
}

/// 実行の前後で `ac:` のラベルが変わっていないかを確かめる。変わっていれば内容を返す。
///
/// ラベルを付け替えるのはCLIだけなので、実行中に変わったならAIが触った可能性がある。
pub fn tampered(before: &[String], after: &[String]) -> Option<String> {
    let ac = |labels: &[String]| -> BTreeSet<String> {
        labels
            .iter()
            .map(|l| l.to_lowercase())
            .filter(|l| l.starts_with(labels::PREFIX))
            .collect()
    };
    let (before, after) = (ac(before), ac(after));
    if before == after {
        return None;
    }
    let added: Vec<_> = after.difference(&before).cloned().collect();
    let removed: Vec<_> = before.difference(&after).cloned().collect();
    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("付いた: {}", added.join(", ")));
    }
    if !removed.is_empty() {
        parts.push(format!("外れた: {}", removed.join(", ")));
    }
    Some(format!(
        "実行中に ac: のラベルが変わりました（{}）",
        parts.join("、")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
operator = "me"

[[product]]
name = "app"
issues = "octo/app"
repos = ["octo/app"]
cells = ["triage", "implement", "review"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
prompt = "p.md"
next = ["implement", "done"]

[[cell]]
name = "implement"
on = ["issue", "pull_request"]
label = "ac:implement"
prompt = "p.md"
next = ["review"]

[[cell]]
name = "review"
on = "pull_request"
label = "ac:review"
prompt = "p.md"
next = ["implement", "done"]
max_rounds = 2
"#;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), "prompt").unwrap();
        let config = Config::parse(CONFIG, dir.path()).unwrap();
        (dir, config)
    }

    fn plan_for(
        config: &Config,
        cell: &str,
        item: Target,
        created_pr: bool,
        requested: &str,
        runs: u32,
    ) -> Plan {
        plan_waiting(config, cell, item, created_pr, requested, runs, None)
    }

    fn plan_waiting(
        config: &Config,
        cell: &str,
        item: Target,
        created_pr: bool,
        requested: &str,
        runs: u32,
        waiting: Option<Waiting>,
    ) -> Plan {
        let finished = Finished {
            cell: config.cell(cell).unwrap(),
            item,
            created_pr,
            requested,
            waiting,
            asked: false,
        };
        plan(config, &config.products[0], &finished, &|_| runs)
    }

    fn strings(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn choices_always_include_needs_human_once() {
        let (_dir, config) = config();
        assert_eq!(
            choices(config.cell("triage").unwrap(), None, false),
            strings(&["implement", "done", NEEDS_HUMAN])
        );
        let mut cell = config.cell("triage").unwrap().clone();
        cell.next.push(NEEDS_HUMAN.to_string());
        assert_eq!(
            choices(&cell, None, false),
            strings(&["implement", "done", NEEDS_HUMAN])
        );
    }

    #[test]
    fn confirming_cells_move_on_only_after_a_human_reply() {
        let (_dir, config) = config();
        let mut cell = config.cell("triage").unwrap().clone();
        cell.next = strings(&["implement", STOP, DONE]);
        cell.confirm = Some(true);
        // 最初に動いたときは、閉じるか人間に確かめるかだけ
        assert_eq!(choices(&cell, None, false), strings(&[DONE, NEEDS_HUMAN]));
        // 自分で確かめたあとの、人間の返事で動いたなら、次のcellと stop も選べる
        assert_eq!(
            choices(&cell, Some(Waiting::NeedsHuman), true),
            strings(&["implement", STOP, DONE, NEEDS_HUMAN])
        );
        // 失敗して人間に回したあとの返事（確かめていない）や、ラベルを付け直して動いたときは、進めない
        assert_eq!(
            choices(&cell, Some(Waiting::NeedsHuman), false),
            strings(&[DONE, NEEDS_HUMAN])
        );
        assert_eq!(choices(&cell, None, true), strings(&[DONE, NEEDS_HUMAN]));
        // 確かめないcellなら、いつでも選べる
        cell.confirm = Some(false);
        assert_eq!(
            choices(&cell, None, false),
            strings(&["implement", STOP, DONE, NEEDS_HUMAN])
        );
        // assess は、書かなければ確かめる
        cell.confirm = None;
        assert!(!cell.confirms());
        cell.name = "assess".to_string();
        assert!(cell.confirms());

        // 確かめるcellが、返事を待たずに次のcellを選んでも、人間に回す
        let mut config = config;
        config.cells[0].next = strings(&["implement", STOP, DONE]);
        config.cells[0].confirm = Some(true);
        let plan = plan_for(&config, "triage", Target::Issue, false, "implement", 0);
        assert_eq!(plan.destination, Destination::NeedsHuman);
        assert!(plan.overridden.unwrap().contains("進めません"));
    }

    #[test]
    fn stop_leaves_the_issue_open_without_agentcells_labels() {
        let (_dir, mut config) = config();
        config.cells[0].next = strings(&[STOP, DONE]);
        let plan = plan_waiting(
            &config,
            "triage",
            Target::Issue,
            false,
            STOP,
            0,
            Some(Waiting::NeedsHuman),
        );
        assert_eq!(plan.destination, Destination::Stop);
        assert!(!plan.close);
        assert!(plan.add.is_empty());
        assert_eq!(
            plan.remove,
            strings(&["ac:triage", labels::WORKING, labels::NEEDS_HUMAN])
        );
        // next に書いていなければ選べない
        config.cells[0].next = strings(&[DONE]);
        let plan = plan_for(&config, "triage", Target::Issue, false, STOP, 0);
        assert_eq!(plan.destination, Destination::NeedsHuman);
    }

    #[test]
    fn schema_limits_next_to_choices() {
        let (_dir, config) = config();
        let schema = output_schema(config.cell("review").unwrap(), None, false);
        assert_eq!(
            schema["properties"]["next"]["enum"],
            json!(["implement", "done", NEEDS_HUMAN])
        );
        assert_eq!(schema["required"], json!(["next", "comment"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert!(schema["properties"].get("findings").is_none());
    }

    #[test]
    fn review_cells_also_return_findings() {
        let (_dir, config) = config();
        let mut cell = config.cell("review").unwrap().clone();
        cell.review = true;
        let schema = output_schema(&cell, None, false);
        assert_eq!(schema["required"], json!(["next", "comment", "findings"]));
        assert_eq!(schema["properties"]["findings"]["type"], "array");

        let outcome = Outcome::from_result(&json!({
            "is_error": false,
            "structured_output": {
                "next": "implement",
                "comment": "指摘は1件です",
                "findings": [{"path": "src/main.rs", "line": 3, "body": "名前が分かりにくい"}]
            }
        }))
        .unwrap();
        assert_eq!(outcome.findings.len(), 1);
    }

    #[test]
    fn outcome_is_read_from_structured_output() {
        let result = json!({
            "subtype": "success",
            "is_error": false,
            "structured_output": {"next": "implement", "comment": "小さなバグ修正です"}
        });
        let outcome = Outcome::from_result(&result).unwrap();
        assert_eq!(outcome.next, "implement");
        assert_eq!(outcome.comment, "小さなバグ修正です");
    }

    #[test]
    fn missing_structured_output_is_an_error() {
        // 許されていない先を返そうとしたとき、Claude Codeは structured_output を null にする
        let result = json!({"subtype": "success", "is_error": false, "structured_output": null});
        assert!(
            Outcome::from_result(&result)
                .unwrap_err()
                .contains("決められた形")
        );
    }

    #[test]
    fn failed_run_is_an_error() {
        let result = json!({"subtype": "error_max_turns", "is_error": true});
        assert!(
            Outcome::from_result(&result)
                .unwrap_err()
                .contains("error_max_turns")
        );
    }

    #[test]
    fn malformed_structured_output_is_an_error() {
        let result = json!({"is_error": false, "structured_output": {"next": "done"}});
        assert!(
            Outcome::from_result(&result)
                .unwrap_err()
                .contains("読めません")
        );
    }

    #[test]
    fn triage_to_implement_stays_on_the_issue() {
        let (_dir, config) = config();
        let plan = plan_for(&config, "triage", Target::Issue, false, "implement", 0);
        assert_eq!(plan.destination, Destination::Cell("implement".into()));
        assert_eq!(plan.remove, strings(&["ac:triage", labels::WORKING]));
        assert_eq!(plan.add, strings(&["ac:implement"]));
        assert!(plan.add_to_created_pr.is_empty());
        assert!(plan.overridden.is_none());
    }

    #[test]
    fn implement_to_review_labels_the_created_pr() {
        let (_dir, config) = config();
        let plan = plan_for(&config, "implement", Target::Issue, true, "review", 0);
        assert_eq!(plan.destination, Destination::Cell("review".into()));
        assert!(plan.add.is_empty());
        assert_eq!(plan.add_to_created_pr, strings(&["ac:review"]));
    }

    #[test]
    fn implement_to_review_without_pr_goes_to_human() {
        let (_dir, config) = config();
        let plan = plan_for(&config, "implement", Target::Issue, false, "review", 0);
        assert_eq!(plan.destination, Destination::NeedsHuman);
        assert!(plan.overridden.unwrap().contains("PRが作られていない"));
    }

    #[test]
    fn review_back_to_implement_stays_on_the_pr() {
        let (_dir, config) = config();
        let plan = plan_for(
            &config,
            "review",
            Target::PullRequest,
            false,
            "implement",
            1,
        );
        assert_eq!(plan.add, strings(&["ac:implement"]));
    }

    #[test]
    fn exceeding_max_rounds_goes_to_human() {
        let (_dir, config) = config();
        // implement は max_rounds が既定の3
        let plan = plan_for(
            &config,
            "review",
            Target::PullRequest,
            false,
            "implement",
            3,
        );
        assert_eq!(plan.destination, Destination::NeedsHuman);
        assert_eq!(plan.add, strings(&[labels::NEEDS_HUMAN]));
        assert!(plan.overridden.unwrap().contains("上限（3回）"));
    }

    #[test]
    fn not_allowed_destination_goes_to_human_with_reason() {
        let (_dir, config) = config();
        let plan = plan_for(&config, "triage", Target::Issue, false, "review", 0);
        assert_eq!(plan.destination, Destination::NeedsHuman);
        assert_eq!(plan.remove, strings(&["ac:triage", labels::WORKING]));
        let reason = plan.overridden.unwrap();
        assert!(reason.contains("「review」には進めません"), "{reason}");
        assert!(reason.contains("implement, done, needs-human"), "{reason}");
    }

    #[test]
    fn needs_human_is_always_allowed_without_override() {
        let (_dir, config) = config();
        let plan = plan_for(&config, "implement", Target::Issue, false, NEEDS_HUMAN, 0);
        assert_eq!(plan.destination, Destination::NeedsHuman);
        assert!(plan.overridden.is_none());
    }

    #[test]
    fn done_closes_an_issue() {
        let (_dir, config) = config();
        let plan = plan_for(&config, "triage", Target::Issue, false, DONE, 0);
        assert_eq!(plan.destination, Destination::Done);
        assert!(plan.close);
        assert!(plan.add.is_empty());
    }

    #[test]
    fn done_on_a_pr_asks_human_to_inspect() {
        let (_dir, config) = config();
        let plan = plan_for(&config, "review", Target::PullRequest, false, DONE, 0);
        assert_eq!(plan.destination, Destination::Done);
        assert!(!plan.close);
        assert_eq!(plan.add, strings(&[labels::IN_REVIEW]));
    }

    #[test]
    fn a_reply_on_an_inspected_pr_may_only_answer() {
        let (_dir, config) = config();
        let implement = config.cell("implement").unwrap();
        assert_eq!(
            choices(implement, None, false),
            strings(&["review", NEEDS_HUMAN])
        );
        assert_eq!(
            choices(implement, Some(Waiting::InReview), false),
            strings(&["review", DONE, NEEDS_HUMAN])
        );
        // 返事だけなら、検品待ちに戻す（付けたまま）
        let plan = plan_waiting(
            &config,
            "implement",
            Target::PullRequest,
            false,
            DONE,
            0,
            Some(Waiting::InReview),
        );
        assert_eq!(plan.destination, Destination::Done);
        assert_eq!(plan.add, strings(&[labels::IN_REVIEW]));
        assert_eq!(plan.remove, strings(&["ac:implement", labels::WORKING]));
        // 直したら、レビューに回し、検品待ちのラベルは外す
        let plan = plan_waiting(
            &config,
            "implement",
            Target::PullRequest,
            false,
            "review",
            0,
            Some(Waiting::InReview),
        );
        assert_eq!(plan.add, strings(&["ac:review"]));
        assert_eq!(
            plan.remove,
            strings(&["ac:implement", labels::WORKING, labels::IN_REVIEW])
        );
    }

    #[test]
    fn a_reply_to_a_question_moves_on_or_waits_again() {
        let (_dir, config) = config();
        let plan = plan_waiting(
            &config,
            "triage",
            Target::Issue,
            false,
            "implement",
            0,
            Some(Waiting::NeedsHuman),
        );
        assert_eq!(plan.add, strings(&["ac:implement"]));
        assert_eq!(
            plan.remove,
            strings(&["ac:triage", labels::WORKING, labels::NEEDS_HUMAN])
        );
        // また質問するなら、needs-human は付けたまま
        let plan = plan_waiting(
            &config,
            "triage",
            Target::Issue,
            false,
            NEEDS_HUMAN,
            0,
            Some(Waiting::NeedsHuman),
        );
        assert_eq!(plan.add, strings(&[labels::NEEDS_HUMAN]));
        assert_eq!(plan.remove, strings(&["ac:triage", labels::WORKING]));
    }

    #[test]
    fn destination_outside_the_product_goes_to_human() {
        let (_dir, mut config) = config();
        config.products[0].cells.retain(|c| c != "implement");
        let plan = plan_for(&config, "triage", Target::Issue, false, "implement", 0);
        assert_eq!(plan.destination, Destination::NeedsHuman);
        assert!(plan.overridden.unwrap().contains("動いていません"));
    }

    #[test]
    fn issue_only_cell_cannot_take_a_pr() {
        let (_dir, mut config) = config();
        config
            .cells
            .iter_mut()
            .find(|c| c.name == "implement")
            .unwrap()
            .on = vec![Target::Issue];
        let plan = plan_for(
            &config,
            "review",
            Target::PullRequest,
            false,
            "implement",
            0,
        );
        assert_eq!(plan.destination, Destination::NeedsHuman);
        assert!(
            plan.overridden
                .unwrap()
                .contains("pull_requestを扱えません")
        );
    }

    #[test]
    fn tampered_detects_ac_label_changes_only() {
        let before = strings(&["ac:triage", "ac:working", "bug"]);
        assert!(
            tampered(
                &before,
                &strings(&["AC:Triage", "ac:working", "enhancement"])
            )
            .is_none()
        );

        let message = tampered(&before, &strings(&["ac:working", "ac:implement"])).unwrap();
        assert!(message.contains("付いた: ac:implement"), "{message}");
        assert!(message.contains("外れた: ac:triage"), "{message}");
    }
}
