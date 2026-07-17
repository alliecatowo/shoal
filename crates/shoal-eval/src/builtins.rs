use super::Evaluator;
use shoal_ast::{CmdArg, CmdCall};
use shoal_exec::CancelToken;
use shoal_value::{ErrorVal, Fs, Record, VResult, Value};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// The canonical builtin command-head registry lives in the leaf `shoal-syntax`
// crate (`shoal_syntax::commands`) — "is this token a command head?" is a
// lexical/syntactic classification every consumer (eval, the shell, the LSP)
// already links `shoal-syntax` for, so the LSP needn't pull the whole evaluator
// in for a name list. Eval keeps its dispatch logic below (`run`/`dispatch`,
// `eval_command`'s special-head guards) and just sources the name list here.
pub(crate) use shoal_syntax::commands::builtin_names;
#[cfg(test)]
pub(crate) use shoal_syntax::commands::{is_builtin, is_special_head};

static TRASH_SEQ: AtomicU64 = AtomicU64::new(1);
static TRASH_SESSION: OnceLock<String> = OnceLock::new();
const TRASH_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const TRASH_PRUNE_SCAN_LIMIT: usize = 64;

/// A builtin signature (defect #12): scalar param types by index, plus an
/// optional variadic type applied to any remaining positional words. `None`
/// leaves words verbatim (→ str). The same word→type coercion machinery user
/// fns use (see `coerce_word`) is applied here so `sleep 1` and `sleep 10ms`
/// both bind.
fn builtin_variadic_ty(name: &str) -> Option<&'static str> {
    match name {
        "ls" | "cat" | "mkdir" | "touch" | "cp" | "mv" | "rm" | "stat" => Some("path"),
        "sleep" => Some("duration"),
        "which" | "env" => Some("str"),
        // `echo` takes any values verbatim (rendered on display).
        _ => None,
    }
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
    if let Some(ty) = builtin_variadic_ty(&call.head) {
        args = args
            .into_iter()
            .map(|v| super::coerce_word(v, ty))
            .collect::<VResult<Vec<_>>>()
            .map_err(|e| e.or_span(call.span))?;
    }
    let fs = ev.host.fs.clone();
    dispatch(
        &call.head,
        fs.as_ref(),
        &ev.exec.shell.cwd,
        &ev.exec.shell.process_env,
        args,
        &flags,
        &ev.exec.control.cancel,
    )
    .map_err(|e| e.or_span(call.span))
}

fn dispatch(
    name: &str,
    fs: &dyn Fs,
    cwd: &Path,
    penv: &[(OsString, OsString)],
    args: Vec<Value>,
    flags: &[String],
    cancel: &CancelToken,
) -> VResult<Value> {
    match name {
        // echo renders every value (lists/records/tables/null included), strings
        // unquoted at top level (site/content/internals/pty-job-control.md).
        "echo" => Ok(Value::Str(
            args.iter().map(echo_display).collect::<Vec<_>>().join(" "),
        )),
        "ls" => ls(fs, cwd, args, has(flags, &["a", "all"])),
        "cat" => cat(fs, cwd, args),
        "mkdir" => mkdir(fs, cwd, args, has(flags, &["p", "parents"])),
        "touch" => touch(fs, cwd, args),
        "cp" => copy_move(fs, cwd, args, has(flags, &["r", "R", "recursive"]), false),
        "mv" => copy_move(fs, cwd, args, true, true),
        "rm" => rm(
            fs,
            cwd,
            args,
            has(flags, &["permanent"]),
            has(flags, &["r", "R", "recursive"]),
        ),
        "stat" => stat(fs, cwd, args),
        "head" => head(fs, cwd, args),
        "ln" => ln(fs, cwd, args, has(flags, &["s", "symbolic"])),
        "which" => which(penv, args),
        "env" => env(penv, args),
        "sleep" => sleep(args, cancel),
        _ => Err(ErrorVal::new(
            "not_found",
            format!("unknown builtin {name}"),
        )),
    }
}

fn has(flags: &[String], names: &[&str]) -> bool {
    flags.iter().any(|f| names.contains(&f.as_str()))
}
/// Top-level display for `echo`: scalars/paths unquoted, everything else via
/// `render_inline` (site/content/internals/pty-job-control.md — lists/records/tables all printable).
fn echo_display(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Path(p) => p.to_string_lossy().into_owned(),
        Value::Null => String::new(),
        other => shoal_value::render::render_inline(other),
    }
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

