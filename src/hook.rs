//! cellとして動くAIがBashを使う前に、Claude Codeが動かすhook（PreToolUse）。
//!
//! Bashで動かしてよいのは `agentcells cell ...` だけ。ほかのコマンドは、権限の設定でも断るが、
//! そのときAIに届くのは「許しが要ります」という文だけで、AIは書き方を変えて試し続ける
//! （2026-10-26に、implementが python・sed・perl での置換や `cd ... &&` を10回試した）。
//! そこで、hookが先に断り、代わりに使うものと、それでできなければ人間に回すことを伝える。
//!
//! hookはsandboxの外で動くので、入力（AIが書いたコマンド）を読んで答えを出すだけにし、何も実行しない。
//! `agentcells cell` で始まるコマンドは、hookでは決めずに権限の設定に任せる（断るのは権限の設定の役目で、
//! hookは案内を足すだけ）。

use serde_json::json;

use crate::actions::Context;

/// AIに許すBashのコマンドの頭。権限の設定の `Bash(agentcells cell:*)` と同じ。
const ALLOWED: &str = "agentcells cell";

/// PreToolUseの入力（JSON）を読み、断るときはClaude Codeに返す答え（JSON）を返す。
/// `None` なら何も返さず、権限の設定に任せる。読めない入力は断る。Bash でないツールには何もしない。
pub fn pre_tool_use(input: &str, context: Option<&Context>) -> Option<String> {
    let input = serde_json::from_str::<serde_json::Value>(input).ok();
    if input.as_ref().is_some_and(|v| v["tool_name"] != "Bash") {
        return None;
    }
    let background = input
        .as_ref()
        .is_some_and(|v| v["tool_input"]["run_in_background"] == true);
    let reason = match input
        .as_ref()
        .and_then(|v| v["tool_input"]["command"].as_str())
    {
        // バックグラウンドで動かしたものは、出力を読むことも止めることもできない（そのツールは渡していない）
        Some(command) if background => {
            // コマンドにも断る理由があれば、つなげて返す（直したあとに、もう一度断らないように）
            let hint = match reason_hint(command) {
                Some(hint) => format!("{BACKGROUND}{hint}"),
                None => BACKGROUND.to_string(),
            };
            reason_for(Some(&hint), context)
        }
        Some(command) => reason(command, context)?,
        None => reason_for(None, context),
    };
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
        .to_string(),
    )
}

const BACKGROUND: &str = "バックグラウンドでは動かせません。run_in_background を付けずに動かしてください（時間がかかるものは、timeout を長くしてください。上限は10分です）。";

/// 断る理由。`agentcells cell` を1つだけ動かすコマンドなら `None`。
fn reason(command: &str, context: Option<&Context>) -> Option<String> {
    reason_hint(command).map(|hint| reason_for(Some(hint), context))
}

/// 断るときの、コマンドに合わせた案内。`agentcells cell` を1つだけ動かすコマンドなら `None`。
fn reason_hint(command: &str) -> Option<&'static str> {
    let command = command.trim();
    let allowed = command
        .strip_prefix(ALLOWED)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
    if allowed && !compound(command) {
        return None;
    }
    let hint = if allowed {
        "1回に1つの `agentcells cell` だけを、そのまま実行してください。`;`、`&&`、`||`、`&`、改行でつなげたり、`>` でファイルに書き出したり、`$(...)` を使ったりしないでください（`| tail`、`| head`、`| grep` のように、読むだけのコマンドに渡すのはかまいません）。"
    } else {
        let first = command.split_whitespace().next().unwrap_or("");
        hint(first)
    };
    Some(hint)
}

fn reason_for(hint: Option<&str>, context: Option<&Context>) -> String {
    let commands = match context {
        Some(c) if !c.commands.is_empty() => {
            let names: Vec<&str> = c.commands.keys().map(String::as_str).collect();
            format!("（使える名前: {}）", names.join("、"))
        }
        Some(_) => "（使える名前はありません）".to_string(),
        None => String::new(),
    };
    let mut text = String::from(
        "このコマンドは使えません。Bashで動かせるのは `agentcells cell ...` だけです。",
    );
    if let Some(hint) = hint.filter(|h| !h.is_empty()) {
        text.push_str(hint);
    }
    text.push_str(&format!(
        "\n\n使えるもの:\n\
         - ファイルを読む、探す: Read、Grep、Glob\n\
         - ファイルを作る、書き換える: Edit、Write（worktreeの中だけ）\n\
         - 消す、動かす: `agentcells cell rm <パス>`、`agentcells cell mv <元> <先>`\n\
         - gitを読む: `agentcells cell git <status/diff/log/show/blame> ...`\n\
         - コミット: `agentcells cell commit -m <メッセージ>`\n\
         - ベースのブランチの取り込み: `agentcells cell merge`\n\
         - ビルドやテスト: `agentcells cell run <名前>`{commands}\n\n\
         これらでできないことが必要なら、別の書き方や別の道を試さずに作業を止めてください。\
         next を needs-human にして、何をしたかったか、何が要るかを comment に書いてください。"
    ));
    text
}

