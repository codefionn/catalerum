//! Gantt charts (`gantt`) → SVG: a project timeline of tasks laid out on a date
//! axis, grouped into sections, with `done`/`active`/`crit` status colours and
//! `milestone` diamonds. Tasks carry a start (`YYYY-MM-DD`, `after <id>`, or
//! implicitly the previous task's end) and a length (`<n>d`/`w`/`h`/`m` or an end
//! date). Dates are handled with a small built-in civil-day calendar (the crate
//! pulls in no date library, to stay wasm-light). All text is escaped.

use std::collections::HashMap;

use super::MermaidError;
use crate::escape::escape_text;

const MARGIN: f64 = 16.0;
const ROW: f64 = 30.0;
const BAR_H: f64 = 16.0;
const AXIS_H: f64 = 26.0;
const CHAR_W: f64 = 7.4;
const TIMELINE_W: f64 = 460.0;

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let chart = parse(src);
    if chart.tasks.is_empty() {
        return Err(MermaidError("no tasks"));
    }
    Ok(render(&chart))
}

// ---- model ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Normal,
    Active,
    Done,
    Crit,
}

struct Task {
    label: String,
    section: Option<usize>,
    start: f64, // days (civil day-number, fractional for sub-day units)
    end: f64,
    status: Status,
    milestone: bool,
}

struct Chart {
    title: Option<String>,
    sections: Vec<String>,
    tasks: Vec<Task>,
}

// ---- parsing -------------------------------------------------------------------

fn parse(src: &str) -> Chart {
    let mut chart = Chart {
        title: None,
        sections: Vec::new(),
        tasks: Vec::new(),
    };
    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    lines.next(); // header (`gantt`)

    let mut section: Option<usize> = None;
    let mut ids: HashMap<String, f64> = HashMap::new(); // task id → end day
    let mut prev_end = 0.0f64;
    let mut have_prev = false;

    for line in lines {
        if let Some(t) = line.strip_prefix("title") {
            chart.title = Some(t.trim().to_string());
            continue;
        }
        if let Some(s) = line.strip_prefix("section") {
            chart.sections.push(s.trim().to_string());
            section = Some(chart.sections.len() - 1);
            continue;
        }
        // Directives we don't lay out (axis/date formatting, exclusions, markers).
        let kw = line.split_whitespace().next().unwrap_or("");
        if matches!(
            kw,
            "dateFormat" | "axisFormat" | "excludes" | "todayMarker" | "tickInterval" | "weekday"
        ) {
            continue;
        }
        // A task: `Label : meta, meta, …`.
        let Some((label, meta)) = line.split_once(':') else {
            continue;
        };
        let label = label.trim();
        if label.is_empty() {
            continue;
        }
        if let Some(task) = parse_task(
            label,
            meta,
            section,
            &ids,
            if have_prev { Some(prev_end) } else { None },
        ) {
            if let Some(id) = task_id(meta) {
                ids.insert(id, task.end);
            }
            prev_end = task.end;
            have_prev = true;
            chart.tasks.push(task);
        }
    }
    chart
}

/// The first metadata token that is a bare identifier (not a status tag, date,
/// `after …`, or duration) is the task's id, usable as an `after` target.
fn task_id(meta: &str) -> Option<String> {
    for tok in meta.split(',').map(str::trim) {
        if classify(tok) == Field::Id {
            return Some(tok.to_string());
        }
    }
    None
}

#[derive(PartialEq)]
enum Field {
    Tag,
    Date,
    After,
    Duration,
    Id,
    Empty,
}

fn classify(tok: &str) -> Field {
    if tok.is_empty() {
        Field::Empty
    } else if matches!(tok, "done" | "active" | "crit" | "milestone") {
        Field::Tag
    } else if parse_date(tok).is_some() {
        Field::Date
    } else if tok.starts_with("after ") {
        Field::After
    } else if parse_duration(tok).is_some() {
        Field::Duration
    } else {
        Field::Id
    }
}

