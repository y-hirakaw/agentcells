//! 設定を、2Dゲーム風の工場として描いた、1つのHTMLファイルにする（`agentcells config view`）。
//!
//! productごとに1本のラインを縦に並べ、cellを機械、`next` をベルトコンベアとして描く。外のJS、CDN、
//! 画像やフォントのファイルは使わず、インラインのSVGとCSSだけで描く（ファイル1つで、どこでも開けるように）。
//! 何を描くか（注意を含む）は [`crate::flow`] が決める。

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::config::Target;
use crate::flow::{Factory, Line, Station};
use crate::transition::{DONE, STOP};

const MACHINE_W: i64 = 232;
const MACHINE_H: i64 = 236;
/// 機械や出口の、手前の面の厚さ（立体に見せる）
const DEPTH: i64 = 8;
const EXIT_W: i64 = 170;
const EXIT_H: i64 = 120;
/// 列の間のすき間の、いちばん狭い幅。ベルトは、ここと、機械の上下の通路だけを通る。
/// 通るベルトが多いすき間は、そのぶん広げる（左の端にも、1列目へ入るベルトのためのすき間を取る）
const GAP: i64 = 130;
/// すき間の両端に残す幅（ベルトの太さの半分と、機械の口のはみ出し）
const GAP_MARGIN: i64 = 36;
const ROW_H: i64 = MACHINE_H + 84;
/// 上や下を回るベルトの、1本ごとの間（ベルトの太さ28より広くして、並んでも重ならないようにする）
const LANE: i64 = 30;
/// すき間を縦に通るベルトの、1本ごとの間
const CHANNEL: i64 = 30;
/// 機械の横の面につながるベルトの、1本ごとの間（口が多いときは、面に収まるように詰める）
const PORT: i64 = 30;
/// ベルトの角の丸み
const RADIUS: i64 = 14;
/// 交わるだけのベルトに架ける橋の、長さの半分（下をくぐるベルトの太さの半分。となりの道へ曲がる角の、
/// 1本手前の道との交わりにも、角の丸みにかからずに架けられるように）
const BRIDGE: i64 = 14;
/// 機械の上にはみ出す煙突と信号灯の高さ（上の通路は、これより上を通す）
const ROOF: i64 = 46;
/// 電光掲示板の字の大きさと、流れる速さ（1秒に進む幅）
const TICKER_FONT: i64 = 14;
const TICKER_SPEED: f64 = 45.0;

/// 機械の名札の灯の色（ラインの中の順に回す）。
const COLORS: &[&str] = &[
    "#ff9f1c", "#2ec4d6", "#ff5d8f", "#9be15d", "#b18cff", "#ffe066", "#7cb8ff",
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
    html.push_str("<header><h1>agentcells <span>の工場</span></h1>");
    let _ = writeln!(
        html,
        "<p class=\"summary\"><span>ライン<b>{}</b>本</span><span>機械<b>{}</b>台</span><span>注意<b>{}</b>件</span></p></header>",
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
#[derive(Debug, Clone, Copy)]
struct Place {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    col: i64,
}

impl Place {
    fn right(&self) -> i64 {
        self.x + self.w
    }

    fn mid_y(&self) -> i64 {
        self.y + self.h / 2
    }
}

/// ベルトの道すじの決め方。どれも、直角に曲がる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// となりの列へ。右の面から出て、間のすき間で上下し、左の面に入る
    Next,
    /// 2列以上先へ。すき間を上がって機械の上の通路を回り、先の手前のすき間を下りる
    Over,
    /// 前の列へ（差し戻し）。機械の下の通路を回る
    Under,
    /// 同じ列の別の機械へ。右のすき間で上下して、右の面に入る
    Beside,
}

/// ベルト1本。`points` は、直角に曲がる折れ線（角の丸みは描くときに付ける）。
#[derive(Debug)]
struct Belt {
    from: String,
    to: String,
    class: &'static str,
    note: Option<&'static str>,
    points: Vec<(i64, i64)>,
}

/// 1本のラインの絵の配置。
#[derive(Debug)]
struct Layout {
    width: i64,
    height: i64,
    places: BTreeMap<String, Place>,
    belts: Vec<Belt>,
    /// 人間の窓口（needs-human）
    booth: Place,
    uses_stop: bool,
}

/// ベルトの口や、すき間を縦に通る道の持ち主。出ていく側はベルトごと、入る側は先ごと（合流するので）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Port<'a> {
    Out(usize),
    In(&'a str),
}

/// 右の面につながる口の材料。
struct End<'a> {
    key: Port<'a>,
    /// 向かう先の高さ。口を、これの順に上から並べる
    toward: i64,
    /// となりの同じ段へまっすぐ行くベルトか
    straight: bool,
}

/// 道すじを決める前の、ベルトの材料。
struct Edge<'a> {
    from: &'a str,
    to: &'a str,
    route: Route,
    class: &'static str,
    note: Option<&'static str>,
}

/// `n` 本を、`step` ずつ離して、0を中心に並べたときの、`k` 本目のずれ。
fn spread(k: usize, n: usize, step: i64) -> i64 {
    (2 * k as i64 - (n as i64 - 1)) * step / 2
}

