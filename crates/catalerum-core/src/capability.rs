//! The capability model (SOUL §19).
//!
//! Authority is **capability-scoped, attenuating, and deny-by-default**. A
//! [`Capability`] is `(action, resource-selector, constraints)`; a
//! [`Grant`](crate::model::Grant) bundles capabilities plus global
//! [`Constraints`]. Nothing runs without a matching capability, and a grant is
//! minted only from capabilities the creator already holds, equal-or-narrower
//! (the attenuation invariant).
//!
//! This module ships a *real but minimal* matcher: [`allows`] answers "does a
//! held capability authorize a requested one?" and [`attenuate`] checks/derives
//! a narrowed capability. The full policy engine lives in `catalerum-iam`; the
//! types and the matching contract are fixed here so every crate agrees.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// The verb of a capability, split read/write/delete (SOUL §19). `Use`/`Expose`
/// cover skills and MCP exposure; [`Action::Any`] (`*`) is a wildcard that
/// subsumes every other action on a resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Wildcard — authorizes any action on the matched resource.
    Any,
    Read,
    Write,
    Delete,
    /// Invoke a skill (`skill:use@…`).
    Use,
    /// Run code/commands (`exec:run@…`).
    Run,
    /// Query the graph (`graph:query`).
    Query,
    /// Semantic search (`vector:search`).
    Search,
    /// Expose over MCP (`mcp:expose@…`).
    Expose,
}

impl Action {
    /// Does `self` (a *held* action) cover `requested`? [`Action::Any`] covers
    /// everything; otherwise actions must be equal.
    #[must_use]
    pub fn covers(self, requested: Action) -> bool {
        self == Action::Any || self == requested
    }
}

/// The resource a capability applies to: a domain (`calendar`, `storage`,
/// `notes`, `tasks`, `graph`, `vector`, `skill`, `exec`, `mcp`, …) and an
/// optional glob `selector` within it (e.g. `storage:write@local/out/*`).
///
/// `Resource { domain: "*", selector: None }` matches every resource and is
/// used for owner-level grants — but note that protected scopes (SOUL §19) are
/// gated by [`Constraints`], not by the selector alone.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Resource {
    /// The resource domain, e.g. `calendar`, `storage`, `notes`, `exec`.
    pub domain: String,
    /// Optional `@`-suffix selector. `*` globs are supported: a trailing `*` (any
    /// remainder), a whole `*` segment, and `*` **within** a segment — e.g. the
    /// host-glob `*.wikipedia.org` or a path-glob `logs/*.log` (SOUL §19). `None`
    /// means "the whole domain".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
}

impl Resource {
    /// Construct a resource covering an entire domain.
    #[must_use]
    pub fn domain(domain: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            selector: None,
        }
    }

    /// Construct a resource with a selector glob.
    #[must_use]
    pub fn new(domain: impl Into<String>, selector: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            selector: Some(selector.into()),
        }
    }

    /// The wildcard resource (`*`) matching every domain/selector.
    #[must_use]
    pub fn any() -> Self {
        Self {
            domain: "*".to_string(),
            selector: None,
        }
    }

    /// Does `self` (a *held* resource) cover `requested`?
    ///
    /// The domain must match (or `self.domain == "*"`), and the selector glob
    /// must cover the requested selector. A held selector of `None` covers any
    /// requested selector within the same domain.
    #[must_use]
    pub fn covers(&self, requested: &Resource) -> bool {
        if self.domain != "*" && self.domain != requested.domain {
            return false;
        }
        match (&self.selector, &requested.selector) {
            // We hold the whole domain → covers any sub-selector.
            (None, _) => true,
            // We hold a specific selector but the request names none → no.
            (Some(_), None) => false,
            (Some(held), Some(req)) => glob_covers(held, req),
        }
    }
}

