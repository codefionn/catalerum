//! XY charts (`xychart-beta`) → SVG: a bar/line chart over a categorical x-axis.
//! Supports `title`, `x-axis [cat, cat, …]` category labels, `y-axis "label" min
//! --> max` (label + optional explicit range, else auto from the data), and any
//! number of `bar [n, n, …]` / `line [n, n, …]` series. Bars from multiple series
//! group side-by-side within each x slot; lines are polylines through the points.
//! All text is escaped.

use super::{MermaidError, PALETTE};
use crate::escape::escape_text;

const MARGIN: f64 = 16.0;
const PLOT_W: f64 = 440.0;
const PLOT_H: f64 = 240.0;
const GUTTER_L: f64 = 48.0; // y-axis labels
const GUTTER_B: f64 = 34.0; // x-axis labels

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let m = parse(src);
    // Need at least one non-empty series to plot.
    if m.series.iter().all(|s| s.values().is_empty()) {
        return Err(MermaidError("no xychart series"));
    }
    Ok(render(&m))
}

// ---- model ---------------------------------------------------------------------

enum Series {
    Bar(Vec<f64>),
    Line(Vec<f64>),
}

impl Series {
    fn values(&self) -> &[f64] {
        match self {
            Series::Bar(v) | Series::Line(v) => v,
        }
    }
}

#[derive(Default)]
struct Model {
    title: Option<String>,
    categories: Vec<String>,
    y_label: Option<String>,
    y_min: Option<f64>,
    y_max: Option<f64>,
    series: Vec<Series>,
}

