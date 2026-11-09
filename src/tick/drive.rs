//! tickの1回分（`agentcells tick`）。仕事を探して、上限まで続けて動かし、最後にinspectorを動かす。
//! 外のコマンド（`Probe`）とプログラム（[`Programs`]）と作業データの置き場所（`Home`）を受け取るので、
//! テストでは、GitHubとプログラムを偽物にして通しで動かせる（プロセスの一覧や手元のgitは本物を使う）。

use crate::config::Config;
use crate::error::{Error, Result};
use crate::github::{self, Purpose};
use crate::guard;
use crate::home::{Home, RepoName};
use crate::inspect;
use crate::labels;
use crate::limit;
use crate::probe::Probe;
use crate::programs::Programs;
use crate::prompt;
use crate::record::{self, unix_now};
use crate::run;
use crate::running;
use crate::tidy;

use super::{MAX_RUNS, Quiet, Session};

pub struct Options {
    /// 動かさず、次に動かすものを見せるだけ
    pub dry_run: bool,
    /// このtickで動かし続ける時間の上限（分）
    pub max_minutes: u64,
    /// 使用量の上限などで止まっていても始める
    pub ignore_pause: bool,
    /// `claude -p` の `--model`。なければ既定のもの
    pub model: Option<String>,
}

/// 終わったところ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    /// 終わった理由。記録（`Ended`）と同じもの（`no_work`、`limit`、`usage`、`ai_failed`、`dry_run`）か、
    /// 始めなかったときの `locked`（ほかのtickが動いている）、`paused`（使用量の上限などで止まっている）
    pub why: &'static str,
    /// 動かした件数
    pub runs: usize,
}

