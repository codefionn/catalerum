//! The Calendar panel (SOUL §8, §12 — M2 calendar view + connect form).
//!
//! Fetches `GET /events` and `GET /calendars` for the signed-in workspace and
//! renders the events as a day-grouped agenda (summary · time · location ·
//! calendar). A "Connect calendar" form `POST`s a new connection (a local `.ics`
//! directory or a CalDAV/webcal URL), kicks an incremental sync
//! (`POST /connections/{id}/sync`), and refreshes the agenda. The sidebar's
//! "Sources" list shows those connections and removes a wrongly-added one
//! (`DELETE /connections/{id}`, server-cascading its synced calendars + events).
//!
//! An optional **date-range filter** (From / To) scopes the agenda to a window
//! via `GET /events?from=&to=`. The agenda starts at today's local date by
//! default; clearing the filter restores the unbounded all-events list.
//!
//! All datetimes from the API are RFC 3339 / ISO-8601 UTC strings. Before
//! rendering, timed events are shifted into the browser's local timezone
//! ([`to_local_event`]) so the agenda, grids, and "now" line all read in the
//! user's wall clock; we then group and format by string slicing (no `chrono`
//! in the wasm bundle). All-day blocks stay pinned to their calendar date.

use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use crate::api::{
    Attachment, Calendar, CalendarProviderKind, Connection, CreateCalendar, CreateConnection,
    CreateEvent, Event, UpdateEvent,
};
use crate::auth;
use crate::components::icons::{Icon, MdIcon};
use crate::components::widgets::{
    attachment_href, attachment_is_image, attachment_label, is_safe_href, list_drawer_scrim,
    list_drawer_toggle, row_action, url_basename,
};
use crate::rest;

/// One day's worth of events in the agenda, in start order.
#[derive(Clone, Debug, PartialEq)]
struct DayGroup {
    /// The `YYYY-MM-DD` date key (local, post-[`to_local_event`]).
    date: String,
    /// Events starting on that date, ascending by start.
    events: Vec<Event>,
}

/// Reactive calendar metadata shared by the week/day renderer.
#[derive(Clone, Copy)]
struct PlannerCalendars {
    names: RwSignal<HashMap<String, String>>,
    all: RwSignal<Vec<Calendar>>,
}

/// An edit session for the event form: which event a save `PUT`s, the fields
/// the form doesn't surface (preserved and sent back verbatim), and the
/// prefilled datetime inputs — an untouched input sends its original stamp
/// back, so a title-only save never shifts an all-day event's date-pinned
/// midnight stamps (or drops seconds the minute-grain input can't express).
#[derive(Clone, Debug, PartialEq)]
struct EditingEvent {
    /// The event being edited.
    id: String,
    /// The stored start stamp, verbatim.
    orig_start: String,
    /// The stored end stamp, verbatim.
    orig_end: String,
    /// What the start `datetime-local` input was prefilled with.
    start_input: String,
    /// What the end `datetime-local` input was prefilled with.
    end_input: String,
    /// The stored all-day flag (the form has no all-day control).
    all_day: bool,
    /// The stored recurrence rule (the form has no rrule field).
    rrule: Option<String>,
}

/// Which calendar view the panel shows. The weekly planner (the default) is the
/// primary interactive surface; the agenda is the only
/// one with the From/To range filter; Month/Week/Day are grids driven by the
/// `anchor` day-number and the prev/next/today navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewMode {
    Agenda,
    Month,
    Week,
    Day,
}

impl ViewMode {
    /// The view modes in toolbar order.
    const ALL: [ViewMode; 4] = [
        ViewMode::Week,
        ViewMode::Day,
        ViewMode::Month,
        ViewMode::Agenda,
    ];

    /// Tab label.
    fn label(self) -> &'static str {
        match self {
            ViewMode::Agenda => "Agenda",
            ViewMode::Month => "Month",
            ViewMode::Week => "Week",
            ViewMode::Day => "Day",
        }
    }

    /// Parse a `/app/calendar/<slug>` sub-segment back into a view. Unknown or
    /// empty segments yield `None`, so the caller falls back to the default.
    fn from_slug(slug: &str) -> Option<ViewMode> {
        match slug.trim().to_ascii_lowercase().as_str() {
            "agenda" => Some(ViewMode::Agenda),
            "month" => Some(ViewMode::Month),
            "week" => Some(ViewMode::Week),
            "day" => Some(ViewMode::Day),
            _ => None,
        }
    }
}

/// The calendar panel's base frontend route. Each view is deep-linkable at
/// `<CALENDAR_ROUTE>/<slug>` (e.g. `/app/calendar/week`).
const CALENDAR_ROUTE: &str = "/app/calendar";

/// The view encoded in the current browser URL (`/app/calendar/<slug>`), if the
/// path carries a recognised view segment. Drives the initial `view_mode` so a
/// deep link or reload lands on the right view.
fn calendar_state_from_path(path: &str) -> Option<(ViewMode, Option<i64>)> {
    let route = path
        .trim_end_matches('/')
        .strip_prefix(CALENDAR_ROUTE)?
        .trim_start_matches('/');
    let mut segments = route.split('/');
    let view = ViewMode::from_slug(segments.next()?)?;
    let date = segments.next();
    if segments.next().is_some() {
        return None;
    }
    let anchor = match (view, date) {
        (ViewMode::Agenda, None) => None,
        (ViewMode::Month, Some(value)) => {
            let (year, month) = value.split_once('-')?;
            let year: i64 = year.parse().ok()?;
            let month: u32 = month.parse().ok()?;
            if !(1..=12).contains(&month) {
                return None;
            }
            Some(days_from_civil(year, month, 1))
        }
        (ViewMode::Week | ViewMode::Day, Some(value)) => {
            parse_ymd(value).map(|(year, month, day)| days_from_civil(year, month, day))
        }
        _ => return None,
    };
    Some((view, anchor))
}

fn calendar_state_from_location() -> Option<(ViewMode, Option<i64>)> {
    let path = web_sys::window()?.location().pathname().ok()?;
    calendar_state_from_path(&path)
}

/// Store the selected view and its focused period as a real history entry.
fn sync_location_to_calendar(view: ViewMode, anchor: i64) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let target = match view {
        ViewMode::Agenda => format!("{CALENDAR_ROUTE}/agenda"),
        ViewMode::Month => {
            let (year, month, _) = civil_from_days(anchor);
            format!("{CALENDAR_ROUTE}/month/{year:04}-{month:02}")
        }
        ViewMode::Week => format!("{CALENDAR_ROUTE}/week/{}", ymd_string(week_start(anchor))),
        ViewMode::Day => format!("{CALENDAR_ROUTE}/day/{}", ymd_string(anchor)),
    };
    if let Ok(current) = window.location().pathname() {
        if current.trim_end_matches('/') == target {
            return;
        }
    }
    if let Ok(history) = window.history() {
        let _ = history.push_state_with_url(&JsValue::NULL, "", Some(&target));
    }
}

