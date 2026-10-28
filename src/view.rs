//! 設定を、2Dゲーム風のポップな工場として描いた、1つのHTMLファイルにする（`agentcells config view`）。
//!
//! productごとに1本のラインを縦に並べ、cellを機械、`next` をベルトコンベアとして描く。外のJS、CDN、
//! 画像やフォントのファイルは使わず、インラインのSVGとCSSだけで描く（ファイル1つで、どこでも開けるように）。
//! 何を描くか（注意を含む）は [`crate::flow`] が決める。

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::config::Target;
use crate::flow::{Factory, Line, Station};
use crate::transition::{DONE, STOP};

const MACHINE_W: i64 = 210;
const MACHINE_H: i64 = 190;
const EXIT_W: i64 = 150;
const EXIT_H: i64 = 90;
const COL_W: i64 = 300;
const ROW_H: i64 = 250;
const PAD_X: i64 = 50;
/// 上を回るベルト（列を2つ以上とばすもの）の1本ごとの間
const LANE: i64 = 16;

/// 機械の色（ラインの中の順に回す）。
const COLORS: &[&str] = &[
    "#60a5fa", "#f472b6", "#34d399", "#fbbf24", "#a78bfa", "#fb7185", "#2dd4bf",
];

/// HTMLの中に置けるように、文字をエスケープする。
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// 工場の絵のHTML。
pub fn render(factory: &Factory) -> String {
    let notes = factory.notes();
    let mut html = String::new();
    html.push_str("<!doctype html>\n<html lang=\"ja\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    html.push_str("<title>agentcells の工場</title>\n<style>\n");
    html.push_str(STYLE);
    html.push_str("</style>\n</head>\n<body>\n");
    html.push_str("<header><h1>agentcells の工場</h1>");
    let _ = writeln!(
        html,
        "<p class=\"summary\">ライン {}本 ・ 機械 {}台 ・ 注意 {}件</p></header>",
        factory.lines.len(),
        factory
            .lines
            .iter()
            .map(|l| l.stations.len())
            .sum::<usize>(),
        notes.len()
    );
    if notes.is_empty() {
        html.push_str("<section class=\"board ok\"><h2>注意はありません</h2></section>\n");
    } else {
        html.push_str("<section class=\"board\"><h2>⚠ 注意</h2><ul>");
        for note in &notes {
            let _ = write!(html, "<li>{}</li>", escape(note));
        }
        html.push_str("</ul></section>\n");
    }
    if factory.lines.is_empty() {
        html.push_str("<p class=\"empty\">productがありません。config.toml に [[product]] を書くと、ラインになります。</p>\n");
    }
    for line in &factory.lines {
        render_line(&mut html, line);
    }
    if !factory.unused.is_empty() {
        html.push_str("<section class=\"warehouse\"><h2>倉庫（どのラインにも入っていないcell）</h2><div class=\"crates\">");
        for name in &factory.unused {
            let _ = write!(html, "<span class=\"crate\">{}</span>", escape(name));
        }
        html.push_str("</div></section>\n");
    }
    html.push_str(LEGEND);
    html.push_str("</body>\n</html>\n");
    html
}

/// 図の中の、機械や出口の場所。
#[derive(Clone, Copy)]
struct Place {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    col: i64,
}

/// ベルトの道すじ。
enum Route {
    /// となりの列へ、横から横へ
    Side,
    /// 2列以上先へ、上を回る
    Over(i64),
    /// 同じ列か前の列へ（差し戻し）、下を回る
    Under(i64),
}

struct Belt<'a> {
    from_name: &'a str,
    to_name: &'a str,
    from: Place,
    to: Place,
    route: Route,
    class: &'static str,
    note: Option<&'static str>,
}

