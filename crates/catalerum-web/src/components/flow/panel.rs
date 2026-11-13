use super::*;

// ---------------------------------------------------------------------------
// The per-node config panel.
// ---------------------------------------------------------------------------

/// The side panel that edits the selected node: a typed trigger builder, an action
/// kind + params editor, or a code/condition runtime + source editor, plus a delete
/// button. Reads/writes the node in place on the shared `graph` signal.
#[component]
pub(super) fn ConfigPanel(
    graph: RwSignal<FlowGraph>,
    selected: RwSignal<Option<String>>,
    on_delete: impl Fn() + Copy + Send + Sync + 'static,
    on_duplicate: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    // The currently-selected node, recomputed from the graph.
    let node = move || {
        let id = selected.get()?;
        graph.with(|g| g.nodes.iter().find(|n| n.id == id).cloned())
    };

    view! {
        // On mobile this is a bottom sheet: `flow-config-open` (set only when a node
        // is selected) slides it up over the canvas; otherwise it stays off-screen so
        // the canvas keeps full height. On desktop it's a static right rail.
        <aside class=move || {
            if node().is_some() { "flow-config flow-config-open" } else { "flow-config" }
        }>
            <Show
                when=move || node().is_some()
                fallback=|| {
                    view! {
                        <div class="flow-config-empty">
                            "Select a node to edit it, or add one from the palette."
                        </div>
                    }
                }
            >
                {move || {
                    // Rebuild the editor only when the SELECTED node changes — never
                    // when its data mutates. The typed editors below own their input
                    // state locally and write straight back to `graph` on every
                    // keystroke; if this closure tracked `graph` (via `node()`), each
                    // keystroke would tear the whole editor down and rebuild it —
                    // dropping the focused input's caret and re-firing the collect
                    // configs' `spawn_local` connection fetch per character. The Poll
                    // cadence / mailbox fields (nested a dynamic layer deeper inside
                    // `trigger_config`) felt this worst. So track `selected` only and
                    // read the node's data untracked. A node's `FlowKind` variant is
                    // fixed for its id, and sub-kind switches are handled by the
                    // editors' own local signals, so nothing here needs `graph`.
                    let Some(id) = selected.get() else { return ().into_any() };
                    let Some(n) = graph
                        .with_untracked(|g| g.nodes.iter().find(|n| n.id == id).cloned())
                    else {
                        return ().into_any();
                    };
                    let head = view! {
                        <div class="flow-config-head">
                            <span class="flow-config-title">
                                {format!("{} · {}", n.kind.tag(), n.id)}
                            </span>
                            <div class="flow-cfg-head-btns">
                                <button
                                    class="flow-cfg-btn"
                                    type="button"
                                    title="Duplicate this node"
                                    on:click=move |_| on_duplicate()
                                >
                                    "Duplicate"
                                </button>
                                // Mobile-only: dismiss the bottom sheet (deselect). On
                                // desktop the rail is always shown, so it's hidden.
                                <button
                                    class="flow-cfg-btn flow-cfg-close"
                                    type="button"
                                    title="Close"
                                    on:click=move |_| selected.set(None)
                                >
                                    <Icon icon=MdIcon::Close />
                                </button>
                                <button
                                    class="flow-cfg-btn flow-cfg-del"
                                    type="button"
                                    on:click=move |_| on_delete()
                                >
                                    "Delete"
                                </button>
                            </div>
                        </div>
                    };
                    let body = match &n.kind {
                        FlowKind::Trigger { trigger } => {
                            trigger_config(graph, id.clone(), trigger.clone()).into_any()
                        }
                        FlowKind::Action { action } => {
                            action_config(graph, id.clone(), action.clone()).into_any()
                        }
                        FlowKind::Code { runtime, source }
                        | FlowKind::Condition { runtime, source } => {
                            let is_cond = matches!(n.kind, FlowKind::Condition { .. });
                            code_config(
                                    graph,
                                    id.clone(),
                                    runtime.clone(),
                                    source.clone(),
                                    is_cond,
                                )
                                .into_any()
                        }
                        FlowKind::ForEach { .. } => {
                            for_each_config(graph, id.clone()).into_any()
                        }
                        FlowKind::LoopEnd { for_each } => {
                            loop_end_config(graph, id.clone(), for_each.clone()).into_any()
                        }
                    };
                    view! {
                        <div class="flow-config-body">
                            {head}
                            {body}
                        </div>
                    }
                        .into_any()
                }}
            </Show>
        </aside>
    }
}

