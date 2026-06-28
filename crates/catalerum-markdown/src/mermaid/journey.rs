//! User-journey diagrams (`journey`) → SVG: a left-to-right sequence of tasks,
//! each scored 1–5 for satisfaction, plotted as points on a shared 1–5 axis and
//! joined into the journey's satisfaction curve. Consecutive tasks in the same
//! `section` sit under a coloured band; each point is tinted red→green by its
//! score, and its actors are listed under the task label. A task line is
//! `Task name : <score> : Actor, Actor …`. All text is escaped.

use super::{MermaidError, PALETTE};
use crate::escape::escape_text;

const MARGIN: f64 = 16.0;
const CHAR_W: f64 = 7.2;
const COL_GAP: f64 = 20.0;
const GUTTER: f64 = 26.0; // left gutter for the 1–5 score-axis numbers
const ROW_GAP: f64 = 34.0; // vertical distance between adjacent score levels
const TEXT_PAD: f64 = 14.0;

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let model = parse(src);
    if model.tasks.is_empty() {
        return Err(MermaidError("no journey tasks"));
    }
    Ok(render(&model))
}

// ---- model ---------------------------------------------------------------------

struct Task {
    name: String,
    score: i32, // clamped to 1..=5
    actors: Vec<String>,
    section: Option<usize>,
}

struct Model {
    title: Option<String>,
    sections: Vec<String>,
    tasks: Vec<Task>,
}

fn parse(src: &str) -> Model {
    let mut model = Model {
        title: None,
        sections: Vec::new(),
        tasks: Vec::new(),
    };
    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    lines.next(); // header (`journey`)

    let mut section: Option<usize> = None;
    for line in lines {
        if let Some(t) = line.strip_prefix("title ") {
            model.title = Some(t.trim().to_string());
            continue;
        }
        if line == "title" || line == "section" {
            continue;
        }
        if let Some(s) = line.strip_prefix("section ") {
            model.sections.push(s.trim().to_string());
            section = Some(model.sections.len() - 1);
            continue;
        }
        // A task: `name : score : actor, actor …`. A line without a parseable
        // score isn't a task (it's malformed) and is skipped.
        let mut parts = line.splitn(3, ':');
        let name = parts.next().unwrap_or("").trim().to_string();
        let Some(score) = parts.next().and_then(|s| s.trim().parse::<i32>().ok()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let actors = parts
            .next()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect();
        model.tasks.push(Task {
            name,
            score: score.clamp(1, 5),
            actors,
            section,
        });
    }
    model
}

// ---- rendering -----------------------------------------------------------------

fn text_w(s: &str) -> f64 {
    s.chars().count() as f64 * CHAR_W
}

/// Red→green tint for a 1–5 satisfaction score.
fn score_color(score: i32) -> &'static str {
    match score.clamp(1, 5) {
        1 => "#ef4444",
        2 => "#f97316",
        3 => "#f59e0b",
        4 => "#84cc16",
        _ => "#10b981",
    }
}

