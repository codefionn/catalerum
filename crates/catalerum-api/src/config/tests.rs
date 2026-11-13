use super::*;

#[test]
fn default_is_well_formed() {
    let c = Config::default();
    assert_eq!(c.server.listen, "0.0.0.0:8787");
    assert!(c.auth.dev_login);
    assert!(c.valkey.enabled);
}

#[test]
fn only_single_user_mode_caches_the_sole_user() {
    // The mode gate for the §29 sole-user personalization cache: single_user
    // may cache, multi_user never consults it (default is single_user).
    assert!(DeploymentMode::default().caches_sole_user());
    assert!(DeploymentMode::SingleUser.caches_sole_user());
    assert!(!DeploymentMode::MultiUser.caches_sole_user());
}

#[test]
fn google_oauth_disabled_when_absent_and_derives_redirect() {
    // No [google] section → the feature is off (the /auth/google/* routes 404).
    let c = Config::default();
    assert!(!c.google.is_enabled());
    // The redirect URL derives off the API base URL when not set explicitly.
    assert_eq!(
        c.google.effective_redirect_url("https://app.example.com"),
        "https://app.example.com/auth/google/callback"
    );
    // With id + secret set, the feature enables; an explicit redirect wins.
    let g = GoogleConfig {
        client_id: "cid".into(),
        client_secret: Secret::from("sec"),
        redirect_url: "https://x/cb".into(),
        state_secret: None,
        ..Default::default()
    };
    assert!(g.is_enabled());
    assert_eq!(g.effective_redirect_url("https://ignored"), "https://x/cb");
    // Push is opt-in (off by default).
    assert!(!g.push);
}

#[test]
fn sso_disabled_when_absent() {
    // No [sso] section → the whole feature is off (routes 404), and every knob
    // has its safe deny-by-default value.
    let c = Config::default();
    assert!(!c.sso.is_enabled());
    assert!(!c.sso.jit_enabled(), "JIT is deny-by-default");
    assert!(!c.sso.trust_email);
    assert_eq!(c.sso.effective_scopes(), "openid email profile");
    assert_eq!(c.sso.jit_org_slug(), "default");
    assert_eq!(c.sso.jit_org_role_token(), "member");
    assert_eq!(
        c.sso.jit_workspace_slug(),
        None,
        "workspace auto-join is off by default"
    );
    assert_eq!(c.sso.jit_workspace_role_token(), "member");
    assert_eq!(c.sso.leeway(), 60);
}

#[test]
fn sso_parses_and_enables_from_toml() {
    let toml = r#"
            [sso]
            issuer = "https://accounts.example.com"
            client_id = "catalerum"
            client_secret = "shhh"
            scopes = "openid email"
            jit_provisioning = "enabled"
            jit_organisation = "acme"
            jit_org_role = "admin"
            jit_workspace = "default"
            jit_workspace_role = "member"
            trust_email = true
            leeway_secs = 120
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    assert!(c.sso.is_enabled());
    assert!(c.sso.jit_enabled());
    assert!(c.sso.trust_email);
    assert_eq!(c.sso.effective_scopes(), "openid email");
    assert_eq!(c.sso.jit_org_slug(), "acme");
    assert_eq!(c.sso.jit_org_role_token(), "admin");
    assert_eq!(c.sso.jit_workspace_slug(), Some("default"));
    assert_eq!(c.sso.jit_workspace_role_token(), "member");
    assert_eq!(c.sso.leeway(), 120);
    // client_secret is a redacting Secret, exposed only where needed.
    assert_eq!(c.sso.client_secret.expose(), "shhh");
    // Redirect URL derives from the API base when unset.
    assert_eq!(
        c.sso.effective_redirect_url("https://api.example.com/"),
        "https://api.example.com/auth/sso/callback"
    );
}

#[test]
fn sso_needs_both_issuer_and_client_id() {
    let only_issuer: Config = toml::from_str("[sso]\nissuer = \"https://x\"\n").unwrap();
    assert!(!only_issuer.sso.is_enabled());
    let only_client: Config = toml::from_str("[sso]\nclient_id = \"c\"\n").unwrap();
    assert!(!only_client.sso.is_enabled());
}

