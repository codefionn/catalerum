//! Quadrant charts (`quadrantChart`) → SVG: a square plot split into four tinted
//! quadrants by a centre cross, with axis labels, per-quadrant labels, and points
//! plotted at `[x, y]` (each in `0..1`, `x` left→right, `y` bottom→top). A point
//! line is `Name : [x, y]`; `x-axis`/`y-axis` carry `low --> high` end labels and
//! `quadrant-1..4` name the quadrants (1=top-right, 2=top-left, 3=bottom-left,
//! 4=bottom-right — standard math numbering with a bottom-left origin). All text
//! is escaped.

use super::{MermaidError, PALETTE};
use crate::escape::escape_text;

const MARGIN: f64 = 16.0;
const PLOT: f64 = 360.0; // side of the square plot area
const GUTTER_L: f64 = 26.0; // left gutter for the y-axis labels
const GUTTER_B: f64 = 26.0; // bottom gutter for the x-axis labels
const DOT_R: f64 = 5.0;

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let m = parse(src);
    // A chart needs at least one plotted point or one named quadrant to be worth
    // rendering; otherwise fall back to the raw source.
    if m.points.is_empty() && m.quadrants.iter().all(Option::is_none) {
        return Err(MermaidError("empty quadrant chart"));
    }
    Ok(render(&m))
}

// ---- model ---------------------------------------------------------------------

struct Point {
    name: String,
    x: f64, // clamped to 0..1
    y: f64, // clamped to 0..1
}

#[derive(Default)]
struct Model {
    title: Option<String>,
    x_left: Option<String>,
    x_right: Option<String>,
    y_bottom: Option<String>,
    y_top: Option<String>,
    /// [q1 top-right, q2 top-left, q3 bottom-left, q4 bottom-right].
    quadrants: [Option<String>; 4],
    points: Vec<Point>,
}

