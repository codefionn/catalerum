//! The voice-conversation overlay (SOUL §7/§12): a full-screen hands-free loop
//! over the chat — the mic auto-listens (the composer's VAD), the transcript is
//! sent as an ordinary chat turn, and the assistant's final reply comes back as
//! audio over `/ws/speech` ([`crate::ws::SpeechSocket`]) and plays aloud before
//! the mic re-arms.
//!
//! This module holds the pieces that are *not* entangled with the chat panel's
//! turn machinery: the overlay state machine's states, the markdown → spoken
//! text extraction, the decode-and-play audio engine (with the sound-reactive
//! level meter), and the presentational [`VoiceOverlay`] component. The loop
//! transitions themselves live in [`crate::components::chat`], which owns the
//! recorder, the turn driver, and the signals.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;

use futures::channel::oneshot;
use futures::{Stream, StreamExt};
use gloo_timers::callback::Interval;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::components::icons::{Icon, MdIcon};
use catalerum_markdown::{parse, Event, Tag, TagEnd};

/// A kept-alive JS callback (parked so it outlives whatever installed it) —
/// the playback/wake-lock sibling of the chat recorder's `StopClosure`.
type KeptClosure = Rc<RefCell<Option<Closure<dyn FnMut()>>>>;

/// Where the hands-free loop currently is. One linear cycle:
/// `Listening → Transcribing → Waiting → Speaking → Listening …`, with `Off`
/// (overlay closed) and `Paused` (user-held) outside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceState {
    /// The overlay is closed.
    Off,
    /// The mic is live; the VAD ends the take after trailing silence.
    Listening,
    /// The recorded take is at `/audio/transcriptions`.
    Transcribing,
    /// The transcript was sent as a chat turn; the assistant is answering
    /// (and, at the tail, its reply audio is being synthesized).
    Waiting,
    /// The reply audio is playing; tapping the orb skips it.
    Speaking,
    /// User-suspended: no mic, no playback, until resumed.
    Paused,
}

/// Cap on how much of a reply is spoken. Above the server's own input cap so
/// the clamp here (at a word boundary, with an ellipsis) is what the user
/// hears, not a server rejection.
const MAX_SPOKEN_CHARS: usize = 4000;

/// Extract the *spoken* text of a markdown reply: literal text and inline code
/// pass through, block structure becomes pauses (newlines), fenced/indented
/// code blocks are elided to "(code omitted)", math and images are skipped
/// (an image's alt text still reads), and the result is clamped to
/// [`MAX_SPOKEN_CHARS`] at a whitespace boundary. Pure — unit-testable off-wasm.
pub fn speech_text(md: &str) -> String {
    let mut out = String::new();
    let mut in_code_block = false;
    for event in parse(md) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
                push_separated(&mut out, "(code omitted).");
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                // Text following the block must not glue onto the elision.
                push_newline(&mut out);
            }
            _ if in_code_block => {}
            Event::Text(t) | Event::Code(t) => out.push_str(&t),
            Event::SoftBreak => out.push(' '),
            Event::HardBreak | Event::Rule => push_newline(&mut out),
            // Block ends read as pauses; inline ends (emphasis, links…) don't.
            Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::BlockQuote
                | TagEnd::Item
                | TagEnd::TableRow,
            ) => push_newline(&mut out),
            Event::End(TagEnd::TableCell) => out.push(' '),
            // Math has no sensible reading; images contribute their alt Text.
            Event::InlineMath(_) | Event::DisplayMath(_) => {}
            Event::Start(_) | Event::End(_) | Event::TaskListMarker(_) => {}
        }
    }
    let out = out.trim().to_string();
    if out.chars().count() <= MAX_SPOKEN_CHARS {
        return out;
    }
    let mut cut = 0;
    for (count, (idx, ch)) in out.char_indices().enumerate() {
        if count >= MAX_SPOKEN_CHARS {
            break;
        }
        if ch.is_whitespace() {
            cut = idx;
        }
    }
    format!("{}…", out[..cut].trim_end())
}

/// A still-streaming paragraph is cut early once it holds this many complete
/// sentences — synthesis of the head starts while the model writes the tail.
const EARLY_SENTENCES: usize = 3;

/// …but only once the cut-off head is at least this many bytes: three clipped
/// "Ok. Sure. Done." sentences aren't worth a seam in the spoken flow.
const EARLY_MIN_CHARS: usize = 160;

