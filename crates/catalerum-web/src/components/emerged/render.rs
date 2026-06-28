//! The generic node interpreter: `render_node` turns one [`UiNode`] into an
//! `AnyView`, recursing over children. Evaluation order per node is
//! `for_each` → `show_if` → `match kind`, mirroring the server's render contract.
//!
//! Reactivity is fine-grained: every dynamic text/attribute is its own `move ||`
//! closure (an isolated reactive effect), `for_each` is a keyed `<For>`, and
//! `show_if` gates on a `Memo` so a hidden subtree mounts/unmounts only when the
//! condition actually flips — not on every unrelated state edit. Unknown kinds and
//! over-deep trees degrade to a neutral container; nothing here can panic on
//! adversarial input.

use leptos::prelude::*;
use serde_json::Value as Json;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use super::handlers;
use super::model::{EventName, FilterMode, ForEachFilter, NodeKind, PageMode, Pagination, UiNode};
use super::path::{abs_data_path, get_path, interpolate, resolve_value, stringify, truthy, Scope};
use super::state::{now_ms, UiState, MAX_APP_DEPTH};
use super::ui::EmergedUi;
use crate::components::charts::{self, ChartOpts, Datum};
use crate::components::icons::{Icon, MdIcon};
use crate::components::markdown::markdown_html;
use crate::{auth, rest};

/// Hard cap on rendered `for_each` rows (the server caps spec size; this caps the
/// product of a `for_each` over a tool-returned array). Also the ceiling on how
/// far infinite scroll will grow its window.
const MAX_ROWS: usize = 1000;
/// Defensive recursion cap (the server validates depth ≤ 32).
const MAX_DEPTH: usize = 64;
/// Largest page/window size an author-set `page_size` is clamped to.
const MAX_PAGE_SIZE: usize = 200;
/// Largest number of filtered rows a paginated loop scans/navigates. Only the
/// current page/window reaches the DOM, so this just bounds the index scan.
const MAX_TOTAL_ROWS: usize = 10_000;

/// Render one node: `for_each` wraps `show_if` wraps the kind.
pub fn render_node(node: UiNode, st: UiState, scope: Scope, depth: usize) -> AnyView {
    if depth > MAX_DEPTH {
        return ().into_any();
    }
    if node.for_each.is_some() {
        return render_for_each(node, st, scope, depth);
    }
    if let Some(cond) = node.show_if.clone() {
        let mut inner = node;
        inner.show_if = None;
        let cond_scope = scope.clone();
        // A Memo isolates the dependency on `data` and only notifies when the
        // boolean flips, so the subtree mounts/unmounts on toggle — not on every
        // keystroke elsewhere.
        let visible = Memo::new(move |_| st.show_if(&cond_scope, &cond));
        return view! {
            <div class="eu-cond">
                {move || {
                    if visible.get() {
                        render_kind(inner.clone(), st, scope.clone(), depth)
                    } else {
                        ().into_any()
                    }
                }}
            </div>
        }
        .into_any();
    }
    render_kind(node, st, scope, depth)
}

/// Render a `for_each` node as a keyed list over a state array. Keyed by the
/// author-declared `key` path (resolved per row) when set, else by index. Rows
/// are capped at this subtree's share of the row budget, and that share is split
/// among the rows so a nested `for_each` cannot multiply without bound.
fn render_for_each(node: UiNode, st: UiState, scope: Scope, depth: usize) -> AnyView {
    let fe = node.for_each.clone().expect("for_each present");
    let mut tmpl = node;
    tmpl.for_each = None;
    let node_id = tmpl.id.clone();

    let source = fe.source;
    let item_name = fe.item;
    let index_name = fe.index;
    let key_path = fe.key;
    // `filter` + `filters` fold into one ANDed list (a row must pass every one).
    let filters: Vec<ForEachFilter> = fe.filter.into_iter().chain(fe.filters).collect();
    let paginate = fe.paginate;
    let cap = scope.budget().min(MAX_ROWS);
    // A paginated loop only DOM-renders its current window, so it may scan a
    // larger array than the row budget; an unpaginated loop keeps the old cap.
    let scan_cap = if paginate.is_some() {
        MAX_TOTAL_ROWS
    } else {
        cap
    };

    // Each row inherits an equal slice of the remaining budget (computed once
    // from the current array length), so nested loops stay bounded.
    let child_budget = {
        let abs = abs_data_path(&scope, &source).unwrap_or_else(|| source.clone());
        let len = st
            .data
            .with_untracked(|d| get_path(d, &abs).as_array().map_or(1, Vec::len))
            .min(cap)
            .max(1);
        (scope.budget() / len).max(1)
    };

    let each = {
        let scope = scope.clone();
        let source = source.clone();
        let item_name = item_name.clone();
        let index_name = index_name.clone();
        let key_path = key_path.clone();
        let filters = filters.clone();
        let id = node_id.clone();
        move || {
            st.data.with(|d| {
                let abs = abs_data_path(&scope, &source).unwrap_or_else(|| source.clone());
                // The declarative row filters read live state, so typing in a
                // bound search box re-runs this closure and narrows the rows.
                // Rows keep their ORIGINAL array index (bindings, `remove_at`
                // and keys all address the unfiltered array). Multiple filters
                // AND together (text search + category dropdown at once).
                let all = filtered_indices(d, &scope, &abs, &filters, scan_cap);
                // Reading the page cursor here re-windows the rows when a pager
                // click / scroll reveal moves it.
                let (start, end) = match &paginate {
                    None => (0, all.len()),
                    Some(p) => page_bounds(p.mode, all.len(), p.page_size, st.page(&id, 0)),
                };
                all[start..end]
                    .iter()
                    .map(|&i| {
                        let key = row_key(
                            &scope,
                            d,
                            &source,
                            &item_name,
                            &index_name,
                            key_path.as_deref(),
                            i,
                        );
                        (i, key)
                    })
                    .collect::<Vec<(usize, String)>>()
            })
        }
    };

    // The pager / infinite sentinel render as a SIBLING after the rows (a
    // fragment, not a wrapper element), so the rows stay direct children of the
    // parent container and grid/flex layouts are preserved.
    let controls = paginate.map(|p| {
        pagination_controls(
            st,
            scope.clone(),
            source.clone(),
            filters.clone(),
            node_id.clone(),
            p,
        )
    });

    view! {
        <For
            each=each
            key=|(_, k): &(usize, String)| k.clone()
            children=move |(i, _k): (usize, String)| {
                let abs = abs_data_path(&scope, &source).unwrap_or_else(|| source.clone());
                let mut row = scope
                    .with_item(&item_name, format!("{abs}.{i}"))
                    .with_budget(child_budget);
                if let Some(idx) = &index_name {
                    row = row.with_index(idx, i);
                }
                render_node(tmpl.clone(), st, row, depth + 1)
            }
        />
        {controls}
    }
    .into_any()
}

/// The original array indices of the rows passing every filter, in order, capped
/// at `cap`. Pure over `data` (the reactive caller wraps it in `st.data.with`);
/// `source_abs` is the already-resolved absolute path to the array.
fn filtered_indices(
    data: &Json,
    scope: &Scope,
    source_abs: &str,
    filters: &[ForEachFilter],
    cap: usize,
) -> Vec<usize> {
    let len = get_path(data, source_abs).as_array().map_or(0, Vec::len);
    if filters.is_empty() {
        return (0..len.min(cap)).collect();
    }
    let queries: Vec<Json> = filters
        .iter()
        .map(|f| resolve_value(scope, data, &f.query))
        .collect();
    (0..len)
        .filter(|&i| {
            let item = get_path(data, &format!("{source_abs}.{i}"));
            filters.iter().zip(&queries).all(|(f, q)| {
                let field = match &f.path {
                    Some(p) => get_path(item, p),
                    None => item,
                };
                filter_passes(field, q, f.mode)
            })
        })
        .take(cap)
        .collect()
}

/// The number of pages for `total` filtered rows at `page_size` (≥ 1, so an
/// empty list still counts as one page). `page_size` is clamped to the render's
/// sane range first.
fn page_count(total: usize, page_size: usize) -> usize {
    let size = page_size.clamp(1, MAX_PAGE_SIZE);
    total.div_ceil(size).max(1)
}

/// The `[start, end)` row slice to render, given the windowing `mode`, the
/// filtered `total`, the `page_size`, and the stored `cursor` (page index in
/// `Paged`, revealed-row count in `Infinite`). Always in-bounds (`end <= total`),
/// so a stale over-large cursor (the array shrank under a filter) snaps back to
/// the last valid page / the whole list.
fn page_bounds(mode: PageMode, total: usize, page_size: usize, cursor: usize) -> (usize, usize) {
    let size = page_size.clamp(1, MAX_PAGE_SIZE);
    match mode {
        PageMode::Paged => {
            let page = cursor.min(page_count(total, page_size) - 1);
            let start = page * size;
            (start, (start + size).min(total))
        }
        // Reveal at least one page, growing with the cursor, capped by the row
        // budget so infinite scroll can't blow past MAX_ROWS DOM nodes.
        PageMode::Infinite => (0, cursor.max(size).min(total).min(MAX_ROWS)),
        // Unknown/future mode: no windowing — degrade to the capped full list.
        PageMode::Unknown => (0, total.min(MAX_ROWS)),
    }
}

/// The client-side pagination affordance rendered after a paginated loop's rows:
/// a numbered pager (`Paged`) or a grow-on-scroll sentinel (`Infinite`).
fn pagination_controls(
    st: UiState,
    scope: Scope,
    source: String,
    filters: Vec<ForEachFilter>,
    node_id: String,
    p: Pagination,
) -> AnyView {
    match p.mode {
        PageMode::Paged => pager_footer(st, scope, source, filters, node_id, p.page_size),
        PageMode::Infinite => infinite_sentinel(st, scope, source, filters, node_id, p.page_size),
        PageMode::Unknown => ().into_any(),
    }
}