pub fn drive(
    home: &Home,
    probe: &dyn Probe,
    programs: &Programs,
    options: &Options,
) -> Result<Ended> {
    crate::schedule::rotate_log(home);
    let Some(_lock) = super::lock(home)? else {
        println!("ほかのtickが動いているので、何もしません");
        return Ok(Ended {
            why: "locked",
            runs: 0,
        });
    };
    // ロックを取れたので、動いているcellはない。殺されたtickが残した印を片付ける
    running::finish(home);
    // 使用量の上限などで止まっている間は、新しく始めない（試すだけなら、知らせて続ける）
    if let Some(pause) = limit::active(home, unix_now())
        && !options.ignore_pause
    {
        println!(
            "{}のため、あと{}は新しく始めません（急ぐときは `agentcells tick --ignore-pause`）",
            pause.reason,
            running::elapsed(pause.until.saturating_sub(unix_now()))
        );
        if !options.dry_run {
            return Ok(Ended {
                why: "paused",
                runs: 0,
            });
        }
    }
    // 動いたことを記録する（inspectorの材料）。試すだけ（--dry-run）なら記録しない
    record::rotate(home);
    let recorder = (!options.dry_run).then(|| record::Recorder::new(home, unix_now()));
    let note = |event: record::Event| {
        if let Some(recorder) = &recorder {
            recorder.write(unix_now(), event);
        }
    };
    note(record::Event::Started {
        version: env!("CARGO_PKG_VERSION").to_string(),
        max_minutes: options.max_minutes,
    });
    // 終わった理由を記録する。ここから先でエラーになったら error とその中身
    let mut why = "error";
    let mut runs = 0;
    let result = (|| -> Result<Config> {
        // 前の実行でAIのプロセスを止めきれなかったら、それが残っている間は何も動かさない
        // （次の仕事の書き込みのトークンを、残ったプロセスに読ませないため）
        if let Some(note_text) = run::stuck(home) {
            why = "stuck";
            return Err(Error::new(note_text).with_hint(
                "`agentcells doctor` でも確かめられます。プロセスがなくなれば、次のtickから動きます",
            ));
        }
        let config = Config::load(&home.config_path())?;
        let credentials_for =
            |repo: &RepoName| github::credentials(&config, probe, repo, Purpose::Read);
        let actors = guard::Actors {
            // 設定を読むときに、productがあれば operator は必ずある
            operator: config.operator.clone().unwrap_or_default(),
            writer: prompt::writer_login(&config, probe)?,
        };
        // ラベルが足りないと、仕事が見つからなかったり、ラベルを付け替えられなかったりする
        match labels::missing(&config, probe) {
            Ok(missing) if !missing.is_empty() => println!(
                "注意: 足りないラベルがあります（`agentcells labels sync` で作れます）:\n{}\n",
                labels::describe_missing(&missing)
            ),
            Ok(_) => {}
            Err(e) => println!("注意: ラベルを確かめられません: {}\n", e.message),
        }
        // 閉じたものに残ったラベルを片付ける。できなくても、仕事は進める
        let (leftovers, errors) = tidy::find(&config, probe, &credentials_for);
        for error in &errors {
            println!("注意: 片付けるものを探せません: {error}");
        }
        if options.dry_run {
            for l in &leftovers {
                println!("片付ける予定: {} を外す", l.describe());
            }
        } else if !leftovers.is_empty() {
            let write_for =
                |repo: &RepoName| github::credentials(&config, probe, repo, Purpose::Work);
            let results = tidy::remove(probe, &leftovers, &write_for);
            for (l, result) in leftovers.iter().zip(results) {
                note(record::Event::tidied(
                    l.kind,
                    &l.repo.to_string(),
                    l.number,
                    &l.labels,
                    result.as_ref().err().map(|e| e.message.clone()),
                ));
                match result {
                    Ok(()) => println!("片付け: {} を外しました", l.describe()),
                    Err(e) => {
                        println!("注意: {} を外せません: {}", l.describe(), e.message)
                    }
                }
            }
        }
        if !errors.is_empty() || !leftovers.is_empty() {
            println!();
        }
        let agentcells = programs.agentcells()?;
        // 仕事が残っていれば、上限まで続けて探して動かす
        let mut session = Session::new(std::time::Duration::from_secs(
            options.max_minutes.saturating_mul(60),
        ));
        // 見送りは、読み直すたびに同じものが出るので、1回のtickでは1度だけ出す
        let mut told: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            if session.runs() > 0 {
                // 止めきれなかったプロセスが残ったら、ほかの仕事も動かさない
                if let Some(note_text) = run::stuck(home) {
                    why = "stuck";
                    return Err(Error::new(note_text).with_hint(
                        "`agentcells doctor` でも確かめられます。プロセスがなくなれば、次のtickから動きます",
                    ));
                }
                if let Some(halted) = session.halted() {
                    println!(
                        "\n{}ので、残りは次のtickで動かします",
                        if halted == "usage" {
                            "使用量が設定を超えた"
                        } else {
                            "AIの実行が失敗した"
                        }
                    );
                    why = halted;
                    break;
                }
                if session.exhausted() {
                    println!(
                        "\nこのtickの上限（{}分、{}件）に達したので、残りは次のtickで動かします",
                        options.max_minutes, MAX_RUNS
                    );
                    why = "limit";
                    break;
                }
                println!(
                    "\n--- 続けて、仕事を探します（このtickで{}件目のあと）",
                    session.runs()
                );
            }
            let mut quiet = Quiet::load(home);
            let found = super::find(&config, probe, &credentials_for, &actors, &mut quiet)?;
            quiet.save(home);
            for (w, reason) in &found.skipped {
                let line = format!("見送り: {}（{reason}）", w.describe());
                if told.insert(line.clone()) {
                    println!("{line}");
                    note(record::Event::Skipped {
                        subject: record::Subject::of(w),
                        reason: reason.clone(),
                    });
                }
            }
            let work = found.work;
            let Some(next) = session.next(&work) else {
                if session.runs() == 0 {
                    println!("仕事はありません");
                } else {
                    println!("ほかに仕事はありません");
                }
                why = "no_work";
                break;
            };
            println!("仕事: {}件", work.len());
            for w in &work {
                println!("  {}", w.describe());
            }
            println!("\n次に動かすもの: {}", next.describe());
            if options.dry_run {
                why = "dry_run";
                break;
            }
            let scanner = programs.scanner()?;
            let claude = programs.claude()?;
            let runner = run::Runner {
                home,
                config: &config,
                probe,
                actors: &actors,
                claude: &claude,
                agentcells: &agentcells,
                scanner: &scanner,
                model: options.model.as_deref(),
            };
            println!("動かしています...");
            let started = unix_now();
            let report = match runner.run(next) {
                Ok(report) => report,
                Err(e) => {
                    let now = unix_now();
                    note(record::Event::Failed {
                        subject: record::Subject::of(next),
                        started,
                        seconds: now.saturating_sub(started),
                        error: e.message.clone(),
                    });
                    // claudeを動かしたあとの失敗なら、その中身も読めるように残す
                    if config.inspector.is_some() {
                        crate::notes::keep_digest(home, next, "failed", 0, started, now);
                    }
                    return Err(e);
                }
            };
            note(record::Event::ran(next, started, unix_now(), &report));
            // inspectorの2段目のために、実行の中身の要約を残す（claudeの出力は、次の実行で上書きされる）
            if config.inspector.is_some() && report.destination != "skipped" {
                crate::notes::keep_digest(
                    home,
                    next,
                    &report.destination,
                    report.round,
                    started,
                    unix_now(),
                );
            }
            let skipped = report.destination == "skipped";
            match &report.reason {
                Some(reason) if skipped => println!("見送りました: {reason}"),
                Some(reason) if report.destination == "paused" => {
                    println!("止めました: {reason}（人間には回さず、元のラベルに戻しました）")
                }
                Some(reason) => println!("{} に進めました（{reason}）", report.destination),
                None => println!("{} に進めました", report.destination),
            }
            if let Some(note) = crate::blocked::note(&report.blocked) {
                println!("{note}");
            }
            if let Some(note) = crate::allowed::note(&report.allow_secrets) {
                println!("{note}");
            }
            session.ran(next, skipped);
            runs = session.runs();
            // 使用量が設定を超えたら、上限に当たる前に止まる（使う人が自分で使う分を残すため）
            if let Some(near) = report.near_limit.as_ref().filter(|_| !report.ai_failed) {
                session.halt("usage");
                let reason = near.reason();
                limit::pause(home, near.until, &reason);
                println!(
                    "{reason}のため、あと{}は新しく始めません（急ぐときは `agentcells tick --ignore-pause`。超えている間は、1件動かすたびにまた止まります）",
                    running::elapsed(near.until.saturating_sub(unix_now()))
                );
            }
            if report.ai_failed {
                session.halt("ai_failed");
                // 使用量の上限なら戻る時刻まで、ほかの仕事も同じように失敗しそうなら（ログインの切れなど）
                // しばらく、新しく始めない
                let now = unix_now();
                let until = report.limited_until.or_else(|| {
                    limit::pauses_after(report.failure.as_deref())
                        .then_some(now + limit::AFTER_FAILURE)
                });
                if let Some(until) = until {
                    let reason = if report.limited_until.is_some() {
                        "使用量の上限"
                    } else {
                        "AIの実行の失敗"
                    };
                    limit::pause(home, until, reason);
                    println!(
                        "{reason}のため、あと{}は新しく始めません",
                        running::elapsed(until.saturating_sub(now))
                    );
                }
            }
        }
        Ok(config)
    })();
    note(record::Event::Ended {
        runs,
        why: why.to_string(),
        error: result.as_ref().err().map(|e| e.message.clone()),
    });
    let config = result?;
    // 1日に1回（設定した間隔ごとに）、最後にinspectorを動かす。AIの失敗や残ったプロセスで止まったときは動かさない
    if let Some(inspector) = &config.inspector
        && matches!(why, "no_work" | "limit")
        && inspect::due(home, inspector, unix_now())
    {
        println!("\n--- inspector");
        match inspect::run_both(home, &config, inspector, probe, programs, &note) {
            Ok(summary) => inspect::print(&summary),
            Err(e) => println!("注意: inspectorが動けませんでした: {}", e.message),
        }
    }
    Ok(Ended { why, runs })
}
