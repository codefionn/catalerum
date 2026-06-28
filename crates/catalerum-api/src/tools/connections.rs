//! Source-connection tools: list/create email + calendar connections.

use super::*;

/// `list_connections` — the workspace's email/calendar **source connections**,
/// each with its collect status (SOUL §10/§28/§29). The discovery half of
/// automation authoring: a `collect_email`/`collect_calendar` trigger's
/// `connection` field must reference one of these ids, so the model looks here
/// first (and reaches for `create_email_connection`/`create_calendar_connection`
/// when the source doesn't exist yet). Config blobs (which carry credentials)
/// are never returned. Capability is per-kind in `invoke` (the
/// `query_structured` pattern, §19): `email:read` / `calendar:read`.
pub(crate) struct ListConnectionsTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ListConnectionsTool {
    fn name(&self) -> &str {
        "list_connections"
    }

    fn required_capability(&self) -> Option<Capability> {
        // Spans two domains; gated per requested `kind` in `invoke` (§19).
        None
    }

    fn description(&self) -> &str {
        "List the workspace's email or calendar source connections — the sources a \
         collect_email / collect_calendar automation trigger pulls from. Each result \
         carries the connection's `id` (the uuid a collect trigger's `connection` \
         field must reference — never a made-up name), its name, and `collecting` \
         (false = dormant: no enabled collect automation ingests from it yet). If the \
         source you need doesn't exist, create it with create_email_connection or \
         create_calendar_connection."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["email", "calendar"],
                    "description": "Which source kind to list."
                }
            },
            "required": ["kind"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let kind = required_str(&args, "kind")?;
        let (want, domain) = match kind.as_str() {
            "email" => (ConnectionKind::Email, "email"),
            "calendar" => (ConnectionKind::Calendar, "calendar"),
            other => {
                return Err(Error::invalid(format!(
                    "unknown connection kind `{other}` (expected email | calendar)"
                )))
            }
        };
        if let Some(caps) = &ctx.capabilities {
            let required = Capability::new(Action::Read, Resource::domain(domain));
            if !caps.iter().any(|held| held.covers(&required)) {
                return Err(Error::unauthorized(format!(
                    "list_connections `{kind}` requires {domain}:read which the \
                     caller's grant does not cover"
                )));
            }
        }
        let connections: Vec<_> = self
            .store
            .connections()
            .list_by_workspace(ws)
            .await?
            .into_iter()
            .filter(|c| c.kind == want)
            .collect();
        // One automations scan marks which sources are live (SOUL §29), the same
        // projection the REST listings return — only the derived boolean, never
        // automation contents.
        let automations = self.store.automations().list_by_workspace(ws).await?;
        let results: Vec<Json> = connections
            .into_iter()
            .map(|c| {
                let collecting =
                    crate::connection_status::is_collecting(&automations, c.kind, c.id);
                json!({ "id": c.id, "name": c.name, "collecting": collecting })
            })
            .collect();
        Ok(json!({ "kind": kind, "connections": results }))
    }
}

/// `create_email_connection` — register a read-only email source (SOUL §28) so a
/// `collect_email` automation can pull it. The tool twin of
/// `POST /email/connections` (same body shape, same
/// [`build_email_config`](crate::routes::email::build_email_config) blob), so a
/// chat can set up email ingest end-to-end: create the connection, then a
/// Collect automation referencing its id. Registering provisions nothing — the
/// source stays dormant until an enabled collect automation heads at it. Gated
/// `email:write` like the route (§19).
pub(crate) struct CreateEmailConnectionTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for CreateEmailConnectionTool {
    fn name(&self) -> &str {
        "create_email_connection"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "email")
    }

    fn description(&self) -> &str {
        "Register a read-only email source connection (catalerum reads mail, it never \
         sends). provider = 'imap' (host/username/password), 'jmap' \
         (session_url/token, e.g. Fastmail's https://api.fastmail.com/jmap/session), \
         'gmail' (client_id/client_secret/refresh_token), or 'maildir' (a local \
         root directory). Returns the new connection's `id` — reference that id in a \
         collect_email trigger's `connection` field (create_automation) and enable \
         the automation, or nothing will ever be ingested. Ask the user for \
         credentials with ask_user if you don't have them; never invent them."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type": "string",
                    "enum": ["imap", "jmap", "gmail", "maildir"],
                    "description": "The email provider protocol."
                },
                "name": { "type": "string", "description": "Human-readable name for the source (e.g. 'Fastmail')." },
                "host": { "type": "string", "description": "IMAP: server hostname." },
                "port": { "type": "integer", "description": "IMAP: server port (default 993, implicit TLS)." },
                "username": { "type": "string", "description": "IMAP: login username." },
                "password": { "type": "string", "description": "IMAP: login password (an app password where offered)." },
                "mailbox": { "type": "string", "description": "IMAP/maildir: folder to ingest (default INBOX)." },
                "session_url": { "type": "string", "description": "JMAP: session resource URL." },
                "token": { "type": "string", "description": "JMAP: bearer token." },
                "account_id": { "type": "string", "description": "JMAP: optional account-id override." },
                "client_id": { "type": "string", "description": "Gmail: OAuth2 client id." },
                "client_secret": { "type": "string", "description": "Gmail: OAuth2 client secret." },
                "refresh_token": { "type": "string", "description": "Gmail: long-lived OAuth2 refresh token." },
                "label": { "type": "string", "description": "Gmail: label to ingest (default INBOX)." },
                "root": { "type": "string", "description": "maildir: the directory containing new/ cur/ tmp/." }
            },
            "required": ["provider", "name"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let body: crate::routes::email::CreateEmailConnection = serde_json::from_value(args)
            .map_err(|e| Error::invalid(format!("invalid email connection spec: {e}")))?;
        let name = body.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::invalid("`name` is required"));
        }
        let config = crate::routes::email::build_email_config(&body).map_err(Error::invalid)?;
        let connection = self
            .store
            .connections()
            .create(ws, ConnectionKind::Email, &name, None, Some(config))
            .await?;
        Ok(json!({
            "id": connection.id,
            "name": connection.name,
            "kind": "email",
            "provider": body.provider.as_str(),
            // Dormant until a collect automation heads at it (SOUL §28) — say so,
            // so the model finishes the job instead of stopping here.
            "collecting": false,
            "next": "reference this id in a collect_email trigger's `connection` \
                     (create_automation with a downstream write_email, commit_on it) \
                     and enable the automation",
        }))
    }
}

