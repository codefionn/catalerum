//! In-app confirm / prompt dialogs (SOUL §12) — a custom, theme-aware,
//! keyboard-accessible replacement for the browser's native `window.confirm`
//! and `window.prompt`.
//!
//! The native dialogs are blocking and un-styleable: they ignore the workbench
//! theme, can't be positioned, and (on some platforms) let the user tick "don't
//! show again", silently disabling every future confirm. This module swaps them
//! for a single modal driven by a small [`Dialogs`] service.
//!
//! # Shape
//! Native `confirm()` returns a `bool` *inline*; a Leptos modal is reactive and
//! can't block. So instead of `if confirm() { act() }`, callers hand the action
//! to the service as a closure that runs *iff* the user confirms:
//!
//! ```ignore
//! let dialogs = use_dialogs();
//! dialogs.confirm(
//!     ConfirmSpec::danger("Delete board?", "This cannot be undone.", "Delete"),
//!     move || do_delete(),
//! );
//! ```
//!
//! [`Dialogs`] is a `Copy` handle over a few signals, provided once via context
//! at the shell root ([`crate::components::shell::Workbench`]) and rendered by a
//! single [`DialogHost`] mounted there; any descendant panel reaches it through
//! [`use_dialogs`]. Only one dialog shows at a time (matching the native modal
//! semantics) — opening a second replaces the first.

use leptos::ev::KeyboardEvent;
use leptos::prelude::*;

/// The deferred action a dialog runs when the user confirms. Boxed (and stored
/// in a `LocalStorage` [`StoredValue`], so it needn't be `Send + Sync`) because
/// each call site hands in a distinct closure.
enum Action {
    /// A plain confirmation — run this on "confirm".
    Confirm(Box<dyn Fn()>),
    /// A text prompt — run this with the trimmed, non-empty input on "submit".
    Prompt(Box<dyn Fn(String)>),
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum DialogKind {
    #[default]
    Confirm,
    Prompt,
}

/// The display-only fields of the open dialog (everything except the callback),
/// held in a reactive signal so the [`DialogHost`] view re-renders when a new
/// dialog opens.
#[derive(Clone, Default)]
struct DialogView {
    kind: DialogKind,
    title: String,
    message: String,
    confirm_label: String,
    cancel_label: String,
    /// Style the confirm button as destructive (used for delete/discard).
    danger: bool,
    /// Prompt-only: the empty-field hint.
    placeholder: String,
}

/// Configuration for a [`Dialogs::confirm`] call.
pub struct ConfirmSpec {
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub cancel_label: String,
    pub danger: bool,
}

impl ConfirmSpec {
    /// A destructive confirmation: the confirm button is styled as a danger
    /// action. `message` is the explanatory line; `confirm_label` the verb
    /// (e.g. "Delete", "Archive", "Discard").
    pub fn danger(
        title: impl Into<String>,
        message: impl Into<String>,
        confirm_label: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            confirm_label: confirm_label.into(),
            cancel_label: "Cancel".into(),
            danger: true,
        }
    }

    /// A neutral (non-destructive) confirmation.
    #[allow(dead_code)]
    pub fn new(
        title: impl Into<String>,
        message: impl Into<String>,
        confirm_label: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            confirm_label: confirm_label.into(),
            cancel_label: "Cancel".into(),
            danger: false,
        }
    }
}

/// Configuration for a [`Dialogs::prompt`] call (a single-line text input).
pub struct PromptSpec {
    pub title: String,
    pub message: String,
    pub placeholder: String,
    pub initial: String,
    pub confirm_label: String,
}

impl PromptSpec {
    /// A prompt titled `title` with an explanatory `message` line above the
    /// field. Defaults: empty field, "Save" confirm verb.
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            placeholder: String::new(),
            initial: String::new(),
            confirm_label: "Save".into(),
        }
    }

    /// Set the empty-field placeholder hint.
    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Override the confirm-button verb (default "Save").
    #[allow(dead_code)]
    pub fn confirm_label(mut self, label: impl Into<String>) -> Self {
        self.confirm_label = label.into();
        self
    }
}

/// A `Copy` handle to the app-wide dialog service. Created once in
/// [`crate::components::shell::Workbench`], put in context, and read by panels
/// through [`use_dialogs`].
#[derive(Clone, Copy)]
pub struct Dialogs {
    open: RwSignal<bool>,
    view: RwSignal<DialogView>,
    /// The prompt field's live value (unused by confirm dialogs).
    input: RwSignal<String>,
    /// The deferred confirm/submit action. `LocalStorage` so the boxed closure
    /// isn't required to be `Send + Sync` (the whole app is single-threaded wasm).
    action: StoredValue<Option<Action>, LocalStorage>,
}

impl Default for Dialogs {
    fn default() -> Self {
        Self::new()
    }
}

impl Dialogs {
    /// Allocate the backing signals. Must run inside a reactive owner (a
    /// component body) — call it once from the shell.
    pub fn new() -> Self {
        Self {
            open: RwSignal::new(false),
            view: RwSignal::new(DialogView::default()),
            input: RwSignal::new(String::new()),
            action: StoredValue::new_local(None),
        }
    }