fn parse(src: &str) -> Model {
    let mut m = Model::default();
    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    lines.next(); // header (`quadrantChart`)

    for line in lines {
        if let Some(t) = line.strip_prefix("title ") {
            m.title = Some(t.trim().to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("x-axis ") {
            (m.x_left, m.x_right) = axis_ends(rest);
            continue;
        }
        if let Some(rest) = line.strip_prefix("y-axis ") {
            (m.y_bottom, m.y_top) = axis_ends(rest);
            continue;
        }
        if let Some(rest) = line.strip_prefix("quadrant-") {
            if let Some((num, label)) = rest.split_once(char::is_whitespace) {
                if let Ok(n) = num.trim().parse::<usize>() {
                    let label = label.trim();
                    if (1..=4).contains(&n) && !label.is_empty() {
                        m.quadrants[n - 1] = Some(label.to_string());
                    }
                }
            }
            continue;
        }
        // A point: `Name : [x, y]`. Anything else (styling directives) is ignored.
        if let Some((name, coords)) = line.split_once(':') {
            if let Some(p) = parse_point(name.trim(), coords.trim()) {
                m.points.push(p);
            }
        }
    }
    m
}

/// Split an axis spec `low --> high` into `(low, high)`; a spec with no `-->` is a
/// single label kept as the low/near end. Empty ends become `None`.
fn axis_ends(rest: &str) -> (Option<String>, Option<String>) {
    let nz = |s: &str| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    match rest.split_once("-->") {
        Some((a, b)) => (nz(a), nz(b)),
        None => (nz(rest), None),
    }
}

/// Parse `[x, y]` (each in `0..1`, clamped) into a [`Point`]; `None` if the
/// coordinate literal isn't a two-number bracketed pair.
fn parse_point(name: &str, coords: &str) -> Option<Point> {
    if name.is_empty() {
        return None;
    }
    let inner = coords.strip_prefix('[')?.strip_suffix(']')?;
    let (xs, ys) = inner.split_once(',')?;
    let x: f64 = xs.trim().parse().ok()?;
    let y: f64 = ys.trim().parse().ok()?;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    Some(Point {
        name: name.to_string(),
        x: x.clamp(0.0, 1.0),
        y: y.clamp(0.0, 1.0),
    })
}

// ---- rendering -----------------------------------------------------------------

fn render(m: &Model) -> String {
    let title_h = if m.title.is_some() { 30.0 } else { 0.0 };
    let plot_left = MARGIN + GUTTER_L;
    let plot_top = MARGIN + title_h;
    let plot_right = plot_left + PLOT;
    let plot_bottom = plot_top + PLOT;
    let mid_x = plot_left + PLOT / 2.0;
    let mid_y = plot_top + PLOT / 2.0;
    let width = plot_right + MARGIN;
    let height = plot_bottom + GUTTER_B + MARGIN;

    let mut out = String::with_capacity(1024 + m.points.len() * 100);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid catalerum-quadrant\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {width:.1} {height:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"12\">"
    ));

    if let Some(title) = &m.title {
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"15\" \
             font-weight=\"bold\" fill=\"#1e293b\">",
            (plot_left + plot_right) / 2.0,
            MARGIN + 16.0
        ));
        escape_text(&mut out, title);
        out.push_str("</text>");
    }

    // Quadrant fills (q2 TL, q1 TR, q3 BL, q4 BR) + their labels, tinted from the
    // shared palette. `rects` maps each quadrant index (0=q1..3=q4) to its corner.
    let half = PLOT / 2.0;
    let rects = [
        (2usize, mid_x, plot_top), // q1 top-right
        (1, plot_left, plot_top),  // q2 top-left
        (0, plot_left, mid_y),     // q3 bottom-left
        (3, mid_x, mid_y),         // q4 bottom-right
    ];
    for &(qi, x, y) in &rects {
        let color = PALETTE[qi % PALETTE.len()];
        out.push_str(&format!(
            "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{half:.1}\" height=\"{half:.1}\" \
             fill=\"{color}\" opacity=\"0.10\"/>"
        ));
        if let Some(label) = &m.quadrants[qi] {
            out.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"{color}\" \
                 font-weight=\"bold\">",
                x + half / 2.0,
                y + 16.0
            ));
            escape_text(&mut out, label);
            out.push_str("</text>");
        }
    }

    // Plot border + centre cross.
    out.push_str(&format!(
        "<rect x=\"{plot_left:.1}\" y=\"{plot_top:.1}\" width=\"{PLOT:.1}\" height=\"{PLOT:.1}\" \
         fill=\"none\" stroke=\"#cbd5e1\" stroke-width=\"1.5\"/>"
    ));
    out.push_str(&format!(
        "<line x1=\"{mid_x:.1}\" y1=\"{plot_top:.1}\" x2=\"{mid_x:.1}\" y2=\"{plot_bottom:.1}\" \
         stroke=\"#e2e8f0\" stroke-width=\"1\"/>"
    ));
    out.push_str(&format!(
        "<line x1=\"{plot_left:.1}\" y1=\"{mid_y:.1}\" x2=\"{plot_right:.1}\" y2=\"{mid_y:.1}\" \
         stroke=\"#e2e8f0\" stroke-width=\"1\"/>"
    ));

    // Points: x left→right, y bottom→top (so y=1 sits at the top).
    for p in &m.points {
        let px = plot_left + p.x * PLOT;
        let py = plot_bottom - p.y * PLOT;
        out.push_str(&format!(
            "<circle cx=\"{px:.1}\" cy=\"{py:.1}\" r=\"{DOT_R:.1}\" fill=\"#334155\"/>"
        ));
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#1e293b\">",
            px + DOT_R + 3.0,
            py + 4.0
        ));
        escape_text(&mut out, &p.name);
        out.push_str("</text>");
    }

    // x-axis end labels below the plot (low at left, high at right).
    let x_label_y = plot_bottom + 16.0;
    if let Some(l) = &m.x_left {
        out.push_str(&format!(
            "<text x=\"{plot_left:.1}\" y=\"{x_label_y:.1}\" fill=\"#475569\" font-size=\"11\">"
        ));
        escape_text(&mut out, l);
        out.push_str("</text>");
    }
    if let Some(r) = &m.x_right {
        out.push_str(&format!(
            "<text x=\"{plot_right:.1}\" y=\"{x_label_y:.1}\" text-anchor=\"end\" fill=\"#475569\" \
             font-size=\"11\">"
        ));
        escape_text(&mut out, r);
        out.push_str("</text>");
    }

    // y-axis end labels rotated in the left gutter (bottom near the bottom, top
    // near the top). `rotate(-90, x, y)` turns the baseline to read upward.
    let y_label_x = MARGIN + 10.0;
    if let Some(b) = &m.y_bottom {
        out.push_str(&format!(
            "<text x=\"{y_label_x:.1}\" y=\"{plot_bottom:.1}\" transform=\"rotate(-90 {y_label_x:.1} \
             {plot_bottom:.1})\" fill=\"#475569\" font-size=\"11\">"
        ));
        escape_text(&mut out, b);
        out.push_str("</text>");
    }
    if let Some(t) = &m.y_top {
        out.push_str(&format!(
            "<text x=\"{y_label_x:.1}\" y=\"{plot_top:.1}\" transform=\"rotate(-90 {y_label_x:.1} \
             {plot_top:.1})\" text-anchor=\"end\" fill=\"#475569\" font-size=\"11\">"
        ));
        escape_text(&mut out, t);
        out.push_str("</text>");
    }

    out.push_str("</svg>");
    out
}

