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

const MACHINE_W: i64 = 220;
const MACHINE_H: i64 = 196;
/// 機械や出口の、手前の面の厚さ（立体に見せる）
const DEPTH: i64 = 10;
const EXIT_W: i64 = 170;
const EXIT_H: i64 = 100;
/// 列の間のすき間の、いちばん狭い幅。ベルトは、ここと、機械の上下の通路だけを通る。
/// 通るベルトが多いすき間は、そのぶん広げる（左の端にも、1列目へ入るベルトのためのすき間を取る）
const GAP: i64 = 130;
/// すき間の両端に残す幅（ベルトの太さの半分と、機械の印のはみ出し）
const GAP_MARGIN: i64 = 36;
const ROW_H: i64 = MACHINE_H + 70;
/// 上や下を回るベルトの、1本ごとの間（ベルトの太さより広くして、並んでも重ならないようにする）
const LANE: i64 = 24;
/// すき間を縦に通るベルトの、1本ごとの間
const CHANNEL: i64 = 24;
/// 機械の横の面につながるベルトの、1本ごとの間（口が多いときは、面に収まるように詰める）
const PORT: i64 = 24;
/// ベルトの角の丸み
const RADIUS: i64 = 14;

/// 機械の色の帯（ラインの中の順に回す）。
const COLORS: &[&str] = &[
    "#ffb547", "#5fd3d8", "#ff7aa8", "#9be36b", "#c3a6ff", "#ffe066", "#7cb8ff",
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

    let top = 80 + over_lanes.len() as i64 * LANE;
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
    let over_y = |lane: i64| top - 50 - lane * LANE;
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
            // 先の手前のすき間まで横に進むので、その左の列の機械からも離す
            .filter(|p| (lo - 1..=hi).contains(&p.col))
            .map(|p| p.y + p.h + DEPTH)
            .max()
            .unwrap_or(machines_bottom);
        bottom + 36 + under_lanes[to] * LANE
    };
    let lowest = under_lanes
        .keys()
        .map(|to| under_y(to) + LANE)
        .fold(machines_bottom, i64::max);
    let booth = Place {
        x: gap_w(-1),
        y: lowest + 20,
        w: 330,
        h: 56,
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

    for belt in &layout.belts {
        render_belt(html, belt);
    }
    // 承認のあとにだけ進むベルトには、出ていくところの線の上に鍵の印を付ける。合流すると色で見分けられないため。
    // ほかのベルトの下に隠れないよう、全部のベルトのあとに描く
    for belt in &layout.belts {
        if let Some(note) = belt.note {
            let (x, y) = (belt.points[0].0 + 16, belt.points[0].1);
            let _ = write!(
                html,
                "<g class=\"lock\"><title>{note}</title><circle cx=\"{x}\" cy=\"{y}\" r=\"11\"/><path class=\"shackle\" d=\"M{},{} v-3 a3,3 0 0 1 6,0 v3\"/><rect x=\"{}\" y=\"{}\" width=\"10\" height=\"8\" rx=\"2\"/></g>",
                x - 3,
                y - 1,
                x - 5,
                y - 1
            );
        }
    }
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
        "dock done",
        DONE,
        "出荷",
        "Issueは閉じる・PRは検品待ち",
    );
    if layout.uses_stop {
        render_dock(
            html,
            layout.places[STOP],
            "dock stop",
            STOP,
            "ラインから下ろす",
            "閉じずに人間が進める",
        );
    }
    render_dock(
        html,
        layout.booth,
        "dock human",
        "needs-human",
        "人間の窓口",
        "どの機械からでも回せる",
    );
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
    let d = rounded(&belt.points);
    let _ = write!(
        html,
        "<g class=\"{}\" data-from=\"{}\" data-to=\"{}\"><path class=\"edge\" d=\"{d}\"/><path class=\"base\" d=\"{d}\"/><path class=\"rollers\" d=\"{d}\"/>",
        belt.class,
        escape(&belt.from),
        escape(&belt.to)
    );
    // 向きを示す山形の印を、まっすぐなところに並べる
    for pair in belt.points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let len = (b.0 - a.0).abs() + (b.1 - a.1).abs();
        if len < 60 {
            continue;
        }
        let angle = match ((b.0 - a.0).signum(), (b.1 - a.1).signum()) {
            (1, _) => 0,
            (-1, _) => 180,
            (_, 1) => 90,
            _ => 270,
        };
        let count = (len / 120).max(1);
        for k in 0..count {
            let t = (2 * k + 1) * 1000 / (2 * count);
            let x = a.0 + (b.0 - a.0) * t / 1000;
            let y = a.1 + (b.1 - a.1) * t / 1000;
            let _ = write!(
                html,
                "<path class=\"chevron\" d=\"M-4,-6 L3,0 L-4,6\" transform=\"translate({x},{y}) rotate({angle})\"/>"
            );
        }
    }
    let end = belt.points[belt.points.len() - 1];
    let before = belt.points[belt.points.len() - 2];
    let angle = match ((end.0 - before.0).signum(), (end.1 - before.1).signum()) {
        (1, _) => 0,
        (-1, _) => 180,
        (_, 1) => 90,
        _ => 270,
    };
    let _ = write!(
        html,
        "<path class=\"head\" d=\"M-6,-9 L6,0 L-6,9 z\" transform=\"translate({},{}) rotate({angle})\"/>",
        end.0, end.1
    );
    html.push_str("</g>\n");
}