fn render(m: &Model) -> String {
    let n = m.tasks.len();
    // Column width per task: the wider of its name and its actor line.
    let col_w: Vec<f64> = m
        .tasks
        .iter()
        .map(|t| {
            let name = text_w(&t.name) + TEXT_PAD;
            let actors = text_w(&t.actors.join(", ")) + TEXT_PAD;
            name.max(actors).max(64.0)
        })
        .collect();

    // Column centres, left to right (after the score-axis gutter).
    let mut centre = vec![0.0; n];
    let mut x = GUTTER + MARGIN;
    for i in 0..n {
        centre[i] = x + col_w[i] / 2.0;
        x += col_w[i] + COL_GAP;
    }
    let width = x - COL_GAP + MARGIN;

    let has_sections = m.tasks.iter().any(|t| t.section.is_some());

    // Vertical bands (top → bottom).
    let mut y = MARGIN;
    let title_y = y;
    if m.title.is_some() {
        y += 30.0;
    }
    let band_y = y;
    if has_sections {
        y += 26.0;
    }
    let chart_top = y + 8.0; // score 5 gridline
    let chart_bottom = chart_top + 4.0 * ROW_GAP; // score 1 gridline
    let label_y = chart_bottom + 22.0; // task name baseline
    let actors_y = label_y + 15.0; // actor line baseline
    let height = actors_y + MARGIN;

    // Map a 1–5 score to a y coordinate (5 at the top).
    let y_for = |score: i32| chart_top + f64::from(5 - score.clamp(1, 5)) * ROW_GAP;

    let mut out = String::with_capacity(512 + n * 200);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid catalerum-journey\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {width:.1} {height:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"13\">"
    ));

    if let Some(title) = &m.title {
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"16\" \
             font-weight=\"bold\" fill=\"#1e293b\">",
            width / 2.0,
            title_y + 20.0
        ));
        escape_text(&mut out, title);
        out.push_str("</text>");
    }

    // Section bands spanning each run of consecutive tasks in the same section.
    if has_sections {
        let mut i = 0;
        while i < n {
            let sec = m.tasks[i].section;
            let mut j = i;
            while j < n && m.tasks[j].section == sec {
                j += 1;
            }
            if let Some(si) = sec {
                let left = centre[i] - col_w[i] / 2.0;
                let right = centre[j - 1] + col_w[j - 1] / 2.0;
                let color = PALETTE[si % PALETTE.len()];
                out.push_str(&format!(
                    "<rect x=\"{left:.1}\" y=\"{band_y:.1}\" width=\"{:.1}\" height=\"20\" rx=\"5\" \
                     fill=\"{color}\" opacity=\"0.18\"/>",
                    right - left
                ));
                out.push_str(&format!(
                    "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-weight=\"bold\" \
                     font-size=\"12\" fill=\"{color}\">",
                    (left + right) / 2.0,
                    band_y + 14.0
                ));
                escape_text(&mut out, &m.sections[si]);
                out.push_str("</text>");
            }
            i = j;
        }
    }

    // Score-axis gridlines (1–5) with a number in the left gutter.
    for score in 1..=5 {
        let gy = y_for(score);
        out.push_str(&format!(
            "<line x1=\"{:.1}\" y1=\"{gy:.1}\" x2=\"{:.1}\" y2=\"{gy:.1}\" stroke=\"#e2e8f0\" \
             stroke-width=\"1\"/>",
            GUTTER,
            width - MARGIN
        ));
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#94a3b8\" \
             font-size=\"11\">{score}</text>",
            GUTTER / 2.0 + 2.0,
            gy + 4.0
        ));
    }

    // The satisfaction curve: a polyline through each task's (column, score) point.
    if n >= 2 {
        let points: Vec<String> = m
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{:.1},{:.1}", centre[i], y_for(t.score)))
            .collect();
        out.push_str(&format!(
            "<polyline points=\"{}\" fill=\"none\" stroke=\"#64748b\" stroke-width=\"2\"/>",
            points.join(" ")
        ));
    }

    // Task points, labels and actors.
    for (i, t) in m.tasks.iter().enumerate() {
        let cx = centre[i];
        let py = y_for(t.score);
        out.push_str(&format!(
            "<circle cx=\"{cx:.1}\" cy=\"{py:.1}\" r=\"6\" fill=\"{}\" stroke=\"#ffffff\" \
             stroke-width=\"1.5\"/>",
            score_color(t.score)
        ));
        // Task name under the chart.
        out.push_str(&format!(
            "<text x=\"{cx:.1}\" y=\"{label_y:.1}\" text-anchor=\"middle\" fill=\"#334155\" \
             font-weight=\"600\">"
        ));
        escape_text(&mut out, &t.name);
        out.push_str("</text>");
        // Actors, muted, under the name.
        if !t.actors.is_empty() {
            out.push_str(&format!(
                "<text x=\"{cx:.1}\" y=\"{actors_y:.1}\" text-anchor=\"middle\" fill=\"#94a3b8\" \
                 font-size=\"11\">"
            ));
            escape_text(&mut out, &t.actors.join(", "));
            out.push_str("</text>");
        }
    }

    out.push_str("</svg>");
    out
}

#[cfg(test)]
mod tests {
    use crate::mermaid::to_svg;

    #[test]
    fn renders_tasks_scores_actors_and_a_curve() {
        let svg = to_svg(
            "journey\n title My day\n section Work\n  Make tea: 5: Me\n  Do work: 1: Me, Cat\n \
             section Home\n  Sit down: 5: Me",
        )
        .unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        assert!(svg.contains(">My day</text>"), "title: {svg}");
        // Section bands.
        assert!(
            svg.contains(">Work</text>") && svg.contains(">Home</text>"),
            "sections: {svg}"
        );
        // Task labels + actors.
        assert!(
            svg.contains(">Make tea</text>") && svg.contains(">Do work</text>"),
            "tasks: {svg}"
        );
        assert!(svg.contains(">Me, Cat</text>"), "multi-actor line: {svg}");
        // One point per task (3 tasks) + the satisfaction curve.
        assert_eq!(
            svg.matches("<circle").count(),
            3,
            "one point per task: {svg}"
        );
        assert!(svg.contains("<polyline"), "satisfaction curve: {svg}");
        // The score-1 task is tinted red, the score-5 tasks green.
        assert!(
            svg.contains("#ef4444") && svg.contains("#10b981"),
            "score tints: {svg}"
        );
    }

    #[test]
    fn out_of_range_scores_are_clamped_and_a_single_task_has_no_curve() {
        // score 9 clamps to 5 (green), no section, single task → no polyline.
        let svg = to_svg("journey\n Solo: 9: A").unwrap();
        assert_eq!(svg.matches("<circle").count(), 1, "{svg}");
        assert!(
            !svg.contains("<polyline"),
            "single point draws no line: {svg}"
        );
        assert!(svg.contains("#10b981"), "score 9 clamps to green: {svg}");
    }

    #[test]
    fn labels_and_actors_are_escaped() {
        let svg = to_svg("journey\n <b>task</b>: 3: <i>actor</i>").unwrap();
        assert!(
            !svg.contains("<b>task") && !svg.contains("<i>actor"),
            "{svg}"
        );
        assert!(
            svg.contains("&lt;b&gt;task") && svg.contains("&lt;i&gt;actor"),
            "{svg}"
        );
    }

    #[test]
    fn a_journey_with_no_valid_tasks_is_unsupported() {
        assert!(to_svg("journey\n title Nothing").is_err());
        // A line without a numeric score isn't a task.
        assert!(to_svg("journey\n just some text").is_err());
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "journey",
            "journey\n title",
            "journey\n section",
            "journey\n :",
            "journey\n : 3 : x",
            "journey\n task: : me",
            "journey\n task: abc: me",
            "journey\n a: 3\n b: 4: x, , y",
        ] {
            let _ = to_svg(s);
        }
    }
}
