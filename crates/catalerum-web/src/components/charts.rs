//! Dependency-free SVG data-visualisation primitives (SOUL §12).
//!
//! A small family of chart renderers — pie, donut, bar (vertical + horizontal),
//! line, area, sparkline, gauge, radar and heatmap — built directly on SVG the
//! same way [`super::flow`] draws the automation canvas: pure geometry helpers
//! (unit-tested) feeding a `view!` tree, every colour a theme token so charts
//! adapt to all palettes automatically (see the `--chart-N` tokens in
//! `crate::STYLE`).
//!
//! Each public renderer is a **pure `data → AnyView` function** (not a reactive
//! `#[component]`): it takes already-resolved owned data and returns a static
//! SVG. Reactivity is the caller's job — wrap a call in a `move ||` closure that
//! reads a signal and the chart re-renders on change (this is exactly how the
//! emerged-UI interpreter drives them, and how a panel would bind live data).
//! Mirrors the plain-`pub fn` idiom of [`super::widgets`].
//!
//! Every renderer is total: empty/degenerate/adversarial input yields a neutral
//! "no data" placeholder rather than a panic, and point/slice/cell counts are
//! capped so a huge array cannot explode the DOM.

use leptos::prelude::*;

/// One labelled datum for the categorical charts (pie, donut, bar) and the
/// series charts (line, area). `color` overrides the palette slot when set.
#[derive(Clone, Debug, PartialEq)]
pub struct Datum {
    /// Category / x-axis label (may be empty — charts fall back to the index).
    pub label: String,
    /// The magnitude.
    pub value: f64,
    /// Optional explicit CSS colour (else the palette is cycled).
    pub color: Option<String>,
}

impl Datum {
    /// A datum with no explicit colour.
    #[must_use]
    pub fn new(label: impl Into<String>, value: f64) -> Datum {
        Datum {
            label: label.into(),
            value,
            color: None,
        }
    }
}

/// Shared rendering knobs. Build with [`ChartOpts::default`] then the chained
/// setters, e.g. `ChartOpts::default().title("Spend").size(320.0, 180.0)`.
#[derive(Clone, Debug, PartialEq)]
pub struct ChartOpts {
    /// Optional caption rendered above the plot.
    pub title: Option<String>,
    /// SVG user-space width (the viewBox scales to the container).
    pub width: f64,
    /// SVG user-space height.
    pub height: f64,
    /// Categorical colour ramp, cycled per series/slice. CSS colours.
    pub palette: Vec<String>,
    /// Show a legend (honoured by pie/donut).
    pub legend: bool,
    /// Explicit value-axis maximum; `None` auto-scales to a "nice" ceiling.
    pub max: Option<f64>,
}

impl Default for ChartOpts {
    fn default() -> ChartOpts {
        ChartOpts {
            title: None,
            width: 360.0,
            height: 220.0,
            palette: default_palette(),
            legend: true,
            max: None,
        }
    }
}

impl ChartOpts {
    /// Set the caption (an empty string clears it).
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> ChartOpts {
        let title = title.into();
        self.title = (!title.trim().is_empty()).then_some(title);
        self
    }

    /// Set the SVG user-space size.
    #[must_use]
    pub fn size(mut self, width: f64, height: f64) -> ChartOpts {
        if width > 0.0 {
            self.width = width;
        }
        if height > 0.0 {
            self.height = height;
        }
        self
    }

    /// Override the colour ramp (ignored when empty, keeping the default).
    #[must_use]
    pub fn palette(mut self, palette: Vec<String>) -> ChartOpts {
        if !palette.is_empty() {
            self.palette = palette;
        }
        self
    }

    /// Toggle the legend.
    #[must_use]
    pub fn legend(mut self, on: bool) -> ChartOpts {
        self.legend = on;
        self
    }

    /// Pin the value-axis maximum (`None` restores auto-scaling).
    #[must_use]
    pub fn with_max(mut self, max: Option<f64>) -> ChartOpts {
        self.max = max.filter(|m| m.is_finite() && *m > 0.0);
        self
    }
}

/// The default eight-slot categorical ramp, each a `--chart-N` theme token so a
/// theme switch recolours every chart at once.
#[must_use]
pub fn default_palette() -> Vec<String> {
    (1..=8).map(|i| format!("var(--chart-{i})")).collect()
}

// Defensive caps: the emerged validator bounds spec size, but a `data` prop can
// still point at a large state array — bound what actually reaches the DOM.
const MAX_SLICES: usize = 48;
const MAX_POINTS: usize = 512;
const MAX_BARS: usize = 96;
const MAX_AXES: usize = 24;
const MAX_CELLS: usize = 2500;

