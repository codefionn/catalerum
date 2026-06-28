//! Git graphs (`gitGraph`) → SVG: the conventional horizontal commit graph, one
//! coloured lane per branch. Supports `commit` (with optional `id:"…"`,
//! `tag:"…"`, `type: NORMAL|REVERSE|HIGHLIGHT`), `branch <name>` (creates and
//! switches), `checkout`/`switch <name>`, `merge <name>` (optional `id:`/`tag:`,
//! drawn as a ring with an edge back to the merged lane) and `cherry-pick id:"…"`
//! (a diamond marker). Commits advance left→right in source order; branch-creation
//! and merge edges curve between lanes. Tags sit above a commit, ids below. All
//! text is escaped.
//!
//! The `gitGraph TB:`/`BT:` vertical variants are parsed but always rendered
//! horizontally (the direction token is ignored).

use super::{MermaidError, PALETTE};
use crate::escape::escape_text;

const MARGIN: f64 = 16.0;
const CHAR_W: f64 = 7.2;
const COL_W: f64 = 52.0; // horizontal spacing between commits
const LANE_H: f64 = 48.0; // vertical spacing between branch lanes
const DOT_R: f64 = 6.5;
const TAG_SPACE: f64 = 26.0; // headroom above the top lane for tags
const ID_SPACE: f64 = 22.0; // room below the bottom lane for ids
const NAME_PAD: f64 = 14.0;

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let m = parse(src);
    if m.commits.is_empty() {
        return Err(MermaidError("no commits"));
    }
    Ok(render(&m))
}

// ---- model ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Normal,
    Reverse,
    Highlight,
    Merge,
    CherryPick,
}

struct Commit {
    branch: usize,
    seq: usize,
    id: Option<String>,
    tag: Option<String>,
    kind: Kind,
    /// Previous commit on this branch, or the branch point it grew from.
    parent: Option<usize>,
    /// For merges: the tip of the branch being merged in.
    merge_parent: Option<usize>,
}

struct Model {
    branches: Vec<String>,
    commits: Vec<Commit>,
}

/// The double-quoted value following `key` (e.g. `id:"abc"` → `abc`), if present.
fn quoted_after(s: &str, key: &str) -> Option<String> {
    let pos = s.find(key)?;
    let after = s[pos + key.len()..].trim_start();
    let after = after.strip_prefix('"')?;
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// The bare word following `key` (e.g. `type: HIGHLIGHT`), if present.
fn word_after(s: &str, key: &str) -> Option<String> {
    let pos = s.find(key)?;
    let after = s[pos + key.len()..].trim_start();
    let w: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!w.is_empty()).then_some(w)
}

fn kind_from(rest: &str) -> Kind {
    match word_after(rest, "type:").as_deref() {
        Some("REVERSE") => Kind::Reverse,
        Some("HIGHLIGHT") => Kind::Highlight,
        _ => Kind::Normal,
    }
}

