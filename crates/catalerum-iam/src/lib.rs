//! catalerum-iam — workspaces, users, pluggable auth (SSO + zero-config dev
//! magic-link login), sessions, capabilities, and grants. Workspace is the
//! tenancy boundary; authority is capability-scoped and attenuating
//! (SOUL §18, §19).
//!
//! # Shape
//! - [`IamService`] is the orchestration entry point the API/binary calls into.
//!   It is generic over an [`IamStore`]:
//!   - [`PgIamStore`] — delegates persistence to the `catalerum-store`
//!     repositories (the single Postgres source of truth, SOUL §6.1); iam runs
//!     no migrations of its own.
//!   - [`MemoryIamStore`] — in-process store for tests / the dev path.
//! - [`Session`] / [`Principal`] model opaque, expiring, workspace-scoped bearer
//!   tokens and the `{user, workspace, role}` the API enforces against.
//! - [`MagicLink`] + [`IamService::ensure_dev_login`] implement the zero-config
//!   dev login (SOUL §17/§18): seed admin + default workspace + one-time token,
//!   yielding a login URL.
//! - [`capability`] resolves a [`Role`](catalerum_core::model::Role) to its base
//!   capability set and re-exports the deny-by-default grant matcher
//!   ([`allows`](capability::allows)).
//!
//! # Quick start
//! ```no_run
//! use catalerum_iam::{IamService, MemoryIamStore};
//!
//! # async fn demo() -> catalerum_iam::Result<()> {
//! let iam = IamService::new(MemoryIamStore::new());
//! // First run: seed admin + default workspace + one-time token.
//! let link = iam.ensure_dev_login().await?;
//! println!("Open to log in: {}", link.url);
//!
//! // The API redeems the token from `GET /auth/magic?token=…` → a session.
//! let session = iam.redeem_login_token(&link.token).await?;
//!
//! // Per request: turn the bearer token into an authenticated principal.
//! let principal = iam.verify_bearer(&session.token).await?;
//! assert_eq!(principal.role, catalerum_core::model::Role::Owner);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod capability;
mod error;
mod magic;
pub mod org;
mod service;
mod session;
pub mod sso;
mod store;
pub mod token;

pub use error::{Error, Result};
pub use magic::{MagicLink, MAGIC_PATH};
pub use service::{
    IamService, SsoDenyReason, SsoResolution, DEFAULT_ADMIN_EMAIL, DEFAULT_ADMIN_NAME,
    DEFAULT_BASE_URL, DEFAULT_ORGANISATION_ID, DEFAULT_ORGANISATION_NAME,
    DEFAULT_ORGANISATION_SLUG, DEFAULT_WORKSPACE_NAME, DEFAULT_WORKSPACE_SLUG,
};
// The OIDC SSO engine (SOUL §18/§29): discovery/JWKS/token + id_token validation.
pub use session::{
    Principal, Session, DEFAULT_LOGIN_TOKEN_TTL, DEFAULT_SESSION_TTL, HANDOFF_TOKEN_TTL,
};
pub use sso::{OidcProvider, OidcSettings, SsoIdentity, DEFAULT_LEEWAY_SECS, DEFAULT_SCOPES};
pub use store::{IamStore, LoginToken, MemoryIamStore, PgIamStore};

// Convenience re-exports of the capability surface callers touch most.
pub use capability::{
    base_capabilities, is_admin, role_allows, role_from_str, role_grant, role_str, STANDARD_DOMAINS,
};

// Organisation role + creation-policy gating (SOUL §18).
pub use org::{
    is_org_admin, is_org_owner, org_admin_or_owner, org_any_member, org_role_from_str,
    org_role_str, organisation_creation_allowed, workspace_creation_allowed,
};

// Re-export the core authorization types so downstream crates can `use
// catalerum_iam::{Capability, Action, …}` without a separate core import.
pub use catalerum_core::capability::{
    allows as grant_allows, attenuate, Action, AttenuationError, Capability, Constraints, Resource,
};
pub use catalerum_core::model::{
    CreationPolicy, Membership, OrgMembership, OrgRole, Organisation, Role, Subject, User,
    Workspace,
};
