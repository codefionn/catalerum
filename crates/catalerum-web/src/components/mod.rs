//! UI components for the catalerum workbench (SOUL §12).
//!
//! Ships the app shell ([`shell::Workbench`]) with a left nav, a working
//! streaming [`chat::ChatPanel`], the M2 [`calendar::CalendarPanel`]
//! (day-grouped agenda + connect-calendar form), the M3 [`notes::NotesPanel`]
//! (markdown notes list + editor), the M3 [`files::FilesPanel`] (object-storage
//! browser), the [`skills::SkillsPanel`] (skills manager), the
//! [`automations::AutomationsPanel`] (automations builder + run history), the
//! [`grants::GrantsPanel`] (§19 capability-grant builder), the
//! [`conversations::ConversationsPanel`] (chat-history browser), the
//! [`email::EmailPanel`] (§28 read-only inbox), the [`fetch::FetchPanel`]
//! (§27 web-fetch utility), the [`tasks::TasksPanel`] (§24 Kanban board), and the
//! [`memory::MemoryPanel`] (§22 memories + profile), and the
//! [`graph::GraphPanel`] (§6.3 graph explorer) — all workbench panels are now
//! active.

pub mod automations;
pub mod calendar;
/// Dependency-free SVG charts (pie/donut/bar/line/area/sparkline/gauge/radar/heatmap).
pub mod charts;
pub mod chat;
pub mod conversations;
/// Shared in-app confirm / prompt dialogs (replace native `window.confirm/prompt`).
pub(crate) mod dialogs;
pub mod email;
/// The emerged-UI interpreter: AI-authored declarative UIs rendered inline.
pub mod emerged;
pub mod fetch;
pub mod files;
pub mod flow;
pub mod grants;
pub mod graph;
/// Typed Material Design SVG icons shared by all workbench surfaces.
pub(crate) mod icons;
/// Shared dependency-free Markdown→HTML renderer (Notes preview + Chat replies).
pub(crate) mod markdown;
/// The settings "MCP clients" section — copy-paste MCP config for external
/// products (Claude Code, Codex, Cursor, …).
pub(crate) mod mcp_connect;
/// The MCP Endpoints panel — author + manage the workspace's scripted MCP
/// endpoints (`/mcp/e/{name}`).
pub mod mcp_endpoints;
/// Reusable Markdown editing field (toolbar + live preview) — Notes + Skills.
pub(crate) mod md_editor;
pub mod memory;
pub mod notes;
pub mod onboarding;
pub mod profiles;
/// The `ask_user` interactive question form rendered inline in chat.
pub(crate) mod question_form;
pub mod settings;
pub mod shell;
pub mod skills;
pub mod tasks;
pub mod terminal;
/// Color themes — catalogue, persistence, and the reusable `ThemePicker`.
pub mod theme;
/// Per-tool rendering for the chat panel's tool-call cards.
pub(crate) mod tool_render;
/// The chat's full-screen voice-conversation overlay (SOUL §7/§12): the
/// hands-free listen → transcribe → chat turn → spoken-reply loop.
pub(crate) mod voice;
/// Small shared widgets: checklist + chip input (Profiles/Skills) and the
/// `row_action` edit/delete icon button (chat/calendar/notes rows).
pub(crate) mod widgets;
pub mod workspace;

pub use automations::AutomationsPanel;
pub use calendar::CalendarPanel;
pub use chat::ChatPanel;
pub use conversations::ConversationsPanel;
pub use email::EmailPanel;
pub use emerged::EmergedUi;
pub use fetch::FetchPanel;
pub use files::FilesPanel;
pub use flow::FlowEditor;
pub use grants::GrantsPanel;
pub use graph::GraphPanel;
pub use mcp_endpoints::McpEndpointsPanel;
pub use memory::MemoryPanel;
pub use notes::NotesPanel;
pub use onboarding::OnboardingPanel;
pub use profiles::ProfilesPanel;
pub use settings::SettingsDialog;
pub use shell::{LoginView, Workbench};
pub use skills::SkillsPanel;
pub use tasks::TasksPanel;
pub use theme::ThemePicker;
pub use workspace::WorkspaceSwitcher;
