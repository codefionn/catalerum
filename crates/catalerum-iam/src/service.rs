//! The IAM service — the orchestration layer over an [`IamStore`] (SOUL §18).
//!
//! [`IamService`] is the single entry point the API/binary calls into:
//! workspace/user/membership creation, session issue + verify, the dev
//! magic-link seed + redeem flow, and the bearer-token auth verifier that turns
//! a token into an authenticated [`Principal`].

use chrono::{Duration, Utc};

use catalerum_core::model::{Membership, Role, Subject, User, Workspace};
use catalerum_core::{GrantId, OrganisationId, UserId, WorkspaceId};

use crate::magic::MagicLink;
use crate::session::{Principal, Session, DEFAULT_LOGIN_TOKEN_TTL, DEFAULT_SESSION_TTL};
use crate::sso::SsoIdentity;
use crate::store::{IamStore, LoginToken};
use crate::{token, Error, Result};

/// Default base URL for dev magic-link generation (matches `just dev`, SOUL §17).
pub const DEFAULT_BASE_URL: &str = "http://localhost:8787";

/// Default workspace name + slug seeded on first run (SOUL §18).
pub const DEFAULT_WORKSPACE_NAME: &str = "Default";
/// Default workspace slug.
pub const DEFAULT_WORKSPACE_SLUG: &str = "default";
/// The well-known **default organisation** seeded by the `0046` migration
/// (SOUL §17/§18). A fixed id so the dev seed + [`create_workspace`] attach the
/// default workspace to it without a lookup — this literal must stay in sync with
/// the `organisations` seed in migration `0046`.
pub const DEFAULT_ORGANISATION_ID: OrganisationId = OrganisationId::from_uuid(
    uuid::Uuid::from_u128(0xdef0_0000_0000_4000_8000_0000_0000_0000),
);
/// Default organisation name.
pub const DEFAULT_ORGANISATION_NAME: &str = "Default";
/// Default organisation slug.
pub const DEFAULT_ORGANISATION_SLUG: &str = "default";
/// Default seeded admin email.
pub const DEFAULT_ADMIN_EMAIL: &str = "admin@localhost";
/// Default seeded admin display name.
pub const DEFAULT_ADMIN_NAME: &str = "Admin";

/// The outcome of resolving a verified [`SsoIdentity`] to a local user
/// ([`IamService::resolve_sso_identity`], SOUL §18). The callback maps this onto a
/// session (for a resolved user) or a friendly error (for a deny).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SsoResolution {
    /// Matched an **existing** user — by SSO subject, or by first-login email
    /// linking (the subject was bound onto the matched account). Proceed to a
    /// normal session into their existing workspace membership.
    Existing(User),
    /// **JIT-provisioned** a brand-new user. The caller must still assign the
    /// configured organisation membership; the user has no workspace yet.
    Provisioned(User),
    /// Login verified but no account results, for a reason the caller renders as a
    /// friendly 403/400 (never a silent guess).
    Denied(SsoDenyReason),
}

/// Why an otherwise-valid SSO login did not resolve to a usable account (SOUL §18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SsoDenyReason {
    /// No subject/email match and JIT provisioning is **disabled** (the deny-by-
    /// default posture) — an admin must invite the user first.
    ProvisioningDisabled,
    /// The identity carried no email the IdP verified (and `trust_email` is off), so
    /// it can neither link to nor create an account keyed by an email.
    NoVerifiedEmail,
    /// The verified email matches a local account already bound to a **different**
    /// SSO subject — refuse rather than re-point an identity (fail closed).
    EmailAlreadyLinked,
}

/// The IAM service. Generic over the [`IamStore`] backing it so the same logic
/// runs against Postgres ([`PgIamStore`](crate::PgIamStore)) in production and
/// the in-memory store in tests.
#[derive(Clone)]
pub struct IamService<S: IamStore> {
    store: S,
    /// Base URL used to render magic-link login URLs (SOUL §17/§18).
    base_url: String,
    session_ttl: Duration,
    login_ttl: Duration,
}

impl<S: IamStore> IamService<S> {
    /// Construct a service over `store` with default base URL + TTLs.
    pub fn new(store: S) -> Self {
        Self {
            store,
            base_url: DEFAULT_BASE_URL.to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            login_ttl: DEFAULT_LOGIN_TOKEN_TTL,
        }
    }

    /// Override the base URL used for magic-link generation (e.g. the public
    /// origin). Trailing slashes are trimmed.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    /// Override the session token lifetime.
    #[must_use]
    pub fn with_session_ttl(mut self, ttl: Duration) -> Self {
        self.session_ttl = ttl;
        self
    }

    /// Override the one-time login token lifetime.
    #[must_use]
    pub fn with_login_ttl(mut self, ttl: Duration) -> Self {
        self.login_ttl = ttl;
        self
    }

    /// Borrow the backing store (for advanced callers / tests).
    pub fn store(&self) -> &S {
        &self.store
    }

