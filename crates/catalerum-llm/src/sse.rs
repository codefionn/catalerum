//! Minimal server-sent-events parser for the chat-completions stream (SOUL §7).
//!
//! We only need the slice of SSE the OpenAI/OpenRouter chat stream uses: lines
//! prefixed `data: `, separated by blank lines, terminated by the sentinel
//! `data: [DONE]`. This decoder buffers raw bytes off the reqwest body and emits
//! the JSON payloads (or the `[DONE]` sentinel) as they complete. Comment lines
//! (`:`-prefixed keep-alives) and non-`data` fields are ignored.

/// One thing pulled out of the SSE feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SseEvent {
    /// A `data:` payload (the JSON for one chunk).
    Data(String),
    /// The terminal `data: [DONE]` sentinel.
    Done,
}

/// Memory bound on the decoder's retained, not-yet-dispatched bytes: the unparsed
/// line buffer plus the current event's accumulated `data:` lines. A well-behaved
/// chat stream retains only KBs (one partial line / one small event) — this is
/// vast headroom. It guards a **buggy/compromised upstream** (a self-hosted or
/// 3rd-party gateway) that streams an endless line, or endless `data:` lines with
/// no blank-line terminator, from OOMing the worker — mirroring the response cap
/// catalerum-email puts on the same threat model.
const MAX_SSE_RETAINED: usize = 16 * 1024 * 1024; // 16 MiB

/// Accumulates bytes and yields complete [`SseEvent`]s.
///
/// Feed it chunks with [`push`](SseDecoder::push); drain ready events from the
/// returned vec. Call [`finish`](SseDecoder::finish) at end-of-stream to flush a
/// trailing event with no final blank line.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Bytes not yet split into a complete line.
    buf: Vec<u8>,
    /// `data:` payload lines collected for the current (not-yet-dispatched) event.
    data: Vec<String>,
    /// Running byte size of [`data`](Self::data), for the [`MAX_SSE_RETAINED`] cap.
    data_len: usize,
    /// Set once retained bytes exceed [`MAX_SSE_RETAINED`]: the stream is abusive,
    /// so all further bytes are dropped (bounded memory) and no more events emit —
    /// the consumer sees a clean end-of-stream rather than an OOM.
    poisoned: bool,
}

impl SseDecoder {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of body bytes; returns any events completed by it.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        if self.poisoned {
            return Vec::new();
        }
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();

        // Split off complete lines (terminated by `\n`); keep the remainder.
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop(); // drop '\n'
            if line.last() == Some(&b'\r') {
                line.pop(); // drop '\r'
            }
            if let Some(ev) = self.feed_line(&line) {
                out.push(ev);
            }
        }
        // Bound retained memory: an endless line (no `\n`) or an event whose
        // blank-line terminator never arrives can't grow the decoder without limit.
        if self.buf.len() + self.data_len > MAX_SSE_RETAINED {
            self.poisoned = true;
            self.buf = Vec::new();
            self.data = Vec::new();
            self.data_len = 0;
        }
        out
    }

    /// Flush at end-of-stream: emit any event whose blank-line terminator never
    /// arrived (lenient providers / abrupt close).
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if self.poisoned {
            return out;
        }
        // Any bytes left without a trailing newline still form a final line.
        if !self.buf.is_empty() {
            let mut line = std::mem::take(&mut self.buf);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(ev) = self.feed_line(&line) {
                out.push(ev);
            }
        }
        if let Some(ev) = self.flush_event() {
            out.push(ev);
        }
        out
    }

    /// Process a single (already-trimmed) SSE line.
    fn feed_line(&mut self, line: &[u8]) -> Option<SseEvent> {
        // Blank line dispatches the accumulated event.
        if line.is_empty() {
            return self.flush_event();
        }
        // Comment / keep-alive.
        if line.first() == Some(&b':') {
            return None;
        }

        let text = String::from_utf8_lossy(line);
        let (field, value) = match text.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (text.as_ref(), ""),
        };

        if field == "data" {
            if value.trim() == "[DONE]" {
                // The sentinel ends the stream immediately.
                self.data.clear();
                self.data_len = 0;
                return Some(SseEvent::Done);
            }
            self.data_len += value.len();
            self.data.push(value.to_string());
        }
        // `event:` / `id:` / `retry:` are not needed for the chat stream.
        None
    }

    /// Emit the event buffered so far (multi-line `data` joined by `\n`), if any.
    fn flush_event(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() {
            return None;
        }
        let joined = self.data.join("\n");
        self.data.clear();
        self.data_len = 0;
        Some(SseEvent::Data(joined))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_events_then_done() {
        let mut d = SseDecoder::new();
        let mut evs = d.push(b"data: {\"a\":1}\n\n");
        evs.extend(d.push(b": keepalive\n"));
        evs.extend(d.push(b"data: {\"a\":2}\n\n"));
        evs.extend(d.push(b"data: [DONE]\n\n"));
        assert_eq!(
            evs,
            vec![
                SseEvent::Data("{\"a\":1}".into()),
                SseEvent::Data("{\"a\":2}".into()),
                SseEvent::Done,
            ]
        );
    }

    #[test]
    fn handles_split_across_chunks() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: {\"a\"").is_empty());
        assert!(d.push(b":1}").is_empty());
        let evs = d.push(b"\n\n");
        assert_eq!(evs, vec![SseEvent::Data("{\"a\":1}".into())]);
    }

    #[test]
    fn flushes_trailing_event_on_finish() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: {\"a\":1}").is_empty());
        let evs = d.finish();
        assert_eq!(evs, vec![SseEvent::Data("{\"a\":1}".into())]);
    }

    #[test]
    fn an_endless_line_is_bounded_then_poisons_the_decoder() {
        let mut d = SseDecoder::new();
        // A buggy/abusive upstream streams bytes with no newline. Past the cap the
        // decoder drops the buffer and poisons — bounded memory, no OOM.
        let blob = vec![b'x'; MAX_SSE_RETAINED + 1];
        assert!(d.push(&blob).is_empty());
        // Poisoned: a subsequent well-formed event yields nothing (stream ended).
        assert!(d.push(b"data: {\"a\":1}\n\n").is_empty());
        assert!(d.finish().is_empty());
    }

    #[test]
    fn many_data_lines_without_a_blank_terminator_are_bounded() {
        let mut d = SseDecoder::new();
        // Each `data:` line is complete (ends in `\n`) and small, but the event never
        // terminates (no blank line), so `data` would accumulate without bound — the
        // cap must stop it. 512 × 64 KiB = 32 MiB, well past the 16 MiB retained cap.
        let line = format!("data: {}\n", "y".repeat(64 * 1024));
        for _ in 0..512 {
            d.push(line.as_bytes());
        }
        // Poisoned: a blank line that would otherwise flush the (huge) accumulated
        // event now yields nothing — proving `data` was bounded, not retained.
        assert!(
            d.push(b"\n").is_empty(),
            "decoder should have poisoned on the retained-bytes cap"
        );
    }

    #[test]
    fn handles_crlf() {
        let mut d = SseDecoder::new();
        let evs = d.push(b"data: x\r\n\r\n");
        assert_eq!(evs, vec![SseEvent::Data("x".into())]);
    }
}
