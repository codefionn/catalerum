//! Renderers over the parsed tree.
//!
//! [`html`] is always available and dependency-free. [`leptos`] is gated behind
//! the `leptos` feature (enabled by `catalerum-web`) and builds real `View` nodes
//! rather than an HTML string, so the workbench can render Markdown without an
//! `inner_html` injection and can attach behaviour (e.g. a mermaid mount hook).

pub mod html;

#[cfg(feature = "leptos")]
pub mod leptos;
