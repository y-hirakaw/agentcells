//! inspectorの1段目（concept.md）。tickの記録（`events.jsonl`）を決まった規則で調べ、見つけた気になることを
//! AIに読ませて、agentcellsのソースから原因と直し方の案を書かせる。agentcells自体の不具合は、設定した
//! リポジトリ（`[inspector] issues`）にIssueにする。書いていなければ、手元に残すだけ。
//!
//! 公開の場所に出すので、AIに渡す記録は伏せ字にする（product-1、repo-1、item-1）。理由やエラーの文は渡さない。
//! AIが書いたものに、productやリポジトリの名前が入っていれば出さずに手元に残す。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, Inspector};
use crate::error::{Error, Result};
use crate::github::{self, Purpose};
use crate::home::{Home, RepoName};
use crate::probe::Probe;
use crate::record::{self, Event, Line};

/// AIに渡す指示。
const SYSTEM: &str = include_str!("../prompts/inspector.md");
/// 作ったIssueの本文の最後に書く目印。同じ気になることのIssueが開いていれば、新しく作らずにコメントする。
const FINGERPRINT: &str = "<!-- agentcells inspector ";
const FINGERPRINT_END: &str = " -->";

/// 規則で見つけた気になること。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    /// 規則の名前（Issueの目印にも使う）
    pub fingerprint: &'static str,
    /// 何を見つけたか
    pub summary: &'static str,
    /// 当てはまったもの（伏せ字）
    pub examples: Vec<Value>,
}

/// 名前を伏せる。同じ名前には、同じ伏せ字を当てる。
#[derive(Default)]
struct Masks {
    names: HashMap<(&'static str, String), String>,
    counts: HashMap<&'static str, usize>,
}

impl Masks {
    fn mask(&mut self, kind: &'static str, name: &str) -> String {
        if let Some(masked) = self.names.get(&(kind, name.to_string())) {
            return masked.clone();
        }
        let n = self.counts.entry(kind).or_default();
        *n += 1;
        let masked = format!("{kind}-{n}");
        self.names.insert((kind, name.to_string()), masked.clone());
        masked
    }

    /// 標準のcellと決まった進め先は、そのまま。使う人が足したcellの名前は、productに関わることがあるので伏せる。
    fn cell(&mut self, name: &str) -> String {
        let known = crate::prompt::builtin(name).is_some()
            || crate::transition::is_reserved(name)
            || name == "skipped"
            || name == "paused";
        if known {
            name.to_string()
        } else {
            self.mask("cell", name)
        }
    }

