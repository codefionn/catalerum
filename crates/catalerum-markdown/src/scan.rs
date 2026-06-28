//! SIMD-accelerated byte scanning — the parser's hot loop.
//!
//! The block and inline parsers spend most of their time skipping over "boring"
//! runs of text to reach the next *interesting* byte (a newline, a Markdown
//! delimiter like `*`/`` ` ``/`[`, or an HTML metacharacter when escaping). Doing
//! that one byte at a time is the bottleneck, so [`ByteSet::find`] vectorises it:
//! it classifies 16 (SSSE3 / NEON / wasm-simd128) or 32 (AVX2) bytes per
//! iteration and reports the offset of the first member of the set.
//!
//! ## The nibble-table membership test (multi-instruction core)
//!
//! For an arbitrary set of bytes `< 0x80`, membership is computed with two
//! shuffles and an AND per chunk — the classic Langdale/`PSHUFB` trick, the same
//! design `pulldown-cmark`'s `simd` feature uses. We precompute `lo_lut[lo]` so
//! that bit `hi` is set iff the byte `(hi << 4) | lo` is in the set. Then for a
//! lane holding byte `b`:
//!
//! * `m       = shuffle(lo_lut, b & 0x0f)`  → the bitmap of high-nibbles special
//!   for `b`'s low nibble.
//! * `hi_bit  = shuffle(bit_lut, b >> 4)`   → `1 << (b >> 4)` (and `0` when the
//!   high nibble is ≥ 8, i.e. `b >= 0x80`, because `bit_lut[8..] == 0`).
//! * the lane is a member iff `(m & hi_bit) == hi_bit` **and** `hi_bit != 0`.
//!
//! The `hi_bit != 0` guard is what makes non-ASCII bytes (UTF-8 lead/continuation
//! bytes, all `>= 0x80`) correctly *non*-members: their `hi_bit` is `0`, so the
//! naive `== ` test (which would also fire on `0 == 0`) is masked out.
//!
//! Four back-ends share that one piece of arithmetic — only the load / shuffle /
//! movemask intrinsics differ. The scalar fallback ([`ByteSet::find_scalar`]) is
//! the oracle the equivalence tests check every SIMD path against, so a wrong
//! table or intrinsic is caught on whichever architecture the test suite runs.

/// A set of "interesting" bytes, with a vectorised "find first member" scan.
///
/// Constructed `const` from a byte slice; every member should be `< 0x80` to take
/// the SIMD path (the inline/HTML delimiter sets all are). A set containing a byte
/// `>= 0x80` still works — it just always uses the scalar path.
pub(crate) struct ByteSet {
    /// `table[b]` ⇔ `b` is a member. Source of truth and the scalar/oracle path.
    table: [bool; 256],
    /// Low-nibble lookup for the SIMD test: bit `hi` of `lo_lut[lo]` is set iff the
    /// byte `(hi << 4) | lo` is a member. Only meaningful for members `< 0x80`.
    lo_lut: [u8; 16],
    /// Whether every member is `< 0x80` (precondition for the SIMD back-ends).
    ascii_only: bool,
}

/// `bit_lut[h] == 1 << h` for `h < 8`, and `0` for `h >= 8` — shared by every
/// SIMD back-end to turn a high nibble into its single-bit mask (and to zero out
/// non-ASCII high nibbles, see the module docs).
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    all(target_arch = "wasm32", target_feature = "simd128")
))]
const BIT_LUT: [i8; 16] = [1, 2, 4, 8, 16, 32, 64, 0x80u8 as i8, 0, 0, 0, 0, 0, 0, 0, 0];

