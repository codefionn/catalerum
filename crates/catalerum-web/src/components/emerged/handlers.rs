//! Event dispatch for emerged-UI handlers.
//!
//! [`Handler::Client`] ops run locally through the [`UiState`] reducer — the full
//! surface of v1 *local* interactivity (navigation, dialogs, form state). The two
//! authority-bearing kinds — [`Handler::Tool`] and [`Handler::Script`] (the Boa
//! host bridge) — round-trip to `POST /uis/{id}/event` carrying the firing node,
//! event, and the **full client state snapshot** (so the server sees in-progress
//! fields, not a stale row), then apply the returned
//! [`UiAction`](super::model::UiAction)s. [`Handler::Ai`] (relay into chat) is not
//! wired from a UI yet and still surfaces a notice.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;

use super::model::{EventName, Handler, UiAction, UiNode, ValidationKind};
use super::path::Scope;
use super::state::{now_ms, UiState};
use crate::{auth, rest};

/// Debounce window for the live `computed.*` refresh while the user types.
const COMPUTE_DEBOUNCE_MS: u64 = 350;

/// Called after every two-way `bind` write. When the spec declares `computed.*`
/// values, schedule a debounced `POST /uis/{id}/compute` against a fresh
/// snapshot, so server-derived values (a servings scaler, a subtotal) track
/// typing live without a round-trip per keystroke. Superseded schedules — and
/// responses that arrive after a newer edit — are dropped via the generation
/// counter.
pub fn bind_changed(st: UiState) {
    if !st.has_computed {
        return;
    }
    let generation = {
        let mut g = 0;
        st.compute_gen.update_value(|v| {
            *v += 1;
            g = *v;
        });
        g
    };
    set_timeout(
        move || {
            if st.compute_gen.get_value() != generation {
                return; // a newer edit re-scheduled
            }
            let ui_id = st.ui_id.get_value();
            let state = st.snapshot();
            spawn_local(async move {
                let token = auth::resolve_token();
                if let Ok(computed) = rest::post_ui_compute(token.as_deref(), &ui_id, &state).await
                {
                    // Drop a stale response that lost the race to a newer edit.
                    if st.compute_gen.get_value() == generation {
                        st.set_computed(computed);
                    }
                }
            });
        },
        Duration::from_millis(COMPUTE_DEBOUNCE_MS),
    );
}

/// Dispatch the handler bound to `event` on node `node_id`. Local ops apply
/// immediately; tool/script handlers post to the server and apply the result.
pub fn dispatch(st: UiState, scope: &Scope, node_id: &str, event: EventName, handler: &Handler) {
    match handler {
        Handler::Client { ops } => {
            for op in ops {
                st.apply_op(scope, op);
            }
        }

        // Authority-bearing: round-trip, then apply the server's actions. The
        // state snapshot + resolved `for_each` scope go in the payload so the
        // server can interpolate args / drive a Boa script against live values.
        Handler::Tool { .. } | Handler::Script { .. } => {
            let ui_id = st.ui_id.get_value();
            let state = st.snapshot();
            let scope = scope.resolve(&state);
            let node_id = node_id.to_string();
            spawn_local(async move {
                let token = auth::resolve_token();
                match rest::post_ui_event(token.as_deref(), &ui_id, &node_id, event, &state, &scope)
                    .await
                {
                    Ok(actions) => {
                        for action in &actions {
                            st.apply_action(action);
                        }
                    }
                    Err(e) => st
                        .notice
                        .set(Some(format!("This action could not run: {e}"))),
                }
            });
        }

        // Relay into chat as a new turn: prompt (if any) + an event note + the
        // current state (when `include_state`). No round-trip — the assistant
        // replies in the transcript, where it can read/act with full authority.
        Handler::Ai {
            prompt,
            include_state,
        } => match st.ai_sink {
            Some(sink) => {
                let mut msg = prompt.clone().unwrap_or_default();
                if msg.is_empty() {
                    msg = format!("The user activated UI control `{node_id}`.");
                }
                if *include_state {
                    let state = st.snapshot();
                    let pretty =
                        serde_json::to_string_pretty(&state).unwrap_or_else(|_| state.to_string());
                    msg = format!("{msg}\n\nCurrent UI state:\n```json\n{pretty}\n```");
                }
                sink.run(msg);
            }
            None => st.notice.set(Some(
                "This control asks the assistant, but no chat is connected here.".to_string(),
            )),
        },
    }
}

/// Debounce window for per-keystroke `input` handlers.
const INPUT_DEBOUNCE_MS: u64 = 400;

/// Fire an input node's `input` handler, debounced per node id: rapid
/// keystrokes collapse into one dispatch ~400 ms after the last edit (the
/// two-way `bind` itself stays per-keystroke and local). A no-op for nodes
/// without an `input` handler, so every input renderer can call it.
pub fn dispatch_input_debounced(st: UiState, scope: &Scope, node: &UiNode) {
    let Some(handler) = node.events.get(&EventName::Input).cloned() else {
        return;
    };
    let node_id = node.id.clone();
    let generation = {
        let mut g = 0;
        st.input_gens.update_value(|m| {
            let v = m.entry(node_id.clone()).or_insert(0);
            *v += 1;
            g = *v;
        });
        g
    };
    let scope = scope.clone();
    set_timeout(
        move || {
            let current = st
                .input_gens
                .with_value(|m| m.get(&node_id).copied().unwrap_or(0));
            if current != generation {
                return; // a newer keystroke re-scheduled
            }
            dispatch(st, &scope, &node_id, EventName::Input, &handler);
        },
        Duration::from_millis(INPUT_DEBOUNCE_MS),
    );
}

