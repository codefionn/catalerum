//! Workbench color themes — catalogue, persistence, and the reusable picker.
//!
//! A theme is a set of CSS custom-property overrides scoped under
//! `:root[data-theme="<id>"]` in the stylesheet (see `lib.rs`). This module owns
//! the Rust side: the [`Theme`] catalogue, caching the choice in `localStorage`,
//! applying it to the document root (`<html data-theme=…>`), and the reusable
//! [`ThemePicker`] surface rendered in **Settings → Appearance** and in the
//! Quick-start wizard.
//!
//! [`Theme::Midnight`] is the built-in default carried by the base `:root`, so it
//! needs no override block. [`Theme::Contrast`] is a WCAG high-contrast theme
//! (pure black, white delineation, a single high-luminance accent).
//!
//! [`Theme::Custom`] is the user's own palette. Unlike the presets — whose
//! tokens live as static `[data-theme]` blocks in the stylesheet — the custom
//! palette is stored as JSON in `localStorage` ([`CustomTheme`]) and projected
//! into the document at runtime via an injected `<style>` element keyed on
//! `:root[data-theme="custom"]`. The picker reveals an inline editor (per-token
//! colour inputs + JSON import/export) when Custom is the active choice.

use crate::components::icons::{Icon, MdIcon};
use leptos::prelude::*;
use serde::{Deserialize, Serialize};

/// `localStorage` key under which the chosen theme id is cached.
const THEME_STORAGE_KEY: &str = "catalerum.theme";
/// `localStorage` key under which the custom palette is cached (a [`CustomTheme`]
/// serialized to JSON).
const CUSTOM_THEME_STORAGE_KEY: &str = "catalerum.theme.custom";
/// `id` of the `<style>` element holding the live `:root[data-theme="custom"]`
/// override block. Created on demand and rewritten whenever the palette changes.
const CUSTOM_STYLE_ELEMENT_ID: &str = "catalerum-custom-theme";

/// A selectable workbench theme. `id` matches the `data-theme` attribute the
/// stylesheet keys its overrides on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    /// The built-in cool slate-blue dark (base `:root`).
    Midnight,
    /// Deep teal night with an emerald accent.
    Aurora,
    /// Warm charcoal with an amber accent.
    Ember,
    /// A refined warm-paper light theme with terracotta ink.
    Parchment,
    /// WCAG high-contrast: pure black, white borders, one bright accent.
    Contrast,
    /// The user's own palette, edited in-app and importable/exportable as JSON.
    Custom,
}

/// Representative colors for a theme's live preview chip. Presets draw from
/// literal values so a card previews its own palette regardless of the active
/// theme; the Custom card mirrors the live [`CustomTheme`].
pub struct Swatch {
    pub bg: String,
    pub panel: String,
    pub border: String,
    pub fg: String,
    pub muted: String,
    pub accent: String,
    pub accent2: String,
}

impl Theme {
    /// The themes in picker order (presets first, then Custom).
    pub fn all() -> [Theme; 6] {
        [
            Theme::Midnight,
            Theme::Aurora,
            Theme::Ember,
            Theme::Parchment,
            Theme::Contrast,
            Theme::Custom,
        ]
    }

