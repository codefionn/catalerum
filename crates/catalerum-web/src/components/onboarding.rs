//! The quick-start / onboarding wizard panel (SOUL §12/§22/§23).
//!
//! A five-step first-run flow that personalizes the workspace:
//!
//! 1. **Connections** — a live `GET /status` health read (Postgres / LLM gateway
//!    / bus / optional stores), so the user knows the assistant can actually run.
//! 2. **Models** — pick a chat + speech model/voice over the `[llm]` config
//!    defaults (shown as placeholders); persisted via `PUT /llm-settings`.
//! 3. **About you** — a small profile form (name, timezone, working hours, role,
//!    communication style) merged into `PUT /profile`, plus an optional free-text
//!    fact stored as a `user`-scoped memory.
//! 4. **Personalize** — an assistant-led chat (`POST /onboarding/personalize`): the
//!    assistant asks the user questions and, as it learns, proposes durable
//!    **memories** and tailored **skills**. The user reviews the proposals and the
//!    chosen ones are written with `POST /memories` and `PUT /skills/{name}`.
//! 5. **Done** — stamp completion (`POST /onboarding/complete`) and jump to Chat.
//!
//! The shell auto-opens this on first run (when `GET /onboarding/state` reports
//! `completed == false`); it is also reachable any time from the "Quick start"
//! nav entry. Every captured datum lands in an existing, editable surface
//! (profile / memories / skills) — no hidden state (SOUL §16).

use std::collections::HashSet;

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api::{
    LlmSettings, PersonalizeRequest, PersonalizeTurn, SkillDraft, StatusInfo, UpdateSkill,
};
use crate::auth;
use crate::components::shell::Panel;
use crate::components::theme::ThemePicker;
use crate::components::widgets::{model_autocomplete, model_options, voice_options};
use crate::rest;

/// The wizard's linear steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Connections,
    Models,
    Profile,
    Personalize,
    Done,
}

/// Steps in order, with their nav labels (for the progress strip).
const STEPS: [(Step, &str); 5] = [
    (Step::Connections, "Connect"),
    (Step::Models, "Models"),
    (Step::Profile, "About you"),
    (Step::Personalize, "Personalize"),
    (Step::Done, "Done"),
];

impl Step {
    /// Zero-based position in [`STEPS`], for the progress strip + "done" styling.
    fn index(self) -> usize {
        STEPS.iter().position(|(s, _)| *s == self).unwrap_or(0)
    }
}

