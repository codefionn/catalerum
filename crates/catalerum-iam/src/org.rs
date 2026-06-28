//! Organisation roles + creation-policy gating (SOUL §18).
//!
//! An [`OrgRole`] governs **administration only** — creating/archiving
//! workspaces in the org, managing org members, org-level policy. It confers
//! **no** data access: reading a workspace still requires a workspace membership,
//! and organisations never appear in capability strings (§19). This module is the
//! pure, deny-by-default policy surface the API enforces against; the actual
//! membership lookups live in the store repos.

use catalerum_core::model::{CreationPolicy, OrgRole};

/// The canonical lowercase string for an [`OrgRole`] (matches the DB encoding and
/// the `serde(rename_all = "snake_case")` form).
#[must_use]
pub fn org_role_str(role: OrgRole) -> &'static str {
    match role {
        OrgRole::Owner => "owner",
        OrgRole::Admin => "admin",
        OrgRole::Member => "member",
    }
}

/// Parse an [`OrgRole`] from its canonical lowercase string (inverse of
/// [`org_role_str`]).
///
/// # Errors
/// Returns [`Error::Invalid`](crate::Error::Invalid) for an unknown role.
pub fn org_role_from_str(s: &str) -> crate::Result<OrgRole> {
    match s {
        "owner" => Ok(OrgRole::Owner),
        "admin" => Ok(OrgRole::Admin),
        "member" => Ok(OrgRole::Member),
        other => Err(crate::Error::invalid(format!("unknown org role: {other}"))),
    }
}

/// Is this org role an **owner**?
#[must_use]
pub fn is_org_owner(role: OrgRole) -> bool {
    matches!(role, OrgRole::Owner)
}

/// Is this org role an **admin** (owner or admin) — the administrative gate for
/// managing org members, org policy, and archiving workspaces (SOUL §18)?
#[must_use]
pub fn is_org_admin(role: OrgRole) -> bool {
    matches!(role, OrgRole::Owner | OrgRole::Admin)
}

/// Does an optional org membership confer **admin** (owner/admin) standing? A
/// non-member (`None`) never does — deny-by-default.
#[must_use]
pub fn org_admin_or_owner(role: Option<OrgRole>) -> bool {
    matches!(role, Some(OrgRole::Owner | OrgRole::Admin))
}

/// Does an optional org membership confer **any** member standing? Any org role
/// (owner/admin/member) is a member; a non-member (`None`) is not.
#[must_use]
pub fn org_any_member(role: Option<OrgRole>) -> bool {
    role.is_some()
}

/// Deny-by-default gate (SOUL §18): may a caller holding `caller_org_role` in the
/// organisation create a workspace, given the org's `workspace_creation` policy?
///
/// - `Disabled` → never.
/// - `Admins` → only an org owner/admin.
/// - `Members` → any org member.
///
/// A non-member (`None`) is always denied, whatever the policy.
#[must_use]
pub fn workspace_creation_allowed(
    policy: CreationPolicy,
    caller_org_role: Option<OrgRole>,
) -> bool {
    match policy {
        CreationPolicy::Disabled => false,
        CreationPolicy::Admins => org_admin_or_owner(caller_org_role),
        CreationPolicy::Members => org_any_member(caller_org_role),
    }
}

/// Deny-by-default gate (SOUL §18): may an authenticated user create a **new**
/// organisation, given the instance `organisation_creation` policy?
///
/// - `Disabled` → never (nobody may create orgs via the API).
/// - `Members` → any authenticated user (they become the new org's Owner).
/// - `Admins` → only a user who already owns/admins at least one organisation
///   (`caller_is_org_admin_somewhere`).
#[must_use]
pub fn organisation_creation_allowed(
    policy: CreationPolicy,
    caller_is_org_admin_somewhere: bool,
) -> bool {
    match policy {
        CreationPolicy::Disabled => false,
        CreationPolicy::Members => true,
        CreationPolicy::Admins => caller_is_org_admin_somewhere,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_role_string_round_trip() {
        for r in [OrgRole::Owner, OrgRole::Admin, OrgRole::Member] {
            assert_eq!(org_role_from_str(org_role_str(r)).unwrap(), r);
        }
        assert!(org_role_from_str("viewer").is_err());
    }

    #[test]
    fn admin_gate_is_owner_or_admin() {
        assert!(is_org_admin(OrgRole::Owner));
        assert!(is_org_admin(OrgRole::Admin));
        assert!(!is_org_admin(OrgRole::Member));
        assert!(is_org_owner(OrgRole::Owner));
        assert!(!is_org_owner(OrgRole::Admin));
    }

    #[test]
    fn optional_gates_deny_non_members() {
        assert!(org_admin_or_owner(Some(OrgRole::Admin)));
        assert!(!org_admin_or_owner(Some(OrgRole::Member)));
        assert!(!org_admin_or_owner(None));
        assert!(org_any_member(Some(OrgRole::Member)));
        assert!(!org_any_member(None));
    }

    #[test]
    fn workspace_creation_is_deny_by_default() {
        // Disabled: nobody, not even an owner.
        assert!(!workspace_creation_allowed(
            CreationPolicy::Disabled,
            Some(OrgRole::Owner)
        ));
        // Admins: owner/admin yes, member no, non-member no.
        assert!(workspace_creation_allowed(
            CreationPolicy::Admins,
            Some(OrgRole::Owner)
        ));
        assert!(workspace_creation_allowed(
            CreationPolicy::Admins,
            Some(OrgRole::Admin)
        ));
        assert!(!workspace_creation_allowed(
            CreationPolicy::Admins,
            Some(OrgRole::Member)
        ));
        assert!(!workspace_creation_allowed(CreationPolicy::Admins, None));
        // Members: any member yes, non-member no.
        assert!(workspace_creation_allowed(
            CreationPolicy::Members,
            Some(OrgRole::Member)
        ));
        assert!(!workspace_creation_allowed(CreationPolicy::Members, None));
    }

    #[test]
    fn organisation_creation_is_deny_by_default() {
        // Disabled: nobody.
        assert!(!organisation_creation_allowed(
            CreationPolicy::Disabled,
            true
        ));
        // Members: any authenticated user.
        assert!(organisation_creation_allowed(
            CreationPolicy::Members,
            false
        ));
        // Admins: only a user who administers some org.
        assert!(organisation_creation_allowed(CreationPolicy::Admins, true));
        assert!(!organisation_creation_allowed(
            CreationPolicy::Admins,
            false
        ));
    }
}