/// The Calendar panel component.
#[component]
pub fn CalendarPanel() -> impl IntoView {
    // Loaded data.
    let events = RwSignal::new(Vec::<Event>::new());
    // calendar_id -> calendar name, for the per-event badge.
    let cal_names = RwSignal::new(HashMap::<String, String>::new());
    // Every calendar, in full — drives the event-form picker and per-event
    // deletability (only local, writable calendars can be edited here).
    let calendars = RwSignal::new(Vec::<Calendar>::new());
    // The remote calendar *sources* (CalDAV/webcal/local-ics connections) — the
    // "Sources" sidebar list, where a wrongly-added source can be removed.
    let connections = RwSignal::new(Vec::<Connection>::new());
    // The Sources section folds away (collapsed by default) — sources are
    // set-and-forget, so the list mostly wastes sidebar space. Client-side
    // only; resets to collapsed on reload.
    let sources_open = RwSignal::new(false);
    // Load / refresh state.
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);

    // Browser-local "today", shared by the agenda's default lower bound and the
    // Month/Week/Day anchor/highlight.
    let today = today_daynum();

    // Optional agenda date-range filter (`YYYY-MM-DD`, empty = no bound). The
    // agenda defaults to starting at today's local date; clearing the filter
    // restores the unbounded all-events view.
    let range_from = RwSignal::new(ymd_string(today));
    let range_to = RwSignal::new(String::new());

    // Sidebar filter: the set of *deactivated* calendar ids whose events are
    // hidden from the agenda. Tracking the hidden set (rather than the active
    // one) means a newly-synced calendar shows up active by default — no need to
    // seed this on every refresh. Purely client-side; resets on reload.
    let hidden = RwSignal::new(HashSet::<String>::new());

    // Connect-form state.
    let form_open = RwSignal::new(false);
    let form_kind = RwSignal::new(CalendarProviderKind::Local);
    let form_name = RwSignal::new(String::new());
    let form_target = RwSignal::new(String::new());
    let form_busy = RwSignal::new(false);
    let form_error = RwSignal::new(Option::<String>::None);
    let form_notice = RwSignal::new(Option::<String>::None);

    // New-local-calendar form state.
    let newcal_open = RwSignal::new(false);
    let newcal_name = RwSignal::new(String::new());
    let newcal_busy = RwSignal::new(false);
    let newcal_error = RwSignal::new(Option::<String>::None);

    // Add-event form state (writes to a local calendar).
    let event_open = RwSignal::new(false);
    let event_cal = RwSignal::new(String::new());
    let event_summary = RwSignal::new(String::new());
    let event_start = RwSignal::new(String::new());
    let event_end = RwSignal::new(String::new());
    let event_location = RwSignal::new(String::new());
    // Description (body), comma-separated labels, and the staged attachments
    // (URL-added or uploaded) for the new event.
    let event_body = RwSignal::new(String::new());
    let event_labels = RwSignal::new(String::new());
    let event_attachments = RwSignal::new(Vec::<Attachment>::new());
    let event_attach_url = RwSignal::new(String::new());
    // Count of attachment uploads currently in flight (not a single bool) so that
    // selecting several files at once uploads them all concurrently.
    let event_uploads = RwSignal::new(0usize);
    let event_busy = RwSignal::new(false);
    let event_error = RwSignal::new(Option::<String>::None);
    // When `Some`, the event form is editing that existing event (a save
    // `PUT`s a replacement) instead of creating a new one.
    let editing = RwSignal::new(Option::<EditingEvent>::None);

    // Agenda label filter: the set of selected labels (stored lowercased);
    // only events carrying at least one of them show (empty = "All"). Purely
    // client-side over the loaded events, like the calendar sidebar filter;
    // labels that disappear from the loaded events are pruned by the effect
    // next to `bar_labels` below.
    let label_filter = RwSignal::new(Vec::<String>::new());

    // --- Grid view state ---------------------------------------------------
    // Which view the panel shows. The weekly planner is the default; the agenda
    // is the only one with the From/To filter. Month/Week/Day are calendar grids driven by the
    // `anchor` day-number below. Navigation and the view itself work entirely on
    // the already-loaded, in-memory event list — no refetch on navigate.
    let initial_url_state = calendar_state_from_location();
    let view_mode = RwSignal::new(
        initial_url_state
            .map(|state| state.0)
            .unwrap_or(ViewMode::Week),
    );
    // The focused date as a day-number (days since 1970-01-01, UTC). Prev/next
    // step it by a month / week / day depending on `view_mode`; "Today" resets
    // it. Computed once from the browser-local date so the grids stay aligned
    // with local-wall-clock agenda grouping.
    let anchor = RwSignal::new(initial_url_state.and_then(|state| state.1).unwrap_or(today));
    Effect::new(move |_| sync_location_to_calendar(view_mode.get(), anchor.get()));
    {
        let on_popstate = Closure::<dyn FnMut(web_sys::Event)>::wrap(Box::new(move |_| {
            if let Some((view, url_anchor)) = calendar_state_from_location() {
                view_mode.set(view);
                if let Some(url_anchor) = url_anchor {
                    anchor.set(url_anchor);
                }
            }
        }));
        if let Some(window) = web_sys::window() {
            let _ = window
                .add_event_listener_with_callback("popstate", on_popstate.as_ref().unchecked_ref());
            // Retain the browser callback for the workbench lifetime, matching
            // the shell's own route listener.
            on_popstate.forget();
        }
    }

    // The current local time as `(day-number, minutes-from-midnight)`, re-read on a
    // 30-second timer so the Week/Day "now" line tracks the wall clock without a
    // reload. Local time stays aligned with the grids' local-wall-clock event
    // positioning. Guarded on change so the grid re-renders at most once a
    // minute, and the interval is torn down when the panel unmounts.
    let now = RwSignal::new(now_daynum_min());
    if let Ok(handle) = set_interval_with_handle(
        move || {
            let n = now_daynum_min();
            if now.get_untracked() != n {
                now.set(n);
            }
        },
        std::time::Duration::from_secs(30),
    ) {
        on_cleanup(move || handle.clear());
    }

    // The local, writable calendars the event form can target.
    let writable_calendars = move || {
        calendars
            .get()
            .into_iter()
            .filter(Calendar::is_writable)
            .collect::<Vec<_>>()
    };

    // Sidebar filter helpers. A calendar is *active* when it is not in the
    // hidden set; toggling flips that membership. All closures capture only the
    // `Copy` `hidden`/`calendars` signals, so they stay `Copy` and compose into
    // the per-row event handlers.
    let is_active = move |id: &str| !hidden.with(|h| h.contains(id));
    let toggle_cal = move |id: String| {
        // `remove` reports whether the id was present: if it was, the calendar
        // becomes visible again; otherwise we hide it.
        hidden.update(|h| {
            if !h.remove(&id) {
                h.insert(id);
            }
        });
    };
    let show_all = move || hidden.set(HashSet::new());
    let hide_all =
        move || hidden.set(calendars.with(|cs| cs.iter().map(|c| c.id.clone()).collect()));
    // How many calendars are currently active (for the sidebar summary).
    let active_count = move || {
        calendars.with(|cs| hidden.with(|h| cs.iter().filter(|c| !h.contains(&c.id)).count()))
    };

    // Fetch calendars + events and fold them into the signals. Shared by the
    // initial load and the post-mutation refreshes.
    let refresh = move || {
        loading.set(true);
        load_error.set(None);

        // Build the optional `[from, to]` window from the date filter. The `to`
        // date is taken inclusive of that whole day (the API's upper bound is
        // exclusive, so use end-of-day). A reversed range is a user error —
        // surface it and skip the query rather than 400 against the API.
        let from_raw = range_from.get_untracked();
        let to_raw = range_to.get_untracked();
        if !from_raw.is_empty() && !to_raw.is_empty() && from_raw > to_raw {
            load_error.set(Some("“From” must not be after “To”.".to_string()));
            loading.set(false);
            return;
        }
        let from_q = (!from_raw.is_empty()).then(|| format!("{from_raw}T00:00:00Z"));
        let to_q = (!to_raw.is_empty()).then(|| format!("{to_raw}T23:59:59Z"));

        spawn_local(async move {
            let token = auth::resolve_token();
            let tok = token.as_deref();

            // Calendars first (best-effort: a failure here only drops the name
            // badges + the event picker, not the agenda).
            match rest::list_calendars(tok).await {
                Ok(cals) => {
                    let map = cals
                        .iter()
                        .map(|c| (c.id.clone(), c.name.clone()))
                        .collect::<HashMap<_, _>>();
                    cal_names.set(map);
                    calendars.set(cals);
                }
                Err(_) => {
                    cal_names.set(HashMap::new());
                    calendars.set(Vec::new());
                }
            }

            // Sources (best-effort, like calendars): the `/connections` list is
            // workspace-wide, so keep only the calendar ones — email sources are
            // managed in Settings.
            match rest::list_connections(tok).await {
                Ok(conns) => {
                    connections.set(conns.into_iter().filter(|c| c.kind == "calendar").collect())
                }
                Err(_) => connections.set(Vec::new()),
            }

            // Unfiltered → the plain list; a window → the filtered query.
            let evs_result = match (from_q.as_deref(), to_q.as_deref()) {
                (None, None) => rest::list_events(tok).await,
                (f, t) => rest::list_events_filtered(tok, f, t).await,
            };
            match evs_result {
                Ok(mut evs) => {
                    // Defensive: the API already orders by start, but make the
                    // agenda independent of that guarantee.
                    evs.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
                    events.set(evs);
                    load_error.set(None);
                }
                Err(e) => {
                    events.set(Vec::new());
                    load_error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    // Initial load.
    refresh();

    // Submit the connect form: create the connection, enqueue a sync, refresh.
    let submit_connect = move || {
        if form_busy.get_untracked() {
            return;
        }
        let kind = form_kind.get_untracked();
        let name = form_name.get_untracked().trim().to_string();
        let target = form_target.get_untracked().trim().to_string();

        form_error.set(None);
        form_notice.set(None);
        if name.is_empty() {
            form_error.set(Some("Give the connection a name.".to_string()));
            return;
        }
        if target.is_empty() {
            let what = if kind.is_local() {
                "a directory path"
            } else {
                "a URL"
            };
            form_error.set(Some(format!("Enter {what}.")));
            return;
        }

        form_busy.set(true);
        let body = CreateConnection::new(kind, name, target);
        spawn_local(async move {
            let token = auth::resolve_token();
            let tok = token.as_deref();
            match rest::create_connection(tok, &body).await {
                Ok(conn) => {
                    // Best-effort sync kick; surface but don't fail the create.
                    let synced = rest::sync_connection(tok, &conn.id).await;
                    form_busy.set(false);
                    form_target.set(String::new());
                    form_name.set(String::new());
                    form_open.set(false);
                    match synced {
                        Ok(_) => form_notice.set(Some(format!(
                            "Connected “{}”. Syncing… events will appear shortly.",
                            conn.name
                        ))),
                        Err(e) => form_notice.set(Some(format!(
                            "Connected “{}”, but the sync could not be queued: {e}",
                            conn.name
                        ))),
                    }
                    refresh();
                }
                Err(e) => {
                    form_busy.set(false);
                    form_error.set(Some(e.to_string()));
                }
            }
        });
    };

    // Submit the new-local-calendar form: create it, pre-select it for the
    // event form, refresh.
    let submit_new_calendar = move || {
        if newcal_busy.get_untracked() {
            return;
        }
        let name = newcal_name.get_untracked().trim().to_string();
        newcal_error.set(None);
        if name.is_empty() {
            newcal_error.set(Some("Give the calendar a name.".to_string()));
            return;
        }
        newcal_busy.set(true);
        let assigning_to_event = event_open.get_untracked();
        let body = CreateCalendar { name };
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::create_calendar(token.as_deref(), &body).await {
                Ok(cal) => {
                    let created_name = cal.name.clone();
                    newcal_busy.set(false);
                    newcal_name.set(String::new());
                    newcal_open.set(false);
                    event_cal.set(cal.id);
                    if assigning_to_event {
                        form_notice.set(Some(format!(
                            "Created “{created_name}” and assigned this event to it."
                        )));
                        scroll_event_editor_into_view();
                    }
                    refresh();
                }
                Err(e) => {
                    newcal_busy.set(false);
                    newcal_error.set(Some(e.to_string()));
                }
            }
        });
    };

    // Clear the event form back to a fresh create state: fields, staged
    // attachments, and any edit session.
    let reset_event_form = move || {
        event_summary.set(String::new());
        event_start.set(String::new());
        event_end.set(String::new());
        event_location.set(String::new());
        event_body.set(String::new());
        event_labels.set(String::new());
        event_attachments.set(Vec::new());
        event_attach_url.set(String::new());
        event_error.set(None);
        editing.set(None);
    };

    // Open a fresh event directly from the planner. Times are snapped to a
    // quarter-hour and default to a one-hour block; `datetime-local` values are
    // deliberately browser-local, matching the grid's wall-clock rendering.
    let open_create_at = move |day: i64, minute: i64| {
        reset_event_form();
        if event_cal.get_untracked().is_empty() {
            if let Some(first) = writable_calendars().first() {
                event_cal.set(first.id.clone());
            }
        }
        let snapped = (minute.clamp(0, 1425) / 15) * 15;
        event_start.set(datetime_local_value(day, snapped));
        event_end.set(datetime_local_value(day, snapped + 60));
        event_open.set(true);
        scroll_event_editor_into_view();
    };

    // Submit the event form: validate, then POST /events (create) or
    // PUT /events/{id} (edit session), and refresh.
    let submit_event = move || {
        if event_busy.get_untracked() {
            return;
        }
        // Don't save while an attachment is still uploading — it isn't staged into
        // `event_attachments` until the upload resolves, so it would be dropped.
        if event_uploads.get_untracked() > 0 {
            event_error.set(Some("Wait for the upload to finish.".to_string()));
            return;
        }
        let calendar_id = event_cal.get_untracked();
        let summary = event_summary.get_untracked().trim().to_string();
        let location = event_location.get_untracked().trim().to_string();
        event_error.set(None);
        if calendar_id.is_empty() {
            event_error.set(Some("Pick a calendar.".to_string()));
            return;
        }
        if summary.is_empty() {
            event_error.set(Some("Give the event a title.".to_string()));
            return;
        }
        let edit = editing.get_untracked();
        // An all-day endpoint marks a calendar *date*, not an instant: its input
        // was prefilled with the stored date unshifted (see `start_edit`), and
        // the store pins it to midnight UTC of that date. So a *changed* all-day
        // input is converted date-only ([`all_day_input_to_rfc3339`], no zone
        // shift) — running it through the local→UTC path would slide the date by
        // the browser's offset. Timed inputs take the local→UTC conversion.
        let all_day_edit = edit.as_ref().is_some_and(|e| e.all_day);
        let to_stamp = |value: &str| {
            if all_day_edit {
                all_day_input_to_rfc3339(value)
            } else {
                local_input_to_rfc3339(value)
            }
        };
        let (Some(mut start), Some(mut end)) = (
            to_stamp(&event_start.get_untracked()),
            to_stamp(&event_end.get_untracked()),
        ) else {
            event_error.set(Some("Enter a start and end time.".to_string()));
            return;
        };
        // In an edit, an untouched datetime input sends its original stamp
        // back verbatim (see [`EditingEvent`]) — only a changed input goes
        // through the conversion above.
        if let Some(ed) = &edit {
            if event_start.get_untracked() == ed.start_input {
                start = ed.orig_start.clone();
            }
            if event_end.get_untracked() == ed.end_input {
                end = ed.orig_end.clone();
            }
        }
        // Both stamps share the `YYYY-MM-DDTHH:MM:SSZ` shape, so a lexical
        // compare is a chronological one.
        if end < start {
            event_error.set(Some("End must not precede start.".to_string()));
            return;
        }
        let description = event_body.get_untracked().trim().to_string();
        let labels = parse_labels(&event_labels.get_untracked());
        let attachments = event_attachments.get_untracked();
        event_busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result = match &edit {
                // Edit: PUT the replacement. `all_day`/`rrule` aren't form
                // fields, so the session's stored values ride back unchanged
                // (the server clears an absent rrule).
                Some(ed) => rest::update_event(
                    token.as_deref(),
                    &ed.id,
                    &UpdateEvent {
                        summary,
                        start,
                        end,
                        all_day: ed.all_day,
                        location: (!location.is_empty()).then_some(location),
                        body: (!description.is_empty()).then_some(description),
                        rrule: ed.rrule.clone(),
                        labels,
                        attachments,
                    },
                )
                .await
                .map(|_| ()),
                None => rest::create_event(
                    token.as_deref(),
                    &CreateEvent {
                        calendar_id,
                        summary,
                        start,
                        end,
                        all_day: false,
                        location: (!location.is_empty()).then_some(location),
                        body: (!description.is_empty()).then_some(description),
                        labels,
                        attachments,
                    },
                )
                .await
                .map(|_| ()),
            };
            match result {
                Ok(()) => {
                    event_busy.set(false);
                    reset_event_form();
                    event_open.set(false);
                    refresh();
                }
                Err(e) => {
                    event_busy.set(false);
                    event_error.set(Some(e.to_string()));
                }
            }
        });
    };

    // Stage an attachment from a pasted URL: derive a display filename + a
    // best-effort content type from the URL, then append it to the list.
    let add_attachment_url = move || {
        let raw = event_attach_url.get_untracked();
        let url = raw.trim().to_string();
        if url.is_empty() {
            return;
        }
        let filename = url_basename(&url);
        let content_type = guess_content_type(&filename);
        event_attachments.update(|list| {
            list.push(Attachment {
                url,
                filename: (!filename.is_empty()).then_some(filename),
                content_type,
                size: None,
            });
        });
        event_attach_url.set(String::new());
    };

    // Stage an attachment from a picked file: upload its bytes to object storage
    // (a unique `events/<ts>-<rand>-<name>` key, auto-provisioning a storage bucket
    // server-side), then append an attachment pointing at the stored object's
    // download path. Uploads are counted in `event_uploads` (not single-flighted),
    // so picking several files at once uploads them all — and the key's random
    // component keeps same-millisecond keys distinct.
    let add_attachment_file = move |file: web_sys::File| {
        event_uploads.update(|n| *n += 1);
        event_error.set(None);
        spawn_local(async move {
            match read_file(file).await {
                Ok((name, ctype, bytes)) => {
                    let size = bytes.len() as u64;
                    let key = upload_key(&name);
                    let token = auth::resolve_token();
                    // Event attachments go to the default store.
                    let result =
                        rest::upload_object(token.as_deref(), &key, None, bytes, ctype.as_deref())
                            .await;
                    event_uploads.update(|n| *n = n.saturating_sub(1));
                    match result {
                        Ok(()) => event_attachments.update(|list| {
                            list.push(Attachment {
                                url: format!("/storage/objects/{key}"),
                                filename: (!name.is_empty()).then_some(name),
                                content_type: ctype,
                                size: Some(size),
                            });
                        }),
                        Err(e) => event_error.set(Some(format!("upload failed: {e}"))),
                    }
                }
                Err(e) => {
                    event_uploads.update(|n| *n = n.saturating_sub(1));
                    event_error.set(Some(e));
                }
            }
        });
    };

    // `<input type=file>` change handler: stage every picked file, then clear the
    // input so re-picking the same file re-fires.
    let on_attach_file_change = move |ev: leptos::ev::Event| {
        let Some(target) = ev.target() else {
            return;
        };
        let Ok(input) = target.dyn_into::<web_sys::HtmlInputElement>() else {
            return;
        };
        if let Some(files) = input.files() {
            for i in 0..files.length() {
                if let Some(file) = files.get(i) {
                    add_attachment_file(file);
                }
            }
        }
        input.set_value("");
    };

    // Remove a staged attachment by index.
    let remove_attachment = move |idx: usize| {
        event_attachments.update(|list| {
            if idx < list.len() {
                list.remove(idx);
            }
        });
    };

    // Delete an event (only offered for events on a local, writable calendar),
    // then refresh. `refresh` is a `Copy` closure, so it composes into this one.
    let delete_event = move |id: String| {
        spawn_local(async move {
            let token = auth::resolve_token();
            if rest::delete_event(token.as_deref(), &id).await.is_ok() {
                refresh();
            }
        });
    };

    // Open the event form pre-filled to edit an existing event (the agenda's ✎;
    // same writable gate as delete). Prefills from the RAW (UTC) event in
    // `events` — not the agenda's local-shifted copy — so the edit session
    // keeps the stored stamps verbatim. Timed events prefill the inputs in
    // local wall clock (`local_input_to_rfc3339` inverts exactly that);
    // all-day blocks prefill their date-pinned stamps unshifted, mirroring
    // `to_local_event`.
    let start_edit = move |id: String| {
        let Some(raw) = events.with_untracked(|evs| evs.iter().find(|e| e.id == id).cloned())
        else {
            return;
        };
        let input_of = |ts: &str| {
            let wall = if is_all_day_block(&raw) {
                ts.to_string()
            } else {
                utc_to_local_wall(ts)
            };
            wall.get(..16).unwrap_or_default().to_string()
        };
        let start_input = input_of(&raw.start);
        let end_input = input_of(&raw.end);
        event_cal.set(raw.calendar_id.clone());
        event_summary.set(raw.summary.clone());
        event_start.set(start_input.clone());
        event_end.set(end_input.clone());
        event_location.set(raw.location.clone().unwrap_or_default());
        event_body.set(raw.body.clone().unwrap_or_default());
        event_labels.set(raw.labels.join(", "));
        event_attachments.set(raw.attachments.clone());
        event_attach_url.set(String::new());
        event_error.set(None);
        editing.set(Some(EditingEvent {
            id,
            orig_start: raw.start.clone(),
            orig_end: raw.end.clone(),
            start_input,
            end_input,
            all_day: raw.all_day,
            rrule: raw.rrule.clone(),
        }));
        event_open.set(true);
        // The form lives at the panel's top — bring it on-screen for an event
        // clicked far down the agenda or time grid.
        scroll_event_editor_into_view();
    };

    // Remove a calendar source (server-cascades its synced calendars + events);
    // a full `refresh` then drops the now-orphaned calendars + events too.
    let delete_connection = move |id: String| {
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::delete_connection(token.as_deref(), &id).await {
                Ok(()) => refresh(),
                Err(e) => load_error.set(Some(e.to_string())),
            }
        });
    };

    // Delete a calendar (and its events). A local calendar is simply removed; a
    // synced one is removed *and* server-side excluded so the next sync won't
    // re-add it (`DELETE /calendars/{id}`; re-adding the source brings it back).
    // `refresh` drops the calendar and its events from the agenda + sidebar.
    let delete_calendar = move |id: String| {
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::delete_calendar(token.as_deref(), &id).await {
                Ok(()) => refresh(),
                Err(e) => load_error.set(Some(e.to_string())),
            }
        });
    };

    // Derived: the events on currently-active calendars, shifted into the
    // browser's local timezone (so every view reads in the user's wall clock).
    // Hiding a calendar in the sidebar drops its events from the agenda.
    let visible_events = move || {
        // The label filter is an agenda-only control (its chip bar shows only in
        // Agenda), so it must not silently filter the Month/Week/Day grids.
        let active_labels = if view_mode.get() == ViewMode::Agenda {
            label_filter.get()
        } else {
            Vec::new()
        };
        let mut out = events.with(|evs| {
            hidden.with(|h| {
                evs.iter()
                    .filter(|e| !h.contains(&e.calendar_id))
                    .filter(|e| {
                        // Empty selection = no label filtering; otherwise an
                        // event shows when it carries *any* selected label.
                        active_labels.is_empty()
                            || active_labels.iter().any(|l| event_has_label(e, l))
                    })
                    .map(to_local_event)
                    .collect::<Vec<_>>()
            })
        });
        // Sort *after* the local-time shift: the fetch's UTC-order sort is not
        // day-key monotonic once timed events shift across midnight while
        // all-day events stay pinned (negative UTC offsets), which would split
        // a day into duplicate agenda groups and break interleaving.
        out.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
        out
    };
    let groups = move || group_by_day(&visible_events());

    // Chip-bar source: the distinct labels (with per-label event counts) on
    // currently-active calendars, so the bar and its counts mirror what the
    // agenda can actually show; hiding a calendar drops its labels too.
    let bar_labels = move || {
        events.with(|evs| {
            hidden.with(|h| label_counts(evs.iter().filter(|e| !h.contains(&e.calendar_id))))
        })
    };
    // The "All" chip's count: every event on an active calendar.
    let bar_total = move || {
        events
            .with(|evs| hidden.with(|h| evs.iter().filter(|e| !h.contains(&e.calendar_id)).count()))
    };
    // Keep the selection honest: when a selected label no longer appears on
    // any active-calendar event (reload, calendar hidden), drop it — otherwise
    // a chip that vanished from the bar would keep filtering forever.
    Effect::new(move |_| {
        let have: HashSet<String> = bar_labels()
            .into_iter()
            .map(|(l, _)| l.to_lowercase())
            .collect();
        let stale = label_filter.with_untracked(|sel| sel.iter().any(|l| !have.contains(l)));
        if stale {
            label_filter.update(|sel| sel.retain(|l| have.contains(l)));
        }
    });

    // --- Grid navigation ---------------------------------------------------
    // Prev/next step the anchor by the active view's unit (month → calendar
    // month, week → 7 days, day → 1 day); "Today" recenters on today. All
    // capture only `Copy` signals, so they compose into the button handlers.
    let is_agenda = move || view_mode.get() == ViewMode::Agenda;
    let go_today = move || anchor.set(today);
    let go_prev = move || {
        anchor.update(|a| {
            *a = match view_mode.get_untracked() {
                ViewMode::Month => add_months(*a, -1),
                ViewMode::Week => *a - 7,
                _ => *a - 1,
            };
        });
    };
    let go_next = move || {
        anchor.update(|a| {
            *a = match view_mode.get_untracked() {
                ViewMode::Month => add_months(*a, 1),
                ViewMode::Week => *a + 7,
                _ => *a + 1,
            };
        });
    };
    // The current range, as a heading next to the nav arrows.
    let nav_title = move || match view_mode.get() {
        ViewMode::Agenda => String::new(),
        ViewMode::Month => {
            let (y, m, _) = civil_from_days(anchor.get());
            format!("{} {y}", month_name(m))
        }
        ViewMode::Week => week_title(week_start(anchor.get())),
        ViewMode::Day => format_day_heading(&ymd_string(anchor.get())),
    };

    // Connect-form derived labels/placeholders (kind-dependent).
    let target_label = move || {
        if form_kind.get().is_local() {
            "Directory path"
        } else {
            "Calendar URL"
        }
    };
    let target_placeholder = move || match form_kind.get() {
        CalendarProviderKind::Local => "/srv/calendars",
        CalendarProviderKind::Caldav => "https://dav.example.com/calendars/me/",
        CalendarProviderKind::Webcal => "https://example.com/feed.ics",
    };
    let on_connect_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        submit_connect();
    };
    let on_newcal_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        submit_new_calendar();
    };
    let on_event_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        submit_event();
    };

    // Whether the calendars sidebar is open as a mobile drawer (SOUL §12); inert
    // on desktop. The agenda/grid is the always-visible detail pane.
    let list_open = RwSignal::new(false);

    view! {
        <div class="pane-split">
        {list_drawer_scrim(list_open)}
        // --- Sidebar: activate / deactivate calendars --------------------
        <aside class="pane-list cal-sidebar list-drawer" class:list-drawer-open=move || list_open.get()>
            <header class="pane-list-header">
                <h3 class="pane-list-title">"Calendars"</h3>
                <Show
                    when=move || !calendars.with(Vec::is_empty)
                    fallback=|| ().into_view()
                >
                    <span class="cal-sidebar-count">
                        {move || format!("{}/{}", active_count(), calendars.with(Vec::len))}
                    </span>
                </Show>
            </header>
            <div class="pane-list-body cal-sidebar-body">
                <Show
                    when=move || calendars.with(Vec::is_empty)
                    fallback=|| ().into_view()
                >
                    <p class="pane-list-status">
                        "No calendars yet — connect or create one to get started."
                    </p>
                </Show>
                <ul class="cal-cal-list">
                    <For
                        each=move || calendars.get()
                        key=|c| c.id.clone()
                        children=move |c: Calendar| {
                            let id_checked = c.id.clone();
                            let id_toggle = c.id.clone();
                            let id_name = c.id.clone();
                            let id_del = c.id.clone();
                            let color = cal_color(&c.id);
                            // Both local and synced calendars can be deleted here.
                            // Deleting a synced one drops it *and* records a
                            // server-side exclusion so the next sync won't re-add
                            // it (re-adding the source brings it back); a local one
                            // is simply gone. The tooltip spells out the difference.
                            let is_local = c.is_local();
                            let del_title = if is_local {
                                "Delete this local calendar (and its events)"
                            } else {
                                "Delete this synced calendar — removes it and its \
                                 events and stops re-syncing (re-add the source to \
                                 bring it back)"
                            };
                            view! {
                                <li class="cal-cal-item">
                                    <label class="cal-toggle">
                                        <input
                                            class="cal-toggle-box"
                                            type="checkbox"
                                            prop:checked=move || is_active(&id_checked)
                                            on:change=move |_| toggle_cal(id_toggle.clone())
                                        />
                                        <span
                                            class="cal-calendar-dot"
                                            aria-hidden="true"
                                            style=format!("background:{color}")
                                        ></span>
                                        <span
                                            class="cal-cal-name"
                                            class:cal-cal-off=move || !is_active(&id_name)
                                        >
                                            {c.name}
                                        </span>
                                    </label>
                                    <div class="row-acts row-acts-reveal">
                                        {row_action(
                                            MdIcon::Delete,
                                            del_title,
                                            true,
                                            move || delete_calendar(id_del.clone()),
                                        )}
                                    </div>
                                </li>
                            }
                        }
                    />
                </ul>
            </div>
            <Show
                when=move || !calendars.with(Vec::is_empty)
                fallback=|| ().into_view()
            >
                <footer class="cal-sidebar-actions">
                    <button class="cal-sidebar-link" on:click=move |_| show_all()>
                        "Show all"
                    </button>
                    <button class="cal-sidebar-link" on:click=move |_| hide_all()>
                        "Hide all"
                    </button>
                </footer>
            </Show>

            // --- Sources: the remote connections feeding the calendars -------
            <Show
                when=move || !connections.with(Vec::is_empty)
                fallback=|| ().into_view()
            >
                <div class="cal-sources">
                    <button
                        class="cal-sources-header"
                        type="button"
                        title="Expand / collapse the source list"
                        on:click=move |_| sources_open.update(|o| *o = !*o)
                    >
                        <span class="cal-sources-arrow">
                            {move || if sources_open.get() { "▾" } else { "▸" }}
                        </span>
                        <h3 class="pane-list-title cal-sources-title">"Sources"</h3>
                        <span class="cal-sources-count">
                            {move || connections.with(Vec::len)}
                        </span>
                        // A dormant source's inline "Idle" warning is hidden
                        // while collapsed — surface a marker so the trap stays
                        // visible without expanding.
                        <Show
                            when=move || connections.with(|cs| cs.iter().any(|c| !c.collecting))
                            fallback=|| ().into_view()
                        >
                            <span
                                class="cal-sources-warn"
                                title="A source is idle — nothing collects from it yet. Expand for details."
                            >
                                <Icon icon=MdIcon::Warning />
                            </span>
                        </Show>
                    </button>
                    <Show when=move || sources_open.get() fallback=|| ().into_view()>
                    <ul class="cal-source-list">
                        <For
                            each=move || connections.get()
                            key=|c| c.id.clone()
                            children=move |c: Connection| {
                                let synced = c.cursor.is_some();
                                // A source no enabled Collect automation references is
                                // *dormant* — configured but nothing ingests from it
                                // (SOUL §29). Warn inline so the "I added it but see
                                // nothing" trap is no longer silent.
                                let dormant = !c.collecting;
                                let id_del = c.id.clone();
                                view! {
                                    <li class="cal-source">
                                        <div class="cal-source-row">
                                            <span class="cal-source-name" title=c.name.clone()>
                                                {c.name.clone()}
                                            </span>
                                            <span
                                                class="cal-source-state"
                                                class:cal-source-synced=synced
                                            >
                                                {if synced { "synced" } else { "pending…" }}
                                            </span>
                                            {row_action(
                                                MdIcon::Delete,
                                                "Remove this source (and its synced calendars + events)",
                                                true,
                                                move || delete_connection(id_del.clone()),
                                            )}
                                        </div>
                                        <Show when=move || dormant fallback=|| ().into_view()>
                                            <p
                                                class="cal-source-idle"
                                                title="Add a Collect calendar automation (a CollectCalendar trigger pointing at this source) to start ingesting its events."
                                            >
                                                "Idle — nothing collects from this source yet. Create a Collect calendar automation to ingest its events."
                                            </p>
                                        </Show>
                                    </li>
                                }
                            }
                        />
                    </ul>
                    </Show>
                </div>
            </Show>
        </aside>

        {list_drawer_toggle("Calendars", list_open)}
        <section class="cal-panel" class:cal-panel-agenda=move || is_agenda()>
            <header class="cal-header">
                <div class="cal-header-titles">
                    <h2 class="cal-title">"Weekly planner"</h2>
                    <span class="cal-subtitle">
                        {move || match view_mode.get() {
                            ViewMode::Week => "Click a time slot to plan your week",
                            ViewMode::Day => "Focus on one day at a time",
                            ViewMode::Month => "See the shape of your month",
                            ViewMode::Agenda => "Your events, grouped by day",
                        }}
                    </span>
                </div>
                <div class="cal-header-actions">
                    <button
                        class="cal-btn"
                        disabled=move || loading.get()
                        on:click=move |_| refresh()
                    >
                        {move || if loading.get() { "Refreshing…" } else { "Refresh" }}
                    </button>
                    <button
                        class="cal-btn cal-btn-primary"
                        on:click=move |_| {
                            if !event_open.get() {
                                // From a dated view, create on the focused day.
                                // Today starts at the next quarter-hour; a future
                                // date uses a calm 09:00 default.
                                let day = if is_agenda() { today } else { anchor.get_untracked() };
                                let minute = if day == now.get_untracked().0 {
                                    ((now.get_untracked().1 + 14) / 15 * 15).min(1425)
                                } else {
                                    9 * 60
                                };
                                open_create_at(day, minute);
                            } else {
                                if editing.with_untracked(Option::is_some) {
                                    // Closing an edit session: clear it so the next
                                    // open is a fresh create form, not a stale edit.
                                    reset_event_form();
                                }
                                event_open.set(false);
                            }
                        }
                    >
                        {move || if event_open.get() { "Close" } else { "Add event" }}
                    </button>
                    <button
                        class="cal-btn"
                        on:click=move |_| {
                            newcal_error.set(None);
                            newcal_open.update(|o| *o = !*o);
                        }
                    >
                        {move || if newcal_open.get() { "Close" } else { "New calendar" }}
                    </button>
                    <button
                        class="cal-btn"
                        on:click=move |_| {
                            form_error.set(None);
                            form_open.update(|o| *o = !*o);
                        }
                    >
                        {move || if form_open.get() { "Close" } else { "Connect calendar" }}
                    </button>
                </div>
            </header>

            // --- View switcher + grid navigation -----------------------------
            <div class="cal-viewbar">
                <div class="cal-viewtabs">
                    {ViewMode::ALL
                        .into_iter()
                        .map(|vm| {
                            let active = move || view_mode.get() == vm;
                            view! {
                                <button
                                    class="cal-viewtab"
                                    class:cal-viewtab-on=active
                                    on:click=move |_| view_mode.set(vm)
                                >
                                    {vm.label()}
                                </button>
                            }
                        })
                        .collect::<Vec<_>>()}
                </div>
                <Show when=move || !is_agenda() fallback=|| ().into_view()>
                    // Title first, buttons last: the button cluster pins to the
                    // group's right edge so its screen position stays fixed as the
                    // title width changes across month/week/day views. Otherwise a
                    // wider title (e.g. the day heading) shoves the buttons left.
                    <div class="cal-nav">
                        <span class="cal-nav-title">{nav_title}</span>
                        <button
                            class="cal-nav-btn"
                            title="Previous"
                            on:click=move |_| go_prev()
                        >
                            "‹"
                        </button>
                        <button class="cal-nav-today" on:click=move |_| go_today()>
                            "Today"
                        </button>
                        <button class="cal-nav-btn" title="Next" on:click=move |_| go_next()>
                            "›"
                        </button>
                    </div>
                </Show>
            </div>

            // --- Agenda date-range filter (optional; empty = all events) -----
            <Show when=move || is_agenda() fallback=|| ().into_view()>
            <div class="cal-filter">
                <label class="cal-filter-label">"From"</label>
                <input
                    class="cal-filter-date"
                    type="date"
                    prop:value=move || range_from.get()
                    on:change=move |ev| {
                        range_from.set(event_target_value(&ev));
                        refresh();
                    }
                />
                <label class="cal-filter-label">"To"</label>
                <input
                    class="cal-filter-date"
                    type="date"
                    prop:value=move || range_to.get()
                    on:change=move |ev| {
                        range_to.set(event_target_value(&ev));
                        refresh();
                    }
                />
                <Show
                    when=move || !range_from.get().is_empty() || !range_to.get().is_empty()
                    fallback=|| ().into_view()
                >
                    <button
                        class="cal-filter-clear"
                        title="Clear the date filter"
                        on:click=move |_| {
                            range_from.set(String::new());
                            range_to.set(String::new());
                            refresh();
                        }
                    >
                        "Clear"
                    </button>
                </Show>
            </div>
            </Show>

            // --- Agenda label filter (chips; shown only when labels exist) ----
            // Multi-select: each chip toggles independently (events matching
            // *any* active chip show); "All" clears the selection.
            <Show
                when=move || is_agenda() && !bar_labels().is_empty()
                fallback=|| ().into_view()
            >
                <div class="cal-labelbar" role="group" aria-label="Filter by label">
                    <button
                        class="cal-label-chip"
                        class:cal-label-chip-on=move || label_filter.with(Vec::is_empty)
                        aria-pressed=move || label_filter.with(|f| f.is_empty()).to_string()
                        title="Show events regardless of label"
                        on:click=move |_| label_filter.set(Vec::new())
                    >
                        "All"
                        <span class="cal-label-count">{move || bar_total()}</span>
                    </button>
                    <For
                        each=move || bar_labels()
                        // Key on the count too, so a count change re-renders
                        // the chip (children receive plain values, not signals).
                        key=|(l, n)| format!("{l}:{n}")
                        children=move |(l, n): (String, usize)| {
                            let key = l.to_lowercase();
                            let active = {
                                let key = key.clone();
                                move || label_filter.with(|f| f.contains(&key))
                            };
                            let pressed = {
                                let key = key.clone();
                                move || label_filter.with(|f| f.contains(&key)).to_string()
                            };
                            let title = format!("Toggle the #{l} filter");
                            view! {
                                <button
                                    class="cal-label-chip"
                                    class:cal-label-chip-on=active
                                    aria-pressed=pressed
                                    title=title
                                    on:click=move |_| {
                                        label_filter.update(|f| {
                                            match f.iter().position(|x| *x == key) {
                                                Some(i) => {
                                                    f.remove(i);
                                                }
                                                None => f.push(key.clone()),
                                            }
                                        });
                                    }
                                >
                                    <span class="cal-label-hash">"#"</span>
                                    {l}
                                    <span class="cal-label-count">{n}</span>
                                </button>
                            }
                        }
                    />
                </div>
            </Show>

            <Show when=move || form_open.get() fallback=|| ().into_view()>
                <form class="cal-connect" on:submit=on_connect_submit>
                    <div class="cal-field">
                        <label class="cal-label">"Provider"</label>
                        <select
                            class="cal-input"
                            on:change=move |ev| {
                                if let Some(k) = CalendarProviderKind::parse_token(
                                    &event_target_value(&ev),
                                ) {
                                    form_kind.set(k);
                                }
                            }
                        >
                            {[
                                CalendarProviderKind::Local,
                                CalendarProviderKind::Caldav,
                                CalendarProviderKind::Webcal,
                            ]
                                .into_iter()
                                .map(|k| {
                                    let selected = move || form_kind.get() == k;
                                    view! {
                                        <option value=k.as_str() selected=selected>
                                            {k.label()}
                                        </option>
                                    }
                                })
                                .collect::<Vec<_>>()}
                        </select>
                    </div>

                    <div class="cal-field">
                        <label class="cal-label">"Name"</label>
                        <input
                            class="cal-input"
                            placeholder="Work calendar"
                            prop:value=move || form_name.get()
                            on:input=move |ev| form_name.set(event_target_value(&ev))
                        />
                    </div>

                    <div class="cal-field">
                        <label class="cal-label">{target_label}</label>
                        <input
                            class="cal-input"
                            placeholder=target_placeholder
                            prop:value=move || form_target.get()
                            on:input=move |ev| form_target.set(event_target_value(&ev))
                        />
                    </div>

                    <Show
                        when=move || form_error.with(Option::is_some)
                        fallback=|| ().into_view()
                    >
                        <div class="cal-form-error">
                            {move || form_error.get().unwrap_or_default()}
                        </div>
                    </Show>

                    <div class="cal-form-actions">
                        <button
                            class="cal-btn cal-btn-primary"
                            type="submit"
                            disabled=move || form_busy.get()
                        >
                            {move || {
                                if form_busy.get() { "Connecting…" } else { "Connect & sync" }
                            }}
                        </button>
                    </div>
                </form>
            </Show>

            // --- New local calendar -------------------------------------------
            <Show when=move || newcal_open.get() fallback=|| ().into_view()>
                <form class="cal-connect" on:submit=on_newcal_submit>
                    <div class="cal-field">
                        <label class="cal-label">"Calendar name"</label>
                        <input
                            class="cal-input"
                            placeholder="Personal"
                            prop:value=move || newcal_name.get()
                            on:input=move |ev| newcal_name.set(event_target_value(&ev))
                        />
                    </div>
                    <Show
                        when=move || newcal_error.with(Option::is_some)
                        fallback=|| ().into_view()
                    >
                        <div class="cal-form-error">
                            {move || newcal_error.get().unwrap_or_default()}
                        </div>
                    </Show>
                    <div class="cal-form-actions">
                        <button
                            class="cal-btn cal-btn-primary"
                            type="submit"
                            disabled=move || newcal_busy.get()
                        >
                            {move || {
                                if newcal_busy.get() {
                                    "Creating…"
                                } else if event_open.get() {
                                    "Create & assign"
                                } else {
                                    "Create calendar"
                                }
                            }}
                        </button>
                    </div>
                </form>
            </Show>

            // --- Add / edit event (local calendars) ---------------------------
            // One form, two modes: `editing` `None` creates (POST), `Some`
            // saves a replacement of that event (PUT).
            <Show when=move || event_open.get() fallback=|| ().into_view()>
                <form class="cal-connect cal-event-editor" on:submit=on_event_submit>
                    <Show
                        when=move || writable_calendars().is_empty()
                        fallback=|| ().into_view()
                    >
                        <div class="cal-empty-editor">
                            <span class="cal-muted">
                                "No writable calendar yet. Create one to place this event on your planner."
                            </span>
                            <button
                                class="cal-btn"
                                type="button"
                                on:click=move |_| {
                                    newcal_error.set(None);
                                    newcal_open.set(true);
                                }
                            >
                                "+ New calendar"
                            </button>
                        </div>
                    </Show>
                    <Show
                        when=move || !writable_calendars().is_empty()
                        fallback=|| ().into_view()
                    >
                        <div class="cal-field">
                            <label class="cal-label">"Calendar"</label>
                            <div class="cal-calendar-picker">
                                // An existing provider event cannot be moved
                                // safely between calendars as part of a PUT, so
                                // assignment locks while editing. New planner
                                // entries can target any writable calendar.
                                <select
                                    class="cal-input"
                                    disabled=move || editing.with(Option::is_some)
                                    on:change=move |ev| event_cal.set(event_target_value(&ev))
                                >
                                    {move || {
                                        writable_calendars()
                                            .into_iter()
                                            .map(|c| {
                                                let value = c.id.clone();
                                                let id = c.id.clone();
                                                let selected = move || event_cal.get() == id;
                                                view! {
                                                    <option value=value selected=selected>
                                                        {c.name}
                                                    </option>
                                                }
                                            })
                                            .collect::<Vec<_>>()
                                    }}
                                </select>
                                <Show
                                    when=move || editing.with(Option::is_none)
                                    fallback=|| ().into_view()
                                >
                                    <button
                                        class="cal-btn cal-newcal-inline"
                                        type="button"
                                        on:click=move |_| {
                                            newcal_error.set(None);
                                            newcal_open.set(true);
                                        }
                                    >
                                        "+ New calendar"
                                    </button>
                                </Show>
                            </div>
                        </div>
                        <div class="cal-field">
                            <label class="cal-label">"Title"</label>
                            <input
                                class="cal-input"
                                placeholder="Team standup"
                                prop:value=move || event_summary.get()
                                on:input=move |ev| event_summary.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="cal-field">
                            <label class="cal-label">"Start"</label>
                            <input
                                class="cal-input"
                                type="datetime-local"
                                prop:value=move || event_start.get()
                                on:input=move |ev| event_start.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="cal-field">
                            <label class="cal-label">"End"</label>
                            <input
                                class="cal-input"
                                type="datetime-local"
                                prop:value=move || event_end.get()
                                on:input=move |ev| event_end.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="cal-field">
                            <label class="cal-label">"Location (optional)"</label>
                            <input
                                class="cal-input"
                                placeholder="Room 2 / video link"
                                prop:value=move || event_location.get()
                                on:input=move |ev| event_location.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="cal-field">
                            <label class="cal-label">"Description (optional)"</label>
                            <textarea
                                class="cal-input cal-textarea"
                                rows="3"
                                placeholder="Agenda, notes, links…"
                                prop:value=move || event_body.get()
                                on:input=move |ev| event_body.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="cal-field">
                            <label class="cal-label">"Labels (optional, comma-separated)"</label>
                            <input
                                class="cal-input"
                                placeholder="work, travel, q3"
                                prop:value=move || event_labels.get()
                                on:input=move |ev| event_labels.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="cal-field">
                            <label class="cal-label">"Attachments (optional)"</label>
                            // Staged attachments, each removable before saving.
                            <Show
                                when=move || !event_attachments.with(Vec::is_empty)
                                fallback=|| ().into_view()
                            >
                                <ul class="cal-attach-staged">
                                    {move || {
                                        event_attachments
                                            .get()
                                            .into_iter()
                                            .enumerate()
                                            .map(|(i, att)| {
                                                let name = attachment_label(&att);
                                                view! {
                                                    <li class="cal-attach-staged-item">
                                                        <span class="cal-attach-name" title=att.url.clone()>
                                                            {name}
                                                        </span>
                                                        <button
                                                            class="cal-attach-del"
                                                            type="button"
                                                            title="Remove attachment"
                                                            on:click=move |_| remove_attachment(i)
                                                        >
                                                            <Icon icon=MdIcon::Close />
                                                        </button>
                                                    </li>
                                                }
                                            })
                                            .collect::<Vec<_>>()
                                    }}
                                </ul>
                            </Show>
                            <div class="cal-attach-add">
                                <input
                                    class="cal-input cal-attach-url"
                                    placeholder="Paste an image / file URL"
                                    prop:value=move || event_attach_url.get()
                                    on:input=move |ev| event_attach_url.set(event_target_value(&ev))
                                    on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                                        if ev.key() == "Enter" {
                                            ev.prevent_default();
                                            add_attachment_url();
                                        }
                                    }
                                />
                                <button
                                    class="cal-btn"
                                    type="button"
                                    on:click=move |_| add_attachment_url()
                                >
                                    "Add URL"
                                </button>
                            </div>
                            <div class="cal-attach-upload">
                                <input
                                    type="file"
                                    multiple
                                    on:change=on_attach_file_change
                                />
                                <Show
                                    when=move || { event_uploads.get() > 0 }
                                    fallback=|| ().into_view()
                                >
                                    <span class="cal-muted">"Uploading…"</span>
                                </Show>
                            </div>
                        </div>
                    </Show>
                    <Show
                        when=move || event_error.with(Option::is_some)
                        fallback=|| ().into_view()
                    >
                        <div class="cal-form-error">
                            {move || event_error.get().unwrap_or_default()}
                        </div>
                    </Show>
                    <div class="cal-form-actions">
                        <button
                            class="cal-btn cal-btn-primary"
                            type="submit"
                            disabled=move || {
                                event_busy.get()
                                    || event_uploads.get() > 0
                                    || writable_calendars().is_empty()
                            }
                        >
                            {move || {
                                if event_busy.get() {
                                    "Saving…"
                                } else if event_uploads.get() > 0 {
                                    "Uploading…"
                                } else if editing.with(Option::is_some) {
                                    "Save changes"
                                } else {
                                    "Add event"
                                }
                            }}
                        </button>
                        <Show
                            when=move || editing.with(Option::is_some)
                            fallback=|| ().into_view()
                        >
                            <button
                                class="cal-btn"
                                type="button"
                                on:click=move |_| {
                                    reset_event_form();
                                    event_open.set(false);
                                }
                            >
                                "Cancel"
                            </button>
                        </Show>
                    </div>
                </form>
            </Show>

            <Show
                when=move || form_notice.with(Option::is_some)
                fallback=|| ().into_view()
            >
                <div class="cal-notice">{move || form_notice.get().unwrap_or_default()}</div>
            </Show>

            <div class="cal-body" class:cal-body-agenda=move || is_agenda()>
                <Show when=move || loading.get() fallback=|| ().into_view()>
                    <div class="cal-status">"Loading events…"</div>
                </Show>

                <Show
                    when=move || !loading.get() && load_error.with(Option::is_some)
                    fallback=|| ().into_view()
                >
                    <div class="cal-status cal-error">
                        {move || {
                            format!(
                                "Could not load events: {}",
                                load_error.get().unwrap_or_default(),
                            )
                        }}
                    </div>
                </Show>

                <Show
                    when=move || {
                        !loading.get()
                            && load_error.with(Option::is_none)
                            && events.with(Vec::is_empty)
                            && is_agenda()
                    }
                    fallback=|| ().into_view()
                >
                    <div class="cal-status">
                        <p>"No events yet."</p>
                        <p class="cal-muted">
                            "Connect a calendar above to ingest your events."
                        </p>
                    </div>
                </Show>

                // Empty because the active *label* filter matched nothing (distinct
                // from "all calendars hidden", so it doesn't misattribute the cause).
                <Show
                    when=move || {
                        !loading.get()
                            && !events.with(Vec::is_empty)
                            && visible_events().is_empty()
                            && is_agenda()
                            && !label_filter.with(Vec::is_empty)
                    }
                    fallback=|| ().into_view()
                >
                    <div class="cal-status">
                        <p>
                            {move || {
                                let picked = label_filter
                                    .get()
                                    .iter()
                                    .map(|l| format!("#{l}"))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                format!("No events with label {picked}.")
                            }}
                        </p>
                        <p class="cal-muted">
                            "Pick “All” in the label bar to clear the filter."
                        </p>
                    </div>
                </Show>

                <Show
                    when=move || {
                        !loading.get()
                            && !events.with(Vec::is_empty)
                            && visible_events().is_empty()
                            && is_agenda()
                            && label_filter.with(Vec::is_empty)
                    }
                    fallback=|| ().into_view()
                >
                    <div class="cal-status">
                        <p>"All calendars are hidden."</p>
                        <p class="cal-muted">
                            "Re-activate a calendar in the sidebar to see its events."
                        </p>
                    </div>
                </Show>

                // --- Month / Week / Day grids ---------------------------------
                // Unlike the agenda, the grids render even with no events (the
                // empty calendar structure is itself useful), so they are gated
                // only on a successful, finished load.
                <Show
                    when=move || {
                        !loading.get() && load_error.with(Option::is_none) && !is_agenda()
                    }
                    fallback=|| ().into_view()
                >
                    {move || {
                        let evs = visible_events();
                        let a = anchor.get();
                        match view_mode.get() {
                            ViewMode::Month => {
                                render_month(evs, a, today, cal_names, anchor, view_mode)
                            }
                            ViewMode::Week => {
                                let start = week_start(a);
                                let days = (0..7).map(|i| start + i).collect::<Vec<_>>();
                                render_timegrid(
                                    days,
                                    evs,
                                    today,
                                    now.get(),
                                    PlannerCalendars {
                                        names: cal_names,
                                        all: calendars,
                                    },
                                    open_create_at,
                                    start_edit,
                                )
                            }
                            ViewMode::Day => {
                                render_timegrid(
                                    vec![a],
                                    evs,
                                    today,
                                    now.get(),
                                    PlannerCalendars {
                                        names: cal_names,
                                        all: calendars,
                                    },
                                    open_create_at,
                                    start_edit,
                                )
                            }
                            ViewMode::Agenda => ().into_any(),
                        }
                    }}
                </Show>

                <Show
                    when=move || {
                        !loading.get() && !visible_events().is_empty() && is_agenda()
                    }
                    fallback=|| ().into_view()
                >
                    <div class="cal-agenda">
                        <For
                            each=move || groups()
                            // Key by date *and* member ids: `<For>` never re-runs
                            // children for a surviving key, so a date-only key
                            // freezes each day at the events it first rendered —
                            // hiding/showing a calendar or toggling a label chip
                            // (which bypass the `loading` remount) would leave
                            // stale, un-interleaved day lists.
                            key=|g| {
                                g.events.iter().fold(g.date.clone(), |mut k, e| {
                                    k.push('|');
                                    k.push_str(&e.id);
                                    k
                                })
                            }
                            children=move |g: DayGroup| {
                                let heading = format_day_heading(&g.date);
                                view! {
                                    <div class="cal-day">
                                        <h3 class="cal-day-heading">{heading}</h3>
                                        <ul class="cal-event-list">
                                            <For
                                                each={
                                                    let evs = g.events.clone();
                                                    move || evs.clone()
                                                }
                                                key=|e| e.id.clone()
                                                children=move |e: Event| {
                                                    let cal_name = cal_names
                                                        .with(|m| m.get(&e.calendar_id).cloned());
                                                    // Editable iff its calendar is local + writable.
                                                    let writable = calendars.with(|cals| {
                                                        cals.iter().any(|c| {
                                                            c.id == e.calendar_id && c.is_writable()
                                                        })
                                                    });
                                                    let edit_id = e.id.clone();
                                                    let on_edit = move || start_edit(edit_id.clone());
                                                    let id = e.id.clone();
                                                    let on_delete = move || delete_event(id.clone());
                                                    event_row(&e, cal_name, writable, on_edit, on_delete)
                                                }
                                            />
                                        </ul>
                                    </div>
                                }
                            }
                        />
                    </div>
                </Show>
            </div>
        </section>
        </div>
    }
}