/// Glob match over `/`-split segments. A whole `*` segment matches one segment; a
/// trailing `/*` (or bare `*`) matches any remainder. **Within** a segment, `*`
/// matches any run of characters — so a host-glob `*.wikipedia.org` covers
/// `en.wikipedia.org` (and any deeper sub-subdomain), but **not** the apex
/// `wikipedia.org` nor `wikipedia.org.evil.com` (the literal `.wikipedia.org` suffix
/// must match the segment's end), and a path-glob `logs/*.log` covers
/// `logs/app.log` (SOUL §19 selectors). A segment without `*` is an exact match.
fn glob_covers(pattern: &str, value: &str) -> bool {
    // Whole-string wildcard.
    if pattern == "*" {
        return true;
    }
    let pat: Vec<&str> = pattern.split('/').collect();
    let val: Vec<&str> = value.split('/').collect();

    let mut i = 0;
    while i < pat.len() {
        match pat[i] {
            // Trailing wildcard segment → matches the rest.
            "*" if i + 1 == pat.len() => return true,
            // Wildcard single segment.
            "*" => {
                if i >= val.len() {
                    return false;
                }
            }
            // A literal-or-intra-segment-globbed segment must match positionally.
            seg => {
                if i >= val.len() || !segment_matches(seg, val[i]) {
                    return false;
                }
            }
        }
        i += 1;
    }
    // All pattern segments consumed → must have consumed all value segments.
    pat.len() == val.len()
}

/// Within-segment glob: `*` matches any run (including empty) of characters within
/// one segment. The standard two-pointer wildcard algorithm (no `?`), char-wise so
/// a `*` boundary never splits a multi-byte char. A `*`-free pattern is an exact
/// match. The literal characters around each `*` are anchored, so `*.wikipedia.org`
/// requires the value to **end** with `.wikipedia.org` (never matches a lookalike
/// like `wikipedia.org.evil.com`).
fn segment_matches(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let (mut pi, mut vi) = (0usize, 0usize);
    // The last `*` seen and the value position it started matching from, for
    // backtracking when a literal run after a `*` fails further along.
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while vi < v.len() {
        if pi < p.len() && p[pi] == v[vi] {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = vi;
            pi += 1;
        } else if let Some(sp) = star {
            // Let the last `*` consume one more value char, retry the rest.
            pi = sp + 1;
            mark += 1;
            vi = mark;
        } else {
            return false;
        }
    }
    // Any pattern tail must be all `*` (each matching empty) to fully match.
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// A single capability: `(action, resource, constraints)` (SOUL §19).
///
/// Per-capability `constraints` are an open JSON map for resource-specific keys
/// (e.g. `exec:run@bao{lang=python, net=none, cpu=1}` → `{"lang":"python",
/// "net":"none","cpu":1}`).
///
/// `Eq` is not derived because the JSON constraint values may carry floats.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    pub action: Action,
    pub resource: Resource,
    /// Resource-specific constraints (lang/net/cpu/env/…).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub constraints: BTreeMap<String, Json>,
}

impl Capability {
    /// Construct a capability with no per-capability constraints.
    #[must_use]
    pub fn new(action: Action, resource: Resource) -> Self {
        Self {
            action,
            resource,
            constraints: BTreeMap::new(),
        }
    }

    /// Does this *held* capability cover `requested` on action + resource?
    ///
    /// Note: per-capability constraints are *not* re-checked here — they
    /// describe the held authority's limits and are validated at the call site
    /// (the API choke point, SOUL §19) against the concrete request. The
    /// action/resource subsumption is the structural gate.
    ///
    /// This is the **request-authorization** contract: a concrete request
    /// generally carries no constraints, so re-checking them here would make
    /// every request fail closed against a constrained held capability. Grant
    /// *minting* wants the stricter [`covers_for_attenuation`](Self::covers_for_attenuation),
    /// which additionally requires the child's constraints to be ⊆ the parent's.
    #[must_use]
    pub fn covers(&self, requested: &Capability) -> bool {
        self.action.covers(requested.action) && self.resource.covers(&requested.resource)
    }

    /// The **mint-time** ⊇ check: does this *parent* capability cover `child` on
    /// action + resource **and** hold per-capability constraints that `child`
    /// keeps at least as tight? Used only when deriving/minting a grant, never
    /// for request authorization (see [`covers`](Self::covers)).
    ///
    /// Equivalent to `self.covers(child) && constraints_subsume(self, child)`.
    /// Both halves are checked on the *same* parent capability, because a
    /// capability's constraints only bound its own resource scope — a different
    /// parent cap's constraints can't be borrowed to justify this child.
    #[must_use]
    pub fn covers_for_attenuation(&self, child: &Capability) -> bool {
        self.covers(child) && constraints_subsume(self, child)
    }
}