/// The count of filtered rows for a paginated loop — recomputed reactively from
/// live state so the pager / sentinel track filter edits.
fn live_filtered_count(
    st: UiState,
    scope: &Scope,
    source: &str,
    filters: &[ForEachFilter],
) -> usize {
    st.data.with(|d| {
        let abs = abs_data_path(scope, source).unwrap_or_else(|| source.to_string());
        filtered_indices(d, scope, &abs, filters, MAX_TOTAL_ROWS).len()
    })
}

/// A numbered pager (prev / "Page X of Y" / next) under a `paged` loop. The
/// active page lives in [`UiState::pages`] keyed by the loop's node id; the whole
/// footer rebuilds on a page change or a filter edit, both cheap.
fn pager_footer(
    st: UiState,
    scope: Scope,
    source: String,
    filters: Vec<ForEachFilter>,
    node_id: String,
    page_size: usize,
) -> AnyView {
    view! {
        <div class="eu-pager">
            {move || {
                let total = live_filtered_count(st, &scope, &source, &filters);
                let pages = page_count(total, page_size);
                let page = st.page(&node_id, 0).min(pages - 1);
                let at_start = page == 0;
                let at_end = page + 1 >= pages;
                let prev_id = node_id.clone();
                let next_id = node_id.clone();
                view! {
                    <button
                        class="eu-btn eu-pager-btn"
                        type="button"
                        disabled=at_start
                        on:click=move |_| st.set_page(&prev_id, page.saturating_sub(1))
                    >
                        "‹ Prev"
                    </button>
                    <span class="eu-pager-status">{format!("Page {} of {}", page + 1, pages)}</span>
                    <button
                        class="eu-btn eu-pager-btn"
                        type="button"
                        disabled=at_end
                        on:click=move |_| st.set_page(&next_id, (page + 1).min(pages - 1))
                    >
                        "Next ›"
                    </button>
                }
            }}
        </div>
    }
    .into_any()
}

/// The infinite-scroll sentinel under an `infinite` loop: an `IntersectionObserver`
/// reveals one more page each time the sentinel scrolls into the viewport, plus a
/// "Load more" button as the accessible fallback (and for inner scroll containers
/// the viewport-rooted observer can't see). The revealed-row count lives in
/// [`UiState::pages`] keyed by the loop's node id.
fn infinite_sentinel(
    st: UiState,
    scope: Scope,
    source: String,
    filters: Vec<ForEachFilter>,
    node_id: String,
    page_size: usize,
) -> AnyView {
    let sentinel_ref: NodeRef<leptos::html::Div> = NodeRef::new();

    // Reveal one more page, capped by the filtered length and the row budget.
    let reveal_more = {
        let scope = scope.clone();
        let source = source.clone();
        let filters = filters.clone();
        let id = node_id.clone();
        move || {
            let total = st.data.with_untracked(|d| {
                let abs = abs_data_path(&scope, &source).unwrap_or_else(|| source.clone());
                filtered_indices(d, &scope, &abs, &filters, MAX_TOTAL_ROWS).len()
            });
            let size = page_size.clamp(1, MAX_PAGE_SIZE);
            let cur = st.page_untracked(&id, 0).max(size);
            let next = (cur + size).min(total).min(MAX_ROWS);
            if next > cur {
                st.set_page(&id, next);
            }
        }
    };

    // Install the observer once the sentinel element mounts; disconnect + drop
    // the JS closure on unmount. The observer/closure are `!Send`, so they live
    // in a LocalStorage-backed `StoredValue` whose HANDLE is `Send + Sync` (what
    // `on_cleanup` requires) while the value stays thread-local.
    let slot = StoredValue::new_local(
        None::<(
            web_sys::IntersectionObserver,
            Closure<dyn FnMut(js_sys::Array)>,
        )>,
    );
    {
        let reveal = reveal_more.clone();
        Effect::new(move |installed: Option<bool>| {
            if installed == Some(true) {
                return true;
            }
            let Some(el) = sentinel_ref.get() else {
                return false;
            };
            let el: web_sys::Element = el.unchecked_into();
            let reveal = reveal.clone();
            let cb = Closure::<dyn FnMut(js_sys::Array)>::wrap(Box::new(
                move |entries: js_sys::Array| {
                    let hit = entries.iter().any(|e| {
                        e.dyn_ref::<web_sys::IntersectionObserverEntry>()
                            .is_some_and(|e| e.is_intersecting())
                    });
                    if hit {
                        reveal();
                    }
                },
            ));
            match web_sys::IntersectionObserver::new(cb.as_ref().unchecked_ref()) {
                Ok(obs) => {
                    obs.observe(&el);
                    slot.set_value(Some((obs, cb)));
                    true
                }
                // Construction failed — the "Load more" button still works.
                Err(_) => false,
            }
        });
    }
    on_cleanup(move || {
        slot.update_value(|s| {
            if let Some((obs, _cb)) = s.take() {
                obs.disconnect();
            }
        });
    });

    view! {
        <div class="eu-scroll-sentinel" node_ref=sentinel_ref>
            {move || {
                let total = live_filtered_count(st, &scope, &source, &filters);
                let (_, end) = page_bounds(PageMode::Infinite, total, page_size, st.page(&node_id, 0));
                (end < total && end < MAX_ROWS).then(|| {
                    let reveal = reveal_more.clone();
                    view! {
                        <button
                            class="eu-btn eu-more-btn"
                            type="button"
                            on:click=move |_| reveal()
                        >
                            "Load more"
                        </button>
                    }
                })
            }}
        </div>
    }
    .into_any()
}

/// The `<For>` key for row `i`: the author `key` path resolved in the row scope
/// (falling back to the index if it is unset or resolves empty).
fn row_key(
    scope: &Scope,
    data: &Json,
    source: &str,
    item_name: &str,
    index_name: &Option<String>,
    key_path: Option<&str>,
    i: usize,
) -> String {
    let Some(kp) = key_path else {
        return i.to_string();
    };
    let abs = abs_data_path(scope, source).unwrap_or_else(|| source.to_string());
    let mut row = scope.with_item(item_name, format!("{abs}.{i}"));
    if let Some(idx) = index_name {
        row = row.with_index(idx, i);
    }
    let k = stringify(&resolve_value(&row, data, kp));
    if k.is_empty() {
        i.to_string()
    } else {
        k
    }
}

/// Whether one `for_each` row passes its declarative filter, given the resolved
/// row `field` and `query` values. A falsy query (empty/cleared search box)
/// passes every row, and an unknown/future mode never filters — both degrade to
/// the unfiltered list rather than hiding data.
fn filter_passes(field: &Json, query: &Json, mode: FilterMode) -> bool {
    if !truthy(query) {
        return true;
    }
    match mode {
        FilterMode::Contains => stringify(field)
            .to_lowercase()
            .contains(&stringify(query).to_lowercase()),
        FilterMode::Equals => field == query || stringify(field) == stringify(query),
        FilterMode::Unknown => true,
    }
}

/// Render the node by its kind (after `for_each`/`show_if` have been handled).
fn render_kind(node: UiNode, st: UiState, scope: Scope, depth: usize) -> AnyView {
    match node.kind {
        NodeKind::Text => {
            let txt = interp(prop_tpl(&node, "text"), st, scope);
            view! { <span class="eu-text">{txt}</span> }.into_any()
        }
        NodeKind::Heading => heading(&node, st, scope),
        NodeKind::Markdown => {
            let tpl = prop_tpl(&node, "text");
            let html = move || {
                st.data
                    .with(|d| markdown_html(&interpolate(&tpl, d, &scope)))
            };
            view! { <div class="eu-md msg-markdown" inner_html=html></div> }.into_any()
        }
        NodeKind::Divider => view! { <hr class="eu-divider" /> }.into_any(),
        NodeKind::Image => image(&node, st, scope),
        NodeKind::Link => link(&node, st, scope),
        NodeKind::Badge => badge(&node, st, scope),
        NodeKind::ProgressBar => progress_bar(&node, st, scope),
        NodeKind::PieChart => chart_host(node, scope, st, |n, d, s, o| {
            charts::pie_chart(datums_from_json(&resolve_prop(n, "data", d, s)), o)
        }),
        NodeKind::DonutChart => chart_host(node, scope, st, |n, d, s, o| {
            charts::donut_chart(datums_from_json(&resolve_prop(n, "data", d, s)), o)
        }),
        NodeKind::BarChart => chart_host(node, scope, st, |n, d, s, o| {
            let horizontal = n.props.get("horizontal").map(truthy).unwrap_or(false);
            charts::bar_chart(
                datums_from_json(&resolve_prop(n, "data", d, s)),
                horizontal,
                o,
            )
        }),
        NodeKind::LineChart => chart_host(node, scope, st, |n, d, s, o| {
            charts::line_chart(datums_from_json(&resolve_prop(n, "data", d, s)), o)
        }),
        NodeKind::AreaChart => chart_host(node, scope, st, |n, d, s, o| {
            charts::area_chart(datums_from_json(&resolve_prop(n, "data", d, s)), o)
        }),
        NodeKind::Sparkline => chart_host(node, scope, st, |n, d, s, o| {
            charts::sparkline(numbers_from_json(&resolve_prop(n, "data", d, s)), o)
        }),
        NodeKind::Gauge => chart_host(node, scope, st, |n, d, s, o| {
            let value = num_prop(n, "value", d, s, 0.0);
            let min = num_prop(n, "min", d, s, 0.0);
            let max = num_prop(n, "max", d, s, 100.0);
            charts::gauge(value, min, max, o)
        }),
        NodeKind::RadarChart => chart_host(node, scope, st, |n, d, s, o| {
            let axes = strings_from_json(&resolve_prop(n, "axes", d, s));
            let values = numbers_from_json(&resolve_prop(n, "data", d, s));
            charts::radar_chart(axes, values, o)
        }),
        NodeKind::Heatmap => chart_host(node, scope, st, |n, d, s, o| {
            let rows = strings_from_json(&resolve_prop(n, "rows", d, s));
            let cols = strings_from_json(&resolve_prop(n, "cols", d, s));
            let grid = grid_from_json(&resolve_prop(n, "data", d, s));
            charts::heatmap(rows, cols, grid, o)
        }),
        NodeKind::Stack | NodeKind::Row | NodeKind::Grid | NodeKind::Card | NodeKind::Tab => {
            container(node, st, scope, depth)
        }
        NodeKind::ConstrainedBox => constrained_box(node, st, scope, depth),
        NodeKind::AspectRatio => aspect_ratio(node, st, scope, depth),
        NodeKind::Tabs => tabs(node, st, scope, depth),
        NodeKind::Dialog => dialog(node, st, scope, depth),
        NodeKind::Button => button(node, st, scope, depth),
        NodeKind::TextInput | NodeKind::Textarea | NodeKind::NumberInput | NodeKind::DateInput => {
            scalar_field(node, st, scope)
        }
        NodeKind::Select => select_field(node, st, scope),
        NodeKind::RadioGroup => radio_field(node, st, scope),
        NodeKind::Checkbox => checkbox_field(node, st, scope),
        NodeKind::Slider => slider_field(node, st, scope),
        NodeKind::List => list_node(node, st, scope),
        NodeKind::Table => table_node(node, st, scope),
        NodeKind::Timer => timer_node(&node, st, scope, true),
        NodeKind::Stopwatch => timer_node(&node, st, scope, false),
        NodeKind::ViewRef => view_ref(&node, st, scope, depth),
        NodeKind::AppRef => app_ref(&node, st),
        // A future/unknown kind still shows any children it carries.
        NodeKind::Unknown => {
            let kids = render_children(&node, st, &scope, depth);
            view! { <div class="eu-unknown">{kids}</div> }.into_any()
        }
    }
}