/// Render one agenda row for an event. When the event carries a description or
/// attendees — data the compact list doesn't otherwise surface — the summary
/// becomes a toggle that expands an inline detail block (description, attendees,
/// and the raw recurrence rule).
fn event_row(
    e: &Event,
    cal_name: Option<String>,
    writable: bool,
    on_edit: impl Fn() + 'static,
    on_delete: impl Fn() + 'static,
) -> impl IntoView {
    let time = if is_all_day_block(e) {
        "All day".to_string()
    } else {
        format_time_range(&e.start, &e.end)
    };
    let summary = if e.summary.trim().is_empty() {
        "(untitled)".to_string()
    } else {
        e.summary.clone()
    };
    let location = e.location.clone().filter(|l| !l.trim().is_empty());
    let recurring = e.rrule.is_some();

    // Detail fields the list doesn't otherwise show.
    let body_text = e.body.clone().filter(|b| !b.trim().is_empty());
    let attendee_names: Vec<String> = e
        .attendees
        .iter()
        .filter_map(|a| a.display_name.clone())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    let attendee_text = (!attendee_names.is_empty()).then(|| attendee_names.join(", "));
    let rrule_text = e.rrule.clone().filter(|r| !r.trim().is_empty());
    let labels: Vec<String> = e
        .labels
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let attachments = e.attachments.clone();
    let has_labels = !labels.is_empty();
    let has_attachments = !attachments.is_empty();
    // A description, attendees, labels, or attachments make the row worth
    // expanding (the `↻` marker already signals a recurrence, so an rrule alone
    // doesn't).
    let has_detail =
        body_text.is_some() || attendee_text.is_some() || has_labels || has_attachments;
    let expanded = RwSignal::new(false);

    // Edit + delete controls, only for events on a local, writable calendar.
    // Built once per row (so the `Fn` callbacks are consumed at most once into
    // their click handlers); the shared `.row-acts` wrapper right-aligns the
    // pair inside the meta line (see `.cal-event-acts`).
    let acts = writable.then(|| {
        view! {
            <div class="row-acts cal-event-acts">
                {row_action(MdIcon::Edit, "Edit event", false, on_edit)}
                {row_action(MdIcon::Delete, "Delete event", true, on_delete)}
            </div>
        }
    });

    view! {
        <li class="cal-event">
            <span class="cal-event-time">{time}</span>
            <div class="cal-event-main">
                <button
                    class="cal-event-summary"
                    disabled=!has_detail
                    on:click=move |_| expanded.update(|v| *v = !*v)
                >
                    <span class="cal-event-summary-text">{summary}</span>
                    <Show when=move || recurring fallback=|| ().into_view()>
                        <span class="cal-recur" title="Recurring event"><Icon icon=MdIcon::Refresh /></span>
                    </Show>
                    <Show
                        when={
                            let h = has_detail;
                            move || h
                        }
                        fallback=|| ().into_view()
                    >
                        <span class="cal-caret">
                            {move || if expanded.get() { "▾" } else { "▸" }}
                        </span>
                    </Show>
                </button>
                <div class="cal-event-meta">
                    <Show
                        when={
                            let has = location.is_some();
                            move || has
                        }
                        fallback=|| ().into_view()
                    >
                        <span class="cal-event-loc">
                            {location.clone().unwrap_or_default()}
                        </span>
                    </Show>
                    <Show
                        when={
                            let has = cal_name.is_some();
                            move || has
                        }
                        fallback=|| ().into_view()
                    >
                        <span class="cal-event-cal">{cal_name.clone().unwrap_or_default()}</span>
                    </Show>
                    {has_attachments.then(|| view! {
                        <span class="cal-event-attach-icon" title="Has attachments"><Icon icon=MdIcon::Attachment /></span>
                    })}
                    {labels
                        .iter()
                        .map(|l| view! { <span class="cal-event-label">{format!("#{l}")}</span> })
                        .collect::<Vec<_>>()}
                    {acts}
                </div>
                <Show
                    when={
                        let h = has_detail;
                        move || h && expanded.get()
                    }
                    fallback=|| ().into_view()
                >
                    <div class="cal-event-detail">
                        {body_text
                            .clone()
                            .map(|t| view! { <p class="cal-event-body">{t}</p> })}
                        {attendee_text
                            .clone()
                            .map(|t| {
                                view! {
                                    <div class="cal-event-kv">
                                        <span class="cal-event-detail-k">"Attendees"</span>
                                        <span>{t}</span>
                                    </div>
                                }
                            })}
                        {rrule_text
                            .clone()
                            .map(|t| {
                                view! {
                                    <div class="cal-event-kv">
                                        <span class="cal-event-detail-k">"Repeats"</span>
                                        <span class="cal-event-rrule">{t}</span>
                                    </div>
                                }
                            })}
                        {(!attachments.is_empty())
                            .then(|| {
                                let items = attachments
                                    .clone()
                                    .into_iter()
                                    .map(|att| attachment_view(&att))
                                    .collect::<Vec<_>>();
                                view! {
                                    <div class="cal-event-kv cal-event-attach-kv">
                                        <span class="cal-event-detail-k">"Attachments"</span>
                                        <div class="cal-attach-list">{items}</div>
                                    </div>
                                }
                            })}
                    </div>
                </Show>
            </div>
        </li>
    }
}