/// Collapse a blank/whitespace string to `None` so a cleared field falls back to
/// the gateway default (matching the `PUT /llm-settings` full-replace contract).
fn blank_to_none(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Case/whitespace-insensitive equality, used to dedup proposed memory facts as the
/// personalization chat re-surfaces them across turns.
fn eq_norm(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// The onboarding wizard. `active` is the shell's selected-panel signal, so the
/// final step can jump the user straight into Chat.
#[component]
pub fn OnboardingPanel(active: RwSignal<Panel>) -> impl IntoView {
    let step = RwSignal::new(Step::Connections);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(Option::<String>::None);

    // Step 1 — connection health.
    let status = RwSignal::new(Option::<StatusInfo>::None);

    // Step 2 — models. The `default_*` are the gateway config defaults (shown as
    // placeholders); `transcription_model` is prefilled-but-hidden so the
    // full-replace PUT preserves it.
    let chat_model = RwSignal::new(String::new());
    let speech_model = RwSignal::new(String::new());
    let speech_voice = RwSignal::new(String::new());
    let transcription_model = RwSignal::new(String::new());
    let default_chat = RwSignal::new(String::new());
    let default_speech = RwSignal::new(String::new());
    let default_voice = RwSignal::new(String::new());
    let chat_models = RwSignal::new(Vec::<crate::api::ModelInfo>::new());
    let tts_models = RwSignal::new(Vec::<crate::api::ModelInfo>::new());
    let voices = RwSignal::new(Vec::<crate::api::VoiceInfo>::new());

    // Step 3 — profile form + an optional free-text memory.
    let name = RwSignal::new(String::new());
    let timezone = RwSignal::new(String::new());
    let working_hours = RwSignal::new(String::new());
    let role = RwSignal::new(String::new());
    let comms = RwSignal::new(String::new());
    let remember = RwSignal::new(String::new());

    // Step 4 — the personalization chat + the memories/skills it proposes.
    // `chat` is the visible transcript (replayed to the server each turn); `input`
    // is the composer; `thinking` guards the in-flight turn; `started` fires the
    // opening question once; `done_hint` is the model's "Finish" cue.
    let chat = RwSignal::new(Vec::<PersonalizeTurn>::new());
    let input = RwSignal::new(String::new());
    let thinking = RwSignal::new(false);
    let started = RwSignal::new(false);
    let done_hint = RwSignal::new(false);
    // Accumulated proposals (deduped), each default-selected for saving on Finish.
    let mem_props = RwSignal::new(Vec::<String>::new());
    let mem_selected = RwSignal::new(HashSet::<String>::new());
    let drafts = RwSignal::new(Vec::<SkillDraft>::new());
    let selected = RwSignal::new(HashSet::<String>::new());

    // Load the status + the user's existing model selections + the model catalogs
    // once on mount, so steps 1 and 2 are populated up front.
    spawn_local(async move {
        let token = auth::resolve_token();
        if let Ok(st) = rest::get_status(token.as_deref()).await {
            default_chat.set(st.llm.default_model.clone());
            default_speech.set(st.llm.speech_model.clone());
            default_voice.set(st.llm.speech_voice.clone());
            status.set(Some(st));
        }
        if let Ok(s) = rest::get_llm_settings(token.as_deref()).await {
            chat_model.set(s.chat_model.unwrap_or_default());
            speech_model.set(s.speech_model.unwrap_or_default());
            speech_voice.set(s.speech_voice.unwrap_or_default());
            transcription_model.set(s.transcription_model.unwrap_or_default());
        }
        // Each picker is filtered to its class: chat (`llm`) and pure TTS (`tts`),
        // so the speech picker offers only speech models — and surfaces TTS-only
        // ids the full catalog omits.
        if let Ok(m) = rest::list_llm_models(token.as_deref(), "llm").await {
            chat_models.set(m);
        }
        if let Ok(m) = rest::list_llm_models(token.as_deref(), "tts").await {
            tts_models.set(m);
        }
    });

    // Voices are per speech-model: reload whenever the chosen (or default) one changes.
    Effect::new(move |_| {
        let chosen = speech_model.get();
        let effective = if chosen.trim().is_empty() {
            default_speech.get()
        } else {
            chosen
        };
        let token = auth::resolve_token();
        spawn_local(async move {
            match rest::list_llm_voices(token.as_deref(), effective.trim()).await {
                Ok(v) => voices.set(v),
                Err(_) => voices.set(Vec::new()),
            }
        });
    });

    // Re-probe the backing services (the "Recheck" button on step 1).
    let recheck = move |_| {
        status.set(None);
        let token = auth::resolve_token();
        spawn_local(async move {
            if let Ok(st) = rest::get_status(token.as_deref()).await {
                status.set(Some(st));
            }
        });
    };

    // One personalization turn: replay the chat, append the assistant's reply, and
    // fold any freshly proposed memories/skills into the review lists (deduped,
    // default-selected). Reused for the opening question and every user message.
    let run_turn = move || {
        if thinking.get_untracked() {
            return;
        }
        thinking.set(true);
        error.set(None);
        let history = chat.get_untracked();
        spawn_local(async move {
            let token = auth::resolve_token();
            let body = PersonalizeRequest { messages: history };
            match rest::personalize(token.as_deref(), &body).await {
                Ok(resp) => {
                    chat.update(|c| {
                        c.push(PersonalizeTurn {
                            role: "assistant".to_string(),
                            content: resp.reply,
                        })
                    });
                    // Merge newly proposed memories (dedup by normalized text).
                    let existing = mem_props.get_untracked();
                    let mut fresh_mem: Vec<String> = Vec::new();
                    for f in resp.memories {
                        let f = f.trim().to_string();
                        if !f.is_empty()
                            && !existing.iter().any(|e| eq_norm(e, &f))
                            && !fresh_mem.iter().any(|e| eq_norm(e, &f))
                        {
                            fresh_mem.push(f);
                        }
                    }
                    if !fresh_mem.is_empty() {
                        mem_selected.update(|s| {
                            for f in &fresh_mem {
                                s.insert(f.clone());
                            }
                        });
                        mem_props.update(|list| list.extend(fresh_mem));
                    }
                    // Merge newly proposed skills (dedup by name).
                    let have: HashSet<String> = drafts
                        .get_untracked()
                        .iter()
                        .map(|d| d.name.clone())
                        .collect();
                    let mut fresh_sk: Vec<SkillDraft> = Vec::new();
                    for d in resp.skills {
                        if !have.contains(&d.name) && !fresh_sk.iter().any(|e| e.name == d.name) {
                            fresh_sk.push(d);
                        }
                    }
                    if !fresh_sk.is_empty() {
                        selected.update(|s| {
                            for d in &fresh_sk {
                                s.insert(d.name.clone());
                            }
                        });
                        drafts.update(|list| list.extend(fresh_sk));
                    }
                    done_hint.set(resp.done);
                }
                Err(err) => error.set(Some(err.to_string())),
            }
            thinking.set(false);
        });
    };

    // Send the composer's text as the user's next turn.
    let submit = move || {
        let text = input.get_untracked().trim().to_string();
        if text.is_empty() || thinking.get_untracked() {
            return;
        }
        chat.update(|c| {
            c.push(PersonalizeTurn {
                role: "user".to_string(),
                content: text,
            })
        });
        input.set(String::new());
        run_turn();
    };

    // Fire the assistant's opening question the first time the Personalize step is
    // shown (an empty history tells the server to greet + ask).
    Effect::new(move |_| {
        if step.get() == Step::Personalize && !started.get_untracked() {
            started.set(true);
            run_turn();
        }
    });

    // The primary "Next/Finish" action — its effect depends on the current step.
    let advance = move |_| {
        if busy.get_untracked() {
            return;
        }
        error.set(None);
        match step.get_untracked() {
            Step::Connections => step.set(Step::Models),
            Step::Models => {
                busy.set(true);
                let body = LlmSettings {
                    chat_model: blank_to_none(chat_model.get_untracked()),
                    speech_model: blank_to_none(speech_model.get_untracked()),
                    speech_voice: blank_to_none(speech_voice.get_untracked()),
                    transcription_model: blank_to_none(transcription_model.get_untracked()),
                    voice_input_speed: crate::api::default_voice_input_speed(),
                    // Onboarding offers no OCR pick; the `[ocr]` engine chain applies.
                    ocr_model: None,
                    // Onboarding doesn't manage the force-image-input list; a plain
                    // `PUT /llm-settings` ignores it anyway.
                    image_input_models: Vec::new(),
                };
                spawn_local(async move {
                    let token = auth::resolve_token();
                    match rest::set_llm_settings(token.as_deref(), &body).await {
                        Ok(_) => step.set(Step::Profile),
                        Err(err) => error.set(Some(err.to_string())),
                    }
                    busy.set(false);
                });
            }
            Step::Profile => {
                busy.set(true);
                let mut fields = serde_json::Map::new();
                for (k, v) in [
                    ("name", name.get_untracked()),
                    ("timezone", timezone.get_untracked()),
                    ("working_hours", working_hours.get_untracked()),
                    ("role", role.get_untracked()),
                    ("communication_style", comms.get_untracked()),
                ] {
                    let t = v.trim();
                    if !t.is_empty() {
                        fields.insert(k.to_string(), serde_json::Value::String(t.to_string()));
                    }
                }
                let fact = remember.get_untracked().trim().to_string();
                spawn_local(async move {
                    let token = auth::resolve_token();
                    if !fields.is_empty() {
                        let payload = serde_json::Value::Object(fields);
                        if let Err(err) = rest::update_profile(token.as_deref(), &payload).await {
                            error.set(Some(err.to_string()));
                            busy.set(false);
                            return;
                        }
                    }
                    if !fact.is_empty() {
                        let mem = crate::api::CreateMemory {
                            scope: "user".to_string(),
                            text: fact,
                        };
                        if let Err(err) = rest::create_memory(token.as_deref(), &mem).await {
                            error.set(Some(err.to_string()));
                            busy.set(false);
                            return;
                        }
                    }
                    step.set(Step::Personalize);
                    busy.set(false);
                });
            }
            Step::Personalize => {
                busy.set(true);
                // Persist the selected memories + skill drafts (both idempotent),
                // then stamp completion and move to the final step.
                let chosen_mem: Vec<String> = mem_props
                    .get_untracked()
                    .into_iter()
                    .filter(|f| mem_selected.with_untracked(|s| s.contains(f)))
                    .collect();
                let chosen_sk: Vec<SkillDraft> = drafts
                    .get_untracked()
                    .into_iter()
                    .filter(|d| selected.with_untracked(|s| s.contains(&d.name)))
                    .collect();
                spawn_local(async move {
                    let token = auth::resolve_token();
                    for text in chosen_mem {
                        let mem = crate::api::CreateMemory {
                            scope: "user".to_string(),
                            text,
                        };
                        if let Err(err) = rest::create_memory(token.as_deref(), &mem).await {
                            error.set(Some(err.to_string()));
                            busy.set(false);
                            return;
                        }
                    }
                    for d in chosen_sk {
                        let body = UpdateSkill {
                            description: d.description,
                            instructions_md: d.instructions_md,
                            tools: d.tools,
                            code: None,
                            advertised: true,
                        };
                        if let Err(err) = rest::update_skill(token.as_deref(), &d.name, &body).await
                        {
                            error.set(Some(format!("could not save `{}`: {err}", d.name)));
                            busy.set(false);
                            return;
                        }
                    }
                    if let Err(err) = rest::complete_onboarding(token.as_deref()).await {
                        error.set(Some(err.to_string()));
                        busy.set(false);
                        return;
                    }
                    step.set(Step::Done);
                    busy.set(false);
                });
            }
            Step::Done => active.set(Panel::Chat),
        }
    };

    let back = move |_| {
        if busy.get_untracked() {
            return;
        }
        error.set(None);
        match step.get_untracked() {
            Step::Models => step.set(Step::Connections),
            Step::Profile => step.set(Step::Models),
            Step::Personalize => step.set(Step::Profile),
            _ => {}
        }
    };

    let primary_label = move || match step.get() {
        Step::Personalize => "Finish",
        Step::Done => "Go to Chat",
        _ if busy.get() => "Working…",
        _ => "Next",
    };
    let back_disabled = move || busy.get() || matches!(step.get(), Step::Connections | Step::Done);
    let primary_disabled = move || busy.get();

    view! {
        <div class="wizard">
            <header class="wizard-head">
                <h1 class="wizard-title">"Quick start"</h1>
                <p class="wizard-sub">"A few steps to set catalerum up around you."</p>
                <ol class="wizard-steps">
                    {STEPS
                        .iter()
                        .map(|(s, label)| {
                            let s = *s;
                            let cls = move || {
                                let cur = step.get().index();
                                let i = s.index();
                                if i == cur {
                                    "wizard-pip wizard-pip-on"
                                } else if i < cur {
                                    "wizard-pip wizard-pip-done"
                                } else {
                                    "wizard-pip"
                                }
                            };
                            view! { <li class=cls>{*label}</li> }
                        })
                        .collect::<Vec<_>>()}
                </ol>
            </header>

            <div class="wizard-body">
                {move || match step.get() {
                    Step::Connections => view! {
                        <section class="wizard-section">
                            <h2 class="wizard-h2">"Connections"</h2>
                            <p class="wizard-help">
                                "Make sure catalerum can reach its services. The LLM gateway must be up for chat and for drafting skills."
                            </p>
                            {move || match status.get() {
                                None => view! { <p class="wizard-muted">"Checking…"</p> }.into_any(),
                                Some(st) => {
                                    let healthy = st.healthy;
                                    let down: Vec<String> = st
                                        .services
                                        .iter()
                                        .filter(|s| s.state == "down")
                                        .map(|s| s.name.clone())
                                        .collect();
                                    // The warning names what is actually down: only when the
                                    // LLM gateway itself is down are chat and skill drafting
                                    // at stake; other services degrade their own features.
                                    let warn = if down.iter().any(|n| n == "LLM gateway") {
                                        "The LLM gateway is down. You can continue, but chat and skill drafting will not work until it is back up."
                                            .to_string()
                                    } else if down.len() == 1 {
                                        format!(
                                            "{} is down. You can continue — chat and skill drafting still work, but features that depend on it may not.",
                                            down[0]
                                        )
                                    } else if !down.is_empty() {
                                        format!(
                                            "Some services are down ({}). You can continue — chat and skill drafting still work, but features that depend on them may not.",
                                            down.join(", ")
                                        )
                                    } else {
                                        "Some services are down. You can continue, but the affected features may not work until they are back up."
                                            .to_string()
                                    };
                                    view! {
                                        <ul class="settings-svc-list">
                                            {st.services
                                                .into_iter()
                                                .map(|s| {
                                                    let cls = match s.state.as_str() {
                                                        "up" => "settings-svc-state settings-svc-up",
                                                        "down" => "settings-svc-state settings-svc-down",
                                                        _ => "settings-svc-state settings-svc-disabled",
                                                    };
                                                    let label = s.state.to_uppercase();
                                                    view! {
                                                        <li class="settings-svc">
                                                            <span class="settings-svc-name">{s.name}</span>
                                                            <span class="settings-svc-detail">{s.detail}</span>
                                                            <span class=cls>{label}</span>
                                                        </li>
                                                    }
                                                })
                                                .collect::<Vec<_>>()}
                                        </ul>
                                        <Show
                                            when=move || !healthy
                                            fallback=|| ().into_view()
                                        >
                                            <p class="wizard-warn">{warn.clone()}</p>
                                        </Show>
                                    }
                                        .into_any()
                                }
                            }}
                            <button class="settings-btn" on:click=recheck>"Recheck"</button>
                        </section>
                    }
                        .into_any(),
                    Step::Models => view! {
                        <section class="wizard-section">
                            <h2 class="wizard-h2">"Models"</h2>
                            <p class="wizard-help">
                                "Pick the chat and voice models, or leave a field blank to use the gateway default shown as the placeholder."
                            </p>
                            <div class="settings-field">
                                <label class="settings-label">"Chat model"</label>
                                {model_autocomplete(
                                    Signal::derive(move || chat_model.get()),
                                    move |v| chat_model.set(v),
                                    model_options(chat_models, false),
                                    Signal::derive(move || {
                                        let d = default_chat.get();
                                        if d.is_empty() { "gateway default".to_string() } else { format!("default: {d}") }
                                    }),
                                    Signal::derive(|| false),
                                    "settings-input",
                                )}
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Speech model (text-to-speech)"</label>
                                {model_autocomplete(
                                    Signal::derive(move || speech_model.get()),
                                    move |v| speech_model.set(v),
                                    model_options(tts_models, false),
                                    Signal::derive(move || {
                                        let d = default_speech.get();
                                        if d.is_empty() { "gateway default".to_string() } else { format!("default: {d}") }
                                    }),
                                    Signal::derive(|| false),
                                    "settings-input",
                                )}
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Voice"</label>
                                {model_autocomplete(
                                    Signal::derive(move || speech_voice.get()),
                                    move |v| speech_voice.set(v),
                                    voice_options(voices),
                                    Signal::derive(move || {
                                        let d = default_voice.get();
                                        if d.is_empty() { "gateway default".to_string() } else { format!("default: {d}") }
                                    }),
                                    Signal::derive(|| false),
                                    "settings-input",
                                )}
                            </div>
                        </section>
                    }
                        .into_any(),
                    Step::Profile => view! {
                        <section class="wizard-section">
                            <h2 class="wizard-h2">"About you"</h2>
                            <p class="wizard-help">
                                "These become your profile, woven into the assistant's context every turn. All of it stays editable in the Memory panel."
                            </p>
                            <div class="settings-field">
                                <label class="settings-label">"Name"</label>
                                <input class="settings-input" placeholder="What should I call you?"
                                    prop:value=move || name.get()
                                    on:input=move |ev| name.set(event_target_value(&ev)) />
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Timezone"</label>
                                <input class="settings-input" placeholder="e.g. Europe/Berlin"
                                    prop:value=move || timezone.get()
                                    on:input=move |ev| timezone.set(event_target_value(&ev)) />
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Working hours"</label>
                                <input class="settings-input" placeholder="e.g. 9–17, Mon–Fri"
                                    prop:value=move || working_hours.get()
                                    on:input=move |ev| working_hours.set(event_target_value(&ev)) />
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Role"</label>
                                <input class="settings-input" placeholder="e.g. software engineer"
                                    prop:value=move || role.get()
                                    on:input=move |ev| role.set(event_target_value(&ev)) />
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Communication style"</label>
                                <input class="settings-input" placeholder="e.g. concise and direct"
                                    prop:value=move || comms.get()
                                    on:input=move |ev| comms.set(event_target_value(&ev)) />
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Anything else to remember? (optional)"</label>
                                <textarea class="settings-input wizard-textarea"
                                    placeholder="A durable fact — saved as a memory."
                                    prop:value=move || remember.get()
                                    on:input=move |ev| remember.set(event_target_value(&ev))
                                ></textarea>
                            </div>
                        </section>
                    }
                        .into_any(),
                    Step::Personalize => view! {
                        <section class="wizard-section wizard-chat-section">
                            <h2 class="wizard-h2">"Personalize"</h2>
                            <p class="wizard-help">
                                "Chat with catalerum so it can get to know you. It'll ask a few questions and — as it learns — suggest memories and skills to keep. Review them below and save the ones you like on Finish."
                            </p>
                            <div class="wizard-chat">
                                <div class="wizard-chat-log">
                                    {move || chat.get()
                                        .into_iter()
                                        .map(|m| {
                                            let cls = if m.role == "user" {
                                                "wizard-msg wizard-msg-user"
                                            } else {
                                                "wizard-msg wizard-msg-assistant"
                                            };
                                            view! { <div class=cls>{m.content}</div> }
                                        })
                                        .collect::<Vec<_>>()}
                                    <Show when=move || thinking.get() fallback=|| ().into_view()>
                                        <div class="wizard-msg wizard-msg-assistant wizard-msg-typing">
                                            <span class="wizard-typing-dot"></span>
                                            <span class="wizard-typing-dot"></span>
                                            <span class="wizard-typing-dot"></span>
                                        </div>
                                    </Show>
                                </div>
                                <div class="wizard-chat-input">
                                    <textarea
                                        class="settings-input wizard-chat-textarea"
                                        placeholder="Type your reply…"
                                        prop:value=move || input.get()
                                        on:input=move |ev| input.set(event_target_value(&ev))
                                        on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                                            if ev.key() == "Enter" && !ev.shift_key() {
                                                ev.prevent_default();
                                                submit();
                                            }
                                        }
                                    ></textarea>
                                    <button
                                        class="settings-btn settings-btn-primary wizard-chat-send"
                                        disabled=move || thinking.get() || input.with(|v| v.trim().is_empty())
                                        on:click=move |_| submit()
                                    >
                                        "Send"
                                    </button>
                                </div>
                            </div>

                            <Show when=move || done_hint.get() fallback=|| ().into_view()>
                                <p class="wizard-muted">
                                    "Looks like we've covered the essentials — hit Finish when you're ready, or keep chatting to add more."
                                </p>
                            </Show>

                            <Show
                                when=move || !mem_props.get().is_empty() || !drafts.get().is_empty()
                                fallback=|| view! {
                                    <p class="wizard-muted">
                                        "Memories and skills to save will appear here as we chat."
                                    </p>
                                }
                            >
                                <div class="wizard-proposals">
                                    <Show when=move || !mem_props.get().is_empty() fallback=|| ().into_view()>
                                        <h3 class="wizard-proposal-title">"Memories to save"</h3>
                                        <ul class="wizard-mem-list">
                                            {move || mem_props.get()
                                                .into_iter()
                                                .map(|f| {
                                                    let for_chk = f.clone();
                                                    let for_tog = f.clone();
                                                    view! {
                                                        <li class="wizard-mem">
                                                            <label class="wizard-mem-head">
                                                                <input
                                                                    type="checkbox"
                                                                    prop:checked=move || mem_selected.with(|s| s.contains(&for_chk))
                                                                    on:change=move |_| mem_selected.update(|s| {
                                                                        if !s.remove(&for_tog) { s.insert(for_tog.clone()); }
                                                                    })
                                                                />
                                                                <span class="wizard-mem-text">{f}</span>
                                                            </label>
                                                        </li>
                                                    }
                                                })
                                                .collect::<Vec<_>>()}
                                        </ul>
                                    </Show>
                                    <Show when=move || !drafts.get().is_empty() fallback=|| ().into_view()>
                                        <h3 class="wizard-proposal-title">"Skills to save"</h3>
                                        <ul class="wizard-skill-list">
                                            <For
                                                each=move || drafts.get()
                                                key=|d| d.name.clone()
                                                children=move |d: SkillDraft| {
                                                    let name_chk = d.name.clone();
                                                    let name_tog = d.name.clone();
                                                    view! {
                                                        <li class="wizard-skill">
                                                            <label class="wizard-skill-head">
                                                                <input
                                                                    type="checkbox"
                                                                    prop:checked=move || selected.with(|s| s.contains(&name_chk))
                                                                    on:change=move |_| selected.update(|s| {
                                                                        if !s.remove(&name_tog) { s.insert(name_tog.clone()); }
                                                                    })
                                                                />
                                                                <span class="wizard-skill-name">{d.name.clone()}</span>
                                                            </label>
                                                            <p class="wizard-skill-desc">{d.description.clone()}</p>
                                                            <details class="wizard-skill-detail">
                                                                <summary>"runbook"</summary>
                                                                <pre class="wizard-skill-md">{d.instructions_md.clone()}</pre>
                                                            </details>
                                                        </li>
                                                    }
                                                }
                                            />
                                        </ul>
                                    </Show>
                                </div>
                            </Show>
                        </section>
                    }
                        .into_any(),
                    Step::Done => view! {
                        <section class="wizard-section wizard-done">
                            <h2 class="wizard-h2">"You're all set"</h2>
                            <p class="wizard-help">
                                "Your profile, model choices, memories, and skills are saved. Open Chat to start — you can revisit this any time from “Quick start”."
                            </p>
                            <h3 class="settings-section-title">"Pick a look"</h3>
                            <p class="wizard-help">
                                "Set the workbench theme — including a high-contrast option. Change it any time in Settings → Appearance."
                            </p>
                            <ThemePicker />
                        </section>
                    }
                        .into_any(),
                }}

                <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                    <p class="wizard-error">{move || error.get().unwrap_or_default()}</p>
                </Show>
            </div>

            <footer class="wizard-foot">
                <button class="settings-btn" disabled=back_disabled on:click=back>"Back"</button>
                <span class="wizard-foot-spacer"></span>
                <button
                    class="settings-btn settings-btn-primary"
                    disabled=primary_disabled
                    on:click=advance
                >
                    {primary_label}
                </button>
            </footer>
        </div>
    }
}