    /// Open a confirmation dialog. `on_confirm` runs only if the user confirms
    /// (never on cancel/backdrop/Escape).
    pub fn confirm(self, spec: ConfirmSpec, on_confirm: impl Fn() + 'static) {
        self.action
            .set_value(Some(Action::Confirm(Box::new(on_confirm))));
        self.input.set(String::new());
        self.view.set(DialogView {
            kind: DialogKind::Confirm,
            title: spec.title,
            message: spec.message,
            confirm_label: spec.confirm_label,
            cancel_label: spec.cancel_label,
            danger: spec.danger,
            placeholder: String::new(),
        });
        self.open.set(true);
    }

    /// Open a single-line text prompt. `on_submit` runs with the trimmed value
    /// only when it is non-empty and the user submits (an empty field disables
    /// submit; cancel/backdrop/Escape never fire it).
    pub fn prompt(self, spec: PromptSpec, on_submit: impl Fn(String) + 'static) {
        self.action
            .set_value(Some(Action::Prompt(Box::new(on_submit))));
        self.input.set(spec.initial);
        self.view.set(DialogView {
            kind: DialogKind::Prompt,
            title: spec.title,
            message: spec.message,
            confirm_label: spec.confirm_label,
            cancel_label: "Cancel".into(),
            danger: false,
            placeholder: spec.placeholder,
        });
        self.open.set(true);
    }
}

/// Read the app-wide [`Dialogs`] service from context. Panics if no
/// [`DialogHost`] provided it — always available under the shell.
pub fn use_dialogs() -> Dialogs {
    expect_context::<Dialogs>()
}

/// The single modal that renders whichever dialog [`Dialogs`] currently holds.
/// Mounted once at the shell root; reads the service from context. Closes on
/// backdrop click, the Cancel button, or Escape (all discard the action).
#[component]
pub fn DialogHost() -> impl IntoView {
    let Dialogs {
        open,
        view,
        input,
        action,
    } = use_dialogs();

    let input_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let confirm_ref: NodeRef<leptos::html::Button> = NodeRef::new();

    // Run the stored action on confirm/submit, then close. A prompt with an
    // empty (trimmed) field is a no-op that keeps the dialog open.
    let submit = move || {
        let is_prompt = view.with(|v| v.kind == DialogKind::Prompt);
        let value = input.get_untracked().trim().to_string();
        if is_prompt && value.is_empty() {
            return;
        }
        let act = action.try_update_value(|slot| slot.take()).flatten();
        open.set(false);
        if let Some(a) = act {
            match a {
                Action::Confirm(f) => f(),
                Action::Prompt(f) => f(value),
            }
        }
    };
    // Dismiss without running anything.
    let cancel = move || {
        action.update_value(|slot| *slot = None);
        open.set(false);
    };

    // Escape closes the open dialog (a global listener, since focus lives inside
    // the modal). Cleaned up with the host.
    let esc = window_event_listener(leptos::ev::keydown, move |ev: KeyboardEvent| {
        if open.get_untracked() && ev.key() == "Escape" {
            ev.prevent_default();
            cancel();
        }
    });
    on_cleanup(move || esc.remove());

    // Move focus into the dialog when it opens: the text field for a prompt,
    // else the confirm button. Reacts to both `open` and the node mounting.
    Effect::new(move |_| {
        if !open.get() {
            return;
        }
        if let Some(el) = input_ref.get() {
            let _ = el.focus();
            el.select();
        } else if let Some(btn) = confirm_ref.get() {
            let _ = btn.focus();
        }
    });

    // Disable submit while a prompt's field is blank (a confirm dialog is always
    // actionable).
    let confirm_disabled =
        move || view.with(|v| v.kind == DialogKind::Prompt) && input.get().trim().is_empty();

    view! {
        <Show when=move || open.get() fallback=|| ().into_view()>
            <div class="dlg-overlay" on:click=move |_| cancel()>
                <div
                    class="dlg-modal"
                    role="dialog"
                    aria-modal="true"
                    // Clicks inside the box don't bubble to the backdrop.
                    on:click=move |ev| ev.stop_propagation()
                >
                    <header class="dlg-header">
                        <h2 class="dlg-title">{move || view.with(|v| v.title.clone())}</h2>
                    </header>
                    <div class="dlg-body">
                        {move || {
                            let m = view.with(|v| v.message.clone());
                            (!m.is_empty()).then(|| view! { <p class="dlg-message">{m}</p> })
                        }}
                        <Show
                            when=move || view.with(|v| v.kind == DialogKind::Prompt)
                            fallback=|| ().into_view()
                        >
                            <input
                                node_ref=input_ref
                                class="dlg-input"
                                type="text"
                                placeholder=move || view.with(|v| v.placeholder.clone())
                                prop:value=move || input.get()
                                on:input=move |ev| input.set(event_target_value(&ev))
                                on:keydown=move |ev: KeyboardEvent| {
                                    if ev.key() == "Enter" {
                                        ev.prevent_default();
                                        submit();
                                    }
                                }
                            />
                        </Show>
                    </div>
                    <div class="dlg-actions">
                        <button class="dlg-btn dlg-btn-cancel" on:click=move |_| cancel()>
                            {move || view.with(|v| v.cancel_label.clone())}
                        </button>
                        <button
                            node_ref=confirm_ref
                            class="dlg-btn dlg-btn-confirm"
                            class:dlg-btn-danger=move || view.with(|v| v.danger)
                            disabled=confirm_disabled
                            on:click=move |_| submit()
                        >
                            {move || view.with(|v| v.confirm_label.clone())}
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}