/// Parse a comma-separated labels input into a clean list: split on commas,
/// trim, drop blanks, dedup case-insensitively (first-seen casing kept). Matches
/// the server's `clean_labels`.
fn parse_labels(raw: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    raw.split(',')
        .filter_map(|l| {
            let label = l.trim();
            (!label.is_empty() && seen.insert(label.to_lowercase())).then(|| label.to_string())
        })
        .collect()
}

/// The distinct labels across `events` with how many events carry each,
/// sorted (case-insensitively, first-seen casing kept) — drives the agenda's
/// label filter chips.
fn label_counts<'a>(events: impl IntoIterator<Item = &'a Event>) -> Vec<(String, usize)> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<(String, usize)> = Vec::new();
    for e in events {
        // Count each event at most once per label, even when it repeats one
        // ("Work, work").
        let mut seen = HashSet::new();
        for l in &e.labels {
            let label = l.trim();
            let key = label.to_lowercase();
            if label.is_empty() || !seen.insert(key.clone()) {
                continue;
            }
            match index.get(&key) {
                Some(&i) => out[i].1 += 1,
                None => {
                    index.insert(key, out.len());
                    out.push((label.to_string(), 1));
                }
            }
        }
    }
    out.sort_by_key(|(l, _)| l.to_lowercase());
    out
}

