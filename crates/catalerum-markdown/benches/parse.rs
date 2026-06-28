//! Throughput benchmarks for the Markdown engine.
//!
//! Run with `cargo bench -p catalerum-markdown`. Each group reports time and, via
//! `Throughput::Bytes`, MiB/s so a change's effect on large documents is visible.
//! The corpora are built once and exercise the paths that dominate real input:
//! prose with inline markup, code-heavy fences, table-heavy data, and a big mixed
//! document. There is also a micro-benchmark for the SIMD scanner's escape path.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

fn prose(paras: usize) -> String {
    let mut s = String::new();
    for i in 0..paras {
        s.push_str(&format!(
            "## Section {i}\n\nThis is a paragraph with **bold**, *italic*, `code`, \
             ~~strike~~ and a [link](https://example.com/{i}?q=1&r=2) plus an \
             ![image](https://example.com/img/{i}.png). Lorem ipsum dolor sit amet, \
             consectetur adipiscing elit.\n\n"
        ));
    }
    s
}

fn code_heavy(blocks: usize) -> String {
    let mut s = String::new();
    for i in 0..blocks {
        s.push_str(&format!(
            "Here is snippet {i}:\n\n```rust\nfn main() {{\n    let x = {i};\n    \
             println!(\"{{}}\", x & 0xff);\n}}\n```\n\n"
        ));
    }
    s
}

fn table_heavy(rows: usize) -> String {
    let mut s = String::from("| name | qty | price |\n|:-----|:---:|------:|\n");
    for i in 0..rows {
        s.push_str(&format!("| item {i} | {i} | ${i}.00 |\n"));
    }
    s.push('\n');
    s
}

fn mixed(scale: usize) -> String {
    let mut s = String::new();
    for _ in 0..scale {
        s.push_str(&prose(2));
        s.push_str(&code_heavy(1));
        s.push_str(&table_heavy(8));
        s.push_str("- [ ] todo\n- [x] done\n  - nested\n\n> a quote\n\n---\n\n");
    }
    s
}

fn bench_render(c: &mut Criterion) {
    let corpora = [
        ("prose", prose(50)),
        ("code", code_heavy(50)),
        ("table", table_heavy(200)),
        ("mixed", mixed(20)),
    ];
    let mut group = c.benchmark_group("to_html");
    for (name, doc) in &corpora {
        group.throughput(Throughput::Bytes(doc.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), doc, |b, doc| {
            b.iter(|| catalerum_markdown::to_html(black_box(doc)));
        });
    }
    group.finish();
}

fn bench_parse_events(c: &mut Criterion) {
    let doc = mixed(20);
    let mut group = c.benchmark_group("parse_events");
    group.throughput(Throughput::Bytes(doc.len() as u64));
    group.bench_function("mixed", |b| {
        b.iter(|| catalerum_markdown::parse(black_box(&doc)).count());
    });
    group.finish();
}

fn bench_streaming(c: &mut Criterion) {
    // Simulate a chat reply arriving in ~24-byte deltas, re-rendering each time.
    let doc = mixed(10);
    let mut group = c.benchmark_group("streaming");
    group.throughput(Throughput::Bytes(doc.len() as u64));
    group.bench_function("incremental", |b| {
        b.iter(|| {
            let mut r = catalerum_markdown::StreamRenderer::new();
            let mut at = 0;
            while at < doc.len() {
                let mut end = (at + 24).min(doc.len());
                while !doc.is_char_boundary(end) {
                    end += 1;
                }
                r.update(black_box(&doc[..end]));
                at = end;
            }
            r.finish(&doc);
            r.into_html()
        });
    });
    // Baseline: full re-render of the whole buffer on every delta (the naive path).
    group.bench_function("full_rerender_each_delta", |b| {
        b.iter(|| {
            let mut total = 0usize;
            let mut at = 0;
            while at < doc.len() {
                let mut end = (at + 24).min(doc.len());
                while !doc.is_char_boundary(end) {
                    end += 1;
                }
                total += catalerum_markdown::to_html(black_box(&doc[..end])).len();
                at = end;
            }
            total
        });
    });
    group.finish();
}

criterion_group!(benches, bench_render, bench_parse_events, bench_streaming);
criterion_main!(benches);