    /// The `data-theme` id and `localStorage` value.
    pub fn id(self) -> &'static str {
        match self {
            Theme::Midnight => "midnight",
            Theme::Aurora => "aurora",
            Theme::Ember => "ember",
            Theme::Parchment => "parchment",
            Theme::Contrast => "contrast",
            Theme::Custom => "custom",
        }
    }

    /// Resolve a stored id back to a theme, falling back to the default.
    pub fn from_id(id: &str) -> Theme {
        Theme::all()
            .into_iter()
            .find(|t| t.id() == id)
            .unwrap_or(Theme::Midnight)
    }

    /// The picker card title.
    pub fn label(self) -> &'static str {
        match self {
            Theme::Midnight => "Midnight",
            Theme::Aurora => "Aurora",
            Theme::Ember => "Ember",
            Theme::Parchment => "Parchment",
            Theme::Contrast => "High contrast",
            Theme::Custom => "Custom",
        }
    }

    /// A one-line description shown under the title.
    pub fn blurb(self) -> &'static str {
        match self {
            Theme::Midnight => "Cool slate blue — the default.",
            Theme::Aurora => "Deep teal night, emerald light.",
            Theme::Ember => "Warm charcoal with amber.",
            Theme::Parchment => "Warm paper, terracotta ink.",
            Theme::Contrast => "Maximum legibility, AAA.",
            Theme::Custom => "Your own palette — import & export JSON.",
        }
    }

    /// Preview colors mirroring the stylesheet's `data-theme` block. For
    /// [`Theme::Custom`] this reflects the persisted palette; for a live
    /// preview that tracks edits, read [`CustomTheme::swatch`] off the editor's
    /// signal instead.
    pub fn swatch(self) -> Swatch {
        let lit = |bg, panel, border, fg, muted, accent, accent2| Swatch {
            bg: String::from(bg),
            panel: String::from(panel),
            border: String::from(border),
            fg: String::from(fg),
            muted: String::from(muted),
            accent: String::from(accent),
            accent2: String::from(accent2),
        };
        match self {
            Theme::Midnight => lit(
                "#0f1115", "#1d212b", "#2a2f3a", "#e6e8ec", "#8b93a1", "#6ea8fe", "#3d6fd1",
            ),
            Theme::Aurora => lit(
                "#08130f", "#12271f", "#1f3a2e", "#dcefe2", "#74a08c", "#44e0a6", "#1b6f54",
            ),
            Theme::Ember => lit(
                "#15100b", "#261c13", "#39291b", "#f1e7d9", "#a8917a", "#ff9e44", "#a85a25",
            ),
            Theme::Parchment => lit(
                "#f3ede0", "#fbf6ec", "#d9ccb2", "#2a261d", "#6b6150", "#b14a32", "#8c3a26",
            ),
            Theme::Contrast => lit(
                "#000000", "#000000", "#ffffff", "#ffffff", "#e6e6e6", "#ffe000", "#ffe000",
            ),
            Theme::Custom => stored_custom_theme().swatch(),
        }
    }
}

/// The full set of theme tokens a user can customise — one field per CSS custom
/// property the stylesheet reads. Serialized to JSON for `localStorage`
/// persistence and for the appearance editor's import/export.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomTheme {
    pub bg: String,
    pub panel: String,
    pub panel_2: String,
    pub border: String,
    pub fg: String,
    pub muted: String,
    pub accent: String,
    pub accent_2: String,
    pub on_accent: String,
    pub user: String,
    pub scrim: String,
    pub err: String,
    pub err_bg: String,
    pub err_border: String,
    pub err_fg: String,
    pub ok_bg: String,
    pub ok_border: String,
    pub ok_fg: String,
    pub warn_bg: String,
    pub warn_border: String,
    pub warn_fg: String,
}

/// Every editable token: `(CSS custom property, human label)`, in editor order.
/// The first entry of each pair is also the key used by [`CustomTheme::get`] /
/// [`CustomTheme::set`].
pub const CUSTOM_FIELDS: [(&str, &str); 21] = [
    ("--bg", "Background"),
    ("--panel", "Panel"),
    ("--panel-2", "Panel (raised)"),
    ("--border", "Border"),
    ("--fg", "Text"),
    ("--muted", "Muted text"),
    ("--accent", "Accent"),
    ("--accent-2", "Accent (fill)"),
    ("--on-accent", "Text on accent"),
    ("--user", "User bubble"),
    ("--scrim", "Overlay scrim"),
    ("--err", "Error surface"),
    ("--err-bg", "Error · bg"),
    ("--err-border", "Error · border"),
    ("--err-fg", "Error · text"),
    ("--ok-bg", "Success · bg"),
    ("--ok-border", "Success · border"),
    ("--ok-fg", "Success · text"),
    ("--warn-bg", "Warning · bg"),
    ("--warn-border", "Warning · border"),
    ("--warn-fg", "Warning · text"),
];

impl Default for CustomTheme {
    /// A fresh custom palette starts from the built-in Midnight values so the
    /// user has a sensible, complete base to tweak.
    fn default() -> Self {
        CustomTheme {
            bg: "#0f1115".into(),
            panel: "#171a21".into(),
            panel_2: "#1d212b".into(),
            border: "#2a2f3a".into(),
            fg: "#e6e8ec".into(),
            muted: "#8b93a1".into(),
            accent: "#6ea8fe".into(),
            accent_2: "#3d6fd1".into(),
            on_accent: "#ffffff".into(),
            user: "#21304a".into(),
            scrim: "rgba(0,0,0,.55)".into(),
            err: "#5a2230".into(),
            err_bg: "#3a1c22".into(),
            err_border: "#5a2a32".into(),
            err_fg: "#ff9aa9".into(),
            ok_bg: "#16361f".into(),
            ok_border: "#1f5e33".into(),
            ok_fg: "#8ef0b0".into(),
            warn_bg: "#3a2c14".into(),
            warn_border: "#5a4420".into(),
            warn_fg: "#ffcf8b".into(),
        }
    }
}

