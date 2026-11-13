use super::*;

// --- node-mutation helpers used by the config editors ---

/// Replace the selected trigger node's `trigger` payload.
pub(super) fn set_node_trigger(graph: RwSignal<FlowGraph>, id: &str, trigger: Value) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            if let FlowKind::Trigger { trigger: t } = &mut n.kind {
                *t = trigger;
            }
        }
    });
}

/// The selected trigger node's current payload (`Value::Null` if it isn't a trigger
/// node). Untracked — the config fields seed from it once.
pub(super) fn node_trigger(graph: RwSignal<FlowGraph>, id: &str) -> Value {
    graph
        .with_untracked(|g| {
            g.nodes
                .iter()
                .find(|n| n.id == id)
                .and_then(|n| match &n.kind {
                    FlowKind::Trigger { trigger } => Some(trigger.clone()),
                    _ => None,
                })
        })
        .unwrap_or(Value::Null)
}

/// Merge a single typed field into the selected trigger node's payload, preserving
/// every other key (including opaque predicates the typed form doesn't model).
pub(super) fn set_trigger_field(graph: RwSignal<FlowGraph>, id: &str, key: &str, value: &str) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            if let FlowKind::Trigger { trigger: t } = &mut n.kind {
                *t = merge_field(std::mem::take(t), key, value);
            }
        }
    });
}

/// Merge the collect trigger's `every` cadence field: a typed value (a bare-integer
/// minutes number, a `{"seconds":N}`-style object, or a duration string like `5m`) via
/// [`every_value`] when non-blank, removed when blank — so `every`'s non-string shapes
/// survive (unlike [`set_trigger_field`], which only writes strings). Persists exactly
/// what the user typed; the server clamps `[60s, 1 year]` at scan time.
pub(super) fn set_trigger_every(graph: RwSignal<FlowGraph>, id: &str, raw: &str) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            if let FlowKind::Trigger { trigger: t } = &mut n.kind {
                if let Some(obj) = t.as_object_mut() {
                    match every_value(raw) {
                        Some(v) => {
                            obj.insert("every".to_string(), v);
                        }
                        None => {
                            obj.remove("every");
                        }
                    }
                }
            }
        }
    });
}

/// Set (or, when blank, remove) one `key` on a trigger object, keeping all other
/// keys. A [string-list field](is_string_list_field) (e.g. `extensions`) is written
/// as a JSON array split from the comma-separated `value`; every other field is a
/// plain string. Non-objects pass through unchanged. Pure + testable.
pub(super) fn merge_field(mut trigger: Value, key: &str, value: &str) -> Value {
    if let Some(obj) = trigger.as_object_mut() {
        let v = value.trim();
        if is_string_list_field(key) {
            let items: Vec<Value> = split_list(v).into_iter().map(Value::String).collect();
            if items.is_empty() {
                obj.remove(key);
            } else {
                obj.insert(key.to_string(), Value::Array(items));
            }
        } else if v.is_empty() {
            obj.remove(key);
        } else {
            obj.insert(key.to_string(), Value::String(v.to_string()));
        }
    }
    trigger
}

/// Replace the selected action node's `action` payload.
pub(super) fn set_node_action(graph: RwSignal<FlowGraph>, id: &str, action: Value) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            if let FlowKind::Action { action: a } = &mut n.kind {
                *a = action;
            }
        }
    });
}

/// Config panel for a **For each** loop head (SOUL §11): the array `source`
/// path, the `item` loop variable, and the optional `index` / `max_iterations`.
/// Every input writes straight onto the node (the [`set_for_each_field`]
/// pattern), like the other typed configs.
pub(super) fn for_each_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let read = |g: RwSignal<FlowGraph>, id: &str, pick: fn(&FlowKind) -> String| {
        g.with_untracked(|g| {
            g.nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| pick(&n.kind))
                .unwrap_or_default()
        })
    };
    let source_sig = RwSignal::new(read(graph, &id, |k| match k {
        FlowKind::ForEach { source, .. } => source.clone(),
        _ => String::new(),
    }));
    let item_sig = RwSignal::new(read(graph, &id, |k| match k {
        FlowKind::ForEach { item, .. } => item.clone(),
        _ => String::new(),
    }));
    let index_sig = RwSignal::new(read(graph, &id, |k| match k {
        FlowKind::ForEach { index, .. } => index.clone().unwrap_or_default(),
        _ => String::new(),
    }));
    let max_sig = RwSignal::new(read(graph, &id, |k| match k {
        FlowKind::ForEach { max_iterations, .. } => {
            max_iterations.map(|n| n.to_string()).unwrap_or_default()
        }
        _ => String::new(),
    }));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        set_for_each_fields(
            graph,
            &id_sv.get_value(),
            source_sig.get_untracked(),
            item_sig.get_untracked(),
            index_sig.get_untracked(),
            max_sig.get_untracked(),
        );
    };
    let field = move |label: &'static str,
                      placeholder: &'static str,
                      sig: RwSignal<String>,
                      hint: &'static str| {
        view! {
            <div class="flow-cfg-field">
                <label class="flow-cfg-label">{label}</label>
                <input
                    class="flow-cfg-input"
                    placeholder=placeholder
                    prop:value=move || sig.get()
                    on:input=move |ev| {
                        sig.set(event_target_value(&ev));
                        apply();
                    }
                />
                <div class="flow-cfg-hint">{hint}</div>
            </div>
        }
    };
    view! {
        {field(
            "Array source",
            "inputs.web_search.searches.rust.results",
            source_sig,
            "A path into the input envelope that yields an array — one iteration per element.",
        )}
        {field(
            "Item variable",
            "item",
            item_sig,
            "The loop variable the body references, e.g. {{ item.title }}.",
        )}
        {field(
            "Index variable (optional)",
            "i",
            index_sig,
            "Bound to the 0-based position when set.",
        )}
        {field(
            "Max iterations (optional)",
            "1000",
            max_sig,
            "Caps the run (hard ceiling 1000). Blank = the hard ceiling.",
        )}
        <div class="flow-cfg-hint">
            "Wire: For each → body nodes → End loop. The body runs once per element; \
             nodes after the End loop read the collected results array."
        </div>
    }
}