    fn item(&mut self, subject: &record::Subject) -> String {
        let key = format!("{}#{}#{}", subject.repo, subject.kind, subject.number);
        format!(
            "{} {} {}",
            self.mask("repo", &subject.repo),
            subject.kind,
            self.mask("item", &key)
        )
    }
}

/// 規則で調べる。`lines` は前回より後の記録。
pub fn find(lines: &[Line]) -> (Vec<Finding>, Value) {
    let mut masks = Masks::default();
    let mut found: BTreeMap<&'static str, Finding> = BTreeMap::new();
    let mut add = |fingerprint: &'static str, summary: &'static str, example: Value| {
        found
            .entry(fingerprint)
            .or_insert_with(|| Finding {
                fingerprint,
                summary,
                examples: Vec::new(),
            })
            .examples
            .push(example);
    };

    // 同じtickで、同じIssue/PRに、同じcellが続けて動いた（返事で動いたものは除く）
    let mut last: HashMap<(String, String), (String, String, u64)> = HashMap::new();
    let mut ended: HashMap<String, String> = HashMap::new();
    let mut started: Vec<String> = Vec::new();
    let mut skipped: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut runs: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for line in lines {
        let tick = masks.mask("tick", &line.tick);
        match &line.event {
            Event::Started { .. } => started.push(line.tick.clone()),
            Event::Ran {
                subject,
                started,
                seconds,
                requested,
                destination,
                ai_failed,
                failure,
                models,
                ..
            } => {
                let cell = masks.cell(&subject.cell);
                let to = masks.cell(destination);
                *runs
                    .entry(cell.clone())
                    .or_default()
                    .entry(to.clone())
                    .or_default() += 1;
                if destination == "skipped" {
                    continue;
                }
                let item = masks.item(subject);
                let key = (line.tick.clone(), item.clone());
                if let Some((before_cell, before, at)) = last.get(&key)
                    && *before_cell == cell
                    && subject.reply.is_none()
                {
                    add(
                        "same-cell-twice",
                        "同じtickの中で、同じIssue/PRに、同じcellが続けて動いた（人間の返事で動いたものは除く）",
                        json!({
                            "tick": tick,
                            "item": item,
                            "cell": cell,
                            "first_destination": before,
                            "second_destination": to,
                            "seconds_between": started.saturating_sub(*at),
                        }),
                    );
                }
                last.insert(key, (cell.clone(), to.clone(), line.at));
                // 使用量の上限は、agentcellsの不具合ではない（戻るまで止まる）
                let limited = failure
                    .as_deref()
                    .is_some_and(|f| f.starts_with("rate_limit"));
                if *ai_failed && !limited {
                    add(
                        "ai-failed",
                        "AIの実行そのものが失敗した（failure: timeout は時間切れ、unreadable は出力を読めない、error:<種類> はclaudeが失敗を返した、no_output と bad_output は決まった形で返さなかった）",
                        json!({ "tick": tick, "item": item, "cell": cell, "seconds": seconds, "failure": failure, "models": models }),
                    );
                } else if destination == "needs-human"
                    && requested.as_deref().is_some_and(|r| r != destination)
                {
                    let requested = requested.as_deref().map(|r| masks.cell(r));
                    add(
                        "routed-to-human",
                        "AIが選んだ先ではなく、CLIが人間に回した（往復の上限、確かめ、ラベルの変化、秘密らしいものなど）",
                        json!({ "tick": tick, "item": item, "cell": cell, "requested": requested }),
                    );
                }
            }
            Event::Failed {
                subject, seconds, ..
            } => {
                let item = masks.item(subject);
                let cell = masks.cell(&subject.cell);
                add(
                    "run-failed",
                    "cellを動かしている途中で、エラーで止まった",
                    json!({ "tick": tick, "item": item, "cell": cell, "seconds": seconds }),
                );
            }
            Event::Skipped { subject, .. } => {
                let item = masks.item(subject);
                let cell = masks.cell(&subject.cell);
                skipped.entry((item, cell)).or_default().push(tick);
            }
            Event::Tidied {
                kind, repo, error, ..
            } if error.is_some() => {
                let repo = masks.mask("repo", repo);
                add(
                    "tidy-failed",
                    "閉じたものに残ったラベルを外せなかった",
                    json!({ "tick": tick, "repo": repo, "kind": kind }),
                );
            }
            Event::Ended { runs: n, why, .. } => {
                ended.insert(line.tick.clone(), why.clone());
                match why.as_str() {
                    "error" => add(
                        "tick-error",
                        "tickがエラーで終わった",
                        json!({ "tick": tick, "runs": n }),
                    ),
                    "stuck" => add(
                        "tick-stuck",
                        "止めきれなかったプロセスが残っていて、tickが動けなかった",
                        json!({ "tick": tick }),
                    ),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    for tick in &started {
        if !ended.contains_key(tick) {
            let tick = masks.mask("tick", tick);
            add(
                "tick-killed",
                "tickが終わりの記録を残さずに止まった（殺された、panicした）",
                json!({ "tick": tick }),
            );
        }
    }
    let mut repeated: Vec<_> = skipped
        .into_iter()
        .filter_map(|((item, cell), ticks)| {
            let mut ticks = ticks;
            ticks.dedup();
            (ticks.len() >= 3).then(|| json!({ "item": item, "cell": cell, "ticks": ticks.len() }))
        })
        .collect();
    repeated.sort_by_key(|v| v.to_string());
    for example in repeated {
        add(
            "skipped-repeatedly",
            "同じIssue/PRが、3回以上のtickで、動かす前の確かめで見送られた",
            example,
        );
    }
    let totals = json!({
        "ticks": started.len(),
        "runs_by_cell_and_destination": runs,
    });
    (found.into_values().collect(), totals)
}

/// 前回どこまで調べたか（`~/.agentcells/inspector/state.json`）。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct State {
    /// 調べた記録の最後の時刻（UNIXの秒）。これより後の記録を調べる
    pub until: u64,
    /// 最後に動き始めた時刻（うまくいかなくても進める。失敗したら、次に動くのは間隔がたってから）
    pub last_run: u64,
    /// AIが見送った目印と、いつまで聞き直さないか（UNIXの秒）
    #[serde(default)]
    pub quiet: BTreeMap<String, u64>,
}

/// AIが見送った目印を、聞き直さない長さ。
const QUIET_FOR: u64 = 7 * 24 * 3600;

fn dir(home: &Home) -> PathBuf {
    home.root().join("inspector")
}

impl State {
    pub fn load(home: &Home) -> Self {
        std::fs::read_to_string(dir(home).join("state.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, home: &Home) {
        let path = dir(home).join("state.json");
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::create_dir_all(dir(home));
            let _ = std::fs::write(path, text);
        }
    }
}

/// 前回から `every_hours` がたっていれば、動かす時。
pub fn due(home: &Home, inspector: &Inspector, now: u64) -> bool {
    now.saturating_sub(State::load(home).last_run) >= u64::from(inspector.every_hours) * 3600
}

/// AIが返すもの。
#[derive(Debug, Deserialize, Serialize)]
struct Proposals {
    issues: Vec<Proposal>,
    dismissed: Vec<Dismissed>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Proposal {
    fingerprint: String,
    title: String,
    body: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Dismissed {
    fingerprint: String,
    reason: String,
}

fn schema(fingerprints: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": {
            "issues": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "fingerprint": { "type": "string", "enum": fingerprints },
                        "title": { "type": "string" },
                        "body": { "type": "string" }
                    },
                    "required": ["fingerprint", "title", "body"],
                    "additionalProperties": false
                }
            },
            "dismissed": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "fingerprint": { "type": "string", "enum": fingerprints },
                        "reason": { "type": "string" }
                    },
                    "required": ["fingerprint", "reason"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["issues", "dismissed"],
        "additionalProperties": false
    })
}

/// 公開の場所に出してはいけない名前（productとリポジトリの名前）。出す先のリポジトリの名前は除く。
fn private_names(config: &Config, target: &RepoName) -> Vec<String> {
    let public = [
        target.owner.to_lowercase(),
        target.name.to_lowercase(),
        target.to_string().to_lowercase(),
    ];
    let mut names = Vec::new();
    for product in &config.products {
        names.push(product.name.clone());
        for repo in std::iter::once(&product.issues).chain(&product.repos) {
            names.push(repo.to_string());
            names.push(repo.name.clone());
        }
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|n| !public.contains(&n.to_lowercase()))
        .collect()
}

/// 本文に入っている、出してはいけない名前。大文字・小文字は区別せず、語として現れるものだけを数える
/// （`app` という名前が `application` の一部に当たって、何も出せなくならないように、前後が英数字・-・_ で
/// ないところだけ）。名前はASCIIだけなので、境目もASCIIで見る（日本語に続けて書かれた名前も見つける）。
fn leaks(text: &str, names: &[String]) -> Vec<String> {
    let lower = text.to_lowercase();
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    names
        .iter()
        .filter(|name| {
            let name = name.to_lowercase();
            lower.match_indices(&name).any(|(i, _)| {
                let before = lower[..i].chars().next_back();
                let after = lower[i + name.len()..].chars().next();
                !before.is_some_and(is_word) && !after.is_some_and(is_word)
            })
        })
        .cloned()
        .collect()
}

/// 動かすのに要るもの。
pub struct Context<'a> {
    pub home: &'a Home,
    pub config: &'a Config,
    pub inspector: &'a Inspector,
    pub probe: &'a dyn Probe,
    pub claude: &'a Path,
    pub scanner: &'a Path,
    /// 書き込みのアカウント（作ったIssueを見分けるため）
    pub writer: &'a str,
    pub now: u64,
}

/// 1回分の結果。
#[derive(Debug, Default)]
pub struct Summary {
    pub findings: usize,
    /// 見つけたことの目印（2段目に、同じことを書かせないため）
    pub fingerprints: Vec<String>,
    pub created: Vec<u64>,
    pub commented: Vec<u64>,
    pub kept: usize,
    /// 手元に残した記録の場所
    pub report: Option<PathBuf>,
    /// 人に伝えること
    pub notes: Vec<String>,
    /// 2段目が手元に残したメモの数と、その場所
    pub observations: usize,
    pub notes_path: Option<PathBuf>,
    /// 2段目が動けなかったときの理由
    pub notes_error: Option<String>,
}

/// 規則で調べるところまで（AIは動かさない）。`agentcells inspect --dry-run` で、AIに渡すものを見せる。
pub fn preview(home: &Home) -> Value {
    let state = State::load(home);
    let lines: Vec<Line> = record::read(home)
        .into_iter()
        .filter(|l| l.at > state.until)
        .collect();
    let (findings, totals) = find(&lines);
    json!({ "findings": findings, "totals": totals })
}

/// 動かす。
///
/// 動き始めたら、うまくいかなくても `last_run` を進める（失敗し続けるときに、tickのたびにAIを動かし直したり、
/// 公開のIssueにコメントを重ねたりしないため）。調べたところ（`until`）は、終わりまで行けたときだけ進める。
/// 公開のIssueへのコメントは、ほかがすべて終わってから最後にする。
pub fn run(ctx: &Context) -> Result<Summary> {
    let mut state = State::load(ctx.home);
    state.last_run = ctx.now;
    state.quiet.retain(|_, until| *until > ctx.now);
    state.save(ctx.home);
    let lines: Vec<Line> = record::read(ctx.home)
        .into_iter()
        .filter(|l| l.at > state.until && !matches!(l.event, Event::Inspected { .. }))
        .collect();
    let until = lines.iter().map(|l| l.at).max().unwrap_or(state.until);
    let (findings, totals) = find(&lines);
    let mut summary = Summary {
        findings: findings.len(),
        fingerprints: findings.iter().map(|f| f.fingerprint.to_string()).collect(),
        ..Summary::default()
    };
    if findings.is_empty() {
        state.until = until;
        state.save(ctx.home);
        return Ok(summary);
    }

    let run_dir = dir(ctx.home).join("runs").join(ctx.now.to_string());
    std::fs::create_dir_all(&run_dir)
        .map_err(|e| Error::new(format!("{} を作れません: {e}", run_dir.display())))?;

    // 前に出したIssue（目印 → いちばん新しいもの）
    let issued = match &ctx.inspector.issues {
        Some(repo) => issued(ctx, repo)?,
        None => HashMap::new(),
    };
    let mut known: Vec<(&Finding, u64)> = Vec::new();
    let mut fresh: Vec<&Finding> = Vec::new();
    let mut passed: Vec<Value> = Vec::new();
    for finding in &findings {
        match issued.get(finding.fingerprint) {
            Some(Issued::Open(number)) => known.push((finding, *number)),
            // 直さないと決めて閉じたものは、作り直さない
            Some(Issued::Declined(number)) => {
                passed.push(json!({ "fingerprint": finding.fingerprint, "why": format!("#{number} を、直さないと決めて閉じた") }));
            }
            _ if state.quiet.contains_key(finding.fingerprint) => {
                passed.push(json!({ "fingerprint": finding.fingerprint, "why": "前にAIが見送った（しばらく聞き直さない）" }));
            }
            // 直して閉じたもの（また起きた）と、まだ出していないものは、AIに聞く
            _ => fresh.push(finding),
        }
    }

    let mut proposals = Proposals {
        issues: Vec::new(),
        dismissed: Vec::new(),
    };
    if !fresh.is_empty() {
        proposals = ask(ctx, &fresh, &totals, &run_dir)?;
    }
    for dismissed in &proposals.dismissed {
        state
            .quiet
            .insert(dismissed.fingerprint.clone(), ctx.now + QUIET_FOR);
    }

    // 出す。出せないもの（出す先がない、名前が入っている、秘密らしいもの）は手元に残す
    let names = ctx
        .inspector
        .issues
        .as_ref()
        .map(|repo| private_names(ctx.config, repo))
        .unwrap_or_default();
    let mut kept: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let write = match &ctx.inspector.issues {
        Some(repo) if !proposals.issues.is_empty() || !known.is_empty() => Some((
            repo,
            github::credentials(ctx.config, ctx.probe, repo, Purpose::Work)?,
        )),
        _ => None,
    };
    for proposal in &proposals.issues {
        // 1つの気になることにつき1件
        if seen.contains(&proposal.fingerprint) {
            continue;
        }
        seen.push(proposal.fingerprint.clone());
        let before = match issued.get(&proposal.fingerprint) {
            Some(Issued::Completed(number)) => Some(*number),
            _ => None,
        };
        let body = issue_body(&proposal.body, &proposal.fingerprint, before);
        let title = format!("inspector: {}", proposal.title.trim());
        let Some((repo, write)) = &write else {
            kept.push(json!({ "proposal": proposal, "why": "出す先（[inspector] issues）がない" }));
            continue;
        };
        let leaked = leaks(&format!("{title}\n{body}"), &names);
        if !leaked.is_empty() {
            kept.push(json!({ "proposal": proposal, "why": format!("名前が入っている: {}", leaked.join(", ")) }));
            continue;
        }
        match crate::actions::scan_text(ctx.scanner, &format!("{title}\n{body}")) {
            Ok(found) if found.is_empty() => {}
            Ok(_) => {
                kept.push(json!({ "proposal": proposal, "why": "秘密らしいものがある" }));
                continue;
            }
            Err(e) => {
                kept.push(json!({ "proposal": proposal, "why": format!("秘密を調べられない: {}", e.message) }));
                continue;
            }
        }
        match create_issue(ctx.probe, write, repo, &title, &body) {
            Ok(number) => summary.created.push(number),
            Err(e) => {
                summary
                    .notes
                    .push(format!("Issueを作れません: {}", e.message));
                kept.push(json!({ "proposal": proposal, "why": "作れなかった" }));
            }
        }
    }
    summary.kept = kept.len();

    // 同じ気になることのIssueが開いていれば、また起きたとコメントする（最後に）
    if let Some((repo, write)) = &write {
        for (finding, number) in &known {
            let body = format!(
                "また {} 件ありました（agentcells v{}）。",
                finding.examples.len(),
                env!("CARGO_PKG_VERSION")
            );
            let number_arg = number.to_string();
            let repo_arg = repo.to_string();
            match gh(
                ctx.probe,
                write,
                &[
                    "issue",
                    "comment",
                    &number_arg,
                    "--repo",
                    &repo_arg,
                    "--body-file",
                    "-",
                ],
                Some(&body),
            ) {
                Ok(_) => summary.commented.push(*number),
                Err(e) => summary
                    .notes
                    .push(format!("#{number} にコメントできません: {}", e.message)),
            }
        }
    }
    if let Some((_, write)) = write {
        write.revoke(ctx.probe);
    }

    let report = run_dir.join("report.json");
    let text = serde_json::to_string_pretty(&json!({
        "findings": findings,
        "totals": totals,
        "known": known.iter().map(|(f, n)| json!({ "fingerprint": f.fingerprint, "issue": n })).collect::<Vec<_>>(),
        "passed": passed,
        "proposals": proposals,
        "kept": kept,
        "created": summary.created,
        "commented": summary.commented,
    }))
    .unwrap_or_default();
    // 書けなくても、Issueは作り終えているので、調べたところは進める（次に同じ分を数え直さない）
    match write_private(&report, &text) {
        Ok(()) => summary.report = Some(report),
        Err(e) => summary.notes.push(e.message),
    }

    state.until = until;
    state.save(ctx.home);
    Ok(summary)
}

/// 作るIssueの本文。AIの本文に目印の形の行があれば取り除き（前に出したIssueを見分けるときに、まぎれないように）、
/// 最後にCLIが目印を足す。`before` は、前に直して閉じたIssue。
fn issue_body(text: &str, fingerprint: &str, before: Option<u64>) -> String {
    let mut body = text
        .trim()
        .lines()
        .filter(|l| !l.trim_start().starts_with(FINGERPRINT.trim_end()))
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(number) = before {
        body.push_str(&format!(
            "\n\n前に #{number} で直したものが、また起きています。"
        ));
    }
    format!(
        "{body}\n\n---\nこのIssueは、agentcells の inspector が、動いた記録（伏せ字）とソースから書きました（agentcells v{}）。\n{FINGERPRINT}{fingerprint}{FINGERPRINT_END}",
        env!("CARGO_PKG_VERSION"),
    )
}

/// AIに、agentcellsのソースを読ませて、原因と直し方の案を書かせる。
fn ask(ctx: &Context, fresh: &[&Finding], totals: &Value, run_dir: &Path) -> Result<Proposals> {
    // 動いているものと同じ版のソースを、作業場所として取ってくる（使う人の手元のcloneは読ませない）
    let source = tempfile::Builder::new()
        .prefix("agentcells-inspect-")
        .tempdir()
        .map_err(|e| Error::new(format!("一時ディレクトリを作れません: {e}")))?;
    clone_source(ctx.probe, source.path())?;
    let settings_path = run_dir.join("settings.json");
    let settings = crate::settings::reader_settings(ctx.home, ctx.config)?;
    write_private(
        &settings_path,
        &serde_json::to_string_pretty(&settings).unwrap_or_default(),
    )?;
    let fingerprints: Vec<&str> = fresh.iter().map(|f| f.fingerprint).collect();
    let schema = schema(&fingerprints);
    let prompt = format!(
        "agentcells v{} の記録から、規則で次のことを見つけました（伏せ字）。それぞれについて決めてください。\n\n```json\n{}\n```\n",
        env!("CARGO_PKG_VERSION"),
        serde_json::to_string_pretty(&json!({ "findings": fresh, "totals": totals }))
            .unwrap_or_default()
    );
    let reader = crate::run::Reader {
        home: ctx.home,
        claude: ctx.claude,
        cwd: source.path(),
        settings: &settings_path,
        system: SYSTEM,
        schema: &schema,
        prompt: &prompt,
        model: ctx.inspector.model.as_deref(),
        timeout: Duration::from_secs(u64::from(ctx.inspector.timeout_minutes) * 60),
        log_dir: run_dir,
    };
    let result = reader
        .run()?
        .ok_or_else(|| Error::new("inspectorのAIが時間内に終わりませんでした"))?;
    if result["is_error"].as_bool() == Some(true) {
        return Err(Error::new(format!(
            "inspectorのAIの実行が失敗しました（{}）",
            result["subtype"].as_str().unwrap_or("理由不明")
        )));
    }
    serde_json::from_value(result["structured_output"].clone())
        .map_err(|e| Error::new(format!("inspectorのAIの結果を読めません: {e}")))
}

/// agentcellsのソースを、動いている版のタグで取ってくる。タグがなければ（手元で作った版）、既定のブランチ。
fn clone_source(probe: &dyn Probe, dest: &Path) -> Result<()> {
    let dest_str = dest
        .to_str()
        .ok_or_else(|| Error::new("一時ディレクトリの場所を文字にできません"))?;
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    // 使う人のgitの設定（insteadOf など）を読まない
    let env = [
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
    ];
    let repo = env!("CARGO_PKG_REPOSITORY");
    for args in [
        vec![
            "clone", "--quiet", "--depth", "1", "--branch", &tag, repo, dest_str,
        ],
        vec!["clone", "--quiet", "--depth", "1", repo, dest_str],
    ] {
        if probe
            .run_with_env("git", &args, &env)
            .is_some_and(|o| o.success)
        {
            return Ok(());
        }
    }
    Err(Error::new(format!(
        "agentcellsのソース（{repo}）を取ってこられません"
    )))
}

/// 前に出したIssue。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Issued {
    Open(u64),
    /// 直して閉じた（また起きたら、作り直す）
    Completed(u64),
    /// 直さないと決めたか、重複として閉じた（作り直さない）
    Declined(u64),
}

