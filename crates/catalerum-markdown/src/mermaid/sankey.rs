//! Sankey diagrams (`sankey-beta`) → SVG: a left-to-right flow diagram over a
//! CSV-like body. Each non-empty, non-`%%` line is one `source,target,value`
//! flow; fields may be double-quoted (a `""` inside a quoted field is a literal
//! quote, and commas inside quotes are kept). Nodes are derived from the flow
//! endpoints, laid out in columns by their longest-path depth, with heights
//! proportional to throughput; flows render as filled bezier ribbons tinted by
//! their source's palette colour. Zero/negative/unparseable values are skipped
//! and a cyclic graph fails to [`Err`] (raw-source fallback) rather than looping.
//! All text is escaped.

use super::{MermaidError, PALETTE};
use crate::escape::escape_text;

const MARGIN: f64 = 20.0;
const NODE_W: f64 = 18.0;
const COL_GAP: f64 = 130.0; // horizontal room for ribbons between columns
const PLOT_H: f64 = 340.0;
const NODE_GAP: f64 = 16.0; // vertical gap between stacked nodes in a column
const MIN_NODE_H: f64 = 2.0;
const RIBBON_OPACITY: &str = "0.45";

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let m = parse(src);
    if m.flows.is_empty() {
        return Err(MermaidError("no sankey flows"));
    }
    if has_cycle(m.names.len(), &m.flows) {
        return Err(MermaidError("cyclic sankey"));
    }
    Ok(render(&m))
}

// ---- model ---------------------------------------------------------------------

struct Flow {
    src: usize,
    dst: usize,
    value: f64,
}

struct Model {
    names: Vec<String>,
    flows: Vec<Flow>,
}

/// Find `name`'s node index, inserting it (in first-seen order) if new.
fn intern(names: &mut Vec<String>, name: &str) -> usize {
    if let Some(i) = names.iter().position(|n| n == name) {
        i
    } else {
        names.push(name.to_string());
        names.len() - 1
    }
}

fn parse(src: &str) -> Model {
    let mut names: Vec<String> = Vec::new();
    let mut flows: Vec<Flow> = Vec::new();

    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    lines.next(); // header (`sankey-beta` / `sankey`)

    for line in lines {
        let Some(fields) = parse_csv_line(line) else {
            continue; // unbalanced quotes etc. → skip gracefully
        };
        if fields.len() < 3 {
            continue;
        }
        let (source, target) = (fields[0].trim(), fields[1].trim());
        if source.is_empty() || target.is_empty() {
            continue;
        }
        // Value must parse to a finite, strictly-positive number.
        let Ok(value) = fields[2].trim().parse::<f64>() else {
            continue;
        };
        if !value.is_finite() || value <= 0.0 {
            continue;
        }
        let src = intern(&mut names, source);
        let dst = intern(&mut names, target);
        flows.push(Flow { src, dst, value });
    }
    Model { names, flows }
}

/// Split one CSV record into trimmed, unquoted fields. Returns `None` on an
/// unterminated quoted field so the caller skips the malformed line.
fn parse_csv_line(line: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = line.chars().collect();
    let mut fields: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0usize;
    loop {
        if chars.get(i) == Some(&'"') {
            i += 1; // opening quote
            loop {
                match chars.get(i) {
                    Some('"') if chars.get(i + 1) == Some(&'"') => {
                        cur.push('"'); // escaped quote
                        i += 2;
                    }
                    Some('"') => {
                        i += 1; // closing quote
                        break;
                    }
                    Some(&c) => {
                        cur.push(c);
                        i += 1;
                    }
                    None => return None, // unterminated
                }
            }
            // Discard any stray characters up to the next comma.
            while matches!(chars.get(i), Some(c) if *c != ',') {
                i += 1;
            }
        } else {
            while matches!(chars.get(i), Some(c) if *c != ',') {
                cur.push(chars[i]);
                i += 1;
            }
        }
        fields.push(std::mem::take(&mut cur));
        match chars.get(i) {
            Some(',') => i += 1,
            _ => break,
        }
    }
    Some(fields)
}