    // -----------------------------------------------------------------------
    // Workspace / user / membership creation (SOUL §18)
    // -----------------------------------------------------------------------

    /// Create a workspace with a fresh id.
    ///
    /// # Errors
    /// [`Error::Conflict`] if the slug is already taken.
    pub async fn create_workspace(
        &self,
        name: impl Into<String>,
        slug: impl Into<String>,
    ) -> Result<Workspace> {
        let ws = Workspace {
            id: WorkspaceId::new(),
            // The dev/seed path creates workspaces in the default organisation
            // (SOUL §18). The Postgres store resolves the default org itself (the
            // input `organisation_id` is advisory — the store owns the persisted
            // row); the in-memory store honors it directly. Additional workspaces in
            // other orgs go through the policy-gated org routes, not this helper.
            organisation_id: DEFAULT_ORGANISATION_ID,
            name: name.into(),
            slug: slug.into(),
            archived_at: None,
        };
        // The store is the id authority: adopt the persisted row so the id we
        // return (and use for membership/session FKs) matches the DB.
        self.store.create_workspace(&ws).await
    }

    /// Create a user with a fresh id. `sso_subject` is `None` for local/dev
    /// users (SSO is M7, see [`Self::create_sso_user`]).
    ///
    /// # Errors
    /// [`Error::Conflict`] if the email is already taken.
    pub async fn create_user(
        &self,
        email: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Result<User> {
        let user = User {
            id: UserId::new(),
            email: email.into(),
            display_name: display_name.into(),
            sso_subject: None,
        };
        // Adopt the store-assigned row (see `create_workspace`).
        self.store.create_user(&user).await
    }

    /// Create an SSO-backed user (subject = `iss`/`sub`).
    ///
    /// TODO(M7 — SSO): this is the JIT-provisioning seam. The OIDC/SAML
    /// callback maps a verified [`Subject`] to a user here, then assigns
    /// membership from claims/groups (SOUL §18). Currently a plain insert.
    pub async fn create_sso_user(
        &self,
        email: impl Into<String>,
        display_name: impl Into<String>,
        subject: Subject,
    ) -> Result<User> {
        let user = User {
            id: UserId::new(),
            email: email.into(),
            display_name: display_name.into(),
            sso_subject: Some(subject),
        };
        // Adopt the store-assigned row (see `create_workspace`).
        self.store.create_user(&user).await
    }

    /// Resolve a verified [`SsoIdentity`] to a local user, applying the fixed match
    /// order (SOUL §18/§29):
    ///
    /// 1. **Bound subject** — a user already carrying this `(iss, sub)` → that user.
    ///    Never create a second user for a known subject.
    /// 2. **First-login email linking** — a **verified** email (`email_verified`, or
    ///    any email when the operator sets `trust_email`) that matches an existing
    ///    account **not yet bound to another subject** → bind the subject onto it.
    ///    A match already bound to a *different* subject is refused
    ///    ([`SsoDenyReason::EmailAlreadyLinked`]) — we never re-point an identity.
    /// 3. **JIT provisioning** — when `jit_enabled`, create a fresh user keyed by the
    ///    verified email (deny-by-default: disabled → [`SsoDenyReason::ProvisioningDisabled`]).
    ///    Without a verified email there is nothing safe to key on
    ///    ([`SsoDenyReason::NoVerifiedEmail`]).
    ///
    /// The organisation/workspace membership a JIT user receives is the caller's
    /// job (this layer owns no org store); a [`SsoResolution::Provisioned`] user has
    /// **no** workspace until one is assigned.
    ///
    /// # Errors
    /// Propagates store failures; a [`Error::Conflict`] from a racing bind surfaces
    /// as-is.
    pub async fn resolve_sso_identity(
        &self,
        identity: &SsoIdentity,
        jit_enabled: bool,
        trust_email: bool,
    ) -> Result<SsoResolution> {
        // (1) Bound subject wins, always.
        if let Some(user) = self.store.get_user_by_sso(&identity.subject).await? {
            return Ok(SsoResolution::Existing(user));
        }

        // The email is only *usable* (for linking or provisioning) when the IdP
        // verified it, or the operator has explicitly opted to trust this IdP's
        // emails. An unverified email never establishes or adopts an account.
        let usable_email = identity
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|_| identity.email_verified || trust_email);

        // (2) First-login email linking.
        if let Some(email) = usable_email {
            if let Some(existing) = self.store.get_user_by_email_ci(email).await? {
                // The matched account is already bound to some *other* subject (step 1
                // didn't match, so it can't be ours) → refuse rather than re-point it.
                if existing.sso_subject.is_some() {
                    return Ok(SsoResolution::Denied(SsoDenyReason::EmailAlreadyLinked));
                }
                let bound = self
                    .store
                    .bind_sso_subject(existing.id, &identity.subject)
                    .await?;
                return Ok(SsoResolution::Existing(bound));
            }
        }

        // (3) JIT provisioning (deny-by-default).
        if !jit_enabled {
            return Ok(SsoResolution::Denied(SsoDenyReason::ProvisioningDisabled));
        }
        let Some(email) = usable_email else {
            return Ok(SsoResolution::Denied(SsoDenyReason::NoVerifiedEmail));
        };
        let display = identity
            .display_name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| email.to_string());
        let user = self
            .create_sso_user(email, display, identity.subject.clone())
            .await?;
        Ok(SsoResolution::Provisioned(user))
    }

    /// Add (or update) a user's membership + role in a workspace.
    pub async fn add_membership(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        role: Role,
    ) -> Result<Membership> {
        let m = Membership {
            workspace_id,
            user_id,
            role,
        };
        self.store.upsert_membership(&m).await?;
        Ok(m)
    }

    /// Fetch a workspace by id.
    pub async fn get_workspace(&self, id: WorkspaceId) -> Result<Option<Workspace>> {
        self.store.get_workspace(id).await
    }

    /// Fetch a user by id.
    pub async fn get_user(&self, id: UserId) -> Result<Option<User>> {
        self.store.get_user(id).await
    }

    /// Resolve a user's membership (role) in a workspace.
    pub async fn membership(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<Option<Membership>> {
        self.store.get_membership(workspace_id, user_id).await
    }

    // -----------------------------------------------------------------------
    // Sessions: issue + verify (SOUL §18)
    // -----------------------------------------------------------------------

    /// Issue a session for `{user, workspace}`, resolving the role from the
    /// user's membership. The returned [`Session`] carries the opaque token.
    ///
    /// # Errors
    /// - [`Error::Forbidden`] if the target workspace is archived (SOUL §18).
    /// - [`Error::Unauthorized`] if the user is not a member of the workspace.
    pub async fn issue_session(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<Session> {
        // Fail closed: never establish a login session into an **archived**
        // workspace (SOUL §18). This is the shared session-issue chokepoint for
        // the membership-gated login paths — magic-link redeem (`GET /auth/magic`)
        // and `POST /auth/switch` — so ALL of them reject an archived target, not
        // just `/auth/switch` (which also pre-checks). `get_workspace` returns
        // archived rows by design (restore/admin resolve them), so we test the
        // flag explicitly; a vanished workspace falls through to the membership
        // check below (non-member → Unauthorized).
        if let Some(ws) = self.store.get_workspace(workspace_id).await? {
            if ws.archived_at.is_some() {
                return Err(Error::forbidden(
                    "this workspace is archived and cannot be logged into; an \
                     organisation admin must restore it first",
                ));
            }
        }
        let membership = self
            .store
            .get_membership(workspace_id, user_id)
            .await?
            .ok_or_else(|| Error::unauthorized("user is not a member of this workspace"))?;
        self.issue_session_with_role(workspace_id, user_id, membership.role)
            .await
    }

    /// Issue a session with an explicit role (skips the membership lookup; the
    /// caller asserts the role), using the default session TTL.
    pub async fn issue_session_with_role(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        role: Role,
    ) -> Result<Session> {
        // A login session is always role-derived (no grant scoping).
        self.issue_session_with_ttl(workspace_id, user_id, role, None, self.session_ttl)
            .await
    }

    /// Issue a session with an explicit role **and TTL** — the basis for a
    /// long-lived, workspace-bound **service token** for scripts / CI / MCP clients
    /// (SOUL §18). The returned [`Session`] carries the raw bearer; it is
    /// `verify_bearer`-able and `revoke_session`-able like any session.
    ///
    /// `grant_id` **scopes** the token to a named §19 grant (SOUL §19/§26): the
    /// bearer's effective authority becomes that grant's capabilities instead of the
    /// role's base set. The caller is responsible for the attenuation gate (a grant
    /// must be ⊆ the minting user's authority) — this method only records the
    /// reference; enforcement + fail-closed-on-delete happen at auth time.
    pub async fn issue_session_with_ttl(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        role: Role,
        grant_id: Option<GrantId>,
        ttl: chrono::Duration,
    ) -> Result<Session> {
        let now = Utc::now();
        let session = Session {
            token: token::generate(),
            user_id,
            workspace_id,
            role,
            grant_id,
            created_at: now,
            expires_at: now + ttl,
        };
        self.store.create_session(&session).await?;
        Ok(session)
    }

    /// [`issue_session_with_ttl`](Self::issue_session_with_ttl) with the TTL given
    /// in **days** (clamped to ≥ 1) — the entry point a `mint a service token` CLI
    /// uses without pulling in `chrono`. `grant_id` scopes the token to a named §19
    /// grant (see [`issue_session_with_ttl`](Self::issue_session_with_ttl)).
    pub async fn issue_session_with_ttl_days(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        role: Role,
        grant_id: Option<GrantId>,
        days: i64,
    ) -> Result<Session> {
        self.issue_session_with_ttl(
            workspace_id,
            user_id,
            role,
            grant_id,
            chrono::Duration::days(days.max(1)),
        )
        .await
    }

    /// Ensure a caller-supplied dev bearer exists for the default workspace
    /// owner. Used by `just dev` to keep the API authorization token stable
    /// across cargo-watch restarts while still going through normal session
    /// verification.
    pub async fn ensure_dev_authorization_token(
        &self,
        token: &str,
        ttl: Duration,
    ) -> Result<Session> {
        let token = token.trim();
        if token.is_empty() {
            return Err(Error::invalid("dev authorization token is empty"));
        }

        let (ws, admin, role) = self.ensure_dev_owner().await?;
        self.ensure_session_with_token(ws.id, admin.id, role, token, ttl)
            .await
    }

    /// [`ensure_dev_authorization_token`](Self::ensure_dev_authorization_token)
    /// with a TTL in days, clamped to at least one day.
    pub async fn ensure_dev_authorization_token_days(
        &self,
        token: &str,
        days: i64,
    ) -> Result<Session> {
        self.ensure_dev_authorization_token(token, Duration::days(days.max(1)))
            .await
    }

    /// Verify a session token: resolve it to a live [`Session`], checking
    /// existence and expiry.
    ///
    /// # Errors
    /// [`Error::Unauthorized`] if the token is unknown or expired.
    pub async fn verify_session(&self, token: &str) -> Result<Session> {
        let session = self
            .store
            .get_session(token)
            .await?
            .ok_or_else(|| Error::unauthorized("unknown session token"))?;
        if session.is_expired() {
            // Best-effort cleanup; ignore the delete result.
            let _ = self.store.delete_session(token).await;
            return Err(Error::unauthorized("session expired"));
        }
        Ok(session)
    }

    /// Verify a bearer token and return the authenticated [`Principal`] — the
    /// `{user, workspace, role}` the API enforces capabilities against
    /// (SOUL §18/§19). This is the **auth verifier** the API calls per request.
    ///
    /// Accepts either a raw token or one with a leading `Bearer ` prefix.
    ///
    /// # Errors
    /// [`Error::Unauthorized`] if the token is missing, unknown, or expired.
    ///
    /// TODO(M7): also accept SSO-issued and long-lived service tokens here
    /// (SOUL §18). Today only opaque session tokens are recognized.
    pub async fn verify_bearer(&self, bearer: &str) -> Result<Principal> {
        let token = strip_bearer(bearer);
        if token.is_empty() {
            return Err(Error::unauthorized("missing bearer token"));
        }
        Ok(self.verify_session(token).await?.principal())
    }

    /// Revoke a session (logout). Returns whether a session was removed.
    pub async fn revoke_session(&self, token: &str) -> Result<bool> {
        self.store.delete_session(strip_bearer(token)).await
    }

    /// Purge expired sessions; returns the count removed. Safe to call on a
    /// timer.
    pub async fn purge_expired_sessions(&self) -> Result<u64> {
        self.store.purge_expired_sessions(Utc::now()).await
    }

    // -----------------------------------------------------------------------
    // Dev magic-link login (SOUL §17/§18)
    // -----------------------------------------------------------------------

    /// Mint a one-time login token for `{user, workspace}` and render a
    /// magic-link login URL (SOUL §18). The token is stored with an expiry and
    /// is redeemable exactly once via [`Self::redeem_login_token`].
    pub async fn issue_login_token(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<MagicLink> {
        self.issue_login_token_with_ttl(workspace_id, user_id, self.login_ttl)
            .await
    }

    /// [`issue_login_token`](Self::issue_login_token) with an explicit TTL — the
    /// basis for the short-lived browser **handoff code** (SOUL §18): the API
    /// redirects into the SPA with `?code=…` and the SPA exchanges it for the
    /// real session, so the session bearer never appears in a URL.
    pub async fn issue_login_token_with_ttl(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        ttl: Duration,
    ) -> Result<MagicLink> {
        let now = Utc::now();
        let row = LoginToken {
            token: token::generate(),
            user_id,
            workspace_id,
            created_at: now,
            expires_at: now + ttl,
            consumed_at: None,
        };
        self.store.create_login_token(&row).await?;
        let url = MagicLink::render_url(&self.base_url, &row.token);
        Ok(MagicLink {
            token: row.token,
            user_id,
            workspace_id,
            expires_at: row.expires_at,
            url,
        })
    }

    /// Consume a one-time login token (idempotent single-use) **without**
    /// issuing a session — the first half of the browser handoff flow, where the
    /// caller mints a fresh short-lived handoff code from the binding instead of
    /// a session. [`Self::redeem_login_token`] is this plus session issuance.
    ///
    /// # Errors
    /// - [`Error::NotFound`] if the token is unknown.
    /// - [`Error::TokenConsumed`] if already redeemed.
    /// - [`Error::Unauthorized`] if expired.
    pub async fn consume_login_token(&self, token: &str) -> Result<LoginToken> {
        let now = Utc::now();
        let row = self.store.consume_login_token(token, now).await?;
        if row.expires_at <= now {
            return Err(Error::unauthorized("login token expired"));
        }
        Ok(row)
    }

    /// Redeem a one-time login token: consume it (idempotent single-use),
    /// resolve the user's role, and issue a fresh [`Session`]. This is the
    /// handler behind `GET /auth/magic?token=…` and `POST /auth/exchange`.
    ///
    /// # Errors
    /// - [`Error::NotFound`] if the token is unknown.
    /// - [`Error::TokenConsumed`] if already redeemed.
    /// - [`Error::Unauthorized`] if expired or the user lost membership.
    pub async fn redeem_login_token(&self, token: &str) -> Result<Session> {
        let row = self.consume_login_token(token).await?;
        self.issue_session(row.workspace_id, row.user_id).await
    }

    /// Ensure the dev defaults exist (first run / seed, SOUL §17/§18): a default
    /// workspace + an admin user (Owner) + a one-time login token, returning the
    /// magic-link login URL. Idempotent — re-running reuses the existing
    /// workspace/admin and mints a **fresh** login token.
    ///
    /// This is the zero-config login path `just dev` prints to the console.
    ///
    /// TODO(M7): auto-disable once real auth/SSO is configured (SOUL §18).
    pub async fn ensure_dev_login(&self) -> Result<MagicLink> {
        let (ws, admin, _) = self.ensure_dev_owner().await?;
        // Fresh one-time login token + URL.
        self.issue_login_token(ws.id, admin.id).await
    }

    async fn ensure_dev_owner(&self) -> Result<(Workspace, User, Role)> {
        // Workspace.
        let ws = match self
            .store
            .get_workspace_by_slug(DEFAULT_WORKSPACE_SLUG)
            .await?
        {
            Some(ws) => ws,
            None => {
                self.create_workspace(DEFAULT_WORKSPACE_NAME, DEFAULT_WORKSPACE_SLUG)
                    .await?
            }
        };

        // Admin user.
        let admin = match self.store.get_user_by_email(DEFAULT_ADMIN_EMAIL).await? {
            Some(u) => u,
            None => {
                self.create_user(DEFAULT_ADMIN_EMAIL, DEFAULT_ADMIN_NAME)
                    .await?
            }
        };

        // Membership (Owner) — insert is idempotent; keep any existing role.
        let role = match self.store.get_membership(ws.id, admin.id).await? {
            Some(m) => m.role,
            None => {
                self.add_membership(ws.id, admin.id, Role::Owner).await?;
                Role::Owner
            }
        };

        Ok((ws, admin, role))
    }

    async fn ensure_session_with_token(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        role: Role,
        token: &str,
        ttl: Duration,
    ) -> Result<Session> {
        let now = Utc::now();
        let _ = self.store.purge_expired_sessions(now).await?;

        if let Some(existing) = self.store.get_session(token).await? {
            if existing.workspace_id == workspace_id && existing.user_id == user_id {
                return Ok(existing);
            }
            return Err(Error::conflict(
                "dev authorization token is already bound to another principal",
            ));
        }

        let session = Session {
            token: token.to_string(),
            user_id,
            workspace_id,
            role,
            // The stable dev authorization token is role-derived (never grant-scoped).
            grant_id: None,
            created_at: now,
            expires_at: now + ttl,
        };
        self.store.create_session(&session).await?;
        Ok(session)
    }
}

/// Strip an optional `Bearer ` (case-insensitive) prefix and surrounding
/// whitespace from an authorization value.
fn strip_bearer(value: &str) -> &str {
    let v = value.trim();
    if v.len() >= 7 && v[..7].eq_ignore_ascii_case("bearer ") {
        v[7..].trim()
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryIamStore;

    fn svc() -> IamService<MemoryIamStore> {
        IamService::new(MemoryIamStore::new())
    }

    #[tokio::test]
    async fn create_and_member_and_session_roundtrip() {
        let s = svc();
        let ws = s.create_workspace("Work", "work").await.unwrap();
        let u = s.create_user("a@b.c", "Alice").await.unwrap();
        s.add_membership(ws.id, u.id, Role::Member).await.unwrap();

        let session = s.issue_session(ws.id, u.id).await.unwrap();
        let principal = s.verify_bearer(&session.token).await.unwrap();
        assert_eq!(principal.user_id, u.id);
        assert_eq!(principal.workspace_id, ws.id);
        assert_eq!(principal.role, Role::Member);

        // Bearer prefix tolerated.
        let p2 = s
            .verify_bearer(&format!("Bearer {}", session.token))
            .await
            .unwrap();
        assert_eq!(p2, principal);
    }

    #[tokio::test]
    async fn unknown_token_is_unauthorized() {
        let s = svc();
        let err = s.verify_bearer("nope").await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[tokio::test]
    async fn service_token_honors_long_ttl_and_revocation() {
        let s = svc();
        let ws = s.create_workspace("Work", "work").await.unwrap();
        let u = s.create_user("svc@b.c", "Svc").await.unwrap();
        s.add_membership(ws.id, u.id, Role::Member).await.unwrap();

        // A long-lived service token (400 days) verifies to its principal.
        let token = s
            .issue_session_with_ttl_days(ws.id, u.id, Role::Member, None, 400)
            .await
            .unwrap();
        assert!(
            (token.expires_at - Utc::now()).num_days() >= 399,
            "the TTL is honored (~400 days out)"
        );
        let p = s.verify_bearer(&token.token).await.unwrap();
        assert_eq!(p.user_id, u.id);
        assert_eq!(p.role, Role::Member);

        // Revoking it makes verification fail immediately.
        assert!(
            s.revoke_session(&token.token).await.unwrap(),
            "revoke reports it existed"
        );
        assert!(
            s.verify_bearer(&token.token).await.is_err(),
            "a revoked token no longer verifies"
        );
        assert!(
            !s.revoke_session(&token.token).await.unwrap(),
            "re-revoke is a no-op"
        );
    }

    #[tokio::test]
    async fn grant_scoped_token_carries_grant_id_through_verify() {
        // A grant-scoped service token (SOUL §19/§26) records its grant on the
        // session and surfaces it on the verified principal, so the API can resolve
        // the grant's capabilities. A grantless token carries `None`.
        let s = svc();
        let ws = s.create_workspace("Work", "work").await.unwrap();
        let u = s.create_user("g@b.c", "G").await.unwrap();
        s.add_membership(ws.id, u.id, Role::Member).await.unwrap();
        let gid = catalerum_core::GrantId::new();

        let scoped = s
            .issue_session_with_ttl_days(ws.id, u.id, Role::Member, Some(gid), 30)
            .await
            .unwrap();
        assert_eq!(scoped.grant_id, Some(gid));
        assert_eq!(
            s.verify_bearer(&scoped.token).await.unwrap().grant_id,
            Some(gid),
            "the grant scope survives verify_bearer"
        );

        let plain = s
            .issue_session_with_ttl_days(ws.id, u.id, Role::Member, None, 30)
            .await
            .unwrap();
        assert_eq!(plain.grant_id, None);
        assert_eq!(s.verify_bearer(&plain.token).await.unwrap().grant_id, None);
    }

    #[tokio::test]
    async fn issue_session_requires_membership() {
        let s = svc();
        let ws = s.create_workspace("Work", "work").await.unwrap();
        let u = s.create_user("a@b.c", "Alice").await.unwrap();
        let err = s.issue_session(ws.id, u.id).await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[tokio::test]
    async fn issue_session_into_archived_workspace_is_forbidden() {
        // Fail closed at the session-issue chokepoint (SOUL §18): even a member
        // cannot establish a session into an archived workspace, so ALL login
        // paths (magic-link redeem, `/auth/switch`) reject it. The service's own
        // `create_workspace` always stamps `archived_at: None`, so we insert an
        // already-archived row directly via the store.
        let s = svc();
        let ws = Workspace {
            id: WorkspaceId::new(),
            organisation_id: DEFAULT_ORGANISATION_ID,
            name: "Archived".into(),
            slug: "archived".into(),
            archived_at: Some(Utc::now()),
        };
        s.store().create_workspace(&ws).await.unwrap();
        let u = s.create_user("a@b.c", "Alice").await.unwrap();
        s.add_membership(ws.id, u.id, Role::Owner).await.unwrap();

        let err = s.issue_session(ws.id, u.id).await.unwrap_err();
        assert!(
            matches!(err, Error::Forbidden(_)),
            "issuing a session into an archived workspace is forbidden, got {err:?}"
        );
        // The membership genuinely exists — the rejection is the archive guard,
        // not a missing membership (which would be `Unauthorized`).
        assert!(s.membership(ws.id, u.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn duplicate_slug_conflicts() {
        let s = svc();
        s.create_workspace("Work", "work").await.unwrap();
        let err = s.create_workspace("Work2", "work").await.unwrap_err();
        assert!(matches!(err, Error::Conflict(_)));
    }

    #[tokio::test]
    async fn dev_login_seeds_and_is_idempotent() {
        let s = svc();
        let link = s.ensure_dev_login().await.unwrap();
        assert!(link
            .url
            .starts_with("http://localhost:8787/auth/magic?token="));
        assert!(link.url.ends_with(&link.token));

        // Re-running reuses workspace/admin and mints a new token.
        let link2 = s.ensure_dev_login().await.unwrap();
        assert_eq!(link.workspace_id, link2.workspace_id);
        assert_eq!(link.user_id, link2.user_id);
        assert_ne!(link.token, link2.token);

        // Redeem yields a session for the admin (Owner).
        let session = s.redeem_login_token(&link2.token).await.unwrap();
        assert_eq!(session.role, Role::Owner);
        let principal = s.verify_bearer(&session.token).await.unwrap();
        assert_eq!(principal.workspace_id, link.workspace_id);
    }

    #[tokio::test]
    async fn dev_authorization_token_is_stable_and_verifiable() {
        let s = svc();
        let session = s
            .ensure_dev_authorization_token_days("dev-stable-token", 1)
            .await
            .unwrap();
        assert_eq!(session.token, "dev-stable-token");
        assert_eq!(session.role, Role::Owner);

        let again = s
            .ensure_dev_authorization_token_days("dev-stable-token", 1)
            .await
            .unwrap();
        assert_eq!(again.token, session.token);
        assert_eq!(again.workspace_id, session.workspace_id);
        assert_eq!(again.user_id, session.user_id);

        let principal = s.verify_bearer("dev-stable-token").await.unwrap();
        assert_eq!(principal.workspace_id, session.workspace_id);
        assert_eq!(principal.user_id, session.user_id);
        assert_eq!(principal.role, Role::Owner);
    }

    #[tokio::test]
    async fn login_token_is_single_use() {
        let s = svc();
        let link = s.ensure_dev_login().await.unwrap();
        s.redeem_login_token(&link.token).await.unwrap();
        let err = s.redeem_login_token(&link.token).await.unwrap_err();
        assert!(matches!(err, Error::TokenConsumed));
    }

    #[tokio::test]
    async fn handoff_code_flow_mints_short_lived_single_use_codes() {
        // The browser login handoff (SOUL §18): `consume_login_token` consumes
        // the magic token WITHOUT a session; `issue_login_token_with_ttl` mints
        // the short-lived handoff code; `redeem_login_token` exchanges it for
        // the real session. The session token never touches a URL.
        use crate::session::HANDOFF_TOKEN_TTL;
        let s = svc();
        let ws = s.create_workspace("Work", "work").await.unwrap();
        let u = s.create_user("a@b.c", "Alice").await.unwrap();
        s.add_membership(ws.id, u.id, Role::Member).await.unwrap();

        // Step 1: consume the magic token — no session is issued yet.
        let magic = s.issue_login_token(ws.id, u.id).await.unwrap();
        let binding = s.consume_login_token(&magic.token).await.unwrap();
        assert_eq!(binding.user_id, u.id);
        assert_eq!(binding.workspace_id, ws.id);
        // Consuming again reports the token spent (single-use holds).
        assert!(matches!(
            s.consume_login_token(&magic.token).await.unwrap_err(),
            Error::TokenConsumed
        ));

        // Step 2: the handoff code carries the same binding with a short TTL.
        let handoff = s
            .issue_login_token_with_ttl(
                binding.workspace_id,
                binding.user_id,
                HANDOFF_TOKEN_TTL,
            )
            .await
            .unwrap();
        let ttl = handoff.expires_at - Utc::now();
        assert!(
            ttl <= HANDOFF_TOKEN_TTL && ttl > HANDOFF_TOKEN_TTL - Duration::minutes(1),
            "the handoff code lives ~5 minutes, got {ttl}"
        );

        // Step 3: exchange redeems the code into the real session.
        let session = s.redeem_login_token(&handoff.token).await.unwrap();
        assert_eq!(session.user_id, u.id);
        assert_eq!(session.role, Role::Member);
        // And the code is spent — no replay.
        assert!(matches!(
            s.redeem_login_token(&handoff.token).await.unwrap_err(),
            Error::TokenConsumed
        ));
    }

    #[tokio::test]
    async fn expired_login_token_is_rejected() {
        // A token minted with a negative TTL is already expired. `consume` itself
        // only checks not-yet-consumed (the SQL doesn't gate on expiry), so this
        // locks the invariant that the **service layer** enforces expiry: an
        // expired magic link must never log in. Guards against a future regression
        // that drops the `redeem_login_token` expiry check.
        let s = svc().with_login_ttl(Duration::seconds(-60));
        let ws = s.create_workspace("Work", "work").await.unwrap();
        let u = s.create_user("a@b.c", "Alice").await.unwrap();
        s.add_membership(ws.id, u.id, Role::Member).await.unwrap();
        let link = s.issue_login_token(ws.id, u.id).await.unwrap();

        let err = s.redeem_login_token(&link.token).await.unwrap_err();
        assert!(
            matches!(err, Error::Unauthorized(_)),
            "an expired token must be rejected, got {err:?}"
        );
        // The rejected attempt still consumed it (single-use holds on the expired
        // path too), so a retry now reports it consumed rather than expired.
        let err2 = s.redeem_login_token(&link.token).await.unwrap_err();
        assert!(
            matches!(err2, Error::TokenConsumed),
            "the expired attempt consumed the token, got {err2:?}"
        );
    }

    #[tokio::test]
    async fn revoke_invalidates_session() {
        let s = svc();
        let link = s.ensure_dev_login().await.unwrap();
        let session = s.redeem_login_token(&link.token).await.unwrap();
        assert!(s.revoke_session(&session.token).await.unwrap());
        let err = s.verify_bearer(&session.token).await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[tokio::test]
    async fn custom_base_url_in_link() {
        let s = svc().with_base_url("https://app.example.com/");
        let link = s.ensure_dev_login().await.unwrap();
        assert!(link
            .url
            .starts_with("https://app.example.com/auth/magic?token="));
    }

    // ---------------------------------------------------------------------
    // SSO identity resolution (match order, SOUL §18/§29)
    // ---------------------------------------------------------------------

    fn identity(sub: &str, email: Option<&str>, verified: bool) -> SsoIdentity {
        SsoIdentity {
            subject: Subject {
                issuer: "https://idp.example.com".into(),
                subject: sub.into(),
            },
            email: email.map(str::to_string),
            email_verified: verified,
            display_name: Some("Someone".into()),
        }
    }

    #[tokio::test]
    async fn sso_resolves_bound_subject_to_that_user() {
        let s = svc();
        let user = s
            .create_sso_user(
                "bound@example.com",
                "Bound",
                Subject {
                    issuer: "https://idp.example.com".into(),
                    subject: "sub-1".into(),
                },
            )
            .await
            .unwrap();
        // Even with JIT off and an unverified email, a bound subject resolves.
        let res = s
            .resolve_sso_identity(
                &identity("sub-1", Some("bound@example.com"), false),
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(res, SsoResolution::Existing(user));
    }

    #[tokio::test]
    async fn sso_links_verified_email_to_existing_local_user() {
        let s = svc();
        // A pre-existing local (non-SSO) user with the same email, case-differing.
        let local = s.create_user("Alice@Example.com", "Alice").await.unwrap();
        assert!(local.sso_subject.is_none());
        let res = s
            .resolve_sso_identity(
                &identity("sub-2", Some("alice@example.com"), true),
                false,
                false,
            )
            .await
            .unwrap();
        match res {
            SsoResolution::Existing(u) => {
                assert_eq!(u.id, local.id, "adopted the existing account, no duplicate");
                assert_eq!(
                    u.sso_subject.as_ref().map(|s| s.subject.as_str()),
                    Some("sub-2"),
                    "the subject was bound onto it"
                );
            }
            other => panic!("expected email link, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sso_will_not_link_unverified_email_and_denies_when_jit_off() {
        let s = svc();
        s.create_user("bob@example.com", "Bob").await.unwrap();
        // Unverified email + trust_email off → no link; JIT off → deny.
        let res = s
            .resolve_sso_identity(
                &identity("sub-3", Some("bob@example.com"), false),
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            res,
            SsoResolution::Denied(SsoDenyReason::ProvisioningDisabled)
        );
    }

    #[tokio::test]
    async fn sso_trust_email_links_unverified() {
        let s = svc();
        let local = s.create_user("carol@example.com", "Carol").await.unwrap();
        // trust_email = true bypasses the email_verified gate.
        let res = s
            .resolve_sso_identity(
                &identity("sub-4", Some("carol@example.com"), false),
                false,
                true,
            )
            .await
            .unwrap();
        match res {
            SsoResolution::Existing(u) => assert_eq!(u.id, local.id),
            other => panic!("expected link via trust_email, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sso_refuses_email_already_linked_to_another_subject() {
        let s = svc();
        // An account already SSO-bound to subject X, email dave@.
        s.create_sso_user(
            "dave@example.com",
            "Dave",
            Subject {
                issuer: "https://idp.example.com".into(),
                subject: "sub-X".into(),
            },
        )
        .await
        .unwrap();
        // A *different* subject arrives with the same verified email → refuse.
        let res = s
            .resolve_sso_identity(
                &identity("sub-Y", Some("dave@example.com"), true),
                true,
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            res,
            SsoResolution::Denied(SsoDenyReason::EmailAlreadyLinked)
        );
    }

    #[tokio::test]
    async fn sso_jit_provisions_new_user_with_verified_email() {
        let s = svc();
        let res = s
            .resolve_sso_identity(
                &identity("sub-5", Some("new@example.com"), true),
                true,
                false,
            )
            .await
            .unwrap();
        match res {
            SsoResolution::Provisioned(u) => {
                assert_eq!(u.email, "new@example.com");
                assert_eq!(
                    u.sso_subject.as_ref().map(|s| s.subject.as_str()),
                    Some("sub-5")
                );
            }
            other => panic!("expected JIT provision, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sso_jit_disabled_denies() {
        let s = svc();
        let res = s
            .resolve_sso_identity(
                &identity("sub-6", Some("x@example.com"), true),
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            res,
            SsoResolution::Denied(SsoDenyReason::ProvisioningDisabled)
        );
    }

    #[tokio::test]
    async fn sso_no_email_cannot_provision() {
        let s = svc();
        // JIT on but the id_token carried no email → clear deny, never a guess.
        let res = s
            .resolve_sso_identity(&identity("sub-7", None, false), true, false)
            .await
            .unwrap();
        assert_eq!(res, SsoResolution::Denied(SsoDenyReason::NoVerifiedEmail));
    }
}