// ---------------------------------------------------------------------------
// Mount-time `load` dedup — the chat-replay guard
// ---------------------------------------------------------------------------

/// How long a mount-time `load` result is shared across mounts of the same
/// `(ui, version, node)`. Reopening a chat replays every line at once: each
/// mount of the same presented UI joins one in-flight round trip (or applies
/// the fresh cached actions) instead of re-firing the tool N times.
const LOAD_CACHE_TTL_MS: f64 = 30_000.0;

/// One cache slot: a round trip in flight (with the mounts waiting on it), or
/// a completed load's actions.
enum LoadEntry {
    Pending(Vec<UiState>),
    Done {
        at_ms: f64,
        actions: Rc<Vec<UiAction>>,
    },
}

thread_local! {
    static LOAD_CACHE: RefCell<HashMap<(String, i64, String), LoadEntry>> =
        RefCell::new(HashMap::new());
}

/// What the cache told a joining mount to do.
enum Joined {
    /// Fresh cached actions — apply them, no round trip.
    Hit(Rc<Vec<UiAction>>),
    /// A round trip is in flight; this mount is now on its waiter list.
    Wait,
    /// This mount fires the round trip (and owes the cache its result).
    Fire,
}

/// Fire a view root's **mount-time** `load` handler with cross-mount dedup.
///
/// Only the authority-bearing kinds go through the cache — at mount every copy
/// of the same `(ui, version)` posts the identical seeded `initial_state`, so
/// their results are interchangeable. Navigate-to `load`s (whose state has
/// diverged per mount) must use plain [`dispatch`] instead. `Client`/`Ai`
/// handlers are local/interactive and never cached.
pub fn dispatch_load(st: UiState, version: i64, node_id: &str, handler: &Handler) {
    if !matches!(handler, Handler::Tool { .. } | Handler::Script { .. }) {
        dispatch(st, &Scope::default(), node_id, EventName::Load, handler);
        return;
    }
    let key = (st.ui_id.get_value(), version, node_id.to_string());
    let joined = LOAD_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        match c.get_mut(&key) {
            Some(LoadEntry::Pending(waiters)) => {
                waiters.push(st);
                Joined::Wait
            }
            Some(LoadEntry::Done { at_ms, actions }) if now_ms() - *at_ms < LOAD_CACHE_TTL_MS => {
                Joined::Hit(actions.clone())
            }
            _ => {
                c.insert(key.clone(), LoadEntry::Pending(vec![st]));
                Joined::Fire
            }
        }
    });
    match joined {
        // Apply outside the cache borrow — a signal write may run reactions.
        Joined::Hit(actions) => {
            for action in actions.iter() {
                st.apply_action(action);
            }
        }
        Joined::Wait => {}
        Joined::Fire => {
            let ui_id = st.ui_id.get_value();
            let state = st.snapshot();
            let node_id = node_id.to_string();
            spawn_local(async move {
                let token = auth::resolve_token();
                let empty_scope = serde_json::Value::Object(serde_json::Map::new());
                let result = rest::post_ui_event(
                    token.as_deref(),
                    &ui_id,
                    &node_id,
                    EventName::Load,
                    &state,
                    &empty_scope,
                )
                .await;
                // Swap the pending slot for the outcome and collect the waiters
                // (this mount included), then apply outside the borrow.
                let (waiters, outcome) = LOAD_CACHE.with(|c| {
                    let mut c = c.borrow_mut();
                    let waiters = match c.remove(&key) {
                        Some(LoadEntry::Pending(w)) => w,
                        _ => vec![st],
                    };
                    let outcome = match result {
                        Ok(actions) => {
                            let actions = Rc::new(actions);
                            c.insert(
                                key,
                                LoadEntry::Done {
                                    at_ms: now_ms(),
                                    actions: actions.clone(),
                                },
                            );
                            Ok(actions)
                        }
                        // Leave no entry on failure so a later mount retries.
                        Err(e) => Err(e.to_string()),
                    };
                    (waiters, outcome)
                });
                match outcome {
                    Ok(actions) => {
                        for w in &waiters {
                            for action in actions.iter() {
                                w.apply_action(action);
                            }
                        }
                    }
                    Err(e) => {
                        for w in &waiters {
                            w.notice
                                .set(Some(format!("This action could not run: {e}")));
                        }
                    }
                }
            });
        }
    }
}

/// Run a field's server-side script validation rules
/// ([`ValidationKind::Script`]) and record the first failure (or clear) in
/// [`UiState::script_errors`]. A no-op for fields with no script rule, so it is
/// safe to attach to every input's `blur`.
pub fn validate_field(st: UiState, scope: &Scope, node: &UiNode) {
    let scripts: Vec<String> = node
        .validate
        .iter()
        .filter_map(|r| match &r.rule {
            ValidationKind::Script { handler } => Some(handler.clone()),
            _ => None,
        })
        .collect();
    let Some(bind) = node.bind.clone() else {
        return;
    };
    if scripts.is_empty() {
        return;
    }

    let value = st.bind_json(scope, &bind);
    let state = st.snapshot();
    let ui_id = st.ui_id.get_value();
    let node_id = node.id.clone();
    spawn_local(async move {
        let token = auth::resolve_token();
        for handler in &scripts {
            match rest::post_ui_validate(token.as_deref(), &ui_id, handler, &value, &state).await {
                Ok(result) => {
                    let ok = result
                        .get("ok")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true);
                    if !ok {
                        let msg = result
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("Invalid value.")
                            .to_string();
                        st.set_script_error(&node_id, Some(msg));
                        return;
                    }
                }
                Err(e) => {
                    st.set_script_error(&node_id, Some(e.to_string()));
                    return;
                }
            }
        }
        // All rules passed.
        st.set_script_error(&node_id, None);
    });
}
