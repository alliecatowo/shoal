use super::Evaluator;
use shoal_ast::{CmdArg, CmdCall};
use shoal_value::{ErrorVal, Record, VResult, Value};
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

static TRASH_SEQ: AtomicU64 = AtomicU64::new(1);
const NAMES: &[&str] = &[
    "echo", "ls", "cat", "mkdir", "touch", "cp", "mv", "rm", "stat", "which", "env", "sleep",
];
pub(super) fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name)
}

pub(super) fn run(ev: &mut Evaluator, call: &CmdCall) -> VResult<Value> {
    let mut args = Vec::new();
    let mut flags = Vec::new();
    for arg in &call.args {
        match arg {
            CmdArg::FlagLong { name, .. } => flags.push(name.clone()),
            CmdArg::FlagShort { chars, .. } => flags.extend(chars.chars().map(|c| c.to_string())),
            CmdArg::DashDash { .. } => {}
            _ => args.extend(ev.expand_arg(arg)?),
        }
    }
    dispatch(&call.head, &ev.cwd, &ev.process_env, args, &flags).map_err(|e| e.or_span(call.span))
}

fn dispatch(
    name: &str,
    cwd: &Path,
    penv: &[(OsString, OsString)],
    args: Vec<Value>,
    flags: &[String],
) -> VResult<Value> {
    match name {
        "echo" => Ok(Value::Str(
            args.iter()
                .map(display)
                .collect::<VResult<Vec<_>>>()?
                .join(" "),
        )),
        "ls" => ls(cwd, args, has(flags, &["a", "all"])),
        "cat" => cat(cwd, args),
        "mkdir" => mkdir(cwd, args, has(flags, &["p", "parents"])),
        "touch" => touch(cwd, args),
        "cp" => copy_move(cwd, args, has(flags, &["r", "R", "recursive"]), false),
        "mv" => copy_move(cwd, args, true, true),
        "rm" => rm(
            cwd,
            args,
            has(flags, &["permanent"]),
            has(flags, &["r", "R", "recursive"]),
        ),
        "stat" => stat(cwd, args),
        "which" => which(penv, args),
        "env" => env(penv, args),
        "sleep" => sleep(args),
        _ => Err(ErrorVal::new(
            "not_found",
            format!("unknown builtin {name}"),
        )),
    }
}

fn has(flags: &[String], names: &[&str]) -> bool {
    flags.iter().any(|f| names.contains(&f.as_str()))
}
fn display(v: &Value) -> VResult<String> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        Value::Path(p) => Ok(p.to_string_lossy().into()),
        Value::Int(i) => Ok(i.to_string()),
        Value::Float(f) => Ok(f.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(ErrorVal::type_error(format!(
            "cannot render {} as an argument",
            v.type_name()
        ))),
    }
}
fn path(cwd: &Path, v: Value) -> VResult<PathBuf> {
    let p = match v {
        Value::Path(p) => p,
        Value::Str(s) => s.into(),
        v => {
            return Err(ErrorVal::type_error(format!(
                "expected path, found {}",
                v.type_name()
            )));
        }
    };
    Ok(if p.is_absolute() { p } else { cwd.join(p) })
}
fn paths(cwd: &Path, args: Vec<Value>) -> VResult<Vec<PathBuf>> {
    args.into_iter().map(|v| path(cwd, v)).collect()
}
fn ioerr(op: &str, p: &Path, e: std::io::Error) -> ErrorVal {
    ErrorVal::new("custom", format!("{op} {}: {e}", p.display()))
}

fn metadata_record(p: PathBuf) -> VResult<Record> {
    let m = fs::symlink_metadata(&p).map_err(|e| ioerr("stat", &p, e))?;
    let mut r = Record::new();
    r.insert("path".into(), Value::Path(p.clone()));
    r.insert(
        "name".into(),
        Value::Path(PathBuf::from(
            p.file_name().unwrap_or_else(|| p.as_os_str()),
        )),
    );
    r.insert(
        "type".into(),
        Value::Str(
            if m.is_dir() {
                "dir"
            } else if m.file_type().is_symlink() {
                "symlink"
            } else if m.is_file() {
                "file"
            } else {
                "other"
            }
            .into(),
        ),
    );
    r.insert("size".into(), Value::Size(m.len()));
    let modified = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| Value::Int(d.as_nanos().min(i64::MAX as u128) as i64))
        .unwrap_or(Value::Null);
    r.insert("modified".into(), modified);
    Ok(r)
}

