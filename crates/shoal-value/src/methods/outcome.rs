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
        "stdout" => Ok(Value::Bytes(o.stdout.clone())),
        "stderr" => Ok(Value::Bytes(o.stderr.clone())),
        _ => super::dispatch(ctx, o.out_value(), name, args),
    }
}
