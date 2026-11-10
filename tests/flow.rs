//! 流れを通しで試す（concept2.md 6の「1の作り」）。偽のGitHub（ghの解釈器）と偽のclaudeで、tickを回す。
//!
//! `harness = false` で、同じバイナリが `--fake-claude` のときは偽のclaudeとして動く（`flow/claude.rs`）。
//! 引数があれば、名前にその文字列を含む場面だけを動かす。

mod flow {
    pub mod claude;
    pub mod github;
    pub mod scene;
}

use std::process::ExitCode;

use flow::scene::{LABELS, OP, STRANGER, Scene, Setup, WRITER};
use serde_json::json;

/// 標準のcellで、Issueを triage → assess（着手を確かめる）→ implement と進める。
const CONFIG: &str = r#"
operator = "op"

[[product]]
name = "app"
issues = "octo/app"
repos = ["octo/app"]
cells = ["triage", "assess", "implement"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
next = ["implement", "assess", "done"]

[[cell]]
name = "assess"
on = "issue"
label = "ac:assess"
next = ["implement", "stop", "done"]

[[cell]]
name = "implement"
on = "issue"
label = "ac:implement"
next = ["done"]
"#;

fn spec(goal: &str) -> serde_json::Value {
    json!({
        "goal": goal,
        "criteria": [
            { "text": "READMEの最初に、何をするツールかが書いてある", "check": "README.mdの1段落目を読む" },
        ],
        "out_of_scope": ["ほかの言語のREADME"],
    })
}

/// 人が着手を認めるまで、assessがIssueを止めておくところまで。
fn asked(s: &Scene) -> u64 {
    let i = s.issue(OP, "READMEをわかりやすくしたい", "初めて見た人に伝わらない");
    s.as_user(OP).assign(i, OP).label(i, "ac:triage");
    s.claude("triage").returns(json!({
        "next": "assess",
        "comment": "どこまで直すかを選んでもらいたいので、assessに回します",
    }));
    s.claude("assess").returns(json!({
        "next": "needs-human",
        "comment": "判断: 直すべきです。方針: 冒頭に1段落を足します。着手してよいですか",
        "spec": spec("初めて見た人が、何をするツールかわかる"),
    }));
    let ended = s.tick();
    assert_eq!((ended.why, ended.runs), ("no_work", 2));
    i
}

/// 認められたあと、assessがimplementに回し、implementは変えずに人に戻す。
fn approved(s: &Scene, i: u64) {
    s.as_user(OP).comment(i, "その方針で進めてください");
    s.claude("assess").returns(json!({
        "next": "implement",
        "comment": "冒頭に1段落を足す方針で、implementに回します",
    }));
    s.claude("implement").returns(json!({
        "next": "needs-human",
        "comment": "試しなので、変えずに人に戻します",
    }));
    let ended = s.tick();
    assert_eq!((ended.why, ended.runs), ("no_work", 2));
}

/// #14 の流れ。抽象的なIssueは、triageからassessに回り、仕様を添えて人に確かめ、
/// 認められてからimplementに進む。implementには、人が確かめた仕様が渡る。
fn abstract_issue_is_confirmed_before_implementing() {
    let s = Scene::new(CONFIG);
    let i = asked(&s);
    assert!(
        s.prompt_of("triage", 1)
            .choices()
            .contains(&"assess".to_string())
    );
    // 認められるまでは、次のcellに進めない
    assert!(
        !s.prompt_of("assess", 1)
            .choices()
            .contains(&"implement".to_string())
    );
    let e = s.expect(i);
    e.labels(&["ac:needs-human"])
        .open(true)
        .comments_by(WRITER, 2);
    let asked = &e.last_comment().body;
    assert!(agentcells::marker::asked(asked), "{asked}");
    assert!(asked.contains("<!-- agentcells spec -->"), "{asked}");
    assert!(
        asked.contains("初めて見た人が、何をするツールかわかる"),
        "{asked}"
    );

    approved(&s, i);
    // 返事のあとのassessには、doneを選ぶと閉じることが書いてある（0.38.2で直したこと）
    let answered = s.prompt_of("assess", 2);
    assert!(answered.choices().contains(&"implement".to_string()));
    assert!(answered.system.contains("着手が認められたときは選ばない"));
    // implementには、人が確かめた仕様が渡る
    let implement = s.prompt_of("implement", 1).user;
    assert!(implement.contains("仕様（assess が"), "{implement}");
    assert!(
        implement.contains("に確かめ、返事があったもの"),
        "{implement}"
    );
    assert!(implement.contains("READMEの最初に、何をするツールかが書いてある"));
    s.expect(i).labels(&["ac:needs-human"]).open(true);
}

/// 認められたあとに、assessが仕様を変えて先へ進もうとしたら、CLIがもう一度人に確かめる。
/// 次の返事で進めば、implementには変えた仕様が渡る。
fn changed_spec_is_confirmed_again() {
    let s = Scene::new(CONFIG);
    let i = asked(&s);
    s.as_user(OP).comment(i, "OKです");
    s.claude("assess").returns(json!({
        "next": "implement",
        "comment": "範囲を広げて進めます",
        "spec": spec("初めて見た人が、何をするツールで、どう入れるかもわかる"),
    }));
    let ended = s.tick();
    assert_eq!((ended.why, ended.runs), ("no_work", 1));
    assert_eq!(s.runs_of("implement"), 0);
    let e = s.expect(i);
    e.labels(&["ac:needs-human"]).comments_by(WRITER, 3);
    let body = &e.last_comment().body;
    // 人に確かめたものとして書く（返事で先へ進めるように）
    assert!(agentcells::marker::asked(body), "{body}");
    assert!(body.contains("どう入れるかもわかる"), "{body}");

    approved(&s, i);
    let implement = s.prompt_of("implement", 1).user;
    assert!(implement.contains("どう入れるかもわかる"), "{implement}");
    assert!(
        implement.contains("に確かめ、返事があったもの"),
        "{implement}"
    );
}

/// ほかの人がラベルを付け直したものは、動かさない（嘘の緑を防ぐ、通らないはずの場面）。
fn a_label_put_back_by_someone_else_is_skipped() {
    let s = Scene::new(CONFIG);
    let i = s.issue(OP, "READMEをわかりやすくしたい", "初めて見た人に伝わらない");
    s.as_user(OP).assign(i, OP).label(i, "ac:triage");
    s.as_user(STRANGER)
        .unlabel(i, "ac:triage")
        .label(i, "ac:triage");
    let ended = s.tick();
    assert_eq!((ended.why, ended.runs), ("no_work", 0));
    assert_eq!(s.runs_of("triage"), 0);
    s.expect(i).labels(&["ac:triage"]).comments_by(WRITER, 0);
    let skipped = s.skipped();
    assert!(
        skipped
            .iter()
            .any(|r| r.contains("ac:triage") && r.contains(STRANGER)),
        "{skipped:?}"
    );
}

/// operatorを割り当てたのがほかの人なら、operatorがラベルを付けていても見送る。
/// eventsの `actor` は割り当てられた人（operator）になるので、`assigner` を見ていなければ動いてしまう。
fn an_assignment_by_someone_else_is_skipped() {
    let s = Scene::new(CONFIG);
    let i = s.issue(OP, "READMEをわかりやすくしたい", "初めて見た人に伝わらない");
    s.as_user(STRANGER).assign(i, OP);
    s.as_user(OP).label(i, "ac:triage");
    let ended = s.tick();
    assert_eq!((ended.why, ended.runs), ("no_work", 0));
    assert_eq!(s.runs_of("triage"), 0);
    s.expect(i).labels(&["ac:triage"]).comments_by(WRITER, 0);
    let skipped = s.skipped();
    assert!(
        skipped
            .iter()
            .any(|r| r.contains("割り当てたのが") && r.contains(STRANGER)),
        "{skipped:?}"
    );
}

/// リポジトリのラベルが大文字を交ぜて作られていても、設定の書き方（小文字）で付け外しでき、
/// リポジトリの書き方のまま残る。
fn labels_keep_the_spelling_of_the_repository() {
    let labels = LABELS.iter().map(|l| l.to_uppercase()).collect::<Vec<_>>();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let s = Scene::with(
        CONFIG,
        Setup {
            labels: &labels,
            ..Setup::default()
        },
    );
    let i = asked(&s);
    s.expect(i).labels(&["AC:NEEDS-HUMAN"]);
}

/// 書き込みのアカウントがoperatorと同じ（ghのアカウントで書く）で、tickの書き込みがすべて同じ秒でも、
/// #14 の流れが止まらない（cellの目印と、operatorのコメントを見分けられる）。
/// cellが進めたラベルを人の承認と取り違えないかは、ここでは確かめられない（ほかの人の書き換えがないので、
/// 取り違えても結果が変わらない。concept2.md 6の残り）。
///
/// 人の操作は、tickより前の秒にする。operatorがラベルを付けたのと同じ秒に、tickが `ac:working` を付けると、
/// そのラベルもcellが進めたものとみなされ、付け直すまで見送られる（docs/design.md のガード）。
fn the_operator_writing_in_one_second() {
    let s = Scene::with(
        CONFIG,
        Setup {
            writer: OP,
            ..Setup::default()
        },
    );
    let i = s.issue(OP, "READMEをわかりやすくしたい", "初めて見た人に伝わらない");
    s.as_user(OP).assign(i, OP).label(i, "ac:triage");
    s.claude("triage").returns(json!({
        "next": "assess",
        "comment": "どこまで直すかを選んでもらいたいので、assessに回します",
    }));
    s.claude("assess").returns(json!({
        "next": "needs-human",
        "comment": "判断: 直すべきです。方針: 冒頭に1段落を足します。着手してよいですか",
        "spec": spec("初めて見た人が、何をするツールかわかる"),
    }));
    s.freeze_clock(true);
    let ended = s.tick();
    s.freeze_clock(false);
    assert_eq!((ended.why, ended.runs), ("no_work", 2));
    s.expect(i).labels(&["ac:needs-human"]).comments_by(OP, 2);

    s.as_user(OP).comment(i, "その方針で進めてください");
    s.claude("assess").returns(json!({
        "next": "implement",
        "comment": "冒頭に1段落を足す方針で、implementに回します",
    }));
    s.claude("implement").returns(json!({
        "next": "needs-human",
        "comment": "試しなので、変えずに人に戻します",
    }));
    s.freeze_clock(true);
    let ended = s.tick();
    s.freeze_clock(false);
    assert_eq!((ended.why, ended.runs), ("no_work", 2));
    let implement = s.prompt_of("implement", 1).user;
    assert!(implement.contains("READMEの最初に、何をするツールかが書いてある"));
    s.expect(i).labels(&["ac:needs-human"]);
}

const SCENES: &[(&str, fn())] = &[
    (
        "abstract_issue_is_confirmed_before_implementing",
        abstract_issue_is_confirmed_before_implementing,
    ),
    (
        "changed_spec_is_confirmed_again",
        changed_spec_is_confirmed_again,
    ),
    (
        "a_label_put_back_by_someone_else_is_skipped",
        a_label_put_back_by_someone_else_is_skipped,
    ),
    (
        "an_assignment_by_someone_else_is_skipped",
        an_assignment_by_someone_else_is_skipped,
    ),
    (
        "labels_keep_the_spelling_of_the_repository",
        labels_keep_the_spelling_of_the_repository,
    ),
    (
        "the_operator_writing_in_one_second",
        the_operator_writing_in_one_second,
    ),
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--fake-claude") {
        return flow::claude::main(&args[2..]);
    }
    if args.iter().any(|a| a == "--list") {
        for (name, _) in SCENES {
            println!("flow::{name}: test");
        }
        return ExitCode::SUCCESS;
    }
    // cargo test の引数（--quiet など。値をとるものは、その値も）は飛ばし、残りを名前の絞り込みにする
    const WITH_VALUE: &[&str] = &["--test-threads", "--skip", "--color", "--format", "-Z"];
    let mut filter: Vec<&str> = Vec::new();
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        if WITH_VALUE.contains(&arg.as_str()) {
            rest.next();
        } else if !arg.starts_with('-') {
            // --list で出した名前（flow:: 付き）でも絞り込めるように
            filter.push(arg.strip_prefix("flow::").unwrap_or(arg));
        }
    }
    let exact = args.iter().any(|a| a == "--exact");
    let scenes: Vec<_> = SCENES
        .iter()
        .filter(|(name, _)| {
            filter.is_empty()
                || filter
                    .iter()
                    .any(|f| if exact { name == f } else { name.contains(f) })
        })
        .collect();
    println!("\nrunning {} flow scenes", scenes.len());
    let mut failed = Vec::new();
    for (name, scene) in &scenes {
        // tickは標準出力に書くので、場面の名前で区切る
        println!("\n=== flow::{name}");
        match std::panic::catch_unwind(scene) {
            Ok(()) => println!("test flow::{name} ... ok"),
            Err(_) => {
                println!("test flow::{name} ... FAILED");
                failed.push(*name);
            }
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed",
        if failed.is_empty() { "ok" } else { "FAILED" },
        scenes.len() - failed.len(),
        failed.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        for name in failed {
            println!("    flow::{name}");
        }
        ExitCode::FAILURE
    }
}
