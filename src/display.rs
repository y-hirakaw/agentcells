//! ターミナルに出す文字。

/// 制御文字（ESC、CR、BELなど）と、文字の向きを変える文字（U+202E など）を `�` に置き換える。
///
/// Issue/PRのタイトルやAIが書いた文は、ほかの人が決められる（公開のIssueや、それに操られたAI）。
/// ESCなどをそのまま出すと、ターミナルが解釈して、表示を書き換えたり、クリップボードに書いたり
/// （OSC 52）できてしまうため。向きを変える文字は、コマンドやタイトルの見た目を入れ替えられる。
pub fn printable(s: &str) -> String {
    let bidi = |c: char| matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    s.chars()
        .map(|c| {
            if c.is_control() || bidi(c) {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_control_characters() {
        assert_eq!(
            printable("a\u{1b}]52;c;aGk=\u{7}b\rc\td"),
            "a\u{fffd}]52;c;aGk=\u{fffd}b\u{fffd}c\u{fffd}d"
        );
        assert_eq!(printable("日本語のタイトル"), "日本語のタイトル");
        assert_eq!(printable("a\u{202e}b\u{2066}c"), "a\u{fffd}b\u{fffd}c");
    }
}
