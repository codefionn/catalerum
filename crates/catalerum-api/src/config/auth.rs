//! Login, SSO, and the Google / Microsoft OAuth integrations.

use serde::{Deserialize, Serialize};

use super::Secret;

/// Auth toggles (SOUL §18). `dev_login` seeds the admin + prints a magic link.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AuthConfig {
    pub dev_login: bool,
    /// Enable instance-local Argon2id credentials and the first-boot setup API.
    /// Off by default so existing distributed/SSO deployments are unchanged.
    pub password_login: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            dev_login: true,
            password_login: false,
        }
    }
}

/// Single sign-on — the **OIDC first cut** (SOUL §18/§16 M7/§29). SOUL §29's open
/// question (OIDC vs SAML to ship first) is resolved here: **OIDC** ships;
/// **SAML is deferred**. This section configures the Authorization Code + PKCE
/// flow. When [`issuer`](SsoConfig::issuer) + [`client_id`](SsoConfig::client_id)
/// are unset the whole feature is **off** and the `/auth/sso/*` routes return
/// `404` — dev magic-link login is unaffected.
///
/// Secrets (`client_secret`, `state_secret`) use [`Secret`] — set them via env
/// (`CATALERUM_SSO__CLIENT_SECRET=…`), never the committed TOML.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SsoConfig {
    /// The OIDC **issuer** URL (e.g. `https://accounts.example.com`). Discovery reads
    /// `{issuer}/.well-known/openid-configuration`; every `id_token`'s `iss` must
    /// equal this. Empty ⇒ SSO disabled.
    pub issuer: String,
    /// The OAuth **client id** registered with the IdP (also the expected `aud`).
    /// Empty ⇒ SSO disabled.
    pub client_id: String,
    /// The confidential-client **secret**. Sent at the token endpoint via
    /// `client_secret_post` (or `client_secret_basic` when
    /// [`token_auth_basic`](SsoConfig::token_auth_basic)).
    pub client_secret: Secret,
    /// The exact **redirect URL** registered with the IdP. Empty ⇒ derived as
    /// `{[server].base_url}/auth/sso/callback`.
    pub redirect_url: String,
    /// The **browser-facing** origin of this API's `/auth/sso/*` routes (e.g.
    /// `https://catalerum-api.example.com`). The SPA's login button navigates
    /// here to start the OIDC dance — set it when the API is not reachable at
    /// the SPA's derived `api.<spa-host>` origin (e.g. a Kubernetes ingress on
    /// a different domain). Empty ⇒ `[server].base_url`, else the SPA derives
    /// the origin itself.
    pub public_url: String,
    /// Space-separated **scopes**. Empty ⇒ `openid email profile`.
    pub scopes: String,
    /// JIT-provisioning policy: `"disabled"` (default, deny-by-default) or
    /// `"enabled"`. When disabled, an SSO login that matches no existing user is
    /// refused with a friendly 403 — an admin must invite the user first.
    pub jit_provisioning: String,
    /// The organisation **slug** a JIT-provisioned user joins. Empty ⇒ the default
    /// organisation (`default`). Without [`jit_workspace`](Self::jit_workspace) JIT
    /// users get **org** membership only — no workspace until an admin adds them
    /// (fail-closed).
    pub jit_organisation: String,
    /// The org **role** a JIT-provisioned user receives (`owner`|`admin`|`member`).
    /// Empty ⇒ `member`.
    pub jit_org_role: String,
    /// The workspace **slug** an SSO user who belongs to no live workspace is
    /// auto-joined to at login (requires JIT provisioning enabled). This is the
    /// "SSO logins just work" knob: it covers both freshly JIT-provisioned users
    /// and previously provisioned ones an admin never placed. Empty ⇒ off (the
    /// fail-closed default): such logins are refused with the no-workspace error.
    pub jit_workspace: String,
    /// The workspace **role** granted by the auto-join
    /// (`owner`|`admin`|`member`|`viewer`). Empty ⇒ `member`.
    pub jit_workspace_role: String,
    /// Trust the IdP's `email` claim for first-login account linking / JIT even when
    /// `email_verified` is absent/false. **Off by default** — only a verified email
    /// links an SSO identity to an existing local account.
    pub trust_email: bool,
    /// Authenticate at the token endpoint with HTTP Basic (`client_secret_basic`)
    /// instead of the request body (`client_secret_post`, the default).
    pub token_auth_basic: bool,
    /// Secret that signs the short-lived **state cookie** (carrying `state`/`nonce`/
    /// PKCE verifier). Set it (any string) so an in-flight login survives a restart /
    /// spans pods; unset ⇒ a fresh random per-process key (in-flight logins then die
    /// with the process — fine for single-pod dev). Independent of the download /
    /// trigger link secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_secret: Option<Secret>,
    /// Clock-skew leeway (seconds) tolerated on the `id_token` `exp`/`iat`. `0` ⇒ 60s.
    pub leeway_secs: u64,
}