#[cfg(test)]
mod tests {
    use crate::mermaid::to_svg;

    #[test]
    fn renders_quadrants_axes_and_points() {
        let svg = to_svg(
            "quadrantChart\n title Campaigns\n x-axis Low Reach --> High Reach\n \
             y-axis Low Eng --> High Eng\n quadrant-1 Expand\n quadrant-2 Promote\n \
             quadrant-3 Re-evaluate\n quadrant-4 Improve\n Camp A: [0.3, 0.6]\n Camp B: [0.8, 0.2]",
        )
        .unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        assert!(svg.contains(">Campaigns</text>"), "title: {svg}");
        // Axis end labels.
        assert!(
            svg.contains(">Low Reach</text>") && svg.contains(">High Reach</text>"),
            "x: {svg}"
        );
        assert!(
            svg.contains(">Low Eng</text>") && svg.contains(">High Eng</text>"),
            "y: {svg}"
        );
        // All four quadrant labels.
        for q in ["Expand", "Promote", "Re-evaluate", "Improve"] {
            assert!(svg.contains(&format!(">{q}</text>")), "quadrant {q}: {svg}");
        }
        // Two points → two dots + their names.
        assert_eq!(
            svg.matches("<circle").count(),
            2,
            "one dot per point: {svg}"
        );
        assert!(
            svg.contains(">Camp A</text>") && svg.contains(">Camp B</text>"),
            "points: {svg}"
        );
    }

    #[test]
    fn out_of_range_coords_are_clamped_and_bad_points_skipped() {
        // [1.5, -0.2] clamps into the plot; a non-bracketed value isn't a point.
        let svg =
            to_svg("quadrantChart\n A: [1.5, -0.2]\n B: not a point\n C: [0.5, 0.5]").unwrap();
        assert_eq!(
            svg.matches("<circle").count(),
            2,
            "A and C plot, B skipped: {svg}"
        );
    }

    #[test]
    fn labels_and_point_names_are_escaped() {
        let svg = to_svg("quadrantChart\n quadrant-1 <b>hi</b>\n <i>P</i>: [0.5, 0.5]").unwrap();
        assert!(!svg.contains("<b>hi") && !svg.contains("<i>P"), "{svg}");
        assert!(
            svg.contains("&lt;b&gt;hi") && svg.contains("&lt;i&gt;P"),
            "{svg}"
        );
    }

    #[test]
    fn empty_chart_is_unsupported() {
        assert!(to_svg("quadrantChart\n title Nothing").is_err());
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "quadrantChart",
            "quadrantChart\n x-axis",
            "quadrantChart\n quadrant-9 x",
            "quadrantChart\n quadrant-",
            "quadrantChart\n : [0.1, 0.2]",
            "quadrantChart\n A: []",
            "quadrantChart\n A: [1]",
            "quadrantChart\n A: [x, y]",
        ] {
            let _ = to_svg(s);
        }
    }
}
