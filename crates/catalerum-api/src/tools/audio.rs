//! Speech tools: `speech_to_text` / `text_to_speech`.

use super::*;

/// The per-user speech/transcription model + voice overrides for `ctx`'s principal,
/// or `None` when the call has no acting user (nothing to look up) — every unset
/// field then falls back to the `[llm]` config default. Shared by the two audio
/// tools so both honour the same picks as the settings UI (SOUL §7, Principle 10).
pub(crate) async fn user_llm_settings(
    store: &Store,
    ws: WorkspaceId,
    ctx: &ToolContext,
) -> Option<LlmSettings> {
    let uid = ctx.user_id?;
    store.llm_settings().get(ws, uid).await.ok()
}

/// `speech_to_text` — transcribe a stored audio file to text (SOUL §7). Reads the
/// audio bytes from a files store (the caller's default when unnamed) and runs them
/// through the effective STT model (an explicit `model` arg → the caller's per-user
/// `transcription_model` override → the `[llm].transcription_model` config default),
/// via llmleaf's `/v1/audio/transcriptions`. Registered whenever object storage is
/// configured; Boa-callable through the shared registry, so an automation code node
/// can `catalerum.callTool("speech_to_text", { key })`. Gated `storage:read` — the
/// input is a stored file, so it reads under the same authority as `read_object`.
pub(crate) struct SpeechToTextTool {
    pub(crate) llm: OpenRouterClient,
    pub(crate) storage: StorageRegistry,
    pub(crate) store: Store,
    pub(crate) default_model: String,
}

#[async_trait]
impl Tool for SpeechToTextTool {
    fn name(&self) -> &str {
        "speech_to_text"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "storage")
    }
    fn description(&self) -> &str {
        "Transcribe a stored audio file (mp3/wav/m4a/ogg/flac/webm/…) to text. Pass \
         the object `key` of the audio file in your files store (omit `store` for your \
         default). Returns the transcript plus the detected language and duration when \
         the provider reports them. Optionally hint the spoken `language` (ISO-639-1, \
         e.g. \"en\") and a `prompt` to bias decoding. To synthesize audio from text \
         instead, use text_to_speech."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Object key of the audio file (store-relative). The extension hints the container/codec." },
                "store": { "type": "string", "description": "Files store name; omitted → your default files store." },
                "model": { "type": "string", "description": "STT model id (search_models with kind `stt` lists them); omitted → your speech-to-text setting, then the server default." },
                "language": { "type": "string", "description": "Spoken-language hint, ISO-639-1 (e.g. \"en\"). Optional." },
                "prompt": { "type": "string", "description": "Optional prompt to bias decoding (e.g. proper nouns, jargon)." }
            },
            "required": ["key"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let key = required_str(&args, "key")?;
        let store_name = opt_str_some(&args, "store");
        let model = match opt_str_some(&args, "model") {
            Some(m) => m,
            None => user_llm_settings(&self.store, ws, ctx)
                .await
                .and_then(|s| s.transcription_model)
                .unwrap_or_else(|| self.default_model.clone()),
        };
        let (bytes, _content_type) = crate::routes::storage::read_object_bytes(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            (store_name.as_deref(), &key),
        )
        .await
        .map_err(|e| Error::other(e.to_string()))?;
        // The filename's extension is the container hint the STT endpoint reads, so
        // carry the key's basename through (fallback keeps a non-empty name).
        let filename = key
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("audio")
            .to_string();
        let mut request = TranscriptionRequest::new(&model, bytes, filename);
        if let Some(language) = opt_str_some(&args, "language") {
            request = request.with_language(language);
        }
        if let Some(prompt) = opt_str_some(&args, "prompt") {
            request = request.with_prompt(prompt);
        }
        let response = self.llm.transcribe(request).await?;
        Ok(json!({
            "text": response.text,
            "language": response.language,
            "duration": response.duration,
            "model": model,
        }))
    }
}

/// `text_to_speech` — synthesize an audio file from text and store it (SOUL §7).
/// Runs `text` through the effective TTS model + voice (an explicit `model`/`voice`
/// arg → the caller's per-user `speech_model`/`speech_voice` override → the
/// `[llm]` config defaults), via llmleaf's `/v1/audio/speech`, then writes the audio
/// to a files store under `key` — catalogued, ingested, and firing the `StorageObject`
/// trigger like an upload (so a downstream automation can pick it up). Registered
/// whenever object storage is configured; Boa-callable via the shared registry
/// (`catalerum.callTool("text_to_speech", { text, key })`). Gated `storage:write` —
/// it writes a stored file, the same authority `copy_object`/upload require.
pub(crate) struct TextToSpeechTool {
    pub(crate) llm: OpenRouterClient,
    pub(crate) storage: StorageRegistry,
    pub(crate) store: Store,
    pub(crate) default_model: String,
    pub(crate) default_voice: String,
}