// ---------------------------------------------------------------------------
// Composition — sub-views (same spec) and sub-apps (another emerged UI)
// ---------------------------------------------------------------------------

/// Render another view of this spec inline (`props.view`). Authoring rejects
/// `view_ref` cycles; the interpreter's depth cap is the runtime backstop.
fn view_ref(node: &UiNode, st: UiState, scope: Scope, depth: usize) -> AnyView {
    let target = node
        .props
        .get("view")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    let root = st
        .views
        .with_value(|vs| vs.iter().find(|v| v.id == target).map(|v| v.root.clone()));
    match root {
        Some(root) => render_node(root, st, scope, depth + 1),
        None => view! { <div class="eu-load-error">"Unknown view: " {target}</div> }.into_any(),
    }
}

/// Mount another emerged UI inline (`props.app` = its ui id or name) — the
/// shell-app seam. Depth is pre-checked here; the id-cycle check lives in
/// [`EmergedUi`] after the reference resolves (a name can only be compared to
/// the chain once the server says which App it is). The child gets its own
/// [`EmergedUi`] (own state, own `/event` authority) and inherits the chat
/// `ai_sink` so its `ai` handlers still reach the transcript.
fn app_ref(node: &UiNode, st: UiState) -> AnyView {
    let child = node
        .props
        .get("app")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if child.is_empty() {
        return view! { <div class="eu-load-error">"app_ref: missing `app` target."</div> }
            .into_any();
    }
    let chain = st.app_chain.get_value();
    if chain.iter().any(|id| id == &child) || chain.len() >= MAX_APP_DEPTH {
        return view! {
            <div class="eu-load-error">"This app embeds itself too deeply (or cyclically)."</div>
        }
        .into_any();
    }
    match st.ai_sink {
        Some(sink) => view! { <EmergedUi ui_id=child ai_sink=sink chain=chain /> }.into_any(),
        None => view! { <EmergedUi ui_id=child chain=chain /> }.into_any(),
    }
}

// ---------------------------------------------------------------------------
// Timers — client-run countdown / stopwatch leaves
// ---------------------------------------------------------------------------

/// A `timer` (countdown, `props.duration` seconds) or `stopwatch` (count-up)
/// node. Run state lives in [`UiState::timers`] keyed by node id (like tabs and
/// dialogs — never in form `data`), so it is addressable from any handler via
/// the `start_timer`/`pause_timer`/`reset_timer` ops. One 250 ms interval per
/// mounted node bumps a local `tick` only while running; a countdown that
/// reaches zero stops exactly at the duration and fires its `complete` handler
/// once per run.
fn timer_node(node: &UiNode, st: UiState, scope: Scope, countdown: bool) -> AnyView {
    let id = node.id.clone();
    let label = {
        let t = prop_tpl(node, "label");
        (!t.is_empty()).then(|| interp(t, st, scope.clone()))
    };
    let show_controls = node.props.get("controls").map(truthy).unwrap_or(true);

    // The local tick: bumped by the interval only while this timer runs, so an
    // idle timer costs nothing reactive.
    let tick = RwSignal::new(0u64);
    {
        let run_id = id.clone();
        if let Ok(handle) = set_interval_with_handle(
            move || {
                if st.timer_running_untracked(&run_id) {
                    tick.update(|t| *t += 1);
                }
            },
            std::time::Duration::from_millis(250),
        ) {
            on_cleanup(move || handle.clear());
        }
    }

    // `auto_start`: begin on mount (once — nothing reactive is tracked).
    if node.props.get("auto_start").map(truthy).unwrap_or(false) {
        let start_id = id.clone();
        Effect::new(move |prev: Option<()>| {
            if prev.is_none() && !st.timer_running_untracked(&start_id) {
                st.start_timer(&start_id);
            }
        });
    }

    // Countdown completion: stop exactly at the duration, fire `complete` once.
    if countdown {
        let done_id = id.clone();
        let done_scope = scope.clone();
        let done_node = node.clone();
        let complete = node.events.get(&EventName::Complete).cloned();
        Effect::new(move |_| {
            tick.get();
            let t = st.timer(&done_id);
            if !t.running() {
                return;
            }
            let dur_ms = st
                .data
                .with(|d| num_prop(&done_node, "duration", d, &done_scope, 0.0))
                * 1000.0;
            if dur_ms > 0.0 && t.elapsed_ms(now_ms()) >= dur_ms && st.finish_timer(&done_id, dur_ms)
            {
                if let Some(h) = &complete {
                    handlers::dispatch(st, &done_scope, &done_id, EventName::Complete, h);
                }
            }
        });
    }

    // The read-out: remaining (countdown, rounded up) or elapsed (stopwatch).
    let disp_id = id.clone();
    let disp_node = node.clone();
    let disp_scope = scope.clone();
    let display = move || {
        tick.get(); // re-render while running
        let t = st.timer(&disp_id); // re-render on start/pause/reset
        let elapsed = t.elapsed_ms(now_ms());
        if countdown {
            let dur_ms = st
                .data
                .with(|d| num_prop(&disp_node, "duration", d, &disp_scope, 0.0))
                * 1000.0;
            format_clock(((dur_ms - elapsed).max(0.0) / 1000.0).ceil() as u64)
        } else {
            format_clock((elapsed / 1000.0).floor() as u64)
        }
    };
    let done_class_id = id.clone();
    let done_node = node.clone();
    let done_scope = scope.clone();
    let class = move || {
        let done = countdown && {
            tick.get();
            let t = st.timer(&done_class_id);
            let dur_ms = st
                .data
                .with(|d| num_prop(&done_node, "duration", d, &done_scope, 0.0))
                * 1000.0;
            dur_ms > 0.0 && t.elapsed_ms(now_ms()) >= dur_ms
        };
        if done {
            "eu-timer eu-timer-done"
        } else {
            "eu-timer"
        }
    };

    let controls = show_controls.then(|| {
        let toggle_id = id.clone();
        let toggle_read = id.clone();
        let reset_id = id.clone();
        let toggle_label = move || {
            if st.timer(&toggle_read).running() {
                "Pause"
            } else {
                "Start"
            }
        };
        view! {
            <div class="eu-timer-controls">
                <button
                    class="eu-btn eu-timer-btn"
                    type="button"
                    on:click=move |_| {
                        if st.timer_running_untracked(&toggle_id) {
                            st.pause_timer(&toggle_id);
                        } else {
                            st.start_timer(&toggle_id);
                        }
                    }
                >
                    {toggle_label}
                </button>
                <button
                    class="eu-btn eu-timer-btn"
                    type="button"
                    on:click=move |_| st.reset_timer(&reset_id)
                >
                    "Reset"
                </button>
            </div>
        }
    });

    view! {
        <div class=class>
            {label.map(|l| view! { <span class="eu-timer-label">{l}</span> })}
            <span class="eu-timer-display">{display}</span>
            {controls}
        </div>
    }
    .into_any()
}