/// Trigger-node config: a kind `<select>` + **typed fields** per kind (channel,
/// cron, path, mailbox + filters, …), shared with the Raw-mode trigger builder via
/// [`trigger_fields`]. Each field **merges** into the trigger object, preserving any
/// opaque predicate (e.g. a channel `filter`) the typed form doesn't model; changing
/// the kind resets the trigger to that kind. Opaque-only kinds (`calendar_event`)
/// show a hint instead.
fn trigger_config(graph: RwSignal<FlowGraph>, id: String, trigger: Value) -> impl IntoView {
    let current_kind = trigger
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("webhook")
        .to_string();
    let kind_sig = RwSignal::new(current_kind);
    let id_for_kind = id.clone();
    let id_for_body = id.clone();

    view! {
        <label class="flow-cfg-label">"Trigger kind"</label>
        <select
            class="flow-cfg-input"
            on:change=move |ev| {
                let k = event_target_value(&ev);
                // Switching kind resets the trigger to that bare kind (its fields differ).
                set_node_trigger(graph, &id_for_kind, serde_json::json!({ "kind": k }));
                kind_sig.set(k);
            }
            prop:value=move || kind_sig.get()
        >
            {TRIGGER_KINDS
                .iter()
                .map(|(k, label)| {
                    view! { <option value=*k>{*label}</option> }
                })
                .collect::<Vec<_>>()}
        </select>
        {move || {
            let id = id_for_body.clone();
            let kind = kind_sig.get();
            // A collect source trigger configures its email/calendar source inline
            // (SOUL §28: no global settings) + wires its `commit` port to a write
            // node — so it gets a typed config, not the generic field list.
            if kind == "collect_email" {
                return collect_email_config(graph, id).into_any();
            }
            if kind == "collect_calendar" {
                return collect_calendar_config(graph, id).into_any();
            }
            if kind == "collect_sql" {
                return collect_sql_config(graph, id).into_any();
            }
            let fields = trigger_fields(&kind);
            if fields.is_empty() {
                return view! {
                    <div class="flow-cfg-hint">
                        "This trigger fires on its kind alone — no extra fields. (Advanced predicates can be set in Raw mode.)"
                    </div>
                }
                .into_any();
            }
            let trigger = node_trigger(graph, &id);
            fields
                .iter()
                .map(|(key, label, _required, multiline)| {
                    let key = *key;
                    let sig = RwSignal::new(trigger_field_display(&trigger, key));
                    let id2 = id.clone();
                    let input = if *multiline {
                        view! {
                            <textarea
                                class="flow-cfg-area"
                                prop:value=move || sig.get()
                                on:input=move |ev| {
                                    let v = event_target_value(&ev);
                                    sig.set(v.clone());
                                    set_trigger_field(graph, &id2, key, &v);
                                }
                            ></textarea>
                        }
                        .into_any()
                    } else {
                        view! {
                            <input
                                class="flow-cfg-input"
                                prop:value=move || sig.get()
                                on:input=move |ev| {
                                    let v = event_target_value(&ev);
                                    sig.set(v.clone());
                                    set_trigger_field(graph, &id2, key, &v);
                                }
                            />
                        }
                        .into_any()
                    };
                    view! {
                        <div class="flow-cfg-field">
                            <label class="flow-cfg-label">{*label}</label>
                            {input}
                        </div>
                    }
                })
                .collect::<Vec<_>>()
                .into_any()
        }}
    }
}