impl SsoConfig {
    /// Whether SSO is configured (issuer + client id set) — else the routes 404.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.issuer.trim().is_empty() && !self.client_id.trim().is_empty()
    }

    /// Whether JIT provisioning is turned on (deny-by-default: only the literal
    /// `"enabled"` opts in).
    #[must_use]
    pub fn jit_enabled(&self) -> bool {
        self.jit_provisioning.trim().eq_ignore_ascii_case("enabled")
    }

    /// The effective scope string (`openid email profile` when unset).
    #[must_use]
    pub fn effective_scopes(&self) -> String {
        let s = self.scopes.trim();
        if s.is_empty() {
            "openid email profile".to_string()
        } else {
            s.to_string()
        }
    }

    /// The advertised full browser-facing `GET /auth/sso/login` URL, when config
    /// pins one: built from the explicit [`public_url`](Self::public_url), else
    /// the server's **explicit** `base_url` (never its `http://<listen>` fallback
    /// — that origin is meaningless to a browser). `None` when neither is set;
    /// the SPA then derives the API origin from its own host (`api.<host>`).
    #[must_use]
    pub fn public_login_url(&self, server_base_url: Option<&str>) -> Option<String> {
        let origin = Some(self.public_url.trim())
            .filter(|s| !s.is_empty())
            .or_else(|| server_base_url.map(str::trim).filter(|s| !s.is_empty()))?;
        Some(format!("{}/auth/sso/login", origin.trim_end_matches('/')))
    }

    /// The effective redirect URL: the explicit value, else
    /// `{api_base_url}/auth/sso/callback`.
    #[must_use]
    pub fn effective_redirect_url(&self, api_base_url: &str) -> String {
        let explicit = self.redirect_url.trim();
        if explicit.is_empty() {
            format!("{}/auth/sso/callback", api_base_url.trim_end_matches('/'))
        } else {
            explicit.to_string()
        }
    }

    /// The org slug JIT users join (`default` when unset).
    #[must_use]
    pub fn jit_org_slug(&self) -> &str {
        let s = self.jit_organisation.trim();
        if s.is_empty() {
            "default"
        } else {
            s
        }
    }

    /// The org role token JIT users receive (`member` when unset).
    #[must_use]
    pub fn jit_org_role_token(&self) -> &str {
        let s = self.jit_org_role.trim();
        if s.is_empty() {
            "member"
        } else {
            s
        }
    }

    /// The workspace slug SSO users without a live workspace are auto-joined to,
    /// or `None` when the knob is unset (the fail-closed default).
    #[must_use]
    pub fn jit_workspace_slug(&self) -> Option<&str> {
        let s = self.jit_workspace.trim();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// The workspace role token the auto-join grants (`member` when unset).
    #[must_use]
    pub fn jit_workspace_role_token(&self) -> &str {
        let s = self.jit_workspace_role.trim();
        if s.is_empty() {
            "member"
        } else {
            s
        }
    }

    /// The effective `id_token` clock-skew leeway in seconds (60 when unset/0).
    #[must_use]
    pub fn leeway(&self) -> u64 {
        if self.leeway_secs == 0 {
            60
        } else {
            self.leeway_secs
        }
    }
}

