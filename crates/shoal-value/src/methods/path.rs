//! `.save`/`.append` — the pure-method-surface's explicit filesystem sinks —
//! plus the pure (no-IO) `path` component accessors (`.name`/`.stem`/`.ext`/
//! `.parent`/`.join`/`.abs`, docs/CONTRACTS.md §3). The filesystem-backed path
//! methods (`.read`/`.lines`/`.size`/…) live in the evaluator instead, since
//! they need the `Fs` port.

use super::*;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// `.name`/`.stem`/`.ext` — a lossy `str` component of the path, or `null` when
/// the component is absent (e.g. `.ext` of an extensionless name).
pub(crate) fn component(p: &Path, which: &str) -> VResult<Value> {
    let part = match which {
        "name" => p.file_name(),
        "stem" => p.file_stem(),
        "ext" => p.extension(),
        _ => unreachable!("path::component called with `{which}`"),
    };
    Ok(part
        .map(|s| Value::Str(s.to_string_lossy().into_owned()))
        .unwrap_or(Value::Null))
}

/// `.parent` — the parent `path`, or `null` at a filesystem root or for a bare
/// name (whose std parent is the empty path).
pub(crate) fn parent(p: &Path) -> Value {
    match p.parent() {
        Some(par) if !par.as_os_str().is_empty() => Value::Path(par.to_path_buf()),
        _ => Value::Null,
    }
}

/// `.join(seg)` — append a path segment (str or path).
pub(crate) fn join(p: &Path, seg: &Value) -> VResult<Value> {
    let seg = match seg {
        Value::Str(s) => PathBuf::from(s),
        Value::Path(q) => q.clone(),
        v => {
            return Err(ErrorVal::type_error(format!(
                "expected str or path, found {}",
                v.type_name()
            )));
        }
    };
    Ok(Value::Path(p.join(seg)))
}

/// `.abs()` — absolutize against the session cwd (already-absolute paths pass
/// through unchanged).
pub(crate) fn abs(ctx: &mut dyn CallCtx, p: &Path) -> Value {
    Value::Path(if p.is_absolute() {
        p.to_path_buf()
    } else {
        ctx.cwd().join(p)
    })
}

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