fn metadata_record(fs: &dyn Fs, p: PathBuf) -> VResult<Record> {
    let m = fs.symlink_metadata(&p).map_err(|e| ioerr("stat", &p, e))?;
    let mut r = Record::new();
    r.insert("path".into(), Value::Path(p.clone()));
    // `name` is the basename as a STRING — a filename you can `.upper()`,
    // `.split(".")`, interpolate, or `+`-concat. The full `path` field stays a
    // `path`. (Was a `Value::Path`, which made every string op on a row's name
    // — `.map(.name.upper())`, `"pre" + row.name` — a type_error.)
    r.insert(
        "name".into(),
        Value::Str(
            p.file_name()
                .unwrap_or_else(|| p.as_os_str())
                .to_string_lossy()
                .into_owned(),
        ),
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
    // `modified` is a real DateTime (defect #4), built from the UNIX epoch.
    let modified = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| jiff::Timestamp::from_nanosecond(d.as_nanos() as i128).ok())
        .map(|ts| Value::DateTime(Box::new(ts.to_zoned(jiff::tz::TimeZone::system()))))
        .unwrap_or(Value::Null);
    r.insert("modified".into(), modified);
    Ok(r)
}

fn ls(fs: &dyn Fs, cwd: &Path, args: Vec<Value>, all: bool) -> VResult<Value> {
    let roots = if args.is_empty() {
        vec![cwd.to_owned()]
    } else {
        paths(cwd, args)?
    };
    let mut rows = Vec::new();
    for root in roots {
        if root.is_dir() {
            for entry in fs.read_dir(&root).map_err(|e| ioerr("list", &root, e))? {
                if !all
                    && entry
                        .file_name()
                        .is_some_and(|n| n.as_encoded_bytes().starts_with(b"."))
                {
                    continue;
                }
                rows.push(metadata_record(fs, entry)?);
            }
        } else {
            rows.push(metadata_record(fs, root)?);
        }
    }
    rows.sort_by(|a, b| match (a.get("path"), b.get("path")) {
        (Some(Value::Path(a)), Some(Value::Path(b))) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    });
    Ok(Value::Table(rows))
}

fn cat(fs: &dyn Fs, cwd: &Path, args: Vec<Value>) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::arg_error("cat requires at least one path"));
    }
    let mut out = Vec::new();
    for p in paths(cwd, args)? {
        out.extend(fs.read(&p).map_err(|e| ioerr("read", &p, e))?);
    }
    Ok(Value::Bytes(std::sync::Arc::new(out)))
}
fn mkdir(fs: &dyn Fs, cwd: &Path, args: Vec<Value>, parents: bool) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::arg_error("mkdir requires at least one path"));
    }
    let ps = paths(cwd, args)?;
    for p in &ps {
        if parents {
            fs.create_dir_all(p)
        } else {
            fs.create_dir(p)
        }
        .map_err(|e| ioerr("mkdir", p, e))?;
    }
    Ok(Value::List(ps.into_iter().map(Value::Path).collect()))
}
fn touch(fs: &dyn Fs, cwd: &Path, args: Vec<Value>) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::arg_error("touch requires at least one path"));
    }
    let ps = paths(cwd, args)?;
    for p in &ps {
        fs.touch(p).map_err(|e| ioerr("touch", p, e))?;
    }
    Ok(Value::List(ps.into_iter().map(Value::Path).collect()))
}

fn copy_move(
    fs: &dyn Fs,
    cwd: &Path,
    args: Vec<Value>,
    recursive: bool,
    moving: bool,
) -> VResult<Value> {
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
            fs.rename(&src, &target)
                .map_err(|e| ioerr("move", &src, e))?;
        } else {
            copy_path(fs, &src, &target, recursive)?;
        }
        out.push(Value::Path(target));
    }
    Ok(Value::List(out))
}
fn copy_path(fs: &dyn Fs, src: &Path, dst: &Path, recursive: bool) -> VResult<()> {
    let m = fs
        .symlink_metadata(src)
        .map_err(|e| ioerr("copy", src, e))?;
    if m.is_dir() {
        if !recursive {
            return Err(ErrorVal::arg_error("cp: directory requires --recursive"));
        }
        fs.create_dir_all(dst).map_err(|e| ioerr("copy", dst, e))?;
        for e in fs.read_dir(src).map_err(|e| ioerr("copy", src, e))? {
            let name = e
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&e));
            copy_path(fs, &e, &dst.join(name), true)?
        }
    } else {
        fs.copy(src, dst).map_err(|e| ioerr("copy", src, e))?;
    }
    Ok(())
}

