use criterion::{Criterion, criterion_group, criterion_main};
use shoal_exec::{CancelToken, ExecMode, ExecSpec, StdinSpec};
use std::{ffi::OsString, path::PathBuf};
fn bench_suite(c: &mut Criterion) {
    c.bench_function("spawn_true_capture", |b| {
        b.iter(|| {
            shoal_exec::run(
                ExecSpec {
                    argv: vec![OsString::from("true")],
                    cwd: PathBuf::from("/tmp"),
                    env: std::env::vars_os().collect(),
                    stdin: StdinSpec::Null,
                    mode: ExecMode::Capture,
                },
                &CancelToken::new(),
            )
            .unwrap()
        })
    });
}
criterion_group!(criterion_benches, bench_suite);
criterion_main!(criterion_benches);