fn parse(src: &str) -> Model {
    let mut m = Model::default();
    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    lines.next(); // header (`xychart-beta`)

    for line in lines {
        if let Some(t) = line.strip_prefix("title") {
            let t = unquote(t.trim());
            if !t.is_empty() {
                m.title = Some(t.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("x-axis") {
            // `x-axis [a, b, c]` (categories) — a numeric `min --> max` form has no
            // bracket and is ignored (points fall back to their index labels).
            if rest.contains('[') {
                m.categories = parse_str_list(rest);
            }
        } else if let Some(rest) = line.strip_prefix("y-axis") {
            parse_y_axis(rest.trim(), &mut m);
        } else if let Some(rest) = line.strip_prefix("bar") {
            let v = parse_num_list(rest);
            if !v.is_empty() {
                m.series.push(Series::Bar(v));
            }
        } else if let Some(rest) = line.strip_prefix("line") {
            let v = parse_num_list(rest);
            if !v.is_empty() {
                m.series.push(Series::Line(v));
            }
        }
    }
    m
}

/// `y-axis "label" 4000 --> 11000` — pull an optional quoted label and an optional
/// `lo --> hi` range (either part may be absent).
fn parse_y_axis(rest: &str, m: &mut Model) {
    let after_label = if let Some(stripped) = rest.strip_prefix('"') {
        if let Some((label, tail)) = stripped.split_once('"') {
            let label = label.trim();
            if !label.is_empty() {
                m.y_label = Some(label.to_string());
            }
            tail.trim()
        } else {
            rest
        }
    } else {
        rest
    };
    if let Some((lo, hi)) = after_label.split_once("-->") {
        m.y_min = lo.trim().parse().ok();
        m.y_max = hi.trim().parse().ok();
    }
}

/// Strip surrounding `[ ]` and split on commas, trimming + unquoting each item.
fn parse_str_list(s: &str) -> Vec<String> {
    bracket_inner(s)
        .split(',')
        .map(|p| unquote(p.trim()).to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Strip `[ ]` and parse each comma-separated item as a number (non-numbers skipped).
fn parse_num_list(s: &str) -> Vec<f64> {
    bracket_inner(s)
        .split(',')
        .filter_map(|p| p.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite())
        .collect()
}

/// The text between the first `[` and the last `]`, else the whole (trimmed) string.
fn bracket_inner(s: &str) -> &str {
    let s = s.trim();
    match (s.find('['), s.rfind(']')) {
        (Some(a), Some(b)) if b > a => &s[a + 1..b],
        _ => s,
    }
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s)
}

// ---- rendering -----------------------------------------------------------------

fn render(m: &Model) -> String {
    let n = m
        .series
        .iter()
        .map(|s| s.values().len())
        .max()
        .unwrap_or(0)
        .max(m.categories.len())
        .max(1);

    // y-range: explicit, else auto (baseline 0 when all-positive, so bars read right).
    let data_min = m
        .series
        .iter()
        .flat_map(|s| s.values())
        .copied()
        .fold(f64::INFINITY, f64::min);
    let data_max = m
        .series
        .iter()
        .flat_map(|s| s.values())
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    let mut y_lo = m.y_min.unwrap_or_else(|| data_min.min(0.0));
    let mut y_hi = m.y_max.unwrap_or(data_max);
    if !y_lo.is_finite() {
        y_lo = 0.0;
    }
    if !y_hi.is_finite() || y_hi <= y_lo {
        y_hi = y_lo + 1.0;
    }

    let title_h = if m.title.is_some() { 28.0 } else { 0.0 };
    let plot_left = MARGIN + GUTTER_L;
    let plot_top = MARGIN + title_h;
    let plot_right = plot_left + PLOT_W;
    let plot_bottom = plot_top + PLOT_H;
    let width = plot_right + MARGIN;
    let height = plot_bottom + GUTTER_B + MARGIN;
    let slot_w = PLOT_W / n as f64;

    let py = |v: f64| plot_bottom - (v - y_lo) / (y_hi - y_lo) * PLOT_H;

    let mut out = String::with_capacity(1024 + n * m.series.len() * 60);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid catalerum-xychart\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {width:.1} {height:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"11\">"
    ));

    if let Some(title) = &m.title {
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"15\" \
             font-weight=\"bold\" fill=\"#1e293b\">",
            (plot_left + plot_right) / 2.0,
            MARGIN + 15.0
        ));
        escape_text(&mut out, title);
        out.push_str("</text>");
    }

    // Horizontal gridlines + y tick labels at 5 steps.
    for k in 0..=4 {
        let frac = f64::from(k) / 4.0;
        let val = y_lo + frac * (y_hi - y_lo);
        let gy = py(val);
        out.push_str(&format!(
            "<line x1=\"{plot_left:.1}\" y1=\"{gy:.1}\" x2=\"{plot_right:.1}\" y2=\"{gy:.1}\" \
             stroke=\"#e2e8f0\" stroke-width=\"1\"/>"
        ));
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" fill=\"#94a3b8\">{}</text>",
            plot_left - 6.0,
            gy + 3.5,
            trim_num(val)
        ));
    }

    // Baseline + left axis.
    out.push_str(&format!(
        "<line x1=\"{plot_left:.1}\" y1=\"{plot_top:.1}\" x2=\"{plot_left:.1}\" \
         y2=\"{plot_bottom:.1}\" stroke=\"#94a3b8\" stroke-width=\"1.2\"/>"
    ));
    out.push_str(&format!(
        "<line x1=\"{plot_left:.1}\" y1=\"{plot_bottom:.1}\" x2=\"{plot_right:.1}\" \
         y2=\"{plot_bottom:.1}\" stroke=\"#94a3b8\" stroke-width=\"1.2\"/>"
    ));

    // Grouped bars: each bar series gets a lane within every x slot.
    let bar_count = m
        .series
        .iter()
        .filter(|s| matches!(s, Series::Bar(_)))
        .count();
    let mut bar_lane = 0usize;
    for (si, s) in m.series.iter().enumerate() {
        let color = PALETTE[si % PALETTE.len()];
        match s {
            Series::Bar(vals) => {
                let group_w = slot_w * 0.72;
                let lane_w = group_w / bar_count.max(1) as f64;
                for (i, &v) in vals.iter().enumerate() {
                    let slot_left = plot_left + i as f64 * slot_w + (slot_w - group_w) / 2.0;
                    let x = slot_left + bar_lane as f64 * lane_w;
                    let top = py(v).min(py(y_lo));
                    let h = (py(v) - py(y_lo)).abs();
                    out.push_str(&format!(
                        "<rect x=\"{x:.1}\" y=\"{top:.1}\" width=\"{:.1}\" height=\"{h:.1}\" \
                         fill=\"{color}\" opacity=\"0.85\"/>",
                        (lane_w - 1.0).max(1.0)
                    ));
                }
                bar_lane += 1;
            }
            Series::Line(vals) => {
                let pts: Vec<String> = vals
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| {
                        let cx = plot_left + (i as f64 + 0.5) * slot_w;
                        format!("{cx:.1},{:.1}", py(v))
                    })
                    .collect();
                if pts.len() >= 2 {
                    out.push_str(&format!(
                        "<polyline points=\"{}\" fill=\"none\" stroke=\"{color}\" \
                         stroke-width=\"2\"/>",
                        pts.join(" ")
                    ));
                }
                for (i, &v) in vals.iter().enumerate() {
                    let cx = plot_left + (i as f64 + 0.5) * slot_w;
                    out.push_str(&format!(
                        "<circle cx=\"{cx:.1}\" cy=\"{:.1}\" r=\"3\" fill=\"{color}\"/>",
                        py(v)
                    ));
                }
            }
        }
    }

    // x-axis category labels (or the 1-based index when unlabelled).
    for i in 0..n {
        let cx = plot_left + (i as f64 + 0.5) * slot_w;
        out.push_str(&format!(
            "<text x=\"{cx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#475569\">",
            plot_bottom + 15.0
        ));
        match m.categories.get(i) {
            Some(c) => escape_text(&mut out, c),
            None => out.push_str(&(i + 1).to_string()),
        }
        out.push_str("</text>");
    }

    // Rotated y-axis label in the far-left gutter.
    if let Some(label) = &m.y_label {
        let lx = MARGIN + 10.0;
        let ly = (plot_top + plot_bottom) / 2.0;
        out.push_str(&format!(
            "<text x=\"{lx:.1}\" y=\"{ly:.1}\" text-anchor=\"middle\" \
             transform=\"rotate(-90 {lx:.1} {ly:.1})\" fill=\"#475569\">"
        ));
        escape_text(&mut out, label);
        out.push_str("</text>");
    }

    out.push_str("</svg>");
    out
}