/// "Use an existing source" picker shared by both collect configs (SOUL §28):
/// most collect nodes should point at a source that already exists — re-entering
/// credentials per node was the only path before. `existing` is the
/// already-kind-filtered list; picking hands the chosen connection to `on_pick`
/// (which writes the id onto the trigger and flips the node to configured).
/// Renders nothing while the list is empty.
fn existing_source_picker(
    existing: RwSignal<Vec<Connection>>,
    on_pick: impl Fn(Connection) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <Show when=move || !existing.get().is_empty() fallback=|| ().into_view()>
            <div class="flow-cfg-field">
                <label class="flow-cfg-label">"Use an existing source"</label>
                <select
                    class="flow-cfg-input"
                    on:change=move |ev| {
                        let id = event_target_value(&ev);
                        if let Some(c) = existing
                            .get_untracked()
                            .into_iter()
                            .find(|c| c.id == id)
                        {
                            on_pick(c);
                        }
                    }
                >
                    <option value="" selected>"— pick a source —"</option>
                    {move || {
                        existing
                            .get()
                            .into_iter()
                            .map(|c| {
                                let text = if c.collecting {
                                    c.name.clone()
                                } else {
                                    format!("{} · idle", c.name)
                                };
                                view! { <option value=c.id.clone()>{text}</option> }
                            })
                            .collect::<Vec<_>>()
                    }}
                </select>
                <div class="flow-cfg-hint">"…or create a new one below."</div>
            </div>
        </Show>
    }
}

/// A labelled text/password input bound to a string signal (one config row).
fn cfg_field(
    label: &'static str,
    placeholder: &'static str,
    sig: RwSignal<String>,
    password: bool,
) -> impl IntoView {
    view! {
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">{label}</label>
            <input
                class="flow-cfg-input"
                r#type=if password { "password" } else { "text" }
                placeholder=placeholder
                prop:value=move || sig.get()
                on:input=move |ev| sig.set(event_target_value(&ev))
            />
        </div>
    }
}

/// The **poll cadence** (`every`) row shared by both collect source configs (SOUL
/// §29). A free text input accepting the documented shapes — a bare number of minutes,
/// a compact duration string (`5m`, `1h30m`), or `{"seconds":N}` — persisted verbatim
/// onto the trigger's `every` field via [`set_trigger_every`]. A soft client-side
/// pattern check ([`every_shape_ok`]) warns on an unrecognized shape but NEVER blocks
/// saving; the server re-parses and clamps `[60s, 1 year]` at scan time.
fn collect_every_field(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let sig = RwSignal::new(every_display(&node_trigger(graph, &id)));
    let id_sv = StoredValue::new(id);
    view! {
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Poll cadence (optional)"</label>
            <input
                class="flow-cfg-input"
                placeholder=r#"e.g. 5m, 1h30m, or {"seconds":90}"#
                prop:value=move || sig.get()
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    sig.set(v.clone());
                    set_trigger_every(graph, &id_sv.get_value(), &v);
                }
            />
            <Show
                when=move || {
                    let v = sig.get();
                    !v.trim().is_empty() && !every_shape_ok(&v)
                }
                fallback=|| ().into_view()
            >
                <div class="flow-cfg-warn">
                    "Unrecognized cadence shape — saved as-is. Use minutes (5m, 1h30m), a bare number of minutes, or {\"seconds\":N}."
                </div>
            </Show>
            <div class="flow-cfg-hint">
                "How often to poll. Clamped to 60s–1 year at scan time; blank = every scheduler tick (60s)."
            </div>
        </div>
    }
}

