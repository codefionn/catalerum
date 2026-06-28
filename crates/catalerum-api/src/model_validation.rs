//! Shared validation of LLM model / voice ids against the gateway catalog (SOUL
//! §7). Used wherever a user-chosen model id is persisted — `/llm-settings`
//! (chat/speech/transcription + voice) and agent profiles (the scoped-agent
//! model) — so a typo can't silently persist and then fail at call time.
//!
//! **Degrades gracefully:** the pure deciders ([`model_in_catalog`] /
//! [`voice_in_list`]) treat a failed catalog fetch (gateway down) or an empty
//! catalog as "accept", so a transient gateway outage never blocks a save; only a
//! successfully-fetched, non-empty catalog that lacks the exact id is a rejection.

use catalerum_core::error::Error;
use catalerum_llm::catalog::{ModelInfo, ModelKind, VoiceInfo};

use crate::error::ApiResult;
use crate::state::AppState;

/// Decide whether `model` is acceptable given the fetched `catalog` for its kind.
/// A `None` catalog (the fetch failed) or an empty one **accepts** the model.
#[must_use]
pub(crate) fn model_in_catalog(catalog: Option<&[ModelInfo]>, model: &str) -> bool {
    match catalog {
        None | Some([]) => true,
        Some(c) => c.iter().any(|m| m.id == model),
    }
}

/// Reject a `model` id that the gateway catalog definitively lacks, so a typo
/// can't silently persist and break every call. `label` names the field in the
/// error; `kind` is the model class to validate against — the **same** class the
/// matching picker lists ([`ModelKind::Tts`] for the speech field,
/// [`ModelKind::Stt`] for transcription, [`ModelKind::Chat`] for chat / agent
/// models), so a model the user can pick is one they can also save (TTS/STT-only
/// ids live only under their kind, not the full catalog). Degrades gracefully via
/// [`model_in_catalog`].
pub(crate) async fn validate_model(
    state: &AppState,
    label: &str,
    model: &str,
    kind: ModelKind,
) -> ApiResult<()> {
    let catalog = state.llm().list_models(kind, None).await.ok();
    if model_in_catalog(catalog.as_deref(), model) {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "{label} model `{model}` is not in the gateway catalog; pick one from /llm-models"
        ))
        .into())
    }
}

/// Decide whether `voice` is acceptable given the fetched `voices` for the chosen
/// speech model. Same graceful degradation as [`model_in_catalog`].
#[must_use]
pub(crate) fn voice_in_list(voices: Option<&[VoiceInfo]>, voice: &str) -> bool {
    match voices {
        None | Some([]) => true,
        Some(v) => v.iter().any(|x| x.id == voice),
    }
}

/// Reject a `voice` the chosen speech `model` doesn't offer, so a typo can't
/// silently persist and break TTS. Voices are per-model, so this needs a concrete
/// `model`. Degrades gracefully via [`voice_in_list`].
pub(crate) async fn validate_voice(state: &AppState, model: &str, voice: &str) -> ApiResult<()> {
    let voices = state.llm().voices(model).await.ok();
    if voice_in_list(voices.as_deref(), voice) {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "voice `{voice}` is not offered by speech model `{model}`; pick one from /llm-voices"
        ))
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_in_catalog_accepts_known_and_degrades_gracefully() {
        let mk = |id: &str| ModelInfo {
            id: id.to_string(),
            ..Default::default()
        };
        let catalog = vec![mk("gpt-4o"), mk("claude-opus-4-8")];
        // A known id is accepted; an unknown one is rejected.
        assert!(model_in_catalog(Some(&catalog), "gpt-4o"));
        assert!(!model_in_catalog(Some(&catalog), "gpt-4o-typo"));
        // Graceful degradation: a failed fetch (None) or empty catalog accepts —
        // a transient gateway outage must not block saving settings.
        assert!(model_in_catalog(None, "anything"));
        assert!(model_in_catalog(Some(&[]), "anything"));
    }

    #[test]
    fn voice_in_list_accepts_known_and_degrades_gracefully() {
        let mk = |id: &str| VoiceInfo {
            id: id.to_string(),
            ..Default::default()
        };
        let voices = vec![mk("nova"), mk("alloy")];
        assert!(voice_in_list(Some(&voices), "nova"));
        assert!(!voice_in_list(Some(&voices), "nova-typo"));
        // Graceful degradation: failed fetch (None) or empty list accepts.
        assert!(voice_in_list(None, "anything"));
        assert!(voice_in_list(Some(&[]), "anything"));
    }
}
