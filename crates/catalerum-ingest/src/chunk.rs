//! Text chunking for embedding (SOUL §6.4/§10).
//!
//! Splitting source text into bounded, semantically-coherent chunks is the step
//! before embedding: an embedding model has a token ceiling, and one vector per
//! whole document buries fine-grained matches. The chunker here is **pure and
//! deterministic** — same input, same chunks — so a re-ingest reproduces the
//! exact chunk set (idempotent projection, SOUL §3.4) and the unit tests need no
//! services.
//!
//! Strategy: greedily pack whole **paragraphs** (blank-line-separated) into
//! chunks up to [`ChunkConfig::max_chars`]; a single paragraph longer than the
//! limit is hard-split on character boundaries. Optional
//! [`ChunkConfig::overlap_chars`] prepends the tail of the previous chunk to
//! each next chunk, so a match straddling a boundary is still recoverable.
//!
//! Sizing is in **characters**, not tokens — a deliberate simplification: it
//! needs no tokenizer, is provider-agnostic, and chars are a safe over-estimate
//! of tokens for any model. (A token-aware splitter can replace this behind the
//! same signature later.)

/// How to split text into chunks.
#[derive(Clone, Copy, Debug)]
pub struct ChunkConfig {
    /// Maximum characters per chunk (before overlap is prepended). Must be > 0.
    pub max_chars: usize,
    /// Characters of trailing context from the previous chunk to prepend to each
    /// next chunk. `0` disables overlap. Clamped below `max_chars`.
    pub overlap_chars: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        // ~1k chars ≈ a few hundred tokens: comfortably under any embedding
        // model's input ceiling, granular enough for useful recall.
        Self {
            max_chars: 1000,
            overlap_chars: 100,
        }
    }
}

impl ChunkConfig {
    /// A config with a given size and no overlap.
    #[must_use]
    pub fn sized(max_chars: usize) -> Self {
        Self {
            max_chars: max_chars.max(1),
            overlap_chars: 0,
        }
    }

    /// Set the overlap (clamped below `max_chars` at chunk time).
    #[must_use]
    pub fn with_overlap(mut self, overlap_chars: usize) -> Self {
        self.overlap_chars = overlap_chars;
        self
    }
}

/// Split `text` into chunks per `cfg`. Whitespace-only input yields no chunks.
/// Every returned chunk is non-empty and (ignoring any prepended overlap) at
/// most `cfg.max_chars` characters.
#[must_use]
pub fn chunk_text(text: &str, cfg: &ChunkConfig) -> Vec<String> {
    let max = cfg.max_chars.max(1);
    let overlap = cfg.overlap_chars.min(max.saturating_sub(1));

    // 1. Split into trimmed, non-empty paragraphs on blank lines.
    let paragraphs: Vec<&str> = text
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();

    // 2. Greedily pack paragraphs into <= max-char bodies, hard-splitting any
    //    single paragraph that itself exceeds max.
    let mut bodies: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize; // in chars

    for para in paragraphs {
        let para_len = para.chars().count();

        if para_len > max {
            // Flush what we have, then hard-split the oversized paragraph.
            if !current.is_empty() {
                bodies.push(std::mem::take(&mut current));
                current_len = 0;
            }
            for piece in hard_split(para, max) {
                bodies.push(piece);
            }
            continue;
        }

        // +2 for the "\n\n" joiner when appending to a non-empty buffer.
        let added = if current.is_empty() {
            para_len
        } else {
            para_len + 2
        };
        if current_len + added > max && !current.is_empty() {
            bodies.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if !current.is_empty() {
            current.push_str("\n\n");
            current_len += 2;
        }
        current.push_str(para);
        current_len += para_len;
    }
    if !current.is_empty() {
        bodies.push(current);
    }

    // 3. Prepend overlap context from the previous body's tail.
    if overlap == 0 {
        return bodies;
    }
    let mut out: Vec<String> = Vec::with_capacity(bodies.len());
    for (i, body) in bodies.iter().enumerate() {
        if i == 0 {
            out.push(body.clone());
            continue;
        }
        let prev_tail = char_tail(&bodies[i - 1], overlap);
        out.push(format!("{prev_tail}\n\n{body}"));
    }
    out
}

/// Hard-split an over-long string into <= `max`-char pieces, preferring a break
/// at the last whitespace within the window so words aren't cut mid-token.
fn hard_split(s: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let hard_end = (start + max).min(chars.len());
        let mut end = hard_end;
        if hard_end < chars.len() {
            // Look back for a whitespace boundary within this window.
            if let Some(ws) = chars[start..hard_end]
                .iter()
                .rposition(|c| c.is_whitespace())
            {
                // Keep a reasonable minimum so we don't make tiny pieces.
                if ws > 0 {
                    end = start + ws;
                }
            }
        }
        let piece: String = chars[start..end].iter().collect();
        let piece = piece.trim();
        if !piece.is_empty() {
            pieces.push(piece.to_string());
        }
        // Advance past the break (skip the whitespace char if we broke on one).
        start = if end < hard_end { end + 1 } else { end };
    }
    pieces
}

