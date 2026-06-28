//! OCR tool: `ocr_document` — extract the text of a stored image/PDF on demand.

use catalerum_core::ocr::OcrRequest;
use catalerum_core::provider::OcrEngine;
use catalerum_llm::VisionOcr;
use catalerum_ocr::FallbackOcr;

use super::audio::user_llm_settings;
use super::*;

/// `ocr_document` — OCR a stored image (or PDF) to text (SOUL §7/§10). Reads the
/// bytes from a files store and runs them through the effective engine: an
/// explicit `model` arg (→ the caller's per-user `ocr_model` override) routes
/// through the **vision** chat engine with that model; otherwise the configured
/// `[ocr]` chain (mistral → vision → tesseract) serves the request. Ingested
/// images are already indexed — `read_object` returns their extracted text;
/// this tool re-reads the document on demand (ingest OCR off, a different
/// model, or a store that was never scanned). Registered whenever object
/// storage is configured; Boa-callable via the shared registry. Gated
/// `storage:read` — same authority as `read_object`/`speech_to_text`.
pub(crate) struct OcrDocumentTool {
    pub(crate) llm: OpenRouterClient,
    pub(crate) storage: StorageRegistry,
    pub(crate) store: Store,
    /// The configured engine chain; `None` when no engine is configured.
    pub(crate) chain: Option<Arc<FallbackOcr>>,
    pub(crate) max_image_bytes: usize,
    pub(crate) max_document_bytes: usize,
}

impl OcrDocumentTool {
    /// The byte cap for `content_type` (PDFs get the larger document cap).
    fn max_bytes(&self, content_type: &str) -> usize {
        if content_type.starts_with("application/pdf") {
            self.max_document_bytes
        } else {
            self.max_image_bytes
        }
    }
}

#[async_trait]
impl Tool for OcrDocumentTool {
    fn name(&self) -> &str {
        "ocr_document"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "storage")
    }
    fn description(&self) -> &str {
        "Extract the text of a stored image (png/jpeg/webp/gif — or a PDF when a \
         Mistral-style OCR API is configured) via OCR. Pass the object `key` of the \
         file in your files store (omit `store` for your default). Note: files a \
         store has indexed already have their text — read_object returns it; use \
         ocr_document to re-read on demand or when ingest OCR is not configured. \
         Optionally hint the text `language`, or pass a vision `model` id to OCR \
         through that chat model instead of the configured engines."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Object key of the image/PDF (store-relative)." },
                "store": { "type": "string", "description": "Files store name; omitted → your default files store." },
                "model": { "type": "string", "description": "Vision chat model id to OCR with (models advertising image input; search_models lists them); omitted → your OCR-model setting, then the configured [ocr] engines." },
                "language": { "type": "string", "description": "Language hint for the text (engine-interpreted). Optional." }
            },
            "required": ["key"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let key = required_str(&args, "key")?;
        let store_name = opt_str_some(&args, "store");
        // An explicit model (arg → per-user setting) targets the vision engine;
        // without one the configured chain decides.
        let model = match opt_str_some(&args, "model") {
            Some(m) => Some(m),
            None => user_llm_settings(&self.store, ws, ctx)
                .await
                .and_then(|s| s.ocr_model),
        };
        let engine: Arc<dyn OcrEngine> =
            match &model {
                Some(m) => Arc::new(VisionOcr::new(self.llm.clone(), m.clone())),
                None => match &self.chain {
                    Some(chain) => chain.clone(),
                    None => return Err(Error::invalid(
                        "no OCR engine configured; set [ocr] in the server config or pass `model`",
                    )),
                },
            };
        let (bytes, content_type) = crate::routes::storage::read_object_bytes(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            (store_name.as_deref(), &key),
        )
        .await
        .map_err(|e| Error::other(e.to_string()))?;
        let content_type = content_type.ok_or_else(|| {
            Error::invalid(format!(
                "`{key}` has no content type; OCR never guesses a format"
            ))
        })?;
        if !engine.supports(&content_type) {
            return Err(Error::invalid(format!(
                "the {} OCR engine does not support `{content_type}`",
                engine.name()
            )));
        }
        let max = self.max_bytes(&content_type);
        if bytes.len() > max {
            return Err(Error::invalid(format!(
                "`{key}` is {} bytes, over the {max}-byte OCR cap (documents are never truncated)",
                bytes.len()
            )));
        }
        let mut request = OcrRequest::new(bytes, content_type);
        if let Some(language) = opt_str_some(&args, "language") {
            request = request.with_language(language);
        }
        if let Some(m) = model {
            request = request.with_model(m);
        }
        let response = engine.ocr(request).await?;
        Ok(json!({
            "text": response.text,
            "engine": response.engine,
            "key": key,
        }))
    }
}

/// Register `ocr_document` (SOUL §7/§10). Called from `AppState` whenever object
/// storage is configured (the input is a stored file), right after the audio
/// tools. `chain` is the boot-built `[ocr]` engine chain (`None` = unconfigured
/// — the tool still registers so a per-user/arg vision model works, and errors
/// clearly otherwise).
pub(crate) fn register_ocr_tool(
    registry: &mut ToolRegistry,
    llm: OpenRouterClient,
    storage: StorageRegistry,
    store: Store,
    chain: Option<Arc<FallbackOcr>>,
    max_image_bytes: usize,
    max_document_bytes: usize,
) {
    registry.register(Arc::new(OcrDocumentTool {
        llm,
        storage,
        store,
        chain,
        max_image_bytes,
        max_document_bytes,
    }));
}