impl CustomTheme {
    /// Read a token by its CSS custom-property name (see [`CUSTOM_FIELDS`]).
    /// Unknown names return `""`.
    pub fn get(&self, var: &str) -> &str {
        match var {
            "--bg" => &self.bg,
            "--panel" => &self.panel,
            "--panel-2" => &self.panel_2,
            "--border" => &self.border,
            "--fg" => &self.fg,
            "--muted" => &self.muted,
            "--accent" => &self.accent,
            "--accent-2" => &self.accent_2,
            "--on-accent" => &self.on_accent,
            "--user" => &self.user,
            "--scrim" => &self.scrim,
            "--err" => &self.err,
            "--err-bg" => &self.err_bg,
            "--err-border" => &self.err_border,
            "--err-fg" => &self.err_fg,
            "--ok-bg" => &self.ok_bg,
            "--ok-border" => &self.ok_border,
            "--ok-fg" => &self.ok_fg,
            "--warn-bg" => &self.warn_bg,
            "--warn-border" => &self.warn_border,
            "--warn-fg" => &self.warn_fg,
            _ => "",
        }
    }

    /// Write a token by its CSS custom-property name. Unknown names are ignored.
    pub fn set(&mut self, var: &str, value: String) {
        match var {
            "--bg" => self.bg = value,
            "--panel" => self.panel = value,
            "--panel-2" => self.panel_2 = value,
            "--border" => self.border = value,
            "--fg" => self.fg = value,
            "--muted" => self.muted = value,
            "--accent" => self.accent = value,
            "--accent-2" => self.accent_2 = value,
            "--on-accent" => self.on_accent = value,
            "--user" => self.user = value,
            "--scrim" => self.scrim = value,
            "--err" => self.err = value,
            "--err-bg" => self.err_bg = value,
            "--err-border" => self.err_border = value,
            "--err-fg" => self.err_fg = value,
            "--ok-bg" => self.ok_bg = value,
            "--ok-border" => self.ok_border = value,
            "--ok-fg" => self.ok_fg = value,
            "--warn-bg" => self.warn_bg = value,
            "--warn-border" => self.warn_border = value,
            "--warn-fg" => self.warn_fg = value,
            _ => {}
        }
    }

    /// The declarations inside `:root[data-theme="custom"] { … }` — e.g.
    /// `--bg: #0f1115; --panel: #171a21; …`.
    pub fn to_css_body(&self) -> String {
        CUSTOM_FIELDS
            .iter()
            .map(|(var, _)| format!("{var}: {};", self.get(var)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Pretty-printed JSON for the export field.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Parse a palette from JSON (the import field). Every field is required.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// The card preview palette for this custom theme.
    pub fn swatch(&self) -> Swatch {
        Swatch {
            bg: self.bg.clone(),
            // Cards preview the raised panel, matching the preset swatches.
            panel: self.panel_2.clone(),
            border: self.border.clone(),
            fg: self.fg.clone(),
            muted: self.muted.clone(),
            accent: self.accent.clone(),
            accent2: self.accent_2.clone(),
        }
    }
}

/// The custom palette cached in `localStorage`, or [`CustomTheme::default`] when
/// none is set or the stored value is unparseable.
#[must_use]
pub fn stored_custom_theme() -> CustomTheme {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(CUSTOM_THEME_STORAGE_KEY).ok().flatten())
        .and_then(|j| CustomTheme::from_json(&j).ok())
        .unwrap_or_default()
}

/// Persist the custom palette as JSON (best-effort).
pub fn save_custom_theme(theme: &CustomTheme) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(CUSTOM_THEME_STORAGE_KEY, &theme.to_json());
    }
}

/// Create-or-update the `<style id="catalerum-custom-theme">` block so the
/// custom palette's tokens apply whenever `data-theme="custom"` is active.
fn apply_custom_css(theme: &CustomTheme) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let style = match doc.get_element_by_id(CUSTOM_STYLE_ELEMENT_ID) {
        Some(el) => el,
        None => {
            let Ok(el) = doc.create_element("style") else {
                return;
            };
            el.set_id(CUSTOM_STYLE_ELEMENT_ID);
            // Append to <body> if present, else the document root — either keeps
            // the override global. (Avoids needing the HtmlHeadElement binding.)
            let Some(parent) = doc
                .body()
                .map(Into::into)
                .or_else(|| doc.document_element())
            else {
                return;
            };
            if parent.append_child(&el).is_err() {
                return;
            }
            el
        }
    };
    style.set_text_content(Some(&format!(
        ":root[data-theme=\"custom\"] {{ {} }}",
        theme.to_css_body()
    )));
}