fn parse_task(
    label: &str,
    meta: &str,
    section: Option<usize>,
    ids: &HashMap<String, f64>,
    prev_end: Option<f64>,
) -> Option<Task> {
    let mut status = Status::Normal;
    let mut milestone = false;
    let mut start: Option<f64> = None;
    let mut end: Option<f64> = None;
    let mut duration: Option<f64> = None;

    for tok in meta.split(',').map(str::trim) {
        match classify(tok) {
            Field::Tag => match tok {
                "done" => status = Status::Done,
                "active" => status = Status::Active,
                "crit" => status = Status::Crit,
                _ => milestone = true, // "milestone"
            },
            Field::Date => {
                let d = parse_date(tok)? as f64;
                if start.is_none() {
                    start = Some(d);
                } else {
                    end = Some(d);
                }
            }
            Field::After => {
                // `after a b` starts at the latest end of the referenced ids.
                let latest = tok[6..]
                    .split_whitespace()
                    .filter_map(|id| ids.get(id).copied())
                    .fold(f64::NEG_INFINITY, f64::max);
                if latest.is_finite() {
                    start = Some(latest);
                }
            }
            Field::Duration => duration = parse_duration(tok),
            Field::Id | Field::Empty => {}
        }
    }

    // Resolve the start: explicit/after, else right after the previous task.
    let start = start.or(prev_end).unwrap_or(0.0);
    // Resolve the end: explicit end date, else start + duration, else a zero-length
    // milestone, else a default single day.
    let end = match end {
        Some(e) => e,
        None => match (duration, milestone) {
            (Some(d), _) => start + d,
            (None, true) => start,
            (None, false) => start + 1.0,
        },
    };
    Some(Task {
        label: label.to_string(),
        section,
        start,
        end: end.max(start),
        status,
        milestone,
    })
}

/// Parse `YYYY-MM-DD` into a civil day-number (days since 1970-01-01; only
/// differences matter here).
fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Parse a duration token like `5d`, `2w`, `8h`, `90m` into days.
fn parse_duration(tok: &str) -> Option<f64> {
    let unit = tok.chars().last()?;
    let scale = match unit {
        'd' => 1.0,
        'w' => 7.0,
        'h' => 1.0 / 24.0,
        'm' => 1.0 / 1440.0,
        _ => return None,
    };
    let n: f64 = tok[..tok.len() - 1].parse().ok()?;
    (n >= 0.0).then_some(n * scale)
}

