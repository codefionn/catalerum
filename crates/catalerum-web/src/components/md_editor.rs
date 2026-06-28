//! A reusable Markdown editing field (SOUL §21/§12) shared across the workbench.
//!
//! [`MarkdownField`] is the writing surface that Notes pioneered: a formatting
//! toolbar over a split view — a Markdown source textarea on the left, a live,
//! safely-rendered preview on the right (via [`markdown_preview_html`]). It keeps
//! Markdown as the durable source of truth while giving a visual writing
//! experience, without pulling a parser into the WASM bundle.
//!
//! The field owns its own selection-tracking state, so a caller only hands it the
//! backing `RwSignal<String>`, a `disabled` flag, and a placeholder. Notes and
//! Skills both render it, so the toolbar/preview behaviour stays identical.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::markdown::markdown_preview_html;

/// A formatting toolbar + split Markdown source/preview editor bound to `markdown`.
///
/// `disabled` greys out the toolbar and textarea (e.g. while a save is in flight);
/// `placeholder` is the empty-state hint shown in the source textarea.
#[component]
pub fn MarkdownField(
    /// The Markdown source — the durable value the editor reads and writes.
    markdown: RwSignal<String>,
    /// When true, the toolbar buttons and textarea are disabled.
    disabled: RwSignal<bool>,
    /// Placeholder shown in the empty source textarea.
    placeholder: &'static str,
) -> impl IntoView {
    // Caret/selection in the source textarea, as a byte range into `markdown`.
    // Toolbar edits act on this range; it is kept in sync on every interaction.
    let selection = RwSignal::new((0usize, 0usize));
    // A handle to the <textarea> so a toolbar edit can restore the caret/selection
    // after the new value reaches the DOM — otherwise reassigning `prop:value`
    // collapses the caret to the end, and an inserted placeholder (e.g. the
    // "bold text" of `**bold text**`) could not be overtyped.
    let textarea_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();

    let apply_edit = move |edit: MarkdownEdit| {
        let source = markdown.get_untracked();
        let (start, end) = clamp_range(selection.get_untracked(), &source);
        let edited = apply_markdown_edit(&source, start, end, edit);
        selection.set((edited.selection_start, edited.selection_end));
        // The DOM selection API counts UTF-16 code units; convert against the
        // *new* text before it is moved into the signal.
        let sel_start = byte_index_to_utf16_position(&edited.markdown, edited.selection_start);
        let sel_end = byte_index_to_utf16_position(&edited.markdown, edited.selection_end);
        markdown.set(edited.markdown);
        // Defer until after the reactive `prop:value` write reaches the DOM, then
        // refocus the textarea and re-apply the range the edit computed.
        request_animation_frame(move || {
            if let Some(el) = textarea_ref.get_untracked() {
                let _ = el.focus();
                let _ = el.set_selection_range(sel_start, sel_end);
            }
        });
    };

    view! {
        <div class="notes-toolbar" role="toolbar" aria-label="Markdown formatting">
            <button class="notes-tool" type="button" title="Heading 1" disabled=move || disabled.get() on:click=move |_| apply_edit(MarkdownEdit::Heading(1))>"H1"</button>
            <button class="notes-tool" type="button" title="Heading 2" disabled=move || disabled.get() on:click=move |_| apply_edit(MarkdownEdit::Heading(2))>"H2"</button>
            <button class="notes-tool" type="button" title="Bold" disabled=move || disabled.get() on:click=move |_| apply_edit(MarkdownEdit::Bold)>"B"</button>
            <button class="notes-tool notes-tool-italic" type="button" title="Italic" disabled=move || disabled.get() on:click=move |_| apply_edit(MarkdownEdit::Italic)>"I"</button>
            <button class="notes-tool" type="button" title="Bullet list" disabled=move || disabled.get() on:click=move |_| apply_edit(MarkdownEdit::BulletList)>"•"</button>
            <button class="notes-tool" type="button" title="Quote" disabled=move || disabled.get() on:click=move |_| apply_edit(MarkdownEdit::Quote)>"“”"</button>
            <button class="notes-tool" type="button" title="Link" disabled=move || disabled.get() on:click=move |_| apply_edit(MarkdownEdit::Link)>"↗"</button>
            <button class="notes-tool" type="button" title="Inline code" disabled=move || disabled.get() on:click=move |_| apply_edit(MarkdownEdit::InlineCode)>"`"</button>
            <button class="notes-tool" type="button" title="Code block" disabled=move || disabled.get() on:click=move |_| apply_edit(MarkdownEdit::CodeBlock)>"{ }"</button>
        </div>

        <div class="notes-wysiwyg">
            <label class="notes-pane notes-pane-source">
                <span class="notes-pane-label">"Markdown"</span>
                <textarea
                    node_ref=textarea_ref
                    class="notes-textarea"
                    placeholder=placeholder
                    disabled=move || disabled.get()
                    prop:value=move || markdown.get()
                    on:click=move |ev| {
                        if let Some(sel) = textarea_selection(ev.target()) {
                            selection.set(sel);
                        }
                    }
                    on:keyup=move |ev| {
                        if let Some(sel) = textarea_selection(ev.target()) {
                            selection.set(sel);
                        }
                    }
                    on:select=move |ev| {
                        if let Some(sel) = textarea_selection(ev.target()) {
                            selection.set(sel);
                        }
                    }
                    on:input=move |ev| {
                        markdown.set(event_target_value(&ev));
                        if let Some(sel) = textarea_selection(ev.target()) {
                            selection.set(sel);
                        }
                    }
                ></textarea>
            </label>
            <section class="notes-pane notes-pane-preview" aria-label="Rendered Markdown preview">
                <div class="notes-pane-label">"Preview"</div>
                <div
                    class="notes-preview"
                    inner_html=move || markdown_preview_html(&markdown.get())
                ></div>
            </section>
        </div>
    }
}

