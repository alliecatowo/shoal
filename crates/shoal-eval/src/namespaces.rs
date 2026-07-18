//! Data & system namespaces as first-class values (site/content/internals/language-conformance-contract.md, site/content/internals/roadmap-and-priorities.md). Each
//! namespace (`json`, `yaml`, `toml`, `csv`, `math`, `http`, `os`, `config`) is a
//! name in the root env exposing constants (field access, e.g. `math.pi`) and
//! functions (method calls, e.g. `json.parse(s)`, `http.get(url)`). The evaluator
//! intercepts `<ns>.member` and `<ns>.member(...)` before generic var/field/
//! method dispatch (see `expr.rs`), so no new `Value` variant is needed — a
//! namespace has no runtime representation of its own, only its members do.

use crate::Evaluator;
use shoal_value::{CallArgs, ErrorVal, Record, VResult, Value, json_to_value, value_to_json};
use std::time::Duration;

/// The namespace names intercepted before ordinary variable resolution. A name
/// bound in the environment (a user `let`/`fn`) always shadows these — the
/// intercept only fires when the name is otherwise unbound.
pub(crate) fn is_namespace(name: &str) -> bool {
    matches!(
        name,
        "json" | "yaml" | "toml" | "csv" | "math" | "http" | "os" | "config"
    )
}

/// `<ns>.member` field access (no call): namespace constants (`math.pi`) and the
/// `config` projection. A member that is really a function (`json.parse`) is a
/// field-missing error here — it must be *called*.
pub(crate) fn field(ev: &Evaluator, ns: &str, member: &str) -> VResult<Value> {
    match ns {
        "math" => math_const(member),
        "config" => config_get(ev, member),
        _ => Err(ErrorVal::new(
            "field_missing",
            format!("`{ns}.{member}` is a function — call it, e.g. `{ns}.{member}(...)`"),
        )),
    }
}

/// `<ns>.method(args)` call dispatch.
pub(crate) fn call_method(
    ev: &mut Evaluator,
    ns: &str,
    method: &str,
    args: CallArgs,
) -> VResult<Value> {
    match ns {
        "json" => json_ns(method, args),
        "yaml" => yaml_ns(method, args),
        "toml" => toml_ns(method, args),
        "csv" => csv_ns(method, args),
        "math" => math_ns(method, args),
        "http" => ev.http_ns(method, args),
        "os" => ev.os_ns(method, args),
        "config" => config_ns(ev, method, args),
        _ => Err(ErrorVal::new(
            "field_missing",
            format!("unknown namespace `{ns}`"),
        )),
    }
}

// --- json / yaml / toml / csv -------------------------------------------------

fn one_str<'a>(args: &'a CallArgs, what: &str) -> VResult<&'a str> {
    match args.pos.first() {
        Some(Value::Str(s)) => Ok(s),
        Some(v) => Err(ErrorVal::arg_error(format!(
            "{what} expects a str, found {}",
            v.type_name()
        ))),
        None => Err(ErrorVal::arg_error(format!(
            "{what} expects a str argument"
        ))),
    }
}

fn json_ns(method: &str, args: CallArgs) -> VResult<Value> {
    match method {
        "parse" => {
            let s = one_str(&args, "json.parse")?;
            let j: serde_json::Value = serde_json::from_str(s)
                .map_err(|e| ErrorVal::arg_error(format!("json.parse: {e}")))?;
            Ok(json_to_value(&j))
        }
        "stringify" => {
            let v = args
                .pos
                .first()
                .ok_or_else(|| ErrorVal::arg_error("json.stringify expects a value"))?;
            let pretty =
                named_bool(&args, "pretty") || matches!(args.pos.get(1), Some(Value::Bool(true)));
            let j = value_to_json(v)?;
            let out = if pretty {
                serde_json::to_string_pretty(&j)
            } else {
                serde_json::to_string(&j)
            }
            .map_err(|e| ErrorVal::new("custom", format!("json.stringify: {e}")))?;
            Ok(Value::Str(out))
        }
        _ => unknown_method("json", method),
    }
}

