//! Deferred tool advertising (SOUL §7): the catalog note and the widening of a
//! run's advertised tool set from discovery results.

use serde_json::Value as Json;

use catalerum_core::llm::{ChatMessage, ToolSpec};
use catalerum_core::model::MessageRole;
use catalerum_core::tool::ToolRegistry;

/// The system note injected on a deferred-advertising run (SOUL §7): the full
/// tool catalog mapped by area (capability domain), plus how to load a tool.
/// Deterministic — sorted domains and names — so the prompt prefix stays
/// byte-stable across turns and provider prompt caching keeps working.
pub(super) fn discovery_note(registry: &ToolRegistry, allowed: Option<&[String]>) -> String {
    let mut note = String::from(
        "## Tool discovery\n\
         To keep your context lean, only a small pinned subset of the available \
         tools is advertised to you up front. Many more exist — their names are \
         listed by area below. To call one, first load it through the advertised \
         discovery tools: describe what you need to `search_tools`, or browse the \
         catalog with `list_tools` (pass `names` to select specific ones). Every \
         tool a discovery result returns becomes callable from your next step. \
         Never guess an unloaded tool's arguments — load it first. Tools loaded \
         through a discovery call earlier in this conversation are still loaded: \
         call them directly, do NOT run `search_tools`/`list_tools` again for a \
         tool you already discovered. Search only for tools you have not loaded \
         yet.\n\n\
         Available tools by area:\n",
    );
    for (domain, names) in registry.domain_groups(allowed) {
        note.push_str("- ");
        note.push_str(&domain);
        note.push_str(": ");
        note.push_str(&names.join(", "));
        note.push('\n');
    }
    note
}

/// Widen a deferred run's advertised tools with those named in a discovery-tool
/// result (SOUL §7). The protocol: a top-level `"tools"` array whose items carry
/// a `"name"` — the shape `search_tools`/`list_tools` return. Names already
/// advertised, outside `allowed`, or unknown to the registry are skipped, so a
/// result can never advertise past the run's allow-list (and advertising grants
/// no authority — dispatch still capability-gates every call).
pub(super) fn widen_advertised(
    advertised: &mut Vec<ToolSpec>,
    registry: &ToolRegistry,
    allowed: Option<&[String]>,
    result: &str,
    input_modalities: &[String],
) {
    let Ok(parsed) = serde_json::from_str::<Json>(result) else {
        return;
    };
    let Some(items) = parsed.get("tools").and_then(Json::as_array) else {
        return;
    };
    let mut fresh: Vec<String> = Vec::new();
    for item in items {
        let Some(n) = item.get("name").and_then(Json::as_str) else {
            continue;
        };
        if allowed.is_some_and(|a| !a.iter().any(|x| x == n))
            || advertised.iter().any(|s| s.name == n)
            || fresh.iter().any(|f| f == n)
        {
            continue;
        }
        fresh.push(n.to_string());
    }
    advertised.extend(registry.specs_for_model_names(&fresh, input_modalities));
}

/// Widen from an explicit `"advertise_tools": ["name", ...]` result. Unlike the
/// legacy discovery-tool protocol above, this may be returned by any tool and is
/// intentionally unambiguous: ordinary business data containing a `tools` field
/// cannot accidentally widen the model surface. Capability/allow-list filtering
/// and exact registry lookup still apply, so this grants no authority.
pub(super) fn widen_explicitly_advertised(
    advertised: &mut Vec<ToolSpec>,
    registry: &ToolRegistry,
    allowed: Option<&[String]>,
    result: &str,
    input_modalities: &[String],
) {
    let Ok(parsed) = serde_json::from_str::<Json>(result) else {
        return;
    };
    let Some(items) = parsed.get("advertise_tools").and_then(Json::as_array) else {
        return;
    };
    let mut fresh: Vec<String> = Vec::new();
    for item in items {
        let Some(name) = item.as_str() else {
            continue;
        };
        if allowed.is_some_and(|a| !a.iter().any(|candidate| candidate == name))
            || advertised.iter().any(|spec| spec.name == name)
            || fresh.iter().any(|candidate| candidate == name)
        {
            continue;
        }
        fresh.push(name.to_string());
    }
    advertised.extend(registry.specs_for_model_names(&fresh, input_modalities));
}

/// Re-widen a deferred run's advertised tools from replayed history (SOUL §7):
/// every discovery-tool result already in the conversation names tools the model
/// loaded on an earlier turn. Without this, each new chat message reopens the run
/// with only the pinned subset and the model has to re-run `search_tools` for
/// tools it already found. Replayed messages carry no error flag, but that's
/// harmless — [`widen_advertised`] only acts on the `"tools": [{"name"}]` result
/// shape, which an error payload never has, and the allow-list still gates every
/// name.
pub(super) fn rewiden_from_history(
    advertised: &mut Vec<ToolSpec>,
    registry: &ToolRegistry,
    allowed: Option<&[String]>,
    history: &[ChatMessage],
    discovery_tools: &[String],
    input_modalities: &[String],
) {
    let discovery_call_ids: std::collections::HashSet<&str> = history
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .flat_map(|m| m.tool_calls.iter())
        .filter(|c| discovery_tools.iter().any(|d| d == &c.name))
        .map(|c| c.id.as_str())
        .collect();
    for message in history {
        if message.role != MessageRole::Tool {
            continue;
        }
        if message
            .tool_call_id
            .as_deref()
            .is_some_and(|id| discovery_call_ids.contains(id))
        {
            widen_advertised(
                advertised,
                registry,
                allowed,
                &message.content,
                input_modalities,
            );
        }
        widen_explicitly_advertised(
            advertised,
            registry,
            allowed,
            &message.content,
            input_modalities,
        );
    }
}
