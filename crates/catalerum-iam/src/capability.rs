//! Baseline capability / grant resolution (SOUL §19).
//!
//! A workspace [`Role`] sets a *base capability set*; named [`Grant`]s attenuate
//! within it (SOUL §18). This module is the **baseline stub** the API enforces
//! against in M1/M4: it maps a role to the capabilities that role implicitly
//! holds, and re-exports the core matcher ([`allows`]) so callers have one
//! authorization entry point.
//!
//! The full policy engine (per-action approval, env/rate/cost constraints,
//! revocation fan-out via Valkey) layers on later; the *contract* — deny by
//! default, a held capability must `cover` the request — is fixed by
//! `catalerum-core::capability`.

use catalerum_core::capability::{allows as core_allows, Action, Capability, Resource};
use catalerum_core::model::{Grant, Role};
use catalerum_core::{GrantId, WorkspaceId};

/// The resource domains a workspace member can touch through the tool/API
/// surface (SOUL §7, §19). Destructive (`delete`), host-exec, and broad MCP
/// exposure are **never** implied by a role — they require an explicit grant
/// capability (protected scopes, SOUL §19).
///
/// Public so a surface that spans the whole workspace and cannot be gated
/// per-resource (e.g. the `GET /mcp` push stream) can require read authority
/// over every standard domain instead of inventing its own list.
pub const STANDARD_DOMAINS: &[&str] = &[
    "calendar",
    "storage",
    "email",
    "notes",
    "tasks",
    // Relationships between objects (SOUL §5/§6.3): read = list, write =
    // create/delete. The `RELATES_TO` projection is derived from these rows.
    "links",
    "graph",
    "vector",
    "skill",
    "memory",
    "profile",
    "channel",
    "agent",
    "automation",
    "conversation",
    // Web egress (SOUL §11/§27): `web:read` authorizes the `fetch_url` tool and
    // `POST /fetch`; `web:write` authorizes outbound webhook delivery (the
    // `send_webhook` tool / `Webhook` automation action). The SSRF guard is
    // enforced in the fetch/webhook backend, independently of this capability.
    "web",
    // Emerged UIs (AI-authored declarative UIs): read = view/run, write =
    // create/patch/delete. Firing a handler is gated on `ui:read`; the handler's
    // own side effects are gated on the underlying tool's capability + the
    // server allow-list (the "emerged UI" feature, SOUL §5/§12/§19).
    "ui",
    // External PostgreSQL databases the workspace owns (SOUL §11/§19): read =
    // SELECT (`db:read@conn`), write = DML + schema migrations (`db:write@conn`,
    // `db:write@conn/schema`), and connection management. A narrow agent grant can
    // scope to a single connection (`db:read@conn`) or exclude schema changes by
    // holding `db:write@conn` without `db:write@conn/schema`.
    "db",
];

/// The base capabilities implied by a workspace [`Role`] (SOUL §18).
///
/// - **Owner / Admin** → full authority within the workspace: `*` on every
///   resource (a single wildcard capability). Owner/Admin can mint grants;
///   protected scopes still require explicit constraint inclusion at mint time.
/// - **Member** → read + write + use/query/search across the standard domains,
///   but **not** delete and **not** host exec / MCP-expose.
/// - **Viewer** → read-only (plus graph query / vector search, which are reads).
///
/// These are the capabilities a grant may be attenuated *from* for a user of
/// that role (the attenuation invariant, SOUL §19): a user can never confer
/// more than their role's base set.
#[must_use]
pub fn base_capabilities(role: Role) -> Vec<Capability> {
    match role {
        // Full authority: one wildcard capability covers every action/resource.
        Role::Owner | Role::Admin => vec![Capability::new(Action::Any, Resource::any())],

        // Read + write + the non-destructive verbs, per domain.
        Role::Member => STANDARD_DOMAINS
            .iter()
            .flat_map(|d| {
                [
                    Capability::new(Action::Read, Resource::domain(*d)),
                    Capability::new(Action::Write, Resource::domain(*d)),
                ]
            })
            .chain([
                Capability::new(Action::Query, Resource::domain("graph")),
                Capability::new(Action::Search, Resource::domain("vector")),
                Capability::new(Action::Search, Resource::domain("web")),
                Capability::new(Action::Use, Resource::domain("skill")),
            ])
            .collect(),

        // Read-only, plus graph query / vector search / web search (all reads).
        Role::Viewer => STANDARD_DOMAINS
            .iter()
            .map(|d| Capability::new(Action::Read, Resource::domain(*d)))
            .chain([
                Capability::new(Action::Query, Resource::domain("graph")),
                Capability::new(Action::Search, Resource::domain("vector")),
                Capability::new(Action::Search, Resource::domain("web")),
            ])
            .collect(),
    }
}

/// Build a synthetic [`Grant`] representing a role's full base authority, for a
/// given workspace.
///
/// Useful where the enforcement path wants a `Grant` value (e.g. web/channel
/// chat that "runs under the user's grant", SOUL §19) but the principal has no
/// explicit named grant — the role's base set *is* their authority.
#[must_use]
pub fn role_grant(workspace_id: WorkspaceId, role: Role) -> Grant {
    Grant {
        id: GrantId::nil(),
        workspace_id,
        name: format!("role:{}", role_str(role)),
        capabilities: base_capabilities(role),
        constraints: Default::default(),
    }
}