fn yaml_ns(method: &str, args: CallArgs) -> VResult<Value> {
    match method {
        "parse" => {
            let s = one_str(&args, "yaml.parse")?;
            let j: serde_json::Value = serde_norway::from_str(s)
                .map_err(|e| ErrorVal::arg_error(format!("yaml.parse: {e}")))?;
            Ok(json_to_value(&j))
        }
        "stringify" => {
            let v = args
                .pos
                .first()
                .ok_or_else(|| ErrorVal::arg_error("yaml.stringify expects a value"))?;
            serde_norway::to_string(&value_to_json(v)?)
                .map(Value::Str)
                .map_err(|e| ErrorVal::new("custom", format!("yaml.stringify: {e}")))
        }
        _ => unknown_method("yaml", method),
    }
}

fn toml_ns(method: &str, args: CallArgs) -> VResult<Value> {
    match method {
        "parse" => {
            let s = one_str(&args, "toml.parse")?;
            let j: serde_json::Value =
                toml::from_str(s).map_err(|e| ErrorVal::arg_error(format!("toml.parse: {e}")))?;
            Ok(json_to_value(&j))
        }
        "stringify" => {
            let v = args
                .pos
                .first()
                .ok_or_else(|| ErrorVal::arg_error("toml.stringify expects a value"))?;
            toml::to_string(&value_to_json(v)?)
                .map(Value::Str)
                .map_err(|e| {
                    ErrorVal::new(
                        "arg_error",
                        format!("toml.stringify: {e} (toml needs a record/table at the top level)"),
                    )
                })
        }
        _ => unknown_method("toml", method),
    }
}

fn csv_ns(method: &str, args: CallArgs) -> VResult<Value> {
    match method {
        "parse" => {
            let s = one_str(&args, "csv.parse")?;
            let mut rdr = csv::ReaderBuilder::new()
                .has_headers(true)
                .from_reader(s.as_bytes());
            let headers = rdr
                .headers()
                .map_err(|e| ErrorVal::arg_error(format!("csv.parse: {e}")))?
                .clone();
            let mut rows = Vec::new();
            for rec in rdr.records() {
                let rec = rec.map_err(|e| ErrorVal::arg_error(format!("csv.parse: {e}")))?;
                let mut r = Record::new();
                for (h, field) in headers.iter().zip(rec.iter()) {
                    r.insert(h.to_string(), Value::Str(field.to_string()));
                }
                rows.push(r);
            }
            Ok(Value::Table(rows))
        }
        "stringify" => {
            let v = args
                .pos
                .first()
                .ok_or_else(|| ErrorVal::arg_error("csv.stringify expects a table"))?;
            csv_stringify(v)
        }
        _ => unknown_method("csv", method),
    }
}

fn csv_stringify(v: &Value) -> VResult<Value> {
    let rows: Vec<&Record> = match v {
        Value::Table(rows) => rows.iter().collect(),
        Value::List(xs) => xs
            .iter()
            .map(|x| match x {
                Value::Record(r) => Ok(r),
                other => Err(ErrorVal::type_error(format!(
                    "csv.stringify expects a list of records, found a {}",
                    other.type_name()
                ))),
            })
            .collect::<VResult<Vec<_>>>()?,
        Value::Record(r) => vec![r],
        other => {
            return Err(ErrorVal::type_error(format!(
                "csv.stringify expects a table, found {}",
                other.type_name()
            )));
        }
    };
    let mut wtr = csv::Writer::from_writer(Vec::new());
    if let Some(first) = rows.first() {
        let headers: Vec<&str> = first.keys().map(String::as_str).collect();
        wtr.write_record(&headers)
            .map_err(|e| ErrorVal::new("custom", format!("csv.stringify: {e}")))?;
        for row in &rows {
            let fields: Vec<String> = first
                .keys()
                .map(|k| match row.get(k) {
                    Some(Value::Str(s)) => s.clone(),
                    Some(other) => shoal_value::render::render_inline(other),
                    None => String::new(),
                })
                .collect();
            wtr.write_record(&fields)
                .map_err(|e| ErrorVal::new("custom", format!("csv.stringify: {e}")))?;
        }
    }
    let bytes = wtr
        .into_inner()
        .map_err(|e| ErrorVal::new("custom", format!("csv.stringify: {e}")))?;
    String::from_utf8(bytes)
        .map(Value::Str)
        .map_err(|_| ErrorVal::new("utf8_error", "csv.stringify produced non-UTF-8"))
}