fn render_line(html: &mut String, line: &Line) {
    // 列（深さ）ごとに、productの cells の順に積む
    let mut columns: BTreeMap<usize, Vec<&Station>> = BTreeMap::new();
    for station in &line.stations {
        columns.entry(station.depth).or_default().push(station);
    }
    let last_col = columns.keys().next_back().copied().unwrap_or(0) as i64;
    let exit_col = last_col + 1;
    // stop は done の右の列に置く。done の下に置くと、上を回って stop へ行くベルトが done の箱を縦に抜けて、
    // done から stop へ出ていくように見えるため
    let stop_col = exit_col + 1;
    let rows = columns.values().map(Vec::len).max().unwrap_or(1).max(1) as i64;
    let uses_stop = line
        .stations
        .iter()
        .any(|s| s.cell.next.iter().any(|n| n == STOP));
    let last_exit_col = if uses_stop { stop_col } else { exit_col };

    // 先に、上と下を回るベルトの数を数えて、その分の場所をあける
    let col_of = |name: &str| -> i64 {
        if name == DONE {
            exit_col
        } else if name == STOP {
            stop_col
        } else {
            line.station(name).map_or(0, |s| s.depth as i64)
        }
    };
    let mut over = 0;
    let mut under = 0;
    for s in &line.stations {
        for next in targets(line, s) {
            let diff = col_of(next) - s.depth as i64;
            if diff > 1 {
                over += 1;
            } else if diff <= 0 {
                under += 1;
            }
        }
    }
    let top = 70 + over * LANE;
    let machines_bottom = top + (rows - 1) * ROW_H + MACHINE_H;
    let under_bottom = machines_bottom + 30 + under * 22;
    let rail_y = under_bottom + 40;
    let height = rail_y + 70;
    let width = PAD_X * 2 + (last_exit_col + 1) * COL_W;

    let mut places: BTreeMap<&str, Place> = BTreeMap::new();
    for (depth, stations) in &columns {
        for (row, station) in stations.iter().enumerate() {
            places.insert(
                station.cell.name.as_str(),
                Place {
                    x: PAD_X + *depth as i64 * COL_W,
                    y: top + row as i64 * ROW_H,
                    w: MACHINE_W,
                    h: MACHINE_H,
                    col: *depth as i64,
                },
            );
        }
    }
    // 出口は1段目の機械と同じ高さに置くので、箱の下の端は機械の下の端より上にある
    let exit_place = |col: i64| Place {
        x: PAD_X + col * COL_W,
        y: top + (MACHINE_H - EXIT_H) / 2,
        w: EXIT_W,
        h: EXIT_H,
        col,
    };
    places.insert(DONE, exit_place(exit_col));
    if uses_stop {
        places.insert(STOP, exit_place(stop_col));
    }

    let mut belts = Vec::new();
    let (mut over_i, mut under_i) = (0, 0);
    for s in &line.stations {
        let from = places[s.cell.name.as_str()];
        for next in targets(line, s) {
            let Some(&to) = places.get(next) else {
                continue;
            };
            let diff = to.col - from.col;
            let route = if diff == 1 {
                Route::Side
            } else if diff > 1 {
                over_i += 1;
                Route::Over(top - 20 - over_i * LANE)
            } else {
                under_i += 1;
                Route::Under(machines_bottom + 30 + under_i * 22)
            };
            let gated = s.cell.confirms() && next != DONE;
            let class = if s.replaced.is_some() {
                "belt replaced"
            } else if gated {
                "belt gated"
            } else {
                "belt"
            };
            belts.push(Belt {
                from_name: s.cell.name.as_str(),
                to_name: next,
                from,
                to,
                route,
                class,
                note: gated.then_some("承認のあと"),
            });
        }
    }

    let _ = writeln!(
        html,
        "<section class=\"line\">\n<h2><span class=\"sign\">LINE</span> {}</h2>\n<p class=\"repos\">Issue: <code>{}</code> ・ 作業: {}</p>",
        escape(&line.product),
        escape(&line.issues),
        line.repos
            .iter()
            .map(|r| format!("<code>{}</code>", escape(r)))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let _ = writeln!(
        html,
        "<div class=\"floor\"><svg viewBox=\"0 0 {width} {height}\" width=\"{width}\" height=\"{height}\" role=\"img\" aria-label=\"product {} のライン\">",
        escape(&line.product)
    );
    html.push_str(
        "<defs><marker id=\"arrow\" viewBox=\"0 0 10 10\" refX=\"7\" refY=\"5\" markerWidth=\"5\" markerHeight=\"5\" orient=\"auto-start-reverse\"><path d=\"M0,0 L10,5 L0,10 z\" fill=\"#334155\"/></marker></defs>\n",
    );

    // 人間の窓口（needs-human）。どの機械からも、点線で降りられる
    let _ = write!(
        html,
        "<g class=\"human\"><line class=\"rail\" x1=\"{}\" y1=\"{rail_y}\" x2=\"{}\" y2=\"{rail_y}\"/>",
        PAD_X + 150,
        width - PAD_X
    );
    for s in &line.stations {
        let p = places[s.cell.name.as_str()];
        let _ = write!(
            html,
            "<line class=\"drop\" x1=\"{x}\" y1=\"{}\" x2=\"{x}\" y2=\"{rail_y}\"/>",
            p.y + p.h,
            x = p.x + 18
        );
    }
    let _ = writeln!(
        html,
        "<rect class=\"booth\" x=\"{}\" y=\"{}\" width=\"150\" height=\"54\" rx=\"14\"/><text class=\"booth-title\" x=\"{}\" y=\"{}\">needs-human</text><text class=\"booth-sub\" x=\"{}\" y=\"{}\">人間の窓口（どこからでも）</text></g>",
        PAD_X,
        rail_y - 27,
        PAD_X + 75,
        rail_y - 5,
        PAD_X + 75,
        rail_y + 14
    );

    for belt in &belts {
        render_belt(html, belt);
    }
    for (i, s) in line.stations.iter().enumerate() {
        render_machine(
            html,
            s,
            places[s.cell.name.as_str()],
            COLORS[i % COLORS.len()],
        );
    }
    render_exit(
        html,
        places[DONE],
        "exit done",
        DONE,
        "出荷",
        "Issueは閉じる・PRは検品待ち",
    );
    if uses_stop {
        render_exit(
            html,
            places[STOP],
            "exit stop",
            STOP,
            "ラインから下ろす",
            "閉じずに人間が進める",
        );
    }
    html.push_str("</svg></div>\n");

    let notes = line.notes();
    if !notes.is_empty() {
        html.push_str("<ul class=\"notes\">");
        for note in &notes {
            let _ = write!(html, "<li>⚠ {}</li>", escape(note));
        }
        html.push_str("</ul>\n");
    }
    html.push_str("</section>\n");
}

/// 機械からつながるベルトの先（cellか done か stop。needs-human は窓口として別に描く）。
fn targets<'a>(line: &Line, station: &'a Station) -> Vec<&'a str> {
    station
        .cell
        .next
        .iter()
        .map(String::as_str)
        .filter(|n| *n == DONE || *n == STOP || line.station(n).is_some())
        .collect()
}