/// A formatting action the toolbar can apply to the current selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkdownEdit {
    Heading(usize),
    Bold,
    Italic,
    BulletList,
    Quote,
    Link,
    InlineCode,
    CodeBlock,
}

/// The result of applying a [`MarkdownEdit`]: the new source plus where the
/// selection should land (so the caret stays sensible after the edit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownEditResult {
    pub markdown: String,
    pub selection_start: usize,
    pub selection_end: usize,
}

pub fn apply_markdown_edit(
    input: &str,
    start: usize,
    end: usize,
    edit: MarkdownEdit,
) -> MarkdownEditResult {
    match edit {
        MarkdownEdit::Heading(level) => {
            apply_line_prefix(input, start, end, &"#".repeat(level), true)
        }
        MarkdownEdit::BulletList => apply_line_prefix(input, start, end, "- ", false),
        MarkdownEdit::Quote => apply_line_prefix(input, start, end, "> ", false),
        MarkdownEdit::Bold => wrap_selection(input, start, end, "**", "**", "bold text"),
        MarkdownEdit::Italic => wrap_selection(input, start, end, "*", "*", "italic text"),
        MarkdownEdit::InlineCode => wrap_selection(input, start, end, "`", "`", "code"),
        MarkdownEdit::Link => link_selection(input, start, end),
        MarkdownEdit::CodeBlock => wrap_selection(input, start, end, "```\n", "\n```", "code"),
    }
}

/// Clamp a (start, end) byte range to valid, ordered char boundaries of `input`.
pub fn clamp_range((start, end): (usize, usize), input: &str) -> (usize, usize) {
    let len = input.len();
    let start = clamp_to_boundary(input, start.min(len));
    let end = clamp_to_boundary(input, end.min(len));
    if start <= end {
        (start, end)
    } else {
        (end, start)
    }
}

fn clamp_to_boundary(input: &str, idx: usize) -> usize {
    if input.is_char_boundary(idx) {
        idx
    } else {
        input
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|i| *i < idx)
            .last()
            .unwrap_or(0)
    }
}