fn rm(
    fs: &dyn Fs,
    cwd: &Path,
    args: Vec<Value>,
    permanent: bool,
    recursive: bool,
) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::new(
            "no_matches",
            "rm requires at least one path; an empty glob deletes nothing",
        ));
    }
    let ps = paths(cwd, args)?;
    let mut cleanup_warnings = Vec::new();
    let primary_trash = if permanent {
        None
    } else {
        let root = shoal_paths::ShoalPaths::discover()
            .runtime_dir()
            .join("shoal")
            .join("trash");
        match prepare_trash_session(fs, &root, &mut cleanup_warnings) {
            Ok(path) => Some(path),
            Err(error) => {
                cleanup_warnings.push(format!(
                    "central trash unavailable at {}: {error}; using a same-filesystem trash",
                    root.display()
                ));
                None
            }
        }
    };
    let mut out = Vec::new();
    for p in ps {
        let meta = fs
            .symlink_metadata(&p)
            .map_err(|e| ioerr("remove", &p, e))?;
        if permanent {
            if meta.is_dir() {
                if !recursive {
                    return Err(ErrorVal::arg_error("rm: directory requires --recursive"));
                }
                fs.remove_dir_all(&p)
            } else {
                fs.remove_file(&p)
            }
            .map_err(|e| ioerr("remove", &p, e))?;
            out.push(Value::Path(p));
        } else {
            let seq = TRASH_SEQ.fetch_add(1, Ordering::Relaxed);
            let name = p
                .file_name()
                .unwrap_or_else(|| OsStr::new("item"))
                .to_string_lossy();
            let entry_name = format!("{seq}-{name}");
            let primary_target = primary_trash.as_ref().map(|root| root.join(&entry_name));
            let target = move_to_trash(
                &p,
                primary_target,
                |source, target| fs.rename(source, target),
                || prepare_adjacent_trash(fs, &p, &entry_name, &mut cleanup_warnings),
            )?;
            let mut r = Record::new();
            r.insert("path".into(), Value::Path(p));
            r.insert("trash".into(), Value::Path(target));
            r.insert(
                "trash_retention_days".into(),
                Value::Int((TRASH_RETENTION.as_secs() / 86_400) as i64),
            );
            if !cleanup_warnings.is_empty() {
                r.insert(
                    "trash_cleanup_warnings".into(),
                    Value::List(cleanup_warnings.iter().cloned().map(Value::Str).collect()),
                );
            }
            out.push(Value::Record(r));
        }
    }
    Ok(Value::List(out))
}

fn move_to_trash(
    source: &Path,
    primary_target: Option<PathBuf>,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    mut adjacent_target: impl FnMut() -> VResult<PathBuf>,
) -> VResult<PathBuf> {
    if let Some(target) = primary_target {
        match rename(source, &target) {
            Ok(()) => return Ok(target),
            Err(error) if !is_cross_device(&error) => {
                return Err(ioerr("trash", source, error));
            }
            Err(_) => {}
        }
    }

    // A rename into a trash directory beside the source stays on the source
    // filesystem. It is atomic and preserves directories, symlinks, metadata,
    // and journal undo without the partial-copy states of a recursive EXDEV
    // fallback.
    let target = adjacent_target()?;
    rename(source, &target).map_err(|error| ioerr("trash", source, error))?;
    Ok(target)
}

fn trash_session_name() -> &'static str {
    TRASH_SESSION.get_or_init(|| {
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}-{started:032x}", std::process::id())
    })
}