/// Whether `event` carries `label` (case-insensitive) — the agenda label filter.
fn event_has_label(event: &Event, label: &str) -> bool {
    let want = label.to_lowercase();
    event.labels.iter().any(|l| l.trim().to_lowercase() == want)
}

/// Milliseconds since the Unix epoch (browser clock).
fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// A collision-resistant object key for an uploaded attachment: `events/<ms>-<rand>-<name>`.
/// The millisecond timestamp orders uploads; the random component keeps keys from
/// the *same* selection (all sharing one `now_ms()`) distinct so concurrent uploads
/// don't clobber one another. The name is path-segment-trimmed; the storage route
/// percent-encodes each segment, so a crafted filename can't escape the key.
fn upload_key(name: &str) -> String {
    let rand = (js_sys::Math::random() * 1_000_000_000.0) as u64;
    format!("events/{}-{}-{}", now_ms(), rand, name.trim_matches('/'))
}

/// Best-effort MIME type from a filename's extension — only the image types the
/// UI renders inline (everything else stays `None` and shows as a link).
fn guess_content_type(name: &str) -> Option<String> {
    let ext = name.rsplit('.').next()?.to_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Render one attachment for an event's detail block: an inline thumbnail for an
/// image, else a labelled download link. Both open in a new tab; the href is
/// resolved via [`attachment_href`] so uploaded files carry the auth token. An
/// attachment whose URL uses an unsafe scheme is shown as inert text (never a
/// clickable `href`), so a pasted `javascript:` URL can't execute (XSS guard).
fn attachment_view(att: &Attachment) -> impl IntoView {
    let href = attachment_href(att);
    let name = attachment_label(att);
    let safe = is_safe_href(&href);
    if safe && attachment_is_image(att) {
        let alt = name.clone();
        let img_src = href.clone();
        view! {
            <a
                class="cal-attach-item cal-attach-img-link"
                href=href
                target="_blank"
                rel="noreferrer"
                title=name
            >
                <img class="cal-attach-img" src=img_src alt=alt loading="lazy" />
            </a>
        }
        .into_any()
    } else if safe {
        view! {
            <a
                class="cal-attach-item cal-attach-link"
                href=href
                target="_blank"
                rel="noreferrer"
            >
                <span class="cal-attach-file-icon"><Icon icon=MdIcon::File /></span>
                <span class="cal-attach-name">{name}</span>
            </a>
        }
        .into_any()
    } else {
        // Unsafe scheme (e.g. `javascript:`): render inert — name as plain text,
        // never an executable link.
        view! {
            <span class="cal-attach-item cal-attach-unsafe" title=att.url.clone()>
                <span class="cal-attach-file-icon"><Icon icon=MdIcon::Warning /></span>
                <span class="cal-attach-name">{name}</span>
            </span>
        }
        .into_any()
    }
}

/// Read a picked browser [`web_sys::File`] into `(name, content_type, bytes)` —
/// the same shape the Files panel uploads with. Shared with the chat composer's
/// attachment upload (SOUL §9/§12).
pub(crate) async fn read_file(
    file: web_sys::File,
) -> Result<(String, Option<String>, Vec<u8>), String> {
    let name = file.name();
    let ctype = {
        let t = file.type_();
        (!t.is_empty()).then_some(t)
    };
    let buf = wasm_bindgen_futures::JsFuture::from(file.array_buffer())
        .await
        .map_err(|_| "could not read the selected file".to_string())?;
    let array = js_sys::Uint8Array::new(&buf);
    Ok((name, ctype, array.to_vec()))
}

/// Group events (already sorted by start) into per-UTC-day buckets, preserving
/// chronological day order.
fn group_by_day(events: &[Event]) -> Vec<DayGroup> {
    let mut groups: Vec<DayGroup> = Vec::new();
    for e in events {
        let date = day_key(&e.start);
        if let Some(last) = groups.last_mut() {
            if last.date == date {
                last.events.push(e.clone());
                continue;
            }
        }
        groups.push(DayGroup {
            date,
            events: vec![e.clone()],
        });
    }
    groups
}

/// The `YYYY-MM-DD` key from an RFC 3339 timestamp (its first 10 chars). Falls
/// back to the whole string if it is shorter / malformed.
fn day_key(ts: &str) -> String {
    if ts.len() >= 10 && ts.as_bytes()[4] == b'-' && ts.as_bytes()[7] == b'-' {
        ts[..10].to_string()
    } else {
        ts.to_string()
    }
}

/// Format a day key (`YYYY-MM-DD`) into a friendly heading like
/// `Saturday, 13 June 2026`. Pure string work (no `chrono`).
fn format_day_heading(date: &str) -> String {
    let parts: Vec<&str> = date.splitn(3, '-').collect();
    if parts.len() != 3 {
        return date.to_string();
    }
    let (Ok(y), Ok(m), Ok(d)) = (
        parts[0].parse::<i64>(),
        parts[1].parse::<u32>(),
        parts[2].parse::<u32>(),
    ) else {
        return date.to_string();
    };
    let month = month_name(m);
    let weekday = weekday_name(y, m, d);
    match weekday {
        Some(w) => format!("{w}, {d} {month} {y}"),
        None => format!("{d} {month} {y}"),
    }
}

/// Format the `HH:MM` time range from two (local-wall-clock) stamps, e.g.
/// `09:00 – 10:00`. If the times are identical (or unparseable) shows just the
/// start; an all-day-ish `00:00 – 00:00` is shown as "All day".
fn format_time_range(start: &str, end: &str) -> String {
    let s = hh_mm(start);
    let e = hh_mm(end);
    match (s, e) {
        (Some(s), Some(e)) if s == "00:00" && e == "00:00" => "All day".to_string(),
        (Some(s), Some(e)) if s == e => s,
        (Some(s), Some(e)) => format!("{s} – {e}"),
        (Some(s), None) => s,
        (None, _) => start.to_string(),
    }
}

/// Parse an `<input type="datetime-local">` value (`YYYY-MM-DDTHH:MM`, any
/// trailing `:SS` ignored to the picker's minute grain) into its calendar
/// fields `(year, month, day, hour, minute)`. `None` for an empty or malformed
/// value. Pure (no browser calls) so it is unit-testable on the host.
fn parse_datetime_local(value: &str) -> Option<(i64, u32, u32, u32, u32)> {
    let v = value.trim();
    // Require at least `YYYY-MM-DDTHH:MM` (16 chars) with the date/time `T`.
    if v.len() < 16 || v.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    let (y, m, d) = parse_ymd(&v[..10])?;
    let t = &v.as_bytes()[11..16];
    if t[2] != b':' {
        return None;
    }
    let h = u32::from((t[0] - b'0') * 10 + (t[1] - b'0'));
    let mi = u32::from((t[3] - b'0') * 10 + (t[4] - b'0'));
    (h < 24 && mi < 60).then_some((y, m, d, h, mi))
}

/// Convert an `<input type="datetime-local">` value into the RFC 3339 **UTC**
/// timestamp the API stores. The entered wall-clock is read in the browser's
/// local timezone (matching how the calendar renders every event, via
/// [`to_local_event`]) and normalised to UTC, so a time the user types back is
/// the time they see. `None` for an empty or malformed value.
fn local_input_to_rfc3339(value: &str) -> Option<String> {
    let (y, m, d, h, mi) = parse_datetime_local(value)?;
    // `new Date(y, monthIndex, …)` reads its fields as local time; the ISO form
    // is UTC. Truncate the `…:SS.sssZ` tail back to the minute grain.
    let date = js_sys::Date::new_with_year_month_day_hr_min_sec(
        y as u32,
        m as i32 - 1,
        d as i32,
        h as i32,
        mi as i32,
        0,
    );
    let iso = date.to_iso_string().as_string()?;
    Some(format!("{}:00Z", iso.get(..16)?))
}

/// Convert an `<input type="datetime-local">` value into the RFC 3339 stamp for
/// an **all-day** endpoint: midnight UTC of the entered calendar *date*, with no
/// timezone shift. An all-day stamp marks a date, not an instant (the store pins
/// it to midnight UTC of its date, and the renderer never zone-shifts it — see
/// `normalize_event_span` server-side and [`to_local_event`]), so an edit must
/// keep it on the day the user picked instead of sliding it by the browser's
/// offset the way [`local_input_to_rfc3339`] would. Pure (no browser calls), so
/// it is unit-testable on the host. `None` for an empty or malformed value.
fn all_day_input_to_rfc3339(value: &str) -> Option<String> {
    let (y, m, d, _, _) = parse_datetime_local(value)?;
    Some(format!("{y:04}-{m:02}-{d:02}T00:00:00Z"))
}

/// Extract `HH:MM` from an RFC 3339 timestamp `YYYY-MM-DDTHH:MM:SS…`.
fn hh_mm(ts: &str) -> Option<String> {
    let t = ts.find('T')?;
    let rest = &ts[t + 1..];
    if rest.len() >= 5 && rest.as_bytes()[2] == b':' {
        Some(rest[..5].to_string())
    } else {
        None
    }
}

fn month_name(m: u32) -> &'static str {
    match m {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "",
    }
}