#[test]
fn sso_public_login_url_prefers_public_url_then_server_base() {
    // Explicit [sso].public_url wins (trailing slash trimmed).
    let c: Config =
        toml::from_str("[sso]\npublic_url = \"https://sso.example.com/\"\n").unwrap();
    assert_eq!(
        c.sso.public_login_url(Some("https://api.internal")),
        Some("https://sso.example.com/auth/sso/login".to_string())
    );
    // Unset → the server's explicit base_url.
    let d = Config::default();
    assert_eq!(
        d.sso.public_login_url(Some("https://api.example.com/")),
        Some("https://api.example.com/auth/sso/login".to_string())
    );
    // Neither set (blank counts as unset) → None; the SPA derives the origin.
    assert_eq!(d.sso.public_login_url(None), None);
    assert_eq!(d.sso.public_login_url(Some("   ")), None);
}

#[test]
fn llm_modality_defaults_target_echo() {
    // Generative modalities share echo. Native decisions require a configured model.
    let c = LlmConfig::default();
    assert_eq!(c.default_model, "echo");
    assert!(c.decision_model.is_none());
    assert_eq!(c.embedding_model, "echo");
    assert_eq!(c.speech_model, "echo");
    assert_eq!(c.transcription_model, "echo");
    assert_eq!(c.speech_voice, "alloy");
    assert_eq!(c.embedding_dimensions, None);
    assert!(!c.control_plane_enabled);
}

#[test]
fn all_in_one_enables_llm_control_plane() {
    let c: Config = toml::from_str(include_str!("../../../../config/all-in-one.toml")).unwrap();
    assert!(c.llm.control_plane_enabled);
}

#[test]
fn llm_modality_models_parse_from_toml() {
    let toml = r#"
            [llm]
            base_url = "http://llm/v1"
            api_key = "k"
            default_model = "gpt-4o"
            embedding_model = "text-embedding-3-small"
            embedding_dimensions = 256
            speech_model = "tts-1"
            speech_voice = "nova"
            transcription_model = "gpt-4o-transcribe"
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    assert_eq!(c.llm.embedding_model, "text-embedding-3-small");
    assert_eq!(c.llm.embedding_dimensions, Some(256));
    assert_eq!(c.llm.speech_model, "tts-1");
    assert_eq!(c.llm.speech_voice, "nova");
    assert_eq!(c.llm.transcription_model, "gpt-4o-transcribe");
}

#[test]
fn llm_partial_toml_keeps_modality_defaults() {
    // A pre-existing config that only sets chat fields still gets working
    // modality defaults (serde(default) per field).
    let toml = r#"
            [llm]
            base_url = "http://llm/v1"
            api_key = "k"
            default_model = "echo"
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    assert_eq!(c.llm.embedding_model, "echo");
    assert_eq!(c.llm.speech_voice, "alloy");
}

#[test]
fn effective_base_url_prefers_explicit() {
    let mut s = ServerConfig::default();
    assert_eq!(s.effective_base_url(), "http://0.0.0.0:8787");
    s.base_url = Some("https://app.example/".to_string());
    assert_eq!(s.effective_base_url(), "https://app.example");
}

#[test]
fn effective_web_url_defaults_and_trims() {
    let mut s = ServerConfig::default();
    assert_eq!(s.effective_web_url(), "http://localhost:8080");
    s.web_url = "https://app.example/".to_string();
    assert_eq!(s.effective_web_url(), "https://app.example");
}

#[test]
fn fetch_defaults_are_safe() {
    let f = FetchConfig::default();
    assert_eq!(f.backend, "http");
    assert!(!f.allow_private_hosts, "SSRF guard must default closed");
    assert_eq!(f.timeout_secs, 30);
    assert!(!f.firecrawl.is_enabled());
    assert!(!f.browser.is_enabled());
}