/// Cuts speakable chunks out of the streaming delta text so the overlay can
/// speak them while the model is still writing (SOUL §7/§12). A chunk is a
/// completed markdown *paragraph* (a blank line **outside** a fenced code
/// block — a fence is never split, so `speech_text`'s "(code omitted)" elision
/// still sees the whole block), or — for a long paragraph that keeps streaming
/// with no blank line in sight — a batch of [`EARLY_SENTENCES`] completed
/// sentences, so synthesis doesn't stall behind one monolithic paragraph.
/// Incremental: push each delta as it arrives; whatever never got its blank
/// line is returned by `flush` when the turn ends.
#[derive(Default)]
pub struct ParagraphSegmenter {
    /// Streamed markdown not yet emitted as a paragraph.
    buf: String,
}

impl ParagraphSegmenter {
    /// Append one streamed fragment; returns every speakable chunk it
    /// completed — finished paragraphs (long ones split into sentence
    /// batches), then any early sentence batches of the still-open paragraph.
    pub fn push(&mut self, frag: &str) -> Vec<String> {
        self.buf.push_str(frag);
        let mut out = Vec::new();
        while let Some(cut) = paragraph_break(&self.buf) {
            let para = self.buf[..cut].trim().to_string();
            self.buf = self.buf[cut..].trim_start_matches('\n').to_string();
            out.extend(split_spoken_chunks(&para));
        }
        // A long paragraph with no blank line in sight is cut early at
        // sentence boundaries: the head goes to synthesis while the model is
        // still writing the rest.
        while let Some(cut) = speech_cut(&self.buf) {
            let head = self.buf[..cut].trim().to_string();
            self.buf = self.buf[cut..].trim_start().to_string();
            if !head.is_empty() {
                out.push(head);
            }
        }
        out
    }

    /// Take whatever remains (the turn ended mid-paragraph), split into
    /// speakable chunks. Empties the buffer.
    pub fn flush(&mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.buf);
        split_spoken_chunks(&rest)
    }

    /// Drop any buffered text (a new spoken turn starts clean).
    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

/// Byte offset of the first paragraph break in `buf`: the start of a blank
/// line that follows some content and is not inside a ``` / ~~~ fence. `None`
/// while the paragraph (or the fence) is still streaming.
fn paragraph_break(buf: &str) -> Option<usize> {
    let mut in_fence = false;
    let mut line_start = 0;
    for (i, b) in buf.bytes().enumerate() {
        if b != b'\n' {
            continue;
        }
        let line = &buf[line_start..i];
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence && line.trim().is_empty() && !buf[..line_start].trim().is_empty() {
            return Some(line_start);
        }
        line_start = i + 1;
    }
    None
}