/// よくある使い方に合わせた案内。
fn hint(first: &str) -> &'static str {
    match first {
        "python" | "python3" | "sed" | "perl" | "awk" | "ruby" | "node" | "tee" => {
            "ファイルの書き換えは、スクリプトではなく Edit か Write でしてください。"
        }
        "cd" | "pushd" => {
            "作業するディレクトリは、はじめからworktreeです。cd は要りません。パスはworktreeからの相対で書いてください。"
        }
        "git" => {
            "gitは `agentcells cell git`（読む）、`agentcells cell commit`、`agentcells cell merge` を使ってください。"
        }
        "rm" | "mv" | "cp" | "mkdir" | "touch" => {
            "消す・動かすのは `agentcells cell rm` / `agentcells cell mv`、作るのは Write を使ってください。"
        }
        "ls" | "cat" | "head" | "tail" | "grep" | "rg" | "find" | "wc" => {
            "読む・探すのは、Read、Grep、Glob を使ってください。"
        }
        _ => "",
    }
}

/// パイプで `agentcells cell` の出力を渡してよいコマンド。読むだけのもの。
const READERS: &[&str] = &["tail", "head", "grep", "wc"];

/// 先頭の空白を飛ばした、最初の語（空白、`|`、`;`、`&`、`>`、`<` まで）。
fn word(rest: &[char]) -> String {
    rest.iter()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| !c.is_whitespace() && !matches!(c, '|' | ';' | '&' | '>' | '<'))
        .collect()
}