#[test]
fn fetch_section_parses_from_toml() {
    let toml = r#"
            [llm]
            base_url = "http://llm/v1"
            api_key = "k"
            default_model = "echo"

            [fetch]
            backend = "firecrawl"
            timeout_secs = 45
            allow_private_hosts = true

            [fetch.firecrawl]
            base_url = "http://firecrawl.internal:3002"
            api_key = "fc-secret"

            [fetch.browser]
            cdp_url = "ws://localhost:9222/devtools/browser/abc"
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    assert_eq!(c.fetch.backend, "firecrawl");
    assert_eq!(c.fetch.timeout_secs, 45);
    assert!(c.fetch.allow_private_hosts);
    assert!(c.fetch.firecrawl.is_enabled());
    assert_eq!(c.fetch.firecrawl.base_url, "http://firecrawl.internal:3002");
    assert!(c.fetch.browser.is_enabled());
}

#[test]
fn fetch_partial_toml_keeps_defaults() {
    // A config with no [fetch] section still gets safe defaults.
    let c: Config = toml::from_str("[server]\nlisten = \"127.0.0.1:1\"\n").unwrap();
    assert_eq!(c.fetch.backend, "http");
    assert!(!c.fetch.allow_private_hosts);
}

#[test]
fn channels_telegram_parses_named_tables_and_gates_is_empty() {
    // Each `[channels.telegram.<name>]` is a bot_token + chat_id (+base_url?).
    let toml = r#"
            [channels.telegram.default]
            bot_token = "123:secret"
            chat_id = "42"

            [channels.telegram.ops]
            bot_token = "9:k"
            chat_id = "-100500"
            base_url = "http://localhost:9099"
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    assert_eq!(c.channels.telegram.len(), 2);
    let ops = c.channels.telegram.get("ops").unwrap();
    assert_eq!(ops.chat_id, "-100500");
    assert_eq!(ops.base_url, "http://localhost:9099");
    assert!(ops.is_configured());
    assert!(!c.channels.is_empty());

    // A telegram entry missing credentials does not count as configured.
    let blank: Config =
        toml::from_str("[channels.telegram.x]\nbot_token = \"\"\nchat_id = \"\"\n").unwrap();
    assert!(blank.channels.is_empty());
}

#[test]
fn channels_discord_parses_named_map_and_env_sets_default() {
    // `[channels.discord]` is a name → webhook-url map (SOUL §25).
    let toml = r#"
            [channels.discord]
            default = "https://discord/default"
            ops = "https://discord/ops"
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    assert_eq!(
        c.channels.discord.get("ops").unwrap(),
        "https://discord/ops"
    );
    assert_eq!(c.channels.discord.len(), 2);
    assert!(!c.channels.is_empty());
    // No [channels] → empty (the notify tool isn't registered).
    let none: Config = toml::from_str("[server]\nlisten=\"127.0.0.1:1\"\n").unwrap();
    assert!(none.channels.is_empty());
}

#[test]
fn storage_s3_parses_and_precedence_holds() {
    // `[storage.s3]` with creds → S3 backend enabled; bucket from `[storage]`.
    let toml = r#"
            [storage]
            bucket = "objects"
            local_path = "/var/lib/cat"

            [storage.s3]
            endpoint = "http://localhost:9000"
            access_key = "ak"
            secret_key = "sk"
            path_style = true
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    assert!(c.storage.enabled());
    assert!(c.storage.s3.enabled(), "creds present → S3 enabled");
    assert_eq!(c.storage.bucket_name(), "objects");
    assert_eq!(c.storage.s3.endpoint, "http://localhost:9000");
    assert!(c.storage.s3.path_style);
    // Region defaults to us-east-1 when unset.
    assert_eq!(c.storage.s3.region_name(), "us-east-1");

    // No creds → S3 disabled (falls back to local_path); storage still enabled.
    let local_only: Config = toml::from_str("[storage]\nlocal_path = \"/tmp/x\"\n").unwrap();
    assert!(local_only.storage.enabled() && !local_only.storage.s3.enabled());

    // `[storage.webdav]` with a url → WebDAV enabled (no S3 creds).
    let dav: Config =
        toml::from_str("[storage.webdav]\nurl = \"http://dav:8788/\"\nusername = \"u\"\n")
            .unwrap();
    assert!(dav.storage.enabled() && dav.storage.webdav.enabled());
    assert!(!dav.storage.s3.enabled());
    assert_eq!(dav.storage.webdav.url, "http://dav:8788/");

    // Neither → storage disabled.
    let none: Config = toml::from_str("[server]\nlisten=\"127.0.0.1:1\"\n").unwrap();
    assert!(!none.storage.enabled());

    // `max_object_bytes`: unset → the default; explicit → honored.
    assert_eq!(none.storage.max_object_bytes(), DEFAULT_MAX_OBJECT_BYTES);
    let capped: Config = toml::from_str("[storage]\nmax_object_bytes = 1048576\n").unwrap();
    assert_eq!(capped.storage.max_object_bytes(), 1_048_576);
}

