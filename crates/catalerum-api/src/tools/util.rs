//! Shared argument-parsing and context helpers for the tool impls.

use super::*;

/// The workspace a tool call is scoped to, or an error if the context lacks one
/// (every authenticated chat/agent run carries a workspace, SOUL §18).
pub(crate) fn workspace(ctx: &ToolContext) -> Result<WorkspaceId> {
    ctx.workspace_id
        .ok_or_else(|| Error::invalid("tool call has no workspace context"))
}

/// The author to record for a note the tool creates: an agent if the call runs
/// under an agent, otherwise the acting user (SOUL §21).
pub(crate) fn author(ctx: &ToolContext) -> Result<Author> {
    if let Some(id) = ctx.agent_id {
        Ok(Author::Agent { id })
    } else if let Some(id) = ctx.user_id {
        Ok(Author::User { id })
    } else {
        Err(Error::invalid("tool call has no acting principal"))
    }
}

/// Pull a required non-empty string argument.
pub(crate) fn required_str(args: &Json, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid(format!("`{key}` is required")))?;
    Ok(value.to_string())
}

/// Pull an optional string argument, defaulting to empty.
pub(crate) fn opt_str(args: &Json, key: &str) -> String {
    args.get(key)
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Pull an optional string argument, trimmed, with all-whitespace treated as
/// absent (`None`).
pub(crate) fn opt_str_some(args: &Json, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Max bytes of extracted text a `read_*` tool returns in one result (~16k
/// tokens). A stored object can be tens of MB, so reading its whole text would
/// blow the model's context / cost; the agent gets the head + `truncated: true`
/// and can narrow with `search_semantic`. (`read_note` is uncapped — notes are
/// small and hand-authored.)
pub(crate) const MAX_READ_TEXT_BYTES: usize = 64 * 1024;

/// Truncate `text` to [`MAX_READ_TEXT_BYTES`] on a UTF-8 char boundary; returns
/// the (possibly shortened) text and whether it was truncated.
pub(crate) fn cap_read_text(text: &str) -> (String, bool) {
    if text.len() <= MAX_READ_TEXT_BYTES {
        return (text.to_string(), false);
    }
    let mut end = MAX_READ_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// Pull a required RFC 3339 / ISO-8601 timestamp argument, normalised to UTC.
pub(crate) fn required_rfc3339(args: &Json, key: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let raw = required_str(args, key)?;
    chrono::DateTime::parse_from_rfc3339(&raw)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| Error::invalid(format!("`{key}` must be an RFC 3339 timestamp: {e}")))
}

/// Pull a required RFC 3339 / ISO-8601 timestamp argument **keeping its UTC
/// offset** (not collapsed to UTC). All-day event endpoints need the wall-clock
/// date the caller wrote to survive so it can be pinned to midnight UTC of *that*
/// date — see [`crate::calendar_writeback::normalize_event_span`]. Collapsing to
/// UTC first would slip an all-day date sent with a positive offset (e.g.
/// `2026-07-07T00:00:00+02:00`) onto the previous day.
pub(crate) fn required_rfc3339_offset(
    args: &Json,
    key: &str,
) -> Result<chrono::DateTime<chrono::FixedOffset>> {
    let raw = required_str(args, key)?;
    chrono::DateTime::parse_from_rfc3339(&raw)
        .map_err(|e| Error::invalid(format!("`{key}` must be an RFC 3339 timestamp: {e}")))
}

/// Pull an optional RFC 3339 / ISO-8601 timestamp argument, normalised to UTC.
/// Absent / blank → `Ok(None)`; present but malformed is still an error (a
/// mistyped bound must not silently widen the query).
pub(crate) fn opt_rfc3339(args: &Json, key: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    match opt_str_some(args, key) {
        None => Ok(None),
        Some(raw) => chrono::DateTime::parse_from_rfc3339(&raw)
            .map(|dt| Some(dt.with_timezone(&chrono::Utc)))
            .map_err(|e| Error::invalid(format!("`{key}` must be an RFC 3339 timestamp: {e}"))),
    }
}

/// Pull an optional `tags` array and normalize it through the same helper the
/// notes REST route uses (trim / drop-empty / de-dup), so tool-authored and
/// user-authored notes get identical tag handling.
pub(crate) fn opt_tags(args: &Json) -> Vec<String> {
    let raw = args
        .get("tags")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    crate::routes::notes::clean_tags(raw)
}

/// Parse a note id argument (`id`) into a [`NoteId`].
pub(crate) fn note_id(args: &Json) -> Result<NoteId> {
    required_str(args, "id")?
        .parse::<NoteId>()
        .map_err(|e| Error::invalid(format!("invalid note id: {e}")))
}

/// Parse a `LinkId` from the `id` argument.
pub(crate) fn link_id(args: &Json) -> Result<LinkId> {
    required_str(args, "id")?
        .parse::<LinkId>()
        .map_err(|e| Error::invalid(format!("invalid link id: {e}")))
}

/// Parse a [`SourceRef`] endpoint from a `{ "kind": …, "id": … }` object at
/// `key` — the tagged `(kind, id)` split the store persists (a uuid for
/// first-class rows: `note`/`event`/`object`/`email`/…, or a uri for `external`).
pub(crate) fn source_ref(args: &Json, key: &str) -> Result<SourceRef> {
    let obj = args
        .get(key)
        .ok_or_else(|| Error::invalid(format!("`{key}` is required")))?;
    let kind = obj
        .get("kind")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid(format!("`{key}.kind` is required")))?;
    let id = obj
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid(format!("`{key}.id` is required")))?;
    source_from_parts(kind, id)
        .map_err(|e| Error::invalid(format!("invalid `{key}` endpoint: {e}")))
}

/// Pull an optional string array at `key` (trimmed, empties dropped). Absent /
/// non-array → empty.
pub(crate) fn opt_str_vec(args: &Json, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Json::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Pull an optional `tools` array and normalize it through the same helper the
/// skills REST route uses (trim / drop-empty / de-dup), so tool-authored and
/// user-authored skills get identical tool-list handling (the `opt_tags` pattern).
pub(crate) fn opt_skill_tools(args: &Json) -> Vec<String> {
    crate::routes::skills::clean_tools(opt_str_vec(args, "tools"))
}

/// Parse a `code` argument value (`{ language, source, entrypoint? }`) into a
/// [`Code`]. Absent/null handling stays at the call site — `edit_skill` must
/// distinguish "keep" (absent) from "clear" (explicit `null`).
pub(crate) fn skill_code(value: &Json) -> Result<Code> {
    serde_json::from_value(value.clone()).map_err(|e| {
        Error::invalid(format!(
            "invalid `code`: expected {{ language, source, entrypoint? }}: {e}"
        ))
    })
}

/// The compact result `create_skill`/`edit_skill` return: enough to confirm the
/// write (id, name, normalized tool set, whether code is attached) without
/// echoing the full runbook back into the model's context.
pub(crate) fn skill_summary(skill: &Skill) -> Json {
    json!({
        "id": skill.id,
        "name": skill.name,
        "description": skill.description,
        "tools": skill.tools,
        "has_code": skill.code.is_some(),
        "advertised": skill.advertised,
    })
}

/// The capability a tool requires: `action` on the whole `domain` (SOUL §19).
pub(crate) fn cap(action: Action, domain: &str) -> Option<Capability> {
    Some(Capability::new(action, Resource::domain(domain)))
}

/// Pull an optional positive integer at `key`, clamped to `[1, max]`, defaulting
/// to `default`.
pub(crate) fn opt_clamped_u64(args: &Json, key: &str, default: u64, max: u64) -> u64 {
    args.get(key)
        .and_then(Json::as_u64)
        .map(|n| n.clamp(1, max))
        .unwrap_or(default)
}

/// Parse a typed id argument (`key`) into `T`, with a clear error.
pub(crate) fn parse_id<T: std::str::FromStr<Err = uuid::Error>>(
    args: &Json,
    key: &str,
) -> Result<T> {
    required_str(args, key)?
        .parse::<T>()
        .map_err(|e| Error::invalid(format!("invalid {key}: {e}")))
}