/// Iterative DFS (white/grey/black) — a grey node reached again (incl. a
/// self-loop) means a cycle, so the graph is not a valid left-to-right sankey.
fn has_cycle(n: usize, flows: &[Flow]) -> bool {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for f in flows {
        adj[f.src].push(f.dst);
    }
    let mut color = vec![0u8; n]; // 0 = white, 1 = grey, 2 = black
    for start in 0..n {
        if color[start] != 0 {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        color[start] = 1;
        while let Some(&(u, ei)) = stack.last() {
            if ei < adj[u].len() {
                stack.last_mut().unwrap().1 += 1;
                let v = adj[u][ei];
                match color[v] {
                    0 => {
                        color[v] = 1;
                        stack.push((v, 0));
                    }
                    1 => return true,
                    _ => {}
                }
            } else {
                color[u] = 2;
                stack.pop();
            }
        }
    }
    false
}

// ---- rendering -----------------------------------------------------------------

fn render(m: &Model) -> String {
    let n = m.names.len();

    // Longest-path layering via Kahn: a node's column is 1 + the max column of
    // any predecessor. The graph is acyclic here (checked in `to_svg`).
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut in_deg = vec![0usize; n];
    for f in &m.flows {
        succ[f.src].push(f.dst);
        in_deg[f.dst] += 1;
    }
    let mut col = vec![0usize; n];
    let mut queue: Vec<usize> = (0..n).filter(|&i| in_deg[i] == 0).collect();
    let mut qi = 0;
    while qi < queue.len() {
        let u = queue[qi];
        qi += 1;
        for &v in &succ[u] {
            col[v] = col[v].max(col[u] + 1);
            in_deg[v] -= 1;
            if in_deg[v] == 0 {
                queue.push(v);
            }
        }
    }
    let max_col = col.iter().copied().max().unwrap_or(0);

    // Throughput per node = max(total in, total out).
    let mut in_sum = vec![0.0f64; n];
    let mut out_sum = vec![0.0f64; n];
    for f in &m.flows {
        out_sum[f.src] += f.value;
        in_sum[f.dst] += f.value;
    }
    let flow: Vec<f64> = (0..n).map(|i| in_sum[i].max(out_sum[i])).collect();

    // Value→pixel scale: pick the tightest column so every stack fits `PLOT_H`.
    let mut scale = f64::INFINITY;
    for c in 0..=max_col {
        let members: Vec<usize> = (0..n).filter(|&i| col[i] == c).collect();
        if members.is_empty() {
            continue;
        }
        let total: f64 = members.iter().map(|&i| flow[i]).sum();
        if total <= 0.0 {
            continue;
        }
        let gaps = (members.len().saturating_sub(1)) as f64 * NODE_GAP;
        let avail = (PLOT_H - gaps).max(members.len() as f64 * MIN_NODE_H);
        scale = scale.min(avail / total);
    }
    if !scale.is_finite() || scale <= 0.0 {
        scale = 1.0;
    }

    let node_h = |i: usize| (flow[i] * scale).max(MIN_NODE_H);
    let node_x = |c: usize| MARGIN + c as f64 * (NODE_W + COL_GAP);

    // Stack each column's nodes (first-seen order), vertically centred.
    let plot_top = MARGIN;
    let mut node_y = vec![0.0f64; n];
    for c in 0..=max_col {
        let members: Vec<usize> = (0..n).filter(|&i| col[i] == c).collect();
        let total_h: f64 = members.iter().map(|&i| node_h(i)).sum::<f64>()
            + (members.len().saturating_sub(1)) as f64 * NODE_GAP;
        let mut y = plot_top + (PLOT_H - total_h) / 2.0;
        for &i in &members {
            node_y[i] = y;
            y += node_h(i) + NODE_GAP;
        }
    }

    let width = node_x(max_col) + NODE_W + MARGIN;
    let height = plot_top + PLOT_H + MARGIN;
    let color_of = |i: usize| PALETTE[i % PALETTE.len()];

    let mut out = String::with_capacity(512 + m.flows.len() * 160);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid catalerum-sankey\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {width:.1} {height:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"12\">"
    ));

    // Ribbons first (under the node bars). Track a running offset on each node's
    // outgoing (right) and incoming (left) edges so ribbons stack without gaps.
    let mut out_off = node_y.clone();
    let mut in_off = node_y.clone();
    for f in &m.flows {
        let thick = (f.value * scale).max(MIN_NODE_H);
        let x0 = node_x(col[f.src]) + NODE_W;
        let x1 = node_x(col[f.dst]);
        let y0 = out_off[f.src];
        let y1 = in_off[f.dst];
        out_off[f.src] += thick;
        in_off[f.dst] += thick;
        let mid = (x0 + x1) / 2.0;
        // A filled band: top edge left→right, down the right side, bottom edge back.
        out.push_str(&format!(
            "<path d=\"M{x0:.1},{y0:.1} C{mid:.1},{y0:.1} {mid:.1},{y1:.1} {x1:.1},{y1:.1} \
             L{x1:.1},{:.1} C{mid:.1},{:.1} {mid:.1},{:.1} {x0:.1},{:.1} Z\" \
             fill=\"{}\" fill-opacity=\"{RIBBON_OPACITY}\"/>",
            y1 + thick,
            y1 + thick,
            y0 + thick,
            y0 + thick,
            color_of(f.src)
        ));
    }

    // Node bars + labels.
    for i in 0..n {
        let x = node_x(col[i]);
        let y = node_y[i];
        let h = node_h(i);
        let color = color_of(i);
        out.push_str(&format!(
            "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{NODE_W:.1}\" height=\"{h:.1}\" rx=\"2\" \
             fill=\"{color}\"/>"
        ));
        // Last column labels sit to the left, everything else to the right.
        let last = col[i] == max_col;
        let (lx, anchor) = if last {
            (x - 6.0, "end")
        } else {
            (x + NODE_W + 6.0, "start")
        };
        let cy = y + h / 2.0;
        out.push_str(&format!(
            "<text x=\"{lx:.1}\" y=\"{:.1}\" text-anchor=\"{anchor}\" fill=\"#1e293b\" \
             font-weight=\"bold\">",
            cy - 1.0
        ));
        escape_text(&mut out, &m.names[i]);
        out.push_str("</text>");
        out.push_str(&format!(
            "<text x=\"{lx:.1}\" y=\"{:.1}\" text-anchor=\"{anchor}\" fill=\"#94a3b8\" \
             font-size=\"10\">{}</text>",
            cy + 11.0,
            trim_num(flow[i])
        ));
    }

    out.push_str("</svg>");
    out
}

