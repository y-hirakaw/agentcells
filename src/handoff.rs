//! 手離れの物差し。1件（Issueと、そこから作ったPR）ごとに、人に回した回数、人の返事で動いた回数、
//! 検品から差し戻した回数、人を待った時間を数え、`status` と `dashboard` に出す。
//!
//! cellの設定やプロンプトを変えたときに、人の手が減ったかを確かめるため（concept2.md）。tickの記録
//! （[`crate::recent`]）だけを読み、GitHubは読まない。なので、人がPRに入れたコミット、マージ、戻し、
//! 検品にかかった時間は数えられない（出すときに、そう書く）。

use std::collections::HashMap;

use crate::labels;
use crate::recent::{Outcome, Run};
use crate::status::rough_elapsed;
use crate::transition::{DONE, NEEDS_HUMAN, STOP};

/// 何日前までに終わったものを数えるか。
pub const DAYS: u64 = 30;

/// 数えていないもの。数字を読み違えないように、いつも添える。
const NOT_COUNTED: &str =
    "数えていないもの: 人がPRに入れたコミット、マージ、戻し、検品にかかった時間";

/// 人に回した理由の分け方（出す順）。
const REASONS: [&str; 5] = [
    "AIが選んだ（質問、着手の確認）",
    "CLIが回した",
    "時間切れ",
    "AIの実行の失敗",
    "エラー",
];

/// 終わり方の分け方（出す順）。
const ENDS: [&str; 3] = ["検品まで", "閉じた", "止めた"];

/// Issue/PRが、前の実行でどうなったか（次に動いたきっかけを知るため）。
#[derive(Debug, Clone, Copy)]
enum Last {
    /// 人に回した（その時刻）
    Handed(u64),
    /// `done` か `stop` に進めた
    Done,
}

/// 次に動いたきっかけ。
enum Trigger {
    /// 人に回したものか、閉じたIssueへの、人の返事か付け直し（人に回したなら、その時刻）
    Human(Option<u64>),
    /// 検品を待つPRへの、人の返事か付け直し
    Review,
}

/// 1件分。
#[derive(Debug, Default)]
struct Case {
    /// 最後の実行が終わった時刻
    last_at: u64,
    /// 最後の実行で `done` か `stop` に進めたなら、終わり方（[`ENDS`] の番号）
    end: Option<usize>,
    runs: usize,
    handed: usize,
    replies: usize,
    sent_back: usize,
    waits: Vec<u64>,
    reasons: [usize; REASONS.len()],
    /// Issue/PRごとの、前の実行
    last: HashMap<(String, u64), Last>,
}

/// 直近 [`DAYS`] 日に最後の実行があった件のまとめ。数は、終わった件だけで数える。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Summary {
    /// 終わった件と、まだ途中の件
    pub finished: usize,
    pub ongoing: usize,
    /// 終わり方ごとの件数（[`ENDS`] の順）
    pub ends: [usize; ENDS.len()],
    /// 途中で人を呼ばずに終わった件（人に回さず、人の返事も差し戻しもない。PRは検品の手前まで）
    pub hands_off: usize,
    /// AIの実行（見送り、使用量の上限で戻したものは除く）
    pub runs: usize,
    /// 人に回した回数（エラーで止まって人に回したものを含む）
    pub handed: usize,
    /// 人の返事か、人のラベルの付け直しで動いた回数（検品から差し戻したものは除く）
    pub replies: usize,
    /// 検品を待つPRが、人の返事か付け直しで動き、直して検品待ちのほかへ進んだ回数
    pub sent_back: usize,
    /// 人に回してから、次に動き始めるまでの秒
    pub waits: Vec<u64>,
    /// 人に回した理由ごとの回数（[`REASONS`] の順）
    pub reasons: [usize; REASONS.len()],
}