/// `create_calendar_connection` — register a calendar source (SOUL §8) so a
/// `collect_calendar` automation can pull it. The tool twin of
/// `POST /connections` (same provider kinds + config validation via
/// [`build_calendar_config`](crate::routes::calendar::build_calendar_config)).
/// Registering provisions nothing until a collect automation heads at it. Gated
/// `calendar:write` like the route (§19).
pub(crate) struct CreateCalendarConnectionTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for CreateCalendarConnectionTool {
    fn name(&self) -> &str {
        "create_calendar_connection"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "calendar")
    }

    fn description(&self) -> &str {
        "Register a calendar source connection. kind = 'caldav' (base_url + \
         optional username/password), 'webcal' (base_url of an .ics feed), or \
         'local' (dir of a directory of .ics files) — pass those settings as \
         top-level fields. Google and Outlook calendars are NOT created here — \
         the user connects them via the OAuth flows (open \
         /auth/google/connect?kind=calendar or /auth/microsoft/connect in the \
         browser). Returns the new connection's `id` — reference that id in a \
         collect_calendar trigger's `connection` field (create_automation with a \
         downstream write_event) and enable the automation, or nothing will ever \
         be ingested."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["local", "caldav", "webcal"],
                    "description": "The calendar provider kind."
                },
                "name": { "type": "string", "description": "Human-readable name for the source." },
                "base_url": { "type": "string", "description": "caldav: the collection URL; webcal: the URL of the .ics feed." },
                "username": { "type": "string", "description": "caldav/webcal: optional HTTP Basic username." },
                "password": { "type": "string", "description": "caldav/webcal: optional HTTP Basic password." },
                "dir": { "type": "string", "description": "local: directory of .ics files." }
            },
            "required": ["kind", "name"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let args = normalize_calendar_connection_args(args)?;
        let body: crate::routes::calendar::CreateConnection = serde_json::from_value(args)
            .map_err(|e| Error::invalid(format!("invalid calendar connection spec: {e}")))?;
        let name = body.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::invalid("`name` is required"));
        }
        let config = crate::routes::calendar::build_calendar_config(body.kind, body.config)
            .map_err(Error::invalid)?;
        let connection = self
            .store
            .connections()
            .create(
                ws,
                ConnectionKind::Calendar,
                &name,
                body.credentials.as_deref(),
                Some(config),
            )
            .await?;
        Ok(json!({
            "id": connection.id,
            "name": connection.name,
            "kind": "calendar",
            "provider": body.kind.as_str(),
            "collecting": false,
            "next": "reference this id in a collect_calendar trigger's `connection` \
                     (create_automation with a downstream write_event, commit_on it) \
                     and enable the automation",
        }))
    }
}

/// Fold the argument spellings models actually produce for
/// `create_calendar_connection` into the route body's nested `config` shape.
///
/// The REST twin nests per-provider settings under `config`, but a
/// `{"type":"object"}` parameter with no declared properties is a trap on the
/// LLM wire: providers with schema-constrained tool decoding strip the
/// undeclared keys (an empty/absent `config` arrives no matter what the model
/// wrote), and weaker models double-encode the object as a JSON string or pass
/// the bare URL. So the schema advertises the settings as flat top-level
/// fields, and this accepts all three spellings: flat fields (folded under
/// `config`, which wins on conflict), a stringified `config` (parsed back),
/// and a bare-string `config` (filed under the kind's required key).
pub(crate) fn normalize_calendar_connection_args(mut args: Json) -> Result<Json> {
    let Some(map) = args.as_object_mut() else {
        return Ok(args);
    };
    if let Some(Json::String(s)) = map.get("config") {
        let s = s.trim();
        let parsed = if s.is_empty() {
            Json::Object(serde_json::Map::new())
        } else if let Ok(obj @ Json::Object(_)) = serde_json::from_str(s) {
            obj
        } else {
            // A bare string (just the feed URL / directory): file it under the
            // kind's required key so intent survives.
            let key = match map.get("kind").and_then(Json::as_str) {
                Some("local") => "dir",
                _ => "base_url",
            };
            json!({ key: s })
        };
        map.insert("config".to_string(), parsed);
    }
    let flat: Vec<(String, Json)> = ["base_url", "dir", "username", "password"]
        .iter()
        .filter_map(|k| map.remove(*k).map(|v| ((*k).to_string(), v)))
        .collect();
    if !flat.is_empty() {
        let config = map
            .entry("config".to_string())
            .or_insert_with(|| Json::Object(serde_json::Map::new()));
        if let Some(cfg) = config.as_object_mut() {
            for (k, v) in flat {
                cfg.entry(k).or_insert(v);
            }
        }
    }
    Ok(args)
}