/// 箱を立体に見せる（手前の面と、上の面）。
fn block(html: &mut String, p: Place, class: &str) {
    let _ = write!(
        html,
        "<rect class=\"side\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"14\"/><rect class=\"{class}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"14\"/>",
        p.x,
        p.y + DEPTH,
        p.w,
        p.h,
        p.x,
        p.y,
        p.w,
        p.h
    );
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
    // 排気口と煙
    let vx = p.x + p.w - 50;
    let _ = write!(
        html,
        "<rect class=\"vent\" x=\"{vx}\" y=\"{}\" width=\"26\" height=\"26\" rx=\"5\"/><circle class=\"smoke\" cx=\"{}\" cy=\"{}\" r=\"8\"/><circle class=\"smoke s2\" cx=\"{}\" cy=\"{}\" r=\"6\"/>",
        p.y - 20,
        vx + 13,
        p.y - 28,
        vx + 18,
        p.y - 38
    );
    block(html, p, "top");
    // 手前の面のオレンジの帯
    let _ = write!(
        html,
        "<rect class=\"stripe\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"5\" rx=\"2\"/>",
        p.x + 18,
        p.y + p.h + 2,
        p.w - 36
    );
    // cellの色の帯と、歯車と名前
    let _ = write!(
        html,
        "<rect class=\"band\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"32\" rx=\"9\"/><circle class=\"gear\" cx=\"{}\" cy=\"{}\" r=\"9\"/><text class=\"name\" x=\"{}\" y=\"{}\">{}</text>",
        p.x + 9,
        p.y + 9,
        p.w - 18,
        p.x + 28,
        p.y + 25,
        p.x + 46,
        p.y + 31,
        escape(&cell.name)
    );
    let _ = write!(
        html,
        "<rect class=\"panel\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"8\"/>",
        p.x + 12,
        p.y + 50,
        p.w - 24,
        p.h - 62
    );
    for (i, (key, value)) in rows.iter().enumerate() {
        let y = p.y + 68 + i as i64 * 18;
        let _ = write!(
            html,
            "<text class=\"key\" x=\"{}\" y=\"{y}\">{key}</text><text class=\"value\" x=\"{}\" y=\"{y}\">{}</text>",
            p.x + 22,
            p.x + 108,
            escape(value)
        );
    }
    if s.entry {
        let _ = write!(
            html,
            "<g class=\"badge entry\"><rect x=\"{}\" y=\"{}\" width=\"58\" height=\"22\" rx=\"11\"/><text x=\"{}\" y=\"{}\">入口</text></g>",
            p.x - 8,
            p.y - 16,
            p.x + 21,
            p.y
        );
    }
    if let Some(original) = &s.replaced {
        let _ = write!(
            html,
            "<g class=\"badge replaced\"><title>[product.next] でこのラインだけ進め先を替えています（元の next: [{}]）</title><rect x=\"{}\" y=\"{}\" width=\"118\" height=\"22\" rx=\"11\"/><text x=\"{}\" y=\"{}\">next を替えた</text></g>",
            escape(&original.join(", ")),
            p.x + 56,
            p.y - 16,
            p.x + 115,
            p.y
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

/// 出口（done、stop）と人間の窓口。下の端に、黄色と黒の縞を付ける。
fn render_dock(html: &mut String, p: Place, class: &str, name: &str, title: &str, sub: &str) {
    let cx = p.x + p.w / 2;
    let _ = write!(html, "<g class=\"{class}\">");
    block(html, p, "top");
    if p.h >= EXIT_H {
        let _ = write!(
            html,
            "<rect class=\"hazard\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"10\" rx=\"3\"/><text class=\"dock-name\" x=\"{cx}\" y=\"{}\">{name}</text><text class=\"dock-title\" x=\"{cx}\" y=\"{}\">{title}</text><text class=\"dock-sub\" x=\"{cx}\" y=\"{}\">{sub}</text>",
            p.x + 10,
            p.y + p.h - 18,
            p.w - 20,
            p.y + 30,
            p.y + 50,
            p.y + 68
        );
    } else {
        let _ = write!(
            html,
            "<text class=\"dock-name left\" x=\"{}\" y=\"{}\">{name}</text><text class=\"dock-title left\" x=\"{}\" y=\"{}\">{title}・{sub}</text>",
            p.x + 16,
            p.y + 24,
            p.x + 16,
            p.y + 44
        );
    }
    html.push_str("</g>\n");
}

/// 縞の模様。
const DEFS: &str = "<defs><pattern id=\"hazard\" width=\"14\" height=\"14\" patternUnits=\"userSpaceOnUse\" patternTransform=\"rotate(45)\"><rect width=\"14\" height=\"14\" fill=\"#ffc94a\"/><rect width=\"7\" height=\"14\" fill=\"#1b1d33\"/></pattern></defs>\n";

const STYLE: &str = r#"
:root { --ink: #1b1d33; --floor: #2a2d4a; --accent: #f39c38; }
* { box-sizing: border-box; }
body { margin: 0; padding: 24px 32px 48px; font-family: system-ui, -apple-system, "Hiragino Sans", "Noto Sans JP", sans-serif; color: var(--ink);
  background: #eceef6; background-image: radial-gradient(#d9dcee 2px, transparent 2px); background-size: 22px 22px; }
header { display: flex; align-items: baseline; gap: 20px; flex-wrap: wrap; }
h1 { margin: 0 0 12px; font-size: 32px; letter-spacing: 1px; color: var(--floor); text-shadow: 3px 3px 0 var(--accent); }
.summary { margin: 0; font-weight: bold; color: #4a4f7a; }
h2 { margin: 0 0 6px; font-size: 22px; }
code { background: #f3f4fb; border: 2px solid #c9cce6; border-radius: 6px; padding: 0 6px; }
.board { background: #fff; border: 4px solid var(--accent); border-radius: 18px; padding: 14px 20px; margin: 12px 0 24px; box-shadow: 6px 6px 0 var(--accent); }
.board.ok { border-color: #3fae7a; box-shadow: 6px 6px 0 #3fae7a; }
.board ul, .notes { margin: 6px 0 0; padding-left: 20px; }
.line { background: #fff; border: 4px solid var(--floor); border-radius: 22px; padding: 16px 20px; margin: 0 0 28px; box-shadow: 8px 8px 0 var(--floor); }
.sign { display: inline-block; background: var(--accent); color: var(--ink); border-radius: 8px; padding: 0 8px; font-size: 14px; vertical-align: middle; }
.repos { margin: 0 0 10px; }
.floor { overflow-x: auto; border-radius: 16px; background: var(--floor);
  background-image: linear-gradient(#33375a 2px, transparent 2px), linear-gradient(90deg, #33375a 2px, transparent 2px); background-size: 40px 40px; }
.notes li { color: #b45309; font-weight: bold; }
.empty { font-size: 18px; }
.warehouse { background: #fff; border: 4px dashed #6b6fb5; border-radius: 18px; padding: 14px 20px; margin: 0 0 24px; }
.crates { display: flex; gap: 12px; flex-wrap: wrap; }
.crate { background: #c98a3c; color: #fff; font-weight: bold; padding: 10px 16px; border-radius: 6px; border: 3px solid #7a4f1c; }
.legend { background: #fff; border: 4px solid #6b6fb5; border-radius: 18px; padding: 12px 20px; }
.legend li { margin: 4px 0; }
.chip { display: inline-block; background: #2e3150; color: var(--a); font-weight: 900; letter-spacing: -2px; border-radius: 6px; padding: 0 8px 0 6px; margin-right: 6px; border: 2px solid var(--ink); }

svg text { font-family: inherit; }
.belt { --a: #ffc94a; }
.belt.replaced { --a: #ff8f3f; }
.belt.gated { --a: #b9a2ff; }
.belt path.edge, .belt path.base, .belt path.rollers { fill: none; stroke-linecap: butt; stroke-linejoin: round; }
.belt .edge { stroke: var(--ink); stroke-width: 20; }
.belt .base { stroke: #3a3e66; stroke-width: 14; }
.belt .rollers { stroke: #50568a; stroke-width: 12; stroke-dasharray: 3 7; animation: run 0.7s linear infinite; }
.belt.gated .rollers { animation-duration: 2s; }
.belt .chevron { fill: none; stroke: var(--a); stroke-width: 3; stroke-linecap: round; stroke-linejoin: round; }
.belt .head { fill: var(--a); stroke: var(--ink); stroke-width: 2; stroke-linejoin: round; }
.lock circle { fill: #b9a2ff; stroke: var(--ink); stroke-width: 2; }
.lock rect { fill: var(--ink); }
.lock .shackle { fill: none; stroke: var(--ink); stroke-width: 2; }
@keyframes run { to { stroke-dashoffset: -10; } }

.side { fill: #34376a; stroke: var(--ink); stroke-width: 3; }
.machine .top { fill: #5c60ad; stroke: var(--ink); stroke-width: 3; }
.machine .stripe { fill: var(--accent); }
.machine .band { fill: var(--c); stroke: var(--ink); stroke-width: 2; }
.machine .panel { fill: #1e2038; stroke: var(--ink); stroke-width: 2; }
.machine .vent { fill: #4b4f8a; stroke: var(--ink); stroke-width: 3; }
.machine .smoke { fill: #e7e9f7; stroke: #b9bde0; stroke-width: 2; animation: puff 2.4s ease-out infinite; }
.machine .smoke.s2 { animation-delay: 1.2s; }
@keyframes puff { from { opacity: 0.9; transform: translateY(0); } to { opacity: 0; transform: translateY(-22px); } }
.machine .gear { fill: #fff; stroke: var(--ink); stroke-width: 4; stroke-dasharray: 4 3; transform-box: fill-box; transform-origin: center; animation: spin 4s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
.machine .name { font-size: 17px; font-weight: 800; fill: var(--ink); }
.machine .key { font-size: 12px; fill: #9ea3d8; }
.machine .value { font-size: 12px; font-weight: bold; fill: #f1f2ff; }
.machine.warn .top { stroke: #ff5c5c; stroke-width: 4; stroke-dasharray: 10 5; }
.alert polygon { fill: #ffc94a; stroke: var(--ink); stroke-width: 3; stroke-linejoin: round; }
.alert text { font-size: 20px; font-weight: 900; text-anchor: middle; fill: var(--ink); }
.alert { animation: blink 1.2s steps(2) infinite; }
@keyframes blink { 50% { opacity: 0.45; } }
.badge rect { stroke: var(--ink); stroke-width: 2; }
.badge text { font-size: 12px; font-weight: bold; text-anchor: middle; fill: var(--ink); }
.badge.entry rect { fill: #7be0a6; }
.badge.replaced rect { fill: #ff8f3f; }

.dock .top { stroke: var(--ink); stroke-width: 3; }
.dock.done .top { fill: #3f9e74; }
.dock.stop .top { fill: #6b7090; }
.dock.human .top { fill: #d9628f; }
.dock .hazard { fill: url(#hazard); stroke: var(--ink); stroke-width: 2; }
.dock text { text-anchor: middle; fill: #fff; }
.dock text.left { text-anchor: start; }
.dock-name { font-size: 18px; font-weight: 800; }
.dock-title { font-size: 13px; font-weight: bold; }
.dock-sub { font-size: 11px; }
"#;

const LEGEND: &str = r#"<section class="legend"><h2>見かた</h2><ul>
<li><span class="chip" style="--a:#ffc94a">›››</span>ベルトコンベア: cellの <code>next</code>。山形の印と矢印の向きに進む</li>
<li><span class="chip" style="--a:#ff8f3f">›››</span>オレンジの印: <code>[product.next]</code> で、このラインだけ進め先を替えたもの</li>
<li><span class="chip" style="--a:#b9a2ff">›››</span>紫の印（出ていくところに鍵）: 着手を確かめるcell（<code>confirm</code>）が、人間に認められたあとにだけ進める先</li>
<li><b>needs-human</b>（ピンクの窓口）: どの機械からでも、人間に回せる（線は省いている）</li>
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

    /// 道の決まりを確かめる。斜めがない、箱（機械の上にはみ出す印も含めて）を通らない（ベルトの太さも入れる）、
    /// 入る面の向きに入る、別の先へ行くベルトどうしが並んで重ならない。
    fn check_routes(layout: &Layout) -> Result<(), String> {
        const HALF: i64 = 9;
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
                    if x0 < p.x + p.w && x1 > p.x - 8 && y0 < p.y + p.h + DEPTH && y1 > p.y - 22 {
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

    /// 平行な2本の線が、見分けられないほど近くに並ぶか（ベルトの中の帯の太さ14より近い）。
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
        ((across(a[0]) - across(b[0])).abs() < 14 && overlap > 14).then(|| format!("{a:?} {b:?}"))
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