/// `h:mm:ss` (past an hour) / `m:ss` display for a whole-second count.
fn format_clock(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

fn container(node: UiNode, st: UiState, scope: Scope, depth: usize) -> AnyView {
    let class = match node.kind {
        NodeKind::Row => "eu-row",
        NodeKind::Grid => "eu-grid",
        NodeKind::Card => "eu-card",
        _ => "eu-stack",
    };
    let header = (node.kind == NodeKind::Card)
        .then(|| prop_tpl(&node, "title"))
        .filter(|t| !t.is_empty())
        .map(|t| {
            let title = interp(t, st, scope.clone());
            view! { <div class="eu-card-title">{title}</div> }
        });
    let kids = render_children(&node, st, &scope, depth);
    view! { <div class=class>{header}{kids}</div> }.into_any()
}

fn render_children(node: &UiNode, st: UiState, scope: &Scope, depth: usize) -> Vec<AnyView> {
    node.children
        .iter()
        .cloned()
        .map(|c| render_node(c, st, scope.clone(), depth + 1))
        .collect()
}

/// A responsive single-child boundary. Dimensions are authored as logical CSS
/// pixels, validated server-side and clamped again here because the browser may
/// render an older/unvalidated persisted definition.
fn constrained_box(node: UiNode, st: UiState, scope: Scope, depth: usize) -> AnyView {
    let style = constraint_style(&node);
    let align = match node.props.get("align").and_then(Json::as_str) {
        Some("center") => "eu-align-center",
        Some("end") => "eu-align-end",
        Some("stretch") => "eu-align-stretch",
        _ => "eu-align-start",
    };
    let overflow = match node.props.get("overflow").and_then(Json::as_str) {
        Some("hidden") => "eu-overflow-hidden",
        Some("auto") => "eu-overflow-auto",
        _ => "eu-overflow-visible",
    };
    let class = format!("eu-constrained {align} {overflow}");
    let kids = render_children(&node, st, &scope, depth);
    view! { <div class=class style=style>{kids}</div> }.into_any()
}

fn constraint_style(node: &UiNode) -> String {
    [
        ("min_width", "min-width"),
        ("max_width", "max-width"),
        ("min_height", "min-height"),
        ("max_height", "max-height"),
    ]
    .into_iter()
    .filter_map(|(prop, css)| {
        node.props
            .get(prop)
            .and_then(Json::as_f64)
            .filter(|n| (0.0..=10_000.0).contains(n))
            .map(|n| format!("{css}:{n}px"))
    })
    .collect::<Vec<_>>()
    .join(";")
}

/// A ratio-preserving single-child frame. `fit` is meaningful for replaced
/// media such as images; other child kinds simply receive the available box.
fn aspect_ratio(node: UiNode, st: UiState, scope: Scope, depth: usize) -> AnyView {
    let ratio = node
        .props
        .get("ratio")
        .and_then(Json::as_f64)
        .filter(|n| (0.05..=20.0).contains(n))
        .unwrap_or(1.0);
    let fit = match node.props.get("fit").and_then(Json::as_str) {
        Some("cover") => "eu-fit-cover",
        Some("fill") => "eu-fit-fill",
        _ => "eu-fit-contain",
    };
    let class = format!("eu-aspect-ratio {fit}");
    let style = format!("aspect-ratio:{ratio}");
    let kids = render_children(&node, st, &scope, depth);
    view! { <div class=class style=style>{kids}</div> }.into_any()
}

/// A tab container: a header strip (one button per child, labelled by the child's
/// `props.label`) over the active child's panel. The active index lives in
/// [`UiState::tabs`] keyed by this node's id, so a click — or a handler-driven
/// `select_tab` action — switches the panel without touching form `data`. The
/// panel rebuilds only when the active index changes (a `Memo`), and reading the
/// index subscribes to `tabs` alone, so unrelated state edits don't remount it.
fn tabs(node: UiNode, st: UiState, scope: Scope, depth: usize) -> AnyView {
    let node_id = node.id.clone();
    let children = node.children.clone();
    let count = children.len();

    let headers = children
        .iter()
        .enumerate()
        .map(|(i, child)| {
            let text = {
                let l = prop_tpl(child, "label");
                if l.is_empty() {
                    format!("Tab {}", i + 1)
                } else {
                    l
                }
            };
            let label = interp(text, st, scope.clone());
            let class_id = node_id.clone();
            let class = move || {
                if st.active_tab(&class_id, count) == i {
                    "eu-tab eu-tab-active"
                } else {
                    "eu-tab"
                }
            };
            let click_id = node_id.clone();
            let on_click = move |_| st.set_tab(&click_id, i);
            view! {
                <button class=class type="button" role="tab" on:click=on_click>
                    {label}
                </button>
            }
        })
        .collect::<Vec<_>>();

    let panel_id = node_id;
    let active = Memo::new(move |_| st.active_tab(&panel_id, count));
    let panel = move || {
        let i = active.get();
        children.get(i).cloned().map_or_else(
            || ().into_any(),
            |c| render_node(c, st, scope.clone(), depth + 1),
        )
    };

    view! {
        <div class="eu-tabs">
            <div class="eu-tab-strip" role="tablist">{headers}</div>
            <div class="eu-tab-panel">{panel}</div>
        </div>
    }
    .into_any()
}

fn dialog(node: UiNode, st: UiState, scope: Scope, depth: usize) -> AnyView {
    let id = node.id.clone();
    let header = {
        let t = prop_tpl(&node, "title");
        (!t.is_empty()).then(|| interp(t, st, scope.clone()))
    };
    let kids = render_children(&node, st, &scope, depth);
    let backdrop_class = {
        let id = id.clone();
        move || {
            if st.is_dialog_open(&id) {
                "eu-dialog-backdrop eu-open"
            } else {
                "eu-dialog-backdrop"
            }
        }
    };
    let backdrop_close_id = id.clone();
    let btn_close_id = id;
    view! {
        <div
            class=backdrop_class
            on:click=move |_| st.dialogs.update(|d| d.retain(|x| x != &backdrop_close_id))
        >
            <div class="eu-dialog" on:click=|ev| ev.stop_propagation()>
                <div class="eu-dialog-head">
                    <span class="eu-dialog-title">{header}</span>
                    <button
                        class="eu-dialog-x"
                        type="button"
                        on:click=move |_| st.dialogs.update(|d| d.retain(|x| x != &btn_close_id))
                    >
                        <Icon icon=MdIcon::Close />
                    </button>
                </div>
                <div class="eu-dialog-body">{kids}</div>
            </div>
        </div>
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

fn heading(node: &UiNode, st: UiState, scope: Scope) -> AnyView {
    let level = node
        .props
        .get("level")
        .and_then(Json::as_u64)
        .unwrap_or(3)
        .clamp(1, 6);
    let txt = interp(prop_tpl(node, "text"), st, scope);
    match level {
        1 => view! { <h1 class="eu-heading">{txt.clone()}</h1> }.into_any(),
        2 => view! { <h2 class="eu-heading">{txt.clone()}</h2> }.into_any(),
        3 => view! { <h3 class="eu-heading">{txt.clone()}</h3> }.into_any(),
        4 => view! { <h4 class="eu-heading">{txt.clone()}</h4> }.into_any(),
        5 => view! { <h5 class="eu-heading">{txt.clone()}</h5> }.into_any(),
        _ => view! { <h6 class="eu-heading">{txt}</h6> }.into_any(),
    }
}

/// An `<img>`. `src` and `alt` may interpolate `{{path}}`; the resolved `src`
/// is scheme-checked every render so a `javascript:`/foreign-`data:` URL never
/// reaches the DOM (falls back to no `src`, leaving the `alt` text). Two extra
/// sources beyond plain URLs:
/// - `files://<store>/<path>` (or `files:<path>` on the default store) → the
///   authed storage download URL, so a workspace file renders directly.
/// - a `db` prop (`{connection, sql, params?, column?}`) → the authed
///   `GET /uis/{id}/image/{node}` endpoint, which runs the **spec-held** SQL
///   against the named external database and serves the image bytes; only the
///   client-resolved bind values ride in the URL.
fn image(node: &UiNode, st: UiState, scope: Scope) -> AnyView {
    let alt = interp(prop_tpl(node, "alt"), st, scope.clone());
    if node.props.get("db").is_some() {
        let db_node = node.clone();
        let src_scope = scope;
        let src = move || {
            let params = st.data.with(|d| db_image_params(&db_node, d, &src_scope));
            let token = auth::resolve_token();
            rest::ui_image_url(
                token.as_deref(),
                &st.ui_id.get_value(),
                &db_node.id,
                &params,
            )
        };
        return view! { <img class="eu-img" src=src alt=alt /> }.into_any();
    }
    let src_tpl = prop_tpl(node, "src");
    let src_scope = scope;
    let src = move || {
        let raw = st.data.with(|d| interpolate(&src_tpl, d, &src_scope));
        resolve_image_src(&raw).unwrap_or_default()
    };
    view! { <img class="eu-img" src=src alt=alt /> }.into_any()
}

/// Resolve an image `src` to what the DOM gets: a `files:` reference becomes
/// the authed storage URL; anything else passes through the scheme allow-list.
fn resolve_image_src(raw: &str) -> Option<String> {
    if let Some((store, key)) = parse_files_src(raw.trim()) {
        let token = auth::resolve_token();
        return Some(rest::download_url(
            token.as_deref(),
            &key,
            (!store.is_empty()).then_some(store.as_str()),
        ));
    }
    safe_url(raw, true)
}

/// Parse a `files://<store>/<path>` / `files:<path>` (default store) image
/// source into `(store, key)`. `None` when it is not a files reference (or the
/// key is empty — `safe_url` then rejects the stray `files:` scheme).
fn parse_files_src(src: &str) -> Option<(String, String)> {
    if let Some(rest) = src.strip_prefix("files://") {
        let (store, key) = rest.split_once('/')?;
        let key = key.trim_start_matches('/');
        if key.is_empty() {
            return None;
        }
        return Some((store.trim().to_string(), key.to_string()));
    }
    if let Some(key) = src.strip_prefix("files:") {
        let key = key.trim_start_matches('/');
        if key.is_empty() {
            return None;
        }
        return Some((String::new(), key.to_string()));
    }
    None
}

/// The client-resolved bind values for an image's authored `db.params`: a
/// string that is exactly one `{{path}}` keeps its typed value (a numeric row
/// id stays a number); other strings splice as text; non-strings pass through.
fn db_image_params(node: &UiNode, data: &Json, scope: &Scope) -> Vec<Json> {
    let Some(arr) = node
        .props
        .get("db")
        .and_then(|db| db.get("params"))
        .and_then(Json::as_array)
    else {
        return Vec::new();
    };
    arr.iter()
        .map(|p| match p {
            Json::String(s) => {
                let t = s.trim();
                if let Some(path) = t
                    .strip_prefix("{{")
                    .and_then(|x| x.strip_suffix("}}"))
                    .filter(|inner| !inner.contains("{{") && !inner.contains("}}"))
                {
                    resolve_value(scope, data, path.trim())
                } else {
                    Json::String(interpolate(s, data, scope))
                }
            }
            other => other.clone(),
        })
        .collect()
}

/// An `<a>`. `href` and `label` may interpolate `{{path}}`; the resolved `href`
/// is scheme-checked (unsafe → `#`). Opens in a new tab when `props.external` is
/// truthy, always with `rel="noopener noreferrer"`.
fn link(node: &UiNode, st: UiState, scope: Scope) -> AnyView {
    let href_tpl = prop_tpl(node, "href");
    let label_tpl = {
        let l = prop_tpl(node, "label");
        if l.is_empty() {
            prop_tpl(node, "text")
        } else {
            l
        }
    };
    let external = node
        .props
        .get("external")
        .map(super::path::truthy)
        .unwrap_or(false);
    let target = external.then_some("_blank");
    let label = interp(label_tpl, st, scope.clone());
    let href_scope = scope;
    let href = move || {
        let raw = st.data.with(|d| interpolate(&href_tpl, d, &href_scope));
        safe_url(&raw, false).unwrap_or_else(|| "#".to_string())
    };
    view! {
        <a class="eu-link" href=href target=target rel="noopener noreferrer">
            {label}
        </a>
    }
    .into_any()
}

/// A small status badge. `props.text` interpolates `{{path}}`; `props.variant`
/// (`neutral` | `info` | `success` | `warn` | `error`) selects the colour.
fn badge(node: &UiNode, st: UiState, scope: Scope) -> AnyView {
    let variant = match node.props.get("variant").and_then(Json::as_str) {
        Some("info") => "info",
        Some("success") => "success",
        Some("warn") => "warn",
        Some("error") => "error",
        _ => "neutral",
    };
    let class = format!("eu-badge eu-badge-{variant}");
    let text = interp(prop_tpl(node, "text"), st, scope);
    view! { <span class=class>{text}</span> }.into_any()
}

/// A determinate progress bar. `props.value` (a number or `{{path}}`) over
/// `props.max` (default 100) sets the fill width, clamped to `[0, 100]%`.
fn progress_bar(node: &UiNode, st: UiState, scope: Scope) -> AnyView {
    let value_tpl = prop_tpl(node, "value");
    let max = node
        .props
        .get("max")
        .and_then(Json::as_f64)
        .filter(|m| *m > 0.0)
        .unwrap_or(100.0);
    let label = {
        let t = prop_tpl(node, "label");
        (!t.is_empty()).then(|| interp(t, st, scope.clone()))
    };
    let pct_scope = scope;
    let pct = move || {
        let v = st
            .data
            .with(|d| interpolate(&value_tpl, d, &pct_scope))
            .trim()
            .parse::<f64>()
            .unwrap_or(0.0);
        ((v / max) * 100.0).clamp(0.0, 100.0)
    };
    let style = move || format!("width: {:.1}%", pct());
    view! {
        <div class="eu-progress-wrap">
            {label.map(|l| view! { <span class="eu-progress-label">{l}</span> })}
            <div class="eu-progress">
                <div class="eu-progress-bar" style=style></div>
            </div>
        </div>
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// Charts (SOUL §12) — leaf nodes drawing a themed SVG from a `data` prop.
// ---------------------------------------------------------------------------

/// Reactive host for a chart leaf: reads `st.data` once per render and hands the
/// resolved state + a props-derived [`ChartOpts`] to `build`, which returns the
/// SVG. Reading `st.data` inside the effect subscribes the chart, so it redraws
/// whenever bound state (its `data`/`value`/… prop) changes — and only then.
fn chart_host(
    node: UiNode,
    scope: Scope,
    st: UiState,
    build: impl Fn(&UiNode, &Json, &Scope, &ChartOpts) -> AnyView + Send + 'static,
) -> AnyView {
    view! {
        <div class="chart-host">
            {move || {
                st.data
                    .with(|d| {
                        let opts = chart_opts(&node, d, &scope);
                        build(&node, d, &scope, &opts)
                    })
            }}
        </div>
    }
    .into_any()
}

/// Build a chart's shared options from its props (title interpolates `{{path}}`;
/// `width`/`height`/`colors`/`legend`/`max` override the per-kind defaults).
fn chart_opts(node: &UiNode, data: &Json, scope: &Scope) -> ChartOpts {
    let (def_w, def_h) = match node.kind {
        NodeKind::Sparkline => (160.0, 44.0),
        NodeKind::Gauge => (220.0, 150.0),
        _ => (360.0, 220.0),
    };
    let width = node
        .props
        .get("width")
        .and_then(Json::as_f64)
        .filter(|v| *v > 0.0)
        .unwrap_or(def_w);
    let height = node
        .props
        .get("height")
        .and_then(Json::as_f64)
        .filter(|v| *v > 0.0)
        .unwrap_or(def_h);
    let title = interpolate(&prop_tpl(node, "title"), data, scope);
    let mut opts = ChartOpts::default().size(width, height).title(title);
    if let Some(colors) = node.props.get("colors").and_then(Json::as_array) {
        let pal: Vec<String> = colors
            .iter()
            .filter_map(Json::as_str)
            .map(str::to_string)
            .collect();
        opts = opts.palette(pal);
    }
    if let Some(on) = node.props.get("legend").map(truthy) {
        opts = opts.legend(on);
    }
    if let Some(max) = node.props.get("max").and_then(Json::as_f64) {
        opts = opts.with_max(Some(max));
    }
    opts
}

/// Resolve a chart data prop to a JSON value: a literal (array/object), a
/// `{"$path":"state.path"}` reference, or a string that is a `{{path}}`/bare
/// state path. Anything else (or a missing prop) resolves to `null`.
fn resolve_prop(node: &UiNode, key: &str, data: &Json, scope: &Scope) -> Json {
    match node.props.get(key) {
        None => Json::Null,
        Some(Json::Object(m)) if m.len() == 1 => match m.get("$path") {
            Some(Json::String(p)) => resolve_value(scope, data, p),
            _ => Json::Object(m.clone()),
        },
        Some(Json::String(s)) => {
            let t = s.trim();
            let path = t
                .strip_prefix("{{")
                .and_then(|x| x.strip_suffix("}}"))
                .unwrap_or(t)
                .trim();
            resolve_value(scope, data, path)
        }
        Some(other) => other.clone(),
    }
}

/// A single numeric prop (gauge `value`/`min`/`max`): a JSON number, a numeric
/// string, or a `{{path}}`/`$path`/bare-path reference into state.
fn num_prop(node: &UiNode, key: &str, data: &Json, scope: &Scope, default: f64) -> f64 {
    match node.props.get(key) {
        Some(Json::Number(n)) => n.as_f64().unwrap_or(default),
        Some(Json::String(s)) => {
            let t = s.trim();
            if let Ok(v) = t.parse::<f64>() {
                v
            } else {
                let path = t
                    .strip_prefix("{{")
                    .and_then(|x| x.strip_suffix("}}"))
                    .unwrap_or(t)
                    .trim();
                resolve_value(scope, data, path).as_f64().unwrap_or(default)
            }
        }
        Some(Json::Object(m)) if m.len() == 1 => match m.get("$path") {
            Some(Json::String(p)) => resolve_value(scope, data, p).as_f64().unwrap_or(default),
            _ => default,
        },
        _ => default,
    }
}

/// Parse a JSON array into chart data: numbers (`[3,1,4]`), `{label,value,color?}`
/// objects, or `[label, value]` pairs. Non-arrays yield an empty series.
fn datums_from_json(v: &Json) -> Vec<Datum> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .map(|it| match it {
            Json::Number(n) => Datum {
                label: String::new(),
                value: n.as_f64().unwrap_or(0.0),
                color: None,
            },
            Json::Object(o) => Datum {
                label: o
                    .get("label")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string(),
                value: o.get("value").and_then(Json::as_f64).unwrap_or(0.0),
                color: o.get("color").and_then(Json::as_str).map(str::to_string),
            },
            Json::Array(pair) => Datum {
                label: pair
                    .first()
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string(),
                value: pair.get(1).and_then(Json::as_f64).unwrap_or(0.0),
                color: None,
            },
            _ => Datum {
                label: String::new(),
                value: 0.0,
                color: None,
            },
        })
        .collect()
}

/// The finite numbers of a JSON array (for sparkline / radar values).
fn numbers_from_json(v: &Json) -> Vec<f64> {
    v.as_array()
        .map(|a| a.iter().filter_map(Json::as_f64).collect())
        .unwrap_or_default()
}

/// A JSON array-of-arrays as a numeric grid (for the heatmap).
fn grid_from_json(v: &Json) -> Vec<Vec<f64>> {
    v.as_array()
        .map(|rows| rows.iter().map(numbers_from_json).collect())
        .unwrap_or_default()
}

/// A JSON array as display strings (axis / row / column labels); non-strings are
/// stringified so a numeric label array still reads.
fn strings_from_json(v: &Json) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| stringify(x))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Collections — read-only leaves over a `data` array (SOUL §12). Rows needing
// per-item interactivity (buttons, inputs) use `for_each` instead.
// ---------------------------------------------------------------------------

/// A read-only `<ul>`/`<ol>` over a `data` array. `props.item` picks the display
/// path within each element (unset = the element itself), `props.ordered` makes
/// it numbered, `props.empty` is the no-rows text. Reading `st.data` inside the
/// closure subscribes it, so the list redraws when its bound array changes.
fn list_node(node: UiNode, st: UiState, scope: Scope) -> AnyView {
    let ordered = node.props.get("ordered").map(truthy).unwrap_or(false);
    let cap = scope.budget().min(MAX_ROWS);
    view! {
        <div class="eu-list-host">
            {move || {
                st.data
                    .with(|d| {
                        let item_path = node.props.get("item").and_then(Json::as_str);
                        let items = list_items(
                            &resolve_prop(&node, "data", d, &scope),
                            item_path,
                            cap,
                        );
                        if items.is_empty() {
                            let empty = empty_text(&node, d, &scope);
                            return view! { <div class="eu-empty">{empty}</div> }.into_any();
                        }
                        let rows = items
                            .into_iter()
                            .map(|t| view! { <li>{t}</li> })
                            .collect::<Vec<_>>();
                        if ordered {
                            view! { <ol class="eu-list">{rows}</ol> }.into_any()
                        } else {
                            view! { <ul class="eu-list">{rows}</ul> }.into_any()
                        }
                    })
            }}
        </div>
    }
    .into_any()
}

/// A read-only `<table>` over a `data` array (rows of objects). Columns come
/// from `props.columns` (`["path"]` or `[{header?, path}]`), else derive from
/// the first row's keys; each cell stringifies the value at the column's path
/// within its row. Horizontal overflow scrolls inside the wrapper.
fn table_node(node: UiNode, st: UiState, scope: Scope) -> AnyView {
    let cap = scope.budget().min(MAX_ROWS);
    view! {
        <div class="eu-table-host">
            {move || {
                st.data
                    .with(|d| {
                        let data = resolve_prop(&node, "data", d, &scope);
                        let Some(rows) = data.as_array().filter(|r| !r.is_empty()) else {
                            let empty = empty_text(&node, d, &scope);
                            return view! { <div class="eu-empty">{empty}</div> }.into_any();
                        };
                        let cols = table_columns(node.props.get("columns"), rows.first());
                        let head = cols
                            .iter()
                            .map(|(h, _)| view! { <th>{h.clone()}</th> })
                            .collect::<Vec<_>>();
                        let body = rows
                            .iter()
                            .take(cap)
                            .map(|row| {
                                let cells = cols
                                    .iter()
                                    .map(|(_, p)| {
                                        let text = if p.is_empty() {
                                            stringify(row)
                                        } else {
                                            stringify(get_path(row, p))
                                        };
                                        view! { <td>{text}</td> }
                                    })
                                    .collect::<Vec<_>>();
                                view! { <tr>{cells}</tr> }
                            })
                            .collect::<Vec<_>>();
                        view! {
                            <div class="eu-table-scroll">
                                <table class="eu-table">
                                    <thead>
                                        <tr>{head}</tr>
                                    </thead>
                                    <tbody>{body}</tbody>
                                </table>
                            </div>
                        }
                        .into_any()
                    })
            }}
        </div>
    }
    .into_any()
}

/// The display strings of a `list` node's rows: each element of `data` (or its
/// `item` sub-path), stringified, capped. Non-arrays yield no rows.
fn list_items(data: &Json, item_path: Option<&str>, cap: usize) -> Vec<String> {
    data.as_array()
        .map(|a| {
            a.iter()
                .take(cap)
                .map(|el| match item_path {
                    Some(p) => stringify(get_path(el, p)),
                    None => stringify(el),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A `table`'s columns as `(header, path)` pairs, from its `columns` prop —
/// `["path", …]` or `[{header?, path}, …]` — else derived from the first row's
/// object keys. A scalar-rowed table falls back to one whole-row column (the
/// empty path stringifies the row itself).
fn table_columns(columns: Option<&Json>, first_row: Option<&Json>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(Json::Array(cols)) = columns {
        for c in cols {
            match c {
                Json::String(p) if !p.is_empty() => out.push((p.clone(), p.clone())),
                Json::Object(m) => {
                    let path = m
                        .get("path")
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let header = m
                        .get("header")
                        .and_then(Json::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| path.clone());
                    if !(path.is_empty() && header.is_empty()) {
                        out.push((header, path));
                    }
                }
                _ => {}
            }
        }
    }
    if out.is_empty() {
        if let Some(Json::Object(m)) = first_row {
            out = m.keys().map(|k| (k.clone(), k.clone())).collect();
        }
    }
    if out.is_empty() {
        out.push(("value".to_string(), String::new()));
    }
    out
}

/// The interpolated `props.empty` no-rows text (default "No data").
fn empty_text(node: &UiNode, data: &Json, scope: &Scope) -> String {
    let tpl = prop_tpl(node, "empty");
    if tpl.is_empty() {
        "No data".to_string()
    } else {
        interpolate(&tpl, data, scope)
    }
}

// ---------------------------------------------------------------------------
// Interactive
// ---------------------------------------------------------------------------

fn button(node: UiNode, st: UiState, scope: Scope, depth: usize) -> AnyView {
    // A button fires its `click` handler, falling back to `submit`; remember which
    // so the server payload names the right event.
    let fired = node
        .events
        .get(&EventName::Click)
        .map(|h| (EventName::Click, h.clone()))
        .or_else(|| {
            node.events
                .get(&EventName::Submit)
                .map(|h| (EventName::Submit, h.clone()))
        });
    let node_id = node.id.clone();
    let label = prop_tpl(&node, "label");
    let label = (!label.is_empty()).then(|| interp(label, st, scope.clone()));
    let kids = render_children(&node, st, &scope, depth);
    let click_scope = scope.clone();
    let on_click = move |_| {
        if let Some((event, h)) = &fired {
            handlers::dispatch(st, &click_scope, &node_id, *event, h);
        }
    };
    view! {
        <button class="eu-btn" type="button" on:click=on_click>
            {label}
            {kids}
        </button>
    }
    .into_any()
}

/// The single-control text-like inputs: `text_input`, `textarea`, `number_input`
/// and `date_input`. They share the label/value/blur-validation plumbing and
/// differ only by HTML input `type`, the optional `min`/`max`/`step` attributes,
/// and how the typed value is coerced back into state (numbers as JSON numbers).
fn scalar_field(node: UiNode, st: UiState, scope: Scope) -> AnyView {
    let id = node.id.clone();
    let kind = node.kind;
    let bind = node.bind.clone();
    let label = field_label(&node, st, scope.clone(), &id);
    let placeholder = interp(prop_tpl(&node, "placeholder"), st, scope.clone());
    let error = field_error_view(node.clone(), st, scope.clone());
    let multiline = kind == NodeKind::Textarea;

    // On blur, run any server-side script validation rules (no-op otherwise).
    let blur_node = node.clone();
    let blur_scope = scope.clone();
    let on_blur = move |_| handlers::validate_field(st, &blur_scope, &blur_node);
    // On a committed change (blur/Enter), fire the node's `change` handler.
    let on_change = change_dispatcher(&node, st, scope.clone());

    let value_scope = scope.clone();
    let value_bind = bind.clone();
    let value = move || match &value_bind {
        Some(b) => st.bind_string(&value_scope, b),
        None => String::new(),
    };
    let input_scope = scope;
    let input_bind = bind;
    let input_node = node.clone();
    let on_input = move |ev: web_sys::Event| {
        if let Some(b) = &input_bind {
            st.set_bind(&input_scope, b, coerce_input(kind, event_target_value(&ev)));
            handlers::bind_changed(st);
            handlers::dispatch_input_debounced(st, &input_scope, &input_node);
        }
    };

    let control = if multiline {
        view! {
            <textarea
                class="eu-input eu-textarea"
                id=id.clone()
                placeholder=placeholder
                prop:value=value
                on:input=on_input
                on:change=on_change
                on:blur=on_blur
            ></textarea>
        }
        .into_any()
    } else {
        view! {
            <input
                class="eu-input"
                type=input_type(kind)
                id=id.clone()
                placeholder=placeholder
                min=prop_attr(&node, "min")
                max=prop_attr(&node, "max")
                step=prop_attr(&node, "step")
                prop:value=value
                on:input=on_input
                on:change=on_change
                on:blur=on_blur
            />
        }
        .into_any()
    };
    view! { <div class="eu-field">{label}{control}{error}</div> }.into_any()
}

/// The `on:change` closure for an input node: dispatch its `change` handler
/// (any kind — client / tool / script / ai) with the fresh state snapshot. The
/// DOM fires `change` on *committed* values (text blur/Enter, a select pick, a
/// slider release), so a server-backed handler sees settled input, not
/// keystrokes.
fn change_dispatcher(
    node: &UiNode,
    st: UiState,
    scope: Scope,
) -> impl Fn(web_sys::Event) + Clone + 'static {
    let handler = node.events.get(&EventName::Change).cloned();
    let node_id = node.id.clone();
    move |_ev: web_sys::Event| {
        if let Some(h) = &handler {
            handlers::dispatch(st, &scope, &node_id, EventName::Change, h);
        }
    }
}

/// A range slider (`<input type="range">`) binding a JSON number, with a live
/// value read-out. `min`/`max`/`step` come from props (range defaults 0–100/1).
fn slider_field(node: UiNode, st: UiState, scope: Scope) -> AnyView {
    let id = node.id.clone();
    let bind = node.bind.clone();
    let label = field_label(&node, st, scope.clone(), &id);

    let value_scope = scope.clone();
    let value_bind = bind.clone();
    let value = move || match &value_bind {
        Some(b) => st.bind_string(&value_scope, b),
        None => String::new(),
    };
    let readout = value.clone();
    let on_change = change_dispatcher(&node, st, scope.clone());
    let input_scope = scope;
    let input_bind = bind;
    let input_node = node.clone();
    let on_input = move |ev: web_sys::Event| {
        if let Some(b) = &input_bind {
            st.set_bind(
                &input_scope,
                b,
                coerce_input(NodeKind::Slider, event_target_value(&ev)),
            );
            handlers::bind_changed(st);
            handlers::dispatch_input_debounced(st, &input_scope, &input_node);
        }
    };
    view! {
        <div class="eu-field">
            {label}
            <div class="eu-range-wrap">
                <input
                    class="eu-range"
                    type="range"
                    id=id
                    min=prop_attr(&node, "min").unwrap_or_else(|| "0".to_string())
                    max=prop_attr(&node, "max").unwrap_or_else(|| "100".to_string())
                    step=prop_attr(&node, "step").unwrap_or_else(|| "1".to_string())
                    prop:value=value
                    on:input=on_input
                    on:change=on_change
                />
                <span class="eu-range-value">{readout}</span>
            </div>
        </div>
    }
    .into_any()
}

/// A radio-button group over `props.options` (same shape as `select`), binding
/// the chosen option's value as a string. All radios share the node id as their
/// `name` so the browser enforces single-selection.
fn radio_field(node: UiNode, st: UiState, scope: Scope) -> AnyView {
    let id = node.id.clone();
    let bind = node.bind.clone();
    let label = field_label(&node, st, scope.clone(), &id);
    let error = field_error_view(node.clone(), st, scope.clone());
    let options = parse_options(&node);

    let radios = options
        .into_iter()
        .map(|(val, lbl)| {
            let checked_scope = scope.clone();
            let checked_bind = bind.clone();
            let opt = val.clone();
            let checked = move || match &checked_bind {
                Some(b) => st.bind_string(&checked_scope, b) == opt,
                None => false,
            };
            let change_scope = scope.clone();
            let change_bind = bind.clone();
            let chosen = val.clone();
            let fire_change = change_dispatcher(&node, st, scope.clone());
            let on_change = move |ev: web_sys::Event| {
                if let Some(b) = &change_bind {
                    st.set_bind(&change_scope, b, Json::String(chosen.clone()));
                    handlers::bind_changed(st);
                }
                fire_change(ev);
            };
            view! {
                <label class="eu-radio">
                    <input
                        type="radio"
                        name=id.clone()
                        value=val
                        prop:checked=checked
                        on:change=on_change
                    />
                    <span class="eu-radio-label">{lbl}</span>
                </label>
            }
        })
        .collect::<Vec<_>>();
    view! {
        <div class="eu-field">
            {label}
            <div class="eu-radio-group">{radios}</div>
            {error}
        </div>
    }
    .into_any()
}

fn select_field(node: UiNode, st: UiState, scope: Scope) -> AnyView {
    let id = node.id.clone();
    let bind = node.bind.clone();
    let label = field_label(&node, st, scope.clone(), &id);
    let error = field_error_view(node.clone(), st, scope.clone());
    let options = parse_options(&node);

    let blur_node = node.clone();
    let blur_scope = scope.clone();
    let on_blur = move |_| handlers::validate_field(st, &blur_scope, &blur_node);

    let value_scope = scope.clone();
    let value_bind = bind.clone();
    let value = move || match &value_bind {
        Some(b) => st.bind_string(&value_scope, b),
        None => String::new(),
    };
    let fire_change = change_dispatcher(&node, st, scope.clone());
    let change_scope = scope;
    let change_bind = bind;
    let on_change = move |ev: web_sys::Event| {
        if let Some(b) = &change_bind {
            st.set_bind(&change_scope, b, Json::String(event_target_value(&ev)));
            handlers::bind_changed(st);
        }
        fire_change(ev);
    };
    let opt_views = options
        .into_iter()
        .map(|(v, l)| view! { <option value=v>{l}</option> })
        .collect::<Vec<_>>();
    view! {
        <div class="eu-field">
            {label}
            <select
                class="eu-input eu-select"
                id=id
                prop:value=value
                on:change=on_change
                on:blur=on_blur
            >
                {opt_views}
            </select>
            {error}
        </div>
    }
    .into_any()
}

fn checkbox_field(node: UiNode, st: UiState, scope: Scope) -> AnyView {
    let id = node.id.clone();
    let bind = node.bind.clone();
    let label_text = prop_tpl(&node, "label");
    let label = (!label_text.is_empty()).then(|| interp(label_text, st, scope.clone()));

    let checked_scope = scope.clone();
    let checked_bind = bind.clone();
    let checked = move || match &checked_bind {
        Some(b) => st.bind_bool(&checked_scope, b),
        None => false,
    };
    let fire_change = change_dispatcher(&node, st, scope.clone());
    let change_scope = scope;
    let change_bind = bind;
    let on_change = move |ev: web_sys::Event| {
        if let Some(b) = &change_bind {
            st.set_bind(&change_scope, b, Json::Bool(event_target_checked(&ev)));
            handlers::bind_changed(st);
        }
        fire_change(ev);
    };
    let input_id = id.clone();
    view! {
        <label class="eu-checkbox" for=id>
            <input
                type="checkbox"
                id=input_id
                prop:checked=checked
                on:change=on_change
            />
            <span class="eu-checkbox-label">{label}</span>
        </label>
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The raw template string for a prop (`{{path}}` resolved later), or `""`.
fn prop_tpl(node: &UiNode, key: &str) -> String {
    node.props
        .get(key)
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A static HTML attribute value from a numeric-or-string prop (`min`/`max`/
/// `step` on number/date/range inputs). `None` → the attribute is omitted.
fn prop_attr(node: &UiNode, key: &str) -> Option<String> {
    match node.props.get(key) {
        Some(Json::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Json::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// The HTML `<input type>` for a text-like scalar input kind.
fn input_type(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::NumberInput => "number",
        NodeKind::DateInput => "date",
        _ => "text",
    }
}

/// Coerce a control's raw string value into the JSON stored in state: numeric
/// inputs store a JSON number (empty → null; unparseable → the raw string, so a
/// half-typed `"1."` is not silently dropped), everything else stores a string.
fn coerce_input(kind: NodeKind, raw: String) -> Json {
    match kind {
        NodeKind::NumberInput | NodeKind::Slider => {
            if raw.trim().is_empty() {
                Json::Null
            } else if let Some(n) = raw
                .trim()
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
            {
                Json::Number(n)
            } else {
                Json::String(raw)
            }
        }
        _ => Json::String(raw),
    }
}

/// Scheme-allow-list a URL authored into an `image` `src` or `link` `href`,
/// returning the URL when safe and `None` otherwise. Relative URLs and fragments
/// pass; `http`/`https`/`mailto`/`tel` pass; `data:image/*` passes only when
/// `allow_data_image` (images). Everything else — notably `javascript:` and
/// `vbscript:` — is rejected, upholding the "no raw JS in the tree" guarantee.
fn safe_url(raw: &str, allow_data_image: bool) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match url_scheme(trimmed) {
        // No scheme → relative path, absolute path, query or fragment: safe.
        None => Some(trimmed.to_string()),
        Some(scheme) => match scheme.as_str() {
            "http" | "https" | "mailto" | "tel" => Some(trimmed.to_string()),
            "data"
                if allow_data_image
                    && trimmed
                        .to_ascii_lowercase()
                        .replace(|c: char| c.is_ascii_whitespace() || c.is_control(), "")
                        .starts_with("data:image/") =>
            {
                Some(trimmed.to_string())
            }
            _ => None,
        },
    }
}

/// Extract a URL's scheme (lower-cased) if it has one. A scheme is a leading
/// `[a-z][a-z0-9+.-]*` run terminated by `:` before any `/`, `?` or `#`. ASCII
/// whitespace and control characters are ignored while scanning the scheme so a
/// `"java\tscript:"`-style obfuscation (which browsers would still execute)
/// cannot slip past as "no scheme".
fn url_scheme(url: &str) -> Option<String> {
    let mut scheme = String::new();
    for c in url.chars() {
        match c {
            ':' => return (!scheme.is_empty()).then_some(scheme),
            '/' | '?' | '#' => return None,
            _ if c.is_ascii_whitespace() || c.is_control() => {}
            _ if scheme.is_empty() && !c.is_ascii_alphabetic() => return None,
            _ if c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-') => {
                scheme.push(c.to_ascii_lowercase());
            }
            _ => return None,
        }
    }
    None
}

/// A reactive, cloneable closure that resolves `{{path}}` interpolation in `tpl`.
fn interp(tpl: String, st: UiState, scope: Scope) -> impl Fn() -> String + Clone {
    move || st.data.with(|d| interpolate(&tpl, d, &scope))
}

/// An optional `<label for=id>` for an input node, from its `label` prop.
fn field_label(node: &UiNode, st: UiState, scope: Scope, id: &str) -> Option<AnyView> {
    let text = prop_tpl(node, "label");
    if text.is_empty() {
        return None;
    }
    let label = interp(text, st, scope);
    let id = id.to_string();
    Some(view! { <label class="eu-label" for=id>{label}</label> }.into_any())
}

/// A reactive validation-error span for an input node, or `None` when the node
/// declares no rules (avoids an empty span under every field).
fn field_error_view(node: UiNode, st: UiState, scope: Scope) -> Option<AnyView> {
    if node.validate.is_empty() {
        return None;
    }
    let node_id = node.id.clone();
    Some(
        view! {
            <span class="eu-err">
                {move || {
                    // Sync client rules first; otherwise the async server-side
                    // script-validation result for this field.
                    st.field_error(&scope, &node)
                        .or_else(|| st.script_error(&node_id))
                        .unwrap_or_default()
                }}
            </span>
        }
        .into_any(),
    )
}

/// Parse a `<select>`'s `options` prop: `["a","b"]` or `[{value,label}, …]`.
fn parse_options(node: &UiNode) -> Vec<(String, String)> {
    let Some(Json::Array(arr)) = node.props.get("options") else {
        return Vec::new();
    };
    arr.iter()
        .map(|o| match o {
            Json::Object(m) => {
                let value = m.get("value").map(stringify).unwrap_or_default();
                let label = m
                    .get("label")
                    .map(stringify)
                    .unwrap_or_else(|| value.clone());
                (value, label)
            }
            other => {
                let s = stringify(other);
                (s.clone(), s)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn constraint_style_emits_only_safe_numeric_dimensions() {
        let node: UiNode = serde_json::from_value(json!({
            "id": "limit", "kind": "constrained_box",
            "props": {
                "min_width": 120,
                "max_width": 480.5,
                "max_height": 300,
                "min_height": -1
            }
        }))
        .unwrap();
        assert_eq!(
            constraint_style(&node),
            "min-width:120px;max-width:480.5px;max-height:300px"
        );
    }

    #[test]
    fn safe_url_allows_ordinary_targets() {
        for url in [
            "https://example.com/x",
            "http://example.com",
            "mailto:a@b.c",
            "tel:+15551234",
            "/relative/path",
            "#anchor",
            "?q=1",
            "page.html",
        ] {
            assert_eq!(safe_url(url, false), Some(url.to_string()), "{url}");
        }
    }

    #[test]
    fn safe_url_rejects_script_schemes() {
        // Including whitespace/obfuscation tricks browsers would still execute.
        for url in [
            "javascript:alert(1)",
            "  javascript:alert(1)",
            "java\tscript:alert(1)",
            "JavaScript:alert(1)",
            "vbscript:msgbox(1)",
            "data:text/html,<script>",
        ] {
            assert_eq!(safe_url(url, false), None, "{url} must be rejected");
        }
    }

    #[test]
    fn safe_url_data_image_only_for_images() {
        let img = "data:image/png;base64,iVBORw0KGgo=";
        assert_eq!(safe_url(img, true), Some(img.to_string()));
        // The same data URL is rejected for a link href (allow_data_image = false).
        assert_eq!(safe_url(img, false), None);
        // A non-image data URL is rejected even when images are allowed.
        assert_eq!(safe_url("data:text/html,x", true), None);
    }

    #[test]
    fn coerce_input_numbers_vs_strings() {
        assert_eq!(
            coerce_input(NodeKind::NumberInput, "42".into()),
            json!(42.0)
        );
        assert_eq!(coerce_input(NodeKind::Slider, "7".into()), json!(7.0));
        assert_eq!(coerce_input(NodeKind::NumberInput, "".into()), Json::Null);
        // A trailing dot still parses as a float (Rust accepts "1." → 1.0).
        assert_eq!(coerce_input(NodeKind::NumberInput, "1.".into()), json!(1.0));
        // A genuinely unparseable value is preserved as a string rather than dropped.
        assert_eq!(
            coerce_input(NodeKind::NumberInput, "1-2".into()),
            json!("1-2")
        );
        // Text-like kinds always store a string.
        assert_eq!(
            coerce_input(NodeKind::DateInput, "2026-06-29".into()),
            json!("2026-06-29")
        );
        assert_eq!(coerce_input(NodeKind::TextInput, "42".into()), json!("42"));
    }

    #[test]
    fn input_type_per_kind() {
        assert_eq!(input_type(NodeKind::NumberInput), "number");
        assert_eq!(input_type(NodeKind::DateInput), "date");
        assert_eq!(input_type(NodeKind::TextInput), "text");
    }

    #[test]
    fn filter_passes_contains_and_equals() {
        // Contains: case-insensitive substring on the stringified values.
        assert!(filter_passes(
            &json!("Chocolate Cake"),
            &json!("cake"),
            FilterMode::Contains
        ));
        assert!(!filter_passes(
            &json!("Chocolate Cake"),
            &json!("pie"),
            FilterMode::Contains
        ));
        // A falsy query (cleared search box) passes every row.
        for empty in [json!(""), Json::Null, json!(0)] {
            assert!(filter_passes(&json!("x"), &empty, FilterMode::Contains));
            assert!(filter_passes(&json!("x"), &empty, FilterMode::Equals));
        }
        // Equals: JSON equality, with scalar-vs-string coercion (a select's
        // string value matches a numeric field).
        assert!(filter_passes(
            &json!("dessert"),
            &json!("dessert"),
            FilterMode::Equals
        ));
        assert!(!filter_passes(
            &json!("dessert"),
            &json!("main"),
            FilterMode::Equals
        ));
        assert!(filter_passes(&json!(4), &json!("4"), FilterMode::Equals));
        // An unknown/future mode never filters (degrade to the full list).
        assert!(filter_passes(&json!("x"), &json!("y"), FilterMode::Unknown));
    }

    #[test]
    fn filtered_indices_all_and_filtered() {
        let d = json!({
            "rows": [{ "t": "apple" }, { "t": "banana" }, { "t": "cherry" }],
            "q": "an"
        });
        let sc = Scope::default();
        // No filters → every index, in order, capped.
        assert_eq!(filtered_indices(&d, &sc, "rows", &[], 10), vec![0, 1, 2]);
        assert_eq!(filtered_indices(&d, &sc, "rows", &[], 2), vec![0, 1]);
        // A contains filter on `t` against the live query at `q` keeps banana,
        // and rows keep their ORIGINAL index (1, not 0).
        let f = vec![ForEachFilter {
            path: Some("t".into()),
            query: "q".into(),
            mode: FilterMode::Contains,
        }];
        assert_eq!(filtered_indices(&d, &sc, "rows", &f, 10), vec![1]);
        // A missing array yields no rows.
        assert!(filtered_indices(&d, &sc, "missing", &[], 10).is_empty());
    }

    #[test]
    fn page_bounds_paged_and_infinite() {
        // 23 rows, 10 per page → 3 pages; each page slices in-bounds.
        assert_eq!(page_count(23, 10), 3);
        assert_eq!(page_bounds(PageMode::Paged, 23, 10, 0), (0, 10));
        assert_eq!(page_bounds(PageMode::Paged, 23, 10, 1), (10, 20));
        assert_eq!(page_bounds(PageMode::Paged, 23, 10, 2), (20, 23));
        // A stale over-large cursor snaps back to the last page.
        assert_eq!(page_bounds(PageMode::Paged, 23, 10, 99), (20, 23));
        // An empty list is one empty page.
        assert_eq!(page_count(0, 10), 1);
        assert_eq!(page_bounds(PageMode::Paged, 0, 10, 0), (0, 0));
        // Infinite: at least one page, grows with the cursor, capped at total.
        assert_eq!(page_bounds(PageMode::Infinite, 23, 10, 0), (0, 10));
        assert_eq!(page_bounds(PageMode::Infinite, 23, 10, 20), (0, 20));
        assert_eq!(page_bounds(PageMode::Infinite, 23, 10, 999), (0, 23));
        // A zero/oversized page_size is clamped to the sane range.
        assert_eq!(page_bounds(PageMode::Paged, 5, 0, 0), (0, 1));
        assert_eq!(page_count(5, 0), 5);
        // Unknown mode degrades to the whole (capped) list.
        assert_eq!(page_bounds(PageMode::Unknown, 7, 10, 3), (0, 7));
    }

    #[test]
    fn list_items_pick_path_and_cap() {
        let data = json!([
            { "title": "Cake", "mins": 40 },
            { "title": "Soup", "mins": 20 },
            { "title": "Stew", "mins": 90 }
        ]);
        assert_eq!(
            list_items(&data, Some("title"), 10),
            vec!["Cake", "Soup", "Stew"]
        );
        // Capped, and without `item` the whole element stringifies (compact JSON).
        assert_eq!(list_items(&data, Some("title"), 2), vec!["Cake", "Soup"]);
        assert_eq!(list_items(&json!(["a", "b"]), None, 10), vec!["a", "b"]);
        // Non-arrays yield no rows.
        assert!(list_items(&json!({ "not": "array" }), None, 10).is_empty());
    }

    #[test]
    fn format_clock_reads_naturally() {
        assert_eq!(format_clock(0), "0:00");
        assert_eq!(format_clock(9), "0:09");
        assert_eq!(format_clock(75), "1:15");
        assert_eq!(format_clock(600), "10:00");
        assert_eq!(format_clock(3600), "1:00:00");
        assert_eq!(format_clock(3600 + 75), "1:01:15");
    }

    #[test]
    fn parse_files_src_forms() {
        // Named store + nested key.
        assert_eq!(
            parse_files_src("files://minio/recipes/cake.png"),
            Some(("minio".to_string(), "recipes/cake.png".to_string()))
        );
        // Default store: empty authority or the bare `files:` form.
        assert_eq!(
            parse_files_src("files:///cake.png"),
            Some((String::new(), "cake.png".to_string()))
        );
        assert_eq!(
            parse_files_src("files:cake.png"),
            Some((String::new(), "cake.png".to_string()))
        );
        // Not files references / empty keys.
        assert_eq!(parse_files_src("https://x/y.png"), None);
        assert_eq!(parse_files_src("files://storeonly"), None);
        assert_eq!(parse_files_src("files:"), None);
    }

    #[test]
    fn db_image_params_resolve_typed_and_spliced() {
        let node: UiNode = serde_json::from_value(json!({
            "id": "img", "kind": "image",
            "props": { "db": {
                "connection": "shop", "sql": "SELECT photo FROM r WHERE id = $1 AND tag = $2",
                "params": ["{{sel.id}}", "tag-{{sel.kind}}", 7]
            } }
        }))
        .unwrap();
        let data = json!({ "sel": { "id": 42, "kind": "cake" } });
        assert_eq!(
            db_image_params(&node, &data, &Scope::default()),
            vec![json!(42), json!("tag-cake"), json!(7)]
        );
        // No params → empty.
        let bare: UiNode = serde_json::from_value(json!({
            "id": "img", "kind": "image", "props": { "db": { "connection": "c", "sql": "s" } }
        }))
        .unwrap();
        assert!(db_image_params(&bare, &data, &Scope::default()).is_empty());
    }

    #[test]
    fn table_columns_from_prop_or_first_row() {
        // Explicit string / object columns, order kept.
        let cols = table_columns(
            Some(&json!(["title", { "header": "Minutes", "path": "mins" }])),
            None,
        );
        assert_eq!(
            cols,
            vec![
                ("title".to_string(), "title".to_string()),
                ("Minutes".to_string(), "mins".to_string())
            ]
        );
        // Omitted → derived from the first row's keys.
        let derived = table_columns(None, Some(&json!({ "a": 1, "b": 2 })));
        assert_eq!(
            derived,
            vec![
                ("a".to_string(), "a".to_string()),
                ("b".to_string(), "b".to_string())
            ]
        );
        // Scalar rows fall back to one whole-row column (empty path).
        assert_eq!(
            table_columns(None, Some(&json!("plain"))),
            vec![("value".to_string(), String::new())]
        );
    }
}
