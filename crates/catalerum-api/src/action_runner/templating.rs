//! `{{ path }}` templating of action params against the node's runtime context.

use serde_json::Value;

/// Substitute `{{ path }}` references in an action's `params` with values from the
/// node's runtime `ctx` (in a graph run, the merged `{ "trigger": …, "inputs":
/// { <upstream_id>: <output> } }`; in the linear loop, the firing trigger itself).
///
/// A param string that is **exactly** a single `{{ path }}` is replaced with the
/// resolved JSON value, *preserving its type* (so a number stays a number); a
/// `{{ path }}` embedded in surrounding text is stringified and interpolated. An
/// **unresolved** path is left verbatim — so an automation that uses no templating,
/// or contains a literal `{{`, is unchanged, and a typo surfaces as a clear
/// downstream error (e.g. "unknown terminal session") rather than silent mangling.
/// Recurses into nested objects/arrays. Pure (no I/O) so it is unit-tested below.
pub(super) fn render_params(
    params: &serde_json::Map<String, Value>,
    ctx: Option<&Value>,
) -> serde_json::Map<String, Value> {
    let Some(ctx) = ctx else {
        return params.clone();
    };
    params
        .iter()
        .map(|(k, v)| (k.clone(), render_value(v, ctx)))
        .collect()
}

/// Recursively render template references in one JSON value (see [`render_params`]).
fn render_value(v: &Value, ctx: &Value) -> Value {
    match v {
        Value::String(s) => render_string(s, ctx),
        Value::Array(a) => Value::Array(a.iter().map(|x| render_value(x, ctx)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), render_value(x, ctx)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Render template references in a single string (see [`render_params`]). A
/// whole-string `{{ path }}` yields the resolved value verbatim (type preserved);
/// otherwise every `{{ path }}` token is stringified and interpolated in place.
fn render_string(s: &str, ctx: &Value) -> Value {
    let trimmed = s.trim();
    // Whole-string template → return the resolved value with its JSON type intact.
    if let Some(path) = trimmed
        .strip_prefix("{{")
        .and_then(|r| r.strip_suffix("}}"))
    {
        if let Some(found) = resolve_path(ctx, path.trim()) {
            return found.clone();
        }
        return Value::String(s.to_string());
    }
    // Embedded templates → string interpolation (resolved values stringified).
    if !s.contains("{{") {
        return Value::String(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let path = after[..end].trim();
                match resolve_path(ctx, path) {
                    Some(found) => out.push_str(&stringify(found)),
                    // Unresolved → leave the token verbatim.
                    None => {
                        out.push_str("{{");
                        out.push_str(&after[..end]);
                        out.push_str("}}");
                    }
                }
                rest = &after[end + 2..];
            }
            // Unterminated `{{` → emit the rest literally and stop.
            None => {
                out.push_str("{{");
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    Value::String(out)
}

/// Resolve a dotted `path` (e.g. `inputs.open.session_id`, `trigger.kind`,
/// `inputs.list.0`) against `ctx`: object keys traverse maps, all-digit segments
/// index arrays. `None` if any segment is missing or the type doesn't match.
fn resolve_path<'a>(ctx: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = ctx;
    for seg in path.split('.').filter(|s| !s.is_empty()) {
        cur = match cur {
            Value::Object(map) => map.get(seg)?,
            Value::Array(arr) => arr.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Stringify a resolved value for **interpolation** into surrounding text: a JSON
/// string contributes its raw text (no quotes); anything else its compact JSON.
fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
