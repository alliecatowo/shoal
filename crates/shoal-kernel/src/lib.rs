//! Long-lived Unix-socket host for the shoal evaluator (TDD §10).

mod dispatch;
mod eventbus;
mod handlers_exec;
mod handlers_session;
mod handlers_task;
mod handlers_value;
mod session;
mod wire;

use eventbus::*;
use session::*;
use wire::*;

use serde_json::{Value as Json, json};
use shoal_ast::{Program, Stmt};
use shoal_auth::TokenStore;
use shoal_eval::{Evaluator, Position};
use shoal_journal::{EntryRecord, Journal, JournalQuery};
use shoal_leash::{
    Effect, EnforcementStatus, EnforcementTier, Estimates, Plan, Policy, Reversibility, Verdict,
};
use shoal_proto::*;
use shoal_value::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub struct Kernel {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    next_client: AtomicU64,
    journal: Mutex<Journal>,
    policy: Policy,
    plans: Mutex<HashMap<String, StoredPlan>>,
    tasks: Mutex<HashMap<Ref, Arc<TaskEntry>>>,
    next_task: AtomicU64,
    auth: Option<Mutex<TokenStore>>,
    events: EventBus,
}

/// Wire version of the AST node-kind vocabulary (TDD §7, IO.md §2.5). Bumped
/// from 1 to 2 when `sh_raw` was retired in favor of the general
/// `lang_block` node — a breaking rename to the AST-kind enum.
const AST_VERSION: u32 = 2;

struct TaskEntry {
    task: Ref,
    session: Arc<Session>,
    started_ns: i64,
    inner: Mutex<TaskInner>,
    done: Condvar,
    cancel: shoal_exec::CancelToken,
    cancel_requested: AtomicBool,
}
struct TaskInner {
    state: &'static str,
    finished_ns: Option<i64>,
    result_ref: Option<Ref>,
    error: Option<RpcError>,
}

struct StoredPlan {
    src: String,
    session: String,
    principal: String,
    plan: Plan,
    approved: bool,
}

impl Kernel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            journal: Mutex::new(Journal::in_memory().expect("in-memory journal")),
            policy: permissive_policy(),
            plans: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            next_task: AtomicU64::new(1),
            events: EventBus::default(),
            auth: None,
        })
    }

    pub fn open(state_dir: impl AsRef<Path>) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let state_dir = state_dir.as_ref();
        Ok(Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            journal: Mutex::new(Journal::open(state_dir)?),
            policy: permissive_policy(),
            plans: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            next_task: AtomicU64::new(1),
            events: EventBus::default(),
            auth: Some(Mutex::new(TokenStore::open(state_dir.join("tokens.json"))?)),
        }))
    }

    pub fn open_with_policy(
        state_dir: impl AsRef<Path>,
        policy: Policy,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let state_dir = state_dir.as_ref();
        Ok(Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            journal: Mutex::new(Journal::open(state_dir)?),
            policy,
            plans: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            next_task: AtomicU64::new(1),
            events: EventBus::default(),
            auth: Some(Mutex::new(TokenStore::open(state_dir.join("tokens.json"))?)),
        }))
    }

    pub fn with_policy(policy: Policy) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            journal: Mutex::new(Journal::in_memory().expect("in-memory journal")),
            policy,
            plans: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            next_task: AtomicU64::new(1),
            events: EventBus::default(),
            auth: None,
        })
    }

    pub fn serve(self: Arc<Self>, path: impl AsRef<Path>) -> io::Result<()> {
        self.serve_until(path, Arc::new(AtomicBool::new(false)))
    }

    pub fn serve_until(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(path)?;
        let _socket_guard = BoundSocket(path.to_path_buf());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        while !stop.load(Ordering::SeqCst) {
            let kernel = self.clone();
            match listener.accept() {
                Ok((stream, _)) => {
                    // The listener is non-blocking so the accept loop can poll
                    // `stop`, but that non-blocking flag is inherited by the
                    // accepted stream on some platforms (e.g. macOS) and not
                    // others (e.g. Linux, where accepted sockets are always
                    // blocking regardless of the listener's flag). Explicitly
                    // force the accepted connection back into blocking mode so
                    // per-connection reads in `handle_stream` block as intended
                    // on every platform, instead of racing the client's next
                    // write and getting a transient `WouldBlock` misread as EOF.
                    stream.set_nonblocking(false)?;
                    std::thread::spawn(move || {
                        let _ = kernel.handle_stream(stream);
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(25))
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn handle_stream(self: &Arc<Self>, stream: UnixStream) -> io::Result<()> {
        let client = self.next_client.fetch_add(1, Ordering::Relaxed);
        let mut reader = BufReader::new(stream.try_clone()?);
        let writer: SharedWriter = Arc::new(Mutex::new(stream));
        let mut attached: Option<Attachment> = None;
        let result = (|| -> io::Result<()> {
            while let Some(request) = read_frame(&mut reader)? {
                let id = request.id.clone();
                let response = if request.jsonrpc != JSONRPC {
                    Response::err(id, -32600, "invalid JSON-RPC version", None)
                } else {
                    self.dispatch(request, client, &mut attached, Some(&writer))
                };
                write_frame(&mut *writer.lock().unwrap(), &response)?;
            }
            Ok(())
        })();
        // On disconnect, drop this connection's subscriptions so publish never
        // writes to a dead fd.
        self.events.remove_conn(client);
        result
    }

    fn task(&self, task: &Ref) -> Result<Arc<TaskEntry>, RpcError> {
        self.tasks
            .lock()
            .unwrap()
            .get(task)
            .cloned()
            .ok_or_else(|| RpcError {
                code: -32021,
                message: "unknown task ref".into(),
                data: None,
            })
    }
}

fn task_record(task: &Arc<TaskEntry>) -> TaskRecord {
    let inner = task.inner.lock().unwrap();
    task_record_locked(task, &inner)
}
fn task_record_locked(task: &TaskEntry, inner: &TaskInner) -> TaskRecord {
    TaskRecord {
        task: task.task.clone(),
        session: task.session.id.clone(),
        state: inner.state.into(),
        started_ns: task.started_ns,
        finished_ns: inner.finished_ns,
        result_ref: inner.result_ref.clone(),
        error: inner.error.clone(),
    }
}

struct BoundSocket(std::path::PathBuf);
impl Drop for BoundSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Json) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|e| RpcError {
        code: -32602,
        message: e.to_string(),
        data: None,
    })
}
fn encode<T: serde::Serialize>(value: T) -> Result<Json, RpcError> {
    serde_json::to_value(value).map_err(internal)
}
fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError {
        code: -32603,
        message: error.to_string(),
        data: None,
    }
}
fn not_attached() -> RpcError {
    RpcError {
        code: -32000,
        message: "attach to a session first".into(),
        data: None,
    }
}
fn principal() -> String {
    format!("uid:{}", unsafe { libc_geteuid() })
}
fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}
fn elapsed_ns(start: Instant) -> i64 {
    start.elapsed().as_nanos().min(i64::MAX as u128) as i64
}
fn permissive_policy() -> Policy {
    Policy::permissive(&principal())
}