fn prepare_trash_session(
    fs: &dyn Fs,
    root: &Path,
    warnings: &mut Vec<String>,
) -> std::io::Result<PathBuf> {
    fs.create_private_dir_all(root)?;
    validate_private_trash_dir(fs, root)?;
    warnings.extend(prune_stale_trash_root(
        fs,
        root,
        trash_session_name(),
        TRASH_RETENTION,
        TRASH_PRUNE_SCAN_LIMIT,
    ));
    let session = root.join(trash_session_name());
    fs.create_private_dir_all(&session)?;
    validate_private_trash_dir(fs, &session)?;
    Ok(session)
}

fn prepare_adjacent_trash(
    fs: &dyn Fs,
    source: &Path,
    entry_name: &str,
    warnings: &mut Vec<String>,
) -> VResult<PathBuf> {
    let parent = source.parent().ok_or_else(|| {
        ErrorVal::new(
            "io_error",
            format!("trash: {} has no parent directory", source.display()),
        )
    })?;
    let root = parent.join(adjacent_trash_name());
    let session =
        prepare_trash_session(fs, &root, warnings).map_err(|error| ioerr("trash", &root, error))?;
    Ok(session.join(entry_name))
}

#[cfg(unix)]
fn adjacent_trash_name() -> String {
    // SAFETY: `geteuid` has no preconditions and returns the effective UID.
    format!(".shoal-trash-{}", unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
fn adjacent_trash_name() -> String {
    ".shoal-trash".into()
}

#[cfg(unix)]
fn validate_private_trash_dir(fs: &dyn Fs, path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs.symlink_metadata(path)?;
    // SAFETY: `geteuid` has no preconditions and returns the effective UID.
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != effective_uid || metadata.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "trash directory {} must be owned by uid {effective_uid} with mode 0700",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_trash_dir(fs: &dyn Fs, path: &Path) -> std::io::Result<()> {
    if fs.symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("trash directory {} is not a directory", path.display()),
        ))
    }
}

fn is_cross_device(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::EXDEV)
}

fn prune_stale_trash_root(
    fs: &dyn Fs,
    root: &Path,
    current_session: &str,
    retention: Duration,
    scan_limit: usize,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let entries = match fs.read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(format!(
                "cannot scan trash retention at {}: {error}",
                root.display()
            ));
            return warnings;
        }
    };
    let now = SystemTime::now();
    for entry in entries.into_iter().take(scan_limit) {
        if entry.file_name() == Some(OsStr::new(current_session)) {
            continue;
        }
        let metadata = match fs.symlink_metadata(&entry) {
            Ok(metadata) => metadata,
            Err(error) => {
                warnings.push(format!(
                    "cannot inspect trash entry {}: {error}",
                    entry.display()
                ));
                continue;
            }
        };
        if !metadata.is_dir()
            || metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_none_or(|age| age < retention)
        {
            continue;
        }
        if let Err(error) = fs.remove_dir_all(&entry) {
            warnings.push(format!(
                "cannot prune trash entry {}: {error}",
                entry.display()
            ));
        }
    }
    warnings
}
fn stat(fs: &dyn Fs, cwd: &Path, args: Vec<Value>) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::arg_error("stat requires at least one path"));
    }
    let rows = paths(cwd, args)?
        .into_iter()
        .map(|p| metadata_record(fs, p))
        .collect::<VResult<Vec<_>>>()?;
    if rows.len() == 1 {
        Ok(Value::Record(rows.into_iter().next().expect("one row")))
    } else {
        Ok(Value::Table(rows))
    }
}
/// `head(file, n: int = 10) -> list<str>` (site/content/internals/language-conformance-contract.md): the first `n` lines of a
/// text file, structured. UTF-8 is read lossily so a stray non-UTF-8 byte never
/// aborts the read.
fn head(fs: &dyn Fs, cwd: &Path, args: Vec<Value>) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::arg_error("head requires a file path"));
    }
    let n = match args.get(1) {
        None => 10usize,
        Some(Value::Int(i)) if *i >= 0 => *i as usize,
        Some(Value::Str(s)) => s.parse::<usize>().map_err(|_| {
            ErrorVal::arg_error(format!("head: expected a line count, found {s:?}"))
        })?,
        Some(v) => {
            return Err(ErrorVal::type_error(format!(
                "head: expected an int line count, found {}",
                v.type_name()
            )));
        }
    };
    let p = path(cwd, args[0].clone())?;
    let bytes = fs.read(&p).map_err(|e| ioerr("read", &p, e))?;
    let text = String::from_utf8_lossy(&bytes);
    let lines = text
        .lines()
        .take(n)
        .map(|l| Value::Str(l.to_string()))
        .collect();
    Ok(Value::List(lines))
}

