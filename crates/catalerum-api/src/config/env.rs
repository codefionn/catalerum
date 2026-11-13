//! `CATALERUM_`-prefixed environment overrides for [`Config`].

use super::Config;

impl Config {
    /// Apply `CATALERUM_`-prefixed environment overrides over this config. The
    /// section delimiter is `__`; only the fields the API needs are wired:
    ///
    /// - `CATALERUM_SERVER__LISTEN`, `CATALERUM_SERVER__POD_IP`,
    ///   `CATALERUM_SERVER__BASE_URL`, `CATALERUM_SERVER__WEB_URL`
    /// - `CATALERUM_SERVER__DOWNLOAD_SECRET`, `CATALERUM_SERVER__TRIGGER_SECRET`
    ///   (shared link-signing keys — set them so links verify across pods, SOUL §16 M7)
    /// - `CATALERUM_DATABASE__URL`
    /// - `CATALERUM_VALKEY__URL`, `CATALERUM_VALKEY__ENABLED`
    /// - `CATALERUM_LLM__BASE_URL`, `CATALERUM_LLM__API_KEY`, `CATALERUM_LLM__CONTROL_TOKEN`,
    ///   `CATALERUM_LLM__CONTROL_PLANE_ENABLED`, `CATALERUM_LLM__DEFAULT_MODEL`,
    ///   `CATALERUM_LLM__EMBEDDING_MODEL`, `CATALERUM_LLM__EMBEDDING_DIMENSIONS`,
    ///   `CATALERUM_LLM__SPEECH_MODEL`, `CATALERUM_LLM__SPEECH_VOICE`,
    ///   `CATALERUM_LLM__TRANSCRIPTION_MODEL`
    /// - `CATALERUM_TELEMETRY__SERVICE_NAME`, `CATALERUM_TELEMETRY__SAMPLE_RATIO`,
    ///   `CATALERUM_TELEMETRY__OTLP__ENABLED`, `CATALERUM_TELEMETRY__OTLP__ENDPOINT`,
    ///   `CATALERUM_TELEMETRY__OTLP__CONTENT`,
    ///   `CATALERUM_TELEMETRY__LANGFUSE__ENABLED`, `CATALERUM_TELEMETRY__LANGFUSE__ENDPOINT`,
    ///   `CATALERUM_TELEMETRY__LANGFUSE__PUBLIC_KEY`,
    ///   `CATALERUM_TELEMETRY__LANGFUSE__SECRET_KEY`,
    ///   `CATALERUM_TELEMETRY__LANGFUSE__CONTENT`
    /// - `CATALERUM_OCR__VISION_MODEL`, `CATALERUM_OCR__MISTRAL__BASE_URL`,
    ///   `CATALERUM_OCR__MISTRAL__API_KEY`, `CATALERUM_OCR__MISTRAL__MODEL`,
    ///   `CATALERUM_OCR__TESSERACT__ENABLED`, `CATALERUM_OCR__TESSERACT__PATH`,
    ///   `CATALERUM_OCR__TESSERACT__LANGUAGES`
    /// - `CATALERUM_AUTH__DEV_LOGIN`
    /// - `CATALERUM_SSO__ISSUER`, `CATALERUM_SSO__CLIENT_ID`, `CATALERUM_SSO__CLIENT_SECRET`,
    ///   `CATALERUM_SSO__REDIRECT_URL`, `CATALERUM_SSO__SCOPES`, `CATALERUM_SSO__STATE_SECRET`,
    ///   `CATALERUM_SSO__JIT_PROVISIONING`, `CATALERUM_SSO__JIT_ORGANISATION`,
    ///   `CATALERUM_SSO__JIT_ORG_ROLE`, `CATALERUM_SSO__TRUST_EMAIL`,
    ///   `CATALERUM_SSO__TOKEN_AUTH_BASIC`, `CATALERUM_SSO__LEEWAY_SECS`
    /// - `CATALERUM_FETCH__BACKEND`, `CATALERUM_FETCH__USER_AGENT`,
    ///   `CATALERUM_FETCH__TIMEOUT_SECS`, `CATALERUM_FETCH__MAX_BYTES`,
    ///   `CATALERUM_FETCH__ALLOW_PRIVATE_HOSTS`,
    ///   `CATALERUM_FETCH__FIRECRAWL__BASE_URL`, `CATALERUM_FETCH__FIRECRAWL__API_KEY`,
    ///   `CATALERUM_FETCH__BROWSER__CDP_URL`
    /// - `CATALERUM_SEARCH__BACKEND`, `CATALERUM_SEARCH__BRAVE__API_KEY`,
    ///   `CATALERUM_SEARCH__TAVILY__API_KEY`, `CATALERUM_SEARCH__EXA__API_KEY`,
    ///   `CATALERUM_SEARCH__SEARXNG__BASE_URL`, `CATALERUM_SEARCH__GOOGLE__API_KEY`,
    ///   `CATALERUM_SEARCH__GOOGLE__CX`, `CATALERUM_SEARCH__SERPAPI__API_KEY`,
    ///   `CATALERUM_SEARCH__SERPAPI__ENGINE`
    /// - `CATALERUM_QDRANT__URL`, `CATALERUM_QDRANT__ENABLED`
    /// - `CATALERUM_NEO4J__URL`, `CATALERUM_NEO4J__USER`, `CATALERUM_NEO4J__PASSWORD`,
    ///   `CATALERUM_NEO4J__DATABASE`, `CATALERUM_NEO4J__ENABLED`
    /// - `CATALERUM_CURATION__ENABLED`, `CATALERUM_CURATION__MODEL`
    /// - `CATALERUM_EXEC__ENABLED`, `CATALERUM_EXEC__ALLOW` (comma-separated)
    /// - `CATALERUM_CHANNELS__DISCORD_WEBHOOK_URL` (the `default` Discord channel)
    /// - `CATALERUM_CHANNELS__TELEGRAM_BOT_TOKEN`, `CATALERUM_CHANNELS__TELEGRAM_CHAT_ID`,
    ///   `CATALERUM_CHANNELS__TELEGRAM_BASE_URL` (the `default` Telegram channel)
    /// - `CATALERUM_STORAGE__LOCAL_PATH`, `CATALERUM_STORAGE__BUCKET`,
    ///   `CATALERUM_STORAGE__MAX_OBJECT_BYTES`
    /// - `CATALERUM_STORAGE__S3__ENDPOINT`, `CATALERUM_STORAGE__S3__REGION`,
    ///   `CATALERUM_STORAGE__S3__ACCESS_KEY`, `CATALERUM_STORAGE__S3__SECRET_KEY`,
    ///   `CATALERUM_STORAGE__S3__PATH_STYLE`
    #[must_use]
    pub fn with_env_overrides(mut self) -> Self {
        use std::env::var;
        if let Ok(v) = var("CATALERUM_SERVER__LISTEN") {
            self.server.listen = v;
        }
        if let Ok(v) = var("CATALERUM_SERVER__POD_IP") {
            self.server.pod_ip = v;
        }
        if let Ok(v) = var("CATALERUM_SERVER__BASE_URL") {
            self.server.base_url = Some(v);
        }
        if let Ok(v) = var("CATALERUM_SERVER__WEB_URL") {
            self.server.web_url = v;
        }
        if let Ok(v) = var("CATALERUM_SERVER__CORS_EXTRA_ORIGINS") {
            // Comma-separated origin list (env has no TOML arrays).
            self.server.cors_extra_origins = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        // The link-signing secrets (SOUL §9/§11): under multiple pods every pod must
        // sign with the SAME key or a link minted on one pod fails to verify on
        // another (each else falls back to a fresh per-process random key). Sourced
        // from the env (a k8s Secret) like `[secrets].master_key`, never the shared
        // committed TOML.
        if let Ok(v) = var("CATALERUM_SERVER__DOWNLOAD_SECRET") {
            self.server.download_secret = Some(v.into());
        }
        if let Ok(v) = var("CATALERUM_SERVER__TRIGGER_SECRET") {
            self.server.trigger_secret = Some(v.into());
        }
        if let Ok(v) = var("CATALERUM_DATABASE__URL") {
            self.database.url = v;
        }
        if let Ok(v) = var("CATALERUM_VALKEY__URL") {
            self.valkey.url = v;
        }
        if let Ok(v) = var("CATALERUM_VALKEY__ENABLED") {
            if let Ok(b) = v.parse() {
                self.valkey.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_LLM__BASE_URL") {
            self.llm.base_url = v;
        }
        if let Ok(v) = var("CATALERUM_LLM__API_KEY") {
            self.llm.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_LLM__CONTROL_TOKEN") {
            self.llm.control_token = v.into();
        }
        if let Ok(v) = var("CATALERUM_LLM__CONTROL_PLANE_ENABLED") {
            if let Ok(b) = v.parse() {
                self.llm.control_plane_enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_LLM__DEFAULT_MODEL") {
            self.llm.default_model = v;
        }
        if let Ok(v) = var("CATALERUM_LLM__EMBEDDING_MODEL") {
            self.llm.embedding_model = v;
        }
        if let Ok(v) = var("CATALERUM_LLM__EMBEDDING_DIMENSIONS") {
            // An explicit 0 (or an unparseable value) means "provider default".
            self.llm.embedding_dimensions = v.parse().ok().filter(|&n| n != 0);
        }
        if let Ok(v) = var("CATALERUM_LLM__SPEECH_MODEL") {
            self.llm.speech_model = v;
        }
        if let Ok(v) = var("CATALERUM_LLM__SPEECH_VOICE") {
            self.llm.speech_voice = v;
        }
        if let Ok(v) = var("CATALERUM_LLM__TRANSCRIPTION_MODEL") {
            self.llm.transcription_model = v;
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__SERVICE_NAME") {
            self.telemetry.service_name = v;
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__SAMPLE_RATIO") {
            if let Ok(n) = v.parse() {
                self.telemetry.sample_ratio = n;
            }
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__OTLP__ENABLED") {
            if let Ok(b) = v.parse() {
                self.telemetry.otlp.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__OTLP__ENDPOINT") {
            self.telemetry.otlp.endpoint = v;
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__OTLP__CONTENT") {
            if let Ok(content) = v.parse() {
                self.telemetry.otlp.content = content;
            }
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__LANGFUSE__ENABLED") {
            if let Ok(b) = v.parse() {
                self.telemetry.langfuse.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__LANGFUSE__ENDPOINT") {
            self.telemetry.langfuse.endpoint = v;
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__LANGFUSE__PUBLIC_KEY") {
            self.telemetry.langfuse.public_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__LANGFUSE__SECRET_KEY") {
            self.telemetry.langfuse.secret_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__LANGFUSE__CONTENT") {
            if let Ok(content) = v.parse() {
                self.telemetry.langfuse.content = content;
            }
        }
        if let Ok(v) = var("CATALERUM_OCR__VISION_MODEL") {
            self.ocr.vision_model = v;
        }
        if let Ok(v) = var("CATALERUM_OCR__MISTRAL__BASE_URL") {
            self.ocr.mistral.base_url = v;
        }
        if let Ok(v) = var("CATALERUM_OCR__MISTRAL__API_KEY") {
            self.ocr.mistral.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_OCR__MISTRAL__MODEL") {
            self.ocr.mistral.model = v;
        }
        if let Ok(v) = var("CATALERUM_OCR__TESSERACT__ENABLED") {
            if let Ok(b) = v.parse() {
                self.ocr.tesseract.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_OCR__TESSERACT__PATH") {
            self.ocr.tesseract.path = v;
        }
        if let Ok(v) = var("CATALERUM_OCR__TESSERACT__LANGUAGES") {
            self.ocr.tesseract.languages = v;
        }
        if let Ok(v) = var("CATALERUM_AUTH__DEV_LOGIN") {
            if let Ok(b) = v.parse() {
                self.auth.dev_login = b;
            }
        }
        if let Ok(v) = var("CATALERUM_AUTH__PASSWORD_LOGIN") {
            if let Ok(b) = v.parse() {
                self.auth.password_login = b;
            }
        }
        if let Ok(v) = var("CATALERUM_FETCH__BACKEND") {
            self.fetch.backend = v;
        }
        if let Ok(v) = var("CATALERUM_FETCH__USER_AGENT") {
            self.fetch.user_agent = Some(v);
        }
        if let Ok(v) = var("CATALERUM_FETCH__TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                self.fetch.timeout_secs = n;
            }
        }
        if let Ok(v) = var("CATALERUM_FETCH__MAX_BYTES") {
            if let Ok(n) = v.parse() {
                self.fetch.max_bytes = n;
            }
        }
        if let Ok(v) = var("CATALERUM_FETCH__ALLOW_PRIVATE_HOSTS") {
            if let Ok(b) = v.parse() {
                self.fetch.allow_private_hosts = b;
            }
        }
        if let Ok(v) = var("CATALERUM_FETCH__FIRECRAWL__BASE_URL") {
            self.fetch.firecrawl.base_url = v;
        }
        if let Ok(v) = var("CATALERUM_FETCH__FIRECRAWL__API_KEY") {
            self.fetch.firecrawl.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_FETCH__BROWSER__CDP_URL") {
            self.fetch.browser.cdp_url = v;
        }
        if let Ok(v) = var("CATALERUM_SEARCH__BACKEND") {
            self.search.backend = v;
        }
        if let Ok(v) = var("CATALERUM_SEARCH__BRAVE__API_KEY") {
            self.search.brave.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_SEARCH__TAVILY__API_KEY") {
            self.search.tavily.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_SEARCH__EXA__API_KEY") {
            self.search.exa.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_SEARCH__SEARXNG__BASE_URL") {
            self.search.searxng.base_url = v;
        }
        if let Ok(v) = var("CATALERUM_SEARCH__GOOGLE__API_KEY") {
            self.search.google.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_SEARCH__GOOGLE__CX") {
            self.search.google.cx = v;
        }
        if let Ok(v) = var("CATALERUM_SEARCH__SERPAPI__API_KEY") {
            self.search.serpapi.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_SEARCH__SERPAPI__ENGINE") {
            self.search.serpapi.engine = v;
        }
        if let Ok(v) = var("CATALERUM_QDRANT__URL") {
            self.qdrant.url = v;
        }
        if let Ok(v) = var("CATALERUM_QDRANT__ENABLED") {
            if let Ok(b) = v.parse() {
                self.qdrant.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_NEO4J__URL") {
            self.neo4j.url = v;
        }
        if let Ok(v) = var("CATALERUM_NEO4J__USER") {
            self.neo4j.user = v;
        }
        if let Ok(v) = var("CATALERUM_NEO4J__PASSWORD") {
            self.neo4j.password = v.into();
        }
        if let Ok(v) = var("CATALERUM_NEO4J__DATABASE") {
            self.neo4j.database = v;
        }
        if let Ok(v) = var("CATALERUM_NEO4J__ENABLED") {
            if let Ok(b) = v.parse() {
                self.neo4j.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_CURATION__ENABLED") {
            if let Ok(b) = v.parse() {
                self.curation.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_CURATION__MODEL") {
            self.curation.model = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__ENABLED") {
            if let Ok(b) = v.parse() {
                self.exec.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_EXEC__ALLOW") {
            // Comma-separated program names.
            self.exec.allow = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        if let Ok(v) = var("CATALERUM_EXEC__BACKEND") {
            self.exec.backend = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__SHELL") {
            self.exec.shell = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__EPHEMERAL_ROOT") {
            self.exec.ephemeral_root = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__PODMAN__BINARY") {
            self.exec.podman.binary = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__PODMAN__IMAGE") {
            self.exec.podman.image = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__PODMAN__NETWORK") {
            self.exec.podman.network = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__K8S__NAMESPACE") {
            self.exec.k8s.namespace = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__K8S__IMAGE") {
            self.exec.k8s.image = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__K8S__STORAGE_CLASS") {
            self.exec.k8s.storage_class = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__PER_WORKSPACE") {
            if let Ok(b) = v.parse() {
                self.exec.per_workspace = b;
            }
        }
        if let Ok(v) = var("CATALERUM_EXEC__IDLE_TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                self.exec.idle_timeout_secs = n;
            }
        }
        if let Ok(v) = var("CATALERUM_EXEC__VOLUME_SIZE") {
            self.exec.volume_size = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__SANDBOX_NETWORK") {
            self.exec.sandbox_network = v;
        }
        if let Ok(v) = var("CATALERUM_CHANNELS__DISCORD_WEBHOOK_URL") {
            // The env shortcut configures the `default` Discord channel; named
            // channels are TOML-only (`[channels.discord]`).
            self.channels.discord.insert("default".to_string(), v);
        }
        // Env shortcuts for the `default` Telegram channel; named channels are
        // TOML-only (`[channels.telegram.<name>]`). The entry is created lazily so
        // a partial override (just a base_url, say) still composes with TOML.
        if let Ok(v) = var("CATALERUM_CHANNELS__TELEGRAM_BOT_TOKEN") {
            self.channels
                .telegram
                .entry("default".to_string())
                .or_default()
                .bot_token = v.into();
        }
        if let Ok(v) = var("CATALERUM_CHANNELS__TELEGRAM_CHAT_ID") {
            self.channels
                .telegram
                .entry("default".to_string())
                .or_default()
                .chat_id = v;
        }
        if let Ok(v) = var("CATALERUM_CHANNELS__TELEGRAM_BASE_URL") {
            self.channels
                .telegram
                .entry("default".to_string())
                .or_default()
                .base_url = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__LOCAL_PATH") {
            self.storage.local_path = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__BUCKET") {
            self.storage.bucket = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__MAX_OBJECT_BYTES") {
            if let Ok(n) = v.parse() {
                self.storage.max_object_bytes = n;
            }
        }
        if let Ok(v) = var("CATALERUM_STORAGE__S3__ENDPOINT") {
            self.storage.s3.endpoint = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__S3__REGION") {
            self.storage.s3.region = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__S3__ACCESS_KEY") {
            self.storage.s3.access_key = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__S3__SECRET_KEY") {
            self.storage.s3.secret_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_STORAGE__S3__PATH_STYLE") {
            if let Ok(b) = v.parse() {
                self.storage.s3.path_style = b;
            }
        }
        // Backup (SOUL §30). The destination mirrors the `[storage]` env shape so
        // production injects its S3/WebDAV credentials via the environment, not the
        // committed TOML.
        if let Ok(v) = var("CATALERUM_BACKUP__ENABLED") {
            if let Ok(b) = v.parse() {
                self.backup.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_BACKUP__INTERVAL_SECS") {
            if let Ok(n) = v.parse() {
                self.backup.interval_secs = n;
            }
        }
        if let Ok(v) = var("CATALERUM_BACKUP__PREFIX") {
            self.backup.prefix = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__KEEP") {
            if let Ok(n) = v.parse() {
                self.backup.keep = n;
            }
        }
        if let Ok(v) = var("CATALERUM_BACKUP__INCLUDE_OBJECTS") {
            if let Ok(b) = v.parse() {
                self.backup.include_objects = b;
            }
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__LOCAL_PATH") {
            self.backup.destination.local_path = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__BUCKET") {
            self.backup.destination.bucket = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__S3__ENDPOINT") {
            self.backup.destination.s3.endpoint = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__S3__REGION") {
            self.backup.destination.s3.region = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__S3__ACCESS_KEY") {
            self.backup.destination.s3.access_key = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__S3__SECRET_KEY") {
            self.backup.destination.s3.secret_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__S3__PATH_STYLE") {
            if let Ok(b) = v.parse() {
                self.backup.destination.s3.path_style = b;
            }
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__WEBDAV__URL") {
            self.backup.destination.webdav.url = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__WEBDAV__USERNAME") {
            self.backup.destination.webdav.username = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__WEBDAV__PASSWORD") {
            self.backup.destination.webdav.password = v.into();
        }
        // Encrypted secret store (SOUL §13) — set the master key via env, never TOML.
        if let Ok(v) = var("CATALERUM_SECRETS__MASTER_KEY") {
            self.secrets.master_key = v.into();
        }
        // SSO / OIDC (SOUL §18/§29) — issuer + client id enable it; secrets via env.
        if let Ok(v) = var("CATALERUM_SSO__ISSUER") {
            self.sso.issuer = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__CLIENT_ID") {
            self.sso.client_id = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__CLIENT_SECRET") {
            self.sso.client_secret = v.into();
        }
        if let Ok(v) = var("CATALERUM_SSO__REDIRECT_URL") {
            self.sso.redirect_url = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__SCOPES") {
            self.sso.scopes = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__STATE_SECRET") {
            self.sso.state_secret = Some(v.into());
        }
        if let Ok(v) = var("CATALERUM_SSO__JIT_PROVISIONING") {
            self.sso.jit_provisioning = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__JIT_ORGANISATION") {
            self.sso.jit_organisation = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__JIT_ORG_ROLE") {
            self.sso.jit_org_role = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__JIT_WORKSPACE") {
            self.sso.jit_workspace = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__JIT_WORKSPACE_ROLE") {
            self.sso.jit_workspace_role = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__TRUST_EMAIL") {
            if let Ok(b) = v.parse() {
                self.sso.trust_email = b;
            }
        }
        if let Ok(v) = var("CATALERUM_SSO__TOKEN_AUTH_BASIC") {
            if let Ok(b) = v.parse() {
                self.sso.token_auth_basic = b;
            }
        }
        if let Ok(v) = var("CATALERUM_SSO__LEEWAY_SECS") {
            if let Ok(n) = v.parse() {
                self.sso.leeway_secs = n;
            }
        }
        // Google OAuth (SOUL §16 M7) — secrets via env, never the committed TOML.
        if let Ok(v) = var("CATALERUM_GOOGLE__CLIENT_ID") {
            self.google.client_id = v;
        }
        if let Ok(v) = var("CATALERUM_GOOGLE__CLIENT_SECRET") {
            self.google.client_secret = v.into();
        }
        if let Ok(v) = var("CATALERUM_GOOGLE__REDIRECT_URL") {
            self.google.redirect_url = v;
        }
        if let Ok(v) = var("CATALERUM_GOOGLE__STATE_SECRET") {
            self.google.state_secret = Some(v.into());
        }
        // Microsoft OAuth (SOUL §8) — secrets via env, never the committed TOML.
        if let Ok(v) = var("CATALERUM_MICROSOFT__CLIENT_ID") {
            self.microsoft.client_id = v;
        }
        if let Ok(v) = var("CATALERUM_MICROSOFT__CLIENT_SECRET") {
            self.microsoft.client_secret = v.into();
        }
        if let Ok(v) = var("CATALERUM_MICROSOFT__TENANT") {
            self.microsoft.tenant = v;
        }
        if let Ok(v) = var("CATALERUM_MICROSOFT__REDIRECT_URL") {
            self.microsoft.redirect_url = v;
        }
        if let Ok(v) = var("CATALERUM_MICROSOFT__STATE_SECRET") {
            self.microsoft.state_secret = Some(v.into());
        }
        // External-Postgres caps (SOUL §11/§19).
        if let Ok(v) = var("CATALERUM_EXTERNAL_DB__POOL_MAX_CONNECTIONS") {
            if let Ok(n) = v.parse() {
                self.external_db.pool_max_connections = n;
            }
        }
        if let Ok(v) = var("CATALERUM_EXTERNAL_DB__STATEMENT_TIMEOUT_MS") {
            if let Ok(n) = v.parse() {
                self.external_db.statement_timeout_ms = n;
            }
        }
        if let Ok(v) = var("CATALERUM_EXTERNAL_DB__MAX_ROWS") {
            if let Ok(n) = v.parse() {
                self.external_db.max_rows = n;
            }
        }
        self
    }
}