fn render_belt(html: &mut String, belt: &Belt) {
    let (f, t) = (belt.from, belt.to);
    let (p0, p1, p2, p3) = match belt.route {
        Route::Side => {
            let (sx, sy) = (f.x + f.w, f.y + f.h / 2);
            let (tx, ty) = (t.x - 6, t.y + t.h / 2);
            let bend = (tx - sx) / 2;
            ((sx, sy), (sx + bend, sy), (tx - bend, ty), (tx, ty))
        }
        Route::Over(lane) => {
            let (sx, sy) = (f.x + f.w * 2 / 3, f.y);
            let (tx, ty) = (t.x + t.w / 3, t.y - 6);
            ((sx, sy), (sx, lane), (tx, lane), (tx, ty))
        }
        Route::Under(lane) => {
            let (sx, sy) = (f.x + f.w / 2 + 20, f.y + f.h);
            let (tx, ty) = (t.x + t.w / 2 - 20, t.y + t.h + 6);
            ((sx, sy), (sx, lane), (tx, lane), (tx, ty))
        }
    };
    let d = format!(
        "M{},{} C{},{} {},{} {},{}",
        p0.0, p0.1, p1.0, p1.1, p2.0, p2.1, p3.0, p3.1
    );
    let _ = write!(
        html,
        "<g class=\"{}\" data-from=\"{}\" data-to=\"{}\"><path class=\"base\" d=\"{d}\"/><path class=\"stripes\" d=\"{d}\" marker-end=\"url(#arrow)\"/>",
        belt.class,
        escape(belt.from_name),
        escape(belt.to_name)
    );
    if let Some(note) = belt.note {
        // ベジェ曲線のまんなか（t = 0.5）
        let mx = (p0.0 + 3 * p1.0 + 3 * p2.0 + p3.0) / 8;
        let my = (p0.1 + 3 * p1.1 + 3 * p2.1 + p3.1) / 8;
        let _ = write!(
            html,
            "<text class=\"belt-note\" x=\"{mx}\" y=\"{}\">🔒 {note}</text>",
            my - 10
        );
    }
    html.push_str("</g>\n");
}

