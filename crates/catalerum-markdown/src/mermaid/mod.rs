//! Mermaid diagrams → SVG, in pure Rust, no JavaScript.
//!
//! [`to_svg`] dispatches by diagram type to a per-type renderer: [`flow`]
//! (flowcharts), [`sequence`] (sequence diagrams), [`pie`] (pie charts), the
//! flowchart-layout-reusing [`state`] (state diagrams), [`er`] (entity-
//! relationship diagrams) and [`class`] (class diagrams), the axis-based
//! [`gantt`] (gantt charts), [`timeline`] (timelines) and [`journey`] (user-journey
//! satisfaction curves), [`quadrant`] (2×2 quadrant charts) and [`xychart`]
//! (bar/line charts), the indentation-tree [`mindmap`] (mindmaps) and [`gitgraph`]
//! (git commit graphs), the CSV-flow [`sankey`] (sankey diagrams) and the grid-
//! layout [`c4`] (C4 model diagrams). Each emits a self-contained `<svg>` with
//! inlined styling and **escapes all text**, so the result is safe to inject.
//! Unsupported diagram types (`packet-beta`, `kanban`, …) return [`Err`] so the
//! Markdown renderer falls back to the raw source.

mod c4;
mod class;
mod er;
mod flow;
mod gantt;
mod gitgraph;
mod journey;
mod mindmap;
mod pie;
mod quadrant;
mod sankey;
mod sequence;
mod state;
mod timeline;
mod xychart;

use std::fmt;

/// Why a diagram could not be rendered (the caller falls back to raw source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MermaidError(pub &'static str);

impl fmt::Display for MermaidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mermaid: {}", self.0)
    }
}

impl std::error::Error for MermaidError {}

/// Parse a Mermaid diagram and render it to a standalone SVG string.
pub fn to_svg(src: &str) -> Result<String, MermaidError> {
    let header = src
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("%%"))
        .ok_or(MermaidError("empty diagram"))?;
    if header.starts_with("graph") || header.starts_with("flowchart") {
        flow::to_svg(src)
    } else if header.starts_with("sequenceDiagram") {
        sequence::to_svg(src)
    } else if header.starts_with("stateDiagram") {
        state::to_svg(src)
    } else if header.starts_with("classDiagram") {
        class::to_svg(src)
    } else if header.starts_with("erDiagram") {
        er::to_svg(src)
    } else if header.starts_with("gantt") {
        gantt::to_svg(src)
    } else if header.starts_with("timeline") {
        timeline::to_svg(src)
    } else if header.starts_with("journey") {
        journey::to_svg(src)
    } else if header.starts_with("quadrantChart") {
        quadrant::to_svg(src)
    } else if header.starts_with("xychart") {
        xychart::to_svg(src)
    } else if header.starts_with("mindmap") {
        mindmap::to_svg(src)
    } else if header.starts_with("gitGraph") {
        gitgraph::to_svg(src)
    } else if header.starts_with("sankey") {
        // `sankey-beta` (and a bare `sankey` alias).
        sankey::to_svg(src)
    } else if header.starts_with("C4Context")
        || header.starts_with("C4Container")
        || header.starts_with("C4Component")
        || header.starts_with("C4Dynamic")
    {
        c4::to_svg(src)
    } else if header.starts_with("pie") {
        pie::to_svg(src)
    } else {
        Err(MermaidError("unsupported diagram type"))
    }
}

// ---- helpers shared by the per-type renderers ---------------------------------

/// Approximate rendered width of `text` at ~15px (used to size boxes/columns).
pub(super) const CHAR_W: f64 = 8.4;

/// Categorical colour palette (pie slices, etc.).
pub(super) const PALETTE: [&str; 10] = [
    "#3b82f6", "#10b981", "#f59e0b", "#ef4444", "#8b5cf6", "#ec4899", "#14b8a6", "#f97316",
    "#6366f1", "#84cc16",
];

pub(super) fn text_width(s: &str) -> f64 {
    s.chars().count() as f64 * CHAR_W
}

pub(super) fn skip_ws(c: &[char], i: &mut usize) {
    while matches!(c.get(*i), Some(ch) if ch.is_whitespace()) {
        *i += 1;
    }
}

pub(super) fn strip_quotes(s: &str) -> String {
    let t = s.trim();
    t.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(t)
        .to_string()
}

pub(super) fn matches_at(c: &[char], i: usize, pat: &[char]) -> bool {
    pat.iter()
        .enumerate()
        .all(|(k, ch)| c.get(i + k) == Some(ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_by_diagram_type() {
        assert!(to_svg("graph TD\n A-->B").unwrap().contains("<svg"));
        assert!(to_svg("flowchart LR\n A-->B").unwrap().contains("<svg"));
        assert!(to_svg("sequenceDiagram\n A->>B: hi")
            .unwrap()
            .contains("<svg"));
        assert!(to_svg("pie\n \"A\": 1\n \"B\": 2")
            .unwrap()
            .contains("<svg"));
        assert!(to_svg("stateDiagram-v2\n [*] --> A")
            .unwrap()
            .contains("<svg"));
        assert!(to_svg("erDiagram\n A ||--o{ B : has")
            .unwrap()
            .contains("<svg"));
        assert!(to_svg("classDiagram\n Animal <|-- Dog")
            .unwrap()
            .contains("<svg"));
        assert!(to_svg("gantt\n A :2014-01-01, 5d")
            .unwrap()
            .contains("<svg"));
        assert!(to_svg("timeline\n 2002 : Event").unwrap().contains("<svg"));
        assert!(to_svg("journey\n Task: 3: Me").unwrap().contains("<svg"));
        assert!(to_svg("quadrantChart\n A: [0.5, 0.5]")
            .unwrap()
            .contains("<svg"));
        assert!(to_svg("xychart-beta\n bar [1, 2, 3]")
            .unwrap()
            .contains("<svg"));
        assert!(to_svg("mindmap\n root((r))\n  child")
            .unwrap()
            .contains("<svg"));
        assert!(to_svg("gitGraph\n commit\n commit")
            .unwrap()
            .contains("<svg"));
        assert!(to_svg("sankey-beta\n a,b,1").unwrap().contains("<svg"));
        assert!(to_svg("sankey\n a,b,1").unwrap().contains("<svg"));
        assert!(to_svg("C4Context\n Person(u, User)\n System(s, App)")
            .unwrap()
            .contains("<svg"));
        assert!(to_svg("C4Container\n System(s, App)")
            .unwrap()
            .contains("<svg"));
    }

    #[test]
    fn unknown_types_are_unsupported() {
        for s in [
            "packet-beta\n title P\n 0-7: X",
            "kanban\n col\n  task",
            "classDiagram\n",
            "",
        ] {
            assert!(to_svg(s).is_err(), "{s:?} should be unsupported");
        }
    }
}
