//! Outcome method forwarding (P1b unification): an unknown method on a
//! command outcome forwards to its structured `.out`, so
//! `ls.where(.size > 1b).sort(.name)` works (`ls` is an outcome; `.where`/
//! `.sort` operate on its `.out` table). Raw stream bytes stay reachable via
//! `.stdout`/`.stderr`.

use super::*;

pub(crate) fn forward(
    ctx: &mut dyn CallCtx,
    o: &OutcomeVal,
    name: &str,
    args: CallArgs,
) -> VResult<Value> {
    match name {
        // site/content/internals/language-conformance-contract.md: a spilled capture surfaces `.stdout` as a lazy, ref-backed
        // `bytes`; ordinary output is the resident `bytes` as before.
        "stdout" => Ok(o.stdout_value()),
        "stderr" => Ok(Value::Bytes(o.stderr.clone())),
        _ => super::dispatch(ctx, o.out_value(), name, args),
    }
}