/// Format a value without a trailing `.0` (so `5.0` shows as `5`).
fn trim_num(v: f64) -> String {
    if v.fract().abs() < 1e-9 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::{has_cycle, parse, parse_csv_line, Flow};
    use crate::mermaid::to_svg;

    #[test]
    fn parses_flows_and_interns_nodes() {
        let m = parse("sankey-beta\n A,B,10\n B,C,5\n A,C,3");
        // Nodes A, B, C in first-seen order.
        assert_eq!(m.names, vec!["A", "B", "C"]);
        assert_eq!(m.flows.len(), 3);
        assert_eq!((m.flows[0].src, m.flows[0].dst), (0, 1));
        assert!((m.flows[0].value - 10.0).abs() < 1e-9);
    }

    #[test]
    fn quoted_fields_keep_commas_and_escaped_quotes() {
        let f = parse_csv_line("\"a, inc\",\"b \"\"x\"\" c\",4").unwrap();
        assert_eq!(
            f,
            vec![
                "a, inc".to_string(),
                "b \"x\" c".to_string(),
                "4".to_string()
            ]
        );
    }

    #[test]
    fn renders_nodes_ribbons_and_labels() {
        let svg = to_svg("sankey-beta\n A,B,10\n A,C,5\n B,D,10\n C,D,5").unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        assert!(svg.contains("catalerum-sankey"), "class: {svg}");
        // 4 node bars (A, B, C, D).
        assert_eq!(svg.matches("<rect").count(), 4, "one bar per node: {svg}");
        // 4 flow ribbons.
        assert_eq!(
            svg.matches("fill-opacity=\"0.45\"").count(),
            4,
            "one ribbon per flow: {svg}"
        );
        for label in ["A", "B", "C", "D"] {
            assert!(
                svg.contains(&format!(">{label}</text>")),
                "label {label}: {svg}"
            );
        }
    }

    #[test]
    fn columns_follow_longest_path_depth() {
        // A→B→C puts C two columns right of A; a direct A→C keeps C at depth 2.
        let svg = to_svg("sankey-beta\n A,B,1\n B,C,1\n A,C,1").unwrap();
        assert!(svg.contains("<svg"), "{svg}");
    }

    #[test]
    fn skips_zero_negative_and_unparseable_values() {
        let m = parse("sankey-beta\n A,B,0\n A,B,-4\n A,B,x\n A,B,7");
        // Only the last (7) survives.
        assert_eq!(m.flows.len(), 1);
        assert!((m.flows[0].value - 7.0).abs() < 1e-9);
    }

    #[test]
    fn self_loop_is_a_cycle() {
        assert!(has_cycle(
            1,
            &[Flow {
                src: 0,
                dst: 0,
                value: 1.0
            }]
        ));
        assert!(
            to_svg("sankey-beta\n A,A,5").is_err(),
            "self-loop → raw fallback"
        );
    }

    #[test]
    fn cycle_falls_back_to_raw_source() {
        // A→B→C→A is cyclic and must not loop forever.
        assert!(to_svg("sankey-beta\n A,B,1\n B,C,1\n C,A,1").is_err());
    }

    #[test]
    fn labels_are_escaped() {
        let svg = to_svg("sankey-beta\n \"<b>x\",\"y\",3").unwrap();
        assert!(!svg.contains("<b>x"), "{svg}");
        assert!(svg.contains("&lt;b&gt;x"), "{svg}");
    }

    #[test]
    fn empty_sankey_is_unsupported() {
        assert!(to_svg("sankey-beta").is_err());
        assert!(to_svg("sankey-beta\n %% only a comment").is_err());
        assert!(to_svg("sankey-beta\n A,B,notanumber").is_err());
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "sankey-beta",
            "sankey-beta\n ",
            "sankey-beta\n A",
            "sankey-beta\n A,B",
            "sankey-beta\n ,,",
            "sankey-beta\n \"unterminated,B,1",
            "sankey-beta\n A,A,1",
            "sankey-beta\n A,B,1\n B,A,1",
            "sankey-beta\n A,B,1e999",
            "sankey-beta\n \"a\"\"\",b,1",
        ] {
            let _ = to_svg(s);
        }
    }
}
