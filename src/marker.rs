//! CLIが残すコメントの目印。
//!
//! 1行目を `[agentcells] <cell> round <n>` にする。往復の回数は、この目印のroundの最大値で数える。

pub const TAG: &str = "[agentcells]";

/// コメントの1行目に書く目印。
pub fn header(cell: &str, round: u32) -> String {
    format!("{TAG} {cell} round {round}")
}

/// コメントの1行目の目印を読む。目印がなければ `None`。`round1` のように空白がなくても読む。
pub fn parse(body: &str) -> Option<(String, u32)> {
    let first = body.lines().next()?.trim();
    let rest = first.strip_prefix(TAG)?.trim_start();
    let (cell, rest) = rest.split_once(char::is_whitespace)?;
    let round = rest.trim().strip_prefix("round")?.trim();
    let round: u32 = round.parse().ok()?;
    if cell.is_empty() || round == 0 {
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
            "[agentcells] review round 0",
            "agentcells review round 1",
        ] {
            assert_eq!(parse(body), None, "{body:?}");
        }
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
