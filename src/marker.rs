//! CLIが残すコメントの目印。
//!
//! 1行目を `[agentcells] <cell> round <n>` にする。往復の回数は、この目印のroundの最大値で数える。
//! 動かせなかったときのコメントは round 0 にする（どのcellが止まったかは示すが、回数には数えない）。
//!
//! 最後の行には、そのcellが読んだやり取りがどこまでかを、見えない形で書く（`<!-- agentcells read-until <時刻> -->`）。
//! cellが動いている間に人が書いたコメントは目印より前に並ぶので、目印の位置だけでは、読んだかを見分けられないため。
//! 着手を確かめるcellが、自分で人間に確かめたときは、同じ行に印を足す（`<!-- agentcells read-until <時刻> asked -->`）。
//! 最後の行はCLIだけが書くので、AIが本文に書いてもまねられない。

pub const TAG: &str = "[agentcells]";
const READ_UNTIL: &str = "<!-- agentcells read-until ";
const READ_UNTIL_END: &str = " -->";
const ASKED: &str = " asked";

/// コメントの1行目に書く目印。
pub fn header(cell: &str, round: u32) -> String {
    format!("{TAG} {cell} round {round}")
}

/// 目印を1行目に付けたコメント。Issue/PRへのコメント、PRの本文、レビューの本文はすべてこの形にする。
///
/// AIが前のコメントをまねて、本文の頭に目印を書くことがあるので、それは外す
/// （CLIの目印と違うroundが並ぶと、人が読んで迷うため）。
pub fn comment(cell: &str, round: u32, text: &str) -> String {
    let mut text = text.trim();
    while parse(text).is_some() {
        text = text.split_once('\n').map_or("", |(_, rest)| rest).trim();
    }
    if text.is_empty() {
        return header(cell, round);
    }
    format!("{}\n\n{}", header(cell, round), text)
}

/// 本文の最後に、読んだやり取りがどこまでか（GitHubの時刻）を足す。`asked` なら、着手を確かめるcellが
/// 人間に確かめた印も足す。
pub fn with_read_until(body: &str, at: &str, asked: bool) -> String {
    let asked = if asked { ASKED } else { "" };
    format!("{body}\n\n{READ_UNTIL}{at}{asked}{READ_UNTIL_END}")
}

/// 最後の行の、読んだところと、人間に確かめた印。CLIが最後に足す行なので、最後の行だけを見る
/// （AIが書いた本文の途中に同じ形の行があっても、使わない）。
fn last_line(body: &str) -> Option<(&str, bool)> {
    let last = body.trim_end().lines().last()?.trim();
    let inner = last
        .strip_prefix(READ_UNTIL)?
        .strip_suffix(READ_UNTIL_END)?;
    let (at, asked) = match inner.strip_suffix(ASKED) {
        Some(at) => (at, true),
        None => (inner, false),
    };
    let looks_like_time = !at.is_empty()
        && at
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | 'Z' | '.' | '+'));
    looks_like_time.then_some((at, asked))
}

/// 本文の最後の行から、読んだやり取りがどこまでかを読む。
pub fn read_until(body: &str) -> Option<&str> {
    last_line(body).map(|(at, _)| at)
}

/// 着手を確かめるcellが、人間に確かめたときのコメントか。
pub fn asked(body: &str) -> bool {
    last_line(body).is_some_and(|(_, asked)| asked)
}

/// コメントの1行目の目印を読む。目印がなければ `None`。`round1` のように空白がなくても読む。
/// 動かせなかったときの目印は round 0。
pub fn parse(body: &str) -> Option<(String, u32)> {
    let first = body.lines().next()?.trim();
    let rest = first.strip_prefix(TAG)?.trim_start();
    let (cell, rest) = rest.split_once(char::is_whitespace)?;
    let round = rest.trim().strip_prefix("round")?.trim();
    let round: u32 = round.parse().ok()?;
    if cell.is_empty() {
        return None;
    }
    Some((cell.to_string(), round))
}