fn parse(src: &str) -> Model {
    let mut m = Model {
        branches: vec!["main".to_string()],
        commits: Vec::new(),
    };
    let mut head: Vec<Option<usize>> = vec![None]; // per-branch tip commit
    let mut current = 0usize;
    let mut seq = 0usize;

    let find_branch = |branches: &[String], name: &str| branches.iter().position(|b| b == name);

    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    lines.next(); // header (`gitGraph`, maybe with a `LR:`/`TB:` direction)

    for line in lines {
        // `commit` must be tested before `checkout`/others via exact prefixes.
        if line == "commit" || line.starts_with("commit ") {
            let rest = &line["commit".len()..];
            let idx = m.commits.len();
            m.commits.push(Commit {
                branch: current,
                seq,
                id: quoted_after(rest, "id:"),
                tag: quoted_after(rest, "tag:"),
                kind: kind_from(rest),
                parent: head[current],
                merge_parent: None,
            });
            seq += 1;
            head[current] = Some(idx);
        } else if let Some(rest) = line.strip_prefix("branch ") {
            let name = rest.split_whitespace().next().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            match find_branch(&m.branches, name) {
                Some(i) => current = i, // re-entering an existing branch
                None => {
                    m.branches.push(name.to_string());
                    head.push(head[current]); // grows from the current tip
                    current = m.branches.len() - 1;
                }
            }
        } else if let Some(rest) = line
            .strip_prefix("checkout ")
            .or_else(|| line.strip_prefix("switch "))
        {
            let name = rest.split_whitespace().next().unwrap_or("");
            if let Some(i) = find_branch(&m.branches, name) {
                current = i;
            }
        } else if let Some(rest) = line.strip_prefix("merge ") {
            let name = rest.split_whitespace().next().unwrap_or("");
            let Some(src_branch) = find_branch(&m.branches, name) else {
                continue;
            };
            let idx = m.commits.len();
            m.commits.push(Commit {
                branch: current,
                seq,
                id: quoted_after(rest, "id:"),
                tag: quoted_after(rest, "tag:"),
                kind: Kind::Merge,
                parent: head[current],
                merge_parent: head[src_branch],
            });
            seq += 1;
            head[current] = Some(idx);
        } else if let Some(rest) = line.strip_prefix("cherry-pick") {
            let idx = m.commits.len();
            m.commits.push(Commit {
                branch: current,
                seq,
                id: quoted_after(rest, "id:"),
                tag: quoted_after(rest, "tag:"),
                kind: Kind::CherryPick,
                parent: head[current],
                merge_parent: None,
            });
            seq += 1;
            head[current] = Some(idx);
        }
        // Any other directive (`gitGraph` config, unknown keywords) is ignored.
    }
    m
}

// ---- rendering -----------------------------------------------------------------

fn text_w(s: &str) -> f64 {
    s.chars().count() as f64 * CHAR_W
}

fn render(m: &Model) -> String {
    let gutter = m
        .branches
        .iter()
        .map(|b| text_w(b) + NAME_PAD)
        .fold(0.0, f64::max)
        .max(48.0);
    let left0 = MARGIN + gutter + COL_W / 2.0;
    let top0 = MARGIN + TAG_SPACE + LANE_H / 2.0;

    let x_of = |seq: usize| left0 + seq as f64 * COL_W;
    let y_of = |branch: usize| top0 + branch as f64 * LANE_H;

    let max_seq = m.commits.iter().map(|c| c.seq).max().unwrap_or(0);
    let width = x_of(max_seq) + COL_W / 2.0 + MARGIN;
    let height =
        top0 + (m.branches.len().saturating_sub(1)) as f64 * LANE_H + LANE_H / 2.0 + ID_SPACE;

    let color_of = |branch: usize| PALETTE[branch % PALETTE.len()];

    let mut out = String::with_capacity(512 + m.commits.len() * 200);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid catalerum-gitgraph\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {width:.1} {height:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"12\">"
    ));

    // Per-branch lane baseline spanning its commits, plus a left-gutter name.
    for (b, name) in m.branches.iter().enumerate() {
        let seqs: Vec<usize> = m
            .commits
            .iter()
            .filter(|c| c.branch == b)
            .map(|c| c.seq)
            .collect();
        let Some(&min) = seqs.iter().min() else {
            continue; // an empty branch draws nothing
        };
        let max = *seqs.iter().max().unwrap();
        let y = y_of(b);
        let color = color_of(b);
        out.push_str(&format!(
            "<line x1=\"{:.1}\" y1=\"{y:.1}\" x2=\"{:.1}\" y2=\"{y:.1}\" stroke=\"{color}\" \
             stroke-width=\"2.5\"/>",
            x_of(min),
            x_of(max)
        ));
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"{color}\" font-weight=\"bold\">",
            MARGIN,
            y + 4.0
        ));
        escape_text(&mut out, name);
        out.push_str("</text>");
    }

    // Branch-creation and merge edges (curves between lanes).
    for c in &m.commits {
        let cx = x_of(c.seq);
        let cy = y_of(c.branch);
        if let Some(p) = c.parent {
            let pc = &m.commits[p];
            if pc.branch != c.branch {
                curve(
                    &mut out,
                    x_of(pc.seq),
                    y_of(pc.branch),
                    cx,
                    cy,
                    color_of(c.branch),
                );
            }
        }
        if let Some(mp) = c.merge_parent {
            let sc = &m.commits[mp];
            curve(
                &mut out,
                x_of(sc.seq),
                y_of(sc.branch),
                cx,
                cy,
                color_of(sc.branch),
            );
        }
    }

    // Commit markers on top, then tags (above) and ids (below).
    for c in &m.commits {
        let cx = x_of(c.seq);
        let cy = y_of(c.branch);
        emit_marker(&mut out, c.kind, cx, cy, color_of(c.branch));

        if let Some(tag) = &c.tag {
            let tw = text_w(tag) + 10.0;
            out.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{tw:.1}\" height=\"15\" rx=\"3\" \
                 fill=\"#fff7ed\" stroke=\"#f59e0b\"/>",
                cx - tw / 2.0,
                cy - DOT_R - 19.0
            ));
            out.push_str(&format!(
                "<text x=\"{cx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#b45309\" \
                 font-size=\"11\">",
                cy - DOT_R - 8.0
            ));
            escape_text(&mut out, tag);
            out.push_str("</text>");
        }
        if let Some(id) = &c.id {
            out.push_str(&format!(
                "<text x=\"{cx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#64748b\" \
                 font-size=\"11\">",
                cy + DOT_R + 13.0
            ));
            escape_text(&mut out, id);
            out.push_str("</text>");
        }
    }

    out.push_str("</svg>");
    out
}