/// Split one complete paragraph (or flushed tail) into speakable chunks of a
/// few sentences each via [`speech_cut`] — a very long paragraph becomes
/// several manageable synthesis requests instead of one monolithic clip.
/// Short paragraphs (fewer than [`EARLY_SENTENCES`] sentences, or under
/// [`EARLY_MIN_CHARS`]) pass through whole.
fn split_spoken_chunks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while let Some(cut) = speech_cut(rest) {
        let head = rest[..cut].trim();
        if !head.is_empty() {
            out.push(head.to_string());
        }
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

/// Byte offset where streamed text can be cut for speech: just past the
/// [`EARLY_SENTENCES`]-th completed sentence, provided at least
/// [`EARLY_MIN_CHARS`] bytes precede the cut. A sentence ends at `.`/`!`/`?`/
/// `…` (plus any closing quotes/emphasis) followed by whitespace — outside
/// ``` / ~~~ fences and inline code, and not after list markers, bare
/// numbers, initials, or common abbreviations. `None` while too little has
/// accumulated (the tail may still be streaming).
fn speech_cut(buf: &str) -> Option<usize> {
    let mut in_fence = false;
    let mut in_code = false;
    let mut sentences = 0;
    let mut line_start = 0;
    for (i, c) in buf.char_indices() {
        if c == '\n' {
            let t = buf[line_start..i].trim_start();
            if t.starts_with("```") || t.starts_with("~~~") {
                in_fence = !in_fence;
            }
            // A stray unpaired backtick must not poison the rest of the stream.
            in_code = false;
            line_start = i + 1;
            continue;
        }
        if in_fence {
            continue;
        }
        if c == '`' {
            in_code = !in_code;
            continue;
        }
        if in_code || !matches!(c, '.' | '!' | '?' | '…') {
            continue;
        }
        // The word before the punctuation must really end a sentence.
        let tok_start = buf[..i]
            .char_indices()
            .rev()
            .find(|&(_, c)| c.is_whitespace())
            .map_or(0, |(p, c)| p + c.len_utf8());
        if !boundary_token_ok(&buf[tok_start..i]) {
            continue;
        }
        // Closing quotes/emphasis belong to the sentence; whitespace must
        // follow (a missing next char means the stream may still extend it).
        let after = i + c.len_utf8();
        let trail: usize = buf[after..]
            .chars()
            .take_while(|&ch| matches!(ch, '"' | '\'' | ')' | '*' | '_'))
            .map(char::len_utf8)
            .sum();
        let end = after + trail;
        match buf[end..].chars().next() {
            Some(ws) if ws.is_whitespace() => {}
            _ => continue,
        }
        sentences += 1;
        if sentences >= EARLY_SENTENCES && end >= EARLY_MIN_CHARS {
            return Some(end);
        }
    }
    None
}

/// Whether the word preceding a sentence-ender really ends a sentence.
/// Rejects list markers and bare numbers ("1."), single letters/initials
/// ("J."), dotted abbreviations ("e.g.", "z.B."), and a small set of common
/// spoken abbreviations.
fn boundary_token_ok(token: &str) -> bool {
    let bare = token.trim_matches(|c: char| !c.is_alphanumeric());
    if bare.is_empty() || bare.chars().count() == 1 {
        return false;
    }
    if bare.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    if bare.contains('.') && bare.chars().count() <= 4 {
        return false;
    }
    !matches!(
        bare.to_ascii_lowercase().as_str(),
        "etc"
            | "vs"
            | "ca"
            | "approx"
            | "dr"
            | "mr"
            | "mrs"
            | "ms"
            | "prof"
            | "st"
            | "nr"
            | "no"
            | "bzw"
            | "usw"
            | "ggf"
            | "inkl"
            | "zzgl"
    )
}

/// Append `text` ensuring one separating space from what's already there.
fn push_separated(out: &mut String, text: &str) {
    if !out.is_empty() && !out.ends_with(char::is_whitespace) {
        out.push(' ');
    }
    out.push_str(text);
}

/// End the current line (idempotent — never stacks blank lines beyond one).
fn push_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// The playback engine's live handles — the audio siblings of the chat panel's
/// recorder cells. All `Rc<RefCell<…>>` because the underlying JS objects are
/// `!Send` and must be reachable from `Copy` view callbacks. `Clone` clones the
/// handles, not the players.
#[derive(Clone, Default)]
pub struct SpeechPlayback {
    /// The output `AudioContext`. Created (and `resume()`d) synchronously in
    /// the 🎧 click — the user gesture — or Safari/Chrome autoplay policy may
    /// keep it suspended; then reused for every reply while the overlay is open.
    pub ctx: Rc<RefCell<Option<web_sys::AudioContext>>>,
    /// The currently playing source node, kept so a skip/close can `stop()` it
    /// (and so the graph edge isn't collected mid-play).
    source: Rc<RefCell<Option<web_sys::AudioBufferSourceNode>>>,
    /// The ~50 ms level-meter poll feeding the orb; dropping it cancels it.
    meter: Rc<RefCell<Option<Interval>>>,
    /// The kept-alive `onended` handler of the current source.
    onended: KeptClosure,
}

impl SpeechPlayback {
    /// Tear everything down (idempotent): stop any playing source, cancel the
    /// meter, close and drop the context. For overlay close / panel unmount.
    pub fn shutdown(&self) {
        stop_speech(self);
        if let Some(ctx) = self.ctx.borrow_mut().take() {
            let _ = ctx.close();
        }
    }
}

/// Keeps the device screen awake while the hands-free conversation is open —
/// on a phone the screen locking mid-conversation kills the mic and the
/// playback. Uses the Screen Wake Lock API dynamically via `Reflect` (the
/// typed web-sys bindings are still `web_sys_unstable_apis`-gated), so an
/// unsupported browser or an insecure context degrades silently: the
/// conversation still works, the screen just dims as usual.
///
/// The browser auto-releases the lock whenever the page hides (tab switch,
/// power button); `acquire` therefore also installs a `visibilitychange`
/// listener that re-requests it once the page is visible again. `Clone`
/// clones the handles, not the lock.
#[derive(Clone, Default)]
pub struct ScreenWakeLock {
    /// The held `WakeLockSentinel`, kept so `release` can end it early.
    sentinel: Rc<RefCell<Option<JsValue>>>,
    /// Whether the overlay currently wants the lock — gates the async request
    /// against a close that happened while it was in flight, and tells the
    /// visibility listener whether to re-acquire.
    want: Rc<Cell<bool>>,
    /// The kept-alive `visibilitychange` handler while the lock is wanted.
    on_visible: KeptClosure,
}

impl ScreenWakeLock {
    /// Ask to keep the screen on (idempotent). For overlay open.
    pub fn acquire(&self) {
        self.want.set(true);
        if self.on_visible.borrow().is_none() {
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                let want = self.want.clone();
                let sentinel = self.sentinel.clone();
                let cb = Closure::wrap(Box::new(move || {
                    let visible = web_sys::window()
                        .and_then(|w| w.document())
                        .is_some_and(|d| !d.hidden());
                    if want.get() && visible {
                        spawn_local(request_screen_lock(sentinel.clone(), want.clone()));
                    }
                }) as Box<dyn FnMut()>);
                let _ = doc.add_event_listener_with_callback(
                    "visibilitychange",
                    cb.as_ref().unchecked_ref(),
                );
                *self.on_visible.borrow_mut() = Some(cb);
            }
        }
        spawn_local(request_screen_lock(
            self.sentinel.clone(),
            self.want.clone(),
        ));
    }

    /// Let the screen sleep again (idempotent). For overlay close / unmount.
    pub fn release(&self) {
        self.want.set(false);
        if let Some(cb) = self.on_visible.borrow_mut().take() {
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                let _ = doc.remove_event_listener_with_callback(
                    "visibilitychange",
                    cb.as_ref().unchecked_ref(),
                );
            }
        }
        if let Some(sentinel) = self.sentinel.borrow_mut().take() {
            release_sentinel(&sentinel);
        }
    }
}