/// Days since 1970-01-01 for a proleptic-Gregorian date (Howard Hinnant's
/// algorithm); paired with [`civil_from_days`] for the axis labels.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn fmt_date(day: f64) -> String {
    let (y, m, d) = civil_from_days(day.round() as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

// ---- rendering -----------------------------------------------------------------

fn render(c: &Chart) -> String {
    // Time range across all tasks (guard a zero/degenerate span).
    let min = c
        .tasks
        .iter()
        .map(|t| t.start)
        .fold(f64::INFINITY, f64::min);
    let max = c
        .tasks
        .iter()
        .map(|t| t.end)
        .fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).max(1.0);

    // Vertical sequence of rows: a section header precedes each new section's tasks.
    enum Item {
        Section(usize),
        Task(usize),
    }
    let mut items = Vec::new();
    let mut cur: Option<usize> = None;
    for (i, t) in c.tasks.iter().enumerate() {
        if t.section != cur {
            cur = t.section;
            if let Some(si) = t.section {
                items.push(Item::Section(si));
            }
        }
        items.push(Item::Task(i));
    }

    let label_w = c
        .tasks
        .iter()
        .map(|t| t.label.chars().count())
        .max()
        .unwrap_or(4) as f64
        * CHAR_W
        + 20.0;
    let label_w = label_w.clamp(110.0, 320.0);
    let tl_x0 = MARGIN + label_w + 12.0;
    let width = tl_x0 + TIMELINE_W + MARGIN;
    let title_h = if c.title.is_some() { 30.0 } else { 8.0 };
    let top = MARGIN + title_h;
    let height = top + items.len() as f64 * ROW + AXIS_H + MARGIN;

    let x_of = |day: f64| tl_x0 + (day - min) / span * TIMELINE_W;

    let mut out = String::with_capacity(512 + items.len() * 200);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {width:.1} {height:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"13\">"
    ));

    if let Some(title) = &c.title {
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"20\" text-anchor=\"middle\" font-size=\"16\" \
             font-weight=\"bold\" fill=\"#1e293b\">",
            width / 2.0
        ));
        escape_text(&mut out, title);
        out.push_str("</text>");
    }

    let chart_bottom = top + items.len() as f64 * ROW;
    // Faint timeline frame + start/end vertical guides.
    for gx in [tl_x0, width - MARGIN] {
        out.push_str(&format!(
            "<line x1=\"{gx:.1}\" y1=\"{top:.1}\" x2=\"{gx:.1}\" y2=\"{chart_bottom:.1}\" \
             stroke=\"#e2e8f0\" stroke-width=\"1\"/>"
        ));
    }

    let mut y = top;
    for item in &items {
        match item {
            Item::Section(si) => {
                out.push_str(&format!(
                    "<rect x=\"{:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{ROW:.1}\" \
                     fill=\"#eef2ff\"/>",
                    MARGIN,
                    width - 2.0 * MARGIN
                ));
                out.push_str(&format!(
                    "<text x=\"{:.1}\" y=\"{:.1}\" font-weight=\"bold\" fill=\"#475569\">",
                    MARGIN + 6.0,
                    y + ROW / 2.0 + 4.0
                ));
                escape_text(&mut out, &c.sections[*si]);
                out.push_str("</text>");
            }
            Item::Task(ti) => {
                render_task(&c.tasks[*ti], y, &x_of, &mut out);
            }
        }
        y += ROW;
    }

    // Axis: a baseline plus the start and end dates.
    out.push_str(&format!(
        "<line x1=\"{tl_x0:.1}\" y1=\"{chart_bottom:.1}\" x2=\"{:.1}\" y2=\"{chart_bottom:.1}\" \
         stroke=\"#cbd5e1\" stroke-width=\"1\"/>",
        width - MARGIN
    ));
    let ay = chart_bottom + 16.0;
    out.push_str(&format!(
        "<text x=\"{tl_x0:.1}\" y=\"{ay:.1}\" font-size=\"11\" fill=\"#64748b\">{}</text>",
        fmt_date(min)
    ));
    out.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{ay:.1}\" text-anchor=\"end\" font-size=\"11\" fill=\"#64748b\">{}</text>",
        width - MARGIN,
        fmt_date(max)
    ));

    out.push_str("</svg>");
    out
}

