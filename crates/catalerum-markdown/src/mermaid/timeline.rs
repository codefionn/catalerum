//! Timeline diagrams (`timeline`) → SVG: a left-to-right chronology of time
//! periods, each with a label on a shared axis and one or more event cards stacked
//! below it. Optional `section`s group consecutive periods under a coloured band
//! (and tint their event cards). A period line is `TimeLabel : event : event …`.
//! All text is escaped.

use super::{MermaidError, PALETTE};
use crate::escape::escape_text;

const MARGIN: f64 = 16.0;
const CHAR_W: f64 = 7.2;
const COL_GAP: f64 = 18.0;
const CARD_H: f64 = 30.0;
const CARD_GAP: f64 = 7.0;
const CARD_PAD: f64 = 16.0;
const MAX_CARD_W: f64 = 220.0;

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let model = parse(src);
    if model.periods.is_empty() {
        return Err(MermaidError("no periods"));
    }
    Ok(render(&model))
}

// ---- model ---------------------------------------------------------------------

struct Period {
    time: String,
    events: Vec<String>,
    section: Option<usize>,
}

struct Model {
    title: Option<String>,
    sections: Vec<String>,
    periods: Vec<Period>,
}

fn parse(src: &str) -> Model {
    let mut model = Model {
        title: None,
        sections: Vec::new(),
        periods: Vec::new(),
    };
    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    lines.next(); // header (`timeline`)

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
        // A period: `TimeLabel : event : event …` (a bare label with no `:` is a
        // period with no events).
        let mut parts = line.split(':').map(str::trim);
        let time = parts.next().unwrap_or("").to_string();
        if time.is_empty() {
            continue;
        }
        let events: Vec<String> = parts
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .collect();
        model.periods.push(Period {
            time,
            events,
            section,
        });
    }
    model
}

// ---- rendering -----------------------------------------------------------------

fn card_width(text: &str) -> f64 {
    (text.chars().count() as f64 * CHAR_W + CARD_PAD).clamp(64.0, MAX_CARD_W)
}