fn curve(out: &mut String, x1: f64, y1: f64, x2: f64, y2: f64, color: &str) {
    let mid = (x1 + x2) / 2.0;
    out.push_str(&format!(
        "<path d=\"M{x1:.1},{y1:.1} C{mid:.1},{y1:.1} {mid:.1},{y2:.1} {x2:.1},{y2:.1}\" \
         fill=\"none\" stroke=\"{color}\" stroke-width=\"2.5\"/>"
    ));
}

fn emit_marker(out: &mut String, kind: Kind, cx: f64, cy: f64, color: &str) {
    match kind {
        Kind::Normal => out.push_str(&format!(
            "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"{DOT_R:.1}\" fill=\"{color}\"/>"
        )),
        Kind::Highlight => {
            let s = DOT_R + 2.5;
            out.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"2\" \
                 fill=\"{color}\" stroke=\"#1e293b\" stroke-width=\"2\"/>",
                cx - s,
                cy - s,
                2.0 * s,
                2.0 * s
            ));
        }
        Kind::Reverse => {
            out.push_str(&format!(
                "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"{DOT_R:.1}\" fill=\"{color}\"/>"
            ));
            let d = DOT_R * 0.55;
            out.push_str(&format!(
                "<path d=\"M{:.1},{:.1} L{:.1},{:.1} M{:.1},{:.1} L{:.1},{:.1}\" stroke=\"#fff\" \
                 stroke-width=\"1.6\"/>",
                cx - d,
                cy - d,
                cx + d,
                cy + d,
                cx - d,
                cy + d,
                cx + d,
                cy - d
            ));
        }
        Kind::Merge => {
            out.push_str(&format!(
                "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"{:.1}\" fill=\"#fff\" stroke=\"{color}\" \
                 stroke-width=\"2.5\"/>",
                DOT_R + 1.0
            ));
            out.push_str(&format!(
                "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"{:.1}\" fill=\"{color}\"/>",
                DOT_R - 2.5
            ));
        }
        Kind::CherryPick => {
            let r = DOT_R + 1.0;
            out.push_str(&format!(
                "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" fill=\"{color}\" \
                 stroke=\"#fff\" stroke-width=\"1.5\"/>",
                cx,
                cy - r,
                cx + r,
                cy,
                cx,
                cy + r,
                cx - r,
                cy
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Kind};
    use crate::mermaid::to_svg;

    #[test]
    fn parses_commits_branches_and_merge() {
        let m = parse(
            "gitGraph\n commit\n commit\n branch develop\n commit\n checkout main\n merge develop",
        );
        // main + develop.
        assert_eq!(m.branches.len(), 2);
        assert_eq!(m.branches[0], "main");
        assert_eq!(m.branches[1], "develop");
        // 4 commits: two on main, one on develop, one merge on main.
        assert_eq!(m.commits.len(), 4);
        let merge = m.commits.last().unwrap();
        assert!(merge.kind == Kind::Merge, "last is a merge");
        assert_eq!(merge.branch, 0, "merge lands on main");
        assert!(merge.merge_parent.is_some(), "merge links the develop tip");
    }

    #[test]
    fn renders_lanes_dots_tags_and_ids() {
        let svg = to_svg(
            "gitGraph\n commit id: \"init\" tag: \"v1.0\"\n branch dev\n commit\n \
             commit type: HIGHLIGHT\n checkout main\n merge dev tag: \"release\"",
        )
        .unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        assert!(svg.contains("catalerum-gitgraph"), "class: {svg}");
        // Two branch names on the left.
        assert!(
            svg.contains(">main</text>") && svg.contains(">dev</text>"),
            "branch labels: {svg}"
        );
        // Tags rendered.
        assert!(
            svg.contains(">v1.0</text>") && svg.contains(">release</text>"),
            "tags: {svg}"
        );
        // Explicit commit id rendered.
        assert!(svg.contains(">init</text>"), "id label: {svg}");
        // Two distinct branch lane colours.
        assert!(
            svg.contains("#3b82f6") && svg.contains("#10b981"),
            "lane colours: {svg}"
        );
        // Highlight commit → a <rect> marker exists.
        assert!(svg.contains("<rect"), "highlight marker: {svg}");
        // Merge edge back to the other lane → at least one curve.
        assert!(svg.contains("<path"), "merge/branch edges: {svg}");
    }

    #[test]
    fn checkout_to_an_earlier_branch_targets_the_right_lane() {
        // Two branches; the final commit after checking out main lands on lane 0.
        let m = parse("gitGraph\n commit\n branch feat\n commit\n checkout main\n commit");
        let last = m.commits.last().unwrap();
        assert_eq!(last.branch, 0, "commit after checkout main is on main");
    }

    #[test]
    fn switch_is_an_alias_for_checkout() {
        let m = parse("gitGraph\n commit\n branch feat\n commit\n switch main\n commit");
        assert_eq!(m.commits.last().unwrap().branch, 0);
    }

    #[test]
    fn cherry_pick_and_reverse_render_distinct_markers() {
        let svg = to_svg(
            "gitGraph\n commit\n commit type: REVERSE\n branch x\n commit\n \
             checkout main\n cherry-pick id: \"abc\"",
        )
        .unwrap();
        // cherry-pick → a diamond polygon.
        assert!(svg.contains("<polygon"), "cherry-pick diamond: {svg}");
        assert!(svg.contains(">abc</text>"), "cherry-pick id: {svg}");
    }

    #[test]
    fn tags_and_ids_are_escaped() {
        let svg = to_svg("gitGraph\n commit id: \"<i>x</i>\" tag: \"<b>t</b>\"").unwrap();
        assert!(!svg.contains("<i>x") && !svg.contains("<b>t"), "{svg}");
        assert!(
            svg.contains("&lt;i&gt;x") && svg.contains("&lt;b&gt;t"),
            "{svg}"
        );
    }

    #[test]
    fn direction_token_is_accepted_and_ignored() {
        // `TB:` is parsed as the header, still yields a horizontal render.
        let svg = to_svg("gitGraph TB:\n commit\n commit").unwrap();
        assert!(
            svg.contains("<svg") && svg.matches("<circle").count() >= 2,
            "{svg}"
        );
    }

    #[test]
    fn empty_gitgraph_is_unsupported() {
        assert!(to_svg("gitGraph").is_err());
        assert!(to_svg("gitGraph\n checkout main").is_err());
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "gitGraph",
            "gitGraph\n commit id:",
            "gitGraph\n commit id: \"unterminated",
            "gitGraph\n branch",
            "gitGraph\n merge nonexistent",
            "gitGraph\n checkout ghost\n commit",
            "gitGraph\n cherry-pick",
            "gitGraph\n commit type:",
            "gitGraph\n branch a\n branch a\n commit",
        ] {
            let _ = to_svg(s);
        }
    }
}
