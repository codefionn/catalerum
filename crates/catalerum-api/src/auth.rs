//! Request authentication (SOUL §18).
//!
//! [`Auth`] is an Axum extractor: it pulls the bearer token from the
//! `Authorization` header (or an `access_token` query parameter, used by the
//! WebSocket handshake where headers are awkward), verifies it through the IAM
//! service, and yields the resolved [`Principal`]. Every protected route takes
//! `Auth` as an argument — auth is enforced on every request by construction.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use catalerum_core::capability::{allows, attenuate, Action, Capability, Resource};
use catalerum_core::model::{Grant, Role};
use catalerum_iam::Principal;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// An authenticated request principal (`{user, workspace, role}`) plus, when the
/// bearer is **grant-scoped** (SOUL §19/§26), the resolved §19 [`Grant`] that is
/// this token's *effective* authority. Obtained by extracting [`Auth`] in a
/// handler signature.
///
/// **Grant-bound tokens are a member-shaped principal with the grant's caps.** A
/// token minted with a named grant (see `routes::tokens`) answers capability
/// questions ([`require`](Self::require), [`capabilities`](Self::capabilities))
/// from the grant's capabilities instead of the role's base set — always a subset
/// of the minting user's authority, never a superset. It additionally **never**
/// passes a workspace-administrator check ([`require_workspace_admin`](Self::require_workspace_admin)),
/// regardless of the minter's role: a scoped token is strictly *less* than the
/// caller. The grant is resolved fresh on every request; a deleted grant fails the
/// token closed (see the extractor).
#[derive(Clone, Debug)]
pub struct Auth {
    principal: Principal,
    /// The resolved grant this token is scoped to, when grant-bound. `None` =
    /// role-derived authority (today's default).
    grant: Option<Grant>,
    /// The raw bearer token the request authenticated with — needed by routes
    /// that act on the session itself (logout revokes exactly this token).
    /// Empty for test-constructed `Auth`s.
    token: String,
}

impl Auth {
    /// Wrap a role-derived principal (no grant scoping) — the login-session shape,
    /// and the construction used by tests.
    #[must_use]
    pub fn from_principal(principal: Principal) -> Self {
        Self {
            principal,
            grant: None,
            token: String::new(),
        }
    }

    /// Wrap a principal bound to an explicit §19 [`Grant`] — the grant-scoped
    /// token shape (also used by tests).
    #[must_use]
    pub fn with_grant(principal: Principal, grant: Grant) -> Self {
        Self {
            principal,
            grant: Some(grant),
            token: String::new(),
        }
    }

    /// The raw bearer token this request authenticated with (empty for
    /// test-constructed `Auth`s). Handle with care — it is a live credential.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The underlying principal (`{user, workspace, role}`). NOTE: the `role` is
    /// the *minting user's* role — authorization must go through
    /// [`require`](Self::require) / [`capabilities`](Self::capabilities), which
    /// answer from the grant when the token is grant-scoped.
    #[must_use]
    pub fn principal(&self) -> Principal {
        self.principal
    }

    /// The grant this token is scoped to, if any (SOUL §19).
    #[must_use]
    pub fn grant(&self) -> Option<&Grant> {
        self.grant.as_ref()
    }

    /// This token's **effective** capability set (SOUL §19): the grant's
    /// capabilities when grant-scoped, else the role's base set. This is what a
    /// tool-dispatch [`ToolContext`](catalerum_core::tool::ToolContext) is built
    /// from (e.g. `POST /mcp`), so an MCP client's tool calls are bounded by the
    /// grant.
    #[must_use]
    pub fn capabilities(&self) -> Vec<Capability> {
        match &self.grant {
            Some(g) => g.capabilities.clone(),
            None => catalerum_iam::base_capabilities(self.principal.role),
        }
    }

    /// Authorize `action` on `domain` for this token's **effective authority**
    /// (SOUL §19) — the capability gate every protected REST handler calls before
    /// touching the store. Deny-by-default: `Ok(())` only if the authority covers
    /// the request, else [`ApiError::Forbidden`] (`403`).
    ///
    /// For a **grant-scoped** token the check runs against the grant's capabilities
    /// (via the same [`allows`] matcher the automation runner uses); for a
    /// role-derived token, against the role's base set. Reads (`Read`) are held by
    /// every role; writes (`Write`) exclude a `Viewer`.
    ///
    /// **On deletion:** the REST handlers that delete a caller's *own* workspace
    /// data (a note, a skill) gate on `Write`, mirroring the `forget` memory tool
    /// — a Member may remove what they may create. The protected `Action::Delete`
    /// (and host-exec) are reserved for §19's destructive scopes — production /
    /// host / external — that no role implies and that need an explicit grant; no
    /// REST handler gates on `Delete` today.
    pub fn require(&self, action: Action, domain: &str) -> ApiResult<()> {
        match &self.grant {
            Some(g) => {
                let requested = Capability::new(action, Resource::domain(domain));
                if allows(g, &requested) {
                    Ok(())
                } else {
                    Err(ApiError::Forbidden(format!(
                        "your grant-scoped token (`{}`) is not permitted to {} the `{domain}` domain",
                        g.name,
                        action_verb(action),
                    )))
                }
            }
            None => authorize(self.principal.role, action, domain),
        }
    }