fn render(m: &Model) -> String {
    let n = m.periods.len();
    // Column width per period: the wider of its time label and its widest event.
    let col_w: Vec<f64> = m
        .periods
        .iter()
        .map(|p| {
            let label = p.time.chars().count() as f64 * CHAR_W + CARD_PAD;
            let widest = p.events.iter().map(|e| card_width(e)).fold(0.0, f64::max);
            label.max(widest).max(70.0)
        })
        .collect();

    // Column centres, left to right.
    let mut centre = vec![0.0; n];
    let mut x = MARGIN;
    for i in 0..n {
        centre[i] = x + col_w[i] / 2.0;
        x += col_w[i] + COL_GAP;
    }
    let width = x - COL_GAP + MARGIN;

    let has_sections = m.periods.iter().any(|p| p.section.is_some());
    let max_events = m.periods.iter().map(|p| p.events.len()).max().unwrap_or(0);

    // Vertical bands (top → bottom).
    let mut y = MARGIN;
    let title_y = y;
    if m.title.is_some() {
        y += 30.0;
    }
    let band_y = y;
    if has_sections {
        y += 30.0;
    }
    let label_y = y; // baseline area for the time labels
    y += 24.0;
    let axis_y = y;
    y += 16.0;
    let events_y = y; // first event card top
    let height = events_y + max_events as f64 * (CARD_H + CARD_GAP) + MARGIN;

    let mut out = String::with_capacity(512 + n * 160 + max_events * n * 80);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid\" xmlns=\"http://www.w3.org/2000/svg\" \
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

    // Section bands spanning each run of consecutive periods in the same section.
    if has_sections {
        let mut i = 0;
        while i < n {
            let sec = m.periods[i].section;
            let mut j = i;
            while j < n && m.periods[j].section == sec {
                j += 1;
            }
            if let Some(si) = sec {
                let left = centre[i] - col_w[i] / 2.0;
                let right = centre[j - 1] + col_w[j - 1] / 2.0;
                let color = PALETTE[si % PALETTE.len()];
                out.push_str(&format!(
                    "<rect x=\"{left:.1}\" y=\"{band_y:.1}\" width=\"{:.1}\" height=\"24\" rx=\"5\" \
                     fill=\"{color}\" opacity=\"0.18\"/>",
                    right - left
                ));
                out.push_str(&format!(
                    "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-weight=\"bold\" \
                     font-size=\"13\" fill=\"{color}\">",
                    (left + right) / 2.0,
                    band_y + 16.0
                ));
                escape_text(&mut out, &m.sections[si]);
                out.push_str("</text>");
            }
            i = j;
        }
    }

    // The shared axis line with a dot at each period.
    out.push_str(&format!(
        "<line x1=\"{:.1}\" y1=\"{axis_y:.1}\" x2=\"{:.1}\" y2=\"{axis_y:.1}\" \
         stroke=\"#cbd5e1\" stroke-width=\"2\"/>",
        MARGIN,
        width - MARGIN
    ));

    for (i, p) in m.periods.iter().enumerate() {
        let cx = centre[i];
        let color = p
            .section
            .map(|s| PALETTE[s % PALETTE.len()])
            .unwrap_or("#3b82f6");

        // Time label above the axis.
        out.push_str(&format!(
            "<text x=\"{cx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-weight=\"bold\" \
             fill=\"#334155\">",
            label_y + 16.0
        ));
        escape_text(&mut out, &p.time);
        out.push_str("</text>");

        // Axis dot.
        out.push_str(&format!(
            "<circle cx=\"{cx:.1}\" cy=\"{axis_y:.1}\" r=\"4.5\" fill=\"{color}\"/>"
        ));

        // Event cards stacked below the axis, centred on the column.
        for (k, ev) in p.events.iter().enumerate() {
            let cw = card_width(ev);
            let cardx = cx - cw / 2.0;
            let cardy = events_y + k as f64 * (CARD_H + CARD_GAP);
            out.push_str(&format!(
                "<rect x=\"{cardx:.1}\" y=\"{cardy:.1}\" width=\"{cw:.1}\" height=\"{CARD_H:.1}\" \
                 rx=\"5\" fill=\"{color}\" opacity=\"0.14\"/>"
            ));
            out.push_str(&format!(
                "<rect x=\"{cardx:.1}\" y=\"{cardy:.1}\" width=\"{cw:.1}\" height=\"{CARD_H:.1}\" \
                 rx=\"5\" fill=\"none\" stroke=\"{color}\" stroke-width=\"1.2\"/>"
            ));
            out.push_str(&format!(
                "<text x=\"{cx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#1e293b\">",
                cardy + CARD_H / 2.0 + 4.0
            ));
            escape_text(&mut out, ev);
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
    fn renders_periods_events_and_a_title() {
        let svg = to_svg(
            "timeline\n title History\n 2002 : LinkedIn\n 2004 : Facebook : Google\n 2005 : YouTube",
        )
        .unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        assert!(svg.contains(">History</text>"), "title: {svg}");
        assert!(
            svg.contains(">2002</text>") && svg.contains(">2005</text>"),
            "periods: {svg}"
        );
        assert!(
            svg.contains(">LinkedIn</text>") && svg.contains(">YouTube</text>"),
            "events: {svg}"
        );
        // The `2004` period has two events on one line.
        assert!(
            svg.contains(">Facebook</text>") && svg.contains(">Google</text>"),
            "multi-event: {svg}"
        );
        // One axis dot per period.
        assert_eq!(
            svg.matches("<circle").count(),
            3,
            "one dot per period: {svg}"
        );
    }

    #[test]
    fn sections_band_and_tint_events() {
        let svg =
            to_svg("timeline\n section 2000s\n  2002 : A\n  2004 : B\n section 2010s\n  2010 : C")
                .unwrap();
        assert!(
            svg.contains(">2000s</text>") && svg.contains(">2010s</text>"),
            "section labels: {svg}"
        );
        // Two section bands (one per run), tinted via opacity.
        assert!(
            svg.matches("opacity=\"0.18\"").count() == 2,
            "two section bands: {svg}"
        );
        // Distinct palette colours for the two sections.
        assert!(
            svg.contains("#3b82f6") && svg.contains("#10b981"),
            "palette colours: {svg}"
        );
    }

    #[test]
    fn period_without_events_still_places_a_dot() {
        let svg = to_svg("timeline\n 2002\n 2004 : Something").unwrap();
        assert_eq!(svg.matches("<circle").count(), 2, "{svg}");
        assert!(svg.contains(">2002</text>"), "{svg}");
    }

    #[test]
    fn labels_are_escaped() {
        let svg = to_svg("timeline\n title <t>\n 2002 : <script>x").unwrap();
        assert!(!svg.contains("<script>") && !svg.contains("<t>"), "{svg}");
        assert!(
            svg.contains("&lt;script&gt;") && svg.contains("&lt;t&gt;"),
            "{svg}"
        );
    }

    #[test]
    fn empty_timeline_is_unsupported() {
        assert!(to_svg("timeline\n title Nothing").is_err());
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "timeline",
            "timeline\n title",
            "timeline\n section",
            "timeline\n :",
            "timeline\n : event with no time",
            "timeline\n 2002 : : :",
            "timeline\n section S\n section T",
        ] {
            let _ = to_svg(s);
        }
    }
}