/// Read a textarea's current selection as a byte range into its value, mapping
/// the browser's UTF-16 selection offsets to Rust byte indices.
pub fn textarea_selection(target: Option<web_sys::EventTarget>) -> Option<(usize, usize)> {
    let textarea =
        target.and_then(|target| target.dyn_into::<web_sys::HtmlTextAreaElement>().ok())?;
    let value = textarea.value();
    let start = textarea.selection_start().ok().flatten().unwrap_or(0);
    let end = textarea.selection_end().ok().flatten().unwrap_or(start);
    Some((
        utf16_position_to_byte_index(&value, start),
        utf16_position_to_byte_index(&value, end),
    ))
}

fn utf16_position_to_byte_index(input: &str, position: u32) -> usize {
    let mut units = 0u32;
    for (idx, ch) in input.char_indices() {
        if units >= position {
            return idx;
        }
        units += ch.len_utf16() as u32;
    }
    input.len()
}

/// Inverse of [`utf16_position_to_byte_index`]: the number of UTF-16 code units
/// before `byte_index` (assumed at a char boundary), i.e. the DOM-selection
/// offset for a Rust byte offset.
fn byte_index_to_utf16_position(input: &str, byte_index: usize) -> u32 {
    let mut units = 0u32;
    for (idx, ch) in input.char_indices() {
        if idx >= byte_index {
            break;
        }
        units += ch.len_utf16() as u32;
    }
    units
}

fn wrap_selection(
    input: &str,
    start: usize,
    end: usize,
    prefix: &str,
    suffix: &str,
    placeholder: &str,
) -> MarkdownEditResult {
    let selected = &input[start..end];
    let inserted = if selected.is_empty() {
        placeholder
    } else {
        selected
    };
    let mut markdown =
        String::with_capacity(input.len() + prefix.len() + suffix.len() + placeholder.len());
    markdown.push_str(&input[..start]);
    markdown.push_str(prefix);
    markdown.push_str(inserted);
    markdown.push_str(suffix);
    markdown.push_str(&input[end..]);
    let selection_start = start + prefix.len();
    let selection_end = selection_start + inserted.len();
    MarkdownEditResult {
        markdown,
        selection_start,
        selection_end,
    }
}

fn link_selection(input: &str, start: usize, end: usize) -> MarkdownEditResult {
    let selected = &input[start..end];
    let label = if selected.is_empty() {
        "link text"
    } else {
        selected
    };
    let inserted = format!("[{label}](https://example.com)");
    let mut markdown = String::with_capacity(input.len() + inserted.len());
    markdown.push_str(&input[..start]);
    markdown.push_str(&inserted);
    markdown.push_str(&input[end..]);
    let selection_start = start + 1;
    let selection_end = selection_start + label.len();
    MarkdownEditResult {
        markdown,
        selection_start,
        selection_end,
    }
}