/// The last `n` characters of `s` (char-safe), trimmed of leading partial
/// whitespace.
fn char_tail(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..]
        .iter()
        .collect::<String>()
        .trim_start()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_whitespace_yield_no_chunks() {
        assert!(chunk_text("", &ChunkConfig::default()).is_empty());
        assert!(chunk_text("   \n\n  \t ", &ChunkConfig::default()).is_empty());
    }

    #[test]
    fn short_text_is_a_single_chunk() {
        let chunks = chunk_text("hello world", &ChunkConfig::sized(100));
        assert_eq!(chunks, vec!["hello world".to_string()]);
    }

    #[test]
    fn paragraphs_pack_greedily_under_the_limit() {
        // Three 10-char paragraphs, max 25 → "aaaaaaaaaa\n\nbbbbbbbbbb" (22) then
        // "cccccccccc".
        let text = "aaaaaaaaaa\n\nbbbbbbbbbb\n\ncccccccccc";
        let chunks = chunk_text(text, &ChunkConfig::sized(25));
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "aaaaaaaaaa\n\nbbbbbbbbbb");
        assert_eq!(chunks[1], "cccccccccc");
        assert!(chunks.iter().all(|c| c.chars().count() <= 25));
    }

    #[test]
    fn oversized_paragraph_is_hard_split_on_whitespace() {
        // 5 words of 6 chars + spaces = 34 chars; max 14 forces splits at spaces.
        let text = "alpha2 bravo3 charl4 delta5 echo67";
        let chunks = chunk_text(text, &ChunkConfig::sized(14));
        assert!(chunks.len() >= 3, "got {chunks:?}");
        assert!(chunks.iter().all(|c| c.chars().count() <= 14), "{chunks:?}");
        // No piece starts or ends with whitespace.
        assert!(chunks.iter().all(|c| c.trim() == c));
        // Reassembling the words recovers the originals (order preserved).
        let joined = chunks.join(" ");
        for w in ["alpha2", "bravo3", "charl4", "delta5", "echo67"] {
            assert!(joined.contains(w), "missing {w} in {joined}");
        }
    }

    #[test]
    fn overlap_prepends_previous_tail() {
        let text = "aaaaaaaaaa\n\nbbbbbbbbbb";
        // max 10 → two chunks; overlap 4 prepends last 4 of chunk0 to chunk1.
        let chunks = chunk_text(text, &ChunkConfig::sized(10).with_overlap(4));
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "aaaaaaaaaa");
        assert_eq!(chunks[1], "aaaa\n\nbbbbbbbbbb");
    }

    #[test]
    fn is_char_safe_on_multibyte_input() {
        // Each 'é' is 2 bytes; a naive byte slice would panic. 12 chars, max 5.
        let text = "éééééééééééé";
        let chunks = chunk_text(text, &ChunkConfig::sized(5));
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.chars().count() <= 5));
        // All content preserved.
        let total: usize = chunks.iter().map(|c| c.chars().count()).sum();
        assert_eq!(total, 12);
    }

    #[test]
    fn no_overlap_by_default_in_sized() {
        assert_eq!(ChunkConfig::sized(100).overlap_chars, 0);
    }
}