/// Google OAuth (SOUL §16 M7) — one confidential-client app used to connect a
/// user's Google Calendar via the `/auth/google/*` web flow. The client id +
/// secret are shared across all Google connections in the deployment; each
/// connection's access/refresh tokens are stored **encrypted** per-connection
/// (SOUL §13), not here.
///
/// Empty [`client_id`](GoogleConfig::client_id) ⇒ the whole feature is **off** and
/// the `/auth/google/*` routes return `404`. Secrets ([`client_secret`],
/// [`state_secret`]) use [`Secret`] — set them via env
/// (`CATALERUM_GOOGLE__CLIENT_SECRET=…`), never the committed TOML.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GoogleConfig {
    /// The OAuth **client id** of the catalerum Google app. Empty ⇒ feature off.
    pub client_id: String,
    /// The confidential-client **secret**, sent at the token endpoint.
    pub client_secret: Secret,
    /// The exact **redirect URL** registered with Google. Empty ⇒ derived as
    /// `{[server].base_url}/auth/google/callback`.
    pub redirect_url: String,
    /// Secret that signs the short-lived Google-OAuth **state cookie** (carrying
    /// the CSRF `state` + the workspace/connection the callback attaches tokens
    /// to). Independent of `[sso].state_secret` and the download/trigger link
    /// secrets, so it rotates separately; unset ⇒ a fresh random per-process key
    /// (in-flight connects then die with the process — fine for single-pod dev).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_secret: Option<Secret>,
    /// Opt-in **push notifications** (SOUL §8/§16 M7 push half): when `true`, the
    /// ingest side registers a Google `events.watch` channel per Google-calendar
    /// connection that has a `CollectCalendar` automation, so a calendar change
    /// triggers a collect promptly instead of waiting for the poll cadence. Off by
    /// default because watching requires a **publicly reachable** `[server].base_url`
    /// (Google POSTs to `{base_url}/webhooks/google/calendar`); with no automation on
    /// a connection nothing is watched (the dormant-connection model). The poll
    /// cadence still runs as the correctness fallback — push is a latency optimization.
    #[serde(default)]
    pub push: bool,
    /// Secret that signs the per-channel **push token** (the `X-Goog-Channel-Token`
    /// Google echoes on every notification — the channel's authorization, SOUL §11
    /// house style). Independent of [`state_secret`](Self::state_secret) so it
    /// rotates separately; unset ⇒ a fresh random per-process key (channels created
    /// before a restart then stop verifying — their notifications 404 and fall back
    /// to the poll until the scan renews them; set it for restart/multi-pod
    /// durability, exactly like `[server].trigger_secret`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_secret: Option<Secret>,
}

impl GoogleConfig {
    /// Whether Google OAuth is configured (client id + secret set) — else the
    /// `/auth/google/*` routes `404`.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.client_secret.is_empty()
    }

    /// The effective redirect URL: the explicit value, else
    /// `{api_base_url}/auth/google/callback`.
    #[must_use]
    pub fn effective_redirect_url(&self, api_base_url: &str) -> String {
        let explicit = self.redirect_url.trim();
        if explicit.is_empty() {
            format!(
                "{}/auth/google/callback",
                api_base_url.trim_end_matches('/')
            )
        } else {
            explicit.to_string()
        }
    }
}

/// Microsoft OAuth (SOUL §8) — one confidential-client Entra app used to connect
/// a user's Outlook / Microsoft 365 calendar via the `/auth/microsoft/*` web
/// flow, [`GoogleConfig`]'s exact twin. The client id + secret are shared across
/// all Microsoft connections in the deployment; each connection's access/refresh
/// tokens are stored **encrypted** per-connection (SOUL §13), not here.
///
/// Empty [`client_id`](MicrosoftConfig::client_id) ⇒ the feature is **off** and
/// the `/auth/microsoft/*` routes return `404`. Secrets use [`Secret`] — set
/// them via env (`CATALERUM_MICROSOFT__CLIENT_SECRET=…`), never committed TOML.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MicrosoftConfig {
    /// The OAuth **client (application) id** of the catalerum Entra app. Empty ⇒
    /// feature off.
    pub client_id: String,
    /// The confidential-client **secret**, sent at the token endpoint.
    pub client_secret: Secret,
    /// The Entra **tenant** the app authenticates against: a tenant id/domain
    /// for single-tenant apps, empty ⇒ the multi-tenant `common` endpoint
    /// (work/school + personal accounts).
    pub tenant: String,
    /// The exact **redirect URL** registered with the Entra app. Empty ⇒ derived
    /// as `{[server].base_url}/auth/microsoft/callback`.
    pub redirect_url: String,
    /// Secret that signs the short-lived Microsoft-OAuth **state cookie**
    /// (independent of `[google].state_secret` and `[sso].state_secret`, so it
    /// rotates separately); unset ⇒ a fresh random per-process key (in-flight
    /// connects then die with the process — fine for single-pod dev).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_secret: Option<Secret>,
}

impl MicrosoftConfig {
    /// Whether Microsoft OAuth is configured (client id + secret set) — else the
    /// `/auth/microsoft/*` routes `404`.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.client_secret.is_empty()
    }

    /// The effective redirect URL: the explicit value, else
    /// `{api_base_url}/auth/microsoft/callback`.
    #[must_use]
    pub fn effective_redirect_url(&self, api_base_url: &str) -> String {
        let explicit = self.redirect_url.trim();
        if explicit.is_empty() {
            format!(
                "{}/auth/microsoft/callback",
                api_base_url.trim_end_matches('/')
            )
        } else {
            explicit.to_string()
        }
    }
}
