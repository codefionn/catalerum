//! Dev magic-link login (SOUL §17, §18).
//!
//! The zero-config dev login: first run seeds an admin + default workspace and
//! prints a **login URL with a generated one-time token**; open it → logged in.
//! [`MagicLink`] is what [`IamService::ensure_dev_login`](crate::IamService::ensure_dev_login)
//! and [`issue_login_token`](crate::IamService::issue_login_token) return — it
//! carries the token, its binding, expiry, and the ready-to-print URL.

use chrono::{DateTime, Utc};

use catalerum_core::{UserId, WorkspaceId};

/// The path the magic-link login URL points at (handled by the API, SOUL §18).
pub const MAGIC_PATH: &str = "/auth/magic";

/// A minted one-time login link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MagicLink {
    /// The opaque one-time token (also embedded in [`url`](Self::url)).
    pub token: String,
    /// The user the token logs in as.
    pub user_id: UserId,
    /// The workspace the resulting session is scoped to.
    pub workspace_id: WorkspaceId,
    /// When the token stops being redeemable.
    pub expires_at: DateTime<Utc>,
    /// The ready-to-open login URL, e.g.
    /// `http://localhost:8787/auth/magic?token=…`.
    pub url: String,
}

impl MagicLink {
    /// Render a login URL from a base URL and a token, e.g.
    /// `http://localhost:8787/auth/magic?token=<token>`.
    ///
    /// The base URL's trailing slash (if any) is trimmed. The token is assumed
    /// URL-safe (it is — see [`crate::token`]).
    #[must_use]
    pub fn render_url(base_url: &str, token: &str) -> String {
        let base = base_url.trim_end_matches('/');
        format!("{base}{MAGIC_PATH}?token={token}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_expected_url() {
        assert_eq!(
            MagicLink::render_url("http://localhost:8787", "abc"),
            "http://localhost:8787/auth/magic?token=abc"
        );
        // Trailing slash trimmed.
        assert_eq!(
            MagicLink::render_url("http://localhost:8787/", "abc"),
            "http://localhost:8787/auth/magic?token=abc"
        );
    }
}