/// Format a number without a trailing `.0` (so `5000.0` shows as `5000`).
fn trim_num(v: f64) -> String {
    if (v.fract()).abs() < 1e-9 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.1}")
    }
}

#[cfg(test)]
mod tests {
    use crate::mermaid::to_svg;

    #[test]
    fn renders_title_axes_bars_and_line() {
        let svg = to_svg(
            "xychart-beta\n title \"Revenue\"\n x-axis [jan, feb, mar]\n \
             y-axis \"USD\" 0 --> 10000\n bar [5000, 6000, 7500]\n line [4000, 8000, 6000]",
        )
        .unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        assert!(svg.contains(">Revenue</text>"), "title: {svg}");
        assert!(svg.contains(">USD</text>"), "y label: {svg}");
        // Three categories on the x-axis.
        for c in ["jan", "feb", "mar"] {
            assert!(svg.contains(&format!(">{c}</text>")), "category {c}: {svg}");
        }
        // Three bars (one per category) and a line polyline through the points.
        assert_eq!(
            svg.matches("<rect").count(),
            3,
            "one bar per category: {svg}"
        );
        assert!(svg.contains("<polyline"), "line series: {svg}");
    }

    #[test]
    fn auto_range_and_index_labels_when_unspecified() {
        // No x-axis + no y-axis range → index labels (1..) and an auto y-range.
        let svg = to_svg("xychart-beta\n bar [3, 9, 6, 12]").unwrap();
        assert_eq!(svg.matches("<rect").count(), 4, "{svg}");
        assert!(
            svg.contains(">1</text>") && svg.contains(">4</text>"),
            "index labels: {svg}"
        );
    }

    #[test]
    fn grouped_bars_share_each_slot() {
        // Two bar series → 2 bars per x slot (6 rects over 3 slots).
        let svg = to_svg("xychart-beta\n x-axis [a, b, c]\n bar [1,2,3]\n bar [3,2,1]").unwrap();
        assert_eq!(svg.matches("<rect").count(), 6, "{svg}");
    }

    #[test]
    fn labels_are_escaped() {
        let svg = to_svg("xychart-beta\n title \"<x>\"\n x-axis [\"<b>\"]\n bar [1]").unwrap();
        assert!(!svg.contains("<x>") && !svg.contains("<b>"), "{svg}");
        assert!(
            svg.contains("&lt;x&gt;") && svg.contains("&lt;b&gt;"),
            "{svg}"
        );
    }

    #[test]
    fn empty_chart_is_unsupported() {
        assert!(to_svg("xychart-beta\n title Nothing").is_err());
        assert!(to_svg("xychart-beta\n x-axis [a, b]").is_err());
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "xychart-beta",
            "xychart-beta\n bar",
            "xychart-beta\n bar []",
            "xychart-beta\n bar [x, y]",
            "xychart-beta\n y-axis \"L\" 5 --> 5",
            "xychart-beta\n y-axis 10 --> 0\n bar [1, 2]",
            "xychart-beta\n line [1]",
        ] {
            let _ = to_svg(s);
        }
    }
}