    /// Require the caller to hold a workspace-**administrator** role (Owner or
    /// Admin), for **workspace-operational config writes** (SOUL §18/§29):
    /// registering / removing the external DB + storage connections a whole
    /// workspace's tools then use. Deny-by-default — a Member or Viewer is
    /// [`ApiError::Forbidden`] (`403`).
    ///
    /// A **grant-scoped** token never passes this check, whatever the minting
    /// user's role: it acts as a capability-bounded member, and workspace-admin is
    /// a role-derived, non-capability privilege a scoped token must not carry
    /// (SOUL §19 — a scoped token is strictly *less* than its minter).
    ///
    /// This is enforced by **workspace role**, never by the deployment `mode`
    /// (which shapes only how much the web shows, SOUL §29): a Member is denied
    /// these writes in `single_user` and `multi_user` alike. It layers *on top
    /// of* the per-domain [`require`](Self::require) capability gate — both must
    /// pass — mirroring the org routes' `require_org_admin` idiom.
    pub fn require_workspace_admin(&self) -> ApiResult<()> {
        if self.grant.is_some() {
            return Err(ApiError::Forbidden(
                "a grant-scoped token acts as a capability-bounded member and cannot \
                 perform workspace-administrator operations"
                    .to_string(),
            ));
        }
        require_admin_role(self.principal.role)
    }
}

/// Deny-by-default workspace-admin check, split out from
/// [`Auth::require_workspace_admin`] so it is unit-testable without an
/// IAM-issued [`Principal`] (mirrors [`authorize`]).
fn require_admin_role(role: Role) -> ApiResult<()> {
    if catalerum_iam::is_admin(role) {
        Ok(())
    } else {
        Err(ApiError::Forbidden(format!(
            "workspace administrator (owner/admin) required; your role is {}",
            catalerum_iam::role_str(role),
        )))
    }
}

/// Deny-by-default role → capability check (SOUL §19), shared by every REST
/// surface. Split out from [`Auth::require`] so it is unit-testable without an
/// IAM-issued [`Principal`].
fn authorize(role: Role, action: Action, domain: &str) -> ApiResult<()> {
    if catalerum_iam::role_allows(role, &Capability::new(action, Resource::domain(domain))) {
        Ok(())
    } else {
        Err(ApiError::Forbidden(format!(
            "your role ({}) is not permitted to {} the `{domain}` domain",
            catalerum_iam::role_str(role),
            action_verb(action),
        )))
    }
}

/// A human verb for an [`Action`], for the `403` message.
fn action_verb(action: Action) -> &'static str {
    match action {
        Action::Read | Action::Query | Action::Search => "read",
        Action::Write => "modify",
        Action::Delete => "delete",
        Action::Use => "use",
        Action::Run => "run",
        Action::Expose => "expose",
        Action::Any => "fully access",
    }
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_from_parts(parts)
            .ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;
        let principal = state.iam().verify_bearer(&token).await?;
        // A grant-scoped token (SOUL §19/§26): resolve its grant fresh on every
        // request and answer capability questions from it instead of the role.
        // Fail **closed** if the grant is gone — never fall back to the (wider)
        // role authority, mirroring the automation-run recorded-grant precedent.
        // (The store's composite FK also cascade-revokes such a token when its
        // grant is deleted; this covers the in-memory store and races.)
        let grant = match principal.grant_id {
            Some(gid) => Some(
                state
                    .store()
                    .grants()
                    .get(principal.workspace_id, gid)
                    .await
                    .map_err(|_| {
                        ApiError::unauthorized(
                            "this token's grant no longer exists; it has been revoked",
                        )
                    })?,
            ),
            None => None,
        };
        Ok(Auth {
            principal,
            grant,
            token,
        })
    }
}

/// The §19 mint-gate: is every capability a grant confers within the caller's
/// own effective authority (`ceiling`)? A scoped token / grant binding must be
/// **⊆ its minter** — a grant that widens the caller is rejected (SOUL §18/§19).
/// Shared by `routes::tokens`, `routes::mcp_endpoints`, and anywhere else a
/// caller pins a grant to a credential.
pub(crate) fn grant_within_authority(ceiling: &[Capability], grant: &Grant) -> bool {
    grant
        .capabilities
        .iter()
        .all(|cap| attenuate(ceiling, cap).is_ok())
}

