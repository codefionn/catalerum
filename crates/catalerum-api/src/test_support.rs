//! Shared helpers for the crate's unit tests.

use catalerum_core::capability::{Action, Capability, Resource};
use catalerum_core::tool::ToolContext;

/// A context holding full authority (`*:*`). Dispatch refuses a context without
/// capabilities (SOUL §19 deny-by-default), so tests that exercise dispatch
/// mechanics rather than the capability check start from this.
pub(crate) fn full_authority() -> ToolContext {
    ToolContext {
        capabilities: Some(vec![Capability::new(Action::Any, Resource::any())]),
        ..ToolContext::default()
    }
}
