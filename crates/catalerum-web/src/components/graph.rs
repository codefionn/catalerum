//! The Graph panel (SOUL §6.3, §12 — graph explorer).
//!
//! A single-pane query tool over `POST /graph/query`: a **Datalog** editor and a
//! results table. It is a thin client of the graph-query route — `graph:query`-gated.
//! The server parses, validates, and evaluates the program **in-process** over the
//! caller's workspace facts; scope is structural (the language cannot name a
//! workspace), so a read can never reach another tenant and there is no injection
//! surface (SOUL §18/§19). An invalid or unsafe program comes back as a `400` shown
//! in the error banner. The server also **caps** the returned rows (a broad read
//! can't dump a whole workspace slice in one response, §18/§19); a capped result is
//! flagged and the meta line says so.
//!
//! A result cell that is a node/relationship (a JSON object/array) is **clickable**:
//! selecting it opens a detail pane that lists the value's properties as
//! `(key, value)` rows. (Datalog rows are scalar strings, so cells are usually plain
//! text; the detail pane still works for any JSON cell.)

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api::{GraphQueryRequest, GraphQueryResponse};
use crate::auth;
use crate::rest;

/// A starter Datalog program shown on first load — every node and its label.
const STARTER_QUERY: &str =
    "% Every node and its label. Try: note(N), prop(N, \"title\", T).\n?- node(X, Label).";

