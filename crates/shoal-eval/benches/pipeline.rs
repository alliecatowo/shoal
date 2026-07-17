//! Stable, representative language-pipeline measurements.
//!
//! These deliberately keep filesystem and subprocess noise out of the input.
//! Criterion can therefore compare the same named cases against a saved local
//! baseline (`cargo bench -p shoal-eval --bench pipeline -- --save-baseline …`).

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use shoal_eval::Evaluator;
use shoal_value::{StreamVal, Value, collect_stream};
use std::hint::black_box;
use std::time::Duration;

fn representative_source() -> String {
    let statement = "let value = [1, 2, 3, 4, 5, 6, 7, 8].map(x => x * 2)\n";
    let mut source = statement.repeat(128);
    source.push_str("value.len()\n");
    source
}

fn evaluator() -> Evaluator {
    Evaluator::new(std::env::temp_dir())
}

fn bench_pipeline(c: &mut Criterion) {
    let source = representative_source();
    let program = shoal_syntax::parse(&source).expect("benchmark source must parse");
    let mut group = c.benchmark_group("language_pipeline");
    group.sample_size(30);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));
    group.throughput(Throughput::Bytes(source.len() as u64));

    group.bench_function("parse_8kb", |b| {
        b.iter(|| shoal_syntax::parse(black_box(&source)).unwrap())
    });
    group.bench_function("plan_8kb", |b| {
        b.iter_batched(
            evaluator,
            |mut evaluator| evaluator.plan_program(black_box(&program)).unwrap(),
            criterion::BatchSize::SmallInput,
        )
    });
    group.bench_function("evaluate_8kb", |b| {
        b.iter_batched(
            evaluator,
            |mut evaluator| evaluator.eval_program(black_box(&program)).unwrap(),
            criterion::BatchSize::SmallInput,
        )
    });
    group.finish();

    let mut streams = c.benchmark_group("stream_pipeline");
    streams.sample_size(30);
    streams.warm_up_time(Duration::from_secs(1));
    streams.measurement_time(Duration::from_secs(5));
    streams.throughput(Throughput::Elements(4096));
    streams.bench_function("distinct_window_enumerate_4k", |b| {
        b.iter_batched(
            || {
                StreamVal::from_iter("int", (0..4096).map(|value| Ok(Value::Int(value % 127))))
                    .distinct()
                    .unwrap()
                    .window_count(16)
                    .unwrap()
                    .enumerate()
                    .unwrap()
            },
            |stream| {
                let mut evaluator = evaluator();
                black_box(collect_stream(&mut evaluator, &stream).unwrap())
            },
            criterion::BatchSize::SmallInput,
        )
    });
    streams.finish();
}

criterion_group!(benches, bench_pipeline);
criterion_main!(benches);