fn apply_line_prefix(
    input: &str,
    start: usize,
    end: usize,
    prefix: &str,
    heading: bool,
) -> MarkdownEditResult {
    let line_start = input[..start].rfind('\n').map_or(0, |idx| idx + 1);
    let line_end = input[end..].find('\n').map_or(input.len(), |idx| end + idx);
    let selected = &input[line_start..line_end];
    let empty = selected.is_empty();
    let replacement = if empty {
        if heading {
            format!("{prefix} Heading")
        } else if prefix == "- " {
            "- list item".to_string()
        } else {
            format!("{prefix}quote")
        }
    } else {
        selected
            .split('\n')
            .map(|line| prefix_line(line, prefix, heading))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut markdown = String::with_capacity(input.len() + replacement.len() + prefix.len());
    markdown.push_str(&input[..line_start]);
    markdown.push_str(&replacement);
    markdown.push_str(&input[line_end..]);
    let (selection_start, selection_end) = if empty {
        // Select only the inserted placeholder word ("Heading"/"list item"/
        // "quote"), not the marker — so the first keystroke overtypes the word and
        // the marker survives, like the wrap edits. The marker is `prefix` (+ the
        // space the heading format adds).
        let word_offset = if heading {
            prefix.len() + 1
        } else {
            prefix.len()
        };
        (line_start + word_offset, line_start + replacement.len())
    } else {
        // Existing text was prefixed; collapse the caret to the line end so the
        // next keystroke doesn't wipe the just-added marker.
        let end = line_start + replacement.len();
        (end, end)
    };
    MarkdownEditResult {
        markdown,
        selection_start,
        selection_end,
    }
}

fn prefix_line(line: &str, prefix: &str, heading: bool) -> String {
    if line.trim().is_empty() {
        return line.to_string();
    }
    if heading {
        let content = line.trim_start_matches('#').trim_start();
        format!("{prefix} {content}")
    } else if line.starts_with(prefix) {
        line.to_string()
    } else {
        format!("{prefix}{line}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_edit_wraps_selection() {
        let result = apply_markdown_edit("make bold", 5, 9, MarkdownEdit::Bold);
        assert_eq!(result.markdown, "make **bold**");
        assert_eq!((result.selection_start, result.selection_end), (7, 11));
    }

    #[test]
    fn markdown_edit_inserts_heading_on_current_line() {
        let result = apply_markdown_edit("plain\ntext", 6, 10, MarkdownEdit::Heading(2));
        assert_eq!(result.markdown, "plain\n## text");
        // Existing text was prefixed → caret collapses to the line end so the next
        // keystroke doesn't wipe the just-added "## ".
        assert_eq!((result.selection_start, result.selection_end), (13, 13));
    }

    #[test]
    fn markdown_edit_prefixes_multiline_lists() {
        let result = apply_markdown_edit("milk\neggs", 0, 9, MarkdownEdit::BulletList);
        assert_eq!(result.markdown, "- milk\n- eggs");
        assert_eq!((result.selection_start, result.selection_end), (13, 13));
    }

    #[test]
    fn markdown_edit_block_placeholder_selects_word_not_marker() {
        // On an empty line the inserted placeholder WORD is selected (overtypable)
        // while the marker survives the first keystroke — the whole point of fix #1.
        for (edit, md, word) in [
            (MarkdownEdit::Heading(1), "# Heading", "Heading"),
            (MarkdownEdit::Heading(3), "### Heading", "Heading"),
            (MarkdownEdit::BulletList, "- list item", "list item"),
            (MarkdownEdit::Quote, "> quote", "quote"),
        ] {
            let r = apply_markdown_edit("", 0, 0, edit);
            assert_eq!(r.markdown, md, "{edit:?}");
            assert_eq!(
                &r.markdown[r.selection_start..r.selection_end],
                word,
                "{edit:?}"
            );
        }
    }

    #[test]
    fn utf16_byte_index_round_trips() {
        // ASCII and multi-byte (é = 1 UTF-16 unit / 2 bytes, 😀 = 2 units / 4 bytes).
        for s in ["plain", "café", "a😀b", "# H\n- x"] {
            for (byte_idx, _) in s.char_indices().chain([(s.len(), ' ')]) {
                let u16 = byte_index_to_utf16_position(s, byte_idx);
                assert_eq!(
                    utf16_position_to_byte_index(s, u16),
                    byte_idx,
                    "{s:?}@{byte_idx}"
                );
            }
        }
    }

    #[test]
    fn clamp_range_orders_and_snaps_to_boundaries() {
        // Reversed range is reordered; out-of-range is clamped to the length.
        assert_eq!(clamp_range((9, 5), "make bold"), (5, 9));
        assert_eq!(clamp_range((100, 0), "abc"), (0, 3));
        // An index inside a multi-byte char snaps back to a boundary.
        let s = "é"; // 2 bytes
        assert_eq!(clamp_range((1, 1), s), (0, 0));
    }
}
