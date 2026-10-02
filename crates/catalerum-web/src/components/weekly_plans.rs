//! The **Plans** tab of the Calendar panel (SOUL §8/§12) — weekly plans.
//!
//! A weekly plan is a reusable week template ("Normal week", "Exam week", …):
//! a set of slots, each a weekday + `HH:MM`–`HH:MM` (or all-day) in the plan's
//! timezone. Several plans can exist side by side and are edited here without
//! touching the calendar; **Apply** materialises a plan into a chosen week (or
//! several), creating one event per slot. Those events stay linked to the plan,
//! so applying the same week again updates them in place (removing events of
//! deleted slots), and **Remove from week** deletes exactly them.
//!
//! Talks to `/weekly-plans` over REST (see [`crate::rest`]); after an apply /
//! unapply it asks the parent calendar to refresh so the grids reflect the
//! change, and "Show" jumps the calendar's Week view to an applied week.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsValue;

use crate::api::{
    ApplyWeeklyPlan, Calendar, DuplicateWeeklyPlan, WeekOutcome, WeeklyPlan,
    WeeklyPlanApplication, WeeklyPlanBody, WeeklyPlanEntry, WeeklyPlanEntryBody,
};
use crate::auth;
use crate::components::calendar::{
    days_from_civil, parse_ymd, week_start, week_title, ymd_string,
};
use crate::components::dialogs::{use_dialogs, ConfirmSpec, PromptSpec};
use crate::components::icons::MdIcon;
use crate::components::widgets::row_action;
use crate::rest;