/// Sakamoto's algorithm: weekday name for a proleptic-Gregorian date, or `None`
/// for an out-of-range month.
fn weekday_name(y: i64, m: u32, d: u32) -> Option<&'static str> {
    if !(1..=12).contains(&m) {
        return None;
    }
    const T: [i64; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let yy = if m < 3 { y - 1 } else { y };
    let idx = (yy + yy / 4 - yy / 100 + yy / 400 + T[(m - 1) as usize] + d as i64).rem_euclid(7);
    Some(
        [
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ][idx as usize],
    )
}

// ===========================================================================
// Grid date math — all proleptic-Gregorian, no `chrono` in the wasm bundle.
//
// Dates are carried as *day-numbers*: signed days since 1970-01-01 (UTC). The
// `days_from_civil` / `civil_from_days` pair (Howard Hinnant's exact algorithm)
// converts to and from `(year, month, day)`, giving clean +/- arithmetic for
// navigation and a single integer key for bucketing events into grid cells.
// ===========================================================================

/// Day-number (days since 1970-01-01) for a `(year, month, day)`. Exact for any
/// proleptic-Gregorian date; `month` is 1..=12, `day` is 1..=31.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let m = i64::from(m);
    let d = i64::from(d);
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: the `(year, month, day)` for a day-number.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Weekday index for a day-number, `0 = Sunday .. 6 = Saturday`. (1970-01-01 was
/// a Thursday, so `(z + 4) mod 7`.)
fn weekday_sun0(z: i64) -> i64 {
    (z + 4).rem_euclid(7)
}

/// Monday-first column index for a day-number, `0 = Monday .. 6 = Sunday` — the
/// grids use the ISO-8601 / European week (matches the agenda's UTC theme).
fn monday_index(z: i64) -> i64 {
    (weekday_sun0(z) + 6).rem_euclid(7)
}

/// The Monday starting the week containing `z`.
fn week_start(z: i64) -> i64 {
    z - monday_index(z)
}

/// `YYYY-MM-DD` for a day-number (the same key shape the agenda buckets by).
fn ymd_string(z: i64) -> String {
    let (y, m, d) = civil_from_days(z);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A browser `datetime-local` value for `minute` on `day`. Values beyond the
/// day's end carry into tomorrow, which lets a 23:30 planner slot default to an
/// event ending at 00:30 without special-casing the form.
fn datetime_local_value(day: i64, minute: i64) -> String {
    let date = day + minute.div_euclid(1440);
    let clock = minute.rem_euclid(1440);
    format!("{}T{:02}:{:02}", ymd_string(date), clock / 60, clock % 60)
}

/// Bring the add/edit form into view after the reactive DOM has mounted it.
fn scroll_event_editor_into_view() {
    spawn_local(async {
        if let Some(el) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.query_selector(".cal-event-editor").ok().flatten())
        {
            el.scroll_into_view();
        }
    });
}

/// The 42 day-numbers (6 weeks × 7) of the month grid containing `anchor_dn`,
/// starting on the Monday on/before the 1st. Six rows always cover any month
/// (≤ 31 days + ≤ 6 leading days = 37 ≤ 42).
fn month_cells(anchor_dn: i64) -> Vec<i64> {
    let (y, m, _) = civil_from_days(anchor_dn);
    let start = week_start(days_from_civil(y, m, 1));
    (0..42).map(|i| start + i).collect()
}

/// The first of the month `delta` calendar months from the month of `anchor_dn`
/// (negative steps backward). Anchoring on the 1st avoids day-of-month overflow.
fn add_months(anchor_dn: i64, delta: i64) -> i64 {
    let (y, m, _) = civil_from_days(anchor_dn);
    let total = y * 12 + (i64::from(m) - 1) + delta;
    let ny = total.div_euclid(12);
    let nm = total.rem_euclid(12) + 1;
    days_from_civil(ny, nm as u32, 1)
}

/// Parse a `YYYY-MM-DD` prefix into `(year, month, day)`.
fn parse_ymd(date: &str) -> Option<(i64, u32, u32)> {
    let p: Vec<&str> = date.splitn(3, '-').collect();
    if p.len() != 3 {
        return None;
    }
    Some((p[0].parse().ok()?, p[1].parse().ok()?, p[2].parse().ok()?))
}

/// Day-number for the date part of an RFC 3339 timestamp.
fn daynum_of_ts(ts: &str) -> Option<i64> {
    let (y, m, d) = parse_ymd(&day_key(ts))?;
    Some(days_from_civil(y, m, d))
}

/// Minutes from midnight for a stamp's `HH:MM` (local, post-[`to_local_event`]);
/// `0` if unparseable.
fn minutes_of_ts(ts: &str) -> i64 {
    match hh_mm(ts) {
        Some(s) => {
            let b = s.as_bytes();
            let h = i64::from((b[0] - b'0') * 10 + (b[1] - b'0'));
            let mi = i64::from((b[3] - b'0') * 10 + (b[4] - b'0'));
            h * 60 + mi
        }
        None => 0,
    }
}

/// Whether an event reads as an all-day / multi-day block — these go in the
/// time grid's all-day row (and the agenda's "All day" label), never the hourly
/// grid. True when the API's `all_day` flag is set, or as a fallback when the
/// stamps run midnight to midnight (flag-less events synced before the flag was
/// projected, or hand-made midnight spans).
fn is_all_day_block(e: &Event) -> bool {
    e.all_day
        || (hh_mm(&e.start).as_deref() == Some("00:00")
            && hh_mm(&e.end).as_deref() == Some("00:00"))
}

/// The inclusive `[first_day, last_day]` day-numbers an event covers. An event
/// ending exactly at midnight occupies up to **but not including** that day (the
/// `end = next day 00:00` convention), so a single all-day event spans one cell.
/// The span is clamped to ≤ 366 days as a guard against a malformed timestamp.
fn event_day_span(e: &Event) -> Option<(i64, i64)> {
    let s = daynum_of_ts(&e.start)?;
    let mut last = daynum_of_ts(&e.end).unwrap_or(s);
    if last > s && hh_mm(&e.end).as_deref() == Some("00:00") {
        last -= 1;
    }
    if last < s {
        last = s;
    }
    Some((s, last.min(s + 366)))
}

/// Whether an event's covered span intersects the inclusive day window
/// `[lo, hi]`.
fn span_overlaps(e: &Event, lo: i64, hi: i64) -> bool {
    matches!(event_day_span(e), Some((s, l)) if s <= hi && l >= lo)
}

/// Greedy lane-packing of a day's timed events (each as `(start_min, end_min)`,
/// pre-sorted by start then end) into side-by-side columns so overlapping events
/// don't hide one another. Returns, per input event, `(column, column_count)`
/// where `column_count` is the width of its overlap cluster.
fn pack_lanes(spans: &[(i64, i64)]) -> Vec<(usize, usize)> {
    let mut out = vec![(0usize, 1usize); spans.len()];
    let mut i = 0;
    while i < spans.len() {
        // Grow the cluster while the next event starts before the running end.
        let mut j = i;
        let mut cluster_end = spans[i].1;
        while j + 1 < spans.len() && spans[j + 1].0 < cluster_end {
            j += 1;
            cluster_end = cluster_end.max(spans[j].1);
        }
        // Assign each event the first column whose last event has already ended.
        let mut col_end: Vec<i64> = Vec::new();
        for (k, &(s, e)) in spans[i..=j].iter().enumerate() {
            let c = match col_end.iter().position(|&ce| ce <= s) {
                Some(c) => {
                    col_end[c] = e;
                    c
                }
                None => {
                    col_end.push(e);
                    col_end.len() - 1
                }
            };
            out[i + k].0 = c;
        }
        let total = col_end.len().max(1);
        for slot in &mut out[i..=j] {
            slot.1 = total;
        }
        i = j + 1;
    }
    out
}

/// A stable, readable accent colour for a calendar id (FNV-1a hash → HSL hue),
/// used as the left-edge stripe on chips and time-grid blocks so events from
/// different calendars are visually distinct.
fn cal_color(id: &str) -> String {
    let mut h: u32 = 2_166_136_261;
    for b in id.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(16_777_619);
    }
    format!("hsl({}, 65%, 60%)", h % 360)
}

/// Today's date (in the browser's local timezone) as a day-number — the grid's
/// initial anchor + "today" highlight. Local to match the local-wall-clock time
/// the grids position and label events at (see [`to_local_event`]).
fn today_daynum() -> i64 {
    now_daynum_min().0
}

/// The current local-timezone time as `(day-number, minutes-from-midnight)`,
/// driving the Week/Day "now" line and the `today` highlight. Local (not UTC)
/// so the line sits at the user's wall-clock height, aligned with the events —
/// which [`to_local_event`] has likewise shifted into local wall-clock.
fn now_daynum_min() -> (i64, i64) {
    let now = js_sys::Date::new_0();
    let dn = days_from_civil(
        i64::from(now.get_full_year()),
        now.get_month().wrapping_add(1),
        now.get_date(),
    );
    let min = i64::from(now.get_hours()) * 60 + i64::from(now.get_minutes());
    (dn, min)
}

/// Rewrite an event's `start`/`end` from their stored UTC instants into the
/// browser's **local wall-clock** (a `YYYY-MM-DDTHH:MM:SS` string, no zone
/// suffix), so the agenda + grids show and position it at the time the user
/// actually experiences. Per-instant conversion means DST is handled correctly.
///
/// All-day blocks are left untouched: their stamps (flagged, or the
/// `00:00`–`00:00` convention) mark a calendar *date*, not an instant, so
/// shifting them by the zone offset would wrongly bleed them onto an adjacent
/// day (and break the midnight-based all-day fallback detection).
fn to_local_event(e: &Event) -> Event {
    if is_all_day_block(e) {
        return e.clone();
    }
    let mut out = e.clone();
    out.start = utc_to_local_wall(&e.start);
    out.end = utc_to_local_wall(&e.end);
    out
}