/// The single-letter wire form of an enforcement tier (TDD §8): A (Landlock),
/// B (namespace fallback), C (Seatbelt), D (advisory). Reported at attach so a
/// client learns the strongest OS backend available on this host.
fn tier_letter(tier: EnforcementTier) -> &'static str {
    match tier {
        EnforcementTier::A => "A",
        EnforcementTier::B => "B",
        EnforcementTier::C => "C",
        EnforcementTier::D => "D",
    }
}

/// Derive a plan's real effects (TDD §8) and give it a source-anchored
/// `plan_ref`. Two distinct programs never collide, even when both derive to
/// the same coarse effect set (e.g. two different `sh { }` blocks, both
/// opaque) — the ref is a blake3 hash over the AST JSON *and* the effects,
/// not effects alone. Falls back to a conservative opaque plan if effect
/// derivation itself errors (arg-shape errors etc.); that must never block
/// real execution, which is the authority on whether the command runs.
fn derive_plan(evaluator: &mut Evaluator, ast: &Program, ast_json: &str) -> Plan {
    let mut plan = evaluator.plan_program(ast).unwrap_or_else(|_| {
        Plan::new(
            vec![Effect::Opaque],
            Reversibility::Unknown,
            Estimates::default(),
        )
    });
    plan.plan_ref = canonical_plan_ref(ast_json, &plan.effects);
    plan
}

fn canonical_plan_ref(ast_json: &str, effects: &[Effect]) -> String {
    let effects_json = serde_json::to_string(effects).unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(ast_json.as_bytes());
    hasher.update(b"\0");
    hasher.update(effects_json.as_bytes());
    format!("plan:{}", &hasher.finalize().to_hex()[..16])
}

/// `position: "value"` (TDD §1.2/§4.5): evaluate the sole top-level command
/// expression without statement-position's raise-on-non-ok, binding `it` to
/// whatever comes back (including a failed outcome). Anything shaped other
/// than a single bare expression statement has no meaningful non-statement
/// reading (`let`/`fn`/`for`/… are already position-agnostic), so it falls
/// back to ordinary statement evaluation.
fn eval_with_position(
    evaluator: &mut Evaluator,
    ast: &Program,
    position: &str,
) -> shoal_value::VResult<Value> {
    if position == "value"
        && let Some((last, init)) = ast.stmts.split_last()
    {
        // Run every statement but the last with ordinary statement semantics,
        // sharing the evaluator's env so bindings carry into the final expr.
        if !init.is_empty() {
            evaluator.eval_program(&Program {
                stmts: init.to_vec(),
            })?;
        }
        // TDD §4.5: the *final* expression is the value; evaluate it in value
        // position so a failed outcome is captured (bound to `it`), not raised.
        if let Stmt::Expr { expr, .. } = last {
            let value = evaluator.eval_expr(expr, Position::Value)?;
            evaluator.it = value.clone();
            return Ok(value);
        }
        // A final `let`/`fn`/`for`/… has no distinct value reading; run it as
        // a statement and return whatever it produces.
        return evaluator.eval_program(&Program {
            stmts: vec![last.clone()],
        });
    }
    evaluator.eval_program(ast)
}
fn verdict_name(v: Verdict) -> &'static str {
    match v {
        Verdict::Allow => "allow",
        Verdict::Deny => "deny",
        Verdict::ApprovalRequired => "approval_required",
    }
}

/// Derive plan reversibility from its concrete effects (AGENT-SURFACE §5,
/// TDD §8): irreversible for opaque work, network effects, or a delete with
/// no journaled inverse; reversible when every effect is reversible/journaled
/// (pure reads/writes, env, session, time). This is computed here rather than
/// trusting the leash's coarser `Reversibility` so the wire answer is derived
/// from the effect set the agent actually sees.
fn reversibility_from_effects(effects: &[Effect]) -> &'static str {
    let irreversible = effects.iter().any(|e| {
        matches!(
            e,
            Effect::Opaque
                | Effect::FsDelete { .. }
                | Effect::NetConnect { .. }
                | Effect::NetListen { .. }
        )
    });
    if irreversible {
        "irreversible"
    } else {
        "reversible"
    }
}

/// The `kind` tag an effect serializes with (`{"kind":"fs.write",…}`), used to
/// scope a `cap.request` grant to a set of effect kinds (AGENT-SURFACE §5).
fn effect_kind(effect: &Effect) -> String {
    serde_json::to_value(effect)
        .ok()
        .and_then(|v| v.get("kind").and_then(Json::as_str).map(String::from))
        .unwrap_or_default()
}

/// Normalize an effect kind so the agent-facing dotted convention (`fs.delete`,
/// per AGENT-SURFACE) matches the snake_case form the effect actually
/// serializes to (`fs_delete`).
fn norm_effect(kind: &str) -> String {
    kind.replace('.', "_")
}

/// The kernel's default elision thresholds, advertised at attach so a client
/// knows the budget before tightening/loosening per call (AGENT-SURFACE §5).
fn elide_defaults_json() -> Json {
    json!({
        "max_bytes": ELIDE_DEFAULT_MAX_BYTES,
        "max_rows": ELIDE_DEFAULT_MAX_ROWS,
        "max_bytes_raw": ELIDE_DEFAULT_MAX_BYTES_RAW,
        "max_items": ELIDE_DEFAULT_MAX_ITEMS,
        "hard_cap": ELIDE_HARD_CAP,
    })
}