/// 実行（古い順）をまとめる。`now` は今の時刻（UNIXの秒）。
pub fn summarize(runs: &[Run], now: u64) -> Summary {
    let mut cases: HashMap<String, Case> = HashMap::new();
    // IssueからPRに進んだなら、PRの実行はIssueの件に数える。product、PRのリポジトリ、PRの番号から、
    // Issueの件へつなぐ（PRのリポジトリがない0.37.0より前の記録は、そのころPRを作っていたIssueのリポジトリ）
    let mut links: HashMap<String, String> = HashMap::new();
    let key = |product: &str, kind: &str, repo: &str, number: u64| {
        format!("{product}\0{kind}\0{}#{number}", repo.to_lowercase())
    };
    for run in runs {
        if matches!(run.outcome, Outcome::Skipped { .. } | Outcome::Paused) {
            continue;
        }
        let s = &run.subject;
        let own = key(&s.product, &s.kind, &s.repo, s.number);
        let case_key = if s.kind == "pull_request" {
            links.get(&own).cloned().unwrap_or(own)
        } else {
            if let Some(pr) = run.pr {
                let repo = run.pr_repo.as_deref().unwrap_or(&s.repo);
                links.insert(key(&s.product, "pull_request", repo, pr), own.clone());
            }
            own
        };
        let case = cases.entry(case_key).or_default();
        case.runs += 1;
        case.last_at = run.at;

        let item = (s.kind.clone(), s.number);
        let last = case.last.remove(&item);
        let is_pr = s.kind == "pull_request";
        // ぶつかりを直しに動いたものは、人のきっかけではない。人に回したものと、終わったものが次に動くのは、
        // 人の返事か、人がラベルを付け直したときだけ（付け直しには、待っていたラベルが残らない）
        let trigger = if s.conflict {
            None
        } else {
            match (s.reply.as_deref(), last) {
                (Some(labels::IN_REVIEW), _) => Some(Trigger::Review),
                (_, Some(Last::Handed(at))) => Some(Trigger::Human(Some(at))),
                (Some(_), _) => Some(Trigger::Human(None)),
                (None, Some(Last::Done)) if is_pr => Some(Trigger::Review),
                (None, Some(Last::Done)) => Some(Trigger::Human(None)),
                (None, None) => None,
            }
        };
        let moved_on = matches!(&run.outcome, Outcome::Moved { destination } if destination != DONE && destination != STOP);
        match trigger {
            // 検品から、直して先へ進めたものだけを差し戻しとし、返事だけをして検品待ちに戻したものは返事に数える
            Some(Trigger::Review) if moved_on => case.sent_back += 1,
            Some(Trigger::Review) => case.replies += 1,
            Some(Trigger::Human(handed_at)) => {
                case.replies += 1;
                if let Some(at) = handed_at {
                    case.waits.push(run.started.saturating_sub(at));
                }
            }
            None => {}
        }

        let reason = match &run.outcome {
            Outcome::Human {
                requested,
                reason,
                failure,
            } => Some(match (failure.as_deref(), reason, requested.as_deref()) {
                (Some("timeout"), _, _) => 2,
                (Some(_), _, _) => 3,
                (None, None, Some(NEEDS_HUMAN)) => 0,
                _ => 1,
            }),
            Outcome::Failed { .. } => Some(4),
            _ => None,
        };
        case.end = match &run.outcome {
            Outcome::Moved { destination } if destination == STOP => Some(2),
            Outcome::Moved { destination } if destination == DONE => {
                Some(if is_pr { 0 } else { 1 })
            }
            _ => None,
        };
        if let Some(reason) = reason {
            case.handed += 1;
            case.reasons[reason] += 1;
            case.last.insert(item, Last::Handed(run.at));
        } else if case.end.is_some() {
            case.last.insert(item, Last::Done);
        }
    }

    let since = now.saturating_sub(DAYS * 24 * 60 * 60);
    let mut summary = Summary::default();
    for case in cases.into_values().filter(|c| c.last_at >= since) {
        let Some(end) = case.end else {
            summary.ongoing += 1;
            continue;
        };
        summary.finished += 1;
        summary.ends[end] += 1;
        if case.handed + case.replies + case.sent_back == 0 {
            summary.hands_off += 1;
        }
        summary.runs += case.runs;
        summary.handed += case.handed;
        summary.replies += case.replies;
        summary.sent_back += case.sent_back;
        summary.waits.extend(case.waits);
        for (total, n) in summary.reasons.iter_mut().zip(case.reasons) {
            *total += n;
        }
    }
    summary.waits.sort_unstable();
    summary
}