/// Path prefixes where a bearer may ride in the **query string** — only the
/// routes a browser cannot attach an `Authorization` header to: WebSocket
/// upgrade handshakes (`/ws/*`, the terminal output socket) and media the SPA
/// embeds via `<img>`/`<a>`/`<audio>` URLs (`/storage/objects/*`, emerged-UI
/// images). Everywhere else the query fallback is refused, so a leaked URL
/// (browser history, referer, logs) on an ordinary API route cannot
/// authenticate. Query auth is additionally restricted to `GET` — all of these
/// surfaces are reads / WS handshakes.
const QUERY_TOKEN_PATH_PREFIXES: &[&str] = &["/ws/", "/terminals/", "/storage/objects/", "/uis/"];

/// Whether this request may authenticate via a query-param bearer (see
/// [`QUERY_TOKEN_PATH_PREFIXES`]).
fn query_token_allowed(parts: &Parts) -> bool {
    parts.method == axum::http::Method::GET
        && QUERY_TOKEN_PATH_PREFIXES
            .iter()
            .any(|prefix| parts.uri.path().starts_with(prefix))
}

/// Extract a bearer token from request parts: prefer the `Authorization` header
/// (`Bearer <t>` or a raw token), else — only on the browser-media / WebSocket
/// routes ([`QUERY_TOKEN_PATH_PREFIXES`]) — fall back to an `access_token` or
/// `token` query param.
pub(crate) fn bearer_from_parts(parts: &Parts) -> Option<String> {
    if let Some(value) = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let trimmed = value.trim();
        let token = trimmed
            .strip_prefix("Bearer ")
            .or_else(|| trimmed.strip_prefix("bearer "))
            .unwrap_or(trimmed);
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }

    if !query_token_allowed(parts) {
        return None;
    }
    parts
        .uri
        .query()
        .and_then(|q| url_query_value(q, "access_token").or_else(|| url_query_value(q, "token")))
}