/// 書き込みのアカウントが作った、inspectorの目印があるIssue（目印 → いちばん新しいもの）。
/// 作ったのが書き込みのアカウントだけなので、目印は本文のどこにあってもよい（人が後ろに書き足しても見つける）。
fn issued(ctx: &Context, repo: &RepoName) -> Result<HashMap<String, Issued>> {
    const QUERY: &str = "query($owner: String!, $name: String!, $after: String) { repository(owner: $owner, name: $name) { issues(first: 100, after: $after, orderBy: {field: CREATED_AT, direction: DESC}) { pageInfo { hasNextPage endCursor } nodes { number state stateReason body author { __typename login } } } } }";
    // 多すぎるときは、新しいものから500件まで
    const PAGES: usize = 5;
    let read = github::credentials(ctx.config, ctx.probe, repo, Purpose::Read)?;
    let mut found = HashMap::new();
    let mut after: Option<String> = None;
    let result = (|| -> Result<()> {
        for _ in 0..PAGES {
            let mut args = vec![
                "api".to_string(),
                "graphql".to_string(),
                "-f".to_string(),
                format!("query={QUERY}"),
                "-f".to_string(),
                format!("owner={}", repo.owner),
                "-f".to_string(),
                format!("name={}", repo.name),
            ];
            if let Some(cursor) = &after {
                args.extend(["-f".to_string(), format!("after={cursor}")]);
            }
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = gh(ctx.probe, &read, &args, None)?;
            let value: Value = serde_json::from_str(&out)
                .map_err(|e| Error::new(format!("{repo} のIssueを読めません: {e}")))?;
            let issues = &value["data"]["repository"]["issues"];
            for node in issues["nodes"].as_array().into_iter().flatten() {
                if let Some((fingerprint, issued)) = issued_of(node, ctx.writer) {
                    // 新しい順に読むので、最初に見つけたものがいちばん新しい
                    found.entry(fingerprint).or_insert(issued);
                }
            }
            if issues["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
                break;
            }
            after = issues["pageInfo"]["endCursor"].as_str().map(str::to_string);
        }
        Ok(())
    })();
    read.revoke(ctx.probe);
    result.map(|()| found)
}