#[test]
fn storage_named_backends_resolve_with_default_and_kinds() {
    // The legacy `[storage]` default + two named backends, one of the same
    // kind as another (two local folders) — the multi-backend case (SOUL §9).
    let toml = r#"
            [storage]
            local_path = "/data/files"

            [storage.backends.archive]
            local_path = "/mnt/archive"

            [storage.backends.minio]
            [storage.backends.minio.s3]
            endpoint = "http://localhost:9000"
            access_key = "ak"
            secret_key = "sk"
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    let backends = c.storage.resolved_backends();
    // default + archive + minio, named ones sorted, default first.
    let names: Vec<&str> = backends.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["default", "archive", "minio"]);
    // Kinds resolve by precedence.
    let kind = |n: &str| {
        backends
            .iter()
            .find(|(name, _)| name == n)
            .and_then(|(_, c)| c.kind())
            .unwrap()
    };
    assert_eq!(kind("default"), "local");
    assert_eq!(kind("archive"), "local");
    assert_eq!(kind("minio"), "s3");
    assert!(c.storage.enabled());

    // An explicit `[storage.backends.default]` overrides the legacy default
    // (it isn't listed twice).
    let override_default: Config = toml::from_str(
        "[storage]\nlocal_path = \"/a\"\n[storage.backends.default]\nlocal_path = \"/b\"\n",
    )
    .unwrap();
    let rb = override_default.storage.resolved_backends();
    assert_eq!(rb.iter().filter(|(n, _)| n == "default").count(), 1);
    assert_eq!(rb[0].1.local_path, "/b");

    // Named-only (no legacy default) still enables storage; an empty backend
    // table is ignored.
    let named_only: Config = toml::from_str(
        "[storage.backends.box]\nlocal_path = \"/x\"\n[storage.backends.empty]\n",
    )
    .unwrap();
    assert!(named_only.storage.enabled());
    let rb2 = named_only.storage.resolved_backends();
    assert_eq!(rb2.len(), 1, "the empty backend table is skipped");
    assert_eq!(rb2[0].0, "box");
}

#[test]
fn storage_workspace_assignment_parses_and_matches() {
    use catalerum_core::model::Workspace;
    use catalerum_core::{OrganisationId, WorkspaceId};

    let ws = Workspace {
        id: WorkspaceId::new(),
        organisation_id: OrganisationId::new(),
        name: "Team A".to_string(),
        slug: "team-a".to_string(),
        archived_at: None,
    };
    let other = Workspace {
        id: WorkspaceId::new(),
        organisation_id: ws.organisation_id,
        name: "Team B".to_string(),
        slug: "team-b".to_string(),
        archived_at: None,
    };

    // Unassigned (the default) admits every workspace — the pre-feature
    // behavior, so existing configs are unaffected.
    let toml = r#"
            [storage.backends.shared]
            local_path = "/x"
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    let shared = &c.storage.backends["shared"];
    assert!(shared.workspaces.is_empty());
    assert!(shared.assigned_to(&ws) && shared.assigned_to(&other));

    // Assigned by slug (case-insensitive) or UUID; everything else is out.
    let toml = format!(
        r#"
            [storage]
            local_path = "/data/files"
            workspaces = ["Team-A"]

            [storage.backends.dav]
            workspaces = ["{}"]
            [storage.backends.dav.webdav]
            url = "http://dav:8788/"
        "#,
        ws.id
    );
    let c: Config = toml::from_str(&toml).unwrap();
    // The default backend carries the top-level assignment.
    let default = c.storage.default_backend();
    assert!(default.assigned_to(&ws), "slug matches case-insensitively");
    assert!(!default.assigned_to(&other));
    let dav = &c.storage.backends["dav"];
    assert!(dav.assigned_to(&ws), "UUID entry matches by id");
    assert!(!dav.assigned_to(&other));

    // The bare predicate ignores whitespace padding around entries.
    assert!(workspace_assigned(&[" team-a ".to_string()], &ws));
    assert!(!workspace_assigned(&["team-a".to_string()], &other));
}