/// The `session.transcript` event payload for a new `out[n]` (AGENT-SURFACE
/// §4): `{n, ref, summary:{type, ok?, cmd?, n?}}` — shape only, never payload.
fn transcript_event(value_ref: &Ref, value: &Value) -> Json {
    let n: i64 = value_ref
        .0
        .split_once(':')
        .and_then(|(_, id)| id.parse().ok())
        .unwrap_or(0);
    let mut summary = serde_json::Map::new();
    summary.insert("type".into(), json!({"$":"str","v": value.type_name()}));
    match value {
        Value::Outcome(o) => {
            summary.insert("ok".into(), json!({"$":"bool","v": o.ok}));
            summary.insert("cmd".into(), json!({"$":"str","v": o.cmd}));
        }
        Value::Table(rows) => {
            summary.insert("n".into(), json!({"$":"int","v": rows.len()}));
        }
        Value::List(items) => {
            summary.insert("n".into(), json!({"$":"int","v": items.len()}));
        }
        _ => {}
    }
    json!({
        "$": "record",
        "v": {
            "n": {"$":"int","v": n},
            "ref": {"$":"str","v": value_ref.0},
            "summary": {"$":"record","v": summary},
        }
    })
}

/// Completion candidates at a cursor byte offset (the kernel `complete`
/// method). Keywords/builtins plus any `let`/`var`/`fn`/`alias` names declared
/// before the cursor, filtered by the partial word under the cursor.
fn complete_at(src: &str, cursor: usize) -> Vec<String> {
    const WORDS: &[&str] = &[
        "let", "var", "fn", "alias", "use", "export", "return", "break", "continue", "if", "else",
        "match", "for", "in", "while", "try", "catch", "true", "false", "null", "spawn", "with",
        "sh", "ls", "cd", "pwd", "cp", "mv", "rm", "mkdir", "cat", "echo", "run", "parallel",
        "pick", "interact", "explain",
    ];
    let before = &src[..cursor];
    // The partial identifier immediately left of the cursor.
    let start = before
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let partial = &before[start..];
    let mut names: Vec<String> = WORDS.iter().map(|s| s.to_string()).collect();
    // Declarations already in scope (`let x`, `fn y`, …).
    let toks: Vec<&str> = before
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|s| !s.is_empty())
        .collect();
    for pair in toks.windows(2) {
        if matches!(pair[0], "let" | "var" | "fn" | "alias") {
            names.push(pair[1].to_string());
        }
    }
    names.retain(|n| n.starts_with(partial));
    names.sort();
    names.dedup();
    names
}
unsafe fn libc_geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(
        writer: &mut UnixStream,
        reader: &mut BufReader<UnixStream>,
        id: i64,
        method: &str,
        params: Json,
    ) -> Response {
        write_frame(
            writer,
            &Request {
                jsonrpc: JSONRPC.into(),
                id: id.into(),
                method: method.into(),
                params,
            },
        )
        .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(reader, &mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    #[test]
    fn unix_stream_session_roundtrip() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        assert!(
            call(
                &mut client,
                &mut reader,
                1,
                "session.attach",
                json!({"client":{"kind":"test","tty":false}})
            )
            .error
            .is_none()
        );
        assert!(
            call(&mut client, &mut reader, 2, "parse", json!({"src":"1 + 2"}))
                .error
                .is_none()
        );
        let exec = call(&mut client, &mut reader, 3, "exec", json!({"src":"1 + 2"}));
        let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        let get = call(
            &mut client,
            &mut reader,
            4,
            "value.get",
            json!({"ref":value_ref,"path":null,"slice":null}),
        );
        assert_eq!(get.result.unwrap()["value"]["v"], 3);
        assert!(
            call(&mut client, &mut reader, 5, "task.list", json!({}))
                .error
                .is_none()
        );
        let journal = call(
            &mut client,
            &mut reader,
            6,
            "journal.query",
            json!({"limit":10}),
        );
        let entries = journal.result.unwrap();
        assert_eq!(entries[0]["src"], "1 + 2");
        assert_eq!(entries[0]["ok"], true);
        assert_eq!(
            entries[0]["opaque"], false,
            "pure arithmetic must not be journaled opaque:true"
        );
        assert!(
            entries[0]["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["kind"] == "value"
                    && o["len"].as_i64().unwrap() > 0
                    && o["hash"].as_str().unwrap().len() == 64)
        );
        let value_hash = entries[0]["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["kind"] == "value")
            .unwrap()["hash"]
            .as_str()
            .unwrap();
        let blob = kernel
            .journal
            .lock()
            .unwrap()
            .read_blob(value_hash)
            .unwrap()
            .unwrap();
        assert!(String::from_utf8(blob).unwrap().contains("\"v\":3"));
        // Slice applies to tables (list<record> semantically) — it used to
        // silently no-op and return the whole table — and slicing an
        // unordered/scalar value is an explicit error, not a silent identity.
        let texec = call(
            &mut client,
            &mut reader,
            40,
            "exec",
            json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
        );
        let table_ref = texec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        let sliced = call(
            &mut client,
            &mut reader,
            41,
            "value.get",
            json!({"ref":table_ref,"slice":[1,3]}),
        );
        let sliced = sliced.result.unwrap()["value"].clone();
        assert_eq!(sliced["$"], "table", "csv.parse yields a table: {sliced}");
        assert_eq!(sliced["n"], 2, "table slice should keep rows 1..3");
        let bad = call(
            &mut client,
            &mut reader,
            42,
            "value.get",
            json!({"ref":value_ref,"slice":[0,1]}),
        );
        assert_eq!(
            bad.error.expect("slicing an int must error").code,
            -32005,
            "slice on a scalar must be an explicit error"
        );
        // `[a..b]` path ranges (AGENT-SURFACE §1) — used to be "bad index".
        let ranged = call(
            &mut client,
            &mut reader,
            43,
            "value.get",
            json!({"ref":table_ref,"path":"rows[0..2]"}),
        );
        let ranged = ranged.result.unwrap()["value"].clone();
        assert_eq!(ranged["$"], "list", "rows[0..2]: {ranged}");
        assert_eq!(ranged["v"].as_array().unwrap().len(), 2);
        // `format=render` returns the human string; `format=raw` on a non-str
        // value is an explicit error.
        let rendered = call(
            &mut client,
            &mut reader,
            44,
            "value.get",
            json!({"ref":table_ref,"format":"render"}),
        );
        let rendered = rendered.result.unwrap();
        assert!(
            rendered["render"].as_str().unwrap().contains('1'),
            "render output: {rendered}"
        );
        let raw_bad = call(
            &mut client,
            &mut reader,
            45,
            "value.get",
            json!({"ref":value_ref,"format":"raw"}),
        );
        assert_eq!(raw_bad.error.expect("raw on int must error").code, -32005);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn leash_plan_approval_and_denial_flow() {
        for (opaque, expected, approvable) in
            [("ask", "approval_required", true), ("deny", "deny", false)]
        {
            let policy = Policy::from_toml(&format!(
                "[principal.\"{}\"]\nopaque='{opaque}'\nauto_apply='never'\n",
                principal()
            ))
            .unwrap();
            let kernel = Kernel::with_policy(policy);
            let (mut client, server) = UnixStream::pair().unwrap();
            let mut reader = BufReader::new(client.try_clone().unwrap());
            let server_kernel = kernel.clone();
            let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
            call(
                &mut client,
                &mut reader,
                1,
                "session.attach",
                json!({"client":{"kind":"agent","tty":false}}),
            );
            let planned = call(
                &mut client,
                &mut reader,
                2,
                "exec",
                json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
            );
            let result = planned.result.unwrap();
            assert_eq!(result["verdict"], expected);
            assert_eq!(result["effects"], json!([{"kind":"opaque"}]));
            let plan_ref = result["plan_ref"].as_str().unwrap();
            assert!(
                call(
                    &mut client,
                    &mut reader,
                    3,
                    "plan.apply",
                    json!({"plan_ref":plan_ref})
                )
                .error
                .is_some()
            );
            let grant = call(
                &mut client,
                &mut reader,
                4,
                "cap.request",
                json!({"plan_ref":plan_ref,"effects":[]}),
            );
            if approvable {
                assert!(grant.error.is_none());
                let applied = call(
                    &mut client,
                    &mut reader,
                    5,
                    "plan.apply",
                    json!({"plan_ref":plan_ref}),
                );
                let value = applied.result.unwrap()["value"].clone();
                assert_eq!(value["$"], "outcome");
                assert_eq!(value["ok"], true);
            } else {
                assert!(grant.error.is_some());
            }
            drop(client);
            drop(reader);
            thread.join().unwrap();
        }
    }

    /// Regression: `mode:"approved"` used to skip the leash verdict for ANY
    /// caller — the magic string alone bypassed policy. It is `plan.apply`'s
    /// re-entry and must name a stored plan that is approved for this
    /// session/principal with the same source; anything else is rejected
    /// even though a plain `run` of the same source would only be
    /// approval_required.
    #[test]
    fn approved_mode_is_not_a_caller_assertable_bypass() {
        let policy = Policy::from_toml(&format!(
            "[principal.\"{}\"]\nopaque='ask'\nauto_apply='never'\n",
            principal()
        ))
        .unwrap();
        let kernel = Kernel::with_policy(policy);
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        // Baseline: the policy gates a plain run of this source.
        let run = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo hi }","mode":"run","position":"stmt"}),
        );
        assert_eq!(run.error.expect("run must be gated").code, -32011);
        // The bypass: bare `mode:"approved"` (no plan_ref) must be rejected…
        let bare = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"sh { echo hi }","mode":"approved","position":"stmt"}),
        );
        assert_eq!(bare.error.expect("bare approved must fail").code, -32010);
        // …as must a plan_ref that was never approved…
        let planned = call(
            &mut client,
            &mut reader,
            4,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
        );
        let plan_ref = planned.result.unwrap()["plan_ref"]
            .as_str()
            .unwrap()
            .to_owned();
        let unapproved = call(
            &mut client,
            &mut reader,
            5,
            "exec",
            json!({"src":"sh { echo hi }","mode":"approved","position":"stmt","plan_ref":plan_ref}),
        );
        assert_eq!(
            unapproved
                .error
                .expect("unapproved plan_ref must fail")
                .code,
            -32010
        );
        // …and an approved plan_ref may not smuggle DIFFERENT source.
        call(
            &mut client,
            &mut reader,
            6,
            "cap.request",
            json!({"plan_ref":plan_ref,"effects":[]}),
        );
        let smuggled = call(
            &mut client,
            &mut reader,
            7,
            "exec",
            json!({"src":"sh { rm -rf / }","mode":"approved","position":"stmt","plan_ref":plan_ref}),
        );
        assert_eq!(
            smuggled.error.expect("source smuggling must fail").code,
            -32010
        );
        // The sanctioned path still works: same source, approved plan.
        let sanctioned = call(
            &mut client,
            &mut reader,
            8,
            "exec",
            json!({"src":"sh { echo hi }","mode":"approved","position":"stmt","plan_ref":plan_ref}),
        );
        assert!(
            sanctioned.error.is_none(),
            "sanctioned approved exec failed: {:?}",
            sanctioned.error
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// Regression for the plan_ref collision (Plan identity used to hash only
    /// effects/reversibility/estimates, so any two opaque `sh { }` plans
    /// collided and `apply` silently ran whichever plan was last inserted).
    #[test]
    fn plan_refs_are_unique_per_source_and_apply_targets_the_right_one() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let plan_a = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo FIRST }","mode":"plan"}),
        )
        .result
        .unwrap();
        let plan_b = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"sh { echo SECOND }","mode":"plan"}),
        )
        .result
        .unwrap();
        let ref_a = plan_a["plan_ref"].as_str().unwrap().to_owned();
        let ref_b = plan_b["plan_ref"].as_str().unwrap().to_owned();
        assert_ne!(ref_a, ref_b, "distinct sources must not share a plan_ref");
        // Both plans are opaque (`sh { }`), so both need cap.request before
        // apply under the default permissive-but-opaque='allow' policy —
        // plan mode always requires explicit approval regardless of opaque
        // mode; grant both, then apply A and confirm it — not B — ran.
        call(
            &mut client,
            &mut reader,
            4,
            "cap.request",
            json!({"plan_ref":ref_a}),
        );
        call(
            &mut client,
            &mut reader,
            5,
            "cap.request",
            json!({"plan_ref":ref_b}),
        );
        let applied = call(
            &mut client,
            &mut reader,
            6,
            "plan.apply",
            json!({"plan_ref":ref_a}),
        );
        let out = applied.result.unwrap()["value"]["out"].clone();
        assert_eq!(out["$"], "str");
        assert_eq!(out["v"], "FIRST");
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn real_effects_not_opaque_for_pure_builtins() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        for src in ["1 + 2", "ls"] {
            let planned = call(
                &mut client,
                &mut reader,
                2,
                "exec",
                json!({"src":src,"mode":"plan"}),
            )
            .result
            .unwrap();
            assert_ne!(
                planned["effects"],
                json!([{"kind":"opaque"}]),
                "`{src}` must derive real effects, not the opaque fallback"
            );
            let exec = call(&mut client, &mut reader, 3, "exec", json!({"src":src}));
            let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
            let journal = call(
                &mut client,
                &mut reader,
                4,
                "journal.query",
                json!({"limit":1}),
            )
            .result
            .unwrap();
            assert_eq!(journal[0]["src"], src);
            assert_eq!(
                journal[0]["opaque"], false,
                "`{src}` must not be journaled opaque:true"
            );
            let _ = value_ref;
        }
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn value_get_path_traversal() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo hello world }"}),
        );
        let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        let out = call(
            &mut client,
            &mut reader,
            3,
            "value.get",
            json!({"ref":value_ref,"path":"out"}),
        );
        assert_eq!(
            out.result.unwrap()["value"],
            json!({"$":"str","v":"hello world"})
        );
        let ok = call(
            &mut client,
            &mut reader,
            4,
            "value.get",
            json!({"ref":value_ref,"path":"ok"}),
        );
        assert_eq!(ok.result.unwrap()["value"], json!({"$":"bool","v":true}));
        let bad = call(
            &mut client,
            &mut reader,
            5,
            "value.get",
            json!({"ref":value_ref,"path":"nope"}),
        );
        assert_eq!(bad.error.unwrap().code, -32005);

        let ls_exec = call(&mut client, &mut reader, 6, "exec", json!({"src":"ls"}));
        let ls_ref = ls_exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        let rows0 = call(
            &mut client,
            &mut reader,
            7,
            "value.get",
            json!({"ref":ls_ref,"path":"rows[0].name"}),
        );
        assert!(rows0.error.is_none(), "{:?}", rows0.error);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn exec_position_stmt_raises_value_does_not() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let stmt = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { exit 7 }","position":"stmt"}),
        );
        assert_eq!(stmt.error.unwrap().code, -32002);
        let value = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"sh { exit 7 }","position":"value"}),
        );
        let result = value.result.unwrap();
        assert_eq!(result["value"]["$"], "outcome");
        assert_eq!(result["value"]["ok"], false);
        assert_eq!(result["value"]["status"], 7);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn async_tasks_survive_disconnect_and_cancel() {
        let kernel = Kernel::new();
        let (mut first, server) = UnixStream::pair().unwrap();
        let mut first_reader = BufReader::new(first.try_clone().unwrap());
        let k = kernel.clone();
        let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
        call(
            &mut first,
            &mut first_reader,
            1,
            "session.attach",
            json!({"session":"tasks","client":{"kind":"test","tty":false}}),
        );
        let started = call(
            &mut first,
            &mut first_reader,
            2,
            "exec",
            json!({"src":"sh { sleep 0.2 }","async":true}),
        );
        let survived: Ref =
            serde_json::from_value(started.result.unwrap()["task"].clone()).unwrap();
        drop(first);
        drop(first_reader);
        thread.join().unwrap();

        let (mut second, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(second.try_clone().unwrap());
        let k = kernel.clone();
        let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
        call(
            &mut second,
            &mut reader,
            3,
            "session.attach",
            json!({"session":"tasks","client":{"kind":"test","tty":false}}),
        );
        let awaited = call(
            &mut second,
            &mut reader,
            4,
            "task.await",
            json!({"task":survived}),
        );
        let awaited_value = awaited.result.unwrap();
        assert_eq!(awaited_value["state"], "completed", "{awaited_value}");
        let long = call(
            &mut second,
            &mut reader,
            5,
            "exec",
            json!({"src":"sh { sleep 30 }","async":true}),
        );
        let task: Ref = serde_json::from_value(long.result.unwrap()["task"].clone()).unwrap();
        let listed = call(&mut second, &mut reader, 6, "task.list", json!({}));
        assert!(listed.result.unwrap().as_array().unwrap().len() >= 2);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            call(
                &mut second,
                &mut reader,
                7,
                "task.cancel",
                json!({"task":task})
            )
            .error
            .is_none()
        );
        let before = Instant::now();
        let cancelled = call(
            &mut second,
            &mut reader,
            8,
            "task.await",
            json!({"task":task}),
        );
        assert!(before.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(cancelled.result.unwrap()["state"], "cancelled");
        drop(second);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn bearer_attach_uses_token_principal_and_rejects_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let mut tokens = TokenStore::open(dir.path().join("tokens.json")).unwrap();
        let (secret, _) = tokens
            .create(
                "agent:codex".into(),
                "readonly".into(),
                vec!["fs.read".into()],
                None,
            )
            .unwrap();
        drop(tokens);
        let kernel = Kernel::open(dir.path()).unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let k = kernel.clone();
        let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
        let attached = call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"token":secret,"client":{"kind":"agent","tty":false}}),
        );
        assert_eq!(attached.result.unwrap()["principal"], "agent:codex");
        let denied = call(
            &mut client,
            &mut reader,
            2,
            "session.attach",
            json!({"token":"not-a-token","client":{"kind":"agent","tty":false}}),
        );
        assert_eq!(denied.error.unwrap().code, -32030);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // The elision rule (AGENT-SURFACE §3).
    // -----------------------------------------------------------------------

    /// A >100-row table (real `ls` over a directory with 150 files, not a
    /// synthetic stand-in) must come back elided: shape + schema + a 5-row
    /// preview, never the 150-row payload. Then drill into a single row by
    /// field-path and confirm that small result is NOT elided.
    #[test]
    fn big_table_exec_elides_then_drills_by_path() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..150 {
            std::fs::write(dir.path().join(format!("f{i:04}.txt")), b"x").unwrap();
        }
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src": format!("ls {}", dir.path().display())}),
        );
        let result = exec.result.expect("ls must succeed");
        let value_ref = result["ref"].as_str().unwrap().to_owned();
        // `ls` is a command: its wire shape is `outcome` with a structured
        // `.out`. Elision unwraps to `.out` for the decision (mirroring
        // render_block's outcome-unification) — the 150-row *table* elides,
        // the outer outcome envelope (status/ok/cmd/…) still travels.
        let value = &result["value"];
        assert_eq!(value["$"], "outcome");
        let out = &value["out"];
        assert_eq!(out["$"], "ref", "a 150-row table must elide, got {out}");
        assert_eq!(out["of"], "table");
        assert_eq!(out["n"], 150);
        assert_eq!(
            out["cols"]["name"], "path",
            "shape (schema) travels even when the payload does not"
        );
        assert_eq!(out["preview"]["$"], "table");
        assert_eq!(
            out["preview"]["n"], 5,
            "preview is a small head, not the full 150 rows"
        );
        assert!(out["render_head"].as_str().unwrap().contains("name"));
        let wire_len = serde_json::to_string(value).unwrap().len();
        assert!(
            wire_len < 4 * 1024,
            "the elided form itself must stay tiny, was {wire_len} bytes"
        );

        // Drill in: value.get with a field-path returns one small row —
        // NOT elided, because it never hits any threshold.
        let get = call(
            &mut client,
            &mut reader,
            3,
            "value.get",
            json!({"ref": value_ref, "path": "out[3]"}),
        );
        let drilled = get.result.unwrap()["value"].clone();
        assert_ne!(
            drilled["$"], "ref",
            "a single drilled row must not be elided: {drilled}"
        );
        assert_eq!(drilled["$"], "record");
        assert!(
            drilled["v"]["name"].is_object(),
            "drilled row keeps its fields: {drilled}"
        );

        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn small_value_is_not_elided() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"[1,2,3]"}),
        );
        let value = exec.result.unwrap()["value"].clone();
        assert_eq!(
            value["$"], "list",
            "a 3-item list is nowhere near any threshold"
        );
        assert_eq!(
            value["v"],
            json!([{"$":"int","v":1},{"$":"int","v":2},{"$":"int","v":3}])
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// A caller may loosen the byte budget, but never past the 64 KiB hard
    /// cap — a misbehaving agent cannot flood its own context by asking
    /// nicely.
    #[test]
    fn elision_hard_cap_cannot_be_disabled() {
        let huge = Value::Str("x".repeat(100_000));
        let loosened = ElideSpec {
            max_bytes: Some(5_000_000),
            max_rows: None,
            max_items: None,
        };
        let budget = ElideBudget::from_spec(Some(&loosened));
        assert_eq!(
            budget.max_bytes, ELIDE_HARD_CAP,
            "a requested budget above the hard cap must clamp down to it"
        );
        match elide_wire_value(&huge, "shoal://out/1", &budget) {
            WireValue::Ref { of, n, .. } => {
                assert_eq!(of, "str");
                assert_eq!(n, 100_000);
            }
            other => panic!(
                "a 100 KB string must still elide despite a 5 MB requested budget, got {other:?}"
            ),
        }
    }

    /// The flip side: loosening below the hard cap is honored, so a caller
    /// that wants a bit more headroom than the 8 KiB default legitimately
    /// gets it.
    #[test]
    fn elision_budget_can_be_loosened_up_to_the_hard_cap() {
        let modest = Value::Str("y".repeat(20_000)); // > 8 KiB default, < 64 KiB cap
        let loosened = ElideSpec {
            max_bytes: Some(5_000_000),
            max_rows: None,
            max_items: None,
        };
        let budget = ElideBudget::from_spec(Some(&loosened));
        match elide_wire_value(&modest, "shoal://out/1", &budget) {
            WireValue::Str { .. } => {}
            other => panic!("a 20 KiB string fits under a loosened 64 KiB cap, got {other:?}"),
        }
    }

    #[test]
    fn value_get_elide_param_tightens_default_row_threshold() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        );
        // 10 items: under every default threshold, so a plain exec would not elide.
        let exec = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"[0,1,2,3,4,5,6,7,8,9]"}),
        );
        assert_ne!(exec.result.as_ref().unwrap()["value"]["$"], "ref");
        let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        // A caller may tighten the budget per call — max_items:5 must elide
        // this same 10-item list on a follow-up `value.get`.
        let get = call(
            &mut client,
            &mut reader,
            3,
            "value.get",
            json!({"ref": value_ref, "path": null, "slice": null, "elide": {"max_items": 5}}),
        );
        let value = get.result.unwrap()["value"].clone();
        assert_eq!(
            value["$"], "ref",
            "a tightened per-call budget must elide: {value}"
        );
        assert_eq!(value["n"], 10);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// Read one already-written frame off the socket (no request sent) — for
    /// asserting on pushed `event` notifications interleaved with responses.
    fn recv_line(reader: &mut BufReader<UnixStream>) -> Json {
        let mut line = String::new();
        std::io::BufRead::read_line(reader, &mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn attach(client: &mut UnixStream, reader: &mut BufReader<UnixStream>) -> Response {
        call(
            client,
            reader,
            1,
            "session.attach",
            json!({"client":{"kind":"agent","tty":false}}),
        )
    }

    fn spawn(
        kernel: &Arc<Kernel>,
    ) -> (
        UnixStream,
        BufReader<UnixStream>,
        std::thread::JoinHandle<()>,
    ) {
        let (client, server) = UnixStream::pair().unwrap();
        let reader = BufReader::new(client.try_clone().unwrap());
        let k = kernel.clone();
        let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
        (client, reader, thread)
    }

    // -----------------------------------------------------------------------
    // Events — channels, cursors, push (AGENT-SURFACE §4/§6).
    // -----------------------------------------------------------------------

    #[test]
    fn events_publish_read_roundtrips_on_a_user_channel() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        // Only user.* channels are client-writable.
        let denied = call(
            &mut client,
            &mut reader,
            2,
            "events.publish",
            json!({"channel":"session.transcript","payload":{"$":"int","v":1}}),
        );
        assert_eq!(denied.error.unwrap().code, -32602);
        // Publish two values, then read them back with monotonic per-channel seq.
        for (i, v) in ["go", "stop"].iter().enumerate() {
            let published = call(
                &mut client,
                &mut reader,
                3 + i as i64,
                "events.publish",
                json!({"channel":"user.deploy","payload":{"$":"str","v":v}}),
            );
            assert_eq!(published.result.unwrap()["seq"], i as i64);
        }
        let read = call(
            &mut client,
            &mut reader,
            9,
            "events.read",
            json!({"channel":"user.deploy"}),
        );
        let events = read.result.unwrap()["events"].clone();
        let events = events.as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["payload"], json!({"$":"str","v":"go"}));
        assert_eq!(events[1]["seq"], 1);
        // Cursor read: since=0 returns only events after seq 0.
        let tail = call(
            &mut client,
            &mut reader,
            10,
            "events.read",
            json!({"channel":"user.deploy","since":0}),
        );
        let tail = tail.result.unwrap()["events"].clone();
        assert_eq!(tail.as_array().unwrap().len(), 1);
        assert_eq!(tail[0]["payload"], json!({"$":"str","v":"stop"}));
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn subscribe_pushes_session_transcript_event_before_the_exec_response() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        call(
            &mut client,
            &mut reader,
            2,
            "events.subscribe",
            json!({"channel":"session.transcript"}),
        );
        // The pushed event is written to the socket during exec dispatch, so it
        // arrives before the exec response frame.
        write_frame(
            &mut client,
            &Request {
                jsonrpc: JSONRPC.into(),
                id: 3.into(),
                method: "exec".into(),
                params: json!({"src":"1 + 2"}),
            },
        )
        .unwrap();
        let note = recv_line(&mut reader);
        assert_eq!(note["method"], "event", "expected a pushed event: {note}");
        assert_eq!(note["params"]["channel"], "session.transcript");
        assert_eq!(note["params"]["payload"]["v"]["ref"]["v"], "out:1");
        let resp = recv_line(&mut reader);
        assert_eq!(resp["id"], 3);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn attach_advertises_channels_elide_defaults_and_enforcement() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        let r = attach(&mut client, &mut reader).result.unwrap();
        assert_eq!(r["caps_enforced"], false);
        assert_eq!(r["elide_defaults"]["max_rows"], 100);
        assert_eq!(r["elide_defaults"]["hard_cap"], 64 * 1024);
        assert!(
            r["channels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "session.transcript")
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn attach_reports_the_honest_detected_tier() {
        // TDD §8 tier honesty: the tier at attach is the strongest OS backend
        // this host actually has (detected), NOT a hardcoded "D". Under the
        // default-permissive human policy nothing is confined, so `enforced`
        // stays false even where a backend exists.
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        let r = attach(&mut client, &mut reader).result.unwrap();
        let expected = tier_letter(EnforcementStatus::detect().available_tier);
        assert_eq!(r["caps"]["tier"], expected);
        assert_eq!(r["caps_enforced"], false);
        assert_eq!(r["caps"]["enforced"], false);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn attach_enforces_only_for_a_scoped_principal_with_a_real_backend() {
        // A genuinely-scoped principal reports `enforced: true` — but only when
        // a real OS backend (Landlock/Seatbelt) exists; on a host without one
        // the answer honestly degrades to false rather than claiming a wall
        // that isn't there.
        let who = principal();
        let policy = Policy::from_toml(&format!(
            "[principal.\"{who}\"]\nopaque='allow'\nauto_apply='in-grant'\n\n\
             [principal.\"{who}\".fs]\nread=[\"/usr/**\"]\n"
        ))
        .unwrap();
        let kernel = Kernel::with_policy(policy);
        let (mut client, mut reader, thread) = spawn(&kernel);
        let r = attach(&mut client, &mut reader).result.unwrap();
        let status = EnforcementStatus::detect();
        let backend_present = matches!(
            status.available_tier,
            EnforcementTier::A | EnforcementTier::C
        );
        assert_eq!(r["caps_enforced"], backend_present);
        assert_eq!(r["caps"]["enforced"], backend_present);
        assert_eq!(r["caps"]["tier"], tier_letter(status.available_tier));
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // Plan reversibility (AGENT-SURFACE §5) — derived, not hardcoded.
    // -----------------------------------------------------------------------

    #[test]
    fn plan_reversibility_is_derived_from_effects() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let del = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"rm doomed.txt","mode":"plan"}),
        )
        .result
        .unwrap();
        assert_eq!(
            del["reversibility"], "irreversible",
            "a delete has no journaled inverse: {del}"
        );
        let pure = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"1 + 2","mode":"plan"}),
        )
        .result
        .unwrap();
        assert_eq!(pure["reversibility"], "reversible");
        let opaque = call(
            &mut client,
            &mut reader,
            4,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan"}),
        )
        .result
        .unwrap();
        assert_eq!(opaque["reversibility"], "irreversible");
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // Value-position multi-statement + error-still-yields-a-ref (§0, TDD §4.5).
    // -----------------------------------------------------------------------

    #[test]
    fn value_position_captures_final_expr_of_multi_statement_src() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        // Two statements; the final bare command must be *captured* (ok:false),
        // not raised — the previous single-statement-only special case raised.
        let r = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"let a = 1\nsh { exit 4 }","position":"value"}),
        );
        let value = r
            .result
            .expect("value position must not raise")
            .get("value")
            .cloned()
            .unwrap();
        assert_eq!(value["$"], "outcome");
        assert_eq!(value["ok"], false);
        assert_eq!(value["status"], 4);
        // And a binding from the first statement is visible to the last.
        let r2 = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"let x = 10\nx + 5","position":"value"}),
        );
        assert_eq!(r2.result.unwrap()["value"], json!({"$":"int","v":15}));
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn raised_error_still_yields_an_inspectable_transcript_ref() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        // A genuine raise (statement position, failed command).
        let raised = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { exit 5 }","position":"stmt"}),
        );
        let err = raised.error.expect("must raise");
        assert_eq!(err.code, -32002);
        let data = err.data.unwrap();
        let value_ref = data["ref"]
            .as_str()
            .expect("error carries a transcript ref");
        // The agent can shoal_get that ref and read the structured error.
        let got = call(
            &mut client,
            &mut reader,
            3,
            "value.get",
            json!({"ref": value_ref}),
        );
        assert_eq!(got.result.unwrap()["value"]["$"], "error");
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // cap.request effect scoping (§5), complete/explain (§5).
    // -----------------------------------------------------------------------

    #[test]
    fn cap_request_scopes_the_grant_to_requested_effects() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let plan = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan"}),
        )
        .result
        .unwrap();
        let plan_ref = plan["plan_ref"].as_str().unwrap().to_owned();
        // Scoped to fs.write only — the plan's opaque effect isn't covered, so
        // the grant stays pending (never silently widens).
        let scoped = call(
            &mut client,
            &mut reader,
            3,
            "cap.request",
            json!({"plan_ref": plan_ref, "effects":["fs.write"]}),
        )
        .result
        .unwrap();
        assert_eq!(scoped["grant"], "approval_pending", "{scoped}");
        // Scoped to the actual effect kind — now it grants.
        let ok = call(
            &mut client,
            &mut reader,
            4,
            "cap.request",
            json!({"plan_ref": plan_ref, "effects":["opaque"]}),
        )
        .result
        .unwrap();
        assert_eq!(ok["grant"], "approved");
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn complete_and_explain_methods() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let c = call(
            &mut client,
            &mut reader,
            2,
            "complete",
            json!({"src":"le","cursor":2}),
        )
        .result
        .unwrap();
        let candidates = c["candidates"].as_array().unwrap();
        assert!(candidates.iter().any(|v| v == "let"));
        assert!(
            candidates
                .iter()
                .all(|v| v.as_str().unwrap().starts_with("le")),
            "candidates must be filtered by the partial word"
        );
        let ex = call(
            &mut client,
            &mut reader,
            3,
            "explain",
            json!({"src":"rm gone.txt"}),
        )
        .result
        .unwrap();
        assert_eq!(ex["reversibility"], "irreversible");
        assert!(ex["ast"].is_object() || ex["ast"].is_array() || ex["ast"]["stmts"].is_array());
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn journal_until_and_effects_filters() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        call(&mut client, &mut reader, 2, "exec", json!({"src":"1 + 2"}));
        call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"rm gone.txt","position":"value"}),
        );
        // effects filter: only entries whose effect set mentions fs.delete.
        let deletes = call(
            &mut client,
            &mut reader,
            4,
            "journal.query",
            json!({"effects":["fs.delete"],"limit":50}),
        )
        .result
        .unwrap();
        let deletes = deletes.as_array().unwrap();
        assert!(!deletes.is_empty());
        assert!(
            deletes
                .iter()
                .all(|e| e["src"].as_str().unwrap().starts_with("rm")),
            "effects filter must keep only fs.delete entries: {deletes:?}"
        );
        // until in the far past matches nothing.
        let none = call(
            &mut client,
            &mut reader,
            5,
            "journal.query",
            json!({"until": 1, "limit":50}),
        )
        .result
        .unwrap();
        assert_eq!(none.as_array().unwrap().len(), 0);
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn background_exec_returns_task_and_events_channel() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let bg = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { sleep 0.05 }","background":true}),
        )
        .result
        .unwrap();
        assert!(
            bg["task"].is_string(),
            "background exec returns a task ref: {bg}"
        );
        assert_eq!(
            bg["events"],
            format!("task.{}", bg["task"].as_str().unwrap())
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    /// docs/ROADMAP.md R3: `task.resume` exists alongside `task.suspend`,
    /// wired the same honest way — never a silent no-op, always a clear
    /// error until a task's process handle is actually reachable here.
    #[test]
    fn task_resume_wire_method_is_honest_and_symmetric_with_suspend() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        let bg = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { sleep 0.05 }","background":true}),
        )
        .result
        .unwrap();
        let task = bg["task"].clone();

        let resume = call(
            &mut client,
            &mut reader,
            3,
            "task.resume",
            json!({"task": task}),
        );
        let error = resume.error.expect("task.resume is not yet implemented");
        assert_eq!(error.code, -32020);
        assert!(
            error.message.contains("resume"),
            "message: {}",
            error.message
        );

        // Same shape as `task.suspend` for the same task.
        let suspend = call(
            &mut client,
            &mut reader,
            4,
            "task.suspend",
            json!({"task": task}),
        );
        assert_eq!(suspend.error.unwrap().code, -32020);

        // An unknown task ref is rejected before the honest-stub error, for
        // both methods.
        let unknown = json!({"task": "task:999999"});
        assert_eq!(
            call(&mut client, &mut reader, 5, "task.resume", unknown.clone())
                .error
                .unwrap()
                .code,
            -32021
        );
        assert_eq!(
            call(&mut client, &mut reader, 6, "task.suspend", unknown)
                .error
                .unwrap()
                .code,
            -32021
        );

        call(
            &mut client,
            &mut reader,
            7,
            "task.cancel",
            json!({"task": task}),
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    #[test]
    fn timeout_converts_a_slow_run_to_a_task() {
        let kernel = Kernel::new();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
        // A 30s command with a 50ms budget must come back as a task ref, never
        // block the caller's context.
        let r = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { sleep 30 }","timeout_ms":50}),
        )
        .result
        .unwrap();
        assert!(r["task"].is_string(), "a timed-out run yields a task: {r}");
        assert_eq!(r["timed_out"], true);
        // Cancel the still-running task so its `sleep 30` child doesn't linger
        // holding the test's output pipe open.
        call(
            &mut client,
            &mut reader,
            10,
            "task.cancel",
            json!({"task": r["task"]}),
        );
        // A fast command under budget returns inline.
        let fast = call(
            &mut client,
            &mut reader,
            3,
            "exec",
            json!({"src":"1 + 2","timeout_ms":5000}),
        )
        .result
        .unwrap();
        assert_eq!(fast["value"], json!({"$":"int","v":3}));
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }
}
