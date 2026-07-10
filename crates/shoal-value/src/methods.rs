//! Value-method stdlib — `.where .sort .map .lines …` (TDD §5).
//!
//! STUB: the full method set is being built to the contract in
//! docs/CONTRACTS.md §3/§7. The dispatch signature below is pinned.

use crate::{CallArgs, CallCtx, ErrorVal, VResult, Value};
use shoal_ast::Span;

/// Dispatch a method call on a value. Unknown methods are `field_missing`.
pub fn call_method(
    _ctx: &mut dyn CallCtx,
    recv: Value,
    name: &str,
    _args: CallArgs,
    span: Span,
) -> VResult<Value> {
    match (recv, name) {
        (Value::List(xs), "len") => Ok(Value::Int(xs.len() as i64)),
        (Value::Str(s), "len") => Ok(Value::Int(s.chars().count() as i64)),
        (recv, _) => Err(ErrorVal::new(
            "field_missing",
            format!("unknown method `.{name}` on {}", recv.type_name()),
        )
        .with_span(span)),
    }
}
