//! `.save`/`.append` — the pure-method-surface's explicit filesystem sinks.

use super::*;
use std::fs::OpenOptions;
use std::io::Write;

pub(crate) fn save(ctx: &mut dyn CallCtx, v: Value, path: &Value, append: bool) -> VResult<Value> {
    let p = match path {
        Value::Path(p) => p.clone(),
        Value::Str(s) => PathBuf::from(s),
        v => {
            return Err(ErrorVal::type_error(format!(
                "expected path, found {}",
                v.type_name()
            )));
        }
    };
    let p = if p.is_absolute() {
        p
    } else {
        ctx.cwd().join(p)
    };
    let bytes = match &v {
        Value::Bytes(b) => (**b).clone(),
        Value::Str(s) => s.as_bytes().to_vec(),
        _ => serde_json::to_vec(&value_to_json(&v))
            .map_err(|e| ErrorVal::new("custom", e.to_string()))?,
    };
    let mut o = OpenOptions::new();
    o.create(true).write(true);
    if append {
        o.append(true)
    } else {
        o.truncate(true)
    };
    o.open(&p)
        .and_then(|mut f| f.write_all(&bytes))
        .map_err(|e| ErrorVal::new("custom", format!("{}: {e}", p.display())))?;
    Ok(v)
}
