//! Emerged UIs — the browser-side interpreter (SOUL §emerged: AI-authored
//! declarative UIs).
//!
//! catalerum-web is Leptos→wasm, so the assistant cannot author Rust at runtime.
//! An emerged UI is instead a typed, closed-vocabulary JSON tree ([`model::UiSpec`])
//! that the AI creates/patches through server tools, persisted as one
//! `ui_definitions` row, shipped to the client as a `UiArtifact` frame (or fetched
//! via `GET /uis/{id}`), and rendered here by a single generic interpreter — the
//! same spec-then-interpret pattern [`super::flow`] uses for the automation graph.
//!
//! The split mirrors the core design:
//! * [`model`] — `Deserialize`-only wire mirrors of the core `model_ui` vocabulary
//!   (plain `String` ids; the wasm crate keeps no `catalerum-core` dependency).
//! * [`path`] — the *entire* client "eval" story: dotted-path get/set, JS-like
//!   truthiness, `{{path}}` interpolation, and the `for_each` [`path::Scope`].
//!   No expression language ships in the bundle.
//! * [`state`] — the client-only transient [`state::UiState`] (current view, open
//!   dialogs, form values) and the single `apply_op` reducer.
//! * [`render`] — `render_node` → `AnyView`; `for_each` → `show_if` → `match kind`.
//! * [`handlers`] — event dispatch. `Client` ops run locally; `Tool`/`Script`/`Ai`
//!   need the server `/uis/{id}/event` round-trip (P3/P4, not yet wired) and
//!   surface an inline notice instead of failing silently.
//! * [`ui`] — the [`ui::EmergedUi`] component: fetch (cached) → seed state → render.
//! * [`pins`] — the localStorage-backed pin set behind the nav's pinned-apps
//!   quick menu (toggled per row in the [`apps`] panel).

pub mod apps;
pub mod handlers;
pub mod model;
pub mod path;
pub mod pins;
pub mod render;
pub mod state;
pub mod ui;

pub use apps::AppsPanel;
pub use ui::EmergedUi;