/// Short weekday labels, Monday first (index = the plan's `weekday`).
const DAY_LABELS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Most consecutive weeks one apply may cover (mirrors the API's cap).
const MAX_APPLY_WEEKS: u32 = 12;

/// `HH:MM` for minutes after midnight.
fn hhmm(minutes: i32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// An entry's time label: `All day` or `09:00–10:30`.
fn entry_time(e: &WeeklyPlanEntry) -> String {
    if e.all_day {
        "All day".to_string()
    } else {
        format!("{}–{}", hhmm(e.start_minute), hhmm(e.end_minute))
    }
}

/// The browser's IANA timezone (the default for a new plan), else `UTC`.
fn browser_timezone() -> String {
    let fmt = js_sys::Intl::DateTimeFormat::new(&js_sys::Array::new(), &js_sys::Object::new());
    js_sys::Reflect::get(&fmt.resolved_options(), &JsValue::from_str("timeZone"))
        .ok()
        .and_then(|v| v.as_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "UTC".to_string())
}

/// Day-number of a `YYYY-MM-DD` string.
fn daynum(date: &str) -> Option<i64> {
    parse_ymd(date).map(|(y, m, d)| days_from_civil(y, m, d))
}

/// One-line summary of what applying did, across all touched weeks.
fn outcome_summary(outcomes: &[WeekOutcome]) -> String {
    let sum = |f: fn(&WeekOutcome) -> usize| outcomes.iter().map(f).sum::<usize>();
    let (c, u, k, r) = (
        sum(|o| o.created.len()),
        sum(|o| o.updated.len()),
        sum(|o| o.unchanged.len()),
        sum(|o| o.removed.len()),
    );
    let weeks = if outcomes.len() == 1 {
        "1 week".to_string()
    } else {
        format!("{} weeks", outcomes.len())
    };
    format!("Applied to {weeks}: {c} created, {u} updated, {k} unchanged, {r} removed.")
}

/// Comma-separated labels → a clean list.
fn split_labels(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn opt(raw: String) -> Option<String> {
    let t = raw.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// The Plans tab. `calendars` drives the target-calendar pickers; `today` is the
/// browser-local day-number (the apply week defaults to next week);
/// `on_applied` refreshes the parent calendar after an apply/unapply;
/// `on_show_week` jumps the calendar's Week view to a day-number.
#[component]
pub fn WeeklyPlansView<A, S>(
    calendars: RwSignal<Vec<Calendar>>,
    today: i64,
    on_applied: A,
    on_show_week: S,
) -> impl IntoView
where
    A: Fn() + Copy + Send + Sync + 'static,
    S: Fn(i64) + Copy + Send + Sync + 'static,
{
    let dialogs = use_dialogs();
    let plans = RwSignal::new(Vec::<WeeklyPlan>::new());
    let selected = RwSignal::new(Option::<String>::None);
    let apps = RwSignal::new(Vec::<WeeklyPlanApplication>::new());
    let loading = RwSignal::new(true);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(Option::<String>::None);
    let notice = RwSignal::new(Option::<String>::None);

    // Plan header form (mirrors the selected plan; saved explicitly).
    let h_name = RwSignal::new(String::new());
    let h_desc = RwSignal::new(String::new());
    let h_tz = RwSignal::new(String::new());
    let h_cal = RwSignal::new(String::new());

    // Entry form. `e_editing` = the entry id a save replaces (None = add).
    let e_open = RwSignal::new(false);
    let e_editing = RwSignal::new(Option::<String>::None);
    let e_weekday = RwSignal::new(0i32);
    let e_all_day = RwSignal::new(false);
    let e_start = RwSignal::new("09:00".to_string());
    let e_end = RwSignal::new("10:00".to_string());
    let e_summary = RwSignal::new(String::new());
    let e_location = RwSignal::new(String::new());
    let e_labels = RwSignal::new(String::new());
    let e_cal = RwSignal::new(String::new());
    let e_error = RwSignal::new(Option::<String>::None);

    // Apply controls: the target week (any date in it) and how many weeks.
    let a_week = RwSignal::new(ymd_string(week_start(today) + 7));
    let a_weeks = RwSignal::new("1".to_string());

    let current = move || {
        let id = selected.get()?;
        plans.with(|ps| ps.iter().find(|p| p.id == id).cloned())
    };
    let writable = move || {
        calendars
            .get()
            .into_iter()
            .filter(Calendar::is_writable)
            .collect::<Vec<_>>()
    };

    // Fill the header form from the selected plan.
    let sync_header = move || {
        if let Some(p) = current() {
            h_name.set(p.name.clone());
            h_desc.set(p.description.clone().unwrap_or_default());
            h_tz.set(p.timezone.clone());
            h_cal.set(p.calendar_id.clone().unwrap_or_default());
        }
    };

    let load_apps = move |id: String| {
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_weekly_plan_applications(token.as_deref(), &id).await {
                Ok(list) => apps.set(list),
                Err(_) => apps.set(Vec::new()),
            }
        });
    };

    let select = move |id: String| {
        selected.set(Some(id.clone()));
        e_open.set(false);
        notice.set(None);
        sync_header();
        load_apps(id);
    };

    // (Re)load all plans; keep the selection when it still exists.
    let reload = move |prefer: Option<String>| {
        loading.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_weekly_plans(token.as_deref()).await {
                Ok(list) => {
                    let keep = prefer
                        .or_else(|| selected.get_untracked())
                        .filter(|id| list.iter().any(|p| &p.id == id))
                        .or_else(|| list.first().map(|p| p.id.clone()));
                    plans.set(list);
                    error.set(None);
                    match keep {
                        Some(id) => select(id),
                        None => {
                            selected.set(None);
                            apps.set(Vec::new());
                        }
                    }
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    };
    reload(None);

    // Shared async runner for a mutation: busy flag + error surfacing.
    let run = move |fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<String>, String>>>>| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match fut.await {
                Ok(msg) => {
                    if msg.is_some() {
                        notice.set(msg);
                    }
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    let new_plan = move || {
        dialogs.prompt(
            PromptSpec::new(
                "New weekly plan",
                "Name the plan — e.g. “Normal week” or “Exam week”.",
            )
            .placeholder("Normal week")
            .confirm_label("Create"),
            move |name| {
                run(Box::pin(async move {
                    let token = auth::resolve_token();
                    let plan = rest::create_weekly_plan(
                        token.as_deref(),
                        &WeeklyPlanBody {
                            name,
                            description: None,
                            calendar_id: None,
                            timezone: browser_timezone(),
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    reload(Some(plan.id));
                    Ok(None)
                }));
            },
        );
    };

    let save_header = move || {
        let Some(id) = selected.get_untracked() else {
            return;
        };
        let body = WeeklyPlanBody {
            name: h_name.get_untracked().trim().to_string(),
            description: opt(h_desc.get_untracked()),
            calendar_id: opt(h_cal.get_untracked()),
            timezone: h_tz.get_untracked().trim().to_string(),
        };
        run(Box::pin(async move {
            let token = auth::resolve_token();
            rest::update_weekly_plan(token.as_deref(), &id, &body)
                .await
                .map_err(|e| e.to_string())?;
            reload(Some(id));
            Ok(Some("Plan saved.".to_string()))
        }));
    };

    let duplicate = move || {
        let Some(p) = current() else { return };
        dialogs.prompt(
            PromptSpec {
                initial: format!("{} (next week)", p.name),
                ..PromptSpec::new(
                    "Duplicate plan",
                    "Copy this plan with all its slots — tweak the copy for a \
                     particular week without changing the original.",
                )
            }
            .confirm_label("Duplicate"),
            move |name| {
                let id = p.id.clone();
                run(Box::pin(async move {
                    let token = auth::resolve_token();
                    let copy = rest::duplicate_weekly_plan(
                        token.as_deref(),
                        &id,
                        &DuplicateWeeklyPlan { name },
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    reload(Some(copy.id));
                    Ok(None)
                }));
            },
        );
    };

    let delete_plan = move || {
        let Some(p) = current() else { return };
        dialogs.confirm(
            ConfirmSpec::danger(
                "Delete plan?",
                format!(
                    "Delete the plan “{}”? Events it already applied stay in your \
                     calendar (use “Remove from week” first to delete them).",
                    p.name
                ),
                "Delete",
            ),
            move || {
                let id = p.id.clone();
                run(Box::pin(async move {
                    let token = auth::resolve_token();
                    rest::delete_weekly_plan(token.as_deref(), &id)
                        .await
                        .map_err(|e| e.to_string())?;
                    selected.set(None);
                    reload(None);
                    Ok(None)
                }));
            },
        );
    };

    // --- entries -----------------------------------------------------------
    let open_entry_form = move |weekday: i32, entry: Option<WeeklyPlanEntry>| {
        e_error.set(None);
        match entry {
            Some(e) => {
                e_editing.set(Some(e.id.clone()));
                e_weekday.set(e.weekday);
                e_all_day.set(e.all_day);
                e_start.set(hhmm(e.start_minute));
                e_end.set(hhmm(e.end_minute.min(23 * 60 + 59)));
                e_summary.set(e.summary.clone());
                e_location.set(e.location.clone().unwrap_or_default());
                e_labels.set(e.labels.join(", "));
                e_cal.set(e.calendar_id.clone().unwrap_or_default());
            }
            None => {
                e_editing.set(None);
                e_weekday.set(weekday);
                e_all_day.set(false);
                e_start.set("09:00".to_string());
                e_end.set("10:00".to_string());
                e_summary.set(String::new());
                e_location.set(String::new());
                e_labels.set(String::new());
                e_cal.set(String::new());
            }
        }
        e_open.set(true);
    };

    let save_entry = move || {
        let Some(plan_id) = selected.get_untracked() else {
            return;
        };
        let summary = e_summary.get_untracked().trim().to_string();
        if summary.is_empty() {
            e_error.set(Some("Give the slot a title.".to_string()));
            return;
        }
        let all_day = e_all_day.get_untracked();
        let body = WeeklyPlanEntryBody {
            weekday: e_weekday.get_untracked(),
            start: (!all_day).then(|| e_start.get_untracked()),
            end: (!all_day).then(|| e_end.get_untracked()),
            all_day,
            summary,
            location: opt(e_location.get_untracked()),
            body: None,
            labels: split_labels(&e_labels.get_untracked()),
            calendar_id: opt(e_cal.get_untracked()),
        };
        let editing = e_editing.get_untracked();
        busy.set(true);
        e_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            let res = match &editing {
                Some(entry_id) => {
                    rest::update_weekly_plan_entry(token.as_deref(), &plan_id, entry_id, &body)
                        .await
                }
                None => rest::add_weekly_plan_entry(token.as_deref(), &plan_id, &body).await,
            };
            match res {
                Ok(_) => {
                    e_open.set(false);
                    reload(Some(plan_id));
                }
                Err(e) => e_error.set(Some(e.to_string())),
            }
            busy.set(false);
        });
    };

    let delete_entry = move |entry_id: String| {
        let Some(plan_id) = selected.get_untracked() else {
            return;
        };
        run(Box::pin(async move {
            let token = auth::resolve_token();
            rest::delete_weekly_plan_entry(token.as_deref(), &plan_id, &entry_id)
                .await
                .map_err(|e| e.to_string())?;
            reload(Some(plan_id));
            Ok(None)
        }));
    };

    // --- apply / unapply ---------------------------------------------------
    let apply_week = move |week: String, weeks: u32| {
        let Some(id) = selected.get_untracked() else {
            return;
        };
        run(Box::pin(async move {
            let token = auth::resolve_token();
            let outcomes = rest::apply_weekly_plan(
                token.as_deref(),
                &id,
                &ApplyWeeklyPlan {
                    week_start: week,
                    weeks: Some(weeks),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            load_apps(id);
            on_applied();
            Ok(Some(outcome_summary(&outcomes)))
        }));
    };

    let unapply = move |week: String| {
        let Some(p) = current() else { return };
        let title = daynum(&week).map(|d| week_title(week_start(d))).unwrap_or(week.clone());
        dialogs.confirm(
            ConfirmSpec::danger(
                "Remove plan from week?",
                format!(
                    "Delete the events “{}” created in the week {title}? Other events \
                     in that week are not touched.",
                    p.name
                ),
                "Remove",
            ),
            move || {
                let (id, week) = (p.id.clone(), week.clone());
                run(Box::pin(async move {
                    let token = auth::resolve_token();
                    let out = rest::unapply_weekly_plan(
                        token.as_deref(),
                        &id,
                        &ApplyWeeklyPlan {
                            week_start: week,
                            weeks: None,
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    load_apps(id);
                    on_applied();
                    Ok(Some(format!("Removed {} event(s).", out.removed.len())))
                }));
            },
        );
    };

    let a_week_title = move || {
        daynum(&a_week.get())
            .map(|d| week_title(week_start(d)))
            .unwrap_or_default()
    };

    view! {
        <div class="wp">
            <div class="wp-bar">
                <div class="wp-plan-tabs" role="tablist" aria-label="Weekly plans">
                    {move || {
                        plans
                            .get()
                            .into_iter()
                            .map(|p| {
                                let id = p.id.clone();
                                let on = {
                                    let id = id.clone();
                                    move || selected.get().as_deref() == Some(id.as_str())
                                };
                                view! {
                                    <button
                                        class="cal-viewtab wp-plan-tab"
                                        class:cal-viewtab-on=on
                                        role="tab"
                                        on:click=move |_| select(id.clone())
                                    >
                                        {p.name.clone()}
                                        <span class="wp-count">{p.entries.len()}</span>
                                    </button>
                                }
                            })
                            .collect_view()
                    }}
                </div>
                <button class="cal-btn cal-btn-primary" on:click=move |_| new_plan()>
                    "New plan"
                </button>
            </div>

            <Show when=move || error.with(Option::is_some) fallback=|| ()>
                <p class="cal-form-error wp-msg">{move || error.get().unwrap_or_default()}</p>
            </Show>
            <Show when=move || notice.with(Option::is_some) fallback=|| ()>
                <p class="cal-notice wp-msg">{move || notice.get().unwrap_or_default()}</p>
            </Show>

            <Show
                when=move || !loading.get() && plans.with(Vec::is_empty)
                fallback=|| ()
            >
                <div class="cal-status">
                    <p>"No weekly plans yet."</p>
                    <p class="cal-muted">
                        "A plan is a reusable week — your regular slots like standups, \
                         gym, or study blocks. Create one, then apply it to any week."
                    </p>
                </div>
            </Show>

            <Show when=move || selected.with(Option::is_some) fallback=|| ()>
                // --- Plan header ------------------------------------------
                <section class="wp-head">
                    <div class="wp-fields">
                        <div class="cal-field">
                            <label class="cal-label">"Name"</label>
                            <input
                                class="cal-input"
                                prop:value=move || h_name.get()
                                on:input=move |ev| h_name.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="cal-field">
                            <label class="cal-label">"Timezone"</label>
                            <input
                                class="cal-input"
                                placeholder="Europe/Berlin"
                                prop:value=move || h_tz.get()
                                on:input=move |ev| h_tz.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="cal-field">
                            <label class="cal-label">"Calendar"</label>
                            <select
                                class="cal-input"
                                prop:value=move || h_cal.get()
                                on:change=move |ev| h_cal.set(event_target_value(&ev))
                            >
                                <option value="">"Default calendar"</option>
                                {move || {
                                    writable()
                                        .into_iter()
                                        .map(|c| view! { <option value=c.id.clone()>{c.name.clone()}</option> })
                                        .collect_view()
                                }}
                            </select>
                        </div>
                        <div class="cal-field wp-desc">
                            <label class="cal-label">"Description"</label>
                            <input
                                class="cal-input"
                                placeholder="Optional"
                                prop:value=move || h_desc.get()
                                on:input=move |ev| h_desc.set(event_target_value(&ev))
                            />
                        </div>
                    </div>
                    <div class="wp-actions">
                        <button
                            class="cal-btn cal-btn-primary"
                            disabled=move || busy.get()
                            on:click=move |_| save_header()
                        >
                            "Save"
                        </button>
                        <button
                            class="cal-btn"
                            disabled=move || busy.get()
                            on:click=move |_| duplicate()
                        >
                            "Duplicate"
                        </button>
                        <button
                            class="cal-btn wp-danger"
                            disabled=move || busy.get()
                            on:click=move |_| delete_plan()
                        >
                            "Delete"
                        </button>
                    </div>
                </section>

                // --- The week template ------------------------------------
                <section class="wp-week" aria-label="Plan slots by weekday">
                    {(0..7)
                        .map(|day| {
                            view! {
                                <div class="wp-day">
                                    <div class="wp-day-head">
                                        <span>{DAY_LABELS[day as usize]}</span>
                                        <button
                                            class="wp-add"
                                            title=format!("Add a slot on {}", DAY_LABELS[day as usize])
                                            on:click=move |_| open_entry_form(day, None)
                                        >
                                            "+"
                                        </button>
                                    </div>
                                    <ul class="wp-entries">
                                        {move || {
                                            current()
                                                .map(|p| p.entries)
                                                .unwrap_or_default()
                                                .into_iter()
                                                .filter(|e| e.weekday == day)
                                                .map(|e| {
                                                    let edit = e.clone();
                                                    let del_id = e.id.clone();
                                                    let del_name = e.summary.clone();
                                                    view! {
                                                        <li class="wp-entry">
                                                            <span class="wp-entry-time">{entry_time(&e)}</span>
                                                            <span class="wp-entry-title">{e.summary.clone()}</span>
                                                            {e.location.clone().map(|l| view! { <span class="wp-entry-meta">{l}</span> })}
                                                            <span class="wp-entry-acts">
                                                                {row_action(MdIcon::Edit, "Edit slot", false, move || {
                                                                    open_entry_form(day, Some(edit.clone()))
                                                                })}
                                                                {row_action(MdIcon::Delete, "Delete slot", true, move || {
                                                                    let id = del_id.clone();
                                                                    dialogs.confirm(
                                                                        ConfirmSpec::danger(
                                                                            "Delete slot?",
                                                                            format!(
                                                                                "Remove “{del_name}” from the plan? Already \
                                                                                 applied weeks drop its event when re-applied."
                                                                            ),
                                                                            "Delete",
                                                                        ),
                                                                        move || delete_entry(id.clone()),
                                                                    );
                                                                })}
                                                            </span>
                                                        </li>
                                                    }
                                                })
                                                .collect_view()
                                        }}
                                    </ul>
                                </div>
                            }
                        })
                        .collect_view()}
                </section>

                // --- Slot editor --------------------------------------------
                <Show when=move || e_open.get() fallback=|| ()>
                    <section class="cal-connect wp-entry-form">
                        <div class="wp-fields">
                            <div class="cal-field">
                                <label class="cal-label">"Day"</label>
                                <select
                                    class="cal-input"
                                    prop:value=move || e_weekday.get().to_string()
                                    on:change=move |ev| {
                                        e_weekday.set(event_target_value(&ev).parse().unwrap_or(0))
                                    }
                                >
                                    {(0..7)
                                        .map(|d| view! { <option value=d.to_string()>{DAY_LABELS[d]}</option> })
                                        .collect_view()}
                                </select>
                            </div>
                            <div class="cal-field">
                                <label class="cal-label">"Title"</label>
                                <input
                                    class="cal-input"
                                    placeholder="Team standup"
                                    prop:value=move || e_summary.get()
                                    on:input=move |ev| e_summary.set(event_target_value(&ev))
                                />
                            </div>
                            <label class="wp-check">
                                <input
                                    type="checkbox"
                                    prop:checked=move || e_all_day.get()
                                    on:change=move |ev| e_all_day.set(event_target_checked(&ev))
                                />
                                "All day"
                            </label>
                            <Show when=move || !e_all_day.get() fallback=|| ()>
                                <div class="cal-field">
                                    <label class="cal-label">"Start"</label>
                                    <input
                                        class="cal-input"
                                        type="time"
                                        prop:value=move || e_start.get()
                                        on:input=move |ev| e_start.set(event_target_value(&ev))
                                    />
                                </div>
                                <div class="cal-field">
                                    <label class="cal-label">"End"</label>
                                    <input
                                        class="cal-input"
                                        type="time"
                                        prop:value=move || e_end.get()
                                        on:input=move |ev| e_end.set(event_target_value(&ev))
                                    />
                                </div>
                            </Show>
                            <div class="cal-field">
                                <label class="cal-label">"Location (optional)"</label>
                                <input
                                    class="cal-input"
                                    prop:value=move || e_location.get()
                                    on:input=move |ev| e_location.set(event_target_value(&ev))
                                />
                            </div>
                            <div class="cal-field">
                                <label class="cal-label">"Labels (comma-separated)"</label>
                                <input
                                    class="cal-input"
                                    placeholder="work, focus"
                                    prop:value=move || e_labels.get()
                                    on:input=move |ev| e_labels.set(event_target_value(&ev))
                                />
                            </div>
                            <div class="cal-field">
                                <label class="cal-label">"Calendar"</label>
                                <select
                                    class="cal-input"
                                    prop:value=move || e_cal.get()
                                    on:change=move |ev| e_cal.set(event_target_value(&ev))
                                >
                                    <option value="">"Plan's calendar"</option>
                                    {move || {
                                        writable()
                                            .into_iter()
                                            .map(|c| view! { <option value=c.id.clone()>{c.name.clone()}</option> })
                                            .collect_view()
                                    }}
                                </select>
                            </div>
                        </div>
                        <Show when=move || e_error.with(Option::is_some) fallback=|| ()>
                            <p class="cal-form-error">{move || e_error.get().unwrap_or_default()}</p>
                        </Show>
                        <div class="wp-actions">
                            <button
                                class="cal-btn cal-btn-primary"
                                disabled=move || busy.get()
                                on:click=move |_| save_entry()
                            >
                                {move || if e_editing.with(Option::is_some) { "Save slot" } else { "Add slot" }}
                            </button>
                            <button class="cal-btn" on:click=move |_| e_open.set(false)>
                                "Cancel"
                            </button>
                        </div>
                    </section>
                </Show>

                // --- Apply to the calendar ----------------------------------
                <section class="wp-apply">
                    <h3 class="wp-h">"Apply to calendar"</h3>
                    <div class="wp-apply-row">
                        <label class="cal-filter-label">"Week of"</label>
                        <input
                            class="cal-filter-date"
                            type="date"
                            prop:value=move || a_week.get()
                            on:change=move |ev| a_week.set(event_target_value(&ev))
                        />
                        <span class="wp-week-title">{a_week_title}</span>
                        <label class="cal-filter-label">"for"</label>
                        <input
                            class="cal-filter-date wp-weeks"
                            type="number"
                            min="1"
                            max=MAX_APPLY_WEEKS.to_string()
                            prop:value=move || a_weeks.get()
                            on:change=move |ev| a_weeks.set(event_target_value(&ev))
                        />
                        <label class="cal-filter-label">"week(s)"</label>
                        <button
                            class="cal-btn cal-btn-primary"
                            disabled=move || busy.get() || a_week.with(String::is_empty)
                            on:click=move |_| {
                                let weeks = a_weeks
                                    .get_untracked()
                                    .trim()
                                    .parse::<u32>()
                                    .unwrap_or(1)
                                    .clamp(1, MAX_APPLY_WEEKS);
                                apply_week(a_week.get_untracked(), weeks)
                            }
                        >
                            "Apply"
                        </button>
                    </div>
                    <p class="cal-muted wp-hint">
                        "Applying again updates the plan's events in that week instead of \
                         duplicating them. Events you added by hand are never touched."
                    </p>

                    <h3 class="wp-h">"Applied weeks"</h3>
                    <Show
                        when=move || !apps.with(Vec::is_empty)
                        fallback=|| view! { <p class="cal-muted">"Not applied to any week yet."</p> }
                    >
                        <ul class="wp-apps">
                            {move || {
                                apps.get()
                                    .into_iter()
                                    .map(|a| {
                                        let d = daynum(&a.week_start).unwrap_or(today);
                                        let (w1, w2) = (a.week_start.clone(), a.week_start.clone());
                                        view! {
                                            <li class="wp-app">
                                                <span class="wp-app-title">{week_title(week_start(d))}</span>
                                                <span class="cal-muted">
                                                    {format!("{} event(s)", a.event_count)}
                                                </span>
                                                <span class="wp-app-acts">
                                                    <button class="cal-btn" on:click=move |_| on_show_week(d)>
                                                        "Show"
                                                    </button>
                                                    <button
                                                        class="cal-btn"
                                                        disabled=move || busy.get()
                                                        on:click=move |_| apply_week(w1.clone(), 1)
                                                    >
                                                        "Re-apply"
                                                    </button>
                                                    <button
                                                        class="cal-btn wp-danger"
                                                        disabled=move || busy.get()
                                                        on:click=move |_| unapply(w2.clone())
                                                    >
                                                        "Remove from week"
                                                    </button>
                                                </span>
                                            </li>
                                        }
                                    })
                                    .collect_view()
                            }}
                        </ul>
                    </Show>
                </section>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hhmm_pads() {
        assert_eq!(hhmm(0), "00:00");
        assert_eq!(hhmm(570), "09:30");
        assert_eq!(hhmm(1440), "24:00");
    }

    #[test]
    fn labels_split_and_trim() {
        assert_eq!(
            split_labels(" work, ,focus ,"),
            vec!["work".to_string(), "focus".to_string()]
        );
    }

    #[test]
    fn outcome_summary_sums_weeks() {
        let o = |c: usize, r: usize| WeekOutcome {
            week_start: "2026-10-05".into(),
            created: vec!["x".into(); c],
            updated: Vec::new(),
            unchanged: Vec::new(),
            removed: vec!["y".into(); r],
        };
        assert_eq!(
            outcome_summary(&[o(2, 0), o(3, 1)]),
            "Applied to 2 weeks: 5 created, 0 updated, 0 unchanged, 1 removed."
        );
    }
}