#[async_trait]
impl Tool for TextToSpeechTool {
    fn name(&self) -> &str {
        "text_to_speech"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "storage")
    }
    fn description(&self) -> &str {
        "Synthesize speech audio from `text` and store it as a file. Pass the output \
         object `key` (store-relative — match its extension to `format`, e.g. \
         \"clips/reply.mp3\"); omit `store` for your default files store. The stored \
         file is catalogued and can head a downstream automation. Optionally set the \
         `voice`, `model`, `format` (mp3 [default]/opus/aac/flac/wav/pcm), and `speed` \
         (playback multiplier). Returns the stored key, size, and content type. To go \
         the other way (audio file → text), use speech_to_text."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "The text to speak." },
                "key": { "type": "string", "description": "Output object key (store-relative). Match its extension to `format` (e.g. \"reply.mp3\")." },
                "store": { "type": "string", "description": "Files store to write to; omitted → your default files store." },
                "voice": { "type": "string", "description": "Provider voice id (e.g. \"alloy\"); omitted → your speech-voice setting, then the server default." },
                "model": { "type": "string", "description": "TTS model id (search_models with kind `tts` lists them); omitted → your text-to-speech setting, then the server default." },
                "format": { "type": "string", "enum": ["mp3", "opus", "aac", "flac", "wav", "pcm"], "description": "Audio container/codec; default mp3." },
                "speed": { "type": "number", "description": "Playback speed multiplier (e.g. 1.25). Optional." }
            },
            "required": ["text", "key"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let text = required_str(&args, "text")?;
        let key = required_str(&args, "key")?;
        let store_name = opt_str_some(&args, "store");
        // One settings lookup covers both the model and the voice fallback.
        let settings = match (opt_str_some(&args, "model"), opt_str_some(&args, "voice")) {
            (Some(_), Some(_)) => None,
            _ => user_llm_settings(&self.store, ws, ctx).await,
        };
        let model = opt_str_some(&args, "model")
            .or_else(|| settings.as_ref().and_then(|s| s.speech_model.clone()))
            .unwrap_or_else(|| self.default_model.clone());
        let voice = opt_str_some(&args, "voice")
            .or_else(|| settings.as_ref().and_then(|s| s.speech_voice.clone()))
            .unwrap_or_else(|| self.default_voice.clone());
        let format = opt_str_some(&args, "format").unwrap_or_else(|| "mp3".to_string());
        let mut request = SpeechRequest::new(&model, &text, &voice).with_format(&format);
        if let Some(speed) = args.get("speed").and_then(Json::as_f64) {
            request = request.with_speed(speed as f32);
        }
        let audio = self.llm.synthesize(request).await?;
        let content_type = (!audio.content_type.is_empty()).then(|| audio.content_type.clone());
        let size = audio.data.len();
        let object = crate::routes::storage::write_object_bytes(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            (store_name.as_deref(), &key),
            audio.data,
            content_type,
        )
        .await
        .map_err(|e| Error::other(e.to_string()))?;
        Ok(json!({
            "key": object.key,
            "store": store_name,
            "size": size,
            "content_type": object.content_type,
            "model": model,
            "voice": voice,
        }))
    }
}

/// Register `speech_to_text` + `text_to_speech` (SOUL §7). Called from `AppState`
/// whenever object storage is configured (they read/write stored files), mirroring
/// `register_copy_object_tool`. The two tools carry the config-default STT/TTS model
/// (+ voice) as the last-resort fallback under a per-user override; both share the
/// llmleaf client, the storage registry, and the store.
pub(crate) fn register_audio_tools(
    registry: &mut ToolRegistry,
    llm: OpenRouterClient,
    storage: StorageRegistry,
    store: Store,
    transcription_model: String,
    speech_model: String,
    speech_voice: String,
) {
    registry.register(Arc::new(SpeechToTextTool {
        llm: llm.clone(),
        storage: storage.clone(),
        store: store.clone(),
        default_model: transcription_model,
    }));
    registry.register(Arc::new(TextToSpeechTool {
        llm,
        storage,
        store,
        default_model: speech_model,
        default_voice: speech_voice,
    }));
}