fn render_machine(html: &mut String, s: &Station, p: Place, color: &str) {
    let cell = &s.cell;
    let on = cell
        .on
        .iter()
        .map(|t| match t {
            Target::Issue => "issue",
            Target::PullRequest => "PR",
        })
        .collect::<Vec<_>>()
        .join(" / ");
    let rows = [
        ("label", cell.label.clone()),
        ("on", on),
        (
            "model",
            cell.model.clone().unwrap_or_else(|| "既定".to_string()),
        ),
        ("max_rounds", cell.max_rounds.to_string()),
        ("timeout", format!("{}分", cell.timeout_minutes)),
        ("confirm", cell.confirms().to_string()),
        ("review", cell.review.to_string()),
    ];
    let warn = !s.notices.is_empty();
    let _ = write!(
        html,
        "<g class=\"machine{}\" style=\"--c:{color}\">",
        if warn { " warn" } else { "" }
    );
    // 煙突と煙
    let cx = p.x + p.w - 46;
    let _ = write!(
        html,
        "<rect class=\"chimney\" x=\"{cx}\" y=\"{}\" width=\"22\" height=\"30\" rx=\"3\"/><circle class=\"smoke\" cx=\"{}\" cy=\"{}\" r=\"8\"/><circle class=\"smoke s2\" cx=\"{}\" cy=\"{}\" r=\"6\"/>",
        p.y - 22,
        cx + 11,
        p.y - 30,
        cx + 16,
        p.y - 40
    );
    let _ = write!(
        html,
        "<rect class=\"body\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"18\"/>",
        p.x, p.y, p.w, p.h
    );
    // 歯車
    let _ = write!(
        html,
        "<circle class=\"gear\" cx=\"{}\" cy=\"{}\" r=\"11\"/>",
        p.x + 24,
        p.y + 22
    );
    let _ = write!(
        html,
        "<text class=\"name\" x=\"{}\" y=\"{}\">{}</text>",
        p.x + 44,
        p.y + 29,
        escape(&cell.name)
    );
    let _ = write!(
        html,
        "<rect class=\"panel\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"10\"/>",
        p.x + 10,
        p.y + 44,
        p.w - 20,
        p.h - 54
    );
    for (i, (key, value)) in rows.iter().enumerate() {
        let y = p.y + 64 + i as i64 * 18;
        let _ = write!(
            html,
            "<text class=\"key\" x=\"{}\" y=\"{y}\">{key}</text><text class=\"value\" x=\"{}\" y=\"{y}\">{}</text>",
            p.x + 20,
            p.x + 104,
            escape(value)
        );
    }
    if s.entry {
        let _ = write!(
            html,
            "<g class=\"badge entry\"><rect x=\"{}\" y=\"{}\" width=\"58\" height=\"22\" rx=\"11\"/><text x=\"{}\" y=\"{}\">入口</text></g>",
            p.x - 8,
            p.y - 14,
            p.x + 21,
            p.y + 2
        );
    }
    if let Some(original) = &s.replaced {
        let _ = write!(
            html,
            "<g class=\"badge replaced\"><title>[product.next] でこのラインだけ進め先を替えています（元の next: [{}]）</title><rect x=\"{}\" y=\"{}\" width=\"118\" height=\"22\" rx=\"11\"/><text x=\"{}\" y=\"{}\">next を替えた</text></g>",
            escape(&original.join(", ")),
            p.x + 56,
            p.y - 14,
            p.x + 115,
            p.y + 2
        );
    }
    if warn {
        let text = s
            .notices
            .iter()
            .map(|n| n.describe())
            .collect::<Vec<_>>()
            .join("\n");
        let (wx, wy) = (p.x + p.w - 4, p.y + 6);
        let _ = write!(
            html,
            "<g class=\"alert\"><title>{}</title><polygon points=\"{},{} {},{} {},{}\"/><text x=\"{wx}\" y=\"{}\">!</text></g>",
            escape(&text),
            wx,
            wy - 22,
            wx + 22,
            wy + 16,
            wx - 22,
            wy + 16,
            wy + 11
        );
    }
    html.push_str("</g>\n");
}

fn render_exit(html: &mut String, p: Place, class: &str, name: &str, title: &str, sub: &str) {
    let cx = p.x + p.w / 2;
    let _ = writeln!(
        html,
        "<g class=\"{class}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"16\"/><text class=\"exit-name\" x=\"{cx}\" y=\"{}\">{name}</text><text class=\"exit-title\" x=\"{cx}\" y=\"{}\">{title}</text><text class=\"exit-sub\" x=\"{cx}\" y=\"{}\">{sub}</text></g>",
        p.x,
        p.y,
        p.w,
        p.h,
        p.y + 30,
        p.y + 52,
        p.y + 72
    );
}

