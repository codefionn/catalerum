//! Allocation budgets — a regression guard so the parser/renderer stays lean.
//!
//! A counting global allocator wraps the system allocator and tallies every
//! `alloc`/`realloc`. Each budget is an upper bound on the allocation *count* for
//! a representative input; tightening the parser (more borrowing, fewer temporary
//! `String`s, buffer reuse) lowers these numbers, and a regression that adds
//! allocations trips the assert. Set `CATALERUM_MD_ALLOC_PRINT=1` to print the
//! observed counts.
//!
//! Everything runs inside a single `#[test]` so no sibling test allocates on
//! another thread mid-measurement (the counter is process-wide).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: forwarding to the system allocator with the same layout.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, l: Layout) {
        // SAFETY: `ptr`/`l` come straight from our `alloc`/`realloc`.
        unsafe { System.dealloc(ptr, l) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, l: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: forwarding a valid `ptr`/`l` and new size to the system allocator.
        unsafe { System.realloc(ptr, l, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Run `f`, returning its value and the number of (re)allocations it triggered.
fn count<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = ALLOCS.load(Relaxed);
    let v = f();
    let after = ALLOCS.load(Relaxed);
    (v, after - before)
}

fn print_enabled() -> bool {
    std::env::var_os("CATALERUM_MD_ALLOC_PRINT").is_some()
}

#[test]
fn allocation_budgets() {
    // Warm up one-time lazy state (CPU-feature detection caches behind a `Once`,
    // which allocates on first touch) so it is not charged to a measured case.
    let _ = catalerum_markdown::to_html("warm `x` **y** [a](https://e.com) | b |\n|-|-|\n|1|2|");

    // Budgets are upper bounds (measured value + small headroom). They are
    // regression guards: a change that lowers them is a win (tighten the budget);
    // a change that raises one above its budget must justify the new allocation.
    let cases: &[(&str, &str, u64)] = &[
        (
            "plain paragraph",
            "the quick brown fox jumps over the lazy dog",
            5,
        ),
        ("inline markup", "a **b** _c_ `d` ~~e~~", 13),
        ("bullet list", "- a\n- b\n- c\n- d", 17),
        (
            "link",
            "see [the docs](https://example.com/path?a=1&b=2)",
            10,
        ),
    ];

    for &(name, input, budget) in cases {
        let (_html, n) = count(|| catalerum_markdown::to_html(input));
        if print_enabled() {
            eprintln!("{name:24} {n:>4} allocs (budget {budget})");
        }
        assert!(
            n <= budget,
            "`{name}` allocated {n} times (budget {budget}) — a regression, or retune the budget"
        );
    }

    // Rendering into a reused, pre-grown buffer must not reallocate the buffer:
    // a steady-state streaming render should be allocation-light.
    let mut buf = String::with_capacity(64 * 1024);
    // Prime the buffer growth once.
    catalerum_markdown::push_html(&mut buf, "the quick brown fox jumps over the lazy dog");
    let (_x, reused) = count(|| {
        buf.clear();
        catalerum_markdown::push_html(&mut buf, "the quick brown fox jumps over the lazy dog");
    });
    if print_enabled() {
        eprintln!("push_html reused buffer {reused:>4} allocs");
    }
    assert!(
        reused <= 4,
        "push_html into a reused buffer allocated {reused} times — expected the \
         output buffer not to regrow"
    );
}
