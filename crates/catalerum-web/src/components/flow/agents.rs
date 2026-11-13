use super::*;

/// Typed config for an **Agent** (`llm_agent`) node — a tool-calling LLM agent
/// (e.g. a channel chatbot). A system prompt, an optional model, a grouped tool
/// picker, and skills, all written into the action's `llm_agent` params. Uses the
/// **normal agent infrastructure**: empty `tools` = the agent may use any tool;
/// include `notify` so it can reply on a channel; `recall`/`remember` give it
/// memory.
pub(super) fn agent_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let (system, model, tools, skills, output, reasoning) =
        agent_form_from_params(&node_action(graph, &id));
    let system_sig = RwSignal::new(system);
    let model_sig = RwSignal::new(model);
    let tools_sig = RwSignal::new(tools);
    let skills_sig = RwSignal::new(skills);
    let output_sig = RwSignal::new(output);
    let reasoning_sig = RwSignal::new(reasoning);
    let id_sv = StoredValue::new(id);

    // The gateway's chat models feed the model autocomplete (best-effort — an empty
    // catalog just leaves the field as free text accepting a typed id).
    let flow_models = RwSignal::new(Vec::<ModelInfo>::new());
    spawn_local(async move {
        let token = auth::resolve_token();
        if let Ok(list) = rest::list_llm_models(token.as_deref(), "llm").await {
            flow_models.set(list);
        }
    });

    // Push the current form state into the node's `llm_agent` action.
    let apply = move || {
        let v = agent_params_from_form(
            &system_sig.get_untracked(),
            &model_sig.get_untracked(),
            &tools_sig.get_untracked(),
            &skills_sig.get_untracked(),
            &output_sig.get_untracked(),
            &reasoning_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };

    view! {
        <label class="flow-cfg-label">"System prompt"</label>
        <textarea
            class="flow-cfg-area flow-cfg-area-tall"
            placeholder="You are a helpful assistant in a chat channel…"
            prop:value=move || system_sig.get()
            on:input=move |ev| {
                system_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>

        <label class="flow-cfg-label">"Output format"</label>
        <select
            class="flow-cfg-input"
            on:change=move |ev| {
                output_sig.set(event_target_value(&ev));
                apply();
            }
            prop:value=move || output_sig.get()
        >
            <option value="text">"Text"</option>
            <option value="json">"JSON (structured — usable by downstream nodes)"</option>
        </select>

        <label class="flow-cfg-label">"Model (optional)"</label>
        {model_autocomplete(
            Signal::derive(move || model_sig.get()),
            move |v| {
                model_sig.set(v);
                apply();
            },
            model_options(flow_models, false),
            Signal::derive(|| "gateway default".to_string()),
            Signal::derive(|| false),
            "flow-cfg-input",
        )}

        <label class="flow-cfg-label">"Thinking (reasoning effort)"</label>
        <select
            class="flow-cfg-input"
            on:change=move |ev| {
                reasoning_sig.set(event_target_value(&ev));
                apply();
            }
            prop:value=move || reasoning_sig.get()
        >
            <option value="">"Off (model default)"</option>
            <option value="low">"Low"</option>
            <option value="medium">"Medium"</option>
            <option value="high">"High"</option>
            <option value="xhigh">"Extra high"</option>
            <option value="max">"Max"</option>
        </select>

        <label class="flow-cfg-label">"Tools the agent may call"</label>
        <div class="flow-tools">
            {KNOWN_TOOLS
                .iter()
                .map(|(group, names)| {
                    let checks = names
                        .iter()
                        .map(|name| {
                            let name = *name;
                            view! {
                                <label class="flow-tool">
                                    <input
                                        type="checkbox"
                                        prop:checked=move || {
                                            tools_sig.with(|sel| sel.iter().any(|x| x.as_str() == name))
                                        }
                                        on:change=move |_| {
                                            tools_sig
                                                .update(|sel| {
                                                    if let Some(i)
                                                        = sel.iter().position(|x| x.as_str() == name)
                                                    {
                                                        sel.remove(i);
                                                    } else {
                                                        sel.push(name.to_string());
                                                    }
                                                });
                                            apply();
                                        }
                                    />
                                    {name}
                                </label>
                            }
                        })
                        .collect::<Vec<_>>();
                    view! {
                        <div class="flow-tool-group">
                            <span class="flow-tool-grp">{*group}</span>
                            {checks}
                        </div>
                    }
                })
                .collect::<Vec<_>>()}
        </div>
        <div class="flow-cfg-hint">
            "No tools ticked = the agent may use any available tool. On a channel-message trigger its reply is sent back to that channel automatically; `recall`/`remember` give it memory. Add `notify` only to also post to other channels."
        </div>

        <label class="flow-cfg-label">"Skills (comma-separated, optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="triage-inbox, …"
            prop:value=move || skills_sig.get()
            on:input=move |ev| {
                skills_sig.set(event_target_value(&ev));
                apply();
            }
        />
    }
}

/// The selected node's current action payload (`Value::Null` if it isn't an action
/// node). Read untracked — the config editors seed their form state from it once.
pub(super) fn node_action(graph: RwSignal<FlowGraph>, id: &str) -> Value {
    graph
        .with_untracked(|g| {
            g.nodes
                .iter()
                .find(|n| n.id == id)
                .and_then(|n| match &n.kind {
                    FlowKind::Action { action } => Some(action.clone()),
                    _ => None,
                })
        })
        .unwrap_or(Value::Null)
}

/// Read an `llm_agent` action into the agent form's fields: `(system, model,
/// tools, skills_csv, output, reasoning)`. Missing fields default to empty; `output`
/// defaults to `"text"` and `reasoning` to `""` (off). Pure + testable.
pub(super) fn agent_form_from_params(action: &Value) -> (String, String, Vec<String>, String, String, String) {
    let str_field = |k: &str| {
        action
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let arr_field = |k: &str| {
        action
            .get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let output = match action.get("output").and_then(Value::as_str) {
        Some("json") => "json",
        _ => "text",
    }
    .to_string();
    (
        str_field("system"),
        str_field("model"),
        arr_field("tools"),
        arr_field("skills").join(", "),
        output,
        str_field("reasoning_effort"),
    )
}

/// Build an `llm_agent` action from the agent form fields, omitting blank/empty
/// ones (an empty `tools` array means "any tool"; `output` is set only for `"json"`,
/// since absent means plain text). Pure + testable.
pub(super) fn agent_params_from_form(
    system: &str,
    model: &str,
    tools: &[String],
    skills_csv: &str,
    output: &str,
    reasoning: &str,
) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("llm_agent".to_string()));
    if !system.trim().is_empty() {
        obj.insert(
            "system".to_string(),
            Value::String(system.trim().to_string()),
        );
    }
    if !model.trim().is_empty() {
        obj.insert("model".to_string(), Value::String(model.trim().to_string()));
    }
    if !reasoning.trim().is_empty() {
        obj.insert(
            "reasoning_effort".to_string(),
            Value::String(reasoning.trim().to_string()),
        );
    }
    if !tools.is_empty() {
        obj.insert(
            "tools".to_string(),
            Value::Array(tools.iter().map(|t| Value::String(t.clone())).collect()),
        );
    }
    let skills: Vec<Value> = skills_csv
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Value::String(s.to_string()))
        .collect();
    if !skills.is_empty() {
        obj.insert("skills".to_string(), Value::Array(skills));
    }
    if output == "json" {
        obj.insert("output".to_string(), Value::String("json".to_string()));
    }
    Value::Object(obj)
}

