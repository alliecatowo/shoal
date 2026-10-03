#![no_main]

use libfuzzer_sys::fuzz_target;
use shoal_value::{CallCtx, ErrorVal, Fs, StdFs, StreamVal, VResult, Value, collect_stream};
use std::path::PathBuf;

struct Context;

impl CallCtx for Context {
    fn call_closure(&mut self, _: &Value, _: Vec<Value>) -> VResult<Value> {
        Err(ErrorVal::new("fuzz", "closure-free stream target"))
    }

    fn buffer_stream(&mut self, _: StreamVal, _: usize) -> VResult<StreamVal> {
        Err(ErrorVal::new("fuzz", "buffer-free stream target"))
    }

    fn cwd(&self) -> PathBuf {
        PathBuf::from(".")
    }

    fn fs(&self) -> &dyn Fs {
        static FS: StdFs = StdFs;
        &FS
    }
}

fuzz_target!(|data: &[u8]| {
    let values = data
        .iter()
        .skip(4)
        .take(128)
        .map(|byte| Ok(Value::Int(i64::from(*byte))))
        .collect::<Vec<_>>();
    let mut stream = StreamVal::from_iter("int", values.into_iter());

    for byte in data.iter().take(4) {
        stream = match byte % 7 {
            0 => stream.take_n(usize::from(*byte % 32)).unwrap(),
            1 => stream.dedupe().unwrap(),
            2 => stream.distinct().unwrap(),
            3 => stream.window_count(usize::from(*byte % 8) + 1).unwrap(),
            4 => stream.enumerate().unwrap(),
            5 => {
                let other =
                    StreamVal::from_iter("int", [Ok(Value::Int(i64::from(*byte)))].into_iter());
                stream.merge(other).unwrap()
            }
            _ => {
                let other =
                    StreamVal::from_iter("int", [Ok(Value::Int(i64::from(*byte)))].into_iter());
                stream.zip(other).unwrap()
            }
        };
    }

    let alias = stream.clone();
    let mut context = Context;
    let output = collect_stream(&mut context, &stream).unwrap();
    assert!(output.len() <= 132);
    assert_eq!(
        collect_stream(&mut context, &alias).unwrap_err().code,
        "stream_consumed"
    );
});