#[test]
fn secrets_redact_in_debug_but_round_trip() {
    let toml = r#"
            [llm]
            api_key = "super-secret-key"
            [neo4j]
            password = "neo-pw"
            [storage.s3]
            access_key = "ak"
            secret_key = "s3-secret"
            [fetch.firecrawl]
            api_key = "fc-secret"
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    // Transparent deserialize: the values are present via `expose()`.
    assert_eq!(c.llm.api_key.expose(), "super-secret-key");
    assert_eq!(c.neo4j.password.expose(), "neo-pw");
    assert_eq!(c.storage.s3.secret_key.expose(), "s3-secret");
    assert_eq!(c.fetch.firecrawl.api_key.expose(), "fc-secret");

    // `Debug` of the whole config redacts EVERY secret — no value leaks.
    let dbg = format!("{c:?}");
    for leaked in ["super-secret-key", "neo-pw", "s3-secret", "fc-secret"] {
        assert!(
            !dbg.contains(leaked),
            "secret `{leaked}` leaked in Debug output"
        );
    }
    assert!(
        dbg.contains("Secret(\"***\")"),
        "secrets render as redacted"
    );

    // Transparent serialize: a `Secret` still round-trips its value (config
    // persistence), so redaction is Debug-only, not data loss.
    assert_eq!(serde_json::to_string(&Secret::from("x")).unwrap(), "\"x\"");
}

#[test]
fn search_config_parses_status_and_redacts_keys() {
    let toml = r#"
            [search]
            backend = "tavily"
            [search.brave]
            api_key = "brave-secret"
            [search.tavily]
            api_key = "tvly-secret"
            [search.searxng]
            base_url = "https://searx.example.org"
            [search.google]
            api_key = "g-secret"
            cx = "0123:abc"
            [search.serpapi]
            api_key = "serp-secret"
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    assert_eq!(c.search.backend, "tavily");
    // Values present via expose(); keys redacted in Debug.
    assert_eq!(c.search.brave.api_key.expose(), "brave-secret");
    assert_eq!(c.search.serpapi.engine, "google"); // default applied
    let dbg = format!("{:?}", c.search);
    for leaked in ["brave-secret", "tvly-secret", "g-secret", "serp-secret"] {
        assert!(!dbg.contains(leaked), "search secret `{leaked}` leaked");
    }
    // is_enabled / provider_status: configured providers on, Exa (unset) off.
    let status = c.search.provider_status();
    let on: std::collections::BTreeMap<_, _> = status.into_iter().collect();
    assert!(on["brave"] && on["tavily"] && on["searxng"] && on["google"] && on["serpapi"]);
    assert!(!on["exa"]);
    assert!(c.search.any_enabled());
    // Google needs BOTH key and cx; default config has neither -> off.
    assert!(!SearchConfig::default().any_enabled());
    assert_eq!(SearchConfig::default().backend, "brave");
}