impl ByteSet {
    /// Build a set from its members. `const` so the parser's sets are baked at
    /// compile time.
    pub(crate) const fn new(bytes: &[u8]) -> Self {
        let mut table = [false; 256];
        let mut lo_lut = [0u8; 16];
        let mut ascii_only = true;
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            table[b as usize] = true;
            if b < 0x80 {
                lo_lut[(b & 0x0f) as usize] |= 1 << (b >> 4);
            } else {
                ascii_only = false;
            }
            i += 1;
        }
        Self {
            table,
            lo_lut,
            ascii_only,
        }
    }

    /// Whether `b` is a member. Branchless table lookup.
    #[inline]
    pub(crate) fn contains(&self, b: u8) -> bool {
        self.table[b as usize]
    }

    /// Byte offset of the first member of the set in `hay`, or `None` if there is
    /// none. Dispatches to the widest SIMD back-end available on the target.
    #[inline]
    pub(crate) fn find(&self, hay: &[u8]) -> Option<usize> {
        #[cfg(target_arch = "x86_64")]
        {
            if self.ascii_only {
                if std::is_x86_feature_detected!("avx2") {
                    // SAFETY: guarded by the runtime `avx2` feature check.
                    return unsafe { self.find_avx2(hay) };
                }
                if std::is_x86_feature_detected!("ssse3") {
                    // SAFETY: guarded by the runtime `ssse3` feature check.
                    return unsafe { self.find_ssse3(hay) };
                }
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            if self.ascii_only {
                // SAFETY: NEON is in the aarch64 baseline (always available).
                return unsafe { self.find_neon(hay) };
            }
        }
        #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
        {
            if self.ascii_only {
                return self.find_simd128(hay);
            }
        }
        self.find_scalar(hay)
    }

    /// The scalar reference: a straight table-lookup scan. Also used as the tail
    /// handler by every SIMD back-end, and as the oracle in equivalence tests.
    #[inline]
    pub(crate) fn find_scalar(&self, hay: &[u8]) -> Option<usize> {
        hay.iter().position(|&b| self.contains(b))
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "ssse3")]
    unsafe fn find_ssse3(&self, hay: &[u8]) -> Option<usize> {
        use core::arch::x86_64::*;
        // SAFETY: every load below is bounds-checked by the `i + 16 <= n` loop
        // condition; the lookups read 16 in-bounds bytes from `self`.
        unsafe {
            let lo_lut = _mm_loadu_si128(self.lo_lut.as_ptr().cast());
            let bit_lut = _mm_loadu_si128(BIT_LUT.as_ptr().cast());
            let nibble_mask = _mm_set1_epi8(0x0f);
            let zero = _mm_setzero_si128();
            let n = hay.len();
            let mut i = 0usize;
            while i + 16 <= n {
                let v = _mm_loadu_si128(hay.as_ptr().add(i).cast());
                let lo = _mm_and_si128(v, nibble_mask);
                let hi = _mm_and_si128(_mm_srli_epi16(v, 4), nibble_mask);
                let m = _mm_shuffle_epi8(lo_lut, lo);
                let hi_bit = _mm_shuffle_epi8(bit_lut, hi);
                let eq = _mm_cmpeq_epi8(_mm_and_si128(m, hi_bit), hi_bit);
                let hibit_zero = _mm_cmpeq_epi8(hi_bit, zero);
                let special = _mm_andnot_si128(hibit_zero, eq);
                let mask = _mm_movemask_epi8(special) as u32;
                if mask != 0 {
                    return Some(i + mask.trailing_zeros() as usize);
                }
                i += 16;
            }
            self.find_scalar(&hay[i..]).map(|j| i + j)
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn find_avx2(&self, hay: &[u8]) -> Option<usize> {
        use core::arch::x86_64::*;
        // SAFETY: loads are bounds-checked by `i + 32 <= n`; the <32 tail uses the
        // 16-wide SSSE3 path (itself guarded) and then the scalar oracle.
        unsafe {
            let lo128 = _mm_loadu_si128(self.lo_lut.as_ptr().cast());
            let lo_lut = _mm256_broadcastsi128_si256(lo128);
            let bit128 = _mm_loadu_si128(BIT_LUT.as_ptr().cast());
            let bit_lut = _mm256_broadcastsi128_si256(bit128);
            let nibble_mask = _mm256_set1_epi8(0x0f);
            let zero = _mm256_setzero_si256();
            let n = hay.len();
            let mut i = 0usize;
            while i + 32 <= n {
                let v = _mm256_loadu_si256(hay.as_ptr().add(i).cast());
                let lo = _mm256_and_si256(v, nibble_mask);
                let hi = _mm256_and_si256(_mm256_srli_epi16(v, 4), nibble_mask);
                let m = _mm256_shuffle_epi8(lo_lut, lo);
                let hi_bit = _mm256_shuffle_epi8(bit_lut, hi);
                let eq = _mm256_cmpeq_epi8(_mm256_and_si256(m, hi_bit), hi_bit);
                let hibit_zero = _mm256_cmpeq_epi8(hi_bit, zero);
                let special = _mm256_andnot_si256(hibit_zero, eq);
                let mask = _mm256_movemask_epi8(special) as u32;
                if mask != 0 {
                    return Some(i + mask.trailing_zeros() as usize);
                }
                i += 32;
            }
            self.find_ssse3(&hay[i..]).map(|j| i + j)
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    unsafe fn find_neon(&self, hay: &[u8]) -> Option<usize> {
        use core::arch::aarch64::*;
        // SAFETY: loads are bounds-checked by `i + 16 <= n`; tail via scalar oracle.
        unsafe {
            let lo_lut = vld1q_u8(self.lo_lut.as_ptr());
            let bit_lut = vld1q_u8(BIT_LUT.as_ptr().cast());
            let nibble_mask = vdupq_n_u8(0x0f);
            let n = hay.len();
            let mut i = 0usize;
            while i + 16 <= n {
                let v = vld1q_u8(hay.as_ptr().add(i));
                let lo = vandq_u8(v, nibble_mask);
                let hi = vandq_u8(vshrq_n_u8(v, 4), nibble_mask);
                let m = vqtbl1q_u8(lo_lut, lo);
                let hi_bit = vqtbl1q_u8(bit_lut, hi);
                let eq = vceqq_u8(vandq_u8(m, hi_bit), hi_bit);
                // `vtstq_u8(x, x)` ⇒ all-ones lanes where `x != 0`, i.e. our guard.
                let nz = vtstq_u8(hi_bit, hi_bit);
                let special = vandq_u8(eq, nz);
                // Reduce the 0xFF/0x00 lane mask to a 64-bit word, one nibble per
                // lane (`shrn #4`), then the first set lane is `trailing_zeros / 4`.
                let narrowed = vshrn_n_u16(vreinterpretq_u16_u8(special), 4);
                let bits = vget_lane_u64(vreinterpret_u64_u8(narrowed), 0);
                if bits != 0 {
                    return Some(i + (bits.trailing_zeros() >> 2) as usize);
                }
                i += 16;
            }
            self.find_scalar(&hay[i..]).map(|j| i + j)
        }
    }

    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    fn find_simd128(&self, hay: &[u8]) -> Option<usize> {
        use core::arch::wasm32::*;
        // SAFETY: both tables are 16 in-bounds bytes.
        let lo_lut = unsafe { v128_load(self.lo_lut.as_ptr().cast()) };
        // SAFETY: `BIT_LUT` is 16 in-bounds bytes.
        let bit_lut = unsafe { v128_load(BIT_LUT.as_ptr().cast()) };
        let nibble_mask = u8x16_splat(0x0f);
        let zero = i8x16_splat(0);
        let n = hay.len();
        let mut i = 0usize;
        while i + 16 <= n {
            // SAFETY: bounds-checked by `i + 16 <= n`.
            let v = unsafe { v128_load(hay.as_ptr().add(i).cast()) };
            let lo = v128_and(v, nibble_mask);
            let hi = v128_and(u8x16_shr(v, 4), nibble_mask);
            let m = i8x16_swizzle(lo_lut, lo);
            let hi_bit = i8x16_swizzle(bit_lut, hi);
            let eq = i8x16_eq(v128_and(m, hi_bit), hi_bit);
            let hibit_zero = i8x16_eq(hi_bit, zero);
            // `v128_andnot(a, b) == a & !b` ⇒ `eq & !(hi_bit == 0)`.
            let special = v128_andnot(eq, hibit_zero);
            let mask = u8x16_bitmask(special);
            if mask != 0 {
                return Some(i + mask.trailing_zeros() as usize);
            }
            i += 16;
        }
        self.find_scalar(&hay[i..]).map(|j| i + j)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic LCG — `rand`-free pseudo-randomness for fuzz buffers
    /// (the workflow forbids `Math.random`/`Date::now`; tests must be reproducible).
    struct Lcg(u64);
    impl Lcg {
        fn next_u8(&mut self) -> u8 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u8
        }
    }

    const INLINE_BYTES: &[u8] = b"\n\r\\`*_[]!<&~|";

    #[test]
    fn contains_matches_member_list() {
        let set = ByteSet::new(INLINE_BYTES);
        for b in 0u8..=255 {
            assert_eq!(set.contains(b), INLINE_BYTES.contains(&b), "byte {b:#x}");
        }
    }

    #[test]
    fn find_equals_scalar_for_every_single_byte() {
        let set = ByteSet::new(INLINE_BYTES);
        for b in 0u8..=255 {
            let buf = [b];
            assert_eq!(set.find(&buf), set.find_scalar(&buf), "byte {b:#x}");
        }
    }

    #[test]
    fn find_equals_scalar_for_every_byte_pair() {
        // Exhaustive 65 536-case oracle check — exercises the SIMD tail on the host
        // arch and proves the nibble-table + intrinsics agree with the scalar set.
        let set = ByteSet::new(INLINE_BYTES);
        for a in 0u8..=255 {
            for b in 0u8..=255 {
                let buf = [a, b];
                assert_eq!(set.find(&buf), set.find_scalar(&buf), "pair {a:#x},{b:#x}");
            }
        }
    }

    #[test]
    fn find_equals_scalar_for_long_random_buffers() {
        // Long buffers exercise the 32-wide AVX2 loop, the 16-wide tail, and the
        // scalar remainder. Several sets, several lengths, members at every offset.
        let sets = [
            ByteSet::new(INLINE_BYTES),
            ByteSet::new(b"&<>"),
            ByteSet::new(b"&<>\"'"),
            ByteSet::new(b"\n"),
        ];
        let mut rng = Lcg(0x1234_5678_9abc_def0);
        for set in &sets {
            for len in [1usize, 7, 15, 16, 17, 31, 32, 33, 48, 64, 100, 257, 1024] {
                // A buffer biased towards non-members so members are rare and land
                // at varied offsets, plus an all-non-member buffer.
                for fill in [0u8, b'x', 0xC3] {
                    let mut buf = vec![fill; len];
                    for slot in buf.iter_mut() {
                        if rng.next_u8() < 24 {
                            *slot = INLINE_BYTES[(rng.next_u8() as usize) % INLINE_BYTES.len()];
                        }
                    }
                    assert_eq!(
                        set.find(&buf),
                        set.find_scalar(&buf),
                        "len={len} fill={fill:#x}"
                    );
                    // And the same buffer with no members at all.
                    let clean = vec![fill.max(b' '); len];
                    if clean.iter().all(|&b| !set.contains(b)) {
                        assert_eq!(set.find(&clean), None, "clean len={len} fill={fill:#x}");
                    }
                }
            }
        }
    }
}