/// `ln(target, link, symbolic: bool = false)` (site/content/internals/language-conformance-contract.md): create a hard link (or a
/// symlink with `--symbolic`/`-s`). Returns a record describing the link created.
fn ln(fs: &dyn Fs, cwd: &Path, args: Vec<Value>, symbolic: bool) -> VResult<Value> {
    if args.len() != 2 {
        return Err(ErrorVal::arg_error("ln requires a target and a link name"));
    }
    let link = path(cwd, args[1].clone())?;
    let mut r = Record::new();
    if symbolic {
        // Preserve the target verbatim so a relative symlink points where the
        // user meant (relative to the link's directory), not to the cwd.
        let target = match &args[0] {
            Value::Path(p) => p.clone(),
            Value::Str(s) => PathBuf::from(s),
            v => {
                return Err(ErrorVal::type_error(format!(
                    "ln: expected a path target, found {}",
                    v.type_name()
                )));
            }
        };
        fs.symlink(&target, &link)
            .map_err(|e| ioerr("symlink", &link, e))?;
        r.insert("target".into(), Value::Path(target));
    } else {
        let target = path(cwd, args[0].clone())?;
        fs.hard_link(&target, &link)
            .map_err(|e| ioerr("link", &link, e))?;
        r.insert("target".into(), Value::Path(target));
    }
    r.insert("link".into(), Value::Path(link));
    r.insert("symbolic".into(), Value::Bool(symbolic));
    Ok(Value::Record(r))
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
fn sleep(args: Vec<Value>, cancel: &CancelToken) -> VResult<Value> {
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
    // Poll the cancel token in small increments so Ctrl-C shortens the sleep
    // (site/content/internals/language-conformance-contract.md): an un-cancellable sleep froze the foreground on interrupt.
    let deadline = Instant::now() + d;
    let step = Duration::from_millis(50);
    loop {
        if cancel.is_cancelled() {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep((deadline - now).min(step));
    }
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoal_value::StdFs;
    fn pe() -> Vec<(OsString, OsString)> {
        std::env::vars_os().collect()
    }
    // The canonical builtin-registry pin (completeness/sorted/deduped/count)
    // moved with the list itself to `shoal_syntax::commands`; the membership
    // gates re-exported here (`is_builtin`/`is_special_head`) still route
    // dispatch, exercised by the fs-family tests below and `command.rs`.
    #[test]
    fn empty_rm_is_safe() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            dispatch(
                "rm",
                &StdFs,
                d.path(),
                &pe(),
                vec![],
                &[],
                &CancelToken::new()
            )
            .unwrap_err()
            .code,
            "no_matches"
        );
    }
    #[test]
    fn rm_trashes_by_default() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), b"x").unwrap();
        let Value::List(xs) = dispatch(
            "rm",
            &StdFs,
            d.path(),
            &pe(),
            vec![Value::Path("x".into())],
            &[],
            &CancelToken::new(),
        )
        .unwrap() else {
            panic!()
        };
        assert!(!d.path().join("x").exists());
        let Value::Record(r) = &xs[0] else { panic!() };
        let Value::Path(t) = &r["trash"] else {
            panic!()
        };
        assert!(t.exists());
        assert_eq!(r["trash_retention_days"], Value::Int(30));
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let parent = std::fs::symlink_metadata(t.parent().unwrap()).unwrap();
            assert_eq!(parent.mode() & 0o077, 0);
            // SAFETY: `geteuid` has no preconditions.
            assert_eq!(parent.uid(), unsafe { libc::geteuid() });
        }
    }
    #[test]
    fn trash_falls_back_to_an_atomic_adjacent_rename_on_exdev() {
        let source = Path::new("/source/item");
        let primary = PathBuf::from("/runtime/trash/item");
        let adjacent = PathBuf::from("/source/.shoal-trash/session/item");
        let mut calls = Vec::new();

        let selected = move_to_trash(
            source,
            Some(primary.clone()),
            |from, to| {
                calls.push((from.to_path_buf(), to.to_path_buf()));
                if to == primary {
                    Err(std::io::Error::from_raw_os_error(libc::EXDEV))
                } else {
                    Ok(())
                }
            },
            || Ok(adjacent.clone()),
        )
        .unwrap();

        assert_eq!(selected, adjacent);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, primary);
        assert_eq!(calls[1].1, selected);
    }
    #[test]
    fn trash_retention_scan_is_bounded_and_preserves_current_session() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("trash");
        std::fs::create_dir(&root).unwrap();
        for name in ["current", "old-a", "old-b", "old-c", "old-d"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }

        let warnings = prune_stale_trash_root(&StdFs, &root, "current", Duration::ZERO, 2);
        assert!(warnings.is_empty());
        let remaining_after_bounded_scan = std::fs::read_dir(&root).unwrap().count();
        assert!(
            remaining_after_bounded_scan >= 3,
            "a two-entry scan removed too many of five sessions"
        );

        let warnings = prune_stale_trash_root(&StdFs, &root, "current", Duration::ZERO, usize::MAX);
        assert!(warnings.is_empty());
        assert!(root.join("current").is_dir());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }
    #[test]
    fn trash_retention_failures_are_reported() {
        let d = tempfile::tempdir().unwrap();
        let not_a_directory = d.path().join("file");
        std::fs::write(&not_a_directory, b"x").unwrap();

        let warnings =
            prune_stale_trash_root(&StdFs, &not_a_directory, "current", Duration::ZERO, 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("cannot scan trash retention"));
    }
    #[cfg(unix)]
    #[test]
    fn trash_rejects_a_symlinked_or_public_directory() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        let linked = d.path().join("linked");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &linked).unwrap();
        assert_eq!(
            validate_private_trash_dir(&StdFs, &linked)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );

        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            validate_private_trash_dir(&StdFs, &real)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
    // Linux allows arbitrary bytes in filenames; macOS (APFS/HFS+) rejects
    // non-UTF-8 names at the syscall, so the fixture can't be created there.
    // shoal's path handling stays bytes-backed regardless (site/content/internals/language-conformance-contract.md); this
    // test just needs a filesystem that can hold the bytes.
    #[cfg(target_os = "linux")]
    #[test]
    fn ls_preserves_non_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let d = tempfile::tempdir().unwrap();
        let name = OsString::from_vec(vec![b'f', 0xff]);
        std::fs::write(d.path().join(&name), b"abc").unwrap();
        let Value::Table(rows) = ls(&StdFs, d.path(), vec![], false).unwrap() else {
            panic!()
        };
        // `name` is now a lossy STRING (the exact bytes stay on `path`); a
        // non-UTF-8 byte becomes the replacement char.
        assert_eq!(
            rows[0]["name"],
            Value::Str(name.to_string_lossy().into_owned())
        );
        assert!(matches!(&rows[0]["path"], Value::Path(p) if p.file_name() == Some(&name)));
        assert_eq!(rows[0]["size"], Value::Size(3));
    }
    #[test]
    fn typed_fs_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        touch(&StdFs, d.path(), vec![Value::Path("a".into())]).unwrap();
        std::fs::write(d.path().join("a"), b"hello").unwrap();
        assert!(
            matches!(cat(&StdFs, d.path(),vec![Value::Path("a".into())]).unwrap(),Value::Bytes(b)if &*b==b"hello")
        );
        copy_move(
            &StdFs,
            d.path(),
            vec![Value::Path("a".into()), Value::Path("b".into())],
            false,
            false,
        )
        .unwrap();
        assert!(d.path().join("b").exists());
    }
    #[test]
    fn sleep_returns_promptly_when_pre_cancelled() {
        // A pre-cancelled token makes even a long sleep return immediately
        // (Ctrl-C shortens `sleep`, site/content/internals/language-conformance-contract.md).
        let cancel = CancelToken::new();
        cancel.cancel();
        let start = Instant::now();
        assert_eq!(sleep(vec![Value::Int(30)], &cancel).unwrap(), Value::Null);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "cancelled sleep should return promptly, took {:?}",
            start.elapsed()
        );
    }
}