#[test]
fn deserializes_full_config_ignoring_unknown_sections() {
    // Mirrors config/catalerum.toml: unknown sections must be ignored.
    let toml = r#"
            [server]
            listen = "127.0.0.1:9000"

            [database]
            url = "postgres://x"

            [llm]
            base_url = "http://llm/v1"
            api_key = "k"
            default_model = "echo"

            [neo4j]
            url = "bolt://localhost"

            [mcp]
            enabled = false
        "#;
    let c: Config = toml::from_str(toml).unwrap();
    assert_eq!(c.server.listen, "127.0.0.1:9000");
    assert_eq!(c.database.url, "postgres://x");
    assert_eq!(c.llm.default_model, "echo");
    // Untouched section keeps its default.
    assert!(c.valkey.enabled);
}

#[test]
fn secrets_master_key_decodes_and_validates() {
    use base64::Engine as _;
    // Unset → None (feature disabled, not an error).
    assert!(SecretsConfig::default()
        .master_key_bytes()
        .unwrap()
        .is_none());

    // A valid 32-byte base64 key → Some(key).
    let key = [7u8; 32];
    let b64 = base64::engine::general_purpose::STANDARD.encode(key);
    let cfg = SecretsConfig {
        master_key: Secret::from(b64.as_str()),
    };
    assert_eq!(cfg.master_key_bytes().unwrap(), Some(key));

    // Wrong length → Err (fail loud, never silently disable a set-but-broken key).
    let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
    let cfg = SecretsConfig {
        master_key: Secret::from(short.as_str()),
    };
    assert!(cfg.master_key_bytes().is_err());

    // Not base64 → Err.
    let cfg = SecretsConfig {
        master_key: Secret::from("not base64!!!"),
    };
    assert!(cfg.master_key_bytes().is_err());
}

#[test]
fn external_db_defaults_are_bounded() {
    let c = ExternalDbConfig::default();
    assert_eq!(c.pool_max_connections, 5);
    assert_eq!(c.statement_timeout_ms, 15_000);
    assert_eq!(c.max_rows, 1000);
    assert!(c.connections.is_empty());
}

#[test]
fn external_db_connections_deserialize_and_are_workspace_scoped() {
    let workspace_id = catalerum_core::WorkspaceId::new();
    let toml = format!(
        r#"
            [external_db.connections.reporting]
            host = "db.internal"
            database = "reports"
            username = "reader"
            password = "do-not-log"
            sslmode = "require"
            schema = "analytics"
            pool_max = 2
            workspaces = ["Team-A", "{workspace_id}"]
            "#
    );
    let c: Config = toml::from_str(&toml).unwrap();
    let reporting = &c.external_db.connections["reporting"];
    assert_eq!(reporting.port, 5432);
    assert_eq!(reporting.resolved_password().unwrap(), "do-not-log");
    assert!(!format!("{reporting:?}").contains("do-not-log"));

    let by_slug = catalerum_core::model::Workspace {
        id: catalerum_core::WorkspaceId::new(),
        organisation_id: catalerum_core::OrganisationId::new(),
        name: "Team A".to_string(),
        slug: "team-a".to_string(),
        archived_at: None,
    };
    let by_id = catalerum_core::model::Workspace {
        id: workspace_id,
        organisation_id: catalerum_core::OrganisationId::new(),
        name: "Other".to_string(),
        slug: "other".to_string(),
        archived_at: None,
    };
    let excluded = catalerum_core::model::Workspace {
        id: catalerum_core::WorkspaceId::new(),
        organisation_id: catalerum_core::OrganisationId::new(),
        name: "Excluded".to_string(),
        slug: "excluded".to_string(),
        archived_at: None,
    };
    assert!(reporting.assigned_to(&by_slug));
    assert!(reporting.assigned_to(&by_id));
    assert!(!reporting.assigned_to(&excluded));

    let pg = reporting.postgres_config();
    assert_eq!(pg.host, "db.internal");
    assert_eq!(pg.sslmode.as_deref(), Some("require"));
    assert_eq!(pg.schema.as_deref(), Some("analytics"));
    assert_eq!(pg.pool_max, Some(2));
}