// --- math ---------------------------------------------------------------------

fn math_const(name: &str) -> VResult<Value> {
    let v = match name {
        "pi" => std::f64::consts::PI,
        "e" => std::f64::consts::E,
        "tau" => std::f64::consts::TAU,
        "inf" => f64::INFINITY,
        "nan" => f64::NAN,
        "sqrt2" => std::f64::consts::SQRT_2,
        _ => {
            return Err(ErrorVal::new(
                "field_missing",
                format!("`math.{name}` is not a constant; if it is a function, call it"),
            ));
        }
    };
    Ok(Value::Float(v))
}

fn num(v: &Value) -> VResult<f64> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        other => Err(ErrorVal::type_error(format!(
            "math expects a number, found {}",
            other.type_name()
        ))),
    }
}

fn math_ns(method: &str, args: CallArgs) -> VResult<Value> {
    let a = |n: usize| -> VResult<f64> {
        args.pos
            .get(n)
            .ok_or_else(|| ErrorVal::arg_error(format!("math.{method} expects argument {}", n + 1)))
            .and_then(num)
    };
    let r = match method {
        "sqrt" => a(0)?.sqrt(),
        "cbrt" => a(0)?.cbrt(),
        "sin" => a(0)?.sin(),
        "cos" => a(0)?.cos(),
        "tan" => a(0)?.tan(),
        "asin" => a(0)?.asin(),
        "acos" => a(0)?.acos(),
        "atan" => a(0)?.atan(),
        "atan2" => a(0)?.atan2(a(1)?),
        "ln" => a(0)?.ln(),
        "log10" => a(0)?.log10(),
        "log2" => a(0)?.log2(),
        "log" => a(0)?.log(a(1)?),
        "exp" => a(0)?.exp(),
        "floor" => a(0)?.floor(),
        "ceil" => a(0)?.ceil(),
        "round" => a(0)?.round(),
        "trunc" => a(0)?.trunc(),
        "abs" => a(0)?.abs(),
        "sign" => a(0)?.signum(),
        "pow" => a(0)?.powf(a(1)?),
        "min" => a(0)?.min(a(1)?),
        "max" => a(0)?.max(a(1)?),
        "hypot" => a(0)?.hypot(a(1)?),
        "clamp" => {
            let (x, lo, hi) = (a(0)?, a(1)?, a(2)?);
            if lo > hi {
                return Err(ErrorVal::arg_error("math.clamp: lo must be <= hi"));
            }
            x.clamp(lo, hi)
        }
        _ => return unknown_method("math", method),
    };
    Ok(Value::Float(r))
}

// --- shared helpers -----------------------------------------------------------

fn named_bool(args: &CallArgs, name: &str) -> bool {
    matches!(args.get_named(name), Some(Value::Bool(true)))
}

fn unknown_method(ns: &str, method: &str) -> VResult<Value> {
    Err(ErrorVal::new(
        "field_missing",
        format!("unknown method `{ns}.{method}`"),
    ))
}

// --- http ---------------------------------------------------------------------

/// Body read cap for `http.*` responses (site/content/internals/roadmap-and-priorities.md "size cap"): responses whose
/// body exceeds this are rejected rather than buffered without bound.
const HTTP_BODY_CAP: u64 = 64 * 1024 * 1024;
/// Global per-request timeout (site/content/internals/roadmap-and-priorities.md "timeout").
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the evaluator's capability-safe HTTP transport. Planning authorizes
/// the literal request authority only, so ambient proxy routing and automatic
/// redirects would connect to endpoints absent from the approved plan.
pub(crate) fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .http_status_as_error(false)
        .proxy(None)
        .max_redirects(0)
        .build();
    ureq::Agent::new_with_config(config)
}

