//! A minimal, **stateful** ANSI/control stripper for the read-only terminal pane
//! (SOUL §20). Raw PTY output is full of CSI/OSC escape sequences, cursor moves,
//! and carriage returns; rendering it verbatim in a `<pre>` is unreadable. This
//! is **not** a terminal emulator — it drops control/escape sequences and keeps
//! printable text + `\n`/`\t`, giving an append-only readable transcript.
//!
//! Stateful because the WebSocket delivers arbitrary byte chunks: an escape
//! sequence or a multi-byte UTF-8 character can straddle a chunk boundary, so any
//! incomplete trailing sequence is carried into the next [`push`](AnsiStripper::push).

/// Bound on a carried in-progress escape (a malformed/never-terminated escape
/// won't buffer unboundedly — past this it is abandoned).
const MAX_PENDING: usize = 4096;

/// Strips ANSI escapes + control bytes from a byte stream, across chunk
/// boundaries. One per live pane.
#[derive(Default)]
pub struct AnsiStripper {
    /// A carried incomplete escape sequence OR a trailing partial UTF-8 sequence.
    pending: Vec<u8>,
}

impl AnsiStripper {
    /// Feed the next chunk; return the readable text it contributes (printable
    /// characters + newlines/tabs), with escapes and other control bytes removed.
    pub fn push(&mut self, chunk: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(chunk);

        let mut out: Vec<u8> = Vec::with_capacity(buf.len());
        let n = buf.len();
        let mut i = 0;
        while i < n {
            let b = buf[i];
            match b {
                0x1B => match consume_escape(&buf[i..]) {
                    // A complete escape sequence — skip it.
                    Some(len) => i += len,
                    // Incomplete escape at the buffer end — carry it (bounded).
                    None => {
                        let tail = &buf[i..];
                        if tail.len() <= MAX_PENDING {
                            self.pending = tail.to_vec();
                        }
                        break;
                    }
                },
                // Keep newlines + tabs.
                0x0A | 0x09 => {
                    out.push(b);
                    i += 1;
                }
                // Drop every other C0 control (incl. `\r`) and DEL.
                0x00..=0x1F | 0x7F => i += 1,
                // Printable ASCII.
                0x20..=0x7E => {
                    out.push(b);
                    i += 1;
                }
                // A UTF-8 lead/continuation byte (>= 0x80).
                _ => {
                    let len = utf8_len(b);
                    if i + len <= n {
                        out.extend_from_slice(&buf[i..i + len]);
                        i += len;
                    } else {
                        // Incomplete trailing UTF-8 — carry to the next chunk.
                        self.pending = buf[i..].to_vec();
                        break;
                    }
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

/// The byte length of a UTF-8 sequence from its lead byte (1 for an invalid lead
/// or a stray continuation byte → handled lossily).
fn utf8_len(lead: u8) -> usize {
    if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else if lead >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// If `s` (which starts with `ESC`) holds a complete escape sequence, return its
/// byte length; else `None` (incomplete — the caller carries it to the next chunk).
fn consume_escape(s: &[u8]) -> Option<usize> {
    if s.len() < 2 {
        return None;
    }
    match s[1] {
        // CSI: ESC `[` params… final byte in 0x40..=0x7E.
        b'[' => {
            let mut j = 2;
            while j < s.len() {
                if (0x40..=0x7E).contains(&s[j]) {
                    return Some(j + 1);
                }
                j += 1;
            }
            None
        }
        // OSC: ESC `]` … terminated by BEL (0x07) or ST (ESC `\`).
        b']' => {
            let mut j = 2;
            while j < s.len() {
                if s[j] == 0x07 {
                    return Some(j + 1);
                }
                if s[j] == 0x1B {
                    return match s.get(j + 1) {
                        Some(b'\\') => Some(j + 2),
                        Some(_) => {
                            j += 1;
                            continue;
                        }
                        None => None, // partial ST — carry it
                    };
                }
                j += 1;
            }
            None
        }
        // Charset designation: ESC ( ) * + <one byte>.
        b'(' | b')' | b'*' | b'+' => {
            if s.len() >= 3 {
                Some(3)
            } else {
                None
            }
        }
        // Any other two-byte escape (ESC + one byte).
        _ => Some(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_keeps_text_and_newlines() {
        let mut s = AnsiStripper::default();
        // "\x1b[1;32mhello\x1b[0m\nworld\r\n"
        let out = s.push(b"\x1b[1;32mhello\x1b[0m\nworld\r\n");
        assert_eq!(out, "hello\nworld\n");
    }

    #[test]
    fn strips_osc_title_sequence() {
        let mut s = AnsiStripper::default();
        // OSC set-title terminated by BEL, then text.
        let out = s.push(b"\x1b]0;my title\x07ready");
        assert_eq!(out, "ready");
    }

    #[test]
    fn handles_escape_split_across_chunks() {
        let mut s = AnsiStripper::default();
        // The CSI is split mid-sequence between two pushes.
        let a = s.push(b"ab\x1b[3");
        let b = s.push(b"1mcd");
        assert_eq!(format!("{a}{b}"), "abcd");
    }

    #[test]
    fn handles_utf8_split_across_chunks() {
        let mut s = AnsiStripper::default();
        // "é" = 0xC3 0xA9, split between chunks.
        let a = s.push(&[b'x', 0xC3]);
        let b = s.push(&[0xA9, b'y']);
        assert_eq!(format!("{a}{b}"), "xéy");
    }

    #[test]
    fn drops_carriage_returns_and_bare_controls() {
        let mut s = AnsiStripper::default();
        assert_eq!(s.push(b"a\rb\x07c"), "abc");
    }
}
