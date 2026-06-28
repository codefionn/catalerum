//! Postgres [`IamStore`] — delegates to the `catalerum-store` repositories
//! (SOUL §6.1: `catalerum-store` is the single Postgres source of truth; iam is
//! a logic layer).
//!
//! This store owns **no** schema and runs **no** migrations: `catalerum-store`
//! owns the entire identity+chat schema and the only sqlx migrator. Each table
//! (workspaces / users / memberships / sessions / login_tokens) is queried in
//! exactly one place — the corresponding `catalerum-store` repo — and this type
//! is a thin adapter from the [`IamStore`] contract onto those repos.
//!
//! ## Reconciling the session/principal model to the store schema
//! The store's `sessions` table is `{id, workspace_id, user_id, token_hash,
//! created_at, expires_at}` — it has **no** `role` column. iam therefore:
//! - stores only the **hash** of each session / login token (the raw token is
//!   high-entropy and returned to the caller; the DB never sees plaintext);
//! - derives a session's `role` by looking up `memberships(workspace_id,
//!   user_id)` when a session is read back.

use catalerum_core::model::{Membership, Subject, User, Workspace};
use catalerum_core::{UserId, WorkspaceId};
use chrono::{DateTime, Utc};

use catalerum_store::{
    DbPool, LoginTokenRepo, MembershipRepo, SessionRepo, Store, StoreError, UserRepo, WorkspaceRepo,
};

use super::{IamStore, LoginToken};
use crate::session::Session;
use crate::token::hash_token;
use crate::{Error, Result};

/// A Postgres-backed IAM store. Holds a [`Store`] (the `catalerum-store`
/// source-of-truth facade) and delegates every operation to its repos.
#[derive(Clone)]
pub struct PgIamStore {
    store: Store,
}

impl PgIamStore {
    /// Wrap an existing connection pool. The pool is shared with
    /// `catalerum-store`; this store performs no migrations of its own.
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self {
            store: Store::new(pool),
        }
    }

    /// Build directly from a shared [`Store`].
    #[must_use]
    pub fn from_store(store: Store) -> Self {
        Self { store }
    }

    /// Borrow the underlying pool (for callers that share it).
    #[must_use]
    pub fn pool(&self) -> &DbPool {
        self.store.pool()
    }

    // --- repo accessors ---

    fn workspaces(&self) -> WorkspaceRepo {
        self.store.workspaces()
    }

    fn users(&self) -> UserRepo {
        self.store.users()
    }

    fn memberships(&self) -> MembershipRepo {
        self.store.memberships()
    }

    fn sessions(&self) -> SessionRepo {
        self.store.sessions()
    }

    fn login_tokens(&self) -> LoginTokenRepo {
        self.store.login_tokens()
    }
}

/// Map a [`StoreError`] onto the IAM [`Error`]. `NotFound` and `Conflict`
/// translate directly; everything else surfaces as a store error.
fn map_store(e: StoreError) -> Error {
    match e {
        StoreError::NotFound => Error::NotFound,
        StoreError::Conflict(m) => Error::Conflict(m),
        StoreError::Decode(m) => Error::Invalid(m),
        StoreError::Sqlx(e) => Error::Store(e),
        // `StoreError` is `#[non_exhaustive]` (e.g. `Migrate`); fold anything
        // else into the shared core error.
        other => Error::Core(catalerum_core::Error::other(other.to_string())),
    }
}

/// `Ok(None)` for a missing row, `Ok(Some(_))` otherwise, `Err` for any other
/// store failure. Used to turn the repos' `NotFound`-as-error style into the
/// `Option` style the [`IamStore`] trait expects.
fn optional<T>(res: std::result::Result<T, StoreError>) -> Result<Option<T>> {
    match res {
        Ok(v) => Ok(Some(v)),
        Err(StoreError::NotFound) => Ok(None),
        Err(e) => Err(map_store(e)),
    }
}

impl IamStore for PgIamStore {
    // --- workspaces ---

    async fn create_workspace(&self, ws: &Workspace) -> Result<Workspace> {
        // The store assigns + returns the persisted id; propagate that row back
        // so iam references the id that actually landed in the table (FKs for
        // memberships/sessions depend on it).
        self.workspaces()
            .create(&ws.name, &ws.slug)
            .await
            .map_err(map_store)
    }

    async fn get_workspace(&self, id: WorkspaceId) -> Result<Option<Workspace>> {
        optional(self.workspaces().get(id).await)
    }

    async fn get_workspace_by_slug(&self, slug: &str) -> Result<Option<Workspace>> {
        optional(self.workspaces().get_by_slug(slug).await)
    }

    // --- users ---

    async fn create_user(&self, user: &User) -> Result<User> {
        let sso = user
            .sso_subject
            .as_ref()
            .map(|s| (s.issuer.as_str(), s.subject.as_str()));
        // Return the store-assigned row so iam uses the persisted user id.
        self.users()
            .create(&user.email, &user.display_name, sso)
            .await
            .map_err(map_store)
    }

    async fn get_user(&self, id: UserId) -> Result<Option<User>> {
        optional(self.users().get(id).await)
    }

    async fn get_user_by_email(&self, email: &str) -> Result<Option<User>> {
        optional(self.users().get_by_email(email).await)
    }

    async fn get_user_by_email_ci(&self, email: &str) -> Result<Option<User>> {
        optional(self.users().get_by_email_ci(email).await)
    }