fn layout(line: &Line) -> Layout {
    // 列（深さ）ごとに、productの cells の順に積む
    let mut columns: BTreeMap<usize, Vec<&Station>> = BTreeMap::new();
    for station in &line.stations {
        columns.entry(station.depth).or_default().push(station);
    }
    let last_col = columns.keys().next_back().copied().unwrap_or(0) as i64;
    let exit_col = last_col + 1;
    // stop は done の右の列に置く（上を回って stop へ行くベルトが、done を抜けないように）
    let stop_col = exit_col + 1;
    let rows = columns.values().map(Vec::len).max().unwrap_or(1).max(1) as i64;
    let uses_stop = line
        .stations
        .iter()
        .any(|s| s.cell.next.iter().any(|n| n == STOP));
    let last_exit_col = if uses_stop { stop_col } else { exit_col };

    let col_of = |name: &str| -> i64 {
        if name == DONE {
            exit_col
        } else if name == STOP {
            stop_col
        } else {
            line.station(name).map_or(0, |s| s.depth as i64)
        }
    };
    let mut edges: Vec<Edge> = Vec::new();
    for s in &line.stations {
        for next in targets(line, s) {
            let diff = col_of(next) - s.depth as i64;
            let route = match diff {
                1 => Route::Next,
                d if d > 1 => Route::Over,
                0 => Route::Beside,
                _ => Route::Under,
            };
            let gated = s.cell.confirms() && next != DONE;
            let class = if s.replaced.is_some() {
                "belt replaced"
            } else if gated {
                "belt gated"
            } else {
                "belt"
            };
            edges.push(Edge {
                from: s.cell.name.as_str(),
                to: next,
                route,
                class,
                note: gated.then_some("承認のあと"),
            });
        }
    }

    // 同じ先へ行くベルトは、上や下の通路と、先の手前のすき間で1本に合流させる（工場のコンベアのように）。
    // 通路は、近い先へのものほど機械に近い側にする（交わりを減らす）
    let lanes_of = |route: Route| -> BTreeMap<&str, i64> {
        let mut targets: Vec<(i64, &str)> = edges
            .iter()
            .filter(|e| e.route == route)
            .map(|e| {
                let col = col_of(e.to);
                (if route == Route::Over { col } else { -col }, e.to)
            })
            .collect();
        targets.sort();
        targets.dedup();
        targets
            .iter()
            .enumerate()
            .map(|(lane, (_, to))| (*to, lane as i64))
            .collect()
    };
    let over_lanes = lanes_of(Route::Over);
    let under_lanes = lanes_of(Route::Under);

    // すき間ごとに、縦に通るベルトの道を数える。出ていくものを左に、合流して先へ入るものを右（先に近い側）にする
    let mut gaps: BTreeMap<i64, Vec<Port>> = BTreeMap::new();
    for (i, e) in edges.iter().enumerate() {
        if e.route != Route::Next {
            gaps.entry(col_of(e.from)).or_default().push(Port::Out(i));
        }
        if e.route != Route::Beside {
            let list = gaps.entry(col_of(e.to) - 1).or_default();
            if !list.contains(&Port::In(e.to)) {
                list.push(Port::In(e.to));
            }
        }
    }
    for list in gaps.values_mut() {
        list.sort_by_key(|key| matches!(key, Port::In(_)));
    }
    // すき間の幅は、通る本数で決める（多いときに、端の道が機械にかからないように）
    let gap_w = |gap: i64| -> i64 {
        let n = gaps.get(&gap).map_or(0, Vec::len) as i64;
        GAP.max((n - 1).max(0) * CHANNEL + 2 * GAP_MARGIN)
    };
    let col_x =
        |col: i64| -> i64 { gap_w(-1) + (0..col).map(|c| MACHINE_W + gap_w(c)).sum::<i64>() };
    let gap_x = |gap: i64| -> i64 {
        if gap < 0 {
            gap_w(-1) / 2
        } else {
            col_x(gap) + MACHINE_W + gap_w(gap) / 2
        }
    };
    let mut channel: BTreeMap<Port, i64> = BTreeMap::new();
    for (gap, list) in &gaps {
        for (k, key) in list.iter().enumerate() {
            channel.insert(*key, gap_x(*gap) + spread(k, list.len(), CHANNEL));
        }
    }

    let top = 100 + over_lanes.len() as i64 * LANE;
    let mut places: BTreeMap<String, Place> = BTreeMap::new();
    for (depth, stations) in &columns {
        for (row, station) in stations.iter().enumerate() {
            places.insert(
                station.cell.name.clone(),
                Place {
                    x: col_x(*depth as i64),
                    y: top + row as i64 * ROW_H,
                    w: MACHINE_W,
                    h: MACHINE_H,
                    col: *depth as i64,
                },
            );
        }
    }
    // 出口は1段目の機械と同じ高さ（まんなかをそろえる）に置く
    let exit_place = |col: i64| Place {
        x: col_x(col),
        y: top + (MACHINE_H - EXIT_H) / 2,
        w: EXIT_W,
        h: EXIT_H,
        col,
    };
    places.insert(DONE.to_string(), exit_place(exit_col));
    if uses_stop {
        places.insert(STOP.to_string(), exit_place(stop_col));
    }
    let machines_bottom = top + (rows - 1) * ROW_H + MACHINE_H + DEPTH;
    let over_y = |lane: i64| top - ROOF - 24 - lane * LANE;
    // 下の通路は、通る列（先の手前の列から、いちばん遠い元の列まで）の機械のすぐ下を通す（大回りしないように）
    let under_y = |to: &str| -> i64 {
        let (lo, hi) = edges
            .iter()
            .filter(|e| e.route == Route::Under && e.to == to)
            .fold((col_of(to), col_of(to)), |(lo, hi), e| {
                (lo.min(col_of(e.from)), hi.max(col_of(e.from)))
            });
        let bottom = places
            .values()
            .filter(|p| (lo..=hi).contains(&p.col))
            .map(|p| p.y + p.h + DEPTH)
            .max()
            .unwrap_or(machines_bottom);
        let mut y = bottom + 36 + under_lanes[to] * LANE;
        // 先の手前のすき間まで横に進むので、その左の列の機械の横を通るときは、その機械から出るベルトと並ばないように、
        // その機械の下まで下げる（横を通らなければ、下げない）
        let mut beside: Vec<&Place> = places.values().filter(|p| p.col == lo - 1).collect();
        beside.sort_by_key(|p| p.y);
        for p in beside {
            if y + 14 > p.y && y - 14 < p.y + p.h + DEPTH {
                y = p.y + p.h + DEPTH + 36 + under_lanes[to] * LANE;
            }
        }
        y
    };
    let lowest = under_lanes
        .keys()
        .map(|to| under_y(to) + LANE)
        .fold(machines_bottom, i64::max);
    let booth = Place {
        x: gap_w(-1),
        y: lowest + 20,
        w: 420,
        h: 76,
        col: 0,
    };
    let height = booth.y + booth.h + DEPTH + 30;
    let width = col_x(last_exit_col) + MACHINE_W + GAP;

    // 左の面には、入ってくるベルトが合流した1本だけがつながるので、まんなかにする。
    // 右の面には、出ていくベルト（と、同じ列から右の面に入ってくる1本）を並べる。となりの同じ段へまっすぐ行くものだけを
    // まんなかにし、ほかは、向かう先の高さの順に、まんなかを空けて上下に並べる（まんなかの高さは、となりの列の同じ段の
    // 機械に入る高さなので、そこから出ると、別の先へのベルトと同じ線の上で重なる）
    let mut right: BTreeMap<&str, Vec<End>> = BTreeMap::new();
    for (i, e) in edges.iter().enumerate() {
        let (f, t) = (places[e.from], places[e.to]);
        let toward = match e.route {
            Route::Next | Route::Beside => t.mid_y(),
            Route::Over => i64::MIN,
            Route::Under => i64::MAX,
        };
        right.entry(e.from).or_default().push(End {
            key: Port::Out(i),
            toward,
            straight: e.route == Route::Next && f.mid_y() == t.mid_y(),
        });
        if e.route == Route::Beside {
            let list = right.entry(e.to).or_default();
            if !list.iter().any(|end| end.key == Port::In(e.to)) {
                list.push(End {
                    key: Port::In(e.to),
                    toward: f.mid_y(),
                    straight: false,
                });
            }
        }
    }
    let mut port: BTreeMap<Port, i64> = BTreeMap::new();
    for (name, list) in &right {
        let place = places[*name];
        let center = place.mid_y();
        let mut others: Vec<&End> = list.iter().collect();
        others.sort_by_key(|e| (e.toward, e.key));
        if let Some(pos) = others.iter().position(|e| e.straight) {
            let middle = others.remove(pos);
            port.insert(middle.key, center);
        }
        let above = others.iter().filter(|e| e.toward < center).count();
        let below = others.len() - above;
        // 面に収まるように、口が多いときは間を詰める
        let room = place.h / 2 - 14;
        let step = PORT.min(room / above.max(below).max(1) as i64);
        for (k, e) in others.iter().enumerate() {
            let slot = if k < above {
                k as i64 - above as i64
            } else {
                (k - above) as i64 + 1
            };
            port.insert(e.key, center + slot * step);
        }
    }
    let in_y = |e: &Edge| -> i64 {
        if e.route == Route::Beside {
            port[&Port::In(e.to)]
        } else {
            places[e.to].mid_y()
        }
    };

    let belts = edges
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let (f, t) = (places[e.from], places[e.to]);
            let start = (f.right(), port[&Port::Out(i)]);
            let end_y = in_y(e);
            let raw = match e.route {
                Route::Next => {
                    let gx = channel[&Port::In(e.to)];
                    vec![start, (gx, start.1), (gx, end_y), (t.x - 8, end_y)]
                }
                Route::Beside => {
                    let gx = channel[&Port::Out(i)];
                    vec![start, (gx, start.1), (gx, end_y), (t.right() + 8, end_y)]
                }
                Route::Over | Route::Under => {
                    let lane = if e.route == Route::Over {
                        over_y(over_lanes[e.to])
                    } else {
                        under_y(e.to)
                    };
                    let (gx, gx2) = (channel[&Port::Out(i)], channel[&Port::In(e.to)]);
                    vec![
                        start,
                        (gx, start.1),
                        (gx, lane),
                        (gx2, lane),
                        (gx2, end_y),
                        (t.x - 8, end_y),
                    ]
                }
            };
            Belt {
                from: e.from.to_string(),
                to: e.to.to_string(),
                class: e.class,
                note: e.note,
                points: simplify(raw),
            }
        })
        .collect();

    Layout {
        width,
        height,
        places,
        belts,
        booth,
        uses_stop,
    }
}

/// 同じ点の重なりと、まっすぐ並んだまんなかの点を除く。
fn simplify(points: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = Vec::new();
    for p in points {
        if out.last() == Some(&p) {
            continue;
        }
        if out.len() >= 2 {
            let (a, b) = (out[out.len() - 2], out[out.len() - 1]);
            if (a.0 == b.0 && b.0 == p.0) || (a.1 == b.1 && b.1 == p.1) {
                out.pop();
            }
        }
        out.push(p);
    }
    out
}

/// 折れ線を、角を丸めたSVGの道にする。
fn rounded(points: &[(i64, i64)]) -> String {
    let mut d = format!("M{},{}", points[0].0, points[0].1);
    for i in 1..points.len() {
        let p = points[i];
        if i + 1 == points.len() {
            let _ = write!(d, " L{},{}", p.0, p.1);
            break;
        }
        let (a, b) = (points[i - 1], points[i + 1]);
        let len_in = (p.0 - a.0).abs() + (p.1 - a.1).abs();
        let len_out = (b.0 - p.0).abs() + (b.1 - p.1).abs();
        let r = RADIUS.min(len_in / 2).min(len_out / 2);
        let before = (
            p.0 - (p.0 - a.0).signum() * r,
            p.1 - (p.1 - a.1).signum() * r,
        );
        let after = (
            p.0 + (b.0 - p.0).signum() * r,
            p.1 + (b.1 - p.1).signum() * r,
        );
        let _ = write!(
            d,
            " L{},{} Q{},{} {},{}",
            before.0, before.1, p.0, p.1, after.0, after.1
        );
    }
    d
}