/// Global constraints attached to a [`Grant`](crate::model::Grant) (SOUL §19):
/// env allow-list, rate/cost/resource caps, time window, dry-run, and per-action
/// approval requirements.
///
/// `Eq` is not derived because `cost_limit` is a float.
///
/// `deny_unknown_fields`: an unrecognized constraint key (DB/schema skew, a grant
/// written by a newer binary, an out-of-band JSONB write) **fails the deserialize**
/// rather than silently vanishing — so it surfaces as a grant-load error and the
/// run fails closed, never runs with a constraint the runtime didn't even see.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constraints {
    /// Environments this grant may touch (e.g. `["dev"]`). `prod` is only ever
    /// present if explicitly added (protected scope, SOUL §19).
    #[serde(default)]
    pub env_allow: Vec<String>,
    /// Maximum number of actions per run/window, if capped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<u32>,
    /// Cost ceiling (in provider-defined units), if capped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_limit: Option<f64>,
    /// Optional active time window (inclusive start, exclusive end), RFC3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_window: Option<TimeWindow>,
    /// When true, actions are simulated, never committed.
    #[serde(default)]
    pub dry_run: bool,
    /// Actions (by `domain:action` string) that require human approval before
    /// committing (SOUL §19 protected scopes).
    #[serde(default)]
    pub requires_approval: Vec<String>,
}

impl Constraints {
    /// Whether any constraint the runtime **cannot yet enforce** is set. The
    /// enforced ones are excluded: [`dry_run`](Self::dry_run) (simulated at dispatch),
    /// [`time_window`](Self::time_window) (checked at run open), and
    /// [`cost_limit`](Self::cost_limit) (a per-run ceiling the agent loop enforces
    /// against `Usage.cost_usd`, SOUL §7/§19 — a non-LLM action has no spend, so the
    /// cap is trivially satisfied). The remainder — `env_allow` / `rate_limit` /
    /// `requires_approval` — await the full policy engine (SOUL §19); a grant
    /// carrying any of them must fail closed rather than run with the guardrail dropped.
    #[must_use]
    pub fn has_unenforced(&self) -> bool {
        // Destructure (no `..`) so ADDING a field to `Constraints` is a COMPILE
        // error here until it's explicitly classified enforced/unenforced — the
        // fail-closed guarantee can't silently regress to fail-open on a future
        // constraint a developer forgot to wire in.
        let Self {
            env_allow,
            rate_limit,
            cost_limit: _,  // ENFORCED in the agent loop (per-run cost ceiling, §7/§19)
            time_window: _, // ENFORCED at run open (TimeWindow::contains)
            dry_run: _,     // ENFORCED at dispatch (simulated)
            requires_approval,
        } = self;
        !env_allow.is_empty() || rate_limit.is_some() || !requires_approval.is_empty()
    }
}

/// An inclusive-start / exclusive-end activation window for a grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeWindow {
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
}

impl TimeWindow {
    /// Whether `now` falls within the window — inclusive `start`, exclusive `end`.
    /// An empty or inverted window (`start >= end`) contains no instant, so a grant
    /// so constrained is never active (deny-safe).
    #[must_use]
    pub fn contains(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.start <= now && now < self.end
    }
}

// ---------------------------------------------------------------------------
// Matcher / attenuation skeleton
// ---------------------------------------------------------------------------

/// Does a [`Grant`](crate::model::Grant) authorize the `requested` capability?
///
/// Returns `true` iff **some** held capability [covers](Capability::covers) the
/// request. This is the deny-by-default gate (SOUL §19): no covering capability
/// → denied. Constraint enforcement (env, rate, approval, dry-run) layers on at
/// the API choke point.
#[must_use]
pub fn allows(grant: &crate::model::Grant, requested: &Capability) -> bool {
    grant.capabilities.iter().any(|held| held.covers(requested))
}