/// Does a [`Role`] alone (its base capability set) authorize `requested`?
///
/// Deny-by-default: returns `true` only if some base capability for the role
/// [covers](Capability::covers) the request. This is the cheap pre-check before
/// consulting an explicit grant.
#[must_use]
pub fn role_allows(role: Role, requested: &Capability) -> bool {
    base_capabilities(role)
        .iter()
        .any(|held| held.covers(requested))
}

/// Re-export of the core grant matcher (SOUL §19): does `grant` authorize
/// `requested`? Deny-by-default; constraint enforcement layers on at the API.
#[must_use]
pub fn allows(grant: &Grant, requested: &Capability) -> bool {
    core_allows(grant, requested)
}

/// The canonical lowercase string for a role (matches the DB encoding and the
/// `serde(rename_all = "snake_case")` form).
#[must_use]
pub fn role_str(role: Role) -> &'static str {
    match role {
        Role::Owner => "owner",
        Role::Admin => "admin",
        Role::Member => "member",
        Role::Viewer => "viewer",
    }
}

/// Is this workspace [`Role`] a workspace **administrator** (Owner or Admin)?
///
/// Owner/Admin hold full workspace authority (§18); Member/Viewer do not. This
/// gates **workspace-operational config writes** — registering/removing the
/// external DB + storage connections a whole workspace's tools then use — which a
/// plain Member must not perform. It is the workspace-role analogue of
/// [`is_org_admin`](crate::is_org_admin), enforced independent of the deployment
/// `mode` (which is presentation only, SOUL §18/§29).
#[must_use]
pub fn is_admin(role: Role) -> bool {
    matches!(role, Role::Owner | Role::Admin)
}

/// Parse a role from its canonical lowercase string (inverse of [`role_str`]).
///
/// # Errors
/// Returns [`Error::Invalid`](crate::Error::Invalid) for an unknown role.
pub fn role_from_str(s: &str) -> crate::Result<Role> {
    match s {
        "owner" => Ok(Role::Owner),
        "admin" => Ok(Role::Admin),
        "member" => Ok(Role::Member),
        "viewer" => Ok(Role::Viewer),
        other => Err(crate::Error::invalid(format!("unknown role: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_can_do_anything() {
        let cap = Capability::new(Action::Delete, Resource::new("storage", "prod/db"));
        assert!(role_allows(Role::Owner, &cap));
        assert!(role_allows(Role::Admin, &cap));
    }

    #[test]
    fn member_writes_but_does_not_delete() {
        let write = Capability::new(Action::Write, Resource::domain("notes"));
        let delete = Capability::new(Action::Delete, Resource::domain("notes"));
        assert!(role_allows(Role::Member, &write));
        assert!(!role_allows(Role::Member, &delete));
    }

    #[test]
    fn viewer_is_read_only() {
        let read = Capability::new(Action::Read, Resource::domain("calendar"));
        let write = Capability::new(Action::Write, Resource::domain("calendar"));
        assert!(role_allows(Role::Viewer, &read));
        assert!(!role_allows(Role::Viewer, &write));
        // graph query / vector search are reads viewers keep.
        assert!(role_allows(
            Role::Viewer,
            &Capability::new(Action::Query, Resource::domain("graph"))
        ));
    }

    #[test]
    fn web_read_is_held_by_every_role() {
        // `POST /fetch` + the `fetch_url` tool gate on `web:read` (SOUL §27/§19):
        // a baseline read every role holds (so the deny-by-default gate is real
        // yet open-to-all today, narrowable by an explicit grant); there is no
        // web write, so even a Member is not granted one.
        let read = Capability::new(Action::Read, Resource::domain("web"));
        assert!(role_allows(Role::Viewer, &read));
        assert!(role_allows(Role::Member, &read));
        assert!(role_allows(Role::Owner, &read));
        assert!(role_allows(Role::Admin, &read));
    }

    #[test]
    fn web_search_is_held_by_every_role() {
        // The `web_search` tool gates on `web:search` (SOUL §27/§19) — its own
        // verb on the `web` domain, mirroring `vector:search`, so search can be
        // denied by an explicit grant independently of `fetch_url`'s `web:read`.
        // Every role holds it today (a baseline read), like `web:read`.
        let search = Capability::new(Action::Search, Resource::domain("web"));
        assert!(role_allows(Role::Viewer, &search));
        assert!(role_allows(Role::Member, &search));
        assert!(role_allows(Role::Owner, &search));
        assert!(role_allows(Role::Admin, &search));
    }

    #[test]
    fn role_grant_round_trips_via_allows() {
        let ws = WorkspaceId::new();
        let grant = role_grant(ws, Role::Member);
        assert!(allows(
            &grant,
            &Capability::new(Action::Write, Resource::domain("tasks"))
        ));
        assert!(!allows(
            &grant,
            &Capability::new(Action::Delete, Resource::domain("tasks"))
        ));
    }

    #[test]
    fn role_string_round_trip() {
        for r in [Role::Owner, Role::Admin, Role::Member, Role::Viewer] {
            assert_eq!(role_from_str(role_str(r)).unwrap(), r);
        }
        assert!(role_from_str("bogus").is_err());
    }

    #[test]
    fn is_admin_is_owner_or_admin_only() {
        // Owner/Admin administer the workspace; Member/Viewer do not (SOUL §18).
        assert!(is_admin(Role::Owner));
        assert!(is_admin(Role::Admin));
        assert!(!is_admin(Role::Member));
        assert!(!is_admin(Role::Viewer));
    }
}