fn render_task(t: &Task, row_y: f64, x_of: &impl Fn(f64) -> f64, out: &mut String) {
    // Task label in the left gutter.
    out.push_str(&format!(
        "<text x=\"{MARGIN:.1}\" y=\"{:.1}\" fill=\"#1e293b\">",
        row_y + ROW / 2.0 + 4.0
    ));
    escape_text(out, &t.label);
    out.push_str("</text>");

    let cy = row_y + ROW / 2.0;
    if t.milestone {
        // A diamond at the start instant.
        let cx = x_of(t.start);
        let r = BAR_H / 2.0;
        out.push_str(&format!(
            "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" \
             fill=\"#6366f1\" stroke=\"#4338ca\" stroke-width=\"1\"/>",
            cx,
            cy - r,
            cx + r,
            cy,
            cx,
            cy + r,
            cx - r,
            cy
        ));
        return;
    }
    let (fill, stroke) = match t.status {
        Status::Normal => ("#bfdbfe", "#3b82f6"),
        Status::Active => ("#93c5fd", "#2563eb"),
        Status::Done => ("#e2e8f0", "#94a3b8"),
        Status::Crit => ("#fecaca", "#ef4444"),
    };
    let x1 = x_of(t.start);
    let x2 = x_of(t.end);
    let w = (x2 - x1).max(3.0);
    out.push_str(&format!(
        "<rect x=\"{x1:.1}\" y=\"{:.1}\" width=\"{w:.1}\" height=\"{BAR_H:.1}\" rx=\"3\" \
         fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1.2\"/>",
        cy - BAR_H / 2.0
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mermaid::to_svg;

    #[test]
    fn civil_day_round_trips() {
        for (y, m, d) in [(1970, 1, 1), (2014, 1, 1), (2000, 2, 29), (2023, 12, 31)] {
            let z = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(z), (y, m, d), "{y}-{m}-{d}");
        }
        // A 30-day task starting 2014-01-01 ends 2014-01-31.
        let start = days_from_civil(2014, 1, 1);
        assert_eq!(civil_from_days(start + 30), (2014, 1, 31));
    }

    #[test]
    fn renders_sections_tasks_and_a_title() {
        let svg = to_svg(
            "gantt\n title Project\n dateFormat YYYY-MM-DD\n section Build\n  Design :2014-01-01, 10d\n  Code :2014-01-11, 20d\n section Ship\n  Release :2014-02-01, 5d",
        )
        .unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        assert!(svg.contains(">Project</text>"), "title: {svg}");
        assert!(
            svg.contains(">Build</text>") && svg.contains(">Ship</text>"),
            "sections: {svg}"
        );
        assert!(
            svg.contains(">Design</text>") && svg.contains(">Release</text>"),
            "tasks: {svg}"
        );
        // Three task bars (rounded rects with rx).
        assert_eq!(
            svg.matches("rx=\"3\"").count(),
            3,
            "one bar per task: {svg}"
        );
        // Axis shows the overall span.
        assert!(svg.contains(">2014-01-01</text>"), "start label: {svg}");
        assert!(svg.contains(">2014-02-06</text>"), "end label: {svg}");
    }

    #[test]
    fn status_colours_and_milestone_diamond() {
        let svg = to_svg(
            "gantt\n  a :done, 2014-01-01, 3d\n  b :active, 2014-01-04, 3d\n  c :crit, 2014-01-07, 3d\n  m :milestone, 2014-01-10, 0d",
        )
        .unwrap();
        assert!(svg.contains("#e2e8f0"), "done colour: {svg}");
        assert!(svg.contains("#93c5fd"), "active colour: {svg}");
        assert!(svg.contains("#fecaca"), "crit colour: {svg}");
        // The milestone is a polygon (diamond), not a bar.
        assert!(svg.contains("<polygon"), "milestone diamond: {svg}");
        assert_eq!(
            svg.matches("rx=\"3\"").count(),
            3,
            "3 bars, milestone excluded: {svg}"
        );
    }

    #[test]
    fn after_dependency_and_implicit_start_chain() {
        // `b after a` starts when a (10d from Jan 1) ends; `c` with no start follows b.
        let svg = to_svg("gantt\n  a :a1, 2014-01-01, 10d\n  b :after a1, 5d\n  c :3d").unwrap();
        // End label is a(10) → b(5) → c(3) = 18 days after Jan 1 = Jan 19.
        assert!(svg.contains(">2014-01-19</text>"), "chained end: {svg}");
        assert_eq!(svg.matches("rx=\"3\"").count(), 3, "{svg}");
    }

    #[test]
    fn labels_are_escaped() {
        let svg = to_svg("gantt\n title <x>\n  <script> :2014-01-01, 1d").unwrap();
        assert!(!svg.contains("<script>") && !svg.contains("<x>"), "{svg}");
        assert!(
            svg.contains("&lt;script&gt;") && svg.contains("&lt;x&gt;"),
            "{svg}"
        );
    }

    #[test]
    fn empty_chart_is_unsupported() {
        assert!(to_svg("gantt\n title Nothing\n section Empty").is_err());
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "gantt",
            "gantt\n title",
            "gantt\n  task :",
            "gantt\n  task :,,,",
            "gantt\n  :2014-01-01, 5d",
            "gantt\n  t :after nope, 5d",
            "gantt\n  t :2014-13-40, 5d",
            "gantt\n  t :9999d",
            "gantt\n  t :milestone",
        ] {
            let _ = to_svg(s);
        }
    }
}