/// Attenuation-time constraint subsumption (SOUL §19): are the `child`
/// capability's per-capability [`constraints`](Capability::constraints) **at
/// least as restrictive** as the `parent`'s?
///
/// This is deliberately *separate* from [`Capability::covers`], which ignores
/// constraints so request authorization never fails closed on a held cap's
/// limits. Constraint values are open JSON with per-key semantics not yet
/// defined (`lang`/`net`/`cpu`/`env`/…), so the rule is **conservative and
/// sound** — it accepts a child only when it is *provably* no looser:
///
/// - **Every key present on `parent` must be present on `child`.** A parent key
///   missing on the child drops a restriction the parent carried → escalation →
///   `false`. (Deny-by-default: an unknown key can't be assumed harmless.)
/// - **Extra keys on the child are fine** — they only *add* restriction the
///   parent didn't impose, which never escalates.
/// - For a **shared** key the child value must [subsume](constraint_value_subsumes)
///   the parent's: equal values subsume; two JSON *arrays* subsume iff the
///   child's is a subset of the parent's (the allow-list convention, e.g.
///   `env`/`env_allow`: fewer allowed values = tighter); any other unequal pair
///   is rejected (we can't prove it narrower).
///
/// A parent capability with no constraints imposes nothing, so any child
/// subsumes it — which is why every role-base mint site (all empty constraints)
/// is unaffected.
#[must_use]
pub fn constraints_subsume(parent: &Capability, child: &Capability) -> bool {
    parent
        .constraints
        .iter()
        .all(|(key, parent_val)| match child.constraints.get(key) {
            // The child dropped a restriction the parent held → escalation.
            None => false,
            Some(child_val) => constraint_value_subsumes(parent_val, child_val),
        })
}

/// Does a single `child` constraint value subsume (is it "at least as
/// restrictive as") the `parent`'s value for the same key?
///
/// - Equal JSON values subsume (identical restriction).
/// - Two JSON **arrays** subsume iff the child is a subset of the parent — the
///   allow-list convention (`env`/`env_allow`-style): the child may only *drop*
///   allowed values, never add one the parent didn't permit.
/// - Every other unequal shape is rejected. Open JSON semantics are per-key and
///   undefined here, so only the provably-narrower shapes above are accepted;
///   the rest fail closed until enforcement defines them.
#[must_use]
fn constraint_value_subsumes(parent: &Json, child: &Json) -> bool {
    if parent == child {
        return true;
    }
    match (parent, child) {
        // Allow-list: child ⊆ parent (child adds no value the parent lacks).
        (Json::Array(parent_items), Json::Array(child_items)) => {
            child_items.iter().all(|item| parent_items.contains(item))
        }
        _ => false,
    }
}

/// The first `parent` constraint key that `child` loosens or drops, if any —
/// used only to name the offending key in an [`AttenuationError`].
fn escalating_constraint_key(parent: &Capability, child: &Capability) -> Option<String> {
    parent
        .constraints
        .iter()
        .find(|(key, parent_val)| match child.constraints.get(*key) {
            Some(child_val) => !constraint_value_subsumes(parent_val, child_val),
            None => true,
        })
        .map(|(key, _)| key.clone())
}

/// The attenuation invariant (SOUL §19): a derived capability is valid only if
/// the `parent` authority covers it — on action, resource, **and** per-capability
/// constraints. Returns the child unchanged when valid, or an
/// [`AttenuationError`] naming why it would escalate.
///
/// This is the structural check a grant-minting path runs for every capability
/// it is about to confer; it guarantees a chat-built agent is provably ⊆ its
/// creator. A child is within bounds iff **some single** parent capability
/// [`covers_for_attenuation`](Capability::covers_for_attenuation) it: that one
/// parent cap must cover its action/resource *and* subsume its constraints,
/// since a cap's constraints only bound its own resource scope (a looser sibling
/// parent cap can't launder a tighter one's constraints).
pub fn attenuate(
    parent: &[Capability],
    child: &Capability,
) -> Result<Capability, AttenuationError> {
    if parent.iter().any(|p| p.covers_for_attenuation(child)) {
        Ok(child.clone())
    } else {
        // Name a constraint escalation precisely: if some parent covered the
        // action/resource but none subsumed the constraints, report the first
        // dropped/loosened key. `None` → a pure action/resource escalation.
        let constraint = parent
            .iter()
            .filter(|p| p.covers(child))
            .find_map(|p| escalating_constraint_key(p, child));
        Err(AttenuationError {
            action: child.action,
            domain: child.resource.domain.clone(),
            selector: child.resource.selector.clone(),
            constraint,
        })
    }
}

/// Returned when [`attenuate`] rejects a capability that would exceed the parent
/// authority.
///
/// `constraint` is `Some(key)` when the escalation is specifically a
/// per-capability constraint the child dropped or loosened (some parent covered
/// its action/resource, but none subsumed its constraints); `None` for a pure
/// action/resource escalation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("capability {action:?}@{domain}{}{} escalates beyond parent authority",
    selector.as_deref().map(|s| format!("/{s}")).unwrap_or_default(),
    constraint.as_deref().map(|k| format!(" (loosens constraint `{k}`)")).unwrap_or_default())]