#[test]
fn env_overrides_the_shared_link_signing_secrets() {
    // The multi-pod HA fix (SOUL §16 M7): the link-signing secrets must be
    // injectable from the environment (a k8s Secret) so every pod signs with
    // the SAME key and links verify across pods — otherwise each pod falls
    // back to a fresh per-process random key and links minted on one pod fail
    // on another. Unset ⇒ still `None` (the per-process-random fallback).
    assert!(Config::default().server.download_secret.is_none());
    assert!(Config::default().server.trigger_secret.is_none());

    // Safe on edition 2021; the only production caller (`main.rs`) reads the env
    // once at single-threaded boot, and no other test asserts on these fields.
    std::env::set_var("CATALERUM_SERVER__DOWNLOAD_SECRET", "dl-shared-key");
    std::env::set_var("CATALERUM_SERVER__TRIGGER_SECRET", "tr-shared-key");
    let cfg = Config::default().with_env_overrides();
    std::env::remove_var("CATALERUM_SERVER__DOWNLOAD_SECRET");
    std::env::remove_var("CATALERUM_SERVER__TRIGGER_SECRET");

    assert_eq!(
        cfg.server.download_secret.as_ref().map(Secret::expose),
        Some("dl-shared-key")
    );
    assert_eq!(
        cfg.server.trigger_secret.as_ref().map(Secret::expose),
        Some("tr-shared-key")
    );
}

#[test]
fn env_overrides_configure_sso() {
    // SSO is env-driven in prod (issuer/client from the deployment, secrets from a
    // k8s Secret). Without env handling the whole feature stays off and the web
    // login view shows no "Sign in with SSO" button — regression guard for that.
    assert!(!Config::default().sso.is_enabled());

    std::env::set_var(
        "CATALERUM_SSO__ISSUER",
        "https://id.example.com/realms/default",
    );
    std::env::set_var("CATALERUM_SSO__CLIENT_ID", "catalerum");
    std::env::set_var("CATALERUM_SSO__CLIENT_SECRET", "cs-secret");
    std::env::set_var("CATALERUM_SSO__JIT_PROVISIONING", "enabled");
    let cfg = Config::default().with_env_overrides();
    std::env::remove_var("CATALERUM_SSO__ISSUER");
    std::env::remove_var("CATALERUM_SSO__CLIENT_ID");
    std::env::remove_var("CATALERUM_SSO__CLIENT_SECRET");
    std::env::remove_var("CATALERUM_SSO__JIT_PROVISIONING");

    assert!(cfg.sso.is_enabled());
    assert_eq!(cfg.sso.issuer, "https://id.example.com/realms/default");
    assert_eq!(cfg.sso.client_id, "catalerum");
    assert_eq!(cfg.sso.client_secret.expose(), "cs-secret");
    assert!(cfg.sso.jit_enabled());
}

#[test]
fn telemetry_defaults_are_private_and_exporters_are_disabled() {
    let telemetry = Config::default().telemetry;
    assert!(!telemetry.otlp.enabled);
    assert!(!telemetry.langfuse.enabled);
    assert_eq!(telemetry.otlp.content, TelemetryContent::MetadataOnly);
    assert_eq!(telemetry.langfuse.content, TelemetryContent::MetadataOnly);
    assert_eq!(telemetry.sample_ratio, 1.0);
}

#[test]
fn telemetry_toml_parses_independent_content_policies() {
    let config: Config = toml::from_str(
        r#"
            [telemetry]
            service_name = "catalog-test"
            sample_ratio = 0.25
            [telemetry.otlp]
            enabled = true
            endpoint = "http://collector:4318"
            content = "everything"
            headers = { x-api-key = "secret" }
            [telemetry.langfuse]
            enabled = true
            public_key = "pk-lf-test"
            secret_key = "sk-lf-test"
            content = "all-except-system-prompts"
            "#,
    )
    .unwrap();
    assert_eq!(config.telemetry.service_name, "catalog-test");
    assert_eq!(config.telemetry.sample_ratio, 0.25);
    assert_eq!(config.telemetry.otlp.content, TelemetryContent::Everything);
    assert_eq!(
        config.telemetry.langfuse.content,
        TelemetryContent::AllExceptSystemPrompts
    );
    assert_eq!(
        config.telemetry.otlp.headers["x-api-key"].expose(),
        "secret"
    );
}