    async fn get_user_by_sso(&self, subject: &Subject) -> Result<Option<User>> {
        optional(
            self.users()
                .get_by_sso(&subject.issuer, &subject.subject)
                .await,
        )
    }

    async fn bind_sso_subject(&self, user_id: UserId, subject: &Subject) -> Result<User> {
        self.users()
            .bind_sso(user_id, &subject.issuer, &subject.subject)
            .await
            .map_err(map_store)
    }

    // --- memberships ---

    async fn upsert_membership(&self, m: &Membership) -> Result<()> {
        self.memberships()
            .upsert(m.workspace_id, m.user_id, m.role)
            .await
            .map_err(map_store)?;
        Ok(())
    }

    async fn get_membership(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<Option<Membership>> {
        optional(self.memberships().get(workspace_id, user_id).await)
    }

    // --- sessions ---
    //
    // The store's `sessions` row has no `role`; iam stores only the token hash
    // and derives the role from membership on read-back.

    async fn create_session(&self, s: &Session) -> Result<()> {
        let token_hash = hash_token(&s.token);
        self.sessions()
            .create(
                s.workspace_id,
                s.user_id,
                &token_hash,
                s.grant_id,
                s.expires_at,
            )
            .await
            .map_err(map_store)?;
        Ok(())
    }

    async fn get_session(&self, token: &str) -> Result<Option<Session>> {
        let token_hash = hash_token(token);
        let row = match self.sessions().get_by_token_hash(&token_hash).await {
            Ok(row) => row,
            Err(StoreError::NotFound) => return Ok(None),
            Err(e) => return Err(map_store(e)),
        };
        let workspace_id = row.workspace_id();
        let user_id = row.user_id();
        // Derive the principal's role from membership (the session row carries
        // no role). A session whose membership was revoked is treated as no
        // longer valid.
        let role = match self.memberships().get(workspace_id, user_id).await {
            Ok(m) => m.role,
            Err(StoreError::NotFound) => return Ok(None),
            Err(e) => return Err(map_store(e)),
        };
        Ok(Some(Session {
            token: token.to_string(),
            user_id,
            workspace_id,
            role,
            // Carry the token's grant scope through; the API resolves it into the
            // grant's capabilities (failing closed if the grant was deleted) at
            // request time. The store's composite FK also cascade-revokes a
            // grant-bound session when its grant is removed (defense-in-depth).
            grant_id: row.grant_id.map(catalerum_core::GrantId::from_uuid),
            created_at: row.created_at,
            expires_at: row.expires_at,
        }))
    }

    async fn delete_session(&self, token: &str) -> Result<bool> {
        let token_hash = hash_token(token);
        // Resolve the session id from its hash, then delete by id.
        match self.sessions().get_by_token_hash(&token_hash).await {
            Ok(row) => {
                self.sessions().delete(row.id).await.map_err(map_store)?;
                Ok(true)
            }
            // `get_by_token_hash` also filters out expired rows; either way
            // there is nothing live to revoke.
            Err(StoreError::NotFound) => Ok(false),
            Err(e) => Err(map_store(e)),
        }
    }

    async fn purge_expired_sessions(&self, _now: DateTime<Utc>) -> Result<u64> {
        // The store deletes by `expires_at <= now()` server-side; the `now`
        // argument is the trait's contract knob and is intentionally ignored
        // so deletion uses the database clock.
        self.sessions().delete_expired().await.map_err(map_store)
    }

    // --- one-time login tokens ---

    async fn create_login_token(&self, t: &LoginToken) -> Result<()> {
        let token_hash = hash_token(&t.token);
        self.login_tokens()
            .create(t.workspace_id, t.user_id, &token_hash, t.expires_at)
            .await
            .map_err(map_store)?;
        Ok(())
    }

    async fn get_login_token(&self, token: &str) -> Result<Option<LoginToken>> {
        let token_hash = hash_token(token);
        match self.login_tokens().get_by_token_hash(&token_hash).await {
            Ok(row) => Ok(Some(LoginToken {
                // Echo back the raw token the caller supplied; the DB only
                // holds its hash.
                token: token.to_string(),
                user_id: row.user_id(),
                workspace_id: row.workspace_id(),
                created_at: row.created_at,
                expires_at: row.expires_at,
                consumed_at: row.consumed_at,
            })),
            Err(StoreError::NotFound) => Ok(None),
            Err(e) => Err(map_store(e)),
        }
    }

    async fn consume_login_token(&self, token: &str, now: DateTime<Utc>) -> Result<LoginToken> {
        let token_hash = hash_token(token);
        // Atomic single-shot consume: only flips the row if not yet consumed,
        // returning the pre-consumption snapshot.
        match self.login_tokens().consume(&token_hash, now).await {
            Ok(row) => Ok(LoginToken {
                token: token.to_string(),
                user_id: row.user_id(),
                workspace_id: row.workspace_id(),
                created_at: row.created_at,
                expires_at: row.expires_at,
                consumed_at: None,
            }),
            // The atomic UPDATE matched no row: the token is either unknown or
            // already consumed. Disambiguate with a follow-up read.
            Err(StoreError::NotFound) => {
                match self.login_tokens().get_by_token_hash(&token_hash).await {
                    Ok(_) => Err(Error::TokenConsumed),
                    Err(StoreError::NotFound) => Err(Error::NotFound),
                    Err(e) => Err(map_store(e)),
                }
            }
            Err(e) => Err(map_store(e)),
        }
    }
}