/// Convert an RFC 3339 UTC timestamp into a local-wall-clock
/// `YYYY-MM-DDTHH:MM:SS` string (no zone suffix — the downstream slicing
/// helpers read the date/`HH:MM` positionally). Unparseable input is returned
/// unchanged so a malformed stamp still renders rather than vanishing.
fn utc_to_local_wall(ts: &str) -> String {
    let d = js_sys::Date::new(&JsValue::from_str(ts));
    if d.get_time().is_nan() {
        return ts.to_string();
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date(),
        d.get_hours(),
        d.get_minutes(),
        d.get_seconds(),
    )
}

/// The Week view's range heading, e.g. `8 – 14 June 2026` (collapsing the shared
/// month / year), given the week's Monday.
fn week_title(start: i64) -> String {
    let (y1, m1, d1) = civil_from_days(start);
    let (y2, m2, d2) = civil_from_days(start + 6);
    if y1 == y2 && m1 == m2 {
        format!("{d1} – {d2} {} {y1}", month_name(m1))
    } else if y1 == y2 {
        format!("{d1} {} – {d2} {} {y1}", month_name(m1), month_name(m2))
    } else {
        format!(
            "{d1} {} {y1} – {d2} {} {y2}",
            month_name(m1),
            month_name(m2)
        )
    }
}

/// `(untitled)` for a blank summary, else the trimmed-as-is title.
fn event_title(e: &Event) -> String {
    if e.summary.trim().is_empty() {
        "(untitled)".to_string()
    } else {
        e.summary.clone()
    }
}

// ===========================================================================
// Grid rendering.
// ===========================================================================

/// Render the **Month** grid: a Monday-first 6×7 calendar. Each in-range event
/// shows as a chip (time + title) on every day it covers, capped per cell with a
/// "+N more". Clicking a day number, a chip, or "+N more" drills into Day view.
fn render_month(
    events: Vec<Event>,
    anchor_dn: i64,
    today_dn: i64,
    cal_names: RwSignal<HashMap<String, String>>,
    anchor: RwSignal<i64>,
    view_mode: RwSignal<ViewMode>,
) -> AnyView {
    const CHIP_CAP: usize = 3;
    const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

    let cells = month_cells(anchor_dn);
    let cur_month = civil_from_days(anchor_dn).1;
    let (lo, hi) = (cells[0], cells[cells.len() - 1]);

    // Bucket each visible event into every day it covers within the grid window.
    let mut bucket: HashMap<i64, Vec<Event>> = HashMap::new();
    for e in &events {
        if let Some((s, l)) = event_day_span(e) {
            let mut d = s.max(lo);
            let to = l.min(hi);
            while d <= to {
                bucket.entry(d).or_default().push(e.clone());
                d += 1;
            }
        }
    }
    // All-day chips sit on top of each cell (stacking vertically when there are
    // several), timed chips follow in start order — matching the time grid's
    // all-day strip.
    for evs in bucket.values_mut() {
        evs.sort_by(|a, b| {
            is_all_day_block(b)
                .cmp(&is_all_day_block(a))
                .then_with(|| a.start.cmp(&b.start))
                .then_with(|| a.summary.cmp(&b.summary))
        });
    }

    let cell_views = cells
        .into_iter()
        .map(|dn| {
            let (_, m, d) = civil_from_days(dn);
            let in_month = m == cur_month;
            let is_today = dn == today_dn;
            let day_evs = bucket.remove(&dn).unwrap_or_default();
            let total = day_evs.len();
            let chips = day_evs
                .into_iter()
                .take(CHIP_CAP)
                .map(|e| {
                    let color = cal_color(&e.calendar_id);
                    let cal = cal_names.with(|mp| mp.get(&e.calendar_id).cloned());
                    let title = event_title(&e);
                    let time = if is_all_day_block(&e) {
                        String::new()
                    } else {
                        hh_mm(&e.start).unwrap_or_default()
                    };
                    let tip = match cal {
                        Some(c) => format!("{title} · {c}"),
                        None => title.clone(),
                    };
                    view! {
                        <button
                            class="cal-chip"
                            title=tip
                            style=format!("border-left-color:{color}")
                            on:click=move |_| {
                                anchor.set(dn);
                                view_mode.set(ViewMode::Day);
                            }
                        >
                            <span class="cal-chip-time">{time}</span>
                            <span class="cal-chip-text">{title}</span>
                        </button>
                    }
                })
                .collect::<Vec<_>>();
            let more = total.saturating_sub(CHIP_CAP);
            view! {
                <div
                    class="cal-mcell"
                    class:cal-mcell-out=!in_month
                    class:cal-mcell-today=is_today
                >
                    <button
                        class="cal-mcell-num"
                        on:click=move |_| {
                            anchor.set(dn);
                            view_mode.set(ViewMode::Day);
                        }
                    >
                        {d.to_string()}
                    </button>
                    <div class="cal-mcell-evs">
                        {chips}
                        {(more > 0)
                            .then(|| {
                                view! {
                                    <button
                                        class="cal-chip-more"
                                        on:click=move |_| {
                                            anchor.set(dn);
                                            view_mode.set(ViewMode::Day);
                                        }
                                    >
                                        {format!("+{more} more")}
                                    </button>
                                }
                            })}
                    </div>
                </div>
            }
        })
        .collect::<Vec<_>>();

    view! {
        <div class="cal-month">
            <div class="cal-month-hdr">
                {WEEKDAYS
                    .iter()
                    .map(|w| view! { <div class="cal-month-hcell">{*w}</div> })
                    .collect::<Vec<_>>()}
            </div>
            <div class="cal-month-grid">{cell_views}</div>
        </div>
    }
    .into_any()
}

