//! Tool dispatch: parse a call's arguments, run it through the registry, and
//! time it.

use serde_json::Value as Json;

use catalerum_core::llm::MediaInput;
use catalerum_core::model::ToolCall;
use catalerum_core::tool::{ToolContext, ToolRegistry, MODEL_MEDIA_RESULT_FIELD};

use super::{error_result, value_to_string, ToolInvocation};

/// Dispatch a single tool call, capturing success or error as a string result
/// and timing the dispatch (for the live [`StreamEvent::ToolResult`] + replay).
pub(super) async fn dispatch_one(
    registry: &ToolRegistry,
    ctx: &ToolContext,
    call: &ToolCall,
    input_modalities: &[String],
) -> ToolInvocation {
    let started = std::time::Instant::now();
    let (result, is_error, media) = run_tool(registry, ctx, call, input_modalities).await;
    ToolInvocation {
        call: call.clone(),
        result,
        is_error,
        duration_ms: started.elapsed().as_millis() as u64,
        media,
    }
}

/// Run a tool call, returning its `(result_string, is_error)`. Split out from
/// [`dispatch_one`] so the latter can stamp a single duration over all paths.
pub(super) async fn run_tool(
    registry: &ToolRegistry,
    ctx: &ToolContext,
    call: &ToolCall,
    input_modalities: &[String],
) -> (String, bool, Vec<MediaInput>) {
    // Parse the JSON arguments string; treat empty as `{}`.
    let args: Json = if call.arguments.trim().is_empty() {
        Json::Object(serde_json::Map::new())
    } else {
        match serde_json::from_str(&call.arguments) {
            Ok(v) => v,
            Err(e) => {
                return (
                    error_result(&format!("invalid tool arguments: {e}")),
                    true,
                    Vec::new(),
                )
            }
        }
    };
    match registry
        .dispatch_for_model(&call.name, args, ctx, input_modalities)
        .await
    {
        Ok(mut value) => {
            let encoded_media = value
                .as_object_mut()
                .and_then(|object| object.remove(MODEL_MEDIA_RESULT_FIELD));
            let media = if input_modalities
                .iter()
                .any(|modality| modality.eq_ignore_ascii_case("image"))
            {
                encoded_media
                    .and_then(|media| serde_json::from_value(media).ok())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            (value_to_string(&value), false, media)
        }
        Err(e) => (error_result(&e.to_string()), true, Vec::new()),
    }
}
