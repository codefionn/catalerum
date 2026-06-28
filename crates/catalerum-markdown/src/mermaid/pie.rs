//! Pie charts (`pie`) → SVG: arc slices + a legend with values and percentages.

use std::f64::consts::PI;

use super::{strip_quotes, text_width, MermaidError, PALETTE};
use crate::escape::escape_text;

struct Slice {
    label: String,
    value: f64,
}

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    let header = lines.next().ok_or(MermaidError("empty diagram"))?;

    // `pie`, `pie showData`, `pie title …`, `pie showData title …`.
    let mut title = String::new();
    let mut after = header.strip_prefix("pie").unwrap_or("").trim();
    after = after.strip_prefix("showData").unwrap_or(after).trim();
    if let Some(t) = after.strip_prefix("title") {
        title = t.trim().to_string();
    }

    let mut slices: Vec<Slice> = Vec::new();
    for line in lines {
        if let Some(t) = line.strip_prefix("title ") {
            title = t.trim().to_string();
            continue;
        }
        if line == "showData" {
            continue;
        }
        // `"Label" : value` (split on the last colon so labels may contain `:`).
        if let Some((label_part, val_part)) = line.rsplit_once(':') {
            let label = strip_quotes(label_part);
            if let Ok(v) = val_part.trim().parse::<f64>() {
                if v.is_finite() && v >= 0.0 {
                    slices.push(Slice { label, value: v });
                }
            }
        }
    }

    let total: f64 = slices.iter().map(|s| s.value).sum();
    if slices.is_empty() || total <= 0.0 {
        return Err(MermaidError("no pie data"));
    }
    Ok(render(&title, &slices, total))
}

const MARGIN: f64 = 16.0;
const R: f64 = 110.0;
const SWATCH: f64 = 15.0;
const ROW_H: f64 = 24.0;

fn render(title: &str, slices: &[Slice], total: f64) -> String {
    let title_h = if title.is_empty() { 0.0 } else { 30.0 };
    let cx = MARGIN + R;
    let cy = title_h + MARGIN + R;

    // Legend width from the longest "label  value (pct%)" row.
    let legend_x = cx + R + 28.0;
    let legend_w = slices
        .iter()
        .map(|s| SWATCH + 8.0 + text_width(&legend_label(s, total)))
        .fold(0.0, f64::max);
    let legend_rows_h = slices.len() as f64 * ROW_H;

    let width = (legend_x + legend_w + MARGIN).max(cx + R + MARGIN);
    let pie_h = title_h + 2.0 * R + 2.0 * MARGIN;
    let legend_total_h = title_h + MARGIN + legend_rows_h + MARGIN;
    let height = pie_h.max(legend_total_h);

    let mut out = String::with_capacity(512 + slices.len() * 200);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid catalerum-pie\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {width:.1} {height:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"14\">"
    ));

    if !title.is_empty() {
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"20\" text-anchor=\"middle\" font-size=\"17\" \
             font-weight=\"600\" fill=\"#1e293b\">",
            width / 2.0
        ));
        escape_text(&mut out, title);
        out.push_str("</text>");
    }

    // Slices, starting at the top (−90°), clockwise.
    let mut angle = -PI / 2.0;
    for (i, s) in slices.iter().enumerate() {
        let frac = s.value / total;
        let sweep = frac * 2.0 * PI;
        let end = angle + sweep;
        let color = PALETTE[i % PALETTE.len()];
        if slices.len() == 1 || frac >= 0.999 {
            out.push_str(&format!(
                "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"{R:.1}\" fill=\"{color}\" \
                 stroke=\"#ffffff\" stroke-width=\"1.5\"/>"
            ));
        } else {
            let (x0, y0) = (cx + R * angle.cos(), cy + R * angle.sin());
            let (x1, y1) = (cx + R * end.cos(), cy + R * end.sin());
            let large = if sweep > PI { 1 } else { 0 };
            out.push_str(&format!(
                "<path d=\"M{cx:.1},{cy:.1} L{x0:.2},{y0:.2} A{R:.1},{R:.1} 0 {large},1 {x1:.2},{y1:.2} Z\" \
                 fill=\"{color}\" stroke=\"#ffffff\" stroke-width=\"1.5\"/>"
            ));
        }
        // Percentage on the slice (skip tiny slivers).
        if frac >= 0.05 {
            let mid = angle + sweep / 2.0;
            let lx = cx + R * 0.62 * mid.cos();
            let ly = cy + R * 0.62 * mid.sin();
            out.push_str(&format!(
                "<text x=\"{lx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#ffffff\" \
                 font-size=\"13\" font-weight=\"600\">{:.0}%</text>",
                ly + 4.0,
                frac * 100.0
            ));
        }
        angle = end;
    }

    // Legend.
    for (i, s) in slices.iter().enumerate() {
        let color = PALETTE[i % PALETTE.len()];
        let ry = title_h + MARGIN + i as f64 * ROW_H;
        out.push_str(&format!(
            "<rect x=\"{legend_x:.1}\" y=\"{ry:.1}\" width=\"{SWATCH}\" height=\"{SWATCH}\" \
             rx=\"2\" fill=\"{color}\"/>"
        ));
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#334155\">",
            legend_x + SWATCH + 8.0,
            ry + SWATCH - 2.0
        ));
        escape_text(&mut out, &legend_label(s, total));
        out.push_str("</text>");
    }

    out.push_str("</svg>");
    out
}

fn legend_label(s: &Slice, total: f64) -> String {
    let pct = s.value / total * 100.0;
    format!("{}  {}  ({:.1}%)", s.label, trim_num(s.value), pct)
}

/// Format a value without a trailing `.0` (so `386.0` shows as `386`).
fn trim_num(v: f64) -> String {
    if (v.fract()).abs() < 1e-9 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_slices_and_legend() {
        let svg =
            to_svg("pie title Pets\n  \"Dogs\" : 386\n  \"Cats\" : 85\n  \"Rats\" : 15").unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains(">Pets</text>")); // title
        assert_eq!(svg.matches("<path").count(), 3); // three slices
        assert!(svg.contains("Dogs"));
        assert!(svg.contains("(79.4%)"), "{svg}"); // 386 / (386+85+15=486)
    }

    #[test]
    fn single_slice_is_a_full_circle() {
        let svg = to_svg("pie\n \"only\" : 1").unwrap();
        assert!(svg.contains("<circle"), "{svg}");
    }

    #[test]
    fn rejects_empty_or_zero() {
        assert!(to_svg("pie\n").is_err());
        assert!(to_svg("pie\n \"a\" : 0\n \"b\" : 0").is_err());
    }

    #[test]
    fn labels_escaped() {
        let svg = to_svg("pie\n \"<b>x</b>\" : 5").unwrap();
        assert!(!svg.contains("<b>x"));
        assert!(svg.contains("&lt;b&gt;"));
    }

    #[test]
    fn never_panics() {
        for s in [
            "pie",
            "pie title",
            "pie\n :",
            "pie\n \"a\" : abc",
            "pie\n \"a\" : -5",
            "pie\n x",
        ] {
            let _ = to_svg(s);
        }
    }
}