fn render_line(html: &mut String, line: &Line) {
    let layout = layout(line);
    let (width, height) = (layout.width, layout.height);
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
    html.push_str(DEFS);

    // 床の印。機械の区画の点線と、出口の前の縞
    for (name, p) in &layout.places {
        if name == DONE || name == STOP {
            let _ = write!(
                html,
                "<rect class=\"zone {}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"16\"/>",
                if name == DONE { "done" } else { "stop" },
                p.x - 14,
                p.y - 30,
                p.w + 28,
                p.h + DEPTH + 44
            );
        } else {
            let _ = write!(
                html,
                "<rect class=\"bay\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"20\"/>",
                p.x - 14,
                p.y - 14,
                p.w + 28,
                p.h + DEPTH + 28
            );
        }
    }
    html.push('\n');

    // ベルトは、層ごとに全部を描く（縁、手すり、ゴム、ローラーの順）。合流したところが、つなぎ目なく1本に見えるように
    let paths: Vec<String> = layout.belts.iter().map(|b| rounded(&b.points)).collect();
    html.push_str("<g class=\"belts\">");
    for layer in LAYERS {
        let _ = write!(html, "<g class=\"{layer}\">");
        for d in &paths {
            let _ = write!(html, "<path d=\"{d}\"/>");
        }
        html.push_str("</g>");
    }
    html.push_str("</g>\n");
    // 交わるだけのベルトは、横のベルトが橋で上を越す（合流と見分けられるように）
    let bridges = crossings(&layout.belts);
    for (x, y) in &bridges {
        let d = format!("M{},{y} H{}", x - BRIDGE, x + BRIDGE);
        let _ = write!(
            html,
            "<g class=\"belts bridge\"><path class=\"shadow\" d=\"{d}\" transform=\"translate(0,5)\"/>"
        );
        for layer in LAYERS {
            let _ = write!(html, "<path class=\"{layer}\" d=\"{d}\"/>");
        }
        html.push_str("</g>\n");
    }
    // 機械の口。右の面には、出ていくベルトの口をまとめた1枚を、ベルトが入るところには口を付ける
    let mut outs: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    for belt in &layout.belts {
        let y = belt.points[0].1;
        let span = outs.entry(belt.from.as_str()).or_insert((y, y));
        *span = (span.0.min(y), span.1.max(y));
    }
    for (name, (lo, hi)) in &outs {
        let p = layout.places[*name];
        let _ = write!(
            html,
            "<rect class=\"flange\" x=\"{}\" y=\"{}\" width=\"12\" height=\"{}\" rx=\"4\"/>",
            p.right() - 3,
            lo - 16,
            hi - lo + 32
        );
    }
    let mut ins: Vec<(i64, i64)> = Vec::new();
    for belt in &layout.belts {
        let end = belt.points[belt.points.len() - 1];
        // 左の面なら、口は面の外へ、右の面なら、面の外から内へ（ベルトの端は、面から8の外にある）
        let x = if end.0 < layout.places[&belt.to].x {
            end.0 - 1
        } else {
            end.0 - 11
        };
        if !ins.contains(&(x, end.1)) {
            ins.push((x, end.1));
        }
    }
    for (x, y) in &ins {
        let _ = write!(
            html,
            "<rect class=\"intake\" x=\"{x}\" y=\"{}\" width=\"12\" height=\"36\" rx=\"4\"/>",
            y - 18
        );
    }
    // 承認のあとにだけ進むベルトには、出ていくところの線の上に鍵の印を付ける。合流すると色で見分けられないため
    let locks: Vec<(i64, i64)> = layout
        .belts
        .iter()
        .filter(|b| b.note.is_some())
        .map(|b| (b.points[0].0 + 16, b.points[0].1))
        .collect();
    let avoid: Vec<(i64, i64)> = bridges.iter().chain(&locks).copied().collect();
    let marks = chevrons(&layout.belts, &avoid);
    for (i, belt) in layout.belts.iter().enumerate() {
        let _ = write!(
            html,
            "<g class=\"{}\" data-from=\"{}\" data-to=\"{}\">",
            belt.class,
            escape(&belt.from),
            escape(&belt.to)
        );
        render_chevrons(html, &marks, Some(i));
        html.push_str("</g>\n");
    }
    // 種類の違うベルトが合流したところの印は、ふつうの色にする
    html.push_str("<g class=\"belt\">");
    render_chevrons(html, &marks, None);
    html.push_str("</g>\n");
    for (belt, (x, y)) in layout.belts.iter().filter(|b| b.note.is_some()).zip(&locks) {
        let _ = write!(
            html,
            "<g class=\"lock\"><title>{}</title><circle cx=\"{x}\" cy=\"{y}\" r=\"11\"/><path class=\"shackle\" d=\"M{},{} v-3 a3,3 0 0 1 6,0 v3\"/><rect x=\"{}\" y=\"{}\" width=\"10\" height=\"8\" rx=\"2\"/></g>",
            belt.note.unwrap_or_default(),
            x - 3,
            y - 1,
            x - 5,
            y - 1
        );
    }

    html.push('\n');

    for (i, s) in line.stations.iter().enumerate() {
        render_machine(
            html,
            s,
            layout.places[&s.cell.name],
            COLORS[i % COLORS.len()],
        );
    }
    render_dock(
        html,
        layout.places[DONE],
        DONE,
        "出荷",
        "Issueは閉じる・PRは検品待ち",
    );
    if layout.uses_stop {
        render_dock(
            html,
            layout.places[STOP],
            STOP,
            "ラインから下ろす",
            "閉じずに人間が進める",
        );
    }
    render_booth(html, layout.booth);
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

/// ベルトの層（下から）。縁、手すりの光る縁、手すり、ゴム、ローラー。
const LAYERS: [&str; 5] = ["edge", "hi", "rail", "base", "rollers"];

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

/// 交わるだけのベルトが交わる点（合流ではないもの）。どちらの線でも端でない点だけを見る（線の端で接するのは、
/// 合流か角のため）。橋は横の線に架けるので、横の線の角の丸みにかかるほど端に近いものは除く。
fn crossings(belts: &[Belt]) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (i, a) in belts.iter().enumerate() {
        for (j, b) in belts.iter().enumerate() {
            if i == j {
                continue;
            }
            for h in a.points.windows(2).filter(|s| s[0].1 == s[1].1) {
                for v in b.points.windows(2).filter(|s| s[0].0 == s[1].0) {
                    let (x, y) = (v[0].0, h[0].1);
                    let (x0, x1) = (h[0].0.min(h[1].0), h[0].0.max(h[1].0));
                    let (y0, y1) = (v[0].1.min(v[1].1), v[0].1.max(v[1].1));
                    if x0 + BRIDGE + RADIUS <= x
                        && x + BRIDGE + RADIUS <= x1
                        && y0 < y
                        && y < y1
                        && !out.contains(&(x, y))
                    {
                        out.push((x, y));
                    }
                }
            }
        }
    }
    out
}

/// 向きを示す山形の印（ベルトの番号、場所、向き）。合流したベルトの印がずれて重ならないように、
/// 図の上の決まった間隔の点に置き、同じ点には1つだけにする。種類（印の色）の違うベルトが同じ点を通るときは、
/// ベルトの番号を `None` にする（ふつうの色で描く）。角の丸みと、`avoid`（橋と鍵）の近くには置かない。
fn chevrons(belts: &[Belt], avoid: &[(i64, i64)]) -> Vec<(Option<usize>, i64, i64, i64)> {
    const STEP: i64 = 70;
    let mut out: Vec<(Option<usize>, i64, i64, i64)> = Vec::new();
    let mut taken: BTreeMap<(i64, i64), usize> = BTreeMap::new();
    for (i, belt) in belts.iter().enumerate() {
        for pair in belt.points.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let horizontal = a.1 == b.1;
            let (from, to) = if horizontal { (a.0, b.0) } else { (a.1, b.1) };
            let angle = match (horizontal, to > from) {
                (true, true) => 0,
                (true, false) => 180,
                (false, true) => 90,
                (false, false) => 270,
            };
            let (lo, hi) = (from.min(to) + 20, from.max(to) - 20);
            let mut t = (lo + STEP - 1).div_euclid(STEP) * STEP;
            while t <= hi {
                let (x, y) = if horizontal { (t, a.1) } else { (a.0, t) };
                let near = avoid.iter().any(|p| (p.0 - x).abs() + (p.1 - y).abs() < 26);
                if !near {
                    match taken.get(&(x, y)) {
                        Some(&k) => {
                            if out[k].0.is_some_and(|j| belts[j].class != belt.class) {
                                out[k].0 = None;
                            }
                        }
                        None => {
                            taken.insert((x, y), out.len());
                            out.push((Some(i), x, y, angle));
                        }
                    }
                }
                t += STEP;
            }
        }
    }
    out
}