/// The theme cached in `localStorage`, or the default when none is set.
#[must_use]
pub fn stored_theme() -> Theme {
    let id = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(THEME_STORAGE_KEY).ok().flatten());
    id.map_or(Theme::Midnight, |id| Theme::from_id(&id))
}

/// Write `data-theme` onto the document root so the stylesheet's overrides apply.
fn set_document_theme(theme: Theme) {
    if let Some(root) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    {
        let _ = root.set_attribute("data-theme", theme.id());
    }
}

/// Apply `theme` live (document root) and persist the choice (best-effort). For
/// [`Theme::Custom`] the persisted palette's `<style>` block is (re)built first.
pub fn apply_theme(theme: Theme) {
    if theme == Theme::Custom {
        apply_custom_css(&stored_custom_theme());
    }
    set_document_theme(theme);
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(THEME_STORAGE_KEY, theme.id());
    }
}

/// Persist `theme` as the custom palette and apply it live. The active selection
/// is switched to [`Theme::Custom`]. Used by the editor on every edit/import.
pub fn apply_custom_theme(theme: &CustomTheme) {
    save_custom_theme(theme);
    apply_custom_css(theme);
    set_document_theme(Theme::Custom);
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(THEME_STORAGE_KEY, Theme::Custom.id());
    }
}

/// Apply the persisted theme on startup (called once from `App`). Does not write
/// storage, so an untouched install keeps following the default.
pub fn init_theme() {
    let theme = stored_theme();
    if theme == Theme::Custom {
        apply_custom_css(&stored_custom_theme());
    }
    set_document_theme(theme);
}

/// A grid of theme cards. Each card shows a live mini-preview of its palette;
/// clicking it applies the theme immediately and persists the choice. Selecting
/// **Custom** reveals an inline editor (per-token colour inputs + JSON
/// import/export). Reused by Settings → Appearance and the Quick-start wizard.
#[component]
pub fn ThemePicker() -> impl IntoView {
    let current = RwSignal::new(stored_theme());
    // The live custom palette, shared between the Custom card preview and the
    // editor so edits reflect in both immediately.
    let custom = RwSignal::new(stored_custom_theme());
    view! {
        <div class="theme-grid">
            {Theme::all()
                .into_iter()
                .map(|t| {
                    let active = move || current.get() == t;
                    // Custom tracks the editor's live signal; presets are static.
                    let sw = move || {
                        if t == Theme::Custom { custom.get().swatch() } else { t.swatch() }
                    };
                    let frame = move || {
                        let s = sw();
                        format!("background:{};border-color:{}", s.bg, s.border)
                    };
                    let bar = move || {
                        let s = sw();
                        format!("background:{};border-color:{}", s.panel, s.border)
                    };
                    let dot = move || format!("background:{}", sw().accent);
                    let pill = move || format!("background:{}", sw().accent2);
                    let line_fg = move || format!("background:{}", sw().fg);
                    let line_muted = move || format!("background:{}", sw().muted);
                    view! {
                        <button
                            class="theme-card"
                            class:theme-card-active=active
                            on:click=move |_| {
                                apply_theme(t);
                                current.set(t);
                            }
                        >
                            <span class="theme-swatch" style=frame>
                                <span class="theme-swatch-bar" style=bar>
                                    <span class="theme-swatch-dot" style=dot></span>
                                </span>
                                <span class="theme-swatch-body">
                                    <span class="theme-swatch-line theme-swatch-line-lg" style=line_fg></span>
                                    <span class="theme-swatch-line theme-swatch-line-sm" style=line_muted></span>
                                    <span class="theme-swatch-pill" style=pill></span>
                                </span>
                            </span>
                            <span class="theme-card-meta">
                                <span class="theme-card-name">
                                    {t.label()}
                                    <Show when=active fallback=|| ().into_view()>
                                        <span class="theme-card-check"><Icon icon=MdIcon::Check /></span>
                                    </Show>
                                </span>
                                <span class="theme-card-blurb">{t.blurb()}</span>
                            </span>
                        </button>
                    }
                })
                .collect::<Vec<_>>()}
        </div>
        <Show when=move || current.get() == Theme::Custom fallback=|| ().into_view()>
            <CustomThemeEditor custom=custom />
        </Show>
    }
}