/// 1つのコマンドに、別の操作を足せる書き方があるか。引用符の外の `;`、`&&`、`||`、`&`、改行、
/// ファイルへのリダイレクト（`>`、`<`、`&>`。`2>&1` のような付け替えは除く）、`<(`、コメント（`#`）、
/// 読むだけでないコマンドへのパイプと、
/// 単引用符の外の `$(...)`、バッククォート。判定は、多めに断る向きにする（hookは案内で、止めるのは権限の設定）。
fn compound(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let (mut single, mut double, mut escaped) = (false, false, false);
    let mut word_start = true;
    let mut prev = '\0';
    for (i, &c) in chars.iter().enumerate() {
        let next = chars.get(i + 1).copied();
        if escaped {
            escaped = false;
        } else if single {
            single = c != '\'';
        } else if c == '\\' {
            escaped = true;
        } else if c == '`' || (c == '$' && next == Some('(')) {
            return true;
        } else if double {
            double = c != '"';
        } else {
            match c {
                '\'' => single = true,
                '"' => double = true,
                ';' | '\n' => return true,
                '#' if word_start => return true,
                '&' if !matches!(prev, '>' | '<') => return true,
                '|' if matches!(next, Some('|' | '&')) => return true,
                // パイプで渡してよいのは、読むだけのコマンドだけ
                '|' if !READERS.contains(&word(&chars[i + 1..]).as_str()) => return true,
                '>' | '<' => {
                    // `2>&1` や `>&-` のような付け替えだけを通す（`>&1x` はファイルへの書き出し）
                    let target = word(chars.get(i + 2..).unwrap_or_default());
                    let dup = next == Some('&')
                        && (target == "-"
                            || (!target.is_empty() && target.chars().all(|d| d.is_ascii_digit())));
                    if !dup {
                        return true;
                    }
                }
                _ => {}
            }
        }
        word_start = !single && !double && (c.is_whitespace() || c == '|');
        prev = c;
    }
    // 引用符が閉じていないものは、読み違えないように断る
    single || double || escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(command: &str) -> String {
        json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": command}})
            .to_string()
    }

    fn denied(command: &str) -> Option<String> {
        pre_tool_use(&input(command), None).map(|out| {
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
            v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .to_string()
        })
    }

    #[test]
    fn one_agentcells_cell_command_is_left_to_the_permissions() {
        for command in [
            "agentcells cell run test",
            "  agentcells cell commit -m \"直す; 2つ目 && 3つ目\"",
            "agentcells cell commit -m 'a || b; $(x) `y` > z # w'",
            "agentcells cell commit -m \"#1 を直す\"",
            "agentcells cell commit -m 直す\\;続き",
            "agentcells cell git diff",
            "agentcells cell run test 2>&1 | tail -50",
            "agentcells cell run test 2>&1|grep -n FAIL | head",
            "agentcells cell run test >&2",
        ] {
            assert_eq!(denied(command), None, "{command}");
        }
    }

    #[test]
    fn other_commands_are_denied_with_what_to_use_instead() {
        let reason = denied("python3 - <<'EOF'\nprint(1)\nEOF").unwrap();
        assert!(reason.contains("Edit か Write"), "{reason}");
        assert!(reason.contains("needs-human"), "{reason}");
        assert!(
            denied("cd /x && agentcells cell run test")
                .unwrap()
                .contains("cd は要りません")
        );
        assert!(
            denied("git commit -m x")
                .unwrap()
                .contains("agentcells cell commit")
        );
        assert!(denied("ls -la").unwrap().contains("Read、Grep、Glob"));
        // 環境変数を付けて呼ぶと、別の「今の仕事」のファイルを読ませられるので断る
        assert!(denied("AGENTCELLS_CONTEXT=x.json agentcells cell run test").is_some());
        assert!(denied("agentcellsx cell run test").is_some());
        assert!(denied("godot --path prototype").is_some());
    }

    #[test]
    fn chained_commands_are_denied_even_after_agentcells_cell() {
        for command in [
            "agentcells cell run test; python3 x.py",
            "agentcells cell run test && rm -rf src",
            "agentcells cell run test || true",
            "agentcells cell run test\nrm x",
            "agentcells cell commit -m \"閉じていない",
            "agentcells cell run test & python3 x.py",
            "agentcells cell run test &> out.txt",
            "agentcells cell run test > out.txt",
            "agentcells cell run test >&out.txt",
            "agentcells cell git diff <(cat x)",
            "agentcells cell commit -m \"$(cat x)\"",
            "agentcells cell commit -m `cat x`",
            "agentcells cell run x #'\nrm y #'",
            "agentcells cell run test |& tail",
            "agentcells cell run test >&1x",
            "agentcells cell git show HEAD:x | python3",
            "agentcells cell git show HEAD:x | (python3 x)",
            "agentcells cell git diff --name-only | xargs rm",
            "agentcells cell run test | tail -5 | sh",
        ] {
            let reason = denied(command).unwrap_or_else(|| panic!("{command}"));
            assert!(reason.contains("1回に1つ"), "{reason}");
        }
    }

    #[test]
    fn the_command_names_of_the_job_are_listed() {
        let mut context: Context = serde_json::from_value(json!({
            "worktree": "/w",
            "identity": {"name": "a", "email": "a@example.com"},
            "scanner": "/bin/betterleaks",
            "tmp": "/t",
        }))
        .unwrap();
        context.commands.insert("test".into(), "make test".into());
        context.commands.insert("lint".into(), "make lint".into());
        let out = pre_tool_use(&input("ls"), Some(&context)).unwrap();
        assert!(out.contains("使える名前: lint、test"), "{out}");
    }

    #[test]
    fn unreadable_input_is_denied() {
        assert!(pre_tool_use("not json", None).is_some());
        let other = json!({"tool_name": "Bash", "tool_input": {}}).to_string();
        assert!(pre_tool_use(&other, None).is_some());
        let background = json!({"tool_name": "Bash", "tool_input": {"command": "agentcells cell run test", "run_in_background": true}}).to_string();
        let reason = pre_tool_use(&background, None).unwrap();
        assert!(reason.contains("run_in_background を付けずに"), "{reason}");
        let both = json!({"tool_name": "Bash", "tool_input": {"command": "grep -r x .", "run_in_background": true}}).to_string();
        let reason = pre_tool_use(&both, None).unwrap();
        assert!(
            reason.contains("run_in_background") && reason.contains("Read、Grep、Glob"),
            "{reason}"
        );
        // Bashでないツールには何もしない
        let read = json!({"tool_name": "Read", "tool_input": {"file_path": "x"}}).to_string();
        assert_eq!(pre_tool_use(&read, None), None);
    }
}