pub struct AttenuationError {
    pub action: Action,
    pub domain: String,
    pub selector: Option<String>,
    /// The per-capability constraint key the child loosened/dropped, if the
    /// rejection was a constraint escalation rather than an action/resource one.
    pub constraint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_covers("*", "anything/at/all"));
        assert!(glob_covers("local/out/*", "local/out/report.pdf"));
        assert!(glob_covers("local/*/x", "local/a/x"));
        assert!(!glob_covers("local/out/*", "local/in/report.pdf"));
        assert!(!glob_covers("local/out", "local/out/report.pdf"));
        assert!(glob_covers("local/out", "local/out"));
    }

    // Intra-segment `*` wildcards (SOUL §19 host-globs + path-globs). Security-
    // critical: the literal characters around a `*` are anchored, so a host-glob can
    // never match a lookalike suffix — locked in so the matcher can't silently widen.
    #[test]
    fn glob_intra_segment_wildcards_and_host_globs() {
        // `*.wikipedia.org` covers any subdomain, including deeper ones…
        assert!(glob_covers("*.wikipedia.org", "en.wikipedia.org"));
        assert!(glob_covers("*.wikipedia.org", "a.b.wikipedia.org"));
        // …but NOT the apex, NOT a lookalike-suffix attacker domain, NOT a near-miss.
        assert!(!glob_covers("*.wikipedia.org", "wikipedia.org"));
        assert!(!glob_covers("*.wikipedia.org", "wikipedia.org.evil.com"));
        assert!(!glob_covers("*.wikipedia.org", "enwikipedia.org"));
        assert!(!glob_covers("*.wikipedia.org", "en.wikipedia.com"));
        // Intra-segment `*` is confined to one segment — it never crosses `/`.
        assert!(glob_covers("logs/*.log", "logs/app.log"));
        assert!(!glob_covers("logs/*.log", "logs/sub/app.log"));
        assert!(!glob_covers("logs/*.log", "logs/app.txt"));
        // Leading / trailing / middle `*`, and `*` matching the empty string.
        assert!(glob_covers("report-*", "report-2026"));
        assert!(glob_covers("*-final", "draft-final"));
        assert!(glob_covers("a*z", "abcz"));
        assert!(glob_covers("a*z", "az"));
        assert!(!glob_covers("a*z", "abc"));
        // A `*`-free segment stays an exact match (no behaviour change).
        assert!(!glob_covers("local/out", "local/output"));
        // The host-glob also works through the full `Resource::covers` path.
        assert!(Resource::new("web", "*.wikipedia.org")
            .covers(&Resource::new("web", "en.wikipedia.org")));
        assert!(!Resource::new("web", "*.wikipedia.org")
            .covers(&Resource::new("web", "wikipedia.org.evil.com")));
    }

    // Edge cases for the authorization glob — locked in so a future change to the
    // matcher can't silently widen or narrow what a grant covers.
    #[test]
    fn glob_edge_cases() {
        // A trailing `*` matches *any* remainder, including multiple segments.
        assert!(glob_covers("local/*", "local/a/b/c"));
        assert!(glob_covers("a/b/*", "a/b/c/d/e"));
        // A non-trailing `*` requires that segment to be present in the value —
        // it cannot match "off the end".
        assert!(!glob_covers("local/*/x", "local/x")); // value too short for `*` + `x`
        assert!(!glob_covers("a/*/c", "a/b")); // missing the `c` segment
                                               // A held selector longer/more specific than the value never covers it.
        assert!(!glob_covers("a/b/c", "a/b"));
        assert!(!glob_covers("a/b", "a")); // holding `a/b` does not cover the parent `a`
                                           // A literal selector covers only itself, not its children.
        assert!(!glob_covers("a", "a/b"));
        assert!(glob_covers("a", "a"));
        // KNOWN LOOSE EDGE (documented, deny-by-default's one soft spot): a trailing
        // `*` also covers the bare *prefix* object, not just its children — so a
        // subtree grant `reports/*` covers an object literally keyed `reports`.
        // Asserted here so the behavior is an explicit, tested decision; tighten by
        // requiring `i < val.len()` on the trailing-`*` arm if ever desired.
        assert!(glob_covers("reports/*", "reports"));
        assert!(Resource::new("storage", "reports/*").covers(&Resource::new("storage", "reports")));
    }

    #[test]
    fn resource_coverage() {
        let held = Resource::new("storage", "local/out/*");
        assert!(held.covers(&Resource::new("storage", "local/out/a.txt")));
        assert!(!held.covers(&Resource::new("storage", "local/in/a.txt")));
        assert!(!held.covers(&Resource::new("calendar", "local/out/a.txt")));

        let whole = Resource::domain("notes");
        assert!(whole.covers(&Resource::new("notes", "personal")));
        assert!(whole.covers(&Resource::domain("notes")));

        assert!(Resource::any().covers(&Resource::new("exec", "bao")));
    }

    #[test]
    fn action_coverage() {
        assert!(Action::Any.covers(Action::Delete));
        assert!(Action::Read.covers(Action::Read));
        assert!(!Action::Read.covers(Action::Write));
    }

    #[test]
    fn attenuation_rejects_escalation() {
        let parent = vec![Capability::new(
            Action::Read,
            Resource::new("storage", "local/*"),
        )];
        let ok = Capability::new(Action::Read, Resource::new("storage", "local/out/a"));
        assert!(attenuate(&parent, &ok).is_ok());

        let escalate = Capability::new(Action::Write, Resource::new("storage", "local/out/a"));
        assert!(attenuate(&parent, &escalate).is_err());
    }

    /// Build a capability carrying per-capability constraints.
    fn cap_c(
        action: Action,
        domain: &str,
        selector: Option<&str>,
        constraints: &[(&str, Json)],
    ) -> Capability {
        Capability {
            action,
            resource: match selector {
                Some(s) => Resource::new(domain, s),
                None => Resource::domain(domain),
            },
            constraints: constraints
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        }
    }

    #[test]
    fn attenuation_checks_per_capability_constraints() {
        // Parent: exec:run@bao constrained to no network.
        let parent = vec![cap_c(
            Action::Run,
            "exec",
            Some("bao"),
            &[("net", Json::from("none"))],
        )];

        // Child DROPS the constraint entirely → escalation (it would run *with*
        // network once enforcement lands). Rejected, and the error names the key.
        let looser = cap_c(Action::Run, "exec", Some("bao"), &[]);
        let err = attenuate(&parent, &looser).unwrap_err();
        assert_eq!(err.constraint.as_deref(), Some("net"));

        // Child LOOSENS the value (none → full) → escalation.
        let widened = cap_c(
            Action::Run,
            "exec",
            Some("bao"),
            &[("net", Json::from("full"))],
        );
        assert!(attenuate(&parent, &widened).is_err());

        // Child holds the SAME constraint → accepted (equal restriction).
        let equal = cap_c(
            Action::Run,
            "exec",
            Some("bao"),
            &[("net", Json::from("none"))],
        );
        assert!(attenuate(&parent, &equal).is_ok());

        // Child ADDS an extra constraint key the parent lacks → accepted (extra
        // restriction never escalates).
        let extra = cap_c(
            Action::Run,
            "exec",
            Some("bao"),
            &[("net", Json::from("none")), ("cpu", Json::from(1))],
        );
        assert!(attenuate(&parent, &extra).is_ok());
    }

    #[test]
    fn attenuation_treats_array_constraints_as_allow_lists() {
        // Parent allows envs {dev, staging}.
        let parent = vec![cap_c(
            Action::Write,
            "db",
            Some("conn"),
            &[("env", Json::from(vec!["dev", "staging"]))],
        )];

        // Child tightens to a subset {dev} → accepted (fewer allowed values).
        let tighter = cap_c(
            Action::Write,
            "db",
            Some("conn"),
            &[("env", Json::from(vec!["dev"]))],
        );
        assert!(attenuate(&parent, &tighter).is_ok());

        // Child adds `prod` (not in parent's list) → widens the allow-list → rejected.
        let widened = cap_c(
            Action::Write,
            "db",
            Some("conn"),
            &[("env", Json::from(vec!["dev", "prod"]))],
        );
        let err = attenuate(&parent, &widened).unwrap_err();
        assert_eq!(err.constraint.as_deref(), Some("env"));
    }

    #[test]
    fn covers_ignores_constraints_but_attenuation_does_not() {
        // Request-authorization contract (UNCHANGED): a request carrying no
        // constraints is still covered by a constrained held capability — requests
        // must not start failing closed against a held cap's limits.
        let held = cap_c(
            Action::Run,
            "exec",
            Some("bao"),
            &[("net", Json::from("none"))],
        );
        let request = Capability::new(Action::Run, Resource::new("exec", "bao"));
        assert!(held.covers(&request), "covers must ignore constraints");
        // …but *minting* that same unconstrained child from the constrained parent
        // is an escalation caught only at attenuation time.
        assert!(!held.covers_for_attenuation(&request));
        assert!(attenuate(std::slice::from_ref(&held), &request).is_err());
    }

    #[test]
    fn attenuation_requires_one_parent_cap_to_justify_both() {
        // A looser sibling parent cap can't launder a tighter one's constraints:
        // the SAME parent cap must cover action/resource AND subsume constraints.
        // Here only the constrained cap covers `exec:run@bao`; the other covers a
        // different selector, so it can't justify the child's dropped constraint.
        let parent = vec![
            cap_c(
                Action::Run,
                "exec",
                Some("bao"),
                &[("net", Json::from("none"))],
            ),
            cap_c(Action::Run, "exec", Some("sandbox"), &[]),
        ];
        let child = cap_c(Action::Run, "exec", Some("bao"), &[]);
        assert!(attenuate(&parent, &child).is_err());
    }

    #[test]
    fn has_unenforced_classifies_every_constraint() {
        // The ENFORCED shapes → false (safe to run): default, dry-run, and a
        // time-window (it's checked at run open, not swept up as "unenforceable").
        assert!(!Constraints::default().has_unenforced());
        assert!(!Constraints {
            dry_run: true,
            ..Default::default()
        }
        .has_unenforced());
        assert!(!Constraints {
            time_window: Some(TimeWindow {
                start: "2026-01-01T00:00:00Z".parse().unwrap(),
                end: "2026-01-02T00:00:00Z".parse().unwrap(),
            }),
            ..Default::default()
        }
        .has_unenforced());

        // Each not-yet-enforced constraint, set alone, must fail closed → true.
        assert!(Constraints {
            env_allow: vec!["prod".into()],
            ..Default::default()
        }
        .has_unenforced());
        assert!(Constraints {
            rate_limit: Some(5),
            ..Default::default()
        }
        .has_unenforced());
        assert!(Constraints {
            requires_approval: vec!["exec:run".into()],
            ..Default::default()
        }
        .has_unenforced());

        // `cost_limit` is now ENFORCED (the agent loop caps per-run spend against
        // `Usage.cost_usd`, §7/§19) — so a grant carrying only it runs, not fails closed.
        assert!(!Constraints {
            cost_limit: Some(1.0),
            ..Default::default()
        }
        .has_unenforced());
    }

    #[test]
    fn time_window_contains_is_inclusive_start_exclusive_end() {
        let w = TimeWindow {
            start: "2026-06-01T00:00:00Z".parse().unwrap(),
            end: "2026-06-02T00:00:00Z".parse().unwrap(),
        };
        let at = |s: &str| s.parse().unwrap();
        assert!(!w.contains(at("2026-05-31T23:59:59Z")), "before start");
        assert!(w.contains(at("2026-06-01T00:00:00Z")), "start is inclusive");
        assert!(w.contains(at("2026-06-01T12:00:00Z")), "midpoint");
        assert!(!w.contains(at("2026-06-02T00:00:00Z")), "end is exclusive");
        assert!(!w.contains(at("2026-06-03T00:00:00Z")), "after end");

        // An inverted/empty window contains no instant (deny-safe).
        let inverted = TimeWindow {
            start: "2026-06-02T00:00:00Z".parse().unwrap(),
            end: "2026-06-01T00:00:00Z".parse().unwrap(),
        };
        assert!(!inverted.contains(at("2026-06-01T12:00:00Z")));
    }

    #[test]
    fn constraints_reject_an_unknown_field_rather_than_dropping_it() {
        // A known shape round-trips.
        let ok: Constraints =
            serde_json::from_value(serde_json::json!({ "dry_run": true })).unwrap();
        assert!(ok.dry_run);

        // An unrecognized constraint key must FAIL the deserialize (deny_unknown_fields)
        // — never silently vanish into a weaker-than-written grant.
        let err = serde_json::from_value::<Constraints>(
            serde_json::json!({ "dry_run": true, "resource_quota": 99 }),
        );
        assert!(
            err.is_err(),
            "an unknown constraint key must be rejected, not dropped"
        );
    }
}