impl Evaluator {
    pub(crate) fn http_ns(&mut self, method: &str, args: CallArgs) -> VResult<Value> {
        let has_body = match method {
            "get" | "delete" => false,
            "post" | "put" => true,
            _ => return unknown_method("http", method),
        };
        let url = match args.pos.first() {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Path(p)) => p.to_string_lossy().into_owned(),
            _ => {
                return Err(ErrorVal::arg_error(format!(
                    "http.{method} expects a url string"
                )));
            }
        };
        // Headers: a `headers:` named record, or the last positional record.
        let headers_rec = args
            .get_named("headers")
            .or_else(|| {
                let idx = if has_body { 2 } else { 1 };
                args.pos.get(idx)
            })
            .cloned();
        let body_bytes = if has_body {
            match args.pos.get(1) {
                Some(v) => shoal_value::feed_bytes(v)
                    .map_err(|e| ErrorVal::arg_error(format!("http.{method} body: {e}")))?,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };

        let agent = http_agent();

        let mut resp = match method {
            "get" => with_headers(agent.get(&url), &headers_rec).call(),
            "delete" => with_headers(agent.delete(&url), &headers_rec).call(),
            "post" => with_headers(agent.post(&url), &headers_rec).send(&body_bytes[..]),
            "put" => with_headers(agent.put(&url), &headers_rec).send(&body_bytes[..]),
            _ => unreachable!(),
        }
        .map_err(|e| ErrorVal::new("net_error", format!("http.{method}: {e}")))?;

        let status = resp.status().as_u16() as i64;
        let ok = (200..300).contains(&status);
        let mut headers = Record::new();
        for (k, v) in resp.headers() {
            headers.insert(
                k.as_str().to_string(),
                Value::Str(v.to_str().unwrap_or("").to_string()),
            );
        }
        let body = resp
            .body_mut()
            .with_config()
            .limit(HTTP_BODY_CAP)
            .read_to_string()
            .map_err(|e| ErrorVal::new("net_error", format!("http.{method} body: {e}")))?;
        // `json`: the body parsed as JSON when it is valid JSON, else null. This
        // is the `resp.json` accessor of the typed response (site/content/internals/roadmap-and-priorities.md).
        let json = serde_json::from_str::<serde_json::Value>(body.trim())
            .map(|j| json_to_value(&j))
            .unwrap_or(Value::Null);

        let mut r = Record::new();
        r.insert("status".into(), Value::Int(status));
        r.insert("ok".into(), Value::Bool(ok));
        r.insert("body".into(), Value::Str(body));
        r.insert("json".into(), json);
        r.insert("headers".into(), Value::Record(headers));
        Ok(Value::Record(r))
    }

    // --- os -------------------------------------------------------------------

    pub(crate) fn os_ns(&mut self, method: &str, args: CallArgs) -> VResult<Value> {
        if !args.pos.is_empty() || !args.named.is_empty() {
            // os.* accessors are nullary; a stray arg is almost always a mistake.
            return Err(ErrorVal::arg_error(format!(
                "os.{method} takes no arguments"
            )));
        }
        match method {
            "platform" => Ok(Value::Str(std::env::consts::OS.to_string())),
            "arch" => Ok(Value::Str(std::env::consts::ARCH.to_string())),
            "pid" => Ok(Value::Int(std::process::id() as i64)),
            "hostname" => Ok(Value::Str(os_hostname())),
            "username" => Ok(Value::Str(self.os_username())),
            "cpus" => Ok(Value::Int(
                std::thread::available_parallelism()
                    .map(|n| n.get() as i64)
                    .unwrap_or(1),
            )),
            "uptime" => Ok(os_uptime()),
            "env" => {
                let mut r = Record::new();
                for (k, v) in &self.exec.shell.process_env {
                    if let (Some(k), Some(v)) = (k.to_str(), v.to_str()) {
                        r.insert(k.to_string(), Value::Str(v.to_string()));
                    }
                }
                Ok(Value::Record(r))
            }
            _ => unknown_method("os", method),
        }
    }

    fn os_username(&self) -> String {
        for key in ["USER", "LOGNAME", "USERNAME"] {
            if let Some(v) = self
                .exec
                .shell
                .process_env
                .iter()
                .find(|(k, _)| k == std::ffi::OsStr::new(key))
                .and_then(|(_, v)| v.to_str())
                && !v.is_empty()
            {
                return v.to_string();
            }
        }
        os_username_libc().unwrap_or_else(|| "unknown".into())
    }
}

/// Apply a header record to a request builder (generic over the body typestate).
fn with_headers<T>(
    mut b: ureq::RequestBuilder<T>,
    headers: &Option<Value>,
) -> ureq::RequestBuilder<T> {
    if let Some(Value::Record(r)) = headers {
        for (k, v) in r {
            let val = match v {
                Value::Str(s) => s.clone(),
                Value::Secret(s) => s.value.to_string(),
                other => shoal_value::render::render_inline(other),
            };
            b = b.header(k.as_str(), val.as_str());
        }
    }
    b
}

