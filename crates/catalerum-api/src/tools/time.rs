//! `current_time` — wall-clock in the user's timezone.

use super::*;

/// `current_time` — the real wall-clock **now** (SOUL §7). The model has no
/// clock of its own, so it must call this rather than guess the date/time; the
/// result also anchors relative phrasing ("tomorrow", "in 2 hours") the agent
/// then feeds to `create_event`/`create_task`. Rendered both in UTC and in a
/// resolved IANA timezone: an explicit `timezone` argument wins, else the acting
/// user's stored profile `timezone` field (SOUL §22), else UTC. Ungated and
/// workspace-optional — a pure utility with no side effect and no private data
/// beyond the caller's own timezone preference.
pub(crate) struct CurrentTimeTool {
    pub(crate) profiles: ProfileRepo,
}

impl CurrentTimeTool {
    /// The acting user's profile `timezone` field, parsed to a [`chrono_tz::Tz`].
    /// Best-effort: `None` when there is no acting user/workspace, no stored
    /// timezone, or the stored value is not a valid IANA name (a bad profile
    /// value falls back to UTC rather than failing the call).
    async fn profile_timezone(&self, ctx: &ToolContext) -> Option<chrono_tz::Tz> {
        let (ws, user_id) = (ctx.workspace_id?, ctx.user_id?);
        let profile = self.profiles.get(ws, user_id).await.ok()?;
        profile
            .fields
            .get("timezone")
            .and_then(Json::as_str)
            .and_then(|s| s.trim().parse::<chrono_tz::Tz>().ok())
    }
}

#[async_trait]
impl Tool for CurrentTimeTool {
    fn name(&self) -> &str {
        "current_time"
    }

    fn description(&self) -> &str {
        "Get the current date and time (you have no clock — call this instead of \
         guessing). Returns `utc` (RFC3339) and `unix` seconds, plus the time in a \
         timezone: pass `timezone` as an IANA name (e.g. `Europe/Berlin`) to \
         override, otherwise it uses the user's profile timezone, otherwise UTC. \
         `timezone_source` says which of those applied. Use this to resolve \
         relative dates like 'tomorrow' before calling create_event/create_task."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "Optional IANA timezone name to render the local time in (e.g. `Europe/Berlin`, `America/New_York`, `UTC`). Omit to use the user's profile timezone, or UTC if none is set."
                }
            }
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        use chrono_tz::Tz;
        let now = chrono::Utc::now();

        // Resolve the IANA timezone (argument > profile > UTC). An explicit but
        // unknown argument is a hard error so the model can correct it; a bad
        // *profile* value was already dropped by `profile_timezone` (→ default).
        let (tz, source): (Tz, &str) = if let Some(arg) = opt_str_some(&args, "timezone") {
            let tz = arg.parse::<Tz>().map_err(|_| {
                Error::invalid(format!(
                    "unknown timezone `{arg}` — pass a valid IANA name like \
                     `Europe/Berlin`, `America/New_York`, or `UTC`"
                ))
            })?;
            (tz, "argument")
        } else if let Some(tz) = self.profile_timezone(ctx).await {
            (tz, "profile")
        } else {
            (Tz::UTC, "default")
        };

        let local = now.with_timezone(&tz);
        Ok(json!({
            "utc": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "unix": now.timestamp(),
            "timezone": tz.name(),
            "timezone_source": source,
            "local": local.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            "utc_offset": local.format("%:z").to_string(),
            "weekday": local.format("%A").to_string(),
            "formatted": local.format("%A, %Y-%m-%d %H:%M:%S %:z").to_string(),
        }))
    }
}