/// Config panel for an **End loop** node: pick which For-each head it closes.
/// The picker lists the graph's For-each node ids so the pairing can't typo.
pub(super) fn loop_end_config(graph: RwSignal<FlowGraph>, id: String, for_each: String) -> impl IntoView {
    let sig = RwSignal::new(for_each);
    let id_sv = StoredValue::new(id);
    let heads = move || {
        graph.with(|g| {
            g.nodes
                .iter()
                .filter(|n| matches!(n.kind, FlowKind::ForEach { .. }))
                .map(|n| n.id.clone())
                .collect::<Vec<_>>()
        })
    };
    view! {
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Closes the For-each"</label>
            <select
                class="flow-cfg-input"
                prop:value=move || sig.get()
                on:change=move |ev| {
                    let v = event_target_value(&ev);
                    sig.set(v.clone());
                    graph
                        .update(|g| {
                            if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id_sv.get_value()) {
                                if let FlowKind::LoopEnd { for_each } = &mut n.kind {
                                    *for_each = v.clone();
                                }
                            }
                        });
                }
            >
                <option value="">"— pick the loop head —"</option>
                {move || {
                    heads()
                        .into_iter()
                        .map(|h| {
                            let sel = {
                                let h = h.clone();
                                move || sig.get() == h
                            };
                            view! { <option value=h.clone() selected=sel>{h.clone()}</option> }
                        })
                        .collect::<Vec<_>>()
                }}
            </select>
            <Show when=move || heads().is_empty() fallback=|| ().into_view()>
                <div class="flow-cfg-warn">
                    "No For-each node in this graph yet — add one for this to close."
                </div>
            </Show>
            <div class="flow-cfg-hint">
                "Downstream nodes read this node's output: the array of per-iteration results."
            </div>
        </div>
    }
}

/// Overwrite a For-each node's fields in place (no-op for other kinds). Blank
/// `index` clears it; `max_iterations` keeps only a parseable positive number.
fn set_for_each_fields(
    graph: RwSignal<FlowGraph>,
    id: &str,
    source: String,
    item: String,
    index: String,
    max_iterations: String,
) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            if let FlowKind::ForEach {
                source: s,
                item: it,
                index: ix,
                max_iterations: mx,
            } = &mut n.kind
            {
                *s = source;
                *it = item;
                let index = index.trim();
                *ix = (!index.is_empty()).then(|| index.to_string());
                *mx = max_iterations
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0);
            }
        }
    });
}

/// Replace the selected code/condition node's `runtime` + `source`.
pub(super) fn set_node_code(
    graph: RwSignal<FlowGraph>,
    id: &str,
    runtime: String,
    source: String,
    is_condition: bool,
) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            match &mut n.kind {
                FlowKind::Code {
                    runtime: r,
                    source: s,
                }
                | FlowKind::Condition {
                    runtime: r,
                    source: s,
                } => {
                    let _ = is_condition;
                    *r = runtime;
                    *s = source;
                }
                _ => {}
            }
        }
    });
}

/// Parse the action-params textarea into a JSON object. Empty → an empty object;
/// a non-object or malformed JSON is an error.
pub(super) fn parse_params(input: &str) -> Result<Map<String, Value>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Map::new());
    }
    let v: Value = serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON ({e})"))?;
    match v {
        Value::Object(m) => Ok(m),
        _ => Err("expected a JSON object".to_string()),
    }
}
