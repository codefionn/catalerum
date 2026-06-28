//! Shared Material Design icons.
//!
//! `md-icons` ships the SVG source as Rust constants.  This small typed layer
//! gives the workbench one component, one set of sizing rules, and a concise
//! enum that can be passed through helpers such as `row_action`.

use leptos::prelude::*;

/// Icons used by the workbench. Add variants here instead of placing emoji or
/// hand-authored SVGs in feature components.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdIcon {
    Add,
    Apps,
    ArrowBack,
    Attachment,
    Automations,
    Calendar,
    Chat,
    Check,
    ChevronRight,
    Close,
    Copy,
    Delete,
    Edit,
    Email,
    Fetch,
    File,
    Folder,
    Grants,
    Graph,
    Headphones,
    History,
    Info,
    McpEndpoints,
    Memory,
    Menu,
    Notes,
    Profiles,
    QuickStart,
    Refresh,
    Remove,
    Settings,
    Skills,
    Tasks,
    Warning,
}

impl MdIcon {
    fn svg(self) -> &'static str {
        use md_icons::outlined;

        match self {
            Self::Add => outlined::ICON_ADD,
            Self::Apps => outlined::ICON_APPS,
            Self::ArrowBack => outlined::ICON_ARROW_BACK,
            Self::Attachment => outlined::ICON_ATTACHMENT,
            Self::Automations => outlined::ICON_SMART_TOY,
            Self::Calendar => outlined::ICON_CALENDAR_MONTH,
            Self::Chat => outlined::ICON_CHAT,
            Self::Check => outlined::ICON_CHECK,
            Self::ChevronRight => outlined::ICON_CHEVRON_RIGHT,
            Self::Close => outlined::ICON_CLOSE,
            Self::Copy => outlined::ICON_CONTENT_COPY,
            Self::Delete => outlined::ICON_DELETE,
            Self::Edit => outlined::ICON_EDIT,
            Self::Email => outlined::ICON_MAIL,
            Self::Fetch => outlined::ICON_PUBLIC,
            Self::File => outlined::ICON_DRAFT,
            Self::Folder => outlined::ICON_FOLDER,
            Self::Grants => outlined::ICON_VERIFIED_USER,
            Self::Graph => outlined::ICON_HUB,
            Self::Headphones => outlined::ICON_HEADPHONES,
            Self::History => outlined::ICON_HISTORY,
            Self::Info => outlined::ICON_INFO,
            Self::McpEndpoints => outlined::ICON_ACCOUNT_TREE,
            Self::Memory => outlined::ICON_MEMORY,
            Self::Menu => outlined::ICON_MENU,
            Self::Notes => outlined::ICON_NOTES,
            Self::Profiles => outlined::ICON_MANAGE_ACCOUNTS,
            Self::QuickStart => outlined::ICON_ROCKET_LAUNCH,
            Self::Refresh => outlined::ICON_REFRESH,
            Self::Remove => outlined::ICON_REMOVE,
            Self::Settings => outlined::ICON_SETTINGS,
            Self::Skills => outlined::ICON_SCHOOL,
            Self::Tasks => outlined::ICON_TASK_ALT,
            Self::Warning => outlined::ICON_WARNING,
        }
    }
}

/// Render a decorative SVG icon. The surrounding button/link owns the
/// accessible label, so the icon itself is hidden from assistive technology.
#[component]
pub fn Icon(icon: MdIcon) -> impl IntoView {
    view! {
        <span class="md-icon" aria-hidden="true" inner_html=icon.svg()></span>
    }
}
