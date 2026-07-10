use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
fn source() -> String {
    let line = "let value = [1, 2, 3].map(x => x * 2)\n";
    line.repeat(10_000 / line.len() + 1)
}
fn bench_suite(c: &mut Criterion) {
    let src = source();
    c.bench_function("parse_10kb", |b| {
        b.iter(|| shoal_syntax::parse(black_box(&src)))
    });
    c.bench_function("reparse_10kb_keystroke", |b| {
        b.iter_batched(
            || {
                let mut s = src.clone();
                s.push(' ');
                s
            },
            |s| shoal_syntax::parse(black_box(&s)),
            criterion::BatchSize::SmallInput,
        )
    });
}
criterion_group!(criterion_benches, bench_suite);
criterion_main!(criterion_benches);
