//! IAM persistence (SOUL §6.1, §18).
//!
//! The [`IamStore`] trait is the storage contract every IAM operation runs
//! against — workspaces, users, memberships, sessions, and one-time login
//! tokens. Two implementations ship:
//!
//! - [`PgIamStore`] — delegates to the `catalerum-store` repositories, which are
//!   the single Postgres source of truth (SOUL §6.1). iam owns no schema and
//!   runs no migrations; each table is queried in exactly one place (the store
//!   repo). Tokens are stored hashed; a session's role is derived from
//!   membership on read-back.
//! - [`MemoryIamStore`] — an in-process store for tests and the zero-dependency
//!   dev path.
//!
//! All rows carry `workspace_id` where applicable; queries are
//! workspace-scoped. The trait uses native `async fn` in traits (AFIT, stable
//! since Rust 1.75) so it needs no `async_trait` dependency; the methods are
//! `Send`-bounded for use across `.await` points in the API.

use std::future::Future;

use chrono::{DateTime, Utc};

use catalerum_core::model::{Membership, Subject, User, Workspace};
use catalerum_core::{UserId, WorkspaceId};

use crate::session::Session;
use crate::Result;

/// A persisted one-time login token (dev magic-link, SOUL §18).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginToken {
    pub token: String,
    pub user_id: UserId,
    pub workspace_id: WorkspaceId,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// When the token was redeemed; `None` while still usable.
    pub consumed_at: Option<DateTime<Utc>>,
}

/// The IAM storage contract (SOUL §6.1). Every method is workspace-aware where
/// the row is tenant-scoped.
///
/// Methods desugar async-fn-in-trait to `-> impl Future + Send` so the returned
/// futures are usable across `.await` in `Send` contexts (Axum handlers); no
/// `async_trait` dependency is needed. For trait-object (`dyn`) dispatch, wrap a
/// concrete store in [`IamService`](crate::IamService) (generic) rather than
/// boxing the trait directly.
pub trait IamStore: Send + Sync {
    // --- workspaces ---

    /// Insert a workspace, returning the **persisted** row. The store is the
    /// authority for the row's id: callers must adopt the returned
    /// [`Workspace`] (its id is the one written to the DB and referenced by
    /// membership/session FKs), not the value they passed in.
    fn create_workspace(&self, ws: &Workspace) -> impl Future<Output = Result<Workspace>> + Send;
    /// Fetch a workspace by id, or `None`.
    fn get_workspace(
        &self,
        id: WorkspaceId,
    ) -> impl Future<Output = Result<Option<Workspace>>> + Send;
    /// Fetch a workspace by its unique slug, or `None`.
    fn get_workspace_by_slug(
        &self,
        slug: &str,
    ) -> impl Future<Output = Result<Option<Workspace>>> + Send;

    // --- users ---

    /// Insert a user, returning the **persisted** row. As with
    /// [`Self::create_workspace`], the store owns the row's id and callers must
    /// adopt the returned [`User`].
    fn create_user(&self, user: &User) -> impl Future<Output = Result<User>> + Send;
    /// Fetch a user by id, or `None`.
    fn get_user(&self, id: UserId) -> impl Future<Output = Result<Option<User>>> + Send;
    /// Fetch a user by email (case-sensitive, as stored), or `None`.
    fn get_user_by_email(&self, email: &str) -> impl Future<Output = Result<Option<User>>> + Send;
    /// Fetch a user by email matched **case-insensitively** (exact address, folded
    /// on both sides), or `None` — the SSO first-login email-linking lookup (§18).
    fn get_user_by_email_ci(
        &self,
        email: &str,
    ) -> impl Future<Output = Result<Option<User>>> + Send;
    /// Fetch a user by their SSO [`Subject`] (`(issuer, subject)` pair), or `None` —
    /// the primary SSO match (§18).
    fn get_user_by_sso(
        &self,
        subject: &Subject,
    ) -> impl Future<Output = Result<Option<User>>> + Send;
    /// Bind an SSO [`Subject`] onto an existing user (first-login account linking,
    /// §18). Fails [`Conflict`](crate::Error::Conflict) if the subject is already
    /// bound to a different user (the store's unique index enforces this).
    fn bind_sso_subject(
        &self,
        user_id: UserId,
        subject: &Subject,
    ) -> impl Future<Output = Result<User>> + Send;

    // --- memberships ---

    /// Insert or replace a membership (`{workspace, user}` is the key).
    fn upsert_membership(&self, m: &Membership) -> impl Future<Output = Result<()>> + Send;
    /// Resolve a user's role in a workspace, or `None` if not a member.
    fn get_membership(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> impl Future<Output = Result<Option<Membership>>> + Send;

    // --- sessions ---

    /// Persist a new session.
    fn create_session(&self, s: &Session) -> impl Future<Output = Result<()>> + Send;
    /// Look up a session by its opaque token, or `None`.
    fn get_session(&self, token: &str) -> impl Future<Output = Result<Option<Session>>> + Send;
    /// Delete a session (logout / revoke). Returns whether a row was removed.
    fn delete_session(&self, token: &str) -> impl Future<Output = Result<bool>> + Send;
    /// Purge all expired sessions as of `now`. Returns the count removed.
    fn purge_expired_sessions(
        &self,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<u64>> + Send;

    // --- one-time login tokens ---

    /// Persist a new one-time login token.
    fn create_login_token(&self, t: &LoginToken) -> impl Future<Output = Result<()>> + Send;
    /// Look up a login token by value, or `None`.
    fn get_login_token(
        &self,
        token: &str,
    ) -> impl Future<Output = Result<Option<LoginToken>>> + Send;
    /// Atomically mark a login token consumed at `now`, returning the row as it
    /// was *before* consumption.
    ///
    /// # Errors
    /// - [`Error::NotFound`] if the token does not exist.
    /// - [`Error::TokenConsumed`] if it was already redeemed.
    fn consume_login_token(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<LoginToken>> + Send;
}

mod memory;
mod postgres;

pub use memory::MemoryIamStore;
pub use postgres::PgIamStore;