fn ls(cwd: &Path, args: Vec<Value>, all: bool) -> VResult<Value> {
    let roots = if args.is_empty() {
        vec![cwd.to_owned()]
    } else {
        paths(cwd, args)?
    };
    let mut rows = Vec::new();
    for root in roots {
        if root.is_dir() {
            for entry in fs::read_dir(&root).map_err(|e| ioerr("list", &root, e))? {
                let entry = entry.map_err(|e| ioerr("list", &root, e))?;
                if !all && entry.file_name().as_encoded_bytes().starts_with(b".") {
                    continue;
                }
                rows.push(metadata_record(entry.path())?);
            }
        } else {
            rows.push(metadata_record(root)?);
        }
    }
    rows.sort_by(|a, b| match (a.get("path"), b.get("path")) {
        (Some(Value::Path(a)), Some(Value::Path(b))) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    });
    Ok(Value::Table(rows))
}

fn cat(cwd: &Path, args: Vec<Value>) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::arg_error("cat requires at least one path"));
    }
    let mut out = Vec::new();
    for p in paths(cwd, args)? {
        out.extend(fs::read(&p).map_err(|e| ioerr("read", &p, e))?);
    }
    Ok(Value::Bytes(std::sync::Arc::new(out)))
}
fn mkdir(cwd: &Path, args: Vec<Value>, parents: bool) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::arg_error("mkdir requires at least one path"));
    }
    let ps = paths(cwd, args)?;
    for p in &ps {
        if parents {
            fs::create_dir_all(p)
        } else {
            fs::create_dir(p)
        }
        .map_err(|e| ioerr("mkdir", p, e))?;
    }
    Ok(Value::List(ps.into_iter().map(Value::Path).collect()))
}
fn touch(cwd: &Path, args: Vec<Value>) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::arg_error("touch requires at least one path"));
    }
    let ps = paths(cwd, args)?;
    for p in &ps {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .map_err(|e| ioerr("touch", p, e))?;
    }
    Ok(Value::List(ps.into_iter().map(Value::Path).collect()))
}

fn copy_move(cwd: &Path, args: Vec<Value>, recursive: bool, moving: bool) -> VResult<Value> {
    if args.len() < 2 {
        return Err(ErrorVal::arg_error(if moving {
            "mv requires source and destination"
        } else {
            "cp requires source and destination"
        }));
    }
    let mut ps = paths(cwd, args)?;
    let dest = ps.pop().expect("length checked");
    if ps.len() > 1 && !dest.is_dir() {
        return Err(ErrorVal::arg_error(
            "destination must be a directory for multiple sources",
        ));
    }
    let mut out = Vec::new();
    for src in ps {
        let target = if dest.is_dir() {
            dest.join(
                src.file_name()
                    .ok_or_else(|| ErrorVal::arg_error("source has no name"))?,
            )
        } else {
            dest.clone()
        };
        if moving {
            fs::rename(&src, &target).map_err(|e| ioerr("move", &src, e))?;
        } else {
            copy_path(&src, &target, recursive)?;
        }
        out.push(Value::Path(target));
    }
    Ok(Value::List(out))
}
fn copy_path(src: &Path, dst: &Path, recursive: bool) -> VResult<()> {
    let m = fs::symlink_metadata(src).map_err(|e| ioerr("copy", src, e))?;
    if m.is_dir() {
        if !recursive {
            return Err(ErrorVal::arg_error("cp: directory requires --recursive"));
        }
        fs::create_dir_all(dst).map_err(|e| ioerr("copy", dst, e))?;
        for e in fs::read_dir(src).map_err(|e| ioerr("copy", src, e))? {
            let e = e.map_err(|e| ioerr("copy", src, e))?;
            copy_path(&e.path(), &dst.join(e.file_name()), true)?
        }
    } else {
        fs::copy(src, dst).map_err(|e| ioerr("copy", src, e))?;
    }
    Ok(())
}