/// Split a newline-separated outcomes textarea into trimmed, non-empty outcome
/// names (the order is preserved). Pure + testable.
pub(super) fn parse_outcomes(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The system prompt a [`classifier_action`] carries: it instructs the model to
/// score each named outcome from 0.0–1.0 and return them as a JSON object. The
/// `instructions` (optional) describe *what* to classify.
fn classifier_system_prompt(outcomes: &[String], instructions: &str) -> String {
    let mut s = String::from(
        "You are a classifier. Read the input and judge how strongly it matches each of the outcomes below.",
    );
    if !instructions.trim().is_empty() {
        s.push_str("\n\nClassification task: ");
        s.push_str(instructions.trim());
    }
    s.push_str("\n\nOutcomes:\n");
    if outcomes.is_empty() {
        s.push_str("- (none defined yet)\n");
    } else {
        for o in outcomes {
            s.push_str("- ");
            s.push_str(o);
            s.push('\n');
        }
    }
    s.push_str(
        "\nReturn a single JSON object whose keys are EXACTLY the outcome names listed above and whose values are probabilities from 0.0 to 1.0. The scores are independent — they need not sum to 1. Output ONLY the JSON object, with no prose, explanation, or markdown fences.",
    );
    s
}

/// Build a **Classifier** action: an `llm_agent` steered to JSON output whose
/// system prompt scores each `outcome` from 0.0–1.0. The `outcomes` list (and the
/// raw `instructions`) are kept as web-only metadata so the node round-trips for
/// editing; the backend reads only the generated `system` + `output: "json"`.
/// Pure + testable.
pub(super) fn classifier_action(outcomes: &[String], instructions: &str) -> Value {
    let clean: Vec<String> = outcomes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("llm_agent".to_string()));
    obj.insert(
        "system".to_string(),
        Value::String(classifier_system_prompt(&clean, instructions)),
    );
    obj.insert("output".to_string(), Value::String("json".to_string()));
    obj.insert(
        "outcomes".to_string(),
        Value::Array(clean.into_iter().map(Value::String).collect()),
    );
    if !instructions.trim().is_empty() {
        obj.insert(
            "instructions".to_string(),
            Value::String(instructions.trim().to_string()),
        );
    }
    Value::Object(obj)
}