fn render_chevrons(
    html: &mut String,
    marks: &[(Option<usize>, i64, i64, i64)],
    belt: Option<usize>,
) {
    for (_, x, y, angle) in marks.iter().filter(|m| m.0 == belt) {
        let _ = write!(
            html,
            "<path class=\"chevron\" d=\"M-3,-4 L3,0 L-3,4\" transform=\"translate({x},{y}) rotate({angle})\"/>"
        );
    }
}

/// 箱を立体に見せる（手前の面と、鋼板の上の面と、上の縁の光）。
fn block(html: &mut String, p: Place, rx: i64) {
    let _ = write!(
        html,
        "<rect class=\"side\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"{rx}\"/><rect class=\"top\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"{rx}\"/><path class=\"shine\" d=\"M{},{} H{}\"/>",
        p.x,
        p.y + DEPTH,
        p.w,
        p.h,
        p.x,
        p.y,
        p.w,
        p.h,
        p.x + rx,
        p.y + 4,
        p.right() - rx
    );
}

/// 黄色と黒の縞の帯。
fn hazard(html: &mut String, x: i64, y: i64, w: i64) {
    let _ = write!(
        html,
        "<rect class=\"hazard\" x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"9\" rx=\"3\"/>"
    );
}

/// cellの説明を右から左へ流す電光掲示板。説明がなければ、灯っていない板だけを描く。
/// 字は、点の並びの穴（`#led`）から透けて見せる。字の幅は、`textLength` で見積もった幅にそろえ、
/// 流す距離と合わせる（JSを使わないので、描いた幅を測れないため）
fn render_ticker(html: &mut String, description: Option<&str>, x: i64, y: i64, w: i64) {
    let _ = write!(
        html,
        "<rect class=\"ticker-frame\" x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"24\" rx=\"6\"/><svg class=\"ticker\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"18\"><rect class=\"off\" width=\"100%\" height=\"100%\"/>",
        x + 3,
        y + 3,
        w - 6
    );
    let text = description
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    if !text.is_empty() {
        let inner = w - 6;
        let len = text_width(&text);
        let distance = inner + len;
        let _ = write!(
            html,
            "<title>{}</title><g class=\"glow\"><g mask=\"url(#led)\"><text x=\"{inner}\" y=\"14\" textLength=\"{len}\" style=\"--d:-{distance}px;--s:-{}px;animation-duration:{:.1}s;animation-delay:-{:.1}s\">{}</text></g></g>",
            escape(&text),
            inner - 6,
            distance as f64 / TICKER_SPEED,
            (inner - 6) as f64 / TICKER_SPEED,
            escape(&text)
        );
    }
    html.push_str("</svg>");
}

/// 電光掲示板の字の幅の見積もり。等幅の字で、ハングルより前の字（ASCIIなど）と、記号（矢印やダッシュ）、
/// 半角カナは、字の大きさの0.6倍。ほかの字（かなや漢字）は0.9倍（等幅のフォントにない字は、ほかのフォントで
/// 描かれ、1倍より少し狭いため）。
fn text_width(text: &str) -> i64 {
    let narrow = |c: char| {
        c < '\u{1100}'
            || ('\u{2000}'..='\u{2bff}').contains(&c)
            || ('\u{ff61}'..='\u{ff9f}').contains(&c)
    };
    let tenths: i64 = text.chars().map(|c| if narrow(c) { 6 } else { 9 }).sum();
    tenths * TICKER_FONT / 10
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
    let (x, y, w, h) = (p.x, p.y, p.w, p.h);
    let _ = write!(
        html,
        "<g class=\"machine{}\" style=\"--c:{color}\">",
        if warn { " warn" } else { "" }
    );
    // 屋根の煙突と煙。信号灯は、ふつうは緑、注意があれば黄をともす
    let cx = x + w - 74;
    let _ = write!(
        html,
        "<circle class=\"smoke\" cx=\"{}\" cy=\"{}\" r=\"7\"/><circle class=\"smoke s2\" cx=\"{}\" cy=\"{}\" r=\"5\"/><rect class=\"chimney\" x=\"{cx}\" y=\"{}\" width=\"22\" height=\"32\"/><rect class=\"cap\" x=\"{}\" y=\"{}\" width=\"30\" height=\"9\" rx=\"3\"/>",
        cx + 12,
        y - 40,
        cx + 5,
        y - 48,
        y - 26,
        cx - 4,
        y - 32
    );
    let lx = x + w - 45;
    let _ = write!(
        html,
        "<rect class=\"stack\" x=\"{lx}\" y=\"{}\" width=\"22\" height=\"44\" rx=\"6\"/>",
        y - ROOF
    );
    for (k, (lamp, lit)) in [("red", false), ("amber", warn), ("green", !warn)]
        .iter()
        .enumerate()
    {
        let _ = write!(
            html,
            "<rect class=\"lamp {lamp}{}\" x=\"{}\" y=\"{}\" width=\"16\" height=\"10\" rx=\"3\"/>",
            if *lit { " on" } else { "" },
            lx + 3,
            y - ROOF + 3 + k as i64 * 12
        );
    }
    block(html, p, 14);
    for (rx, ry) in [
        (x + 7, y + 7),
        (x + w - 7, y + 7),
        (x + 7, y + h - 7),
        (x + w - 7, y + h - 7),
    ] {
        let _ = write!(
            html,
            "<circle class=\"rivet\" cx=\"{rx}\" cy=\"{ry}\" r=\"3\"/>"
        );
    }
    // 名札（cellの色の灯と、歯車と名前）
    let _ = write!(
        html,
        "<rect class=\"plate\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"34\" rx=\"8\"/><rect class=\"led\" x=\"{}\" y=\"{}\" width=\"6\" height=\"20\" rx=\"3\"/><circle class=\"gear\" cx=\"{}\" cy=\"{}\" r=\"7\"/><text class=\"name\" x=\"{}\" y=\"{}\">{}</text>",
        x + 13,
        y + 11,
        w - 26,
        x + 22,
        y + 18,
        x + 42,
        y + 28,
        x + 56,
        y + 34,
        escape(&cell.name)
    );
    // 設定を映す画面
    let _ = write!(
        html,
        "<rect class=\"screen\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"124\" rx=\"8\"/><rect class=\"scan\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"120\" rx=\"7\"/>",
        x + 13,
        y + 52,
        w - 26,
        x + 15,
        y + 54,
        w - 30
    );
    for (i, (key, value)) in rows.iter().enumerate() {
        let ry = y + 70 + i as i64 * 15;
        let _ = write!(
            html,
            "<text class=\"key\" x=\"{}\" y=\"{ry}\">{key}</text><text class=\"value{}\" x=\"{}\" y=\"{ry}\">{}</text>",
            x + 23,
            if value == "true" { " yes" } else { "" },
            x + 101,
            escape(value)
        );
    }
    render_ticker(html, cell.description.as_deref(), x + 13, y + 184, w - 26);
    hazard(html, x + 16, y + h - 19, w - 32);
    if s.entry {
        let _ = write!(
            html,
            "<g class=\"badge entry\"><rect x=\"{}\" y=\"{}\" width=\"42\" height=\"19\" rx=\"5\"/><text x=\"{}\" y=\"{}\">入口</text></g>",
            x + 8,
            y - 28,
            x + 29,
            y - 14
        );
    }
    if let Some(original) = &s.replaced {
        let bx = x + if s.entry { 56 } else { 8 };
        let _ = write!(
            html,
            "<g class=\"badge replaced\"><title>[product.next] でこのラインだけ進め先を替えています（元の next: [{}]）</title><rect x=\"{bx}\" y=\"{}\" width=\"92\" height=\"19\" rx=\"5\"/><text x=\"{}\" y=\"{}\">next を替えた</text></g>",
            escape(&original.join(", ")),
            y - 28,
            bx + 46,
            y - 14
        );
    }
    if warn {
        let text = s
            .notices
            .iter()
            .map(|n| n.describe())
            .collect::<Vec<_>>()
            .join("\n");
        let wx = x + w - 32;
        let _ = write!(
            html,
            "<g class=\"alert\"><title>{}</title><polygon points=\"{wx},{} {},{} {},{}\"/><text x=\"{wx}\" y=\"{}\">!</text></g>",
            escape(&text),
            y + 16,
            wx + 12,
            y + 38,
            wx - 12,
            y + 38,
            y + 36
        );
    }
    html.push_str("</g>\n");
}