/// Push one edited token into the live palette: update the signal, persist, and
/// repaint the injected `<style>` block.
fn update_token(custom: RwSignal<CustomTheme>, var: &str, value: String) {
    custom.update(|c| c.set(var, value));
    let theme = custom.get_untracked();
    save_custom_theme(&theme);
    apply_custom_css(&theme);
}

/// `#rrggbb` if `value` is a 6-digit hex colour (what `<input type=color>`
/// accepts), else a neutral fallback so the swatch still renders. Non-hex
/// tokens (e.g. the `rgba()` scrim) are edited via the adjacent text field.
fn hex_or_default(value: &str) -> String {
    let v = value.trim();
    if v.len() == 7 && v.starts_with('#') && v[1..].chars().all(|c| c.is_ascii_hexdigit()) {
        v.to_string()
    } else {
        "#000000".to_string()
    }
}

/// The Custom-palette editor: a colour input + free-text field per token, a
/// read-only JSON export, and a paste-to-import box. Every change applies live.
#[component]
fn CustomThemeEditor(custom: RwSignal<CustomTheme>) -> impl IntoView {
    let import_text = RwSignal::new(String::new());
    let import_err = RwSignal::new(Option::<String>::None);

    let on_import = move |_| match CustomTheme::from_json(&import_text.get_untracked()) {
        Ok(theme) => {
            apply_custom_theme(&theme);
            custom.set(theme);
            import_text.set(String::new());
            import_err.set(None);
        }
        Err(e) => import_err.set(Some(format!("Invalid theme JSON: {e}"))),
    };

    let on_reset = move |_| {
        let theme = CustomTheme::default();
        apply_custom_theme(&theme);
        custom.set(theme);
    };

    view! {
        <div class="custom-theme">
            <div class="custom-theme-head">
                <h4 class="custom-theme-title">"Custom palette"</h4>
                <button class="settings-btn" on:click=on_reset>"Reset to Midnight"</button>
            </div>
            <p class="custom-theme-hint">
                "Edit any token below — changes apply live and are saved on this device. "
                "Each value is any CSS colour (a #hex, or rgb()/rgba())."
            </p>
            <div class="custom-theme-grid">
                {CUSTOM_FIELDS
                    .iter()
                    .map(|(var, label)| {
                        let var = *var;
                        let value = move || custom.get().get(var).to_string();
                        view! {
                            <label class="custom-theme-field">
                                <span class="custom-theme-label">{*label}</span>
                                <span class="custom-theme-inputs">
                                    <input
                                        class="custom-theme-color"
                                        type="color"
                                        prop:value=move || hex_or_default(&value())
                                        on:input=move |ev| {
                                            update_token(custom, var, event_target_value(&ev))
                                        }
                                    />
                                    <input
                                        class="settings-input custom-theme-text"
                                        type="text"
                                        prop:value=value
                                        on:input=move |ev| {
                                            update_token(custom, var, event_target_value(&ev))
                                        }
                                    />
                                </span>
                            </label>
                        }
                    })
                    .collect::<Vec<_>>()}
            </div>
            <div class="custom-theme-io">
                <div class="custom-theme-io-col">
                    <span class="custom-theme-label">"Export — select all and copy"</span>
                    <textarea
                        class="settings-input custom-theme-json"
                        readonly
                        prop:value=move || custom.get().to_json()
                    ></textarea>
                </div>
                <div class="custom-theme-io-col">
                    <span class="custom-theme-label">"Import — paste JSON, then apply"</span>
                    <textarea
                        class="settings-input custom-theme-json"
                        prop:value=move || import_text.get()
                        on:input=move |ev| import_text.set(event_target_value(&ev))
                    ></textarea>
                    <div class="custom-theme-io-actions">
                        <button
                            class="settings-btn settings-btn-primary"
                            on:click=on_import
                            disabled=move || import_text.get().trim().is_empty()
                        >
                            "Apply imported JSON"
                        </button>
                    </div>
                    <Show
                        when=move || import_err.get().is_some()
                        fallback=|| ().into_view()
                    >
                        <p class="settings-error">
                            {move || import_err.get().unwrap_or_default()}
                        </p>
                    </Show>
                </div>
            </div>
        </div>
    }
}