const STYLE: &str = r#"
:root { --ink: #1e293b; }
* { box-sizing: border-box; }
body { margin: 0; padding: 24px 32px 48px; font-family: system-ui, -apple-system, "Hiragino Sans", "Noto Sans JP", sans-serif; color: var(--ink);
  background: #fef9c3; background-image: radial-gradient(#fde68a 2px, transparent 2px); background-size: 22px 22px; }
header { display: flex; align-items: baseline; gap: 20px; flex-wrap: wrap; }
h1 { margin: 0 0 12px; font-size: 32px; letter-spacing: 1px; color: #7c2d12; text-shadow: 3px 3px 0 #fdba74; }
.summary { margin: 0; font-weight: bold; color: #9a3412; }
h2 { margin: 0 0 6px; font-size: 22px; }
code { background: #fff7ed; border: 2px solid #fed7aa; border-radius: 6px; padding: 0 6px; }
.board { background: #fff; border: 4px solid #f59e0b; border-radius: 18px; padding: 14px 20px; margin: 12px 0 24px; box-shadow: 6px 6px 0 #f59e0b; }
.board.ok { border-color: #22c55e; box-shadow: 6px 6px 0 #22c55e; }
.board ul, .notes { margin: 6px 0 0; padding-left: 20px; }
.line { background: #fff; border: 4px solid var(--ink); border-radius: 22px; padding: 16px 20px; margin: 0 0 28px; box-shadow: 8px 8px 0 var(--ink); }
.sign { display: inline-block; background: #ef4444; color: #fff; border-radius: 8px; padding: 0 8px; font-size: 14px; vertical-align: middle; }
.repos { margin: 0 0 10px; }
.floor { overflow-x: auto; border-radius: 16px; background: #e2e8f0;
  background-image: linear-gradient(#cbd5e1 2px, transparent 2px), linear-gradient(90deg, #cbd5e1 2px, transparent 2px); background-size: 40px 40px; }
.notes li { color: #b45309; font-weight: bold; }
.empty { font-size: 18px; }
.warehouse { background: #fff; border: 4px dashed #92400e; border-radius: 18px; padding: 14px 20px; margin: 0 0 24px; }
.crates { display: flex; gap: 12px; flex-wrap: wrap; }
.crate { background: #d97706; color: #fff; font-weight: bold; padding: 10px 16px; border-radius: 6px; border: 3px solid #92400e; }
.legend { background: #fff; border: 4px solid #64748b; border-radius: 18px; padding: 12px 20px; }
.legend li { margin: 4px 0; }
.chip { display: inline-block; width: 36px; height: 10px; border-radius: 5px; vertical-align: middle; margin-right: 6px; }

svg text { font-family: inherit; }
.belt .base { fill: none; stroke: #475569; stroke-width: 12; stroke-linecap: round; }
.belt .stripes { fill: none; stroke: #fde047; stroke-width: 4; stroke-dasharray: 10 12; animation: run 0.8s linear infinite; }
.belt.replaced .stripes { stroke: #fb923c; }
.belt.gated .stripes { stroke: #c4b5fd; animation-duration: 2s; }
.belt-note { font-size: 12px; font-weight: bold; text-anchor: middle; fill: #6d28d9; }
@keyframes run { to { stroke-dashoffset: -22; } }

.machine .body { fill: var(--c); stroke: var(--ink); stroke-width: 4; }
.machine .panel { fill: #fff; stroke: var(--ink); stroke-width: 2; opacity: 0.95; }
.machine .chimney { fill: #64748b; stroke: var(--ink); stroke-width: 3; }
.machine .smoke { fill: #f8fafc; stroke: #cbd5e1; stroke-width: 2; animation: puff 2.4s ease-out infinite; }
.machine .smoke.s2 { animation-delay: 1.2s; }
@keyframes puff { from { opacity: 0.9; transform: translateY(0); } to { opacity: 0; transform: translateY(-22px); } }
.machine .gear { fill: #fff; stroke: var(--ink); stroke-width: 4; stroke-dasharray: 5 3; transform-box: fill-box; transform-origin: center; animation: spin 4s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
.machine .name { font-size: 18px; font-weight: 800; fill: var(--ink); }
.machine .key { font-size: 12px; fill: #64748b; }
.machine .value { font-size: 12px; font-weight: bold; fill: var(--ink); }
.machine.warn .body { stroke: #dc2626; stroke-dasharray: 10 5; }
.alert polygon { fill: #facc15; stroke: var(--ink); stroke-width: 3; stroke-linejoin: round; }
.alert text { font-size: 20px; font-weight: 900; text-anchor: middle; fill: var(--ink); }
.alert { animation: blink 1.2s steps(2) infinite; }
@keyframes blink { 50% { opacity: 0.45; } }
.badge rect { stroke: var(--ink); stroke-width: 2; }
.badge text { font-size: 12px; font-weight: bold; text-anchor: middle; }
.badge.entry rect { fill: #4ade80; }
.badge.replaced rect { fill: #fb923c; }

.exit rect { stroke: var(--ink); stroke-width: 4; }
.exit.done rect { fill: #86efac; }
.exit.stop rect { fill: #cbd5e1; }
.exit text { text-anchor: middle; fill: var(--ink); }
.exit-name { font-size: 18px; font-weight: 800; }
.exit-title { font-size: 13px; font-weight: bold; }
.exit-sub { font-size: 11px; }

.human .rail { stroke: #ec4899; stroke-width: 4; stroke-dasharray: 2 8; stroke-linecap: round; }
.human .drop { stroke: #ec4899; stroke-width: 2; stroke-dasharray: 3 6; }
.human .booth { fill: #fbcfe8; stroke: var(--ink); stroke-width: 3; }
.booth-title { font-size: 15px; font-weight: 800; text-anchor: middle; }
.booth-sub { font-size: 11px; text-anchor: middle; }
"#;

const LEGEND: &str = r#"<section class="legend"><h2>見かた</h2><ul>
<li><span class="chip" style="background:#fde047"></span>ベルトコンベア: cellの <code>next</code>。矢印の向きに進む</li>
<li><span class="chip" style="background:#fb923c"></span>オレンジのベルト: <code>[product.next]</code> で、このラインだけ進め先を替えたもの</li>
<li><span class="chip" style="background:#c4b5fd"></span>紫のベルト（🔒）: 着手を確かめるcell（<code>confirm</code>）が、人間に認められたあとにだけ進める先</li>
<li><span class="chip" style="background:#ec4899"></span>ピンクの点線: <code>needs-human</code>。どの機械からでも、人間の窓口に回せる</li>
<li><b>入口</b>: Issueを扱い、ほかのcellの <code>next</code> から来ないcell。人がラベルを付けて始める</li>
<li><b>⚠</b>: 想定通りに流れないかもしれないcell。上の「注意」に理由がある</li>
</ul></section>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::flow;

    const CONFIG: &str = r#"
operator = "me"

[[product]]
name = "web-app"
issues = "octo/issues"
repos = ["octo/web"]
cells = ["triage", "implement", "review"]

[product.next]
review = ["implement", "stop"]

[[product]]
name = "docs"
issues = "octo/docs"
repos = ["octo/docs"]
cells = ["assess"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
next = ["implement", "done"]
model = "sonnet"

[[cell]]
name = "implement"
on = ["issue", "pull_request"]
label = "ac:implement"
next = ["review"]

[[cell]]
name = "review"
on = "pull_request"
label = "ac:review<b>"
next = ["implement", "done"]
review = true

[[cell]]
name = "assess"
on = "issue"
label = "ac:assess"
next = ["stop", "done"]
"#;

    fn html() -> String {
        let config = Config::parse(CONFIG, std::path::Path::new("/")).unwrap();
        render(&flow::read(&config))
    }

    #[test]
    fn draws_every_product_and_cell() {
        let html = html();
        for name in ["web-app", "docs", "octo/issues", "octo/web"] {
            assert!(html.contains(name), "{name}");
        }
        for cell in ["triage", "implement", "review", "assess"] {
            assert!(html.contains(&format!(">{cell}</text>")), "{cell}");
        }
        assert!(html.contains(">sonnet</text>"));
        assert!(html.contains("needs-human"));
        // stop を使うラインだけに stop を描く（web-app は [product.next] で、docs は assess で使う）
        assert_eq!(html.matches("class=\"exit stop\"").count(), 2);
        assert_eq!(html.matches("class=\"exit done\"").count(), 2);
        // 外のファイルを読まない
        assert!(!html.contains("<script"));
        assert!(!html.contains("http://") && !html.contains("https://"));
    }

    #[test]
    fn marks_replaced_and_gated_belts() {
        let html = html();
        assert!(html.contains("元の next: [implement, done]"));
        assert!(html.contains("class=\"belt replaced\""));
        // assess は確かめるcellなので、stop へのベルトは承認のあと
        assert!(html.contains("class=\"belt gated\""));
        assert!(html.contains("承認のあと"));
    }

    #[test]
    fn escapes_names_from_the_config() {
        let html = html();
        assert!(html.contains("ac:review&lt;b&gt;"));
        assert!(!html.contains("ac:review<b>"));
        assert_eq!(escape("a&\"'<>"), "a&amp;&quot;&#39;&lt;&gt;");
    }

    /// `marker` のあとに出てくる、最初の `name="..."` の数。
    fn number_after(html: &str, marker: &str, name: &str) -> i64 {
        let rest = &html[html.find(marker).unwrap_or_else(|| panic!("{marker}"))..];
        let key = format!("{name}=\"");
        let rest = &rest[rest.find(&key).unwrap() + key.len()..];
        rest[..rest.find('"').unwrap()].parse().unwrap()
    }

    /// 出口の箱（x, y, 幅, 高さ）。
    fn exit_box(html: &str, name: &str) -> (i64, i64, i64, i64) {
        let marker = format!("<g class=\"exit {name}\">");
        let n = |attr| number_after(html, &marker, attr);
        (n("x"), n("y"), n("width"), n("height"))
    }

    /// ベルトの道すじの点（M と C の4つ）。
    fn belt_points(html: &str, from: &str, to: &str) -> Vec<(i64, i64)> {
        let marker = format!("data-from=\"{from}\" data-to=\"{to}\"><path class=\"base\" d=\"");
        let rest = &html[html.find(&marker).unwrap_or_else(|| panic!("{marker}")) + marker.len()..];
        rest[..rest.find('"').unwrap()]
            .split([' ', 'M', 'C'])
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (x, y) = p.split_once(',').unwrap();
                (x.parse().unwrap(), y.parse().unwrap())
            })
            .collect()
    }

    #[test]
    fn belts_to_stop_do_not_run_through_done() {
        // triage から stop へは、列を3つとばして上を回る
        let text = CONFIG.replace(
            "next = [\"implement\", \"done\"]\nmodel",
            "next = [\"implement\", \"stop\", \"done\"]\nmodel",
        );
        let config = Config::parse(&text, std::path::Path::new("/")).unwrap();
        let html = render(&flow::read(&config));
        let line = &html[..html.find("LINE</span> docs").unwrap()];
        let (dx, dy, dw, _) = exit_box(line, "done");
        let (sx, _, _, _) = exit_box(line, "stop");
        // stop は done の右に置く
        assert!(sx >= dx + dw, "stop {sx} / done {dx}..{}", dx + dw);
        for from in ["triage", "review"] {
            let points = belt_points(line, from, "stop");
            // 上を回る道は done の箱より上を通り、done の箱の幅の中で降りない
            let (lane_end, end) = (points[2], points[3]);
            assert!(lane_end.1 < dy, "{from}: {points:?}");
            assert!(end.0 > dx + dw, "{from}: {points:?}");
        }
    }

    #[test]
    fn the_human_rail_runs_below_the_exits() {
        let html = html();
        // docs のラインは、機械が assess の1台だけで、下を回るベルトもない
        let line = &html[html.find("LINE</span> docs").unwrap()..];
        let rail_y = number_after(line, "<line class=\"rail\"", "y1");
        for exit in ["done", "stop"] {
            let (_, y, _, h) = exit_box(line, exit);
            assert!(y + h < rail_y, "{exit}: {} / rail {rail_y}", y + h);
        }
    }

    #[test]
    fn shows_notices_on_the_picture_and_in_a_list() {
        let text = CONFIG.replace(
            "review = [\"implement\", \"stop\"]",
            "review = [\"implement\"]",
        );
        let config = Config::parse(&text, std::path::Path::new("/")).unwrap();
        let html = render(&flow::read(&config));
        assert!(html.contains("class=\"machine warn\""));
        assert!(html.contains("done にも stop にも行けない輪"));
        assert!(html.contains("⚠ 注意"));
    }
}