/// 名前と回数を、0のものを除いて並べる（`検品まで 3、閉じた 1`）。
fn counts(names: &[&str], counts: &[usize]) -> String {
    names
        .iter()
        .zip(counts)
        .filter(|(_, count)| **count > 0)
        .map(|(name, count)| format!("{name} {count}"))
        .collect::<Vec<_>>()
        .join("、")
}

impl Summary {
    /// `status` に出す行。
    pub fn describe(&self) -> Vec<String> {
        let ongoing = if self.ongoing > 0 {
            format!("。ほかに途中のもの{}件", self.ongoing)
        } else {
            String::new()
        };
        if self.finished == 0 {
            return vec![format!(
                "手離れ（直近{DAYS}日）: 終わったものはまだありません{ongoing}"
            )];
        }
        let n = self.finished as f64;
        let mut out = vec![
            format!(
                "手離れ（直近{DAYS}日に終わった{}件{ongoing}）",
                self.finished
            ),
            format!("  終わり方: {}", counts(&ENDS, &self.ends)),
            format!(
                "  途中で人を呼ばずに終わった: {}件（{}%。PRは検品の手前まで）",
                self.hands_off,
                self.hands_off_percent()
            ),
            format!(
                "  1件あたり: AIの実行 {:.1}回、人に回した {:.1}回、人の返事で動いた {:.1}回、検品から差し戻した {:.1}回",
                self.runs as f64 / n,
                self.handed as f64 / n,
                self.replies as f64 / n,
                self.sent_back as f64 / n
            ),
        ];
        if self.handed > 0 {
            out.push(format!(
                "  人に回した理由: {}",
                counts(&REASONS, &self.reasons)
            ));
        }
        if let (Some(median), Some(longest)) = (self.median_wait(), self.waits.last()) {
            out.push(format!(
                "  人を待った時間: 中央値 {}、最長 {}",
                rough_elapsed(median),
                rough_elapsed(*longest)
            ));
        }
        out.push(format!("  ※{NOT_COUNTED}"));
        out
    }

    /// `dashboard` に出す1行。終わった件がなければ `None`。
    pub fn brief(&self) -> Option<String> {
        if self.finished == 0 {
            return None;
        }
        let mut text = format!(
            "手離れ（直近{DAYS}日に終わった{}件）: 途中で人を呼ばずに {}%、1件あたり人に回した {:.1}回",
            self.finished,
            self.hands_off_percent(),
            self.handed as f64 / self.finished as f64
        );
        if let Some(median) = self.median_wait() {
            text.push_str(&format!("、待った時間の中央値 {}", rough_elapsed(median)));
        }
        Some(text)
    }

    /// 途中で人を呼ばずに終わった割合（四捨五入）。
    fn hands_off_percent(&self) -> usize {
        (self.hands_off * 200 + self.finished) / (self.finished * 2)
    }