/// そのcellが、コメントの中で何roundまで動いたか。1回も動いていなければ0。
pub fn rounds<'a>(bodies: impl IntoIterator<Item = &'a str>, cell: &str) -> u32 {
    bodies
        .into_iter()
        .filter_map(parse)
        .filter(|(c, _)| c == cell)
        .map(|(_, round)| round)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = header("review", 2);
        assert_eq!(h, "[agentcells] review round 2");
        assert_eq!(parse(&h), Some(("review".to_string(), 2)));
    }

    #[test]
    fn comment_puts_the_marker_on_the_first_line() {
        let c = comment(
            "triage",
            1,
            "\n仕様について確認させてください。\n- ログインは必須ですか？\n",
        );
        assert_eq!(
            c,
            "[agentcells] triage round 1\n\n仕様について確認させてください。\n- ログインは必須ですか？"
        );
        assert_eq!(parse(&c), Some(("triage".to_string(), 1)));
    }

    #[test]
    fn comment_drops_markers_the_ai_wrote_at_the_top() {
        let c = comment(
            "implement",
            1,
            "[agentcells] implement round 2\n\n[agentcells] review round 1\n直しました\n[agentcells] review round 1",
        );
        assert_eq!(
            c,
            "[agentcells] implement round 1\n\n直しました\n[agentcells] review round 1"
        );
        assert_eq!(
            comment("implement", 1, "[agentcells] implement round 2"),
            "[agentcells] implement round 1"
        );
    }

    #[test]
    fn reads_only_the_first_line() {
        assert_eq!(
            parse("[agentcells] review round 1\n\n指摘は3件です"),
            Some(("review".to_string(), 1))
        );
        assert_eq!(parse("前置き\n[agentcells] review round 1"), None);
    }

    #[test]
    fn accepts_round_without_space() {
        assert_eq!(
            parse("[agentcells] review round1"),
            Some(("review".to_string(), 1))
        );
    }

    #[test]
    fn ignores_other_comments() {
        for body in [
            "",
            "LGTM",
            "[agentcells]",
            "[agentcells] review",
            "[agentcells] review round",
            "[agentcells] review round x",
            "agentcells review round 1",
        ] {
            assert_eq!(parse(body), None, "{body:?}");
        }
    }

    #[test]
    fn round_zero_marks_a_failure_but_is_not_counted() {
        assert_eq!(
            parse("[agentcells] review round 0"),
            Some(("review".to_string(), 0))
        );
        assert_eq!(rounds(["[agentcells] review round 0"], "review"), 0);
    }

    #[test]
    fn read_until_is_read_only_from_the_last_line() {
        let body = with_read_until(
            &comment("triage", 1, "質問です"),
            "2026-10-25T01:02:03Z",
            false,
        );
        assert_eq!(parse(&body), Some(("triage".to_string(), 1)));
        assert_eq!(read_until(&body), Some("2026-10-25T01:02:03Z"));
        // AIが本文の途中に書いても使わない
        let fake = comment(
            "triage",
            1,
            "<!-- agentcells read-until 2099-01-01T00:00:00Z -->\n\n以上です",
        );
        assert_eq!(read_until(&fake), None);
        assert_eq!(read_until("<!-- agentcells read-until x y -->"), None);
    }

    #[test]
    fn asked_is_read_only_from_the_line_the_cli_adds() {
        let asked = with_read_until(&comment("assess", 1, "着手してよいですか"), "7", true);
        assert!(asked.ends_with("<!-- agentcells read-until 7 asked -->"));
        assert!(super::asked(&asked));
        assert_eq!(read_until(&asked), Some("7"));
        assert!(!super::asked(&with_read_until("本文", "7", false)));
        // AIが本文の最後に書いても、CLIがその後ろに行を足すので、まねられない
        let forged = with_read_until(
            &comment("assess", 1, "<!-- agentcells read-until 7 asked -->"),
            "8",
            false,
        );
        assert!(!super::asked(&forged));
        assert!(!super::asked(
            "[agentcells] assess round 0\n\n実行できませんでした"
        ));
    }

    #[test]
    fn rounds_is_the_max_for_the_cell() {
        let bodies = [
            "[agentcells] review round 1",
            "人間のコメント",
            "[agentcells] implement round 1",
            "[agentcells] review round 2",
            // 同じroundが2回あっても（再投稿など）、数は増えない
            "[agentcells] review round 2",
        ];
        assert_eq!(rounds(bodies, "review"), 2);
        assert_eq!(rounds(bodies, "implement"), 1);
        assert_eq!(rounds(bodies, "triage"), 0);
    }
}