/// 出口（done は開いたシャッター、stop は閉じたシャッター）。屋根に回転灯を付ける。
fn render_dock(html: &mut String, p: Place, name: &str, title: &str, sub: &str) {
    let (x, y, w, h) = (p.x, p.y, p.w, p.h);
    let cx = x + w / 2;
    let _ = write!(
        html,
        "<g class=\"dock {name}\"><rect class=\"beacon-base\" x=\"{}\" y=\"{}\" width=\"28\" height=\"9\" rx=\"3\"/><path class=\"beacon\" d=\"M{},{} v-6 a9,9 0 0 1 18,0 v6 z\"/>",
        cx - 14,
        y - 7,
        cx - 9,
        y - 5
    );
    block(html, p, 12);
    let _ = write!(
        html,
        "<text class=\"dock-name\" x=\"{}\" y=\"{}\">{name}</text><text class=\"dock-title\" x=\"{}\" y=\"{}\">{title}</text><rect class=\"opening\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"6\"/>",
        x + 12,
        y + 26,
        x + w - 12,
        y + 25,
        x + 10,
        y + 36,
        w - 20,
        h - 62
    );
    if name == DONE {
        // 巻き上げたシャッターの下から、荷が出ていく
        let _ = write!(
            html,
            "<rect class=\"shutter\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"12\"/><text class=\"dock-sub\" x=\"{cx}\" y=\"{}\">{sub}</text>",
            x + 11,
            y + 37,
            w - 22,
            y + h - 34
        );
    } else {
        let _ = write!(
            html,
            "<rect class=\"shutter\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"5\"/><rect class=\"sub-plate\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"18\" rx=\"4\"/><text class=\"dock-sub\" x=\"{cx}\" y=\"{}\">{sub}</text>",
            x + 11,
            y + 37,
            w - 22,
            h - 64,
            cx - 58,
            y + 57,
            116,
            y + 70
        );
    }
    hazard(html, x + 10, y + h - 20, w - 20);
    html.push_str("</g>\n");
}

/// 人間の窓口（needs-human）。窓から人が見える詰め所と、呼び出しのボタン。
fn render_booth(html: &mut String, p: Place) {
    let (x, y, w) = (p.x, p.y, p.w);
    let _ = write!(
        html,
        "<g class=\"dock human\"><rect class=\"awning\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"12\" rx=\"5\"/>",
        x + 12,
        y - 8,
        w - 24
    );
    block(html, p, 12);
    let (bx, by) = (x + w - 34, y + 38);
    let _ = writeln!(
        html,
        "<rect class=\"window\" x=\"{}\" y=\"{}\" width=\"64\" height=\"50\" rx=\"8\"/><circle class=\"person\" cx=\"{}\" cy=\"{}\" r=\"8\"/><path class=\"person\" d=\"M{},{} c3,-9 9,-13 16,-13 s13,4 16,13\"/>\
<text class=\"dock-name\" x=\"{}\" y=\"{}\">needs-human</text><text class=\"dock-title\" x=\"{}\" y=\"{}\">人間の窓口・どの機械からでも回せる</text>\
<circle class=\"call\" cx=\"{bx}\" cy=\"{by}\" r=\"19\"/><path class=\"bell\" d=\"M{},{} v-5 a6,6 0 0 1 12,0 v5 l2,2 h-16 z M{},{} h4\"/></g>",
        x + 10,
        y + 13,
        x + 42,
        y + 32,
        x + 26,
        y + 60,
        x + 88,
        y + 34,
        x + 88,
        y + 55,
        bx - 6,
        by + 2,
        bx - 2,
        by + 7
    );
}

/// 鋼板や縞などの模様。
const DEFS: &str = r##"<defs>
<linearGradient id="steel" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#5d687c"/><stop offset=".45" stop-color="#4a5466"/><stop offset="1" stop-color="#3c4453"/></linearGradient>
<radialGradient id="rivet" cx=".35" cy=".3" r=".7"><stop offset="0" stop-color="#c3cad6"/><stop offset="1" stop-color="#596274"/></radialGradient>
<pattern id="hazard" width="12" height="12" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><rect width="12" height="12" fill="#ffc21a"/><rect width="6" height="12" fill="#16181d"/></pattern>
<pattern id="scan" width="4" height="3" patternUnits="userSpaceOnUse"><rect width="4" height="1" fill="#fff" fill-opacity=".035"/></pattern>
<pattern id="slats" width="8" height="6" patternUnits="userSpaceOnUse"><rect width="8" height="6" fill="#454d5c"/><rect width="8" height="3" fill="#6b7588"/></pattern>
<pattern id="vents" width="22" height="7" patternUnits="userSpaceOnUse"><rect width="22" height="7" fill="#5a6477"/><rect y="5" width="22" height="2" fill="#363d4b"/></pattern>
<pattern id="zone-done" width="20" height="20" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><rect width="10" height="20" fill="#39e58c" fill-opacity=".1"/></pattern>
<pattern id="zone-stop" width="20" height="20" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><rect width="10" height="20" fill="#ff4d4d" fill-opacity=".09"/></pattern>
<pattern id="awning" width="28" height="12" patternUnits="userSpaceOnUse"><rect width="28" height="12" fill="#f4f6fa"/><rect width="14" height="12" fill="#ff6fae"/></pattern>
<linearGradient id="opening" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#0d2a1d"/><stop offset="1" stop-color="#16523a"/></linearGradient>
<linearGradient id="glass" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#b8e3ff"/><stop offset=".55" stop-color="#6aa6d8"/><stop offset="1" stop-color="#4a86bd"/></linearGradient>
<pattern id="led-dots" width="2" height="2" patternUnits="userSpaceOnUse"><circle cx="1" cy="1" r=".8" fill="#fff"/></pattern>
<mask id="led" maskUnits="userSpaceOnUse" x="0" y="0" width="100%" height="100%"><rect width="100%" height="100%" fill="url(#led-dots)"/></mask>
<pattern id="led-off" width="2" height="2" patternUnits="userSpaceOnUse"><rect width="2" height="2" fill="#06080b"/><circle cx="1" cy="1" r=".8" fill="#1a1f29"/></pattern>
<radialGradient id="call" cx=".4" cy=".35" r=".7"><stop offset="0" stop-color="#ffa6cf"/><stop offset=".6" stop-color="#ff6fae"/><stop offset="1" stop-color="#d9478a"/></radialGradient>
</defs>
"##;

const STYLE: &str = r#"
:root { --ink: #0e1117; --yellow: #ffc21a;
  --display: "Avenir Next", "Segoe UI", "Hiragino Sans", "Noto Sans JP", system-ui, sans-serif;
  --mono: ui-monospace, "SF Mono", Menlo, Consolas, monospace; }