fn rm(cwd: &Path, args: Vec<Value>, permanent: bool, recursive: bool) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::new(
            "no_matches",
            "rm requires at least one path; an empty glob deletes nothing",
        ));
    }
    let ps = paths(cwd, args)?;
    let trash = std::env::temp_dir()
        .join("shoal-trash")
        .join(std::process::id().to_string());
    if !permanent {
        fs::create_dir_all(&trash).map_err(|e| ioerr("trash", &trash, e))?;
    }
    let mut out = Vec::new();
    for p in ps {
        let meta = fs::symlink_metadata(&p).map_err(|e| ioerr("remove", &p, e))?;
        if permanent {
            if meta.is_dir() {
                if !recursive {
                    return Err(ErrorVal::arg_error("rm: directory requires --recursive"));
                }
                fs::remove_dir_all(&p)
            } else {
                fs::remove_file(&p)
            }
            .map_err(|e| ioerr("remove", &p, e))?;
            out.push(Value::Path(p));
        } else {
            let seq = TRASH_SEQ.fetch_add(1, Ordering::Relaxed);
            let name = p
                .file_name()
                .unwrap_or_else(|| OsStr::new("item"))
                .to_string_lossy();
            let target = trash.join(format!("{seq}-{name}"));
            fs::rename(&p, &target).map_err(|e| ioerr("trash", &p, e))?;
            let mut r = Record::new();
            r.insert("path".into(), Value::Path(p));
            r.insert("trash".into(), Value::Path(target));
            out.push(Value::Record(r));
        }
    }
    Ok(Value::List(out))
}
fn stat(cwd: &Path, args: Vec<Value>) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::arg_error("stat requires at least one path"));
    }
    let rows = paths(cwd, args)?
        .into_iter()
        .map(metadata_record)
        .collect::<VResult<Vec<_>>>()?;
    if rows.len() == 1 {
        Ok(Value::Record(rows.into_iter().next().expect("one row")))
    } else {
        Ok(Value::Table(rows))
    }
}
fn which(penv: &[(OsString, OsString)], args: Vec<Value>) -> VResult<Value> {
    if args.len() != 1 {
        return Err(ErrorVal::arg_error("which requires exactly one command"));
    }
    let name = display(&args[0])?;
    let path = penv
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_os_str());
    Ok(shoal_exec::which(OsStr::new(&name), path)
        .map(Value::Path)
        .unwrap_or(Value::Null))
}
fn env(penv: &[(OsString, OsString)], args: Vec<Value>) -> VResult<Value> {
    if args.is_empty() {
        let mut r = Record::new();
        for (k, v) in penv {
            if let (Some(k), Some(v)) = (k.to_str(), v.to_str()) {
                r.insert(k.into(), Value::Str(v.into()));
            }
        }
        Ok(Value::Record(r))
    } else if args.len() == 1 {
        let key = display(&args[0])?;
        Ok(penv
            .iter()
            .find(|(k, _)| k == &OsString::from(&key))
            .map(|(_, v)| Value::Str(v.to_string_lossy().into()))
            .unwrap_or(Value::Null))
    } else {
        Err(ErrorVal::arg_error("env accepts zero or one name"))
    }
}
fn sleep(args: Vec<Value>) -> VResult<Value> {
    if args.len() != 1 {
        return Err(ErrorVal::arg_error("sleep requires one duration"));
    }
    let d = match args[0] {
        Value::Duration(ns) if ns >= 0 => Duration::from_nanos(ns as u64),
        Value::Int(s) if s >= 0 => Duration::from_secs(s as u64),
        _ => {
            return Err(ErrorVal::type_error(
                "sleep expects a non-negative duration",
            ));
        }
    };
    std::thread::sleep(d);
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pe() -> Vec<(OsString, OsString)> {
        std::env::vars_os().collect()
    }
    #[test]
    fn empty_rm_is_safe() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            dispatch("rm", d.path(), &pe(), vec![], &[])
                .unwrap_err()
                .code,
            "no_matches"
        );
    }
    #[test]
    fn rm_trashes_by_default() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("x"), b"x").unwrap();
        let Value::List(xs) =
            dispatch("rm", d.path(), &pe(), vec![Value::Path("x".into())], &[]).unwrap()
        else {
            panic!()
        };
        assert!(!d.path().join("x").exists());
        let Value::Record(r) = &xs[0] else { panic!() };
        let Value::Path(t) = &r["trash"] else {
            panic!()
        };
        assert!(t.exists());
    }
    #[test]
    fn ls_preserves_non_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let d = tempfile::tempdir().unwrap();
        let name = OsString::from_vec(vec![b'f', 0xff]);
        fs::write(d.path().join(&name), b"abc").unwrap();
        let Value::Table(rows) = ls(d.path(), vec![], false).unwrap() else {
            panic!()
        };
        assert!(matches!(&rows[0]["name"],Value::Path(p)if p.as_os_str()==name));
        assert_eq!(rows[0]["size"], Value::Size(3));
    }
    #[test]
    fn typed_fs_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), vec![Value::Path("a".into())]).unwrap();
        fs::write(d.path().join("a"), b"hello").unwrap();
        assert!(
            matches!(cat(d.path(),vec![Value::Path("a".into())]).unwrap(),Value::Bytes(b)if &*b==b"hello")
        );
        copy_move(
            d.path(),
            vec![Value::Path("a".into()), Value::Path("b".into())],
            false,
            false,
        )
        .unwrap();
        assert!(d.path().join("b").exists());
    }
}