/// Minimal `application/x-www-form-urlencoded` query parser for a single key.
/// Handles `+`→space and `%XX` percent-decoding.
fn url_query_value(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        if k == key {
            let raw = it.next().unwrap_or("");
            return Some(percent_decode(raw));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push(hi << 4 | lo);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_value_extracts_key() {
        assert_eq!(
            url_query_value("foo=1&access_token=abc&bar=2", "access_token"),
            Some("abc".to_string())
        );
        assert_eq!(url_query_value("foo=1", "access_token"), None);
    }

    #[test]
    fn query_value_percent_decodes() {
        assert_eq!(
            url_query_value("token=a%2Bb%20c", "token"),
            Some("a+b c".to_string())
        );
        assert_eq!(
            url_query_value("token=plus+space", "token"),
            Some("plus space".to_string())
        );
    }

    #[test]
    fn bearer_from_header_strips_prefix() {
        let parts = parts_with_header("Bearer tok123");
        assert_eq!(bearer_from_parts(&parts), Some("tok123".to_string()));

        let parts = parts_with_header("rawtoken");
        assert_eq!(bearer_from_parts(&parts), Some("rawtoken".to_string()));
    }

    #[test]
    fn bearer_falls_back_to_query() {
        let parts = parts_with_uri("/ws/chat?access_token=qtok");
        assert_eq!(bearer_from_parts(&parts), Some("qtok".to_string()));
    }

    #[test]
    fn bearer_query_fallback_is_restricted_to_browser_media_routes() {
        // Allowed: WS handshakes + browser-embedded media, GET only.
        for uri in [
            "/ws/chat?token=t1",
            "/ws/speech?access_token=t2",
            "/terminals/sessions/abc/output?token=t3",
            "/storage/objects/docs/a.txt?token=t4",
            "/uis/123/image/node?token=t5",
        ] {
            assert!(
                bearer_from_parts(&parts_with_uri(uri)).is_some(),
                "query token should authenticate on {uri}"
            );
        }
        // Refused: ordinary API routes — a token in such a URL is a leak, not a
        // credential.
        for uri in [
            "/notes?token=leak",
            "/auth/magic?token=leak",
            "/mcp?access_token=leak",
            "/storage?token=leak",
            "/tokens?token=leak",
            "/?token=leak",
        ] {
            assert_eq!(
                bearer_from_parts(&parts_with_uri(uri)),
                None,
                "query token must be refused on {uri}"
            );
        }
        // Refused: non-GET even on an allowed prefix.
        let req = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri("/storage/objects/a.txt?token=leak")
            .body(())
            .unwrap();
        assert_eq!(bearer_from_parts(&req.into_parts().0), None);
    }

    #[test]
    fn authorize_is_deny_by_default_per_role() {
        // Reads are held by every role.
        for r in [Role::Owner, Role::Admin, Role::Member, Role::Viewer] {
            assert!(
                authorize(r, Action::Read, "notes").is_ok(),
                "{r:?} may read"
            );
        }
        // Writes: Owner/Admin/Member yes; a Viewer is forbidden (deny-by-default §19).
        assert!(authorize(Role::Member, Action::Write, "skill").is_ok());
        assert!(authorize(Role::Owner, Action::Write, "calendar").is_ok());
        assert!(matches!(
            authorize(Role::Viewer, Action::Write, "notes"),
            Err(ApiError::Forbidden(_))
        ));
        // The gate denies role-implied `Delete` (a §19 protected scope). No REST
        // handler gates on it — workspace-data deletes use `Write` (see `require`).
        assert!(matches!(
            authorize(Role::Member, Action::Delete, "notes"),
            Err(ApiError::Forbidden(_))
        ));
    }

    #[test]
    fn require_admin_role_is_owner_or_admin_only() {
        // The gate every workspace-operational config write (register/remove an
        // external DB or storage connection) calls first: Owner/Admin pass, a
        // Member and a Viewer are `403` — regardless of the deployment mode, which
        // this check never consults (SOUL §18/§29, deny-by-default).
        assert!(require_admin_role(Role::Owner).is_ok());
        assert!(require_admin_role(Role::Admin).is_ok());
        assert!(matches!(
            require_admin_role(Role::Member),
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            require_admin_role(Role::Viewer),
            Err(ApiError::Forbidden(_))
        ));
    }

    #[test]
    fn require_workspace_admin_matches_the_role_gate() {
        // The public `Auth` method delegates to `require_admin_role`, so an
        // Owner-scoped principal passes and a Member-scoped one is denied — the
        // exact gate the newly-restricted connection routes invoke.
        use catalerum_core::{UserId, WorkspaceId};
        let admin = Auth::from_principal(catalerum_iam::Principal::new(
            UserId::new(),
            WorkspaceId::new(),
            Role::Owner,
        ));
        let member = Auth::from_principal(catalerum_iam::Principal::new(
            UserId::new(),
            WorkspaceId::new(),
            Role::Member,
        ));
        assert!(admin.require_workspace_admin().is_ok());
        assert!(matches!(
            member.require_workspace_admin(),
            Err(ApiError::Forbidden(_))
        ));
    }

    #[test]
    fn grant_scoped_token_answers_from_the_grant_not_the_role() {
        // A grant-bound token minted by an Owner (full `*`) carries ONLY the
        // grant's capabilities — `notes:read`/`notes:write` here. It can read/write
        // notes, but is denied every domain the grant omits (calendar), and — the
        // key invariant — it NEVER passes the workspace-admin check even though the
        // minting principal is an Owner. A scoped token is strictly less than its
        // minter (SOUL §19).
        use catalerum_core::{GrantId, UserId, WorkspaceId};
        let ws = WorkspaceId::new();
        let grant = Grant {
            id: GrantId::new(),
            workspace_id: ws,
            name: "notes-only".into(),
            capabilities: vec![
                Capability::new(Action::Read, Resource::domain("notes")),
                Capability::new(Action::Write, Resource::domain("notes")),
            ],
            constraints: Default::default(),
        };
        let mut p = catalerum_iam::Principal::new(UserId::new(), ws, Role::Owner);
        p.grant_id = Some(grant.id);
        let auth = Auth::with_grant(p, grant);

        assert!(auth.require(Action::Read, "notes").is_ok());
        assert!(auth.require(Action::Write, "notes").is_ok());
        // Omitted domain → forbidden, despite the Owner role behind the token.
        assert!(matches!(
            auth.require(Action::Write, "calendar"),
            Err(ApiError::Forbidden(_))
        ));
        // Never an admin, whatever the role.
        assert!(matches!(
            auth.require_workspace_admin(),
            Err(ApiError::Forbidden(_))
        ));
        // The effective capability set is the grant's, not the role's `*`.
        assert_eq!(auth.capabilities().len(), 2);
    }

    fn parts_with_header(value: &str) -> Parts {
        let req = axum::http::Request::builder()
            .uri("/")
            .header(axum::http::header::AUTHORIZATION, value)
            .body(())
            .unwrap();
        req.into_parts().0
    }

    fn parts_with_uri(uri: &str) -> Parts {
        let req = axum::http::Request::builder().uri(uri).body(()).unwrap();
        req.into_parts().0
    }
}