/// Inline **email source** config for a `CollectEmail` node (SOUL §28: the source
/// is set up in the node, not a global settings tab). A provider form whose "Create
/// source" button creates the connection and writes its id into the trigger's
/// `connection`; plus an optional mailbox filter. The cursor-commit write is wired
/// via the node's `commit` port, not typed here.
fn collect_email_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let trigger = node_trigger(graph, &id);
    // The node id lives in a Copy `StoredValue` so the event-handler closures (which
    // need it) stay `Fn`/`Copy` rather than moving a `String` out of the view body.
    let id_sv = StoredValue::new(id);
    let initial = trigger
        .get("connection")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let configured = RwSignal::new(!initial.is_empty());
    let conn_label = RwSignal::new(initial.clone());

    // The workspace's existing email sources: fed to the reuse picker, and used to
    // resolve an already-configured trigger's connection id to its NAME (the ✓
    // badge showed a raw uuid before).
    let existing = RwSignal::new(Vec::<Connection>::new());
    spawn_local(async move {
        let tok = auth::resolve_token();
        if let Ok(list) = rest::list_email_connections(tok.as_deref()).await {
            if let Some(c) = list.iter().find(|c| c.id == initial) {
                conn_label.set(c.name.clone());
            }
            existing.set(list);
        }
    });

    let provider = RwSignal::new(EmailProviderKind::Maildir);
    let name = RwSignal::new(String::new());
    let root = RwSignal::new(String::new());
    let folder = RwSignal::new(String::new());
    let host = RwSignal::new(String::new());
    let port = RwSignal::new(String::new());
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let session_url = RwSignal::new(String::new());
    let token = RwSignal::new(String::new());
    let account_id = RwSignal::new(String::new());
    let client_id = RwSignal::new(String::new());
    let client_secret = RwSignal::new(String::new());
    let refresh_token = RwSignal::new(String::new());
    let label = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let err = RwSignal::new(Option::<String>::None);
    let mailbox = RwSignal::new(
        trigger
            .get("mailbox")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    );
    // Edit-an-existing-source mode (SOUL §28): `editing` holds the connection id
    // being edited (the form is prefilled from its stored, secret-free settings
    // and the submit PUTs instead of POSTing); `edit_provider` remembers the
    // stored provider so secrets may be left blank (= keep) only while the
    // provider is unchanged — a provider switch needs fresh credentials.
    let editing = RwSignal::new(Option::<String>::None);
    let edit_provider = RwSignal::new(Option::<EmailProviderKind>::None);

    let opt = move |s: RwSignal<String>| {
        let v = s.get_untracked().trim().to_string();
        (!v.is_empty()).then_some(v)
    };
    // Prefill the provider form from the stored source and enter edit mode. The
    // connection id is read fresh off the trigger (the picker may have changed it).
    let start_edit = move || {
        let trig = node_trigger(graph, &id_sv.get_value());
        let Some(cid) = trig
            .get("connection")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return;
        };
        err.set(None);
        busy.set(true);
        spawn_local(async move {
            let tok = auth::resolve_token();
            match rest::get_email_connection(tok.as_deref(), &cid).await {
                Ok(d) => {
                    let prov = EmailProviderKind::parse_token(&d.provider).unwrap_or_default();
                    provider.set(prov);
                    name.set(d.name.clone());
                    root.set(d.setting("root"));
                    // Maildir stores its folder under `name`; IMAP under `mailbox`.
                    folder.set(match prov {
                        EmailProviderKind::Maildir => d.setting("name"),
                        _ => d.setting("mailbox"),
                    });
                    host.set(d.setting("host"));
                    port.set(d.setting("port"));
                    username.set(d.setting("username"));
                    session_url.set(d.setting("session_url"));
                    account_id.set(d.setting("account_id"));
                    client_id.set(d.setting("client_id"));
                    label.set(d.setting("label"));
                    // Secrets never arrive — blank means "keep the stored one".
                    password.set(String::new());
                    token.set(String::new());
                    client_secret.set(String::new());
                    refresh_token.set(String::new());
                    edit_provider.set(Some(prov));
                    editing.set(Some(d.id));
                    configured.set(false);
                }
                Err(e) => err.set(Some(format!("Could not load the source: {e}"))),
            }
            busy.set(false);
        });
    };
    // Create the source (POST) — or, in edit mode, save it (PUT). When editing
    // with the provider unchanged, blank secret fields keep the stored secrets
    // (the server back-fills them); otherwise secrets are required as on create.
    let submit = move || {
        let prov = provider.get_untracked();
        let nm = name.get_untracked().trim().to_string();
        err.set(None);
        if nm.is_empty() {
            err.set(Some("Give the source a name.".into()));
            return;
        }
        let editing_id = editing.get_untracked();
        let secrets_kept = editing_id.is_some() && edit_provider.get_untracked() == Some(prov);
        let mut body = CreateEmailConnection {
            provider: prov,
            name: nm,
            ..Default::default()
        };
        let missing: Option<&str> = match prov {
            EmailProviderKind::Maildir => match opt(root) {
                Some(r) => {
                    body.root = r;
                    body.mailbox = opt(folder);
                    None
                }
                None => Some("Enter the Maildir directory (the folder with new/ cur/ tmp/)."),
            },
            EmailProviderKind::Imap => match (opt(host), opt(username), opt(password)) {
                (Some(h), Some(u), p) if p.is_some() || secrets_kept => {
                    body.host = Some(h);
                    body.username = Some(u);
                    body.password = p;
                    body.port = opt(port).and_then(|p| p.parse::<u16>().ok());
                    body.mailbox = opt(folder);
                    None
                }
                _ => Some("IMAP needs a host, username, and password."),
            },
            EmailProviderKind::Jmap => match (opt(session_url), opt(token)) {
                (Some(u), t) if t.is_some() || secrets_kept => {
                    body.session_url = Some(u);
                    body.token = t;
                    body.account_id = opt(account_id);
                    None
                }
                _ => Some("JMAP needs a session URL and a bearer token."),
            },
            EmailProviderKind::Gmail => {
                match (opt(client_id), opt(client_secret), opt(refresh_token)) {
                    (Some(c), s, r) if (s.is_some() && r.is_some()) || secrets_kept => {
                        body.client_id = Some(c);
                        body.client_secret = s;
                        body.refresh_token = r;
                        body.label = opt(label);
                        None
                    }
                    _ => Some("Gmail needs a client id, client secret, and refresh token."),
                }
            }
        };
        if let Some(m) = missing {
            err.set(Some(m.into()));
            return;
        }
        busy.set(true);
        let id2 = id_sv.get_value();
        spawn_local(async move {
            let tok = auth::resolve_token();
            let result = match &editing_id {
                Some(cid) => rest::update_email_connection(tok.as_deref(), cid, &body).await,
                None => rest::create_email_connection(tok.as_deref(), &body).await,
            };
            match result {
                Ok(c) => {
                    set_trigger_field(graph, &id2, "connection", &c.id);
                    conn_label.set(c.name.clone());
                    // Keep the reuse picker's entry fresh (a rename shows at once).
                    existing.update(|list| {
                        if let Some(e) = list.iter_mut().find(|e| e.id == c.id) {
                            e.name = c.name.clone();
                        }
                    });
                    editing.set(None);
                    edit_provider.set(None);
                    configured.set(true);
                    busy.set(false);
                }
                Err(e) => {
                    err.set(Some(e.to_string()));
                    busy.set(false);
                }
            }
        });
    };
    let is = move |k: EmailProviderKind| move || provider.get() == k;

    view! {
        <Show when=move || configured.get() fallback=|| ().into_view()>
            <div class="flow-cfg-hint">
                {move || format!("✓ Email source configured ({})", conn_label.get())}
            </div>
            <button
                class="flow-cfg-btn"
                disabled=move || busy.get()
                title="View and change this source's settings"
                on:click=move |_| start_edit()
            >
                {move || if busy.get() { "Loading…" } else { "View / edit source" }}
            </button>
            <button
                class="flow-cfg-btn"
                on:click=move |_| {
                    editing.set(None);
                    edit_provider.set(None);
                    configured.set(false);
                }
            >
                "Switch source"
            </button>
        </Show>
        <Show when=move || !configured.get() fallback=|| ().into_view()>
            <Show when=move || editing.with(Option::is_none) fallback=|| ().into_view()>
                {existing_source_picker(existing, move |c: Connection| {
                    set_trigger_field(graph, &id_sv.get_value(), "connection", &c.id);
                    conn_label.set(c.name);
                    configured.set(true);
                })}
            </Show>
            <Show when=move || editing.with(Option::is_some) fallback=|| ().into_view()>
                <div class="flow-cfg-hint">
                    "Editing this source — leave the secret fields blank to keep the stored values."
                </div>
                <button
                    class="flow-cfg-btn"
                    on:click=move |_| {
                        editing.set(None);
                        edit_provider.set(None);
                        err.set(None);
                        configured.set(true);
                    }
                >
                    "Cancel edit"
                </button>
            </Show>
            <label class="flow-cfg-label">"Email provider"</label>
            <select
                class="flow-cfg-input"
                on:change=move |ev| {
                    if let Some(k) = EmailProviderKind::parse_token(&event_target_value(&ev)) {
                        provider.set(k);
                    }
                }
            >
                {EmailProviderKind::all()
                    .into_iter()
                    .map(|k| {
                        let sel = move || provider.get() == k;
                        view! { <option value=k.as_str() selected=sel>{k.label()}</option> }
                    })
                    .collect::<Vec<_>>()}
            </select>
            {cfg_field("Name", "Personal inbox", name, false)}
            <Show when=is(EmailProviderKind::Maildir) fallback=|| ().into_view()>
                {cfg_field("Maildir directory", "/home/me/Mail/INBOX", root, false)}
                {cfg_field("Mailbox name (optional)", "INBOX", folder, false)}
            </Show>
            <Show when=is(EmailProviderKind::Imap) fallback=|| ().into_view()>
                {cfg_field("Host", "imap.example.com", host, false)}
                {cfg_field("Port (optional, default 993)", "993", port, false)}
                {cfg_field("Username", "me@example.com", username, false)}
                {cfg_field("Password", "", password, true)}
                {cfg_field("Folder (optional)", "INBOX", folder, false)}
            </Show>
            <Show when=is(EmailProviderKind::Jmap) fallback=|| ().into_view()>
                {cfg_field("Session URL", "https://api.fastmail.com/jmap/session", session_url, false)}
                {cfg_field("Bearer token", "", token, true)}
                {cfg_field("Account id (optional)", "", account_id, false)}
            </Show>
            <Show when=is(EmailProviderKind::Gmail) fallback=|| ().into_view()>
                {cfg_field("Client id", "", client_id, false)}
                {cfg_field("Client secret", "", client_secret, true)}
                {cfg_field("Refresh token", "", refresh_token, true)}
                {cfg_field("Label (optional)", "INBOX", label, false)}
            </Show>
            <button class="flow-cfg-btn" disabled=move || busy.get() on:click=move |_| submit()>
                {move || match (busy.get(), editing.with(Option::is_some)) {
                    (true, true) => "Saving…",
                    (true, false) => "Creating…",
                    (false, true) => "Save changes",
                    (false, false) => "Create source",
                }}
            </button>
        </Show>
        <Show when=move || err.with(Option::is_some) fallback=|| ().into_view()>
            <div class="flow-cfg-err">{move || err.get().unwrap_or_default()}</div>
        </Show>
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Mailbox filter (optional)"</label>
            <input
                class="flow-cfg-input"
                placeholder="INBOX"
                prop:value=move || mailbox.get()
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    mailbox.set(v.clone());
                    set_trigger_field(graph, &id_sv.get_value(), "mailbox", &v);
                }
            />
        </div>
        {collect_every_field(graph, id_sv.get_value())}
        <div class="flow-cfg-hint">
            "Wire the ⚡ commit port to the write node whose success advances the cursor (optional)."
        </div>
    }
}