/// Read a classifier action back into its editable fields: `(outcomes, instructions)`.
/// Pure + testable.
pub(super) fn classifier_form_from_params(action: &Value) -> (Vec<String>, String) {
    let outcomes = action
        .get("outcomes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let instructions = action
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (outcomes, instructions)
}

/// Update a classifier while keeping fields such as an explicit state or editor metadata.
pub(super) fn classifier_update(
    original: &Value,
    outcomes: &[String],
    instructions: &str,
    engine: &str,
    model: &str,
) -> Value {
    let mut action = original.as_object().cloned().unwrap_or_default();
    for key in [
        "system",
        "output",
        "questions",
        "model",
        "instructions",
        "tools",
        "skills",
        "reasoning_effort",
    ] {
        action.remove(key);
    }
    let legacy = classifier_action(outcomes, instructions);
    action.extend(legacy.as_object().unwrap().clone());
    if engine == "decisions" {
        action.insert("kind".into(), Value::String("llm_decision".into()));
        action.remove("system");
        action.remove("output");
        let clean = action["outcomes"].as_array().unwrap();
        let questions: Map<String, Value> = clean.iter().filter_map(Value::as_str).map(|outcome| {
            let task = if instructions.trim().is_empty() { "Classify the input." } else { instructions.trim() };
            (outcome.to_string(), serde_json::json!({
                "type": "noul",
                "instructions": format!("{task} Does the input match the outcome '{outcome}'?"),
                "criteria": { "true": format!("The input matches '{outcome}'."), "false": format!("The input does not match '{outcome}'.") }
            }))
        }).collect();
        action.insert("questions".into(), Value::Object(questions));
    }
    if !model.trim().is_empty() {
        action.insert("model".into(), Value::String(model.trim().to_string()));
    }
    Value::Object(action)
}

/// Classifiers can use chat JSON scoring or native yes/no decision probabilities.
pub(super) fn classifier_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let original = node_action(graph, &id);
    let (outcomes, instructions) = classifier_form_from_params(&original);
    let engine_sig = RwSignal::new(
        if original["kind"] == "llm_decision" {
            "decisions"
        } else {
            "llm"
        }
        .to_string(),
    );
    let model_sig = RwSignal::new(
        original
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    );
    let instr_sig = RwSignal::new(instructions);
    let outcomes_sig = RwSignal::new(outcomes.join("\n"));
    let id_sv = StoredValue::new(id);
    let chat_models = RwSignal::new(Vec::<ModelInfo>::new());
    let decision_models = RwSignal::new(Vec::<ModelInfo>::new());
    spawn_local(async move {
        let token = auth::resolve_token();
        if let Ok(models) = rest::list_llm_models(token.as_deref(), "llm").await {
            chat_models.set(models);
        }
        if let Ok(models) = rest::list_llm_models(token.as_deref(), "decisions").await {
            decision_models.set(models);
        }
    });
    let apply = move || {
        let original = node_action(graph, &id_sv.get_value());
        let outs = parse_outcomes(&outcomes_sig.get_untracked());
        let v = classifier_update(
            &original,
            &outs,
            &instr_sig.get_untracked(),
            &engine_sig.get_untracked(),
            &model_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    let chat_options = model_options(chat_models, false);
    let decision_options = model_options(decision_models, false);

    view! {
        <label class="flow-cfg-label">"Classifier engine"</label>
        <select class="flow-cfg-input" prop:value=move || engine_sig.get() on:change=move |ev| {
            engine_sig.set(event_target_value(&ev));
            model_sig.set(String::new());
            apply();
        }>
            <option value="llm">"Chat model (JSON scores)"</option>
            <option value="decisions">"Decision model (typed probabilities)"</option>
        </select>
        <label class="flow-cfg-label">"Model (optional)"</label>
        {model_autocomplete(
            Signal::derive(move || model_sig.get()),
            move |v| { model_sig.set(v); apply(); },
            Signal::derive(move || if engine_sig.get() == "decisions" { decision_options.get() } else { chat_options.get() }),
            Signal::derive(move || if engine_sig.get() == "decisions" { "user or server decision model".to_string() } else { "gateway default".to_string() }),
            Signal::derive(|| false),
            "flow-cfg-input",
        )}
        <label class="flow-cfg-label">"What to classify"</label>
        <textarea class="flow-cfg-area" placeholder="e.g. Judge the sentiment of the incoming message"
            prop:value=move || instr_sig.get() on:input=move |ev| { instr_sig.set(event_target_value(&ev)); apply(); }></textarea>
        <label class="flow-cfg-label">"Outcomes (one per line)"</label>
        <textarea class="flow-cfg-area flow-cfg-area-tall" placeholder="positive&#10;negative&#10;neutral"
            prop:value=move || outcomes_sig.get() on:input=move |ev| { outcomes_sig.set(event_target_value(&ev)); apply(); }></textarea>
        <div class="flow-cfg-hint">
            "Each outcome has an independent probability from 0.0 to 1.0. A downstream Condition can check `input.inputs[<id>].data.positive > 0.5`. Decision mode also returns the typed results in `answers`."
        </div>
    }
}

/// Write a native decision form while preserving other action metadata.
pub(super) fn decision_params(
    original: &Value,
    model: &str,
    questions: &str,
    state: &str,
) -> Result<Value, String> {
    let mut params = original.as_object().cloned().unwrap_or_default();
    params.insert("kind".into(), Value::String("llm_decision".into()));
    if model.trim().is_empty() {
        params.remove("model");
    } else {
        params.insert("model".into(), Value::String(model.trim().to_string()));
    }
    let questions: Value =
        serde_json::from_str(questions).map_err(|err| format!("Questions JSON: {err}"))?;
    if !questions
        .as_object()
        .is_some_and(|questions| !questions.is_empty())
    {
        return Err("Questions must be a non-empty JSON object.".into());
    }
    params.insert("questions".into(), questions);
    if state.trim().is_empty() {
        params.remove("state");
    } else {
        let state: Value =
            serde_json::from_str(state).map_err(|err| format!("State JSON: {err}"))?;
        if !matches!(state, Value::String(_) | Value::Object(_) | Value::Array(_)) {
            return Err("State must be a JSON string, object, or array.".into());
        }
        params.insert("state".into(), state);
    }
    Ok(Value::Object(params))
}

/// Native decisions accept named typed questions and structured input state.
pub(super) fn decision_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let model_sig = RwSignal::new(
        action
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    );
    let questions_sig = RwSignal::new(action.get("questions").map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
        .unwrap_or_else(|| "{\n  \"urgent\": {\n    \"type\": \"noul\",\n    \"instructions\": \"Does this need an immediate response?\"\n  }\n}".to_string()));
    let state_sig = RwSignal::new(
        action
            .get("state")
            .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
            .unwrap_or_default(),
    );
    let parse_err = RwSignal::new(Option::<String>::None);
    let models = RwSignal::new(Vec::<ModelInfo>::new());
    let id_sv = StoredValue::new(id);
    spawn_local(async move {
        let token = auth::resolve_token();
        if let Ok(list) = rest::list_llm_models(token.as_deref(), "decisions").await {
            models.set(list);
        }
    });
    let apply = move || match decision_params(
        &node_action(graph, &id_sv.get_value()),
        &model_sig.get_untracked(),
        &questions_sig.get_untracked(),
        &state_sig.get_untracked(),
    ) {
        Ok(params) => {
            parse_err.set(None);
            set_node_action(graph, &id_sv.get_value(), params);
        }
        Err(error) => parse_err.set(Some(error)),
    };
    view! {
        <label class="flow-cfg-label">"Decision model (optional)"</label>
        {model_autocomplete(Signal::derive(move || model_sig.get()), move |v| { model_sig.set(v); apply(); },
            model_options(models, false), Signal::derive(|| "user or server decision model".to_string()),
            Signal::derive(|| false), "flow-cfg-input")}
        <label class="flow-cfg-label">"Questions (JSON object)"</label>
        <div class="flow-cfg-hint">
            "Each named question needs `type` and `instructions`. Use `noul` for a yes/no probability, `choice` with a named criteria object, or `score` with an ordered criteria array. Results are in `answers` and scalar values in `data`."
        </div>
        <textarea class="flow-cfg-area flow-cfg-area-tall" prop:value=move || questions_sig.get()
            on:input=move |ev| { questions_sig.set(event_target_value(&ev)); apply(); }></textarea>
        <label class="flow-cfg-label">"State (JSON, optional)"</label>
        <div class="flow-cfg-hint">"Leave blank to evaluate the node's input, including upstream outputs. Explicit state can use templates such as {{ trigger.text }}."</div>
        <textarea class="flow-cfg-area" prop:value=move || state_sig.get()
            on:input=move |ev| { state_sig.set(event_target_value(&ev)); apply(); }></textarea>
        <Show when=move || parse_err.with(Option::is_some) fallback=|| ().into_view()>
            <div class="flow-cfg-err">{move || parse_err.get().unwrap_or_default()}</div>
        </Show>
    }
}

/// Code / condition node config: a runtime `<select>` (js/shell/python) + a `source`
/// textarea. A condition's source is evaluated for truthiness to route its branch
/// ports.
pub(super) fn code_config(
    graph: RwSignal<FlowGraph>,
    id: String,
    runtime: String,
    source: String,
    is_condition: bool,
) -> impl IntoView {
    let runtime_sig = RwSignal::new(runtime);
    let source_sig = RwSignal::new(source);

    let id_for_rt = id.clone();
    let id_for_src = id.clone();

    let hint = if is_condition {
        "Condition source — its result routes the true / false ports."
    } else {
        "Code source — a pure data transform over `input`."
    };

    view! {
        <label class="flow-cfg-label">"Runtime"</label>
        <select
            class="flow-cfg-input"
            on:change=move |ev| {
                runtime_sig.set(event_target_value(&ev));
                set_node_code(graph, &id_for_rt, runtime_sig.get_untracked(), source_sig.get_untracked(), is_condition);
            }
            prop:value=move || runtime_sig.get()
        >
            {RUNTIMES
                .iter()
                .map(|(k, label)| {
                    view! { <option value=*k>{*label}</option> }
                })
                .collect::<Vec<_>>()}
        </select>
        <label class="flow-cfg-label">"Source"</label>
        <textarea
            class="flow-cfg-area flow-cfg-area-tall"
            prop:value=move || source_sig.get()
            on:input=move |ev| {
                source_sig.set(event_target_value(&ev));
                set_node_code(graph, &id_for_src, runtime_sig.get_untracked(), source_sig.get_untracked(), is_condition);
            }
        ></textarea>
        <div class="flow-cfg-hint">{hint}</div>
    }
}
