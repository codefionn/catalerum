//! In-memory [`IamStore`] for tests and the zero-dependency dev path.
//!
//! Backed by `Mutex`-guarded maps. Not durable — a process restart loses all
//! state — but exercises the exact same [`IamStore`] contract as
//! [`PgIamStore`](super::PgIamStore), so service logic is tested without a
//! database.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use catalerum_core::model::{Membership, Subject, User, Workspace};
use catalerum_core::{UserId, WorkspaceId};

use super::{IamStore, LoginToken};
use crate::session::Session;
use crate::{Error, Result};

/// An in-process IAM store. Cheap to construct; clone via `Arc` for sharing.
#[derive(Default)]
pub struct MemoryIamStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    workspaces: HashMap<WorkspaceId, Workspace>,
    users: HashMap<UserId, User>,
    memberships: HashMap<(WorkspaceId, UserId), Membership>,
    sessions: HashMap<String, Session>,
    login_tokens: HashMap<String, LoginToken>,
}

impl MemoryIamStore {
    /// Construct an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl IamStore for MemoryIamStore {
    async fn create_workspace(&self, ws: &Workspace) -> Result<Workspace> {
        let mut g = self.inner.lock().unwrap();
        if g.workspaces.values().any(|w| w.slug == ws.slug) {
            return Err(Error::conflict(format!(
                "workspace slug taken: {}",
                ws.slug
            )));
        }
        // This store honors the caller-supplied id, so the persisted row is
        // identical to the input.
        g.workspaces.insert(ws.id, ws.clone());
        Ok(ws.clone())
    }

    async fn get_workspace(&self, id: WorkspaceId) -> Result<Option<Workspace>> {
        Ok(self.inner.lock().unwrap().workspaces.get(&id).cloned())
    }

    async fn get_workspace_by_slug(&self, slug: &str) -> Result<Option<Workspace>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .workspaces
            .values()
            .find(|w| w.slug == slug)
            .cloned())
    }

    async fn create_user(&self, user: &User) -> Result<User> {
        let mut g = self.inner.lock().unwrap();
        if g.users.values().any(|u| u.email == user.email) {
            return Err(Error::conflict(format!("email taken: {}", user.email)));
        }
        g.users.insert(user.id, user.clone());
        Ok(user.clone())
    }

    async fn get_user(&self, id: UserId) -> Result<Option<User>> {
        Ok(self.inner.lock().unwrap().users.get(&id).cloned())
    }

    async fn get_user_by_email(&self, email: &str) -> Result<Option<User>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .users
            .values()
            .find(|u| u.email == email)
            .cloned())
    }

    async fn get_user_by_email_ci(&self, email: &str) -> Result<Option<User>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .users
            .values()
            .find(|u| u.email.eq_ignore_ascii_case(email))
            .cloned())
    }

    async fn get_user_by_sso(&self, subject: &Subject) -> Result<Option<User>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .users
            .values()
            .find(|u| u.sso_subject.as_ref() == Some(subject))
            .cloned())
    }

    async fn bind_sso_subject(&self, user_id: UserId, subject: &Subject) -> Result<User> {
        let mut g = self.inner.lock().unwrap();
        // Reject binding a subject already owned by a *different* user (mirrors the
        // Postgres unique `(sso_issuer, sso_subject)` index — never re-point a subject).
        if g.users
            .values()
            .any(|u| u.id != user_id && u.sso_subject.as_ref() == Some(subject))
        {
            return Err(Error::conflict("sso subject already bound to another user"));
        }
        let user = g.users.get_mut(&user_id).ok_or(Error::NotFound)?;
        user.sso_subject = Some(subject.clone());
        Ok(user.clone())
    }

    async fn upsert_membership(&self, m: &Membership) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .memberships
            .insert((m.workspace_id, m.user_id), m.clone());
        Ok(())
    }

    async fn get_membership(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<Option<Membership>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .memberships
            .get(&(workspace_id, user_id))
            .cloned())
    }

    async fn create_session(&self, s: &Session) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .insert(s.token.clone(), s.clone());
        Ok(())
    }

    async fn get_session(&self, token: &str) -> Result<Option<Session>> {
        Ok(self.inner.lock().unwrap().sessions.get(token).cloned())
    }

    async fn delete_session(&self, token: &str) -> Result<bool> {
        Ok(self.inner.lock().unwrap().sessions.remove(token).is_some())
    }

    async fn purge_expired_sessions(&self, now: DateTime<Utc>) -> Result<u64> {
        let mut g = self.inner.lock().unwrap();
        let before = g.sessions.len();
        g.sessions.retain(|_, s| s.expires_at > now);
        Ok((before - g.sessions.len()) as u64)
    }

    async fn create_login_token(&self, t: &LoginToken) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .login_tokens
            .insert(t.token.clone(), t.clone());
        Ok(())
    }

    async fn get_login_token(&self, token: &str) -> Result<Option<LoginToken>> {
        Ok(self.inner.lock().unwrap().login_tokens.get(token).cloned())
    }

    async fn consume_login_token(&self, token: &str, now: DateTime<Utc>) -> Result<LoginToken> {
        let mut g = self.inner.lock().unwrap();
        let row = g.login_tokens.get_mut(token).ok_or(Error::NotFound)?;
        if row.consumed_at.is_some() {
            return Err(Error::TokenConsumed);
        }
        let snapshot = row.clone();
        row.consumed_at = Some(now);
        Ok(snapshot)
    }
}