/// Render the **Week** (7 days) or **Day** (1 day) time grid: a sticky day
/// header, an all-day row for midnight-to-midnight events, and a 24-hour grid of
/// absolutely-positioned timed blocks, lane-packed per day so overlaps sit
/// side-by-side. Multi-day timed events clamp to each column's day edges.
///
/// `now` is the current local `(day-number, minutes-from-midnight)`; when its
/// day is one of the rendered columns a horizontal "now" line is drawn across that
/// column at the matching height, so the active / next event is obvious.
fn render_timegrid<OnCreate, OnEdit>(
    days: Vec<i64>,
    events: Vec<Event>,
    today_dn: i64,
    now: (i64, i64),
    calendars: PlannerCalendars,
    on_create: OnCreate,
    on_edit: OnEdit,
) -> AnyView
where
    OnCreate: Fn(i64, i64) + Copy + 'static,
    OnEdit: Fn(String) + Copy + 'static,
{
    const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const ALLDAY_CAP: usize = 4;

    let single = days.len() == 1;
    let ncols = days.len();
    let day0 = days[0];
    let day_last = days[days.len() - 1];
    let cols_style = format!("grid-template-columns: repeat({ncols}, minmax(0, 1fr))");

    // Day headers.
    let heads = days
        .iter()
        .map(|&dn| {
            let (_, _, d) = civil_from_days(dn);
            let w = WEEKDAYS[monday_index(dn) as usize];
            view! {
                <button
                    class="cal-tg-dayhead"
                    class:cal-tg-today=(dn == today_dn)
                    title="Plan an event at 09:00"
                    on:click=move |_| on_create(dn, 9 * 60)
                >
                    <span class="cal-tg-dow">{w}</span>
                    <span class="cal-tg-dnum">{d.to_string()}</span>
                </button>
            }
        })
        .collect::<Vec<_>>();

    // All-day events: one row each (no overlap), spanning their covered columns.
    let mut allday: Vec<Event> = events
        .iter()
        .filter(|e| is_all_day_block(e) && span_overlaps(e, day0, day_last))
        .cloned()
        .collect();
    allday.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| a.summary.cmp(&b.summary))
    });
    let allday_total = allday.len();
    let allday_rows = allday
        .into_iter()
        .take(ALLDAY_CAP)
        .map(|e| {
            let (s, l) = event_day_span(&e).unwrap_or((day0, day0));
            let c1 = (s.max(day0) - day0).max(0);
            let c2 = (l.min(day_last) - day0).max(c1);
            let color = cal_color(&e.calendar_id);
            let title = event_title(&e);
            let writable = calendars
                .all
                .with(|cs| cs.iter().any(|c| c.id == e.calendar_id && c.is_writable()));
            let edit_id = e.id.clone();
            let cal = calendars.names.with(|mp| mp.get(&e.calendar_id).cloned());
            let tip = match cal {
                Some(c) => format!("{title} · {c}"),
                None => title.clone(),
            };
            view! {
                <div class="cal-tg-allrow" style=cols_style.clone()>
                    <button
                        class="cal-tg-allbar"
                        title=tip
                        disabled=!writable
                        on:click=move |_| on_edit(edit_id.clone())
                        style=format!(
                            "grid-column: {} / {}; border-left-color:{color}",
                            c1 + 1,
                            c2 + 2,
                        )
                    >
                        {title}
                    </button>
                </div>
            }
        })
        .collect::<Vec<_>>();
    let allday_more = allday_total.saturating_sub(ALLDAY_CAP);

    // Hour gutter labels.
    let hours = (0..24)
        .map(|h| {
            view! {
                <div class="cal-tg-hour">
                    <span>{format!("{h:02}:00")}</span>
                </div>
            }
        })
        .collect::<Vec<_>>();

    // One column of timed blocks per day.
    let cols = days
        .iter()
        .map(|&dn| {
            let mut raw: Vec<(i64, i64, Event)> = events
                .iter()
                .filter(|e| !is_all_day_block(e) && span_overlaps(e, dn, dn))
                .filter_map(|e| {
                    let s = daynum_of_ts(&e.start)?;
                    let raw_end = daynum_of_ts(&e.end).unwrap_or(s);
                    // Clamp to this column's day: a block that starts earlier
                    // begins at 00:00; one that ends later runs to 24:00.
                    let start_min = if s < dn { 0 } else { minutes_of_ts(&e.start) };
                    let end_min = if raw_end > dn {
                        1440
                    } else {
                        minutes_of_ts(&e.end).max(start_min)
                    };
                    Some((start_min.clamp(0, 1440), end_min.clamp(0, 1440), e.clone()))
                })
                .collect();
            raw.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            let lanes = pack_lanes(&raw.iter().map(|(s, e, _)| (*s, *e)).collect::<Vec<_>>());
            let blocks = raw
                .into_iter()
                .zip(lanes)
                .map(|((s, e, ev), (col, total))| {
                    let dur_min = (e - s).max(20);
                    let top = s as f64 / 1440.0 * 100.0;
                    let height = dur_min as f64 / 1440.0 * 100.0;
                    // A block under ~45 min is too shallow for a stacked time +
                    // title, so it renders on a single line (`.cal-tg-block-short`).
                    let short = dur_min < 45;
                    let width = 100.0 / total as f64;
                    let left = col as f64 * width;
                    let color = cal_color(&ev.calendar_id);
                    let time = format_time_range(&ev.start, &ev.end);
                    let title = event_title(&ev);
                    let writable = calendars
                        .all
                        .with(|cs| cs.iter().any(|c| c.id == ev.calendar_id && c.is_writable()));
                    let edit_id = ev.id.clone();
                    let cal = calendars.names.with(|mp| mp.get(&ev.calendar_id).cloned());
                    let tip = match cal {
                        Some(c) => format!("{title} · {time} · {c}"),
                        None => format!("{title} · {time}"),
                    };
                    view! {
                        <button
                            class="cal-tg-block"
                            class:cal-tg-block-short=short
                            title=tip
                            disabled=!writable
                            on:click=move |click: leptos::ev::MouseEvent| {
                                click.stop_propagation();
                                on_edit(edit_id.clone());
                            }
                            style=format!(
                                "top:{top:.3}%; height:{height:.3}%; \
                                 left:calc({left:.3}% + 1px); width:calc({width:.3}% - 2px); \
                                 border-left-color:{color}",
                            )
                        >
                            <span class="cal-tg-btime">{time}</span>
                            <span class="cal-tg-bsum">{title}</span>
                        </button>
                    }
                })
                .collect::<Vec<_>>();
            // The moving current-time line — only in the column whose day is
            // "now", positioned like a zero-height event. The dot is a CSS
            // pseudo-element; the label repeats the time for a quick read.
            let now_line = (dn == now.0).then(|| {
                let top = now.1 as f64 / 1440.0 * 100.0;
                let label = format!("{:02}:{:02}", now.1 / 60, now.1 % 60);
                view! {
                    <div class="cal-tg-now" style=format!("top:{top:.3}%") aria-hidden="true">
                        <span class="cal-tg-now-label">{label}</span>
                    </div>
                }
            });
            view! {
                <div
                    class="cal-tg-col"
                    class:cal-tg-today=(dn == today_dn)
                    title="Click to add an event"
                    on:click=move |click: leptos::ev::MouseEvent| {
                        let Some(target) = click.current_target() else {
                            return;
                        };
                        let Ok(element) = target.dyn_into::<web_sys::Element>() else {
                            return;
                        };
                        let rect = element.get_bounding_client_rect();
                        if rect.height() <= 0.0 {
                            return;
                        }
                        let ratio = ((f64::from(click.client_y()) - rect.top()) / rect.height())
                            .clamp(0.0, 1.0);
                        let minute = ((ratio * 1440.0) as i64 / 15 * 15).min(1425);
                        on_create(dn, minute);
                    }
                >
                    {blocks}
                    {now_line}
                </div>
            }
        })
        .collect::<Vec<_>>();

    view! {
        <div class="cal-tg" class:cal-tg-single=single>
            <div class="cal-tg-head">
                <div class="cal-tg-corner"></div>
                <div class="cal-tg-headcells" style=cols_style.clone()>
                    {heads}
                </div>
            </div>
            {(allday_total > 0)
                .then(|| {
                    view! {
                        <div class="cal-tg-allday">
                            <div class="cal-tg-allgutter">"all-day"</div>
                            <div class="cal-tg-allbody">
                                {allday_rows}
                                {(allday_more > 0)
                                    .then(|| {
                                        view! {
                                            <div class="cal-tg-allmore">
                                                {format!("+{allday_more} more")}
                                            </div>
                                        }
                                    })}
                            </div>
                        </div>
                    }
                })}
            <div class="cal-tg-grid">
                <div class="cal-tg-hours">{hours}</div>
                <div class="cal-tg-cols" style=cols_style>
                    {cols}
                </div>
            </div>
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, cal: &str, start: &str, end: &str, summary: &str) -> Event {
        Event {
            id: id.to_string(),
            workspace_id: "w".to_string(),
            calendar_id: cal.to_string(),
            uid: format!("{id}-uid"),
            start: start.to_string(),
            end: end.to_string(),
            all_day: false,
            rrule: None,
            summary: summary.to_string(),
            location: None,
            attendees: Vec::new(),
            body: None,
            labels: Vec::new(),
            attachments: Vec::new(),
            etag: None,
            sequence: 0,
        }
    }

    #[test]
    fn day_key_takes_date_prefix() {
        assert_eq!(day_key("2026-06-13T09:00:00Z"), "2026-06-13");
        assert_eq!(day_key("garbage"), "garbage");
    }

    #[test]
    fn parse_labels_trims_dedups_and_drops_blanks() {
        // "Work"/"work" collapse (case-insensitive); blank dropped; casing kept.
        assert_eq!(
            parse_labels("Work, work ,  , Travel"),
            vec!["Work".to_string(), "Travel".to_string()]
        );
        assert!(parse_labels("  ,  ").is_empty());
    }

    #[test]
    fn label_counts_and_filter_membership() {
        let mut a = ev(
            "1",
            "c",
            "2026-06-13T09:00:00Z",
            "2026-06-13T10:00:00Z",
            "A",
        );
        a.labels = vec!["Work".to_string(), "Q3".to_string()];
        let mut b = ev(
            "2",
            "c",
            "2026-06-14T09:00:00Z",
            "2026-06-14T10:00:00Z",
            "B",
        );
        // "work" merges with "Work" case-insensitively; the repeat counts once.
        b.labels = vec!["work".to_string(), "Work ".to_string()];
        let labels = label_counts([&a, &b]);
        // Sorted, deduped (first-seen casing kept), counted per event.
        assert_eq!(labels, vec![("Q3".to_string(), 1), ("Work".to_string(), 2)]);
        assert!(event_has_label(&a, "work")); // case-insensitive match
        assert!(!event_has_label(&a, "travel"));
    }

    #[test]
    fn guess_content_type_only_images() {
        assert_eq!(guess_content_type("a.png").as_deref(), Some("image/png"));
        assert_eq!(guess_content_type("a.JPEG").as_deref(), Some("image/jpeg"));
        assert_eq!(guess_content_type("a.pdf"), None);
        assert_eq!(guess_content_type("noext"), None);
    }

    #[test]
    fn datetime_local_parses_to_fields() {
        // `local_input_to_rfc3339` itself shifts local→UTC via the browser Date,
        // which is wasm-only; the parse/validate half is pure, so test that.
        assert_eq!(
            parse_datetime_local("2026-06-18T09:00"),
            Some((2026, 6, 18, 9, 0))
        );
        // Seconds in the input are dropped to the minute (the picker's grain).
        assert_eq!(
            parse_datetime_local("2026-06-18T09:00:30"),
            Some((2026, 6, 18, 9, 0))
        );
        assert_eq!(parse_datetime_local(""), None);
        assert_eq!(parse_datetime_local("2026-06-18"), None);
        // Out-of-range clock components are rejected.
        assert_eq!(parse_datetime_local("2026-06-18T24:00"), None);
        assert_eq!(parse_datetime_local("2026-06-18T09:60"), None);
    }

    #[test]
    fn all_day_input_pins_date_midnight_utc_unshifted() {
        // An edited all-day endpoint keeps the entered *date* at midnight UTC,
        // no timezone shift — so it never slides onto an adjacent day the way a
        // local→UTC conversion would in a non-UTC browser.
        assert_eq!(
            all_day_input_to_rfc3339("2026-07-10T00:00").as_deref(),
            Some("2026-07-10T00:00:00Z")
        );
        // The time component is irrelevant for an all-day date — only the date
        // survives (the store snaps to midnight UTC either way).
        assert_eq!(
            all_day_input_to_rfc3339("2026-07-10T13:45").as_deref(),
            Some("2026-07-10T00:00:00Z")
        );
        assert_eq!(all_day_input_to_rfc3339(""), None);
        assert_eq!(all_day_input_to_rfc3339("2026-07-10"), None);
    }

    #[test]
    fn calendar_writability() {
        let local = Calendar {
            id: "c".into(),
            workspace_id: "w".into(),
            connection_id: None,
            external_id: "x".into(),
            name: "Personal".into(),
            read_only: false,
        };
        assert!(local.is_local() && local.is_writable());

        // A writable provider calendar (CalDAV/Google/Outlook) IS editable now —
        // the server writes the edit back to the provider (SOUL §8).
        let provider = Calendar {
            connection_id: Some("conn".into()),
            ..local.clone()
        };
        assert!(!provider.is_local() && provider.is_writable());

        // Read-only stays read-only, local or provider (webcal subscriptions).
        let ro_local = Calendar {
            read_only: true,
            ..local.clone()
        };
        assert!(ro_local.is_local() && !ro_local.is_writable());
        let ro_provider = Calendar {
            connection_id: Some("conn".into()),
            read_only: true,
            ..local
        };
        assert!(!ro_provider.is_writable());
    }

    #[test]
    fn groups_consecutive_days() {
        let evs = vec![
            ev(
                "a",
                "c",
                "2026-06-13T09:00:00Z",
                "2026-06-13T10:00:00Z",
                "A",
            ),
            ev(
                "b",
                "c",
                "2026-06-13T11:00:00Z",
                "2026-06-13T12:00:00Z",
                "B",
            ),
            ev(
                "c",
                "c",
                "2026-06-14T09:00:00Z",
                "2026-06-14T10:00:00Z",
                "C",
            ),
        ];
        let groups = group_by_day(&evs);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].date, "2026-06-13");
        assert_eq!(groups[0].events.len(), 2);
        assert_eq!(groups[1].date, "2026-06-14");
        assert_eq!(groups[1].events.len(), 1);
    }

    #[test]
    fn time_range_formats() {
        assert_eq!(
            format_time_range("2026-06-13T09:00:00Z", "2026-06-13T10:30:00Z"),
            "09:00 – 10:30"
        );
        assert_eq!(
            format_time_range("2026-06-13T00:00:00Z", "2026-06-14T00:00:00Z"),
            "All day"
        );
        assert_eq!(
            format_time_range("2026-06-13T09:00:00Z", "2026-06-13T09:00:00Z"),
            "09:00"
        );
    }

    #[test]
    fn weekday_is_correct() {
        // 2026-06-13 is a Saturday.
        assert_eq!(weekday_name(2026, 6, 13), Some("Saturday"));
        // 2000-01-01 is a Saturday.
        assert_eq!(weekday_name(2000, 1, 1), Some("Saturday"));
        // 1970-01-01 is a Thursday.
        assert_eq!(weekday_name(1970, 1, 1), Some("Thursday"));
    }

    #[test]
    fn day_heading_is_friendly() {
        assert_eq!(format_day_heading("2026-06-13"), "Saturday, 13 June 2026");
        assert_eq!(format_day_heading("nope"), "nope");
    }

    // --- Grid date math ----------------------------------------------------

    fn ev_span(start: &str, end: &str) -> Event {
        ev("i", "c", start, end, "s")
    }

    #[test]
    fn civil_days_roundtrip() {
        for &(y, m, d) in &[
            (1970, 1, 1),
            (2000, 1, 1),
            (2024, 2, 29),
            (2026, 6, 13),
            (1999, 12, 31),
            (2027, 3, 1),
            (1900, 1, 1),
        ] {
            let z = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(z), (y, m, d), "roundtrip {y}-{m}-{d}");
        }
        // 1970-01-01 is day 0; the next day is 1.
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
    }

    #[test]
    fn weekday_and_week_start_monday_first() {
        // 2026-06-13 is a Saturday.
        let sat = days_from_civil(2026, 6, 13);
        assert_eq!(weekday_sun0(sat), 6); // 0=Sun..6=Sat
        assert_eq!(monday_index(sat), 5); // Sat is the 6th Monday-first column
                                          // Its week starts on Monday 2026-06-08.
        let ws = week_start(sat);
        assert_eq!(civil_from_days(ws), (2026, 6, 8));
        assert_eq!(monday_index(ws), 0);
    }

    #[test]
    fn month_cells_are_42_monday_aligned() {
        let cells = month_cells(days_from_civil(2026, 6, 15));
        assert_eq!(cells.len(), 42);
        assert_eq!(monday_index(cells[0]), 0);
        // June 2026 starts on a Monday, so the first cell is June 1.
        assert_eq!(civil_from_days(cells[0]), (2026, 6, 1));
        // Cells are consecutive days.
        assert_eq!(cells[41] - cells[0], 41);
    }

    #[test]
    fn add_months_wraps_across_years() {
        let dec = days_from_civil(2026, 12, 10);
        assert_eq!(civil_from_days(add_months(dec, 1)), (2027, 1, 1));
        let jan = days_from_civil(2026, 1, 10);
        assert_eq!(civil_from_days(add_months(jan, -1)), (2025, 12, 1));
        // A 31st anchored into February lands on the 1st (no overflow).
        let jan31 = days_from_civil(2026, 1, 31);
        assert_eq!(civil_from_days(add_months(jan31, 1)), (2026, 2, 1));
    }

    #[test]
    fn event_span_drops_exclusive_midnight_end() {
        // A single all-day event (00:00 → next 00:00) covers exactly one day.
        let (s, l) =
            event_day_span(&ev_span("2026-06-13T00:00:00Z", "2026-06-14T00:00:00Z")).unwrap();
        assert_eq!(civil_from_days(s), (2026, 6, 13));
        assert_eq!(civil_from_days(l), (2026, 6, 13));
        // A multi-day timed event covers each calendar day it touches.
        let (s2, l2) =
            event_day_span(&ev_span("2026-06-13T09:00:00Z", "2026-06-15T17:00:00Z")).unwrap();
        assert_eq!(l2 - s2, 2);
        // A within-day event covers just that day.
        let (s3, l3) =
            event_day_span(&ev_span("2026-06-13T09:00:00Z", "2026-06-13T10:00:00Z")).unwrap();
        assert_eq!(s3, l3);
    }

    #[test]
    fn all_day_classification() {
        assert!(is_all_day_block(&ev_span(
            "2026-06-13T00:00:00Z",
            "2026-06-14T00:00:00Z"
        )));
        assert!(!is_all_day_block(&ev_span(
            "2026-06-13T09:00:00Z",
            "2026-06-13T10:00:00Z"
        )));
        // The API's `all_day` flag wins even when the stamps are not a clean
        // midnight-to-midnight span (e.g. a chat-created 09:00–17:00 all-day).
        let mut flagged = ev_span("2026-06-13T09:00:00Z", "2026-06-13T17:00:00Z");
        flagged.all_day = true;
        assert!(is_all_day_block(&flagged));
    }

    #[test]
    fn minutes_from_timestamp() {
        assert_eq!(minutes_of_ts("2026-06-13T09:30:00Z"), 570);
        assert_eq!(minutes_of_ts("2026-06-13T00:00:00Z"), 0);
        assert_eq!(minutes_of_ts("2026-06-13T23:59:00Z"), 1439);
    }

    #[test]
    fn lanes_pack_overlaps_and_reuse_columns() {
        // Three mutually overlapping events need three columns.
        let lanes = pack_lanes(&[(0, 60), (30, 90), (45, 120)]);
        assert_eq!(lanes.iter().map(|&(_, t)| t).max(), Some(3));
        assert_eq!(lanes, vec![(0, 3), (1, 3), (2, 3)]);
        // Back-to-back (touching) events reuse one column.
        assert_eq!(pack_lanes(&[(0, 60), (60, 120)]), vec![(0, 1), (0, 1)]);
        // A short event next to a long one packs into 2 columns, second freed.
        let lanes = pack_lanes(&[(0, 120), (0, 30), (40, 60)]);
        assert_eq!(lanes[0], (0, 2));
        assert_eq!(lanes[1], (1, 2));
        assert_eq!(lanes[2], (1, 2)); // reuses column 1 after (0,30) ends
    }

    #[test]
    fn week_title_collapses_shared_parts() {
        // Same month + year.
        assert_eq!(week_title(days_from_civil(2026, 6, 8)), "8 – 14 June 2026");
        // Crosses a month boundary, same year.
        assert_eq!(
            week_title(days_from_civil(2026, 6, 29)),
            "29 June – 5 July 2026"
        );
        // Crosses a year boundary.
        assert_eq!(
            week_title(days_from_civil(2025, 12, 29)),
            "29 December 2025 – 4 January 2026"
        );
    }

    #[test]
    fn cal_color_is_stable_and_hsl() {
        let a = cal_color("calendar-123");
        assert_eq!(a, cal_color("calendar-123"));
        assert!(a.starts_with("hsl(") && a.ends_with(')'));
    }

    #[test]
    fn ymd_string_pads() {
        assert_eq!(ymd_string(days_from_civil(2026, 6, 1)), "2026-06-01");
        assert_eq!(ymd_string(days_from_civil(789, 1, 9)), "0789-01-09");
    }

    #[test]
    fn planner_datetime_defaults_and_rolls_over_midnight() {
        let day = days_from_civil(2026, 8, 13);
        assert_eq!(datetime_local_value(day, 9 * 60 + 15), "2026-08-13T09:15");
        assert_eq!(datetime_local_value(day, 24 * 60 + 30), "2026-08-14T00:30");
    }

    #[test]
    fn dated_calendar_routes_restore_view_and_anchor() {
        let (view, anchor) = calendar_state_from_path("/app/calendar/month/2027-02").unwrap();
        assert_eq!(view, ViewMode::Month);
        assert_eq!(civil_from_days(anchor.unwrap()), (2027, 2, 1));

        let (view, anchor) = calendar_state_from_path("/app/calendar/week/2026-06-08").unwrap();
        assert_eq!(view, ViewMode::Week);
        assert_eq!(civil_from_days(anchor.unwrap()), (2026, 6, 8));

        let (view, anchor) = calendar_state_from_path("/app/calendar/day/2026-06-13").unwrap();
        assert_eq!(view, ViewMode::Day);
        assert_eq!(civil_from_days(anchor.unwrap()), (2026, 6, 13));
        assert!(calendar_state_from_path("/app/calendar/month/2026-13").is_none());
        assert!(calendar_state_from_path("/app/calendar/day").is_none());
    }
}