fn fingerprint_of(line: &str) -> Option<String> {
    line.trim()
        .strip_prefix(FINGERPRINT)?
        .strip_suffix(FINGERPRINT_END)
        .map(str::to_string)
}

fn issued_of(node: &Value, writer: &str) -> Option<(String, Issued)> {
    let login = node["author"]["login"].as_str()?;
    let author = match node["author"]["__typename"].as_str() {
        Some("Bot") => format!("{login}[bot]"),
        _ => login.to_string(),
    };
    if !author.eq_ignore_ascii_case(writer) {
        return None;
    }
    // CLIは目印をAIの本文のあとに足し、AIの本文からは目印の行を取り除くので、最後のものを使う
    let fingerprint = node["body"]
        .as_str()?
        .lines()
        .filter_map(fingerprint_of)
        .next_back()?;
    let number = node["number"].as_u64()?;
    let issued = match (node["state"].as_str(), node["stateReason"].as_str()) {
        (Some("OPEN"), _) => Issued::Open(number),
        // 重複として閉じたものも、作り直さない
        (_, Some("NOT_PLANNED" | "DUPLICATE")) => Issued::Declined(number),
        _ => Issued::Completed(number),
    };
    Some((fingerprint, issued))
}

pub(crate) fn create_issue(
    probe: &dyn Probe,
    write: &github::Credentials,
    repo: &RepoName,
    title: &str,
    body: &str,
) -> Result<u64> {
    let out = gh(
        probe,
        write,
        &[
            "issue",
            "create",
            "--repo",
            &repo.to_string(),
            "--title",
            title,
            "--body-file",
            "-",
        ],
        Some(body),
    )?;
    out.trim()
        .rsplit('/')
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| Error::new(format!("作ったIssueの番号を読めません: {}", out.trim())))
}

