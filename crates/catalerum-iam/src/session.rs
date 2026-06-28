//! Sessions and the authenticated principal (SOUL §18).
//!
//! A [`Session`] is an opaque, workspace-scoped, expiring bearer token bound to
//! `{user, workspace, role}`. Verifying a token yields a [`Principal`] — the
//! resolved identity the API enforces capabilities against (SOUL §19).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use catalerum_core::model::Role;
use catalerum_core::{GrantId, UserId, WorkspaceId};

/// Default session lifetime: short-lived, per SOUL §18 ("sessions issue
/// short-lived, workspace-scoped tokens").
pub const DEFAULT_SESSION_TTL: Duration = Duration::hours(12);

/// Default lifetime of a one-time dev magic-link login token.
pub const DEFAULT_LOGIN_TOKEN_TTL: Duration = Duration::hours(24);

/// Lifetime of a browser **handoff code** (SOUL §18): the short-lived one-time
/// token the API redirects into the SPA as `?code=…` after a magic-link / SSO
/// login. The SPA exchanges it for the real session via `POST /auth/exchange`,
/// so the long-lived session bearer never appears in a URL (browser history,
/// referers, access logs). Five minutes is ample for one redirect + one POST.
pub const HANDOFF_TOKEN_TTL: Duration = Duration::minutes(5);

/// A persisted session: an opaque token bound to a principal, with an expiry
/// (SOUL §18). The token itself is the random string from [`crate::token`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// The opaque bearer token (primary key).
    pub token: String,
    pub user_id: UserId,
    pub workspace_id: WorkspaceId,
    pub role: Role,
    /// When set, the named §19 grant this token is **scoped to** (SOUL §19/§26):
    /// the bearer's effective authority is the grant's capabilities (⊆ the
    /// minting user's role), not the role's full base set. `None` = today's
    /// role-derived authority. The grant is resolved (and fail-closed if
    /// deleted) at auth time — the session row only records the reference.
    pub grant_id: Option<GrantId>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl Session {
    /// Is the session still valid at `now`?
    #[must_use]
    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        now < self.expires_at
    }

    /// Is the session expired as of the current wall clock?
    #[must_use]
    pub fn is_expired(&self) -> bool {
        !self.is_valid_at(Utc::now())
    }

    /// Resolve this session into a [`Principal`] (drops the token, keeps the
    /// identity triple).
    #[must_use]
    pub fn principal(&self) -> Principal {
        Principal {
            user_id: self.user_id,
            workspace_id: self.workspace_id,
            role: self.role,
            grant_id: self.grant_id,
        }
    }
}

/// The authenticated principal the API acts as on every request (SOUL §18/§19):
/// `{user, workspace, role}`. Capability checks (`§19`) are resolved against
/// this triple (see [`crate::capability`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub user_id: UserId,
    pub workspace_id: WorkspaceId,
    pub role: Role,
    /// The named §19 grant this principal's bearer is **scoped to**, if any
    /// (SOUL §19/§26). Copied through from the [`Session`]. `None` = role-derived
    /// authority. The API resolves this reference into the grant's capabilities at
    /// request time (failing closed if the grant was deleted); the role alone
    /// never widens a grant-bound token.
    #[serde(default)]
    pub grant_id: Option<GrantId>,
}

impl Principal {
    /// Construct a role-derived principal (e.g. for tests or a login session) —
    /// `grant_id` is `None`, so authority is the role's base set.
    #[must_use]
    pub fn new(user_id: UserId, workspace_id: WorkspaceId, role: Role) -> Self {
        Self {
            user_id,
            workspace_id,
            role,
            grant_id: None,
        }
    }

    /// The base capabilities this principal holds by virtue of its role
    /// (SOUL §19); convenience wrapper over [`crate::capability::base_capabilities`].
    #[must_use]
    pub fn base_capabilities(&self) -> Vec<catalerum_core::capability::Capability> {
        crate::capability::base_capabilities(self.role)
    }

    /// A synthetic [`Grant`](catalerum_core::model::Grant) for this principal's
    /// role-derived authority within its workspace (SOUL §19).
    #[must_use]
    pub fn role_grant(&self) -> catalerum_core::model::Grant {
        crate::capability::role_grant(self.workspace_id, self.role)
    }
}
