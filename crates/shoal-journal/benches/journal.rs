use criterion::{Criterion, criterion_group, criterion_main};
use shoal_journal::{EntryRecord, Journal, JournalQuery};
use std::hint::black_box;
fn bench_suite(c: &mut Criterion) {
    let j = Journal::in_memory().unwrap();
    for i in 0..100_000 {
        j.append(&EntryRecord {
            session: "bench".into(),
            principal: "human".into(),
            ts_ns: i,
            cwd: b"/tmp".to_vec(),
            src: format!("echo {i}"),
            ast_json: "{}".into(),
            effects_json: "[]".into(),
            opaque: false,
        })
        .unwrap();
    }
    c.bench_function("journal_query_100k", |b| {
        b.iter(|| {
            j.query(black_box(&JournalQuery {
                limit: 100,
                ..Default::default()
            }))
            .unwrap()
        })
    });
}
criterion_group!(criterion_benches, bench_suite);
criterion_main!(criterion_benches);
