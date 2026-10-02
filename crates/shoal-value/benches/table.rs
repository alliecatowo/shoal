//! Honest eager-table and large-stream benchmarks through Shoal's real method dispatcher.
//!
//! `table_16k_where_sort` measures eager `Value::Table` filtering and sorting at the public
//! 16,384-value admission ceiling. `stream_1m_where_each` keeps one million rows lazy and measures
//! the real stream `where` stage plus an incremental `each` sink. Neither benchmark bypasses Shoal
//! with a Rust-side filter/sort, and neither asks an eager operation to exceed its supported domain.
//!
//! Closure evaluation itself is intentionally excluded. `Value::Closure` contains evaluator-owned
//! AST, and `shoal-value` cannot depend upward on `shoal-eval`; `BenchCallCtx` recognizes fixed
//! marker values in the same way this crate's method tests do. The measurements therefore cover
//! value storage, method dispatch, lazy stream pulling, and Shoal's comparator, but not AST
//! interpretation.

use std::hint::black_box;
use std::path::PathBuf;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use shoal_ast::Span;
use shoal_value::methods::call_method;
use shoal_value::{CallArgs, CallCtx, Fs, Record, StdFs, StreamVal, VResult, Value};

const EAGER_TABLE_ROWS: i64 = 16_384;
const LARGE_STREAM_ROWS: i64 = 1_000_000;
const PRED_MOD10: &str = "__bench_pred_n_mod_10_eq_0";
const KEY_NEG_N: &str = "__bench_key_neg_n";
const SINK_BLACK_BOX: &str = "__bench_sink_black_box";

struct BenchCallCtx;

impl CallCtx for BenchCallCtx {
    fn call_closure(&mut self, f: &Value, args: Vec<Value>) -> VResult<Value> {
        let Value::Record(record) = &args[0] else {
            unreachable!("bench rows are always records")
        };
        let Some(Value::Int(n)) = record.get("n") else {
            unreachable!("bench rows always carry an int `n` field")
        };
        match f {
            Value::Str(tag) if tag.as_str() == PRED_MOD10 => Ok(Value::Bool(n % 10 == 0)),
            Value::Str(tag) if tag.as_str() == KEY_NEG_N => Ok(Value::Int(-n)),
            Value::Str(tag) if tag.as_str() == SINK_BLACK_BOX => {
                black_box(*n);
                Ok(Value::Null)
            }
            _ => unreachable!("bench installs only its fixed marker closures"),
        }
    }

    fn buffer_stream(&mut self, _stream: StreamVal, _capacity: usize) -> VResult<StreamVal> {
        unreachable!("benchmarks do not drive stream buffers")
    }

    fn cwd(&self) -> PathBuf {
        PathBuf::from(".")
    }

    fn fs(&self) -> &dyn Fs {
        static STD: StdFs = StdFs;
        &STD
    }
}

fn record(n: i64) -> Record {
    let mut record = Record::new();
    record.insert("n".into(), Value::Int(n));
    record
}

fn build_table(rows: i64) -> Value {
    Value::Table((0..rows).map(record).collect())
}

fn build_stream(rows: i64) -> Value {
    Value::Stream(StreamVal::from_iter(
        "record",
        (0..rows).map(|n| Ok(Value::Record(record(n)))),
    ))
}

fn call(ctx: &mut BenchCallCtx, receiver: Value, method: &str, marker: &str) -> Value {
    call_method(
        ctx,
        receiver,
        method,
        CallArgs {
            pos: vec![Value::Str(marker.into())],
            named: vec![],
        },
        Span::default(),
    )
    .unwrap_or_else(|error| panic!("{method} benchmark dispatch failed: {error:?}"))
}

fn bench_suite(c: &mut Criterion) {
    let table = build_table(EAGER_TABLE_ROWS);
    let mut eager = c.benchmark_group("eager_table");
    eager.throughput(Throughput::Elements(EAGER_TABLE_ROWS as u64));
    eager.bench_function("table_16k_where_sort", |b| {
        b.iter_batched(
            || table.clone(),
            |table| {
                let mut ctx = BenchCallCtx;
                let filtered = call(&mut ctx, table, "where", PRED_MOD10);
                black_box(call(&mut ctx, filtered, "sort", KEY_NEG_N))
            },
            criterion::BatchSize::LargeInput,
        )
    });
    eager.finish();

    let mut stream = c.benchmark_group("large_stream");
    stream.throughput(Throughput::Elements(LARGE_STREAM_ROWS as u64));
    stream.bench_function("stream_1m_where_each", |b| {
        b.iter(|| {
            let mut ctx = BenchCallCtx;
            let filtered = call(
                &mut ctx,
                build_stream(LARGE_STREAM_ROWS),
                "where",
                PRED_MOD10,
            );
            black_box(call(&mut ctx, filtered, "each", SINK_BLACK_BOX))
        })
    });
    stream.finish();
}

criterion_group!(criterion_benches, bench_suite);
criterion_main!(criterion_benches);