/// Inline **calendar source** config for a `CollectCalendar` node — the calendar
/// twin of [`collect_email_config`]. Creates a calendar connection (local `.ics`
/// directory, CalDAV, or webcal) and writes its id into the trigger's `connection`.
fn collect_calendar_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let trigger = node_trigger(graph, &id);
    let id_sv = StoredValue::new(id);
    let initial = trigger
        .get("connection")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let configured = RwSignal::new(!initial.is_empty());
    let conn_label = RwSignal::new(initial.clone());

    // Existing calendar sources for the reuse picker (`GET /connections` returns
    // every kind — keep only calendars) + uuid → name resolution for the ✓ badge.
    let existing = RwSignal::new(Vec::<Connection>::new());
    spawn_local(async move {
        let tok = auth::resolve_token();
        if let Ok(list) = rest::list_connections(tok.as_deref()).await {
            let list: Vec<Connection> = list.into_iter().filter(|c| c.kind == "calendar").collect();
            if let Some(c) = list.iter().find(|c| c.id == initial) {
                conn_label.set(c.name.clone());
            }
            existing.set(list);
        }
    });

    let provider = RwSignal::new(CalendarProviderKind::Local);
    let name = RwSignal::new(String::new());
    let target = RwSignal::new(String::new());
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let err = RwSignal::new(Option::<String>::None);
    let calendar = RwSignal::new(
        trigger
            .get("calendar")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    );

    let opt = move |s: RwSignal<String>| {
        let v = s.get_untracked().trim().to_string();
        (!v.is_empty()).then_some(v)
    };
    let create = move || {
        let prov = provider.get_untracked();
        let nm = name.get_untracked().trim().to_string();
        err.set(None);
        if nm.is_empty() {
            err.set(Some("Give the source a name.".into()));
            return;
        }
        let Some(tgt) = opt(target) else {
            err.set(Some(
                "Enter the directory (local) or URL (CalDAV/webcal).".into(),
            ));
            return;
        };
        let mut config = Map::new();
        config.insert(prov.config_key().to_string(), Value::String(tgt));
        if matches!(prov, CalendarProviderKind::Caldav) {
            if let Some(u) = opt(username) {
                config.insert("username".to_string(), Value::String(u));
            }
            if let Some(p) = opt(password) {
                config.insert("password".to_string(), Value::String(p));
            }
        }
        let body = CreateConnection {
            kind: prov,
            name: nm,
            config: Value::Object(config),
            credentials: None,
        };
        busy.set(true);
        let id2 = id_sv.get_value();
        spawn_local(async move {
            let tok = auth::resolve_token();
            match rest::create_connection(tok.as_deref(), &body).await {
                Ok(c) => {
                    set_trigger_field(graph, &id2, "connection", &c.id);
                    conn_label.set(c.name);
                    configured.set(true);
                    busy.set(false);
                }
                Err(e) => {
                    err.set(Some(e.to_string()));
                    busy.set(false);
                }
            }
        });
    };
    let is = move |k: CalendarProviderKind| move || provider.get() == k;
    let is_local = move || provider.get() == CalendarProviderKind::Local;

    view! {
        <Show when=move || configured.get() fallback=|| ().into_view()>
            <div class="flow-cfg-hint">
                {move || format!("✓ Calendar source configured ({})", conn_label.get())}
            </div>
            <button class="flow-cfg-btn" on:click=move |_| configured.set(false)>
                "Reconfigure source"
            </button>
        </Show>
        <Show when=move || !configured.get() fallback=|| ().into_view()>
            {existing_source_picker(existing, move |c: Connection| {
                set_trigger_field(graph, &id_sv.get_value(), "connection", &c.id);
                conn_label.set(c.name);
                configured.set(true);
            })}
            <label class="flow-cfg-label">"Calendar provider"</label>
            <select
                class="flow-cfg-input"
                on:change=move |ev| {
                    if let Some(k) = CalendarProviderKind::parse_token(&event_target_value(&ev)) {
                        provider.set(k);
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
                        let sel = move || provider.get() == k;
                        view! { <option value=k.as_str() selected=sel>{k.label()}</option> }
                    })
                    .collect::<Vec<_>>()}
            </select>
            {cfg_field("Name", "Home calendar", name, false)}
            <Show when=is_local fallback=|| ().into_view()>
                {cfg_field("Directory (.ics files)", "/srv/calendars", target, false)}
            </Show>
            <Show when=move || !is_local() fallback=|| ().into_view()>
                {cfg_field("URL", "https://dav.example.com/cal/", target, false)}
            </Show>
            <Show when=is(CalendarProviderKind::Caldav) fallback=|| ().into_view()>
                {cfg_field("Username (optional)", "", username, false)}
                {cfg_field("Password (optional)", "", password, true)}
            </Show>
            <Show when=move || err.with(Option::is_some) fallback=|| ().into_view()>
                <div class="flow-cfg-err">{move || err.get().unwrap_or_default()}</div>
            </Show>
            <button class="flow-cfg-btn" disabled=move || busy.get() on:click=move |_| create()>
                {move || if busy.get() { "Creating…" } else { "Create source" }}
            </button>
        </Show>
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Calendar filter (optional)"</label>
            <input
                class="flow-cfg-input"
                placeholder="(every calendar on the connection)"
                prop:value=move || calendar.get()
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    calendar.set(v.clone());
                    set_trigger_field(graph, &id_sv.get_value(), "calendar", &v);
                }
            />
        </div>
        {collect_every_field(graph, id_sv.get_value())}
        <div class="flow-cfg-hint">
            "Wire the ⚡ commit port to the write node whose success advances the cursor (optional)."
        </div>
    }
}

