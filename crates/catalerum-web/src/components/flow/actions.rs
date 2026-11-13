use super::*;

/// Action-node config: an action-kind `<select>`, then either a typed **Agent**
/// form (when the kind is `llm_agent` — a tool-calling chatbot) or a params-JSON
/// textarea for any other action kind.
pub(super) fn action_config(graph: RwSignal<FlowGraph>, id: String, action: Value) -> impl IntoView {
    let kind = action
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("summarize")
        .to_string();
    let kind_sig = RwSignal::new(kind);
    let id_for_kind = id.clone();
    let id_for_body = id.clone();
    let id_for_value = id.clone();

    view! {
        <label class="flow-cfg-label">"Action kind"</label>
        <select
            class="flow-cfg-input"
            on:change=move |ev| {
                let k = event_target_value(&ev);
                // Agents and decisions get ready-to-edit defaults; other kinds start empty.
                let next = if k == "llm_agent" {
                    default_agent_action()
                } else if k == "llm_decision" {
                    default_decision_action()
                } else {
                    serde_json::json!({ "kind": k })
                };
                set_node_action(graph, &id_for_kind, next);
                kind_sig.set(k);
            }
            prop:value=move || node_action(graph, &id_for_value).get("kind").and_then(Value::as_str).unwrap_or("summarize").to_string()
        >
            {ACTION_KINDS
                .iter()
                .map(|(k, label)| {
                    view! { <option value=*k>{*label}</option> }
                })
                .collect::<Vec<_>>()}
        </select>
        {move || {
            let id = id_for_body.clone();
            match kind_sig.get().as_str() {
                "llm_agent" | "llm_decision" => {
                    // Classifiers carry outcomes metadata on either model engine.
                    if is_classifier(&node_action(graph, &id)) {
                        classifier_config(graph, id).into_any()
                    } else if kind_sig.get() == "llm_decision" {
                        decision_config(graph, id).into_any()
                    } else {
                        agent_config(graph, id).into_any()
                    }
                }
                "notify" => notify_config(graph, id).into_any(),
                "create_chat_thread" => chat_thread_config(graph, id).into_any(),
                "start_chat" => start_chat_config(graph, id).into_any(),
                "create_note" => note_config(graph, id).into_any(),
                "create_task" => task_config(graph, id).into_any(),
                "create_event" => event_config(graph, id).into_any(),
                "summarize" => summarize_config(graph, id).into_any(),
                "write_object" => write_object_config(graph, id).into_any(),
                "move_object" => move_object_config(graph, id).into_any(),
                "webhook" => webhook_config(graph, id).into_any(),
                _ => params_config(graph, id).into_any(),
            }
        }}
    }
}

