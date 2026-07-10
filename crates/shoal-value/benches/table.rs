use criterion::{Criterion, criterion_group, criterion_main};
use shoal_value::{Record, Value};
use std::hint::black_box;
fn bench_suite(c: &mut Criterion) {
    let rows = (0..1_000_000)
        .map(|i| {
            let mut r = Record::new();
            r.insert("n".into(), Value::Int(i));
            r
        })
        .collect::<Vec<_>>();
    c.bench_function("table_1m_where_sort", |b| {
        b.iter(|| {
            let mut selected = rows
                .iter()
                .filter_map(|r| match r.get("n") {
                    Some(Value::Int(n)) if n % 10 == 0 => Some(*n),
                    _ => None,
                })
                .collect::<Vec<_>>();
            selected.sort_unstable_by(|a, b| b.cmp(a));
            black_box(selected)
        })
    });
}
criterion_group!(criterion_benches, bench_suite);
criterion_main!(criterion_benches);