* { box-sizing: border-box; }
body { margin: 0; padding: 26px 30px 48px; font-family: "Hiragino Sans", "Noto Sans JP", system-ui, -apple-system, sans-serif; color: #e8edf5; background: #11141a; }
header { display: flex; align-items: center; gap: 20px; flex-wrap: wrap; margin-bottom: 14px; }
h1 { margin: 0; font-size: 32px; font-weight: 900; letter-spacing: .02em; color: #f4f6fa; }
h1 span { color: var(--yellow); }
.summary { display: flex; gap: 8px; flex-wrap: wrap; margin: 0; font-size: 12px; font-weight: bold; color: #9aa6ba; }
.summary span { background: #1d222c; border: 2px solid var(--ink); border-radius: 8px; padding: 3px 12px; box-shadow: inset 0 2px 0 rgba(255,255,255,.06); }
.summary b { font: 700 18px var(--display); color: var(--yellow); margin: 0 4px; }
h2 { margin: 0 0 6px; font-size: 20px; }
code { font-family: var(--mono); font-size: 13px; background: #1d222c; border: 1px solid #2d3442; border-radius: 4px; padding: 1px 6px; color: #cfd8e6; }
.board { background: #171b23; border: 2px solid var(--ink); border-radius: 10px; padding: 10px 18px; margin: 0 0 18px; box-shadow: inset 0 2px 0 rgba(255,255,255,.05); }
.board h2 { margin: 0; font-size: 15px; display: flex; align-items: center; gap: 12px; }
.board h2::before { content: ""; width: 12px; height: 12px; border-radius: 50%; background: #ffb020; box-shadow: 0 0 10px #ffb020; }
.board.ok h2::before { background: #39e58c; box-shadow: 0 0 10px #39e58c; }
.board ul, .notes { margin: 6px 0 0; padding-left: 22px; }
.board li, .notes li { color: #ffc75a; font-weight: bold; }
.line { background: #161a21; border: 2px solid var(--ink); border-radius: 16px; padding: 16px 20px; margin: 0 0 28px; }
.line h2 { font: 700 22px var(--display); color: #f4f6fa; }
.sign { display: inline-block; background: var(--yellow); color: #14161b; border: 2px solid var(--ink); border-radius: 5px; padding: 0 8px; font-size: 12px; letter-spacing: .08em; vertical-align: middle; }
.repos { margin: 0 0 12px; font-size: 13px; color: #9aa6ba; }
.floor { overflow-x: auto; border: 10px solid var(--yellow); border-image: repeating-linear-gradient(45deg, #ffc21a 0 12px, #15171c 12px 24px) 10; box-shadow: 0 0 0 3px var(--ink);
  background-color: #1b1f27; background-image: linear-gradient(rgba(255,255,255,.035) 2px, transparent 2px), linear-gradient(90deg, rgba(255,255,255,.035) 2px, transparent 2px), radial-gradient(rgba(255,255,255,.05) 1px, transparent 1.6px);
  background-size: 80px 80px, 80px 80px, 10px 10px; }
.floor svg { display: block; }
.empty { font-size: 18px; }
.warehouse { background: #161a21; border: 2px dashed #4b5466; border-radius: 16px; padding: 14px 20px; margin: 0 0 24px; }
.crates { display: flex; gap: 12px; flex-wrap: wrap; }
.crate { background: repeating-linear-gradient(90deg, #b9813a 0 18px, #a8722f 18px 20px); color: #fff; font-weight: bold; padding: 10px 16px; border-radius: 4px; border: 3px solid #4a2f10; text-shadow: 0 1px 0 #4a2f10; }
.legend { background: #161a21; border: 2px solid var(--ink); border-radius: 16px; padding: 12px 20px; }
.legend li { margin: 6px 0; font-size: 14px; color: #c8d1df; }
.legend b { color: #f3f6fb; }
.chip { display: inline-block; background: #1d222b; color: var(--a); font-weight: 900; letter-spacing: -2px; border-radius: 4px; padding: 0 8px 0 6px; margin-right: 8px; border: 3px solid #566174; outline: 2px solid var(--ink); }
.dot { display: inline-block; width: 12px; height: 12px; border-radius: 50%; border: 2px solid var(--ink); background: var(--a); box-shadow: 0 0 8px var(--a); margin-right: 8px; vertical-align: -1px; }

svg text { font-family: inherit; }
.bay { fill: none; stroke: rgba(255,194,26,.3); stroke-width: 2; stroke-dasharray: 8 6; }
.zone.done { fill: url(#zone-done); }
.zone.stop { fill: url(#zone-stop); }
.belts path { fill: none; stroke-linecap: butt; stroke-linejoin: round; }
.belts .edge { stroke: var(--ink); stroke-width: 28; }
.belts .hi { stroke: #8590a3; stroke-width: 22; }
.belts .rail { stroke: #566174; stroke-width: 18; }
.belts .base { stroke: #1d222b; stroke-width: 12; }
.belts .rollers { stroke: #2f3643; stroke-width: 12; stroke-dasharray: 2 5; animation: run .6s linear infinite; }
.bridge .shadow { stroke: #000; stroke-opacity: .5; stroke-width: 32; }
.bridge .edge { stroke-width: 30; }
.bridge .hi { stroke: #9aa5b8; }
@keyframes run { to { stroke-dashoffset: -7; } }
.belt { --a: #ffc21a; }
.belt.replaced { --a: #ff7a1a; }
.belt.gated { --a: #b18cff; }
.belt .chevron { fill: none; stroke: var(--a); stroke-width: 3; stroke-linecap: round; stroke-linejoin: round; }
.lock circle { fill: #b18cff; stroke: var(--ink); stroke-width: 3; }
.lock rect { fill: var(--ink); }
.lock .shackle { fill: none; stroke: var(--ink); stroke-width: 2; }
.flange { fill: #2a303b; stroke: var(--ink); stroke-width: 3; }
.intake { fill: #1a1f28; stroke: var(--ink); stroke-width: 3; }

.machine, .dock { filter: drop-shadow(0 12px 10px rgba(0,0,0,.45)); }
.side { fill: #262c37; stroke: var(--ink); stroke-width: 3; }
.top { fill: url(#steel); stroke: var(--ink); stroke-width: 3; }
.shine { fill: none; stroke: rgba(255,255,255,.22); stroke-width: 3; stroke-linecap: round; }
.rivet { fill: url(#rivet); }
.chimney { fill: url(#vents); stroke: var(--ink); stroke-width: 3; }
.cap { fill: #2a303b; stroke: var(--ink); stroke-width: 3; }
.smoke { fill: rgba(205,213,226,.25); animation: puff 2.4s ease-out infinite; }
.smoke.s2 { animation-delay: 1.2s; }
@keyframes puff { from { opacity: 1; transform: translateY(0); } to { opacity: 0; transform: translateY(-18px); } }
.stack { fill: var(--ink); }
.lamp.red { fill: #4a1c22; }
.lamp.amber { fill: #4a3a14; }
.lamp.green { fill: #173b28; }
.lamp.amber.on { fill: #ffb020; filter: drop-shadow(0 0 5px #ffb020); }
.lamp.green.on { fill: #39e58c; filter: drop-shadow(0 0 5px #39e58c); }
.plate { fill: #161a22; stroke: var(--ink); stroke-width: 2; }
.led { fill: var(--c); filter: drop-shadow(0 0 4px var(--c)); }
.gear { fill: none; stroke: var(--c); stroke-width: 4; stroke-dasharray: 3.5 2; transform-box: fill-box; transform-origin: center; animation: spin 4s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
.machine .name { font: 700 18px var(--display); fill: #f3f6fb; }
.screen { fill: #0a0e14; stroke: var(--ink); stroke-width: 2; }
.scan { fill: url(#scan); }
.machine .key { font: 11px var(--mono); fill: #7d8aa3; }
.machine .value { font: 700 11px var(--mono); fill: #e8edf5; }
.machine .value.yes { fill: #c9b0ff; }
.ticker-frame { fill: #05070a; stroke: var(--ink); stroke-width: 2; }
.ticker .off { fill: url(#led-off); }
.ticker .glow { filter: drop-shadow(0 0 2px var(--c)); }
.ticker text { font: 700 14px var(--mono); fill: var(--c); animation: ticker linear infinite; }
@keyframes ticker { to { transform: translateX(var(--d)); } }
@media print, (prefers-reduced-motion: reduce) { .ticker text { animation: none; transform: translateX(var(--s)); } }
.hazard { fill: url(#hazard); stroke: var(--ink); stroke-width: 2; }
.alert polygon { fill: #ffb020; stroke: var(--ink); stroke-width: 2; stroke-linejoin: round; }
.alert text { font-size: 14px; font-weight: 900; text-anchor: middle; fill: var(--ink); }
.alert { animation: blink 1.2s steps(2) infinite; }
@keyframes blink { 50% { opacity: 0.45; } }
.badge rect { stroke: var(--ink); stroke-width: 2; }
.badge text { font-size: 11px; font-weight: 800; text-anchor: middle; fill: #14161b; }
.badge.entry rect { fill: var(--yellow); }
.badge.replaced rect { fill: #ff7a1a; }

.beacon-base { fill: #2a303b; stroke: var(--ink); stroke-width: 2; }
.beacon { stroke: var(--ink); stroke-width: 2; }
.dock.done .beacon { fill: #39e58c; filter: drop-shadow(0 0 6px #39e58c); }
.dock.stop .beacon { fill: #ff4d4d; filter: drop-shadow(0 0 6px #ff4d4d); }
.opening { stroke: var(--ink); stroke-width: 2; }
.dock.done .opening { fill: url(#opening); }
.shutter { fill: url(#slats); }
.sub-plate { fill: rgba(14,17,23,.86); }
.dock text { fill: #f3f6fb; }
.dock-name { font: 700 18px var(--display); }
.dock-title { font-size: 12px; font-weight: 800; text-anchor: end; }
.dock.done .dock-title { fill: #9ff0c6; }
.dock.stop .dock-title { fill: #ffb3b3; }
.dock-sub { font-size: 10px; font-weight: bold; text-anchor: middle; }
.dock.done .dock-sub { fill: #c4f7dc; }
.dock.human .dock-title { text-anchor: start; fill: #d3dbe7; }
.awning { fill: url(#awning); stroke: var(--ink); stroke-width: 3; }
.window { fill: url(#glass); stroke: var(--ink); stroke-width: 3; }
.person { fill: none; stroke: var(--ink); stroke-width: 2.6; stroke-linecap: round; }
.call { fill: url(#call); stroke: var(--ink); stroke-width: 3; filter: drop-shadow(0 0 7px rgba(255,111,174,.6)); }
.bell { fill: none; stroke: var(--ink); stroke-width: 2.2; stroke-linejoin: round; stroke-linecap: round; }
"#;

const LEGEND: &str = r#"<section class="legend"><h2>見かた</h2><ul>
<li><span class="chip" style="--a:#ffc21a">›››</span>ベルトコンベア: cellの <code>next</code>。山形の印と、ローラーの流れる向きに進む。同じ先へ行くベルトは、途中で1本に合流する</li>
<li><span class="chip" style="--a:#ff7a1a">›››</span>オレンジの印: <code>[product.next]</code> で、このラインだけ進め先を替えたもの</li>
<li><span class="chip" style="--a:#b18cff">›››</span>紫の印（出ていくところに鍵）: 着手を確かめるcell（<code>confirm</code>）が、人間に認められたあとにだけ進める先</li>
<li><b>橋</b>: 交わるだけのベルトは、橋で上を越す（合流とは見分ける）</li>
<li><span class="dot" style="--a:#39e58c"></span><b>信号灯</b>: 緑はふつう。黄と <b>⚠</b> は、想定通りに流れないかもしれないcell。上の「注意」に理由がある</li>
<li><b>電光掲示板</b>: cellの <code>description</code>（何をするcellか）を流す。書いていなければ灯らない</li>
<li><b>入口</b>: Issueを扱い、ほかのcellの <code>next</code> から来ないcell。人がラベルを付けて始める</li>
<li><span class="dot" style="--a:#ff6fae"></span><b>needs-human</b>（人間の窓口）: どの機械からでも、人間に回せる（線は省いている）</li>
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
description = "Issueを  仕分ける\n<急ぎ>"
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
        assert_eq!(html.matches("class=\"dock stop\"").count(), 2);
        assert_eq!(html.matches("class=\"dock done\"").count(), 2);
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

    #[test]
    fn descriptions_run_on_the_ticker() {
        let html = html();
        // 説明は1行にまとめ、エスケープして流す。書いていないcellの板は灯らない
        assert!(html.contains("Issueを 仕分ける &lt;急ぎ&gt;</text>"));
        assert_eq!(html.matches("<svg class=\"ticker\"").count(), 4);
        assert_eq!(html.matches("mask=\"url(#led)\"").count(), 1);
        // 流す距離は、板の幅と字の幅
        let inner = MACHINE_W - 26 - 6;
        let len = text_width("Issueを 仕分ける <急ぎ>");
        assert_eq!(
            len,
            (5 * 6 + 9 + 6 + 4 * 9 + 6 + 6 + 2 * 9 + 6) * TICKER_FONT / 10
        );
        assert_eq!(text_width("a→ｱ"), 3 * 6 * TICKER_FONT / 10);
        assert!(html.contains(&format!(
            "textLength=\"{len}\" style=\"--d:-{}px;",
            inner + len
        )));
    }

    /// 機械や出口の箱の中（ふちは除く）を、線分が通るか。
    fn crosses(a: (i64, i64), b: (i64, i64), p: &Place, depth: i64) -> bool {
        let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
        let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
        x0 < p.x + p.w && x1 > p.x && y0 < p.y + p.h + depth && y1 > p.y
    }

    fn layouts(text: &str) -> Vec<Layout> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("design.md"), "設計を書く").unwrap();
        let config = Config::parse(text, dir.path()).unwrap();
        flow::read(&config).lines.iter().map(layout).collect()
    }

    /// 入口が3つ縦に並び、差し戻しや、列をとばすベルトもある設定。
    fn crowded() -> String {
        CONFIG
            .replace(
                "cells = [\"triage\", \"implement\", \"review\"]",
                "cells = [\"triage\", \"implement\", \"review\", \"assess\", \"design\"]",
            )
            .replace(
                "next = [\"implement\", \"done\"]\nmodel",
                "next = [\"implement\", \"stop\", \"done\"]\nmodel",
            )
            .replace(
                "next = [\"stop\", \"done\"]",
                "next = [\"implement\", \"stop\", \"done\"]\nconfirm = true",
            )
            .replace(
                "cells = [\"assess\"]",
                "cells = [\"assess\"]\n\n[product.next]\nassess = [\"stop\", \"done\"]",
            )
            + "\n[[cell]]\nname = \"design\"\non = \"issue\"\nlabel = \"ac:design\"\nnext = [\"implement\", \"done\"]\nconfirm = true\nprompt = \"design.md\"\n"
    }

    #[test]
    fn belts_turn_at_right_angles_and_go_around_the_boxes() {
        for text in [CONFIG.to_string(), crowded()] {
            for layout in layouts(&text) {
                for belt in &layout.belts {
                    let name = format!("{} → {}", belt.from, belt.to);
                    assert!(belt.points.len() >= 2, "{name}");
                    for pair in belt.points.windows(2) {
                        let (a, b) = (pair[0], pair[1]);
                        assert!(a.0 == b.0 || a.1 == b.1, "{name}: 斜め {a:?} {b:?}");
                        for (box_name, place) in &layout.places {
                            assert!(
                                !crosses(a, b, place, DEPTH),
                                "{name}: {box_name} を通る {a:?} {b:?}"
                            );
                        }
                        assert!(!crosses(a, b, &layout.booth, DEPTH), "{name}: 窓口を通る");
                    }
                }
            }
        }
    }

    #[test]
    fn belts_between_neighbors_on_the_same_row_run_straight() {
        for layout in layouts(&crowded()) {
            for belt in &layout.belts {
                let (f, t) = (&layout.places[&belt.from], &layout.places[&belt.to]);
                if t.col == f.col + 1 && f.mid_y() == t.mid_y() {
                    assert_eq!(belt.points.len(), 2, "{} → {}", belt.from, belt.to);
                }
            }
        }
    }

    #[test]
    fn stop_stands_right_of_done_and_the_booth_below_everything() {
        for layout in layouts(&crowded()) {
            let done = layout.places[DONE];
            if layout.uses_stop {
                assert!(layout.places[STOP].x >= done.right());
            }
            let lowest = layout
                .belts
                .iter()
                .flat_map(|b| b.points.iter().map(|p| p.1))
                .chain(layout.places.values().map(|p| p.y + p.h + DEPTH))
                .max()
                .unwrap();
            assert!(layout.booth.y > lowest);
            assert!(layout.booth.y + layout.booth.h < layout.height);
        }
    }

    /// 道の決まりを確かめる。斜めがない、箱（機械の上にはみ出す煙突と信号灯、横の口も含めて）を通らない
    /// （ベルトの太さも入れる）、入る面の向きに入る、別の先へ行くベルトどうしが並んで重ならない。
    fn check_routes(layout: &Layout) -> Result<(), String> {
        const HALF: i64 = 14;
        for belt in &layout.belts {
            let name = format!("{} → {}", belt.from, belt.to);
            let points = &belt.points;
            if points.len() < 2 {
                return Err(format!("{name}: 点が足りない"));
            }
            for (i, pair) in points.windows(2).enumerate() {
                let (a, b) = (pair[0], pair[1]);
                if a.0 != b.0 && a.1 != b.1 {
                    return Err(format!("{name}: 斜め {a:?} {b:?}"));
                }
                let boxes = layout
                    .places
                    .iter()
                    .map(|(n, p)| (n.as_str(), *p))
                    .chain([("needs-human", layout.booth)]);
                for (box_name, p) in boxes {
                    // 出る最初の線と元の箱、入る最後の線と先の箱は、つながっているので除く
                    if (i == 0 && box_name == belt.from)
                        || (i + 2 == points.len() && box_name == belt.to)
                    {
                        continue;
                    }
                    let (x0, x1) = (a.0.min(b.0) - HALF, a.0.max(b.0) + HALF);
                    let (y0, y1) = (a.1.min(b.1) - HALF, a.1.max(b.1) + HALF);
                    // 上にはみ出すのは、機械は煙突と信号灯、出口は回転灯、窓口はひさし
                    let roof = match box_name {
                        DONE | STOP => 18,
                        "needs-human" => 8,
                        _ => ROOF,
                    };
                    if x0 < p.x + p.w + 9
                        && x1 > p.x - 9
                        && y0 < p.y + p.h + DEPTH
                        && y1 > p.y - roof
                    {
                        return Err(format!("{name}: {box_name} を通る {a:?} {b:?}"));
                    }
                }
            }
            let (before, end) = (points[points.len() - 2], points[points.len() - 1]);
            let t = layout.places[&belt.to];
            let inward = if end.0 < t.x {
                end.0 > before.0
            } else {
                end.0 < before.0
            };
            if before.1 != end.1 || !inward {
                return Err(format!("{name}: 入る向きが逆 {before:?} {end:?}"));
            }
        }
        for (i, a) in layout.belts.iter().enumerate() {
            for b in &layout.belts[i + 1..] {
                if a.to == b.to {
                    continue;
                }
                for pa in a.points.windows(2) {
                    for pb in b.points.windows(2) {
                        if let Some(why) = side_by_side(pa, pb) {
                            return Err(format!(
                                "{} → {} と {} → {} が重なる: {why}",
                                a.from, a.to, b.from, b.to
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 平行な2本の線が、見分けられないほど近くに並ぶか（ベルトの手すりの太さ18より近い。縁と手すりが重なって、
    /// 1本の太いベルトに見える）。
    fn side_by_side(a: &[(i64, i64)], b: &[(i64, i64)]) -> Option<String> {
        let horizontal = |s: &[(i64, i64)]| s[0].1 == s[1].1;
        if horizontal(a) != horizontal(b) {
            return None;
        }
        let flip = !horizontal(a);
        let along = |p: (i64, i64)| if flip { p.1 } else { p.0 };
        let across = |p: (i64, i64)| if flip { p.0 } else { p.1 };
        let (a0, a1) = (along(a[0]).min(along(a[1])), along(a[0]).max(along(a[1])));
        let (b0, b1) = (along(b[0]).min(along(b[1])), along(b[0]).max(along(b[1])));
        let overlap = a1.min(b1) - a0.max(b0);
        ((across(a[0]) - across(b[0])).abs() < 18 && overlap > 14).then(|| format!("{a:?} {b:?}"))
    }

    /// でたらめな設定（cellが2〜7個、next が1〜4個、done と stop も混ぜる）。
    fn random_config(seed: u64) -> String {
        let mut state = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut next = |n: u64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) % n
        };
        let count = 2 + next(6) as usize;
        let names: Vec<String> = (0..count).map(|i| format!("c{i}")).collect();
        let mut text = format!(
            "operator = \"me\"\n\n[[product]]\nname = \"p\"\nissues = \"o/r\"\nrepos = [\"o/r\"]\ncells = {names:?}\n"
        );
        for name in &names {
            let mut targets: Vec<String> = Vec::new();
            for _ in 0..1 + next(4) {
                let target = match next(count as u64 + 2) as usize {
                    i if i < count => names[i].clone(),
                    i if i == count => DONE.to_string(),
                    _ => STOP.to_string(),
                };
                if target != *name && !targets.contains(&target) {
                    targets.push(target);
                }
            }
            if targets.is_empty() {
                targets.push(DONE.to_string());
            }
            let confirm = next(4) == 0;
            text.push_str(&format!(
                "\n[[cell]]\nname = \"{name}\"\non = \"issue\"\nlabel = \"ac:{name}\"\nnext = {targets:?}\nconfirm = {confirm}\nprompt = \"design.md\"\n"
            ));
        }
        text
    }

    #[test]
    fn random_configs_keep_to_the_routing_rules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("design.md"), "設計を書く").unwrap();
        let mut checked = 0;
        for seed in 0..1500 {
            let text = random_config(seed);
            // 設定の確かめで断られるもの（承認のあとの先がない、など）は飛ばす
            let Ok(config) = Config::parse(&text, dir.path()) else {
                continue;
            };
            for line in &flow::read(&config).lines {
                if let Err(why) = check_routes(&layout(line)) {
                    panic!("seed {seed}: {why}\n{text}");
                }
                checked += 1;
            }
        }
        assert!(checked > 500, "{checked}");
    }

    #[test]
    fn every_crossing_gets_a_bridge() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("design.md"), "設計を書く").unwrap();
        let texts = (0..1500)
            .map(random_config)
            .chain([CONFIG.to_string(), crowded()]);
        for text in texts {
            let Ok(config) = Config::parse(&text, dir.path()) else {
                continue;
            };
            for line in &flow::read(&config).lines {
                let layout = layout(line);
                let bridges = crossings(&layout.belts);
                // どちらの線でも端でないところで、横と縦の線が交わる点
                for a in &layout.belts {
                    for b in &layout.belts {
                        for h in a.points.windows(2).filter(|s| s[0].1 == s[1].1) {
                            for v in b.points.windows(2).filter(|s| s[0].0 == s[1].0) {
                                let (x, y) = (v[0].0, h[0].1);
                                let inside = h[0].0.min(h[1].0) < x
                                    && x < h[0].0.max(h[1].0)
                                    && v[0].1.min(v[1].1) < y
                                    && y < v[0].1.max(v[1].1);
                                assert!(
                                    !inside || bridges.contains(&(x, y)),
                                    "({x}, {y}) に橋がない\n{text}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn crossed_entries_do_not_share_a_line() {
        // 入口が2つあり、たすきがけに進む（triage → design、assess → implement）
        let text = CONFIG
            .replace(
                "cells = [\"triage\", \"implement\", \"review\"]",
                "cells = [\"triage\", \"assess\", \"implement\", \"design\", \"review\"]",
            )
            .replace(
                "next = [\"implement\", \"done\"]\nmodel",
                "next = [\"design\"]\nmodel",
            )
            .replace(
                "cells = [\"assess\"]",
                "cells = [\"triage\"]\n\n[product.next]\ntriage = [\"done\"]",
            )
            .replace("next = [\"stop\", \"done\"]", "next = [\"implement\"]")
            + "\n[[cell]]\nname = \"design\"\non = \"issue\"\nlabel = \"ac:design\"\nnext = [\"review\"]\nprompt = \"design.md\"\n";
        for layout in layouts(&text) {
            check_routes(&layout).unwrap();
        }
    }

    #[test]
    fn corners_are_rounded() {
        let d = rounded(&[(0, 0), (100, 0), (100, 50), (40, 50)]);
        assert_eq!(d, "M0,0 L86,0 Q100,0 100,14 L100,36 Q100,50 86,50 L40,50");
        // 短い折れ目では、丸みを小さくする
        assert_eq!(
            rounded(&[(0, 0), (10, 0), (10, 8)]),
            "M0,0 L6,0 Q10,0 10,4 L10,8"
        );
        assert_eq!(
            simplify(vec![(0, 0), (5, 0), (5, 0), (9, 0), (9, 3)]),
            [(0, 0), (9, 0), (9, 3)]
        );
    }

    fn belt(points: &[(i64, i64)], to: &str) -> Belt {
        Belt {
            from: "a".to_string(),
            to: to.to_string(),
            class: "belt",
            note: None,
            points: points.to_vec(),
        }
    }

    #[test]
    fn belts_that_only_cross_get_a_bridge_and_merging_ones_do_not() {
        let across = belt(&[(0, 100), (300, 100)], "x");
        let up = belt(&[(150, 200), (150, 0), (400, 0)], "y");
        assert_eq!(crossings(&[across, up]), [(150, 100)]);
        // 横の線に入って合流するもの（角が線の上）と、線の端に近すぎるものには架けない
        let main = belt(&[(0, 100), (300, 100)], "x");
        let join = belt(&[(150, 200), (150, 100), (300, 100)], "x");
        let near_end = belt(&[(20, 200), (20, 0)], "y");
        assert!(crossings(&[main, join, near_end]).is_empty());
    }

    #[test]
    fn chevrons_of_merged_belts_sit_on_the_same_points() {
        let a = belt(&[(0, 100), (400, 100)], "x");
        let b = belt(&[(100, 300), (100, 100), (400, 100)], "x");
        let marks = chevrons(&[a, b], &[(280, 100)]);
        let on_line: Vec<i64> = marks.iter().filter(|m| m.2 == 100).map(|m| m.1).collect();
        // 同じ線の上では1つずつ、決まった間隔で、避ける点の近くは除く
        assert_eq!(on_line, [70, 140, 210, 350]);
        assert!(
            marks
                .iter()
                .any(|m| m.0 == Some(1) && m.1 == 100 && m.3 == 270)
        );
        // 種類の違うベルトが合流したところは、ふつうの色（None）にする
        let a = belt(&[(0, 100), (400, 100)], "x");
        let mut b = belt(&[(100, 300), (100, 100), (400, 100)], "x");
        b.class = "belt replaced";
        let marks = chevrons(&[a, b], &[]);
        assert!(
            marks
                .iter()
                .filter(|m| m.2 == 100 && m.1 > 100)
                .all(|m| m.0.is_none())
        );
        assert!(
            marks
                .iter()
                .filter(|m| m.2 == 100 && m.1 < 100)
                .all(|m| m.0 == Some(0))
        );
    }

    #[test]
    fn crowded_lines_draw_bridges_and_flanges() {
        let config = {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("design.md"), "設計を書く").unwrap();
            Config::parse(&crowded(), dir.path()).unwrap()
        };
        let html = render(&flow::read(&config));
        assert!(html.contains("class=\"belts bridge\""));
        assert!(html.contains("class=\"flange\""));
        assert!(html.contains("class=\"intake\""));
        assert!(html.contains("class=\"badge entry\""));
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
