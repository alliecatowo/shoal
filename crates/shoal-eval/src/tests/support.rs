use super::*;

pub(super) fn run(src: &str) -> VResult<Value> {
    let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
    eval(&program, std::env::current_dir().unwrap())
}

/// Evaluate `src` capturing everything routed to the statement sink.
pub(super) fn run_capturing(src: &str) -> (VResult<Value>, Vec<Value>) {
    use std::sync::{Arc, Mutex};
    let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
    let mut ev = Evaluator::new(std::env::current_dir().unwrap());
    let sink: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink2 = sink.clone();
    ev.set_statement_sink(Box::new(move |v: &Value| {
        sink2.lock().unwrap().push(v.clone())
    }));
    let out = ev.eval_program(&program);
    drop(ev); // release the sink's Arc clone before unwrapping
    let captured = Arc::try_unwrap(sink).unwrap().into_inner().unwrap();
    (out, captured)
}

pub(super) fn run_in(src: &str, cwd: &Path) -> VResult<Value> {
    let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
    eval(&program, cwd)
}

/// The structured `.out` of a captured command outcome.
pub(super) fn out_of(v: &Value) -> Value {
    match v {
        Value::Outcome(o) => o.out_value(),
        other => other.clone(),
    }
}

/// `run_capturing`, but in an explicit cwd (for glob/fixture tests).
pub(super) fn run_capturing_in(src: &str, cwd: &Path) -> (VResult<Value>, Vec<Value>) {
    use std::sync::{Arc, Mutex};
    let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
    let mut ev = Evaluator::new(cwd.to_path_buf());
    let sink: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink2 = sink.clone();
    ev.set_statement_sink(Box::new(move |v: &Value| {
        sink2.lock().unwrap().push(v.clone())
    }));
    let out = ev.eval_program(&program);
    drop(ev);
    let captured = Arc::try_unwrap(sink).unwrap().into_inner().unwrap();
    (out, captured)
}