    fn median_wait(&self) -> Option<u64> {
        self.waits
            .get(self.waits.len().checked_sub(1)? / 2)
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Subject;

    const DAY: u64 = 24 * 60 * 60;
    const NOW: u64 = 100 * DAY;

    fn run(at: u64, kind: &str, number: u64, outcome: Outcome) -> Run {
        Run {
            at,
            subject: Subject {
                product: "app".into(),
                cell: "c".into(),
                kind: kind.into(),
                repo: "Octo/App".into(),
                number,
                reply: None,
                conflict: false,
            },
            started: at - 60,
            seconds: 60,
            round: 1,
            pr: None,
            pr_repo: None,
            outcome,
        }
    }

    fn moved(to: &str) -> Outcome {
        Outcome::Moved {
            destination: to.into(),
        }
    }

    fn asked() -> Outcome {
        Outcome::Human {
            requested: Some(NEEDS_HUMAN.into()),
            reason: None,
            failure: None,
        }
    }

    fn reply(mut run: Run, label: &str) -> Run {
        run.subject.reply = Some(label.into());
        run
    }

    fn with_pr(mut run: Run, pr: u64) -> Run {
        run.pr = Some(pr);
        run
    }

    #[test]
    fn counts_an_issue_and_its_pr_as_one_case() {
        let runs = [
            run(NOW - 4000, "issue", 7, moved("implement")),
            with_pr(run(NOW - 3000, "issue", 7, moved("review")), 12),
            run(NOW - 2000, "pull_request", 12, moved(DONE)),
        ];
        let summary = summarize(&runs, NOW);
        assert_eq!(summary.finished, 1);
        assert_eq!(summary.ongoing, 0);
        assert_eq!(summary.hands_off, 1);
        assert_eq!(summary.runs, 3);
        assert_eq!(
            summary.describe()[..4],
            [
                "手離れ（直近30日に終わった1件）",
                "  終わり方: 検品まで 1",
                "  途中で人を呼ばずに終わった: 1件（100%。PRは検品の手前まで）",
                "  1件あたり: AIの実行 3.0回、人に回した 0.0回、人の返事で動いた 0.0回、検品から差し戻した 0.0回",
            ]
        );
        assert_eq!(
            summary.describe().last().unwrap(),
            &format!("  ※{NOT_COUNTED}")
        );
        assert_eq!(
            summary.brief().unwrap(),
            "手離れ（直近30日に終わった1件）: 途中で人を呼ばずに 100%、1件あたり人に回した 0.0回"
        );
    }

    #[test]
    fn does_not_join_prs_of_another_repository_or_unlinked_ones() {
        let mut other = run(NOW - 1000, "pull_request", 12, moved(DONE));
        other.subject.repo = "octo/other".into();
        let runs = [
            // Issueは人に回ったまま
            with_pr(run(NOW - 3000, "issue", 7, asked()), 12),
            // 同じproductの、別のリポジトリの同じ番号のPRは、別の件
            other,
            // Issueからつながらない（人が作った）PRは、PRだけで1件
            run(NOW - 500, "pull_request", 20, moved(DONE)),
        ];
        let summary = summarize(&runs, NOW);
        assert_eq!(summary.finished, 2);
        assert_eq!(summary.ongoing, 1);
        assert_eq!(summary.hands_off, 2);
        // リポジトリの大文字・小文字は区別しない
        let mut same = run(NOW - 1000, "pull_request", 12, moved(DONE));
        same.subject.repo = "octo/app".into();
        let summary = summarize(&[runs[0].clone(), same], NOW);
        assert_eq!((summary.finished, summary.ongoing), (1, 0));
        // PRのリポジトリが記録にあれば、それでつなぐ（Issueと作業するリポジトリが別のとき）
        let mut issue = with_pr(run(NOW - 3000, "issue", 7, moved("review")), 12);
        issue.subject.repo = "octo/issues".into();
        issue.pr_repo = Some("octo/app".into());
        let summary = summarize(&[issue.clone(), runs[2].clone()], NOW);
        assert_eq!((summary.finished, summary.ongoing), (1, 1));
        let summary = summarize(
            &[issue, run(NOW - 1000, "pull_request", 12, moved(DONE))],
            NOW,
        );
        assert_eq!((summary.finished, summary.ongoing), (1, 0));
    }

    #[test]
    fn counts_replies_relabels_send_backs_and_waits() {
        // ぶつかりを直しに動いたものは、人のきっかけに数えない
        let mut conflict = reply(
            run(NOW - 1800, "pull_request", 12, moved("review")),
            labels::IN_REVIEW,
        );
        conflict.subject.conflict = true;
        let runs = [
            // 質問して、2時間後の返事で動いた
            run(NOW - 20 * 3600, "issue", 7, asked()),
            reply(
                run(NOW - 18 * 3600 + 60, "issue", 7, moved("implement")),
                labels::NEEDS_HUMAN,
            ),
            // 時間切れで人に回り、5時間後に人がラベルを付け直した（返事のラベルはない）
            run(
                NOW - 14 * 3600,
                "issue",
                7,
                Outcome::Human {
                    requested: None,
                    reason: Some("時間の上限".into()),
                    failure: Some("timeout".into()),
                },
            ),
            run(NOW - 9 * 3600 + 60, "issue", 7, moved("implement")),
            with_pr(run(NOW - 8 * 3600, "issue", 7, moved("review")), 12),
            // 見送りと、使用量の上限で戻したものは数えない
            run(
                NOW - 7 * 3600,
                "pull_request",
                12,
                Outcome::Skipped { reason: None },
            ),
            run(NOW - 6 * 3600, "pull_request", 12, Outcome::Paused),
            run(NOW - 5 * 3600, "pull_request", 12, moved(DONE)),
            // 検品中の返事に、返事だけをして検品待ちに戻した（差し戻しではない）
            reply(
                run(NOW - 4 * 3600, "pull_request", 12, moved(DONE)),
                labels::IN_REVIEW,
            ),
            // 検品で差し戻され、直してレビューへ進めた
            reply(
                run(NOW - 3 * 3600, "pull_request", 12, moved("review")),
                labels::IN_REVIEW,
            ),
            run(NOW - 2 * 3600, "pull_request", 12, moved(DONE)),
            conflict,
            run(NOW - 1700, "pull_request", 12, moved(DONE)),
            // 検品待ちのPRに、人がcellのラベルを付け直して、直させた（返事のラベルはない）
            run(NOW - 1000, "pull_request", 12, moved("review")),
            run(NOW - 900, "pull_request", 12, moved(DONE)),
        ];
        let summary = summarize(&runs, NOW);
        assert_eq!(summary.finished, 1);
        assert_eq!(summary.hands_off, 0);
        assert_eq!(summary.runs, 13);
        assert_eq!(summary.handed, 2);
        assert_eq!(summary.replies, 3);
        assert_eq!(summary.sent_back, 2);
        assert_eq!(summary.waits, [2 * 3600, 5 * 3600]);
        let lines = summary.describe();
        assert!(
            lines.contains(
                &"  人に回した理由: AIが選んだ（質問、着手の確認） 1、時間切れ 1".to_string()
            ),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"  人を待った時間: 中央値 2時間、最長 5時間".to_string()),
            "{lines:?}"
        );
        assert!(
            summary
                .brief()
                .unwrap()
                .ends_with("、待った時間の中央値 2時間")
        );
    }