fn gh(
    probe: &dyn Probe,
    credentials: &github::Credentials,
    args: &[&str],
    input: Option<&str>,
) -> Result<String> {
    let env = credentials.gh_env()?;
    let out = probe
        .run_with_input("gh", args, &env, input)
        .ok_or_else(|| {
            Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
        })?;
    if !out.success {
        return Err(Error::new(format!(
            "gh {} {} が失敗しました: {}",
            args.first().unwrap_or(&""),
            args.get(1).unwrap_or(&""),
            out.stderr.trim()
        )));
    }
    Ok(out.stdout)
}

/// 手元の記録は、ほかのユーザーに読ませない。
fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| Error::new(format!("{} を書けません: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Subject;

    fn subject(repo: &str, number: u64, cell: &str, reply: Option<&str>) -> Subject {
        Subject {
            product: "secret-game".into(),
            cell: cell.into(),
            kind: "pull_request".into(),
            repo: repo.into(),
            number,
            reply: reply.map(str::to_string),
        }
    }

    fn ran(tick: &str, at: u64, subject: Subject, requested: &str, destination: &str) -> Line {
        Line {
            at,
            tick: tick.into(),
            event: Event::Ran {
                subject,
                started: at - 10,
                seconds: 10,
                round: 1,
                requested: Some(requested.into()),
                destination: destination.into(),
                pr: None,
                reason: Some("octo/secret-game のパス".into()),
                ai_failed: false,
                failure: None,
                model: None,
                models: Vec::new(),
                turns: None,
                cost_usd: None,
            },
        }
    }

    fn started(tick: &str, at: u64) -> Line {
        Line {
            at,
            tick: tick.into(),
            event: Event::Started {
                version: "0".into(),
                max_minutes: 30,
            },
        }
    }

    fn ended(tick: &str, at: u64, why: &str) -> Line {
        Line {
            at,
            tick: tick.into(),
            event: Event::Ended {
                runs: 1,
                why: why.into(),
                error: Some("octo/secret-game を読めません".into()),
            },
        }
    }

    fn fingerprints(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.fingerprint).collect()
    }

    #[test]
    fn finds_the_same_cell_running_twice_in_one_tick() {
        let pr = |cell: &str, reply: Option<&str>| subject("octo/secret-game", 4, cell, reply);
        let lines = vec![
            started("t1", 100),
            ran("t1", 200, pr("implement", None), "review", "review"),
            // 検索の遅れで、前のcellの仕事として拾い直した
            ran(
                "t1",
                300,
                pr("implement", None),
                "needs-human",
                "needs-human",
            ),
            ran("t1", 400, pr("review", None), "done", "done"),
            // 人間の返事で動いたものは数えない
            ran(
                "t1",
                500,
                pr("review", Some("ac:in-review")),
                "done",
                "done",
            ),
            ended("t1", 600, "no_work"),
            // 別のtickなら数えない
            started("t2", 700),
            ran("t2", 800, pr("review", None), "done", "done"),
            ended("t2", 900, "no_work"),
        ];
        let (findings, totals) = find(&lines);
        assert_eq!(fingerprints(&findings), ["same-cell-twice"]);
        assert_eq!(findings[0].examples.len(), 1);
        let example = &findings[0].examples[0];
        assert_eq!(example["cell"], "implement");
        assert_eq!(example["item"], "repo-1 pull_request item-1");
        assert_eq!(example["seconds_between"], 90);
        assert_eq!(totals["ticks"], 2);
        assert_eq!(totals["runs_by_cell_and_destination"]["review"]["done"], 3);
    }

    #[test]
    fn finds_failures_and_masks_names() {
        let s = |n: u64| subject("octo/secret-game", n, "implement", None);
        let mut failed = ran("t1", 200, s(1), "review", "needs-human");
        if let Event::Ran {
            ai_failed,
            requested,
            failure,
            ..
        } = &mut failed.event
        {
            *ai_failed = true;
            *requested = None;
            *failure = Some("timeout".into());
        }
        let skipped = |tick: &str| Line {
            at: 300,
            tick: tick.into(),
            event: Event::Skipped {
                subject: s(3),
                reason: "eager が付けた".into(),
            },
        };
        let lines = vec![
            started("t1", 100),
            failed,
            ran("t1", 250, s(2), "review", "needs-human"),
            skipped("t1"),
            ended("t1", 400, "error"),
            started("t2", 500),
            skipped("t2"),
            // 終わりの記録がない
            started("t3", 600),
            skipped("t3"),
        ];
        let (findings, _) = find(&lines);
        assert_eq!(
            fingerprints(&findings),
            [
                "ai-failed",
                "routed-to-human",
                "skipped-repeatedly",
                "tick-error",
                "tick-killed"
            ]
        );
        let ai = findings
            .iter()
            .find(|f| f.fingerprint == "ai-failed")
            .unwrap();
        assert_eq!(ai.examples[0]["failure"], "timeout");
        let killed = findings
            .iter()
            .find(|f| f.fingerprint == "tick-killed")
            .unwrap();
        assert_eq!(killed.examples.len(), 2);
        // 名前や、理由・エラーの文は渡さない
        let text = serde_json::to_string(&findings).unwrap();
        for word in ["secret-game", "octo", "eager", "読めません", "パス"] {
            assert!(!text.contains(word), "{word}: {text}");
        }
    }

    #[test]
    fn keeps_private_names_out_of_public_issues() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"
operator = "me"

[[product]]
name = "secret-game"
issues = "octo/secret-game"
repos = ["octo/secret-game", "octo/engine"]
cells = ["triage"]

[[product]]
name = "agentcells"
issues = "octo/agentcells"
repos = ["octo/agentcells"]
cells = ["triage"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
next = ["done"]
"#;
        let config = Config::parse(text, dir.path()).unwrap();
        let target = RepoName::parse("octo/agentcells").unwrap();
        let names = private_names(&config, &target);
        assert_eq!(
            names,
            ["engine", "octo/engine", "octo/secret-game", "secret-game"]
        );
        assert_eq!(
            leaks("src/tick.rs の Secret-Game で起きた", &names),
            ["secret-game"]
        );
        assert!(leaks("octo/agentcells の src/tick.rs", &names).is_empty());
        // 語の一部に当たっても数えない
        assert!(leaks("game-engine と engines と engine_x", &names).is_empty());
        assert_eq!(leaks("(engine)", &names), ["engine"]);
        // 日本語に続けて書いても見つける
        assert_eq!(leaks("Secret-Gameで起きた", &names), ["secret-game"]);
        assert_eq!(
            leaks("octo/secret-gameの", &names),
            ["octo/secret-game", "secret-game"]
        );
    }

    #[test]
    fn custom_cell_names_are_masked_but_standard_ones_are_not() {
        let s = |cell: &str| subject("octo/secret-game", 1, cell, None);
        let lines = vec![
            started("t1", 100),
            ran("t1", 200, s("secret-design"), "implement", "implement"),
            ran(
                "t1",
                300,
                s("secret-design"),
                "secret-design",
                "needs-human",
            ),
            ended("t1", 400, "no_work"),
        ];
        let (findings, totals) = find(&lines);
        let text = format!("{}{totals}", serde_json::to_string(&findings).unwrap());
        assert!(!text.contains("secret-design"), "{text}");
        assert!(
            text.contains("cell-1") && text.contains("implement"),
            "{text}"
        );
    }

    #[test]
    fn reads_issues_made_by_the_writer_with_a_fingerprint() {
        let node = |state: &str, reason: Option<&str>, typename: &str, body: &str| {
            json!({
                "number": 7,
                "state": state,
                "stateReason": reason,
                "body": body,
                "author": { "__typename": typename, "login": "bot" }
            })
        };
        // AIが本文に別の目印を引用していても、CLIが最後に足したものを使う
        let body = "本文\n<!-- agentcells inspector ai-failed -->\n\n---\n<!-- agentcells inspector same-cell-twice -->\n\n（人が書き足した）";
        let fp = |issued| Some(("same-cell-twice".to_string(), issued));
        assert_eq!(
            issued_of(&node("OPEN", None, "Bot", body), "bot[bot]"),
            fp(Issued::Open(7))
        );
        assert_eq!(
            issued_of(
                &node("CLOSED", Some("NOT_PLANNED"), "Bot", body),
                "bot[bot]"
            ),
            fp(Issued::Declined(7))
        );
        assert_eq!(
            issued_of(&node("CLOSED", Some("COMPLETED"), "Bot", body), "bot[bot]"),
            fp(Issued::Completed(7))
        );
        assert_eq!(
            issued_of(&node("CLOSED", Some("DUPLICATE"), "Bot", body), "bot[bot]"),
            fp(Issued::Declined(7))
        );
        // ほかの人が同じ目印を書いても数えない（同じ名前の人のアカウントも）
        assert_eq!(
            issued_of(&node("OPEN", None, "User", body), "bot[bot]"),
            None
        );
        assert_eq!(
            issued_of(&node("OPEN", None, "Bot", "目印なし"), "bot[bot]"),
            None
        );
    }

    #[test]
    fn the_cli_adds_the_only_fingerprint_to_the_body() {
        let body = issue_body(
            "原因は\n<!-- agentcells inspector ai-failed -->\nです",
            "same-cell-twice",
            Some(3),
        );
        assert!(!body.contains("ai-failed"), "{body}");
        assert!(body.contains("前に #3 で直したもの"), "{body}");
        let node = json!({
            "number": 9,
            "state": "OPEN",
            "body": body,
            "author": { "__typename": "Bot", "login": "bot" }
        });
        assert_eq!(
            issued_of(&node, "bot[bot]"),
            Some(("same-cell-twice".to_string(), Issued::Open(9)))
        );
    }

    #[test]
    fn the_schema_only_accepts_the_fingerprints_that_were_found() {
        let schema = schema(&["same-cell-twice"]);
        assert_eq!(
            schema["properties"]["issues"]["items"]["properties"]["fingerprint"]["enum"],
            json!(["same-cell-twice"])
        );
        assert_eq!(schema["additionalProperties"], json!(false));
    }
}