fn os_hostname() -> String {
    let mut buf = vec![0u8; 256];
    // SAFETY: `gethostname` writes at most `buf.len()` bytes into `buf`.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn os_username_libc() -> Option<String> {
    const FALLBACK_BYTES: usize = 16 * 1024;
    const MAX_BYTES: usize = 1024 * 1024;
    // `getpwuid` uses process-global static storage and is unsafe under the
    // kernel's concurrent evaluator threads. The reentrant API writes both the
    // passwd record and all pointed-to strings into this caller-owned buffer.
    let hint = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut bytes = usize::try_from(hint)
        .unwrap_or(FALLBACK_BYTES)
        .clamp(1024, MAX_BYTES);
    loop {
        let mut buffer = vec![0u8; bytes];
        // SAFETY: an all-zero passwd is a valid output slot; getpwuid_r fills it
        // and points its string fields into `buffer` on success.
        let mut record = unsafe { std::mem::zeroed::<libc::passwd>() };
        let mut result = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &raw mut record,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &raw mut result,
            )
        };
        if rc == libc::ERANGE && bytes < MAX_BYTES {
            bytes = bytes.saturating_mul(2).min(MAX_BYTES);
            continue;
        }
        if rc != 0 || result.is_null() || record.pw_name.is_null() {
            return None;
        }
        let base = buffer.as_ptr() as usize;
        let name = record.pw_name as usize;
        let offset = name.checked_sub(base)?;
        let tail = buffer.get(offset..)?;
        let end = tail.iter().position(|byte| *byte == 0)?;
        if end == 0 {
            return None;
        }
        return Some(String::from_utf8_lossy(&tail[..end]).into_owned());
    }
}

/// System uptime as a `duration`, best-effort. Uses `CLOCK_MONOTONIC` which counts
/// from boot on both Linux and macOS; `null` if the clock read fails.
fn os_uptime() -> Value {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` fills the `timespec` we pass by pointer.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return Value::Null;
    }
    let ns = ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128;
    Value::Duration(ns.clamp(0, i64::MAX as i128) as i64)
}

// --- config -------------------------------------------------------------------

/// The resolved configuration record backing `config.all`/`config.get`
/// (site/content/internals/language-conformance-contract.md). Reads the host-injected config snapshot (`Evaluator::set_config`),
/// NOT a raw `shoal.toml` walked off the filesystem: the host loads and applies
/// its config through `shoal-config` (layering + env overrides + validation),
/// then injects that same resolved value here, so in-language `config` can
/// never disagree with the config the host applied to itself. With no snapshot
/// injected (kernel-less / `-c` / test — the default [`ConfigSnapshot::empty`])
/// this is an empty record, so `config.all` is `{}` and `config.get(key)` is
/// `null` — the same zero-config answer as before, with no filesystem walk.
fn config_record(ev: &Evaluator) -> VResult<Value> {
    Ok(ev.host.config.snapshot().clone())
}

fn config_get(ev: &Evaluator, key: &str) -> VResult<Value> {
    match ev.host.config.snapshot() {
        Value::Record(r) => Ok(r.get(key).cloned().unwrap_or(Value::Null)),
        _ => Ok(Value::Null),
    }
}

fn config_ns(ev: &mut Evaluator, method: &str, args: CallArgs) -> VResult<Value> {
    match method {
        "all" => config_record(ev),
        "get" => {
            let key = one_str(&args, "config.get")?;
            config_get(ev, key)
        }
        _ => unknown_method("config", method),
    }
}

#[cfg(test)]
mod os_boundary_tests {
    use super::os_username_libc;

    #[test]
    fn concurrent_username_lookup_uses_only_caller_owned_storage() {
        let expected = os_username_libc();
        let threads = (0..32)
            .map(|_| {
                std::thread::spawn(|| (0..100).map(|_| os_username_libc()).collect::<Vec<_>>())
            })
            .collect::<Vec<_>>();
        for thread in threads {
            assert!(thread.join().unwrap().iter().all(|name| name == &expected));
        }
    }
}