    #[test]
    fn counts_a_relabel_of_a_closed_issue_as_a_reply() {
        let runs = [
            run(NOW - 3000, "issue", 7, moved(DONE)),
            // 人が開き直して、ラベルを付け直した
            run(NOW - 2000, "issue", 7, moved(STOP)),
        ];
        let summary = summarize(&runs, NOW);
        assert_eq!(
            (summary.finished, summary.replies, summary.hands_off),
            (1, 1, 0)
        );
        assert_eq!(summary.ends, [0, 0, 1]);
        assert!(summary.waits.is_empty());
    }

    #[test]
    fn counts_only_recent_cases_and_keeps_ongoing_ones_apart() {
        let runs = [
            // 古いもの
            run(NOW - (DAYS + 1) * DAY, "issue", 1, moved(DONE)),
            // 途中のもの（人を待っている、エラーで止まった）
            run(NOW - DAY, "issue", 2, asked()),
            run(
                NOW - DAY,
                "issue",
                3,
                Outcome::Failed { error: "gh".into() },
            ),
            // 閉じたものと止めたものも、終わったものに数える
            run(NOW - DAY, "issue", 4, moved(STOP)),
            run(NOW - DAY, "issue", 5, moved(DONE)),
            run(NOW - DAY, "issue", 6, moved(DONE)),
        ];
        let summary = summarize(&runs, NOW);
        assert_eq!(summary.finished, 3);
        assert_eq!(summary.ongoing, 2);
        assert_eq!(
            summary.describe()[..2],
            [
                "手離れ（直近30日に終わった3件。ほかに途中のもの2件）",
                "  終わり方: 閉じた 2、止めた 1"
            ]
        );
        // 割合は四捨五入（2/3 は 67%）
        let mut two_of_three = summary.clone();
        two_of_three.hands_off = 2;
        assert_eq!(two_of_three.hands_off_percent(), 67);
        // 終わったものがなければ、そう書く
        let summary = summarize(&runs[1..3], NOW);
        assert_eq!(
            summary.describe(),
            ["手離れ（直近30日）: 終わったものはまだありません。ほかに途中のもの2件"]
        );
        assert!(summary.brief().is_none());
        assert_eq!(
            summarize(&[], NOW).describe(),
            ["手離れ（直近30日）: 終わったものはまだありません"]
        );
    }
}