/// The Graph panel component.
#[component]
pub fn GraphPanel() -> impl IntoView {
    let query = RwSignal::new(STARTER_QUERY.to_string());
    let running = RwSignal::new(false);
    let error = RwSignal::new(Option::<String>::None);
    let result = RwSignal::new(Option::<GraphQueryResponse>::None);
    // The node/relationship cell the user clicked to inspect, if any — its
    // properties are surfaced in the detail pane. Cleared on each new query.
    let selected = RwSignal::new(Option::<serde_json::Value>::None);

    let run = move || {
        if running.get_untracked() {
            return;
        }
        let q = query.get_untracked().trim().to_string();
        error.set(None);
        if q.is_empty() {
            error.set(Some("Enter a Datalog query.".to_string()));
            return;
        }
        running.set(true);
        result.set(None);
        selected.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::graph_query(token.as_deref(), &GraphQueryRequest { query: q }).await {
                Ok(r) => {
                    result.set(Some(r));
                    error.set(None);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            running.set(false);
        });
    };

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        run();
    };

    view! {
        <section class="graph-panel">
            <header class="graph-header">
                <div class="graph-header-titles">
                    <h2 class="graph-title">"Graph"</h2>
                    <span class="graph-subtitle">
                        "Safe Datalog over the derived graph — e.g. "
                        <code>"?- note(N)."</code>
                    </span>
                </div>
                <form class="graph-form" on:submit=on_submit>
                    <textarea
                        class="graph-cypher"
                        placeholder="?- note(N), references(N, T), topic(T)."
                        disabled=move || running.get()
                        prop:value=move || query.get()
                        on:input=move |ev| query.set(event_target_value(&ev))
                    ></textarea>
                    <div class="graph-form-actions">
                        <button class="graph-btn" type="submit" disabled=move || running.get()>
                            {move || if running.get() { "Running…" } else { "Run" }}
                        </button>
                    </div>
                </form>
            </header>

            <div class="graph-body">
                <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                    <div class="graph-status graph-error">
                        {move || error.get().unwrap_or_default()}
                    </div>
                </Show>

                <Show
                    when=move || running.get() && result.with(Option::is_none)
                    fallback=|| ().into_view()
                >
                    <div class="graph-status">"Running the query…"</div>
                </Show>

                {move || {
                    result.get().map(|r| {
                        let columns = r.columns.clone();
                        let rows = r.rows.clone();
                        let row_count = rows.len();
                        let truncated = r.truncated;
                        let col_count = columns.len();
                        if row_count == 0 {
                            return view! {
                                <div class="graph-status">"The query returned no rows."</div>
                            }
                            .into_any();
                        }
                        let header = columns
                            .iter()
                            .map(|c| view! { <th>{c.clone()}</th> })
                            .collect::<Vec<_>>();
                        let body = rows
                            .iter()
                            .map(|row| {
                                let cells = (0..col_count)
                                    .map(|i| {
                                        // A node/relationship comes back as a JSON object (an
                                        // array too) — make it a clickable cell that opens its
                                        // properties in the detail pane. Scalars stay plain text.
                                        match row.get(i) {
                                            Some(v) if v.is_object() || v.is_array() => {
                                                let v = v.clone();
                                                let label = cell_text(&v);
                                                view! {
                                                    <td>
                                                        <button
                                                            class="graph-cell-node"
                                                            on:click=move |_| selected.set(Some(v.clone()))
                                                        >
                                                            {label}
                                                        </button>
                                                    </td>
                                                }
                                                .into_any()
                                            }
                                            other => {
                                                let text = other.map(cell_text).unwrap_or_default();
                                                view! { <td>{text}</td> }.into_any()
                                            }
                                        }
                                    })
                                    .collect::<Vec<_>>();
                                view! { <tr>{cells}</tr> }
                            })
                            .collect::<Vec<_>>();
                        view! {
                            <div class="graph-result">
                                <div class="graph-result-meta">
                                    {format!(
                                        "{row_count} row{}{}",
                                        if row_count == 1 { "" } else { "s" },
                                        if truncated {
                                            " · capped — narrow the query (a tighter goal) to see the rest"
                                        } else {
                                            ""
                                        },
                                    )}
                                </div>
                                <div class="graph-table-wrap">
                                    <table class="graph-table">
                                        <thead>
                                            <tr>{header}</tr>
                                        </thead>
                                        <tbody>{body}</tbody>
                                    </table>
                                </div>
                            </div>
                        }
                        .into_any()
                    })
                }}

                <Show
                    when=move || selected.with(Option::is_some)
                    fallback=|| ().into_view()
                >
                    <div class="graph-detail">
                        <div class="graph-detail-head">
                            <span class="graph-detail-title">"Selected node"</span>
                            <button
                                class="graph-btn graph-detail-close"
                                on:click=move |_| selected.set(None)
                            >
                                "Close"
                            </button>
                        </div>
                        <ul class="graph-detail-fields">
                            {move || {
                                node_fields(&selected.get().unwrap_or(serde_json::Value::Null))
                                    .into_iter()
                                    .map(|(k, v)| {
                                        view! {
                                            <li class="graph-field">
                                                <span class="graph-field-key">{k}</span>
                                                <span class="graph-field-val">{v}</span>
                                            </li>
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            }}
                        </ul>
                    </div>
                </Show>
            </div>
        </section>
    }
}

/// Break a clicked result value into displayable `(key, value)` rows for the
/// detail pane: an **object** (a graph node/relationship) yields one row per
/// property, an **array** yields index→element rows, and a scalar a single
/// `"value"` row. Each value is rendered with [`cell_text`], so a nested
/// object stays compact JSON. Pure, so the breakdown is unit-testable.
fn node_fields(value: &serde_json::Value) -> Vec<(String, String)> {
    match value {
        serde_json::Value::Object(map) => {
            map.iter().map(|(k, v)| (k.clone(), cell_text(v))).collect()
        }
        serde_json::Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), cell_text(v)))
            .collect(),
        other => vec![("value".to_string(), cell_text(other))],
    }
}

/// Render one result cell: strings verbatim, scalars stringified, and
/// objects/arrays as compact JSON (a graph node/relationship comes back as a JSON
/// object). `null` renders empty.
fn cell_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cell_text_renders_scalars_and_json() {
        assert_eq!(cell_text(&json!("Ada")), "Ada");
        assert_eq!(cell_text(&json!(3)), "3");
        assert_eq!(cell_text(&json!(true)), "true");
        assert_eq!(cell_text(&serde_json::Value::Null), "");
        // Objects/arrays come back as compact JSON.
        assert_eq!(cell_text(&json!({"name":"Ada"})), r#"{"name":"Ada"}"#);
        assert_eq!(cell_text(&json!(["a", "b"])), r#"["a","b"]"#);
    }
}