/// Inline **external database source** config for a `CollectSql` node (SOUL
/// §11/§19): pick an existing external Postgres connection (they are registered
/// by an admin over `POST /db/connections` — there is no inline create form),
/// plus the tables wildcard pattern, an optional cursor column, and the poll
/// cadence. The cursor-commit node is wired via the node's `commit` port.
fn collect_sql_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let trigger = node_trigger(graph, &id);
    let id_sv = StoredValue::new(id);
    let initial = trigger
        .get("connection")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // The picked connection's display name (the raw uuid until the list resolves).
    let conn_label = RwSignal::new(initial.clone());
    let loaded = RwSignal::new(false);

    let existing = RwSignal::new(Vec::<Connection>::new());
    spawn_local(async move {
        let tok = auth::resolve_token();
        if let Ok(list) = rest::list_db_connections(tok.as_deref()).await {
            if let Some(c) = list.iter().find(|c| c.id == initial) {
                conn_label.set(c.name.clone());
            }
            existing.set(list);
        }
        loaded.set(true);
    });

    let on_pick = move |c: Connection| {
        set_trigger_field(graph, &id_sv.get_value(), "connection", &c.id);
        conn_label.set(c.name);
    };

    let tables = RwSignal::new(
        trigger
            .get("tables")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    );
    let cursor_column = RwSignal::new(
        trigger
            .get("cursor_column")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    );

    view! {
        {existing_source_picker(existing, on_pick)}
        <Show when=move || !conn_label.get().is_empty() fallback=|| ().into_view()>
            <div class="flow-cfg-hint">{move || format!("Source: {}", conn_label.get())}</div>
        </Show>
        <Show
            when=move || loaded.get() && existing.get().is_empty()
            fallback=|| ().into_view()
        >
            <div class="flow-cfg-hint">
                "No external Postgres connections yet — an admin registers one via POST /db/connections (or ask the assistant)."
            </div>
        </Show>
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Tables pattern"</label>
            <input
                class="flow-cfg-input"
                placeholder="e.g. orders_* or analytics.fact_*"
                prop:value=move || tables.get()
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    tables.set(v.clone());
                    set_trigger_field(graph, &id_sv.get_value(), "tables", &v);
                }
            />
            <div class="flow-cfg-hint">
                "* is a wildcard; later-created matching tables join automatically. Fires one run per newly-inserted row — downstream nodes read trigger.row.<column>."
            </div>
        </div>
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Cursor column (optional)"</label>
            <input
                class="flow-cfg-input"
                placeholder="auto-detect: a serial id, else created_at"
                prop:value=move || cursor_column.get()
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    cursor_column.set(v.clone());
                    set_trigger_field(graph, &id_sv.get_value(), "cursor_column", &v);
                }
            />
        </div>
        {collect_every_field(graph, id_sv.get_value())}
        <div class="flow-cfg-hint">
            "Wire the ⚡ commit port to the node whose success advances the cursor (optional)."
        </div>
    }
}