// ---------------------------------------------------------------------------
// Geometry (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Format a coordinate for an SVG path/attribute: ≤2 decimals, trailing zeros
/// trimmed, non-finite coerced to `0` (so a NaN can never emit `"NaN"`).
fn num(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let s = format!("{v:.2}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-0" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Point on a circle, angle in degrees measured **clockwise from 12 o'clock**
/// (0° = top, 90° = right) — the natural convention for pies and gauges.
fn polar(cx: f64, cy: f64, r: f64, deg: f64) -> (f64, f64) {
    let rad = deg.to_radians();
    (cx + r * rad.sin(), cy - r * rad.cos())
}

/// A pie wedge path from the centre out to the arc `start`→`end` and back.
fn wedge_path(cx: f64, cy: f64, r: f64, start: f64, end: f64) -> String {
    let (x0, y0) = polar(cx, cy, r, start);
    let (x1, y1) = polar(cx, cy, r, end);
    let large = i32::from((end - start).abs() > 180.0);
    format!(
        "M {cx} {cy} L {x0} {y0} A {r} {r} 0 {large} 1 {x1} {y1} Z",
        cx = num(cx),
        cy = num(cy),
        x0 = num(x0),
        y0 = num(y0),
        r = num(r),
        x1 = num(x1),
        y1 = num(y1),
    )
}

/// A donut ring **segment** between radii `r_in`/`r_out` over `start`→`end`.
fn ring_path(cx: f64, cy: f64, r_out: f64, r_in: f64, start: f64, end: f64) -> String {
    let (ox0, oy0) = polar(cx, cy, r_out, start);
    let (ox1, oy1) = polar(cx, cy, r_out, end);
    let (ix1, iy1) = polar(cx, cy, r_in, end);
    let (ix0, iy0) = polar(cx, cy, r_in, start);
    let large = i32::from((end - start).abs() > 180.0);
    format!(
        "M {ox0} {oy0} A {ro} {ro} 0 {large} 1 {ox1} {oy1} \
         L {ix1} {iy1} A {ri} {ri} 0 {large} 0 {ix0} {iy0} Z",
        ox0 = num(ox0),
        oy0 = num(oy0),
        ro = num(r_out),
        ox1 = num(ox1),
        oy1 = num(oy1),
        ix1 = num(ix1),
        iy1 = num(iy1),
        ri = num(r_in),
        ix0 = num(ix0),
        iy0 = num(iy0),
    )
}

/// A full annulus (evenodd fill) — the single-slice (100 %) donut case, where a
/// 360° arc would degenerate to a point.
fn full_ring_path(cx: f64, cy: f64, r_out: f64, r_in: f64) -> String {
    format!(
        "M {l} {cy} A {ro} {ro} 0 1 1 {r} {cy} A {ro} {ro} 0 1 1 {l} {cy} Z \
         M {il} {cy} A {ri} {ri} 0 1 0 {ir} {cy} A {ri} {ri} 0 1 0 {il} {cy} Z",
        l = num(cx - r_out),
        r = num(cx + r_out),
        il = num(cx - r_in),
        ir = num(cx + r_in),
        cy = num(cy),
        ro = num(r_out),
        ri = num(r_in),
    )
}

/// An open (stroked) arc `start`→`end` — gauge track / value sweep.
fn open_arc(cx: f64, cy: f64, r: f64, start: f64, end: f64) -> String {
    let (x0, y0) = polar(cx, cy, r, start);
    let (x1, y1) = polar(cx, cy, r, end);
    let large = i32::from((end - start).abs() > 180.0);
    let sweep = i32::from(end >= start);
    format!(
        "M {x0} {y0} A {r} {r} 0 {large} {sweep} {x1} {y1}",
        x0 = num(x0),
        y0 = num(y0),
        r = num(r),
        x1 = num(x1),
        y1 = num(y1),
    )
}

/// Round a positive value up to a "nice" axis maximum (1/2/2.5/5/10 × 10ⁿ).
fn nice_ceil(v: f64) -> f64 {
    if !v.is_finite() || v <= 0.0 {
        return 1.0;
    }
    let base = 10f64.powf(v.log10().floor());
    let frac = v / base;
    let nice = if frac <= 1.0 {
        1.0
    } else if frac <= 2.0 {
        2.0
    } else if frac <= 2.5 {
        2.5
    } else if frac <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * base
}

/// The palette colour for series/slice `i`, or its explicit `override_` colour.
fn color_at(opts: &ChartOpts, i: usize, override_: Option<&str>) -> String {
    if let Some(c) = override_ {
        if !c.is_empty() {
            return c.to_string();
        }
    }
    if opts.palette.is_empty() {
        return "var(--accent)".to_string();
    }
    opts.palette[i % opts.palette.len()].clone()
}

// ---------------------------------------------------------------------------
// Frame helpers
// ---------------------------------------------------------------------------

/// Wrap a chart's SVG (and optional legend) in the shared `<figure>` frame with
/// its optional caption.
fn frame(opts: &ChartOpts, svg: AnyView, legend: Option<AnyView>) -> AnyView {
    let title = opts
        .title
        .clone()
        .map(|t| view! { <figcaption class="chart-title">{t}</figcaption> }.into_any());
    view! {
        <figure class="chart">
            {title}
            {svg}
            {legend}
        </figure>
    }
    .into_any()
}

/// The root `<svg>` with a scaling viewBox and an accessible label.
fn svg_root(opts: &ChartOpts, children: Vec<AnyView>) -> AnyView {
    let vb = format!("0 0 {} {}", num(opts.width), num(opts.height));
    let label = opts.title.clone().unwrap_or_else(|| "chart".to_string());
    view! {
        <svg
            class="chart-svg"
            viewBox=vb
            preserveAspectRatio="xMidYMid meet"
            role="img"
            aria-label=label
        >
            {children}
        </svg>
    }
    .into_any()
}

/// The `(label, colour)` legend strip shared by pie/donut.
fn legend_strip(items: &[(String, String)]) -> AnyView {
    let rows = items
        .iter()
        .cloned()
        .map(|(label, color)| {
            view! {
                <span class="chart-legend-item">
                    <span class="chart-legend-swatch" style=format!("background:{color}")></span>
                    <span class="chart-legend-label">{label}</span>
                </span>
            }
            .into_any()
        })
        .collect::<Vec<_>>();
    view! { <div class="chart-legend">{rows}</div> }.into_any()
}

/// The neutral placeholder shown for empty/degenerate input.
fn no_data(opts: &ChartOpts) -> AnyView {
    let title = opts
        .title
        .clone()
        .map(|t| view! { <figcaption class="chart-title">{t}</figcaption> }.into_any());
    view! {
        <figure class="chart">
            {title}
            <div class="chart-empty">"No data"</div>
        </figure>
    }
    .into_any()
}

/// Truncate a label so a long category can't overflow the plot.
fn clip_label(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let mut out: String = chars[..max.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    }
}

// ---------------------------------------------------------------------------
// Hover tooltips
// ---------------------------------------------------------------------------
//
// Each mark (bar, slice, line/radar vertex, heatmap cell) carries mouse
// handlers that write a `Tip` into a per-chart `hover` signal; `tip_layer`
// reads that signal and draws a floating label on top. The signal is *local
// interaction state* — created only once a chart has data to draw (so the pure
// `no_data` paths, and the degenerate-input unit tests, never touch a reactive
// runtime), reset for free whenever the caller re-renders on a data change.

/// What the tooltip shows and where it points, in SVG user space.
#[derive(Clone, PartialEq)]
struct Tip {
    /// Anchor x the tooltip centres over.
    x: f64,
    /// Anchor y the tooltip sits above (flipping below near the top edge).
    y: f64,
    /// The one-line caption (already `label: value`-formatted).
    text: String,
}

/// A number for a tooltip caption: integers bare, else ≤2 dp with trailing
/// zeros trimmed; non-finite shows an em dash rather than `NaN`/`inf`.
fn fmt_val(v: f64) -> String {
    if !v.is_finite() {
        return "—".to_string();
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{v:.0}");
    }
    let s = format!("{v:.2}");
    let t = s.trim_end_matches('0').trim_end_matches('.');
    if t.is_empty() || t == "-0" {
        "0".to_string()
    } else {
        t.to_string()
    }
}

/// The `label: value` caption for a mark, falling back to a 1-based index when
/// the datum is unlabelled.
fn tip_text(label: &str, index: usize, value: f64) -> String {
    let head = if label.is_empty() {
        format!("#{}", index + 1)
    } else {
        clip_label(label, 28)
    };
    format!("{head}: {}", fmt_val(value))
}

/// The floating tooltip `<g>` for the current hover, or nothing. Sized to its
/// caption, clamped inside the `w`×`h` viewBox, and pointer-transparent (via
/// `.chart-tooltip`) so it never steals the hover that drives it.
fn tip_layer(hover: RwSignal<Option<Tip>>, w: f64, h: f64) -> AnyView {
    view! {
        {move || {
            hover.get().map(|t| {
                let fs = 11.0_f64;
                let (px, py) = (6.0_f64, 4.0_f64);
                // Monospace-ish width estimate — wide enough to avoid clipping.
                let text_w = t.text.chars().count() as f64 * fs * 0.6;
                let box_w = text_w + px * 2.0;
                let box_h = fs + py * 2.0;
                let bx = (t.x - box_w / 2.0).clamp(2.0, (w - box_w - 2.0).max(2.0));
                // Prefer above the anchor; flip below when there's no room.
                let by = if t.y - box_h - 9.0 >= 2.0 {
                    t.y - box_h - 9.0
                } else {
                    (t.y + 9.0).min((h - box_h - 2.0).max(2.0))
                };
                view! {
                    <g class="chart-tooltip">
                        <rect
                            class="chart-tooltip-bg"
                            x=num(bx)
                            y=num(by)
                            width=num(box_w)
                            height=num(box_h)
                            rx="3"
                        ></rect>
                        <text
                            class="chart-tooltip-text"
                            x=num(bx + box_w / 2.0)
                            y=num(by + box_h / 2.0)
                            text-anchor="middle"
                            dominant-baseline="middle"
                        >
                            {t.text}
                        </text>
                    </g>
                }
                .into_any()
            })
        }}
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// Pie / Donut
// ---------------------------------------------------------------------------

/// A pie chart over positive-valued slices.
#[must_use]
pub fn pie_chart(data: Vec<Datum>, opts: &ChartOpts) -> AnyView {
    pie_like(data, 0.0, opts)
}

/// A donut chart (a pie with a centre hole).
#[must_use]
pub fn donut_chart(data: Vec<Datum>, opts: &ChartOpts) -> AnyView {
    pie_like(data, 0.58, opts)
}

fn pie_like(data: Vec<Datum>, inner_ratio: f64, opts: &ChartOpts) -> AnyView {
    let data: Vec<Datum> = data
        .into_iter()
        .filter(|d| d.value.is_finite() && d.value > 0.0)
        .take(MAX_SLICES)
        .collect();
    let total: f64 = data.iter().map(|d| d.value).sum();
    if data.is_empty() || total <= 0.0 {
        return no_data(opts);
    }

    // A square drawing area centred in the viewBox.
    let side = opts.width.min(opts.height);
    let cx = opts.width / 2.0;
    let cy = opts.height / 2.0;
    let r = side / 2.0 - 4.0;
    let r_in = r * inner_ratio;

    let hover = RwSignal::new(None::<Tip>);
    let mut angle = 0.0_f64;
    let mut slices = Vec::with_capacity(data.len() + 1);
    let mut legend = Vec::with_capacity(data.len());
    let single = data.len() == 1;
    for (i, d) in data.iter().enumerate() {
        let frac = d.value / total;
        let end = angle + frac * 360.0;
        let color = color_at(opts, i, d.color.as_deref());
        // Anchor the tooltip at the slice's angular midpoint, mid-radius.
        let mid = (angle + end) / 2.0;
        let anchor_r = if inner_ratio > 0.0 {
            (r + r_in) / 2.0
        } else {
            r * 0.62
        };
        let (ax, ay) = polar(cx, cy, anchor_r, mid);
        let pct = (frac * 100.0).round();
        let tip = format!("{} ({pct:.0}%)", tip_text(&d.label, i, d.value));
        let d_attr = if single {
            if inner_ratio > 0.0 {
                full_ring_path(cx, cy, r, r_in)
            } else {
                // Full circle as two half-arcs (a 360° wedge degenerates).
                format!(
                    "M {cx} {top} A {r} {r} 0 1 1 {cx} {bot} A {r} {r} 0 1 1 {cx} {top} Z",
                    cx = num(cx),
                    top = num(cy - r),
                    bot = num(cy + r),
                    r = num(r),
                )
            }
        } else if inner_ratio > 0.0 {
            ring_path(cx, cy, r, r_in, angle, end)
        } else {
            wedge_path(cx, cy, r, angle, end)
        };
        slices.push(
            view! {
                <path
                    class="chart-slice"
                    d=d_attr
                    fill=color
                    fill-rule="evenodd"
                    on:mouseenter=move |_| hover.set(Some(Tip { x: ax, y: ay, text: tip.clone() }))
                    on:mouseleave=move |_| hover.set(None)
                ></path>
            }
            .into_any(),
        );
        let label = if d.label.is_empty() {
            format!("{} ({pct:.0}%)", i + 1)
        } else {
            format!("{} ({pct:.0}%)", clip_label(&d.label, 18))
        };
        legend.push((label, color_at(opts, i, d.color.as_deref())));
        angle = end;
    }

    slices.push(tip_layer(hover, opts.width, opts.height));
    let svg = svg_root(opts, slices);
    let legend = opts.legend.then(|| legend_strip(&legend));
    frame(opts, svg, legend)
}

// ---------------------------------------------------------------------------
// Bar (vertical + horizontal)
// ---------------------------------------------------------------------------

/// A bar chart. `horizontal` lays the bars left-to-right (labels on the left)
/// rather than bottom-up (labels underneath).
#[must_use]
pub fn bar_chart(data: Vec<Datum>, horizontal: bool, opts: &ChartOpts) -> AnyView {
    let data: Vec<Datum> = data
        .into_iter()
        .filter(|d| d.value.is_finite())
        .take(MAX_BARS)
        .collect();
    if data.is_empty() {
        return no_data(opts);
    }
    if horizontal {
        bars_horizontal(&data, opts)
    } else {
        bars_vertical(&data, opts)
    }
}

fn bars_vertical(data: &[Datum], opts: &ChartOpts) -> AnyView {
    let w = opts.width;
    let h = opts.height;
    let has_labels = data.iter().any(|d| !d.label.is_empty());
    let (pad_l, pad_r, pad_t) = (6.0, 6.0, 10.0);
    let pad_b = if has_labels { 20.0 } else { 8.0 };
    let plot_w = (w - pad_l - pad_r).max(1.0);
    let plot_h = (h - pad_t - pad_b).max(1.0);

    let data_max = data.iter().map(|d| d.value).fold(0.0_f64, f64::max);
    let data_min = data
        .iter()
        .map(|d| d.value)
        .fold(0.0_f64, f64::min)
        .min(0.0);
    let hi = opts.max.unwrap_or_else(|| nice_ceil(data_max.max(0.0)));
    let lo = if data_min < 0.0 {
        -nice_ceil(-data_min)
    } else {
        0.0
    };
    let range = (hi - lo).max(1e-9);
    let y_of = |v: f64| pad_t + (hi - v) / range * plot_h;
    let base_y = y_of(0.0);

    let n = data.len();
    let slot = plot_w / n as f64;
    let bw = (slot * 0.68).max(1.0);

    let hover = RwSignal::new(None::<Tip>);
    let mut els = Vec::with_capacity(n * 2 + 2);
    // Zero baseline.
    els.push(
        view! {
            <line
                class="chart-axis"
                x1=num(pad_l)
                y1=num(base_y)
                x2=num(w - pad_r)
                y2=num(base_y)
            ></line>
        }
        .into_any(),
    );
    for (i, d) in data.iter().enumerate() {
        let x = pad_l + slot * i as f64 + (slot - bw) / 2.0;
        let yv = y_of(d.value);
        let (ry, rh) = if d.value >= 0.0 {
            (yv, base_y - yv)
        } else {
            (base_y, yv - base_y)
        };
        let color = color_at(opts, i, d.color.as_deref());
        let (ax, ay) = (x + bw / 2.0, ry.min(base_y));
        let tip = tip_text(&d.label, i, d.value);
        els.push(
            view! {
                <rect
                    class="chart-bar"
                    x=num(x)
                    y=num(ry)
                    width=num(bw)
                    height=num(rh.max(0.0))
                    rx="1.5"
                    fill=color
                    on:mouseenter=move |_| hover.set(Some(Tip { x: ax, y: ay, text: tip.clone() }))
                    on:mouseleave=move |_| hover.set(None)
                ></rect>
            }
            .into_any(),
        );
        if has_labels {
            let label = if d.label.is_empty() {
                (i + 1).to_string()
            } else {
                clip_label(&d.label, 8)
            };
            els.push(
                view! {
                    <text
                        class="chart-axis-label"
                        x=num(x + bw / 2.0)
                        y=num(h - 6.0)
                        text-anchor="middle"
                    >
                        {label}
                    </text>
                }
                .into_any(),
            );
        }
    }
    els.push(tip_layer(hover, opts.width, opts.height));
    frame(opts, svg_root(opts, els), None)
}

fn bars_horizontal(data: &[Datum], opts: &ChartOpts) -> AnyView {
    let w = opts.width;
    let h = opts.height;
    let has_labels = data.iter().any(|d| !d.label.is_empty());
    let label_w = if has_labels {
        (w * 0.28).clamp(48.0, 140.0)
    } else {
        6.0
    };
    let (pad_r, pad_t, pad_b) = (10.0, 6.0, 6.0);
    let plot_w = (w - label_w - pad_r).max(1.0);
    let plot_h = (h - pad_t - pad_b).max(1.0);

    // Horizontal bars anchor at zero and grow right (non-negative domain).
    let data_max = data.iter().map(|d| d.value).fold(0.0_f64, f64::max);
    let hi = opts
        .max
        .unwrap_or_else(|| nice_ceil(data_max.max(0.0)))
        .max(1e-9);
    let base_x = label_w;

    let n = data.len();
    let slot = plot_h / n as f64;
    let bh = (slot * 0.68).max(1.0);

    let hover = RwSignal::new(None::<Tip>);
    let mut els = Vec::with_capacity(n * 2 + 2);
    els.push(
        view! {
            <line class="chart-axis" x1=num(base_x) y1=num(pad_t) x2=num(base_x) y2=num(h - pad_b)></line>
        }
        .into_any(),
    );
    for (i, d) in data.iter().enumerate() {
        let y = pad_t + slot * i as f64 + (slot - bh) / 2.0;
        let bar_w = (d.value.max(0.0) / hi * plot_w).max(0.0);
        let color = color_at(opts, i, d.color.as_deref());
        let (ax, ay) = (base_x + bar_w, y + bh / 2.0);
        let tip = tip_text(&d.label, i, d.value);
        els.push(
            view! {
                <rect
                    class="chart-bar"
                    x=num(base_x)
                    y=num(y)
                    width=num(bar_w)
                    height=num(bh)
                    rx="1.5"
                    fill=color
                    on:mouseenter=move |_| hover.set(Some(Tip { x: ax, y: ay, text: tip.clone() }))
                    on:mouseleave=move |_| hover.set(None)
                ></rect>
            }
            .into_any(),
        );
        if has_labels {
            let label = if d.label.is_empty() {
                (i + 1).to_string()
            } else {
                clip_label(&d.label, 14)
            };
            els.push(
                view! {
                    <text
                        class="chart-axis-label"
                        x=num(label_w - 6.0)
                        y=num(y + bh / 2.0)
                        text-anchor="end"
                        dominant-baseline="middle"
                    >
                        {label}
                    </text>
                }
                .into_any(),
            );
        }
    }
    els.push(tip_layer(hover, opts.width, opts.height));
    frame(opts, svg_root(opts, els), None)
}

// ---------------------------------------------------------------------------
// Line / Area
// ---------------------------------------------------------------------------

/// A single-series line chart.
#[must_use]
pub fn line_chart(data: Vec<Datum>, opts: &ChartOpts) -> AnyView {
    line_like(data, false, opts)
}

/// A single-series area chart (a line filled to the zero baseline).
#[must_use]
pub fn area_chart(data: Vec<Datum>, opts: &ChartOpts) -> AnyView {
    line_like(data, true, opts)
}

fn line_like(data: Vec<Datum>, filled: bool, opts: &ChartOpts) -> AnyView {
    let data: Vec<Datum> = data
        .into_iter()
        .filter(|d| d.value.is_finite())
        .take(MAX_POINTS)
        .collect();
    if data.is_empty() {
        return no_data(opts);
    }
    let w = opts.width;
    let h = opts.height;
    let has_labels = data.iter().any(|d| !d.label.is_empty());
    let (pad_l, pad_r, pad_t) = (6.0, 6.0, 10.0);
    let pad_b = if has_labels { 18.0 } else { 8.0 };
    let plot_w = (w - pad_l - pad_r).max(1.0);
    let plot_h = (h - pad_t - pad_b).max(1.0);

    let data_max = data
        .iter()
        .map(|d| d.value)
        .fold(f64::NEG_INFINITY, f64::max);
    let data_min = data.iter().map(|d| d.value).fold(f64::INFINITY, f64::min);
    // Area anchors at zero; a plain line frames its own min..max.
    let (mut lo, mut hi) = if filled {
        (data_min.min(0.0), nice_ceil(data_max.max(0.0)))
    } else {
        (data_min, data_max)
    };
    if let Some(m) = opts.max {
        hi = m;
    }
    if (hi - lo).abs() < 1e-9 {
        lo -= 1.0;
        hi += 1.0;
    }
    let range = (hi - lo).max(1e-9);
    let n = data.len();
    let x_of = |i: usize| {
        if n == 1 {
            pad_l + plot_w / 2.0
        } else {
            pad_l + plot_w * i as f64 / (n - 1) as f64
        }
    };
    let y_of = |v: f64| pad_t + (hi - v) / range * plot_h;

    let pts: Vec<(f64, f64)> = data
        .iter()
        .enumerate()
        .map(|(i, d)| (x_of(i), y_of(d.value)))
        .collect();
    let color = color_at(opts, 0, None);

    let hover = RwSignal::new(None::<Tip>);
    let mut els: Vec<AnyView> = Vec::new();
    if filled {
        let base_y = y_of(lo.max(0.0).min(hi));
        let mut d_attr = String::from("M ");
        for (k, (x, y)) in pts.iter().enumerate() {
            if k > 0 {
                d_attr.push_str("L ");
            }
            d_attr.push_str(&format!("{} {} ", num(*x), num(*y)));
        }
        if let (Some((lx, _)), Some((fx, _))) = (pts.last(), pts.first()) {
            d_attr.push_str(&format!(
                "L {} {} L {} {} Z",
                num(*lx),
                num(base_y),
                num(*fx),
                num(base_y)
            ));
        }
        els.push(view! { <path class="chart-area" d=d_attr fill=color.clone()></path> }.into_any());
    }
    // The line itself.
    let poly = pts
        .iter()
        .map(|(x, y)| format!("{},{}", num(*x), num(*y)))
        .collect::<Vec<_>>()
        .join(" ");
    els.push(
        view! {
            <polyline class="chart-line" points=poly fill="none" stroke=color.clone()></polyline>
        }
        .into_any(),
    );
    // Point markers when sparse enough to read.
    if n <= 32 {
        for (x, y) in &pts {
            els.push(
                view! { <circle class="chart-dot" cx=num(*x) cy=num(*y) r="2.4" fill=color.clone()></circle> }
                    .into_any(),
            );
        }
    }
    // X labels: all when few, else just the ends.
    if has_labels {
        let show_all = n <= 12;
        for (i, d) in data.iter().enumerate() {
            if d.label.is_empty() {
                continue;
            }
            let ends = i == 0 || i == n - 1;
            if !show_all && !ends {
                continue;
            }
            let anchor = if i == 0 {
                "start"
            } else if i == n - 1 {
                "end"
            } else {
                "middle"
            };
            els.push(
                view! {
                    <text
                        class="chart-axis-label"
                        x=num(x_of(i))
                        y=num(h - 5.0)
                        text-anchor=anchor
                    >
                        {clip_label(&d.label, 8)}
                    </text>
                }
                .into_any(),
            );
        }
    }
    // Invisible per-point hit targets — a comfortable radius so hovering
    // anywhere near a point reveals its tooltip, even on a dense line where the
    // visible dots were suppressed.
    for (i, (x, y)) in pts.iter().enumerate() {
        let (px, py) = (*x, *y);
        let tip = tip_text(&data[i].label, i, data[i].value);
        els.push(
            view! {
                <circle
                    class="chart-hit"
                    cx=num(px)
                    cy=num(py)
                    r="7"
                    on:mouseenter=move |_| hover.set(Some(Tip { x: px, y: py, text: tip.clone() }))
                    on:mouseleave=move |_| hover.set(None)
                ></circle>
            }
            .into_any(),
        );
    }
    els.push(tip_layer(hover, opts.width, opts.height));
    frame(opts, svg_root(opts, els), None)
}

// ---------------------------------------------------------------------------
// Sparkline
// ---------------------------------------------------------------------------

/// A compact, axis-less trend line — for inline/at-a-glance series.
#[must_use]
pub fn sparkline(values: Vec<f64>, opts: &ChartOpts) -> AnyView {
    let values: Vec<f64> = values
        .into_iter()
        .filter(|v| v.is_finite())
        .take(MAX_POINTS)
        .collect();
    if values.len() < 2 {
        return no_data(opts);
    }
    let w = opts.width;
    let h = opts.height;
    let pad = 2.5;
    let plot_w = (w - 2.0 * pad).max(1.0);
    let plot_h = (h - 2.0 * pad).max(1.0);
    let mut lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let mut hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if (hi - lo).abs() < 1e-9 {
        lo -= 1.0;
        hi += 1.0;
    }
    let range = hi - lo;
    let n = values.len();
    let pt = |i: usize, v: f64| {
        let x = pad + plot_w * i as f64 / (n - 1) as f64;
        let y = pad + (hi - v) / range * plot_h;
        (x, y)
    };
    let poly = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let (x, y) = pt(i, *v);
            format!("{},{}", num(x), num(y))
        })
        .collect::<Vec<_>>()
        .join(" ");
    let color = color_at(opts, 0, None);
    let (lx, lv) = (n - 1, values[n - 1]);
    let (dx, dy) = pt(lx, lv);
    let svg = svg_root(
        opts,
        vec![
            view! {
                <polyline class="chart-spark" points=poly fill="none" stroke=color.clone()></polyline>
            }
            .into_any(),
            view! { <circle class="chart-dot" cx=num(dx) cy=num(dy) r="2" fill=color></circle> }.into_any(),
        ],
    );
    frame(opts, svg, None)
}

// ---------------------------------------------------------------------------
// Gauge
// ---------------------------------------------------------------------------

/// A 270° radial gauge showing `value` within `[min, max]`.
#[must_use]
pub fn gauge(value: f64, min: f64, max: f64, opts: &ChartOpts) -> AnyView {
    let (min, max) = if max > min {
        (min, max)
    } else {
        (min, min + 1.0)
    };
    let value = if value.is_finite() { value } else { min };
    let frac = ((value - min) / (max - min)).clamp(0.0, 1.0);

    let w = opts.width;
    let h = opts.height;
    let cx = w / 2.0;
    let cy = h * 0.56;
    let r = (w / 2.0).min(h * 0.62) - 8.0;
    let sw = (r * 0.2).max(4.0);
    let start = 225.0;
    let span = 270.0;

    let track = open_arc(cx, cy, r, start, start + span);
    let val = open_arc(cx, cy, r, start, start + span * frac);
    let color = color_at(opts, 0, None);

    let mut els: Vec<AnyView> = vec![
        view! {
            <path
                class="chart-gauge-track"
                d=track
                fill="none"
                stroke-width=num(sw)
                stroke-linecap="round"
            ></path>
        }
        .into_any(),
        view! {
            <path
                class="chart-gauge-value"
                d=val
                fill="none"
                stroke=color
                stroke-width=num(sw)
                stroke-linecap="round"
            ></path>
        }
        .into_any(),
        view! {
            <text class="chart-gauge-text" x=num(cx) y=num(cy) text-anchor="middle" dominant-baseline="middle">
                {num(value)}
            </text>
        }
        .into_any(),
    ];
    if let Some(t) = &opts.title {
        els.push(
            view! {
                <text class="chart-gauge-label" x=num(cx) y=num(cy + r * 0.42) text-anchor="middle">
                    {clip_label(t, 22)}
                </text>
            }
            .into_any(),
        );
    }
    // Gauge draws its own centred label, so suppress the frame caption.
    let bare = ChartOpts {
        title: None,
        ..opts.clone()
    };
    frame(&bare, svg_root(opts, els), None)
}

// ---------------------------------------------------------------------------
// Radar
// ---------------------------------------------------------------------------

/// A radar/spider chart: one value per `axes` spoke (a single series).
#[must_use]
pub fn radar_chart(axes: Vec<String>, values: Vec<f64>, opts: &ChartOpts) -> AnyView {
    let axes: Vec<String> = axes.into_iter().take(MAX_AXES).collect();
    let n = axes.len();
    if n < 3 {
        return no_data(opts);
    }
    let vmax = opts
        .max
        .unwrap_or_else(|| {
            nice_ceil(
                values
                    .iter()
                    .copied()
                    .filter(|v| v.is_finite())
                    .fold(0.0_f64, f64::max),
            )
        })
        .max(1e-9);

    let w = opts.width;
    let h = opts.height;
    let cx = w / 2.0;
    let cy = h / 2.0;
    let r = (w.min(h) / 2.0 - 22.0).max(1.0);
    let deg = |i: usize| i as f64 * 360.0 / n as f64;

    let mut els: Vec<AnyView> = Vec::new();
    // Concentric grid rings.
    for level in [0.25, 0.5, 0.75, 1.0] {
        let ring = (0..n)
            .map(|i| {
                let (x, y) = polar(cx, cy, r * level, deg(i));
                format!("{},{}", num(x), num(y))
            })
            .collect::<Vec<_>>()
            .join(" ");
        els.push(
            view! { <polygon class="chart-grid" points=ring fill="none"></polygon> }.into_any(),
        );
    }
    // Spokes + axis labels.
    for (i, axis) in axes.iter().enumerate().take(n) {
        let (sx, sy) = polar(cx, cy, r, deg(i));
        els.push(
            view! { <line class="chart-grid" x1=num(cx) y1=num(cy) x2=num(sx) y2=num(sy)></line> }
                .into_any(),
        );
        let (lx, ly) = polar(cx, cy, r + 12.0, deg(i));
        let anchor = if lx > cx + 1.0 {
            "start"
        } else if lx < cx - 1.0 {
            "end"
        } else {
            "middle"
        };
        els.push(
            view! {
                <text class="chart-axis-label" x=num(lx) y=num(ly) text-anchor=anchor dominant-baseline="middle">
                    {clip_label(axis, 10)}
                </text>
            }
            .into_any(),
        );
    }
    // The data polygon: one vertex per axis (kept so the same points feed both
    // the outline and the hover hit targets).
    let verts: Vec<(f64, f64, f64)> = (0..n)
        .map(|i| {
            let v = values
                .get(i)
                .copied()
                .filter(|v| v.is_finite())
                .unwrap_or(0.0);
            let rr = r * (v / vmax).clamp(0.0, 1.0);
            let (x, y) = polar(cx, cy, rr, deg(i));
            (x, y, v)
        })
        .collect();
    let poly = verts
        .iter()
        .map(|(x, y, _)| format!("{},{}", num(*x), num(*y)))
        .collect::<Vec<_>>()
        .join(" ");
    let color = color_at(opts, 0, None);
    els.push(
        view! { <polygon class="chart-radar-area" points=poly.clone() fill=color.clone()></polygon> }.into_any(),
    );
    els.push(
        view! { <polygon class="chart-radar-line" points=poly fill="none" stroke=color></polygon> }
            .into_any(),
    );
    let hover = RwSignal::new(None::<Tip>);
    for (i, (x, y, v)) in verts.iter().enumerate() {
        let (px, py, val) = (*x, *y, *v);
        let tip = tip_text(&axes[i], i, val);
        els.push(
            view! {
                <circle
                    class="chart-hit"
                    cx=num(px)
                    cy=num(py)
                    r="7"
                    on:mouseenter=move |_| hover.set(Some(Tip { x: px, y: py, text: tip.clone() }))
                    on:mouseleave=move |_| hover.set(None)
                ></circle>
            }
            .into_any(),
        );
    }
    els.push(tip_layer(hover, opts.width, opts.height));
    frame(opts, svg_root(opts, els), None)
}

// ---------------------------------------------------------------------------
// Heatmap
// ---------------------------------------------------------------------------

/// A heatmap grid: `grid[row][col]` intensities, with optional row/col labels.
/// Cells shade one palette colour by opacity, so the map re-themes for free.
#[must_use]
pub fn heatmap(
    rows: Vec<String>,
    cols: Vec<String>,
    grid: Vec<Vec<f64>>,
    opts: &ChartOpts,
) -> AnyView {
    let nr = grid.len();
    let nc = grid.iter().map(Vec::len).max().unwrap_or(0);
    if nr == 0 || nc == 0 || nr * nc > MAX_CELLS {
        return no_data(opts);
    }
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for row in &grid {
        for v in row {
            if v.is_finite() {
                lo = lo.min(*v);
                hi = hi.max(*v);
            }
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        return no_data(opts);
    }
    let range = if (hi - lo).abs() < 1e-9 { 1.0 } else { hi - lo };

    let w = opts.width;
    let h = opts.height;
    let has_rl = rows.iter().any(|s| !s.is_empty());
    let has_cl = cols.iter().any(|s| !s.is_empty());
    let row_lw = if has_rl {
        (w * 0.2).clamp(30.0, 90.0)
    } else {
        2.0
    };
    let col_lh = if has_cl { 16.0 } else { 2.0 };
    let plot_w = (w - row_lw - 2.0).max(1.0);
    let plot_h = (h - col_lh - 2.0).max(1.0);
    let cw = plot_w / nc as f64;
    let ch = plot_h / nr as f64;
    let base = color_at(opts, 0, None);
    let show_vals = nr * nc <= 64 && cw > 22.0 && ch > 14.0;

    let hover = RwSignal::new(None::<Tip>);
    let mut els: Vec<AnyView> = Vec::new();
    for (r, row) in grid.iter().enumerate() {
        for c in 0..nc {
            let v = row.get(c).copied().unwrap_or(lo);
            let norm = if v.is_finite() {
                ((v - lo) / range).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let opacity = 0.12 + 0.88 * norm;
            let x = row_lw + c as f64 * cw;
            let y = col_lh + r as f64 * ch;
            // Caption from whichever labels exist: "row × col: value".
            let rl = rows.get(r).map(String::as_str).filter(|s| !s.is_empty());
            let cl = cols.get(c).map(String::as_str).filter(|s| !s.is_empty());
            let head = match (rl, cl) {
                (Some(a), Some(b)) => format!("{} × {}", clip_label(a, 16), clip_label(b, 16)),
                (Some(a), None) => clip_label(a, 24),
                (None, Some(b)) => clip_label(b, 24),
                (None, None) => format!("r{}, c{}", r + 1, c + 1),
            };
            let tip = if v.is_finite() {
                format!("{head}: {}", fmt_val(v))
            } else {
                head
            };
            let (ax, ay) = (x + cw / 2.0, y + ch / 2.0);
            els.push(
                view! {
                    <rect
                        class="chart-cell"
                        x=num(x)
                        y=num(y)
                        width=num((cw - 1.0).max(0.5))
                        height=num((ch - 1.0).max(0.5))
                        fill=base.clone()
                        fill-opacity=num(opacity)
                        on:mouseenter=move |_| hover.set(Some(Tip { x: ax, y: ay, text: tip.clone() }))
                        on:mouseleave=move |_| hover.set(None)
                    ></rect>
                }
                .into_any(),
            );
            if show_vals && v.is_finite() {
                els.push(
                    view! {
                        <text
                            class="chart-cell-label"
                            x=num(x + cw / 2.0)
                            y=num(y + ch / 2.0)
                            text-anchor="middle"
                            dominant-baseline="middle"
                        >
                            {num(v)}
                        </text>
                    }
                    .into_any(),
                );
            }
        }
        if has_rl {
            if let Some(label) = rows.get(r).filter(|s| !s.is_empty()) {
                els.push(
                    view! {
                        <text
                            class="chart-axis-label"
                            x=num(row_lw - 5.0)
                            y=num(col_lh + r as f64 * ch + ch / 2.0)
                            text-anchor="end"
                            dominant-baseline="middle"
                        >
                            {clip_label(label, 12)}
                        </text>
                    }
                    .into_any(),
                );
            }
        }
    }
    if has_cl {
        for (c, label) in cols.iter().enumerate().take(nc) {
            if label.is_empty() {
                continue;
            }
            els.push(
                view! {
                    <text
                        class="chart-axis-label"
                        x=num(row_lw + c as f64 * cw + cw / 2.0)
                        y=num(col_lh - 5.0)
                        text-anchor="middle"
                    >
                        {clip_label(label, 8)}
                    </text>
                }
                .into_any(),
            );
        }
    }
    els.push(tip_layer(hover, opts.width, opts.height));
    frame(opts, svg_root(opts, els), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    #[test]
    fn num_trims_and_guards() {
        assert_eq!(num(1.0), "1");
        assert_eq!(num(1.50), "1.5");
        assert_eq!(num(1.234), "1.23");
        assert_eq!(num(0.0), "0");
        assert_eq!(num(-0.0), "0");
        assert_eq!(num(f64::NAN), "0");
        assert_eq!(num(f64::INFINITY), "0");
    }

    #[test]
    fn polar_cardinal_points() {
        // 0° = top, 90° = right, 180° = bottom, 270° = left.
        let (x, y) = polar(10.0, 10.0, 5.0, 0.0);
        approx(x, 10.0);
        approx(y, 5.0);
        let (x, y) = polar(10.0, 10.0, 5.0, 90.0);
        approx(x, 15.0);
        approx(y, 10.0);
        let (x, y) = polar(10.0, 10.0, 5.0, 180.0);
        approx(x, 10.0);
        approx(y, 15.0);
        let (x, y) = polar(10.0, 10.0, 5.0, 270.0);
        approx(x, 5.0);
        approx(y, 10.0);
    }

    #[test]
    fn nice_ceil_rounds_up() {
        approx(nice_ceil(0.0), 1.0);
        approx(nice_ceil(0.7), 1.0);
        approx(nice_ceil(1.0), 1.0);
        approx(nice_ceil(1.5), 2.0);
        approx(nice_ceil(3.0), 5.0);
        approx(nice_ceil(7.0), 10.0);
        approx(nice_ceil(42.0), 50.0);
        approx(nice_ceil(230.0), 250.0);
    }

    #[test]
    fn wedge_and_arc_paths_are_finite() {
        // A half-circle wedge sets the large-arc flag off (exactly 180°) and
        // never emits a non-finite token.
        let p = wedge_path(50.0, 50.0, 40.0, 0.0, 180.0);
        assert!(p.starts_with("M 50 50"));
        assert!(!p.contains("NaN") && !p.contains("inf"));
        let a = open_arc(50.0, 50.0, 40.0, 225.0, 495.0);
        assert!(a.contains(" 1 1 ")); // large=1, sweep=1 for the 270° track
    }

    #[test]
    fn color_falls_back_and_cycles() {
        let opts = ChartOpts::default();
        assert_eq!(color_at(&opts, 0, None), "var(--chart-1)");
        assert_eq!(color_at(&opts, 8, None), "var(--chart-1)"); // wraps (8 slots)
        assert_eq!(color_at(&opts, 0, Some("#abc")), "#abc"); // explicit wins
        let bare = ChartOpts {
            palette: vec![],
            ..ChartOpts::default()
        };
        assert_eq!(color_at(&bare, 0, None), "var(--accent)"); // empty palette guard
    }

    #[test]
    fn clip_label_ellipsizes() {
        assert_eq!(clip_label("short", 8), "short");
        assert_eq!(clip_label("a very long label", 8), "a very …");
    }

    #[test]
    fn fmt_val_is_clean_and_finite_safe() {
        assert_eq!(fmt_val(42.0), "42"); // integer: no decimal point
        assert_eq!(fmt_val(3.5), "3.5");
        assert_eq!(fmt_val(1.23456), "1.23"); // rounds to ≤2 dp
        assert_eq!(fmt_val(2.50), "2.5"); // trailing zero trimmed
        assert_eq!(fmt_val(f64::NAN), "—"); // never "NaN"
        assert_eq!(fmt_val(f64::INFINITY), "—");
    }

    #[test]
    fn tip_text_labels_or_falls_back_to_index() {
        assert_eq!(tip_text("Jan", 0, 12.0), "Jan: 12");
        assert_eq!(tip_text("", 2, 4.5), "#3: 4.5"); // unlabelled → 1-based index
    }

    #[test]
    fn renderers_tolerate_empty_input() {
        // None of these should panic; each returns an AnyView (placeholder).
        let o = ChartOpts::default();
        let _ = pie_chart(vec![], &o);
        let _ = donut_chart(vec![Datum::new("z", 0.0)], &o); // all-zero → no data
        let _ = bar_chart(vec![], false, &o);
        let _ = line_chart(vec![], &o);
        let _ = sparkline(vec![1.0], &o); // <2 points
        let _ = radar_chart(vec!["a".into(), "b".into()], vec![1.0, 2.0], &o); // <3 axes
        let _ = heatmap(vec![], vec![], vec![], &o);
    }
}