/// `navigator.wakeLock.request("screen")`, dynamically. A rejection
/// (permissions policy, battery saver) is cosmetic — the conversation loop
/// must never notice — so every failure path just returns.
async fn request_screen_lock(sentinel: Rc<RefCell<Option<JsValue>>>, want: Rc<Cell<bool>>) {
    if !want.get() {
        return;
    }
    let Some(nav) = web_sys::window().map(|w| w.navigator()) else {
        return;
    };
    let Ok(manager) = js_sys::Reflect::get(&nav, &JsValue::from_str("wakeLock")) else {
        return;
    };
    if !manager.is_object() {
        return; // unsupported browser or insecure context
    }
    let Some(request) = js_sys::Reflect::get(&manager, &JsValue::from_str("request"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
    else {
        return;
    };
    let Some(promise) = request
        .call1(&manager, &JsValue::from_str("screen"))
        .ok()
        .and_then(|p| p.dyn_into::<js_sys::Promise>().ok())
    else {
        return;
    };
    let Ok(fresh) = JsFuture::from(promise).await else {
        return;
    };
    // Supersede whatever an earlier request slotted; if the overlay closed
    // while this one was in flight, the fresh lock is released immediately.
    if let Some(old) = sentinel.borrow_mut().take() {
        release_sentinel(&old);
    }
    if want.get() {
        *sentinel.borrow_mut() = Some(fresh);
    } else {
        release_sentinel(&fresh);
    }
}

/// `sentinel.release()`, dynamically; already-released sentinels no-op.
fn release_sentinel(sentinel: &JsValue) {
    if let Some(release) = js_sys::Reflect::get(sentinel, &JsValue::from_str("release"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
    {
        let _ = release.call0(sentinel); // returns a Promise; fire-and-forget
    }
}

/// Decode `bytes` (whatever container the provider produced — `decodeAudioData`
/// sniffs; the content type is not trusted) and play them through an analyser
/// tap that feeds `level` (0..1, decayed) every ~50 ms for the orb. `on_ended`
/// fires when playback finishes **or** is stopped via [`stop_speech`]. Errors
/// (undecodable audio, missing context, graph failures) return `Err` for the
/// caller to surface — they must drop the loop back to listening, never wedge it.
pub async fn play_speech(
    pb: &SpeechPlayback,
    bytes: Vec<u8>,
    level: RwSignal<f32>,
    on_ended: Rc<dyn Fn()>,
) -> Result<(), String> {
    // Any previous reply still sounding is superseded.
    stop_speech(pb);
    let ctx = pb
        .ctx
        .borrow()
        .clone()
        .ok_or_else(|| "audio output is not initialized".to_string())?;
    let array = js_sys::Uint8Array::from(bytes.as_slice()).buffer();
    let decoded = JsFuture::from(
        ctx.decode_audio_data(&array)
            .map_err(|_| "could not start audio decode".to_string())?,
    )
    .await
    .map_err(|_| "could not decode the reply audio (unsupported format?)".to_string())?;
    let buffer: web_sys::AudioBuffer = decoded
        .dyn_into()
        .map_err(|_| "audio decode returned no buffer".to_string())?;

    let source = ctx
        .create_buffer_source()
        .map_err(|_| "could not create the audio source".to_string())?;
    source.set_buffer(Some(&buffer));
    let analyser = ctx
        .create_analyser()
        .map_err(|_| "could not create the audio analyser".to_string())?;
    analyser.set_fft_size(1024);
    source
        .connect_with_audio_node(&analyser)
        .map_err(|_| "could not wire the audio graph".to_string())?;
    analyser
        .connect_with_audio_node(&ctx.destination())
        .map_err(|_| "could not wire the audio output".to_string())?;

    // The ended handler covers both natural completion and an explicit stop.
    let ended = {
        let meter = pb.meter.clone();
        let source_slot = pb.source.clone();
        Closure::wrap(Box::new(move || {
            meter.borrow_mut().take();
            source_slot.borrow_mut().take();
            level.set(0.0);
            (*on_ended)();
        }) as Box<dyn FnMut()>)
    };
    // `set_onended`/`start` live on the `AudioScheduledSourceNode` base (the
    // subclass duplicates are deprecated in web-sys).
    let scheduled: &web_sys::AudioScheduledSourceNode = source.as_ref();
    scheduled.set_onended(Some(ended.as_ref().unchecked_ref()));
    *pb.onended.borrow_mut() = Some(ended);

    scheduled
        .start()
        .map_err(|_| "could not start playback".to_string())?;
    *pb.source.borrow_mut() = Some(source);

    // The speaking meter: the same RMS the mic VAD computes, at a faster tick
    // (playback level swings quicker than speech onset detection needs).
    let bins = analyser.frequency_bin_count() as usize;
    let mut prev = 0f32;
    let meter = Interval::new(50, move || {
        let mut buf = vec![0u8; bins];
        analyser.get_byte_time_domain_data(&mut buf);
        let mut sum = 0f64;
        for &s in &buf {
            let v = (f64::from(s) - 128.0) / 128.0;
            sum += v * v;
        }
        let rms = (sum / buf.len().max(1) as f64).sqrt();
        prev = level_from_rms(rms, prev);
        level.set(prev);
    });
    *pb.meter.borrow_mut() = Some(meter);
    Ok(())
}

/// Stop the current reply's playback (idempotent). Stopping the source fires
/// its `onended` handler, which clears the meter and reports through the same
/// path as natural completion.
pub fn stop_speech(pb: &SpeechPlayback) {
    if let Some(source) = pb.source.borrow_mut().take() {
        let scheduled: &web_sys::AudioScheduledSourceNode = source.as_ref();
        let _ = scheduled.stop();
    }
    pb.meter.borrow_mut().take();
}

/// Map one waveform RMS reading to the orb's 0..1 level: gained so ordinary
/// speech fills the range, with a decay envelope so the fall is smooth rather
/// than flickery. Shared by the mic meter and the playback meter.
#[must_use]
pub fn level_from_rms(rms: f64, prev: f32) -> f32 {
    let lvl = (rms * 6.0).min(1.0) as f32;
    lvl.max(prev * 0.8)
}

/// The full-screen voice overlay: a sound-reactive orb (scale/glow driven by
/// `--voice-level`, set from the live mic or playback meter), a state label,
/// the last transcript heard, and the close / pause controls. Purely
/// presentational — every transition runs through the callbacks the chat panel
/// passes in.
#[component]
pub fn VoiceOverlay(
    /// Where the loop is; also drives the state-styling classes.
    state: RwSignal<VoiceState>,
    /// The live 0..1 audio level (mic while listening, playback while speaking).
    level: RwSignal<f32>,
    /// The last utterance the mic transcribed.
    heard: RwSignal<String>,
    /// The last error worth showing (transient ones clear on the next cycle).
    error: RwSignal<Option<String>>,
    /// Close the overlay (✕ button and Escape).
    on_close: UnsyncCallback<()>,
    /// Tap on the orb: skip the reply while speaking (no-op otherwise).
    on_orb: UnsyncCallback<()>,
    /// Toggle pause/resume.
    on_toggle_pause: UnsyncCallback<()>,
) -> impl IntoView {
    // Escape closes, like every modal (the DialogHost convention).
    let esc = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" && state.get_untracked() != VoiceState::Off {
            on_close.run(());
        }
    });
    on_cleanup(move || esc.remove());

    let close_ref = NodeRef::<leptos::html::Button>::new();
    Effect::new(move |_| {
        if let Some(btn) = close_ref.get() {
            let _ = btn.focus();
        }
    });

    let status = move || match state.get() {
        VoiceState::Off => "",
        VoiceState::Listening => "Listening…",
        VoiceState::Transcribing => "Heard you — transcribing…",
        VoiceState::Waiting => "Thinking…",
        VoiceState::Speaking => "Speaking — tap the orb to skip",
        VoiceState::Paused => "Paused",
    };

    view! {
        <div
            class="voice-overlay"
            class:voice-listening=move || state.get() == VoiceState::Listening
            class:voice-transcribing=move || state.get() == VoiceState::Transcribing
            class:voice-thinking=move || state.get() == VoiceState::Waiting
            class:voice-speaking=move || state.get() == VoiceState::Speaking
            class:voice-paused=move || state.get() == VoiceState::Paused
            role="dialog"
            aria-modal="true"
            aria-label="Voice conversation"
        >
            <button
                node_ref=close_ref
                type="button"
                class="voice-close"
                title="End the voice conversation (Esc)"
                on:click=move |_| on_close.run(())
            >
                <Icon icon=MdIcon::Close />
            </button>
            <div
                class="voice-orb-wrap"
                style=move || format!("--voice-level:{:.3}", level.get())
                on:click=move |_| on_orb.run(())
            >
                <div class="voice-ring voice-ring-a"></div>
                <div class="voice-ring voice-ring-b"></div>
                <div class="voice-orb"></div>
            </div>
            <div class="voice-status">{status}</div>
            <Show when=move || heard.with(|h| !h.is_empty()) fallback=|| ().into_view()>
                <div class="voice-heard">"“" {move || heard.get()} "”"</div>
            </Show>
            <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="voice-error">{move || error.get().unwrap_or_default()}</div>
            </Show>
            <button
                type="button"
                class="voice-pause"
                on:click=move |_| on_toggle_pause.run(())
            >
                {move || {
                    if state.get() == VoiceState::Paused { "Resume" } else { "Pause" }
                }}
            </button>
        </div>
    }
}

/// A monotonically increasing correlation id for `/ws/speech` requests — the
/// overlay bumps it per reply and discards inbound frames tagged with an older
/// id (a skipped reply whose audio was still in flight).
#[derive(Clone, Default)]
pub struct SpeechReqId(Rc<Cell<u64>>);

impl SpeechReqId {
    /// Claim the next id.
    pub fn next(&self) -> u64 {
        let id = self.0.get().wrapping_add(1);
        self.0.set(id);
        id
    }

    /// The most recently claimed id — inbound frames for anything older are stale.
    #[must_use]
    pub fn current(&self) -> u64 {
        self.0.get()
    }
}

/// The result of preparing or starting one clip in [`pump_one_ahead`].
pub(super) enum PipelineStep<T> {
    /// The block produced a clip (or the clip started playing).
    Ready(T),
    /// This block has nothing playable; continue with the following block.
    Skip,
    /// End the pump after allowing the clip already sounding to finish.
    Stop,
}

/// Read a stream of completed text blocks through a strictly one-clip-ahead
/// speech pipeline.
///
/// `prepare` synthesizes a block into audio. It deliberately runs before the
/// previous clip's end signal is awaited, so the immediate next completed block
/// is generated while the current one is being read. `start` is not called until
/// that signal fires; only then does the loop poll and prepare another block.
/// Consequently a backlog can never synthesize block N+2 while N is still
/// playing.
pub(super) async fn pump_one_ahead<S, A, P, PFut, Start, StartFut>(
    mut blocks: S,
    mut prepare: P,
    mut start: Start,
) where
    S: Stream<Item = String> + Unpin,
    P: FnMut(String) -> PFut,
    PFut: Future<Output = PipelineStep<A>>,
    Start: FnMut(A) -> StartFut,
    StartFut: Future<Output = PipelineStep<oneshot::Receiver<()>>>,
{
    let mut playing: Option<oneshot::Receiver<()>> = None;
    while let Some(block) = blocks.next().await {
        let audio = match prepare(block).await {
            PipelineStep::Ready(audio) => audio,
            PipelineStep::Skip => continue,
            PipelineStep::Stop => break,
        };

        // The next clip is now fully prepared, but it must not replace the one
        // still sounding. This await is also the gate that prevents preparing a
        // second future clip from an already-buffered block.
        if let Some(current) = playing.take() {
            let _ = current.await;
        }

        match start(audio).await {
            PipelineStep::Ready(ended) => playing = Some(ended),
            PipelineStep::Skip => {}
            PipelineStep::Stop => break,
        }
    }
    if let Some(current) = playing {
        let _ = current.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::LocalPool;
    use futures::stream;
    use futures::task::LocalSpawnExt;
    use std::collections::VecDeque;

    #[test]
    fn speech_text_passes_prose_and_elides_code() {
        let md = "Hello **world**.\n\n```rust\nfn main() {}\n```\n\nDone `x` now.";
        let text = speech_text(md);
        assert!(text.contains("Hello world."), "{text}");
        assert!(text.contains("(code omitted)."), "{text}");
        assert!(!text.contains("fn main"), "{text}");
        assert!(text.contains("Done x now."), "{text}");
    }

    #[test]
    fn speech_text_reads_link_text_and_skips_math() {
        let md = "See [the docs](https://example.com) for $x^2$ details.";
        let text = speech_text(md);
        assert!(text.contains("the docs"), "{text}");
        assert!(!text.contains("example.com"), "{text}");
        assert!(!text.contains("x^2"), "{text}");
    }

    #[test]
    fn speech_text_clamps_at_whitespace() {
        let md = "word ".repeat(2000);
        let text = speech_text(&md);
        assert!(
            text.chars().count() <= MAX_SPOKEN_CHARS + 1,
            "{}",
            text.len()
        );
        assert!(text.ends_with('…'), "{text}");
        // Never cuts mid-word: the char before the ellipsis finishes "word".
        assert!(text.trim_end_matches('…').ends_with("word"), "{text}");
    }

    #[test]
    fn level_gains_and_decays() {
        // Ordinary speech RMS (~0.1) should fill most of the range.
        assert!(level_from_rms(0.1, 0.0) > 0.5);
        // Silence decays smoothly from the previous level instead of snapping.
        let fallen = level_from_rms(0.0, 1.0);
        assert!(fallen > 0.7 && fallen < 0.9);
    }

    #[test]
    fn segmenter_emits_paragraphs_incrementally() {
        let mut seg = ParagraphSegmenter::default();
        // Deltas arrive in arbitrary fragments; the break can even be split.
        assert!(seg.push("Hello ").is_empty());
        assert!(seg.push("world.\n").is_empty());
        let out = seg.push("\nSecond para starts");
        assert_eq!(out, vec!["Hello world.".to_string()]);
        assert!(seg.push(" and continues").is_empty());
        assert_eq!(
            seg.flush(),
            vec!["Second para starts and continues".to_string()]
        );
        assert!(seg.flush().is_empty());
    }

    #[test]
    fn segmenter_never_splits_inside_a_fence() {
        let mut seg = ParagraphSegmenter::default();
        // The blank line inside the fence is NOT a paragraph break.
        assert!(seg.push("```rust\nlet a = 1;\n\nlet b = 2;\n").is_empty());
        let out = seg.push("```\n\nAfter.\n\n");
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].starts_with("```rust") && out[0].contains("let b = 2;"));
        // The fenced block stayed whole, so speech elides it entirely.
        assert_eq!(speech_text(&out[0]), "(code omitted).");
        assert_eq!(out[1], "After.");
    }

    #[test]
    fn speech_text_separates_text_after_a_code_block() {
        let text = speech_text("```\nlet x = 1;\n```\nRight after.");
        assert_eq!(text, "(code omitted).\nRight after.");
    }

    #[test]
    fn segmenter_emits_several_paragraphs_from_one_push() {
        let mut seg = ParagraphSegmenter::default();
        let out = seg.push("One.\n\nTwo.\n\nThree");
        assert_eq!(out, vec!["One.".to_string(), "Two.".to_string()]);
        assert_eq!(seg.flush(), vec!["Three".to_string()]);
    }

    #[test]
    fn segmenter_cuts_long_streaming_paragraph_at_sentences() {
        let mut seg = ParagraphSegmenter::default();
        let s = "This sentence is deliberately padded to a realistic spoken length. ";
        // Two completed sentences: not yet enough for an early cut.
        assert!(seg.push(&s.repeat(2)).is_empty());
        // The third completed sentence triggers it — no blank line needed.
        let out = seg.push(s);
        assert_eq!(out, vec![s.repeat(3).trim().to_string()]);
        // The tail keeps streaming and flushes as usual.
        assert!(seg.push("And a tail").is_empty());
        assert_eq!(seg.flush(), vec!["And a tail".to_string()]);
    }

    #[test]
    fn segmenter_ignores_abbreviations_and_list_markers() {
        let mut seg = ParagraphSegmenter::default();
        // None of these dots end a sentence, however long the text gets.
        let md = format!("{} e.g. z.B. Dr. 1. 2. 3. more words", "x".repeat(200));
        assert!(seg.push(&md).is_empty());
        assert_eq!(seg.flush(), vec![md]);
    }

    #[test]
    fn segmenter_never_cuts_sentences_inside_a_fence() {
        let mut seg = ParagraphSegmenter::default();
        let line = "let x = 1. Then two. Then three. Then four. ".repeat(4);
        assert!(seg.push(&format!("```\n{line}\n")).is_empty());
        // Closing the fence and ending the paragraph emits the block whole.
        let out = seg.push("```\n\n");
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(
            out[0].starts_with("```") && out[0].ends_with("```"),
            "{out:?}"
        );
    }

    #[test]
    fn completed_long_paragraph_splits_into_sentence_batches() {
        let mut seg = ParagraphSegmenter::default();
        let s = "Words that pad this sentence out to a natural spoken length here. ";
        // Arrives complete with its blank line — one finished long paragraph.
        let out = seg.push(&format!("{}\n\n", s.repeat(7)));
        assert!(out.len() >= 2, "{out:?}");
        for chunk in &out {
            assert!(
                chunk.matches('.').count() <= EARLY_SENTENCES,
                "batch too big: {chunk}"
            );
        }
        assert_eq!(out.join(" "), s.repeat(7).trim());
        assert!(seg.flush().is_empty());
    }

    #[test]
    fn short_multi_sentence_paragraph_stays_whole() {
        let mut seg = ParagraphSegmenter::default();
        // Three sentences but well under EARLY_MIN_CHARS: no seam.
        let out = seg.push("Ok. Sure. Done. Fine.\n\n");
        assert_eq!(out, vec!["Ok. Sure. Done. Fine.".to_string()]);
    }

    #[test]
    fn req_ids_increase() {
        let ids = SpeechReqId::default();
        let a = ids.next();
        let b = ids.next();
        assert!(b > a);
        assert_eq!(ids.current(), b);
    }

    #[test]
    fn speech_pipeline_prepares_exactly_one_clip_ahead() {
        let events = Rc::new(RefCell::new(Vec::<String>::new()));
        let endings = Rc::new(RefCell::new(VecDeque::<oneshot::Sender<()>>::new()));
        let finished = Rc::new(Cell::new(false));

        let prepare_events = events.clone();
        let start_events = events.clone();
        let start_endings = endings.clone();
        let finished_after = finished.clone();
        let future = async move {
            pump_one_ahead(
                stream::iter(["one", "two", "three"].map(str::to_string)),
                move |block| {
                    prepare_events.borrow_mut().push(format!("prepare {block}"));
                    async move { PipelineStep::Ready(block) }
                },
                move |block| {
                    start_events.borrow_mut().push(format!("play {block}"));
                    let (tx, rx) = oneshot::channel();
                    start_endings.borrow_mut().push_back(tx);
                    async move { PipelineStep::Ready(rx) }
                },
            )
            .await;
            finished_after.set(true);
        };

        let mut pool = LocalPool::new();
        pool.spawner().spawn_local(future).unwrap();
        pool.run_until_stalled();
        assert_eq!(*events.borrow(), ["prepare one", "play one", "prepare two"]);
        assert!(!finished.get());

        // Once clip one ends, clip two starts and only clip three is prepared.
        endings.borrow_mut().pop_front().unwrap().send(()).unwrap();
        pool.run_until_stalled();
        assert_eq!(
            *events.borrow(),
            [
                "prepare one",
                "play one",
                "prepare two",
                "play two",
                "prepare three"
            ]
        );

        endings.borrow_mut().pop_front().unwrap().send(()).unwrap();
        pool.run_until_stalled();
        assert_eq!(
            events.borrow().last().map(String::as_str),
            Some("play three")
        );
        assert!(!finished.get());

        endings.borrow_mut().pop_front().unwrap().send(()).unwrap();
        pool.run_until_stalled();
        assert!(finished.get());
    }
}