/// Typed config for a **Notify** action: a channel + message. The message can
/// reference upstream node output (it's available to the agent/runner), but the
/// common case is a fixed announcement; for a chatbot reply, an Agent node
/// auto-replies instead.
fn notify_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let msg_sig = RwSignal::new(str_field(&action, "message"));
    let chan_sig = RwSignal::new(str_field(&action, "channel"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = notify_params(&chan_sig.get_untracked(), &msg_sig.get_untracked());
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Channel (optional, default \"default\")"</label>
        <input
            class="flow-cfg-input"
            placeholder="ops"
            prop:value=move || chan_sig.get()
            on:input=move |ev| {
                chan_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Message"</label>
        <textarea
            class="flow-cfg-area"
            placeholder="message to deliver…"
            prop:value=move || msg_sig.get()
            on:input=move |ev| {
                msg_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <div class="flow-cfg-hint">"Delivers to a configured channel ([channels])."</div>
    }
}

/// Typed config for a **Create chat thread** output action. Its message commonly
/// templates the final text produced by an upstream Agent or Summarize node.
fn chat_thread_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let title_sig = RwSignal::new(str_field(&action, "title"));
    let message_sig = RwSignal::new(str_field(&action, "message"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let value = chat_thread_params(&title_sig.get_untracked(), &message_sig.get_untracked());
        set_node_action(graph, &id_sv.get_value(), value);
    };
    view! {
        <label class="flow-cfg-label">"Thread title (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="Automation output"
            prop:value=move || title_sig.get()
            on:input=move |ev| {
                title_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Message"</label>
        <textarea
            class="flow-cfg-area"
            placeholder="{{ inputs.agent.text }}"
            prop:value=move || message_sig.get()
            on:input=move |ev| {
                message_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <div class="flow-cfg-hint">
            "Creates a new thread in Chat with this assistant message. You can reference upstream output with {{ inputs.node.field }}."
        </div>
    }
}

/// Typed config for a **Start chat** action: a prompt the chat assistant
/// answers in a new thread, plus optional title/model/profile and whether the
/// node waits for the reply.
fn start_chat_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let prompt_sig = RwSignal::new(str_field(&action, "prompt"));
    let title_sig = RwSignal::new(str_field(&action, "title"));
    let model_sig = RwSignal::new(str_field(&action, "model"));
    let profile_sig = RwSignal::new(str_field(&action, "profile"));
    let wait_sig = RwSignal::new(action.get("wait").and_then(Value::as_bool).unwrap_or(true));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let value = start_chat_params(&StartChatForm {
            prompt: &prompt_sig.get_untracked(),
            title: &title_sig.get_untracked(),
            model: &model_sig.get_untracked(),
            profile: &profile_sig.get_untracked(),
            wait: wait_sig.get_untracked(),
        });
        set_node_action(graph, &id_sv.get_value(), value);
    };
    view! {
        <label class="flow-cfg-label">"Prompt"</label>
        <textarea
            class="flow-cfg-area"
            placeholder="Summarize {{ inputs.fetch.content }}"
            prop:value=move || prompt_sig.get()
            on:input=move |ev| {
                prompt_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <label class="flow-cfg-label">"Thread title (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="Generated from the conversation"
            prop:value=move || title_sig.get()
            on:input=move |ev| {
                title_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Model (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="Your chat model"
            prop:value=move || model_sig.get()
            on:input=move |ev| {
                model_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Agent profile (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="profile name"
            prop:value=move || profile_sig.get()
            on:input=move |ev| {
                profile_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-tool">
            <input
                type="checkbox"
                prop:checked=move || wait_sig.get()
                on:change=move |_| {
                    wait_sig.update(|b| *b = !*b);
                    apply();
                }
            />
            "Wait for the reply"
        </label>
        <div class="flow-cfg-hint">
            "Opens a new Chat thread with this prompt as your message and lets the assistant answer it. With waiting on, later nodes can use {{ inputs.node.reply }}."
        </div>
    }
}

/// Typed config for a **Create note** action: title, markdown body, and tags.
fn note_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let title_sig = RwSignal::new(str_field(&action, "title"));
    let md_sig = RwSignal::new(str_field(&action, "markdown"));
    let tags_sig = RwSignal::new(
        action
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
    );
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = note_params(
            &title_sig.get_untracked(),
            &md_sig.get_untracked(),
            &tags_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Title"</label>
        <input
            class="flow-cfg-input"
            placeholder="note title"
            prop:value=move || title_sig.get()
            on:input=move |ev| {
                title_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Markdown (optional)"</label>
        <textarea
            class="flow-cfg-area"
            prop:value=move || md_sig.get()
            on:input=move |ev| {
                md_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <label class="flow-cfg-label">"Tags (comma-separated, optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="home, urgent"
            prop:value=move || tags_sig.get()
            on:input=move |ev| {
                tags_sig.set(event_target_value(&ev));
                apply();
            }
        />
    }
}

/// Typed config for a **Create task** action: a board id (required by the
/// backend), an optional column id (defaults to the board's first column), a
/// title, and an optional markdown body. Board/column are ids — the Boards panel
/// surfaces them — kept as free text here so a graph can target a known board
/// without dropping to raw JSON, mirroring how Notify takes a channel name.
fn task_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let board_sig = RwSignal::new(str_field(&action, "board_id"));
    let col_sig = RwSignal::new(str_field(&action, "column_id"));
    let title_sig = RwSignal::new(str_field(&action, "title"));
    let body_sig = RwSignal::new(str_field(&action, "body"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = task_params(
            &board_sig.get_untracked(),
            &col_sig.get_untracked(),
            &title_sig.get_untracked(),
            &body_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Board id"</label>
        <input
            class="flow-cfg-input"
            placeholder="board uuid"
            prop:value=move || board_sig.get()
            on:input=move |ev| {
                board_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Column id (optional, defaults to first)"</label>
        <input
            class="flow-cfg-input"
            placeholder="column uuid"
            prop:value=move || col_sig.get()
            on:input=move |ev| {
                col_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Title"</label>
        <input
            class="flow-cfg-input"
            placeholder="task title"
            prop:value=move || title_sig.get()
            on:input=move |ev| {
                title_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Body (markdown, optional)"</label>
        <textarea
            class="flow-cfg-area"
            prop:value=move || body_sig.get()
            on:input=move |ev| {
                body_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <div class="flow-cfg-hint">
            "Board/column are ids (see the Boards panel); board_id + title are required."
        </div>
    }
}

/// Typed config for a **Create event** action: a summary, RFC-3339 start/end
/// times, an all-day toggle, and optional location / body / calendar id. Times
/// are free text (the backend validates RFC 3339 + `end >= start`), kept verbatim
/// so a fixed timestamp round-trips losslessly — no ambiguous local→UTC coercion.
fn event_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let summary_sig = RwSignal::new(str_field(&action, "summary"));
    let start_sig = RwSignal::new(str_field(&action, "start"));
    let end_sig = RwSignal::new(str_field(&action, "end"));
    let loc_sig = RwSignal::new(str_field(&action, "location"));
    let body_sig = RwSignal::new(str_field(&action, "body"));
    let cal_sig = RwSignal::new(str_field(&action, "calendar_id"));
    let allday_sig = RwSignal::new(
        action
            .get("all_day")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = event_params(
            &summary_sig.get_untracked(),
            &start_sig.get_untracked(),
            &end_sig.get_untracked(),
            &loc_sig.get_untracked(),
            &body_sig.get_untracked(),
            &cal_sig.get_untracked(),
            allday_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Summary"</label>
        <input
            class="flow-cfg-input"
            placeholder="event title"
            prop:value=move || summary_sig.get()
            on:input=move |ev| {
                summary_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Start (RFC 3339)"</label>
        <input
            class="flow-cfg-input"
            placeholder="2026-06-18T09:00:00Z"
            prop:value=move || start_sig.get()
            on:input=move |ev| {
                start_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"End (RFC 3339)"</label>
        <input
            class="flow-cfg-input"
            placeholder="2026-06-18T10:00:00Z"
            prop:value=move || end_sig.get()
            on:input=move |ev| {
                end_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-tool">
            <input
                type="checkbox"
                prop:checked=move || allday_sig.get()
                on:change=move |_| {
                    allday_sig.update(|b| *b = !*b);
                    apply();
                }
            />
            "All-day event"
        </label>
        <label class="flow-cfg-label">"Location (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="Room 1 / a URL"
            prop:value=move || loc_sig.get()
            on:input=move |ev| {
                loc_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Description (optional)"</label>
        <textarea
            class="flow-cfg-area"
            prop:value=move || body_sig.get()
            on:input=move |ev| {
                body_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <label class="flow-cfg-label">"Calendar id (optional, defaults to local)"</label>
        <input
            class="flow-cfg-input"
            placeholder="calendar uuid"
            prop:value=move || cal_sig.get()
            on:input=move |ev| {
                cal_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <div class="flow-cfg-hint">
            "Times are RFC 3339 (e.g. 2026-06-18T09:00:00Z); end must not precede start."
        </div>
    }
}

/// Read a string action field, or `""`.
pub(super) fn str_field(action: &Value, key: &str) -> String {
    action
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Insert `{key: trimmed-value}` into `obj` only when `value` is non-blank.
fn insert_if_set(obj: &mut Map<String, Value>, key: &str, value: &str) {
    let v = value.trim();
    if !v.is_empty() {
        obj.insert(key.to_string(), Value::String(v.to_string()));
    }
}

/// Build a `notify` action from the typed form (blank fields omitted). Pure +
/// testable.
pub(super) fn notify_params(channel: &str, message: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("notify".to_string()));
    insert_if_set(&mut obj, "message", message);
    insert_if_set(&mut obj, "channel", channel);
    Value::Object(obj)
}

/// Build a `create_chat_thread` action from the typed form. A blank title is
/// omitted so the backend supplies "Automation output"; `message` is required at
/// execution time.
pub(super) fn chat_thread_params(title: &str, message: &str) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "kind".to_string(),
        Value::String("create_chat_thread".to_string()),
    );
    insert_if_set(&mut obj, "title", title);
    insert_if_set(&mut obj, "message", message);
    Value::Object(obj)
}

/// The typed `start_chat` form fields.
pub(super) struct StartChatForm<'a> {
    pub(super) prompt: &'a str,
    pub(super) title: &'a str,
    pub(super) model: &'a str,
    pub(super) profile: &'a str,
    pub(super) wait: bool,
}

/// Build a `start_chat` action from the typed form. Blank optional fields are
/// omitted, and `wait` is emitted only when off (the backend defaults it on).
pub(super) fn start_chat_params(form: &StartChatForm<'_>) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("start_chat".to_string()));
    insert_if_set(&mut obj, "prompt", form.prompt);
    insert_if_set(&mut obj, "title", form.title);
    insert_if_set(&mut obj, "model", form.model);
    insert_if_set(&mut obj, "profile", form.profile);
    if !form.wait {
        obj.insert("wait".to_string(), Value::Bool(false));
    }
    Value::Object(obj)
}

/// Build a `create_note` action from the typed form (blank fields omitted; tags
/// split on commas). Pure + testable.
pub(super) fn note_params(title: &str, markdown: &str, tags_csv: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("create_note".to_string()));
    insert_if_set(&mut obj, "title", title);
    insert_if_set(&mut obj, "markdown", markdown);
    let tags: Vec<Value> = tags_csv
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Value::String(s.to_string()))
        .collect();
    if !tags.is_empty() {
        obj.insert("tags".to_string(), Value::Array(tags));
    }
    Value::Object(obj)
}

/// Build a `create_task` action from the typed form (blank fields omitted). The
/// backend `create_task` tool requires `board_id` + `title`; `column_id` defaults
/// to the board's first column and `body` is optional markdown. Pure + testable.
pub(super) fn task_params(board_id: &str, column_id: &str, title: &str, body: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("create_task".to_string()));
    insert_if_set(&mut obj, "board_id", board_id);
    insert_if_set(&mut obj, "column_id", column_id);
    insert_if_set(&mut obj, "title", title);
    insert_if_set(&mut obj, "body", body);
    Value::Object(obj)
}

/// Build a `create_event` action from the typed form (blank fields omitted;
/// `all_day` emitted only when set, since the backend defaults it to false). The
/// backend `create_event` tool requires `summary` + RFC-3339 `start`/`end`;
/// `location`, `body`, and `calendar_id` are optional. Start/end stay free text in
/// the form (the tool itself documents RFC 3339, e.g. `2026-06-18T09:00:00Z`, and
/// validates the format + `end >= start` at run time), so the builder is a pure,
/// lossless pass-through — no ambiguous datetime-local→UTC conversion. Testable.
pub(super) fn event_params(
    summary: &str,
    start: &str,
    end: &str,
    location: &str,
    body: &str,
    calendar_id: &str,
    all_day: bool,
) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "kind".to_string(),
        Value::String("create_event".to_string()),
    );
    insert_if_set(&mut obj, "summary", summary);
    insert_if_set(&mut obj, "start", start);
    insert_if_set(&mut obj, "end", end);
    insert_if_set(&mut obj, "location", location);
    insert_if_set(&mut obj, "body", body);
    insert_if_set(&mut obj, "calendar_id", calendar_id);
    if all_day {
        obj.insert("all_day".to_string(), Value::Bool(true));
    }
    Value::Object(obj)
}

/// Build a `summarize` action from the typed form (blank fields omitted;
/// `max_words` only when it parses as a positive integer). Pure + testable.
pub(super) fn summarize_params(input: &str, instructions: &str, max_words: &str, model: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("summarize".to_string()));
    insert_if_set(&mut obj, "input", input);
    insert_if_set(&mut obj, "instructions", instructions);
    if let Ok(words) = max_words.trim().parse::<u64>() {
        if words > 0 {
            obj.insert("max_words".to_string(), Value::from(words));
        }
    }
    insert_if_set(&mut obj, "model", model);
    Value::Object(obj)
}

/// Typed config for a **Summarize** action: the input to condense (usually a
/// `{{ path }}` template over an upstream node's output; unset = the firing
/// trigger event), optional steering instructions, a word bound, and a model
/// override. One LLM call, no tools — for tool-calling steps use an Agent node.
fn summarize_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let input_sig = RwSignal::new(str_field(&action, "input"));
    let instr_sig = RwSignal::new(str_field(&action, "instructions"));
    let words_sig = RwSignal::new(
        action
            .get("max_words")
            .and_then(Value::as_u64)
            .map(|w| w.to_string())
            .unwrap_or_default(),
    );
    let model_sig = RwSignal::new(str_field(&action, "model"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = summarize_params(
            &input_sig.get_untracked(),
            &instr_sig.get_untracked(),
            &words_sig.get_untracked(),
            &model_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Input (optional — empty summarizes the trigger event)"</label>
        <textarea
            class="flow-cfg-area"
            placeholder="{{ inputs.fetch.content }}"
            prop:value=move || input_sig.get()
            on:input=move |ev| {
                input_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <label class="flow-cfg-label">"Instructions (optional)"</label>
        <textarea
            class="flow-cfg-area"
            placeholder="Focus on action items; bullet list."
            prop:value=move || instr_sig.get()
            on:input=move |ev| {
                instr_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <label class="flow-cfg-label">"Max words (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="120"
            inputmode="numeric"
            prop:value=move || words_sig.get()
            on:input=move |ev| {
                words_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Model (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="default model"
            prop:value=move || model_sig.get()
            on:input=move |ev| {
                model_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <div class="flow-cfg-hint">
            "One LLM call, no tools; downstream nodes read {{ inputs.<node-id>.summary }}."
        </div>
    }
}

/// Build a `write_object` action from the typed form (blank fields omitted).
/// Pure + testable. The form covers the text case; binary `content_base64` is
/// authoring-tool territory.
pub(super) fn write_object_params(key: &str, content: &str, store: &str, content_type: &str) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "kind".to_string(),
        Value::String("write_object".to_string()),
    );
    insert_if_set(&mut obj, "key", key);
    // Content is NOT trimmed like the id-ish fields: written bytes are verbatim
    // (leading/trailing whitespace can be intentional), only fully-empty is omitted.
    if !content.is_empty() {
        obj.insert("content".to_string(), Value::String(content.to_string()));
    }
    insert_if_set(&mut obj, "store", store);
    insert_if_set(&mut obj, "content_type", content_type);
    Value::Object(obj)
}

/// Typed config for a **Write object** action: create/overwrite a stored file
/// from content in hand — commonly a template over an upstream node's output.
fn write_object_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let key_sig = RwSignal::new(str_field(&action, "key"));
    let content_sig = RwSignal::new(str_field(&action, "content"));
    let store_sig = RwSignal::new(str_field(&action, "store"));
    let ctype_sig = RwSignal::new(str_field(&action, "content_type"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = write_object_params(
            &key_sig.get_untracked(),
            &content_sig.get_untracked(),
            &store_sig.get_untracked(),
            &ctype_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Key"</label>
        <input
            class="flow-cfg-input"
            placeholder="reports/2026/summary.md"
            prop:value=move || key_sig.get()
            on:input=move |ev| {
                key_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Content"</label>
        <textarea
            class="flow-cfg-area flow-cfg-area-tall"
            placeholder="{{ inputs.agent.content }}"
            prop:value=move || content_sig.get()
            on:input=move |ev| {
                content_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <label class="flow-cfg-label">"Store (optional, default files store)"</label>
        <input
            class="flow-cfg-input"
            placeholder="default"
            prop:value=move || store_sig.get()
            on:input=move |ev| {
                store_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Content type (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="text/markdown"
            prop:value=move || ctype_sig.get()
            on:input=move |ev| {
                ctype_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <div class="flow-cfg-hint">
            "Creates or overwrites the file; it is catalogued, searchable, and can fire storage triggers."
        </div>
    }
}

/// Build a `move_object` action from the typed form (blank fields omitted).
/// Pure + testable.
pub(super) fn move_object_params(from_key: &str, to_key: &str, from_store: &str, to_store: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("move_object".to_string()));
    insert_if_set(&mut obj, "from_key", from_key);
    insert_if_set(&mut obj, "to_key", to_key);
    insert_if_set(&mut obj, "from_store", from_store);
    insert_if_set(&mut obj, "to_store", to_store);
    Value::Object(obj)
}

/// Typed config for a **Move object** action: relocate a stored file (copy to
/// the destination, delete the source), within a store or across stores.
fn move_object_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let from_key_sig = RwSignal::new(str_field(&action, "from_key"));
    let to_key_sig = RwSignal::new(str_field(&action, "to_key"));
    let from_store_sig = RwSignal::new(str_field(&action, "from_store"));
    let to_store_sig = RwSignal::new(str_field(&action, "to_store"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = move_object_params(
            &from_key_sig.get_untracked(),
            &to_key_sig.get_untracked(),
            &from_store_sig.get_untracked(),
            &to_store_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"From key"</label>
        <input
            class="flow-cfg-input"
            placeholder="{{ trigger.key }}"
            prop:value=move || from_key_sig.get()
            on:input=move |ev| {
                from_key_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"To key (optional, defaults to from key)"</label>
        <input
            class="flow-cfg-input"
            placeholder="archive/{{ trigger.key }}"
            prop:value=move || to_key_sig.get()
            on:input=move |ev| {
                to_key_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"From store (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="default"
            prop:value=move || from_store_sig.get()
            on:input=move |ev| {
                from_store_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"To store (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="archive"
            prop:value=move || to_store_sig.get()
            on:input=move |ev| {
                to_store_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <div class="flow-cfg-hint">
            "Copies to the destination, then deletes the source; to keep the source use copy_object from a Code node."
        </div>
    }
}

/// Build a `webhook` action from the typed form. `payload`/`headers` arrive as
/// **already-parsed** JSON objects (the form validates before applying); blank
/// url/method are omitted (method defaults to post server-side). Pure + testable.
pub(super) fn webhook_params(
    url: &str,
    method: &str,
    payload: Option<Map<String, Value>>,
    headers: Option<Map<String, Value>>,
) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("webhook".to_string()));
    insert_if_set(&mut obj, "url", url);
    // "post" is the server default — omit it so the stored node stays minimal.
    if method != "post" {
        insert_if_set(&mut obj, "method", method);
    }
    if let Some(p) = payload.filter(|p| !p.is_empty()) {
        obj.insert("payload".to_string(), Value::Object(p));
    }
    if let Some(h) = headers.filter(|h| !h.is_empty()) {
        obj.insert("headers".to_string(), Value::Object(h));
    }
    Value::Object(obj)
}

/// Typed config for a **Webhook** action: deliver a JSON payload to an external
/// URL. Payload/headers are JSON-object textareas validated on input (an invalid
/// edit shows the error and leaves the node's last good state in place, like the
/// raw params editor).
fn webhook_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let url_sig = RwSignal::new(str_field(&action, "url"));
    let method_sig = RwSignal::new({
        let m = str_field(&action, "method");
        if m.is_empty() {
            "post".to_string()
        } else {
            m
        }
    });
    let json_obj_text = |key: &str| {
        action
            .get(key)
            .and_then(Value::as_object)
            .filter(|m| !m.is_empty())
            .map(|m| serde_json::to_string_pretty(&Value::Object(m.clone())).unwrap_or_default())
            .unwrap_or_default()
    };
    let payload_sig = RwSignal::new(json_obj_text("payload"));
    let headers_sig = RwSignal::new(json_obj_text("headers"));
    let payload_err = RwSignal::new(Option::<String>::None);
    let headers_err = RwSignal::new(Option::<String>::None);
    let id_sv = StoredValue::new(id);
    // Apply only when BOTH JSON fields parse; an invalid edit surfaces its error
    // and leaves the node's last good params untouched.
    let apply = move || {
        let payload = match parse_params(&payload_sig.get_untracked()) {
            Ok(p) => {
                payload_err.set(None);
                p
            }
            Err(e) => {
                payload_err.set(Some(e));
                return;
            }
        };
        let headers = match parse_params(&headers_sig.get_untracked()) {
            Ok(h) => {
                headers_err.set(None);
                h
            }
            Err(e) => {
                headers_err.set(Some(e));
                return;
            }
        };
        let v = webhook_params(
            &url_sig.get_untracked(),
            &method_sig.get_untracked(),
            Some(payload),
            Some(headers),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"URL"</label>
        <input
            class="flow-cfg-input"
            placeholder="https://hooks.example.com/notify"
            prop:value=move || url_sig.get()
            on:input=move |ev| {
                url_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Method"</label>
        <select
            class="flow-cfg-input"
            prop:value=move || method_sig.get()
            on:change=move |ev| {
                method_sig.set(event_target_value(&ev));
                apply();
            }
        >
            <option value="post">"POST"</option>
            <option value="put">"PUT"</option>
            <option value="patch">"PATCH"</option>
        </select>
        <label class="flow-cfg-label">"Payload (JSON object, optional)"</label>
        <textarea
            class="flow-cfg-area"
            placeholder=r#"{"summary":"{{ inputs.summarize.summary }}"}"#
            prop:value=move || payload_sig.get()
            on:input=move |ev| {
                payload_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <Show when=move || payload_err.with(Option::is_some) fallback=|| ().into_view()>
            <div class="flow-cfg-err">{move || payload_err.get().unwrap_or_default()}</div>
        </Show>
        <label class="flow-cfg-label">"Headers (JSON object, optional)"</label>
        <textarea
            class="flow-cfg-area"
            placeholder=r#"{"Authorization":"Bearer …"}"#
            prop:value=move || headers_sig.get()
            on:input=move |ev| {
                headers_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <Show when=move || headers_err.with(Option::is_some) fallback=|| ().into_view()>
            <div class="flow-cfg-err">{move || headers_err.get().unwrap_or_default()}</div>
        </Show>
        <div class="flow-cfg-hint">
            "Sends the payload as application/json; a non-2xx response fails the step. Values may be {{ path }} templates."
        </div>
    }
}

/// The raw-JSON params editor for a non-agent action: a `{..params}` textarea
/// merged with the node's current `kind`.
fn params_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let kind = action
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("summarize")
        .to_string();
    let params = {
        let mut m = action.as_object().cloned().unwrap_or_default();
        m.remove("kind");
        if m.is_empty() {
            String::new()
        } else {
            serde_json::to_string_pretty(&Value::Object(m)).unwrap_or_default()
        }
    };
    let params_sig = RwSignal::new(params);
    let parse_err = RwSignal::new(Option::<String>::None);
    let id_for_params = id.clone();

    view! {
        <label class="flow-cfg-label">"Params (JSON object, optional)"</label>
        <textarea
            class="flow-cfg-area"
            placeholder=r#"{"channel":"ops","message":"hi"}"#
            prop:value=move || params_sig.get()
            on:input=move |ev| {
                params_sig.set(event_target_value(&ev));
                match parse_params(&params_sig.get_untracked()) {
                    Ok(mut obj) => {
                        obj.insert("kind".to_string(), Value::String(kind.clone()));
                        parse_err.set(None);
                        set_node_action(graph, &id_for_params, Value::Object(obj));
                    }
                    Err(e) => parse_err.set(Some(e)),
                }
            }
        ></textarea>
        <Show when=move || parse_err.with(Option::is_some) fallback=|| ().into_view()>
            <div class="flow-cfg-err">{move || parse_err.get().unwrap_or_default()}</div>
        </Show>
    }
}
