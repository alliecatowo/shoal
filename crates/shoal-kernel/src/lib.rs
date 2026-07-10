//! Long-lived Unix-socket host for the shoal evaluator (TDD §10).

use serde_json::{Value as Json, json};
use shoal_ast::{Program, Stmt};
use shoal_auth::TokenStore;
use shoal_eval::{Evaluator, Position};
use shoal_journal::{EntryRecord, Journal, JournalQuery};
use shoal_leash::{Effect, Estimates, Plan, Policy, Reversibility, Verdict};
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

/// A per-connection socket writer shared between the request/response path and
/// any subscription push threads. Whole frames are serialized then written
/// under this lock so a pushed `event` notification never interleaves with a
/// response on the same fd.
type SharedWriter = Arc<Mutex<UnixStream>>;

/// Kernel-native pub/sub (AGENT-SURFACE §4/§6). One ring buffer per channel;
/// `seq` is monotonic per channel. Subscribers get `event` notifications
/// pushed on their own connection.
#[derive(Default)]
struct EventBus {
    channels: Mutex<HashMap<String, ChannelBuf>>,
    subs: Mutex<Vec<Subscriber>>,
}

/// Ring-buffered event log for one channel.
#[derive(Default)]
struct ChannelBuf {
    next_seq: u64,
    ring: VecDeque<Event>,
}

struct Subscriber {
    conn: u64,
    channel: String,
    writer: SharedWriter,
}

/// Ring depth per channel (AGENT-SURFACE §4 requires ≥1024).
const EVENT_RING_CAP: usize = 1024;

/// The static channels a session may always subscribe to (AGENT-SURFACE §4).
/// `task.{id}` and `user.{name}` are dynamic and not listed here.
const STATIC_CHANNELS: &[&str] = &[
    "session.transcript",
    "journal",
    "approval",
    "render",
    "reef",
];

impl EventBus {
    /// Append `payload` to `channel`'s ring and push it to every live
    /// subscriber of that channel. Returns the assigned event.
    fn publish(&self, channel: &str, payload: Json) -> Event {
        let event = {
            let mut channels = self.channels.lock().unwrap();
            let buf = channels.entry(channel.to_string()).or_default();
            let seq = buf.next_seq;
            buf.next_seq += 1;
            let event = Event {
                channel: channel.to_string(),
                seq,
                ts: now_ns(),
                payload,
            };
            buf.ring.push_back(event.clone());
            while buf.ring.len() > EVENT_RING_CAP {
                buf.ring.pop_front();
            }
            event
        };
        // Push to subscribers. A dead connection (write error) is dropped from
        // the subscriber list — the accept loop also cleans up on disconnect.
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| {
            if s.channel != channel {
                return true;
            }
            let note = json!({
                "jsonrpc": JSONRPC,
                "method": "event",
                "params": &event,
            });
            let mut w = s.writer.lock().unwrap();
            write_json_notification(&mut w, &note).is_ok()
        });
        event
    }

    /// Buffered tail of `channel` from `since` (exclusive), capped at `limit`.
    fn read(&self, channel: &str, since: Option<u64>, limit: Option<usize>) -> Vec<Event> {
        let channels = self.channels.lock().unwrap();
        let Some(buf) = channels.get(channel) else {
            return Vec::new();
        };
        let mut out: Vec<Event> = buf
            .ring
            .iter()
            .filter(|e| since.is_none_or(|s| e.seq > s))
            .cloned()
            .collect();
        if let Some(limit) = limit
            && out.len() > limit
        {
            out = out.split_off(out.len() - limit);
        }
        out
    }

    /// Register `writer` as a subscriber to `channel`. Any already-buffered
    /// events after `since` are pushed immediately (replay, then live).
    fn subscribe(&self, conn: u64, channel: &str, since: Option<u64>, writer: &SharedWriter) {
        {
            let mut subs = self.subs.lock().unwrap();
            if !subs.iter().any(|s| s.conn == conn && s.channel == channel) {
                subs.push(Subscriber {
                    conn,
                    channel: channel.to_string(),
                    writer: writer.clone(),
                });
            }
        }
        for event in self.read(channel, since, None) {
            let note = json!({"jsonrpc": JSONRPC, "method": "event", "params": &event});
            let mut w = writer.lock().unwrap();
            let _ = write_json_notification(&mut w, &note);
        }
    }

    fn unsubscribe(&self, conn: u64, channel: &str) {
        self.subs
            .lock()
            .unwrap()
            .retain(|s| !(s.conn == conn && s.channel == channel));
    }

    fn remove_conn(&self, conn: u64) {
        self.subs.lock().unwrap().retain(|s| s.conn != conn);
    }
}

fn write_json_notification(writer: &mut UnixStream, value: &Json) -> io::Result<()> {
    let mut buf = serde_json::to_vec(value).map_err(io::Error::other)?;
    buf.push(b'\n');
    use std::io::Write as _;
    writer.write_all(&buf)?;
    writer.flush()
}
#[derive(Clone)]
struct Attachment {
    session: Arc<Session>,
    principal: String,
}
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

struct Session {
    id: String,
    evaluator: Mutex<Evaluator>,
    transcript: Mutex<HashMap<Ref, Value>>,
    client_it: Mutex<HashMap<u64, Ref>>,
    next_value: AtomicU64,
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

    fn dispatch(
        self: &Arc<Self>,
        request: Request,
        client: u64,
        attached: &mut Option<Attachment>,
        conn: Option<&SharedWriter>,
    ) -> Response {
        let id = request.id;
        let result: Result<Json, RpcError> = (|| match request.method.as_str() {
            "session.attach" => {
                let params: AttachParams = decode(request.params)?;
                let (who, token_caps, profile) = if let Some(token) = params.token {
                    let auth = self.auth.as_ref().ok_or_else(|| RpcError {
                        code: -32030,
                        message: "bearer tokens unavailable in ephemeral kernel".into(),
                        data: None,
                    })?;
                    let meta = auth
                        .lock()
                        .unwrap()
                        .validate(&token)
                        .ok_or_else(|| RpcError {
                            code: -32030,
                            message: "invalid, expired, or revoked bearer token".into(),
                            data: None,
                        })?;
                    (meta.principal, meta.caps, meta.profile)
                } else {
                    (principal(), vec![], "local-human".into())
                };
                let name = params.session.unwrap_or_else(|| "default".into());
                let session = self.session(&name).map_err(internal)?;
                let cwd = session
                    .evaluator
                    .lock()
                    .unwrap()
                    .cwd()
                    .as_os_str()
                    .to_owned();
                *attached = Some(Attachment {
                    session,
                    principal: who.clone(),
                });
                encode(AttachResult {
                    session: name,
                    principal: who.clone(),
                    caps: json!({"enforced":false,"tier":"D","policy_principal":who,"profile":profile,"token_caps":token_caps,"opaque":verdict_name(self.policy.evaluate_effect(&who, &Effect::Opaque))}),
                    cwd: WirePath::encode(&cwd),
                    env_hash: "local".into(),
                    ast_version: 1,
                    // TDD §8 tier honesty: the built-in policy is tier D, so
                    // the wall is advisory, not enforced. Say so at attach.
                    caps_enforced: false,
                    elide_defaults: elide_defaults_json(),
                    channels: STATIC_CHANNELS.iter().map(|s| s.to_string()).collect(),
                })
            }
            "parse" => {
                let params: ParseParams = decode(request.params)?;
                let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
                    code: -32001,
                    message: e.msg,
                    data: Some(json!({"span":e.span,"hint":e.hint})),
                })?;
                encode(json!({"ast_version":1,"ast":ast}))
            }
            "exec" => {
                let attachment = attached.as_ref().ok_or_else(not_attached)?;
                let session = &attachment.session;
                let actor = attachment.principal.clone();
                let params: ExecParams = decode(request.params)?;
                // AGENT-SURFACE §5: `background:true`, or a synchronous run that
                // exceeds `timeout_ms`, becomes a task ref + events channel —
                // never a blocked context. A bare timeout runs the work on a
                // task and waits up to the deadline for a fast inline answer.
                if params.asynchronous || params.timeout_ms.is_some() {
                    let elide_spec = params.elide;
                    let wait = params.timeout_ms.map(std::time::Duration::from_millis);
                    let is_background = params.asynchronous;
                    let cancel = {
                        let mut evaluator = session.evaluator.lock().unwrap();
                        evaluator.reset_cancel();
                        evaluator.cancellation_token()
                    };
                    let task_ref = Ref::new("task", self.next_task.fetch_add(1, Ordering::Relaxed));
                    let task = Arc::new(TaskEntry {
                        task: task_ref.clone(),
                        session: session.clone(),
                        started_ns: now_ns(),
                        inner: Mutex::new(TaskInner {
                            state: "running",
                            finished_ns: None,
                            result_ref: None,
                            error: None,
                        }),
                        done: Condvar::new(),
                        cancel,
                        cancel_requested: AtomicBool::new(false),
                    });
                    self.tasks
                        .lock()
                        .unwrap()
                        .insert(task_ref.clone(), task.clone());
                    let waiter = task.clone();
                    let kernel = self.clone();
                    let mut task_attached = Some(attachment.clone());
                    let task_channel = format!("task.{}", task_ref.0);
                    kernel
                        .events
                        .publish(&task_channel, json!({"$":"str","v":"started"}));
                    std::thread::spawn(move || {
                        let response = kernel.dispatch(
                            Request {
                                jsonrpc: JSONRPC.into(),
                                id: Json::Null,
                                method: "exec".into(),
                                params: serde_json::to_value(ExecParams {
                                    asynchronous: false,
                                    timeout_ms: None,
                                    ..params
                                })
                                .unwrap(),
                            },
                            client,
                            &mut task_attached,
                            None,
                        );
                        let exit_payload;
                        {
                            let mut inner = task.inner.lock().unwrap();
                            inner.finished_ns = Some(now_ns());
                            if let Some(error) = response.error {
                                inner.state = if task.cancel_requested.load(Ordering::SeqCst) {
                                    "cancelled"
                                } else {
                                    "failed"
                                };
                                inner.error = Some(error);
                            } else {
                                inner.state = "completed";
                                inner.result_ref = response
                                    .result
                                    .as_ref()
                                    .and_then(|r| r.get("ref"))
                                    .and_then(Json::as_str)
                                    .map(|s| Ref(s.into()));
                            }
                            exit_payload = json!({
                                "$": "record",
                                "v": {
                                    "state": {"$":"str","v": inner.state},
                                    "ref": inner.result_ref.as_ref()
                                        .map(|r| json!({"$":"str","v": r.0}))
                                        .unwrap_or(Json::Null),
                                }
                            });
                            task.done.notify_all();
                        }
                        kernel.events.publish(&task_channel, exit_payload);
                    });
                    let events_channel = format!("task.{}", task_ref.0);
                    if is_background {
                        return encode(json!({"task":task_ref,"events":events_channel}));
                    }
                    // Synchronous timeout: wait up to the deadline for the task
                    // to finish; return an inline result if it beats the clock,
                    // otherwise hand back the still-running task ref.
                    let deadline = wait.map(|d| Instant::now() + d);
                    let mut inner = waiter.inner.lock().unwrap();
                    while matches!(inner.state, "running" | "cancelling") {
                        let Some(deadline) = deadline else { break };
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let (guard, timed) =
                            waiter.done.wait_timeout(inner, deadline - now).unwrap();
                        inner = guard;
                        if timed.timed_out() {
                            break;
                        }
                    }
                    if matches!(inner.state, "running" | "cancelling") {
                        drop(inner);
                        return encode(
                            json!({"task":task_ref,"events":events_channel,"timed_out":true}),
                        );
                    }
                    let result_ref = inner.result_ref.clone();
                    let task_error = inner.error.clone();
                    drop(inner);
                    if let Some(error) = task_error {
                        return Err(error);
                    }
                    if let Some(result_ref) = result_ref {
                        let values = session.transcript.lock().unwrap();
                        if let Some(value) = values.get(&result_ref) {
                            let budget = ElideBudget::from_spec(elide_spec.as_ref());
                            let uri = short_ref_to_uri(&result_ref, None);
                            let wire = elide_wire_value(value, &uri, &budget);
                            let render = shoal_value::render::render_block(value, 80);
                            return encode(ExecResult {
                                r#ref: result_ref,
                                value: Some(wire),
                                render: Some(render),
                            });
                        }
                    }
                    return encode(json!({"task":task_ref,"events":events_channel}));
                }
                if params.mode == "plan" {
                    let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
                        code: -32001,
                        message: e.msg,
                        data: Some(json!({"span":e.span,"hint":e.hint})),
                    })?;
                    let ast_json = serde_json::to_string(&ast).map_err(internal)?;
                    let plan = {
                        let mut evaluator = session.evaluator.lock().unwrap();
                        derive_plan(&mut evaluator, &ast, &ast_json)
                    };
                    let verdict = self.policy.evaluate_plan(&actor, &plan);
                    let result = PlanResult {
                        plan_ref: plan.plan_ref.clone(),
                        effects: plan
                            .effects
                            .iter()
                            .map(|e| serde_json::to_value(e).unwrap())
                            .collect(),
                        reversibility: reversibility_from_effects(&plan.effects).into(),
                        verdict: verdict_name(verdict).into(),
                        approval_pending: verdict == Verdict::ApprovalRequired,
                    };
                    self.plans.lock().unwrap().insert(
                        plan.plan_ref.clone(),
                        StoredPlan {
                            src: params.src,
                            session: session.id.clone(),
                            principal: actor.clone(),
                            plan,
                            approved: verdict == Verdict::Allow,
                        },
                    );
                    return encode(result);
                } else if params.mode != "run" && params.mode != "approved" {
                    return Err(RpcError {
                        code: -32602,
                        message: "mode must be run or plan".into(),
                        data: None,
                    });
                }
                let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
                    code: -32001,
                    message: e.msg,
                    data: Some(json!({"span":e.span,"hint":e.hint})),
                })?;
                let ast_json = serde_json::to_string(&ast).map_err(internal)?;
                let mut evaluator = session.evaluator.lock().unwrap();
                let run_plan = derive_plan(&mut evaluator, &ast, &ast_json);
                if params.mode == "run" {
                    match self.policy.evaluate_plan(&actor, &run_plan) {
                        Verdict::Deny => {
                            return Err(RpcError {
                                code: -32010,
                                message: "leash denied execution".into(),
                                data: Some(json!({"effects":run_plan.effects})),
                            });
                        }
                        Verdict::ApprovalRequired => {
                            return Err(RpcError {
                                code: -32011,
                                message: "approval required; plan first".into(),
                                data: Some(json!({"effects":run_plan.effects})),
                            });
                        }
                        Verdict::Allow => {}
                    }
                }
                evaluator.interactive = false;
                let started = Instant::now();
                let opaque = run_plan.effects.iter().any(|e| matches!(e, Effect::Opaque));
                let effects_json = serde_json::to_string(&run_plan.effects).map_err(internal)?;
                let entry_id = self
                    .journal
                    .lock()
                    .unwrap()
                    .append(&EntryRecord {
                        session: session.id.clone(),
                        principal: actor,
                        ts_ns: now_ns(),
                        cwd: evaluator.cwd().as_os_str().as_bytes().to_vec(),
                        src: params.src.clone(),
                        ast_json: ast_json.clone(),
                        effects_json,
                        opaque,
                    })
                    .map_err(internal)?;
                let value = match eval_with_position(&mut evaluator, &ast, &params.position) {
                    Ok(value) => value,
                    Err(e) => {
                        {
                            let journal = self.journal.lock().unwrap();
                            let _ = journal.finish(entry_id, e.status, false, elapsed_ns(started));
                            if let Some(stderr) = &e.stderr {
                                let _ =
                                    journal.record_output(entry_id, "stderr", stderr.as_bytes());
                            }
                        }
                        // AGENT-SURFACE §0/§5: even a raised error is
                        // addressable — store it as an out[n] transcript value
                        // so the agent can `shoal_get` the structured error
                        // (code/msg/span/hint) instead of parsing message text.
                        let value_ref =
                            Ref::new("out", session.next_value.fetch_add(1, Ordering::Relaxed));
                        session.transcript.lock().unwrap().insert(
                            value_ref.clone(),
                            Value::Error(std::sync::Arc::new(e.clone())),
                        );
                        session
                            .client_it
                            .lock()
                            .unwrap()
                            .insert(client, value_ref.clone());
                        let uri = short_ref_to_uri(&value_ref, None);
                        return Err(RpcError {
                            code: -32002,
                            message: e.msg,
                            data: Some(json!({
                                "code": e.code, "span": e.span, "hint": e.hint,
                                "status": e.status, "stderr": e.stderr,
                                "ref": value_ref, "uri": uri
                            })),
                        });
                    }
                };
                let value_ref = Ref::new("out", session.next_value.fetch_add(1, Ordering::Relaxed));
                session
                    .transcript
                    .lock()
                    .unwrap()
                    .insert(value_ref.clone(), value.clone());
                session
                    .client_it
                    .lock()
                    .unwrap()
                    .insert(client, value_ref.clone());
                let render = shoal_value::render::render_block(&value, 80);
                {
                    let journal = self.journal.lock().unwrap();
                    journal
                        .finish(entry_id, Some(0), true, elapsed_ns(started))
                        .map_err(internal)?;
                    journal
                        .record_output(
                            entry_id,
                            "value",
                            &serde_json::to_vec(&wire_value(&value)).map_err(internal)?,
                        )
                        .map_err(internal)?;
                    if !render.is_empty() {
                        journal
                            .record_output(entry_id, "render", render.as_bytes())
                            .map_err(internal)?;
                    }
                    if let Value::Outcome(out) = &value {
                        journal
                            .record_output(entry_id, "stdout", &out.stdout)
                            .map_err(internal)?;
                        if !out.stderr.is_empty() {
                            journal
                                .record_output(entry_id, "stderr", &out.stderr)
                                .map_err(internal)?;
                        }
                    }
                }
                // AGENT-SURFACE §4: announce the new transcript value on the
                // `session.transcript` channel — subscribers learn a new
                // out[n] exists (with its shape summary) without polling.
                self.events
                    .publish("session.transcript", transcript_event(&value_ref, &value));
                let exec_budget = ElideBudget::from_spec(params.elide.as_ref());
                let exec_uri = short_ref_to_uri(&value_ref, None);
                encode(ExecResult {
                    r#ref: value_ref,
                    value: Some(elide_wire_value(&value, &exec_uri, &exec_budget)),
                    render: Some(render),
                })
            }
            "value.get" => {
                let attachment = attached.as_ref().ok_or_else(not_attached)?;
                let session = &attachment.session;
                let params: ValueGetParams = decode(request.params)?;
                let values = session.transcript.lock().unwrap();
                let value = values.get(&params.r#ref).ok_or_else(|| RpcError {
                    code: -32004,
                    message: "unknown value ref".into(),
                    data: None,
                })?;
                let resolved = match params.path.as_deref() {
                    Some(path) if !path.is_empty() => {
                        resolve_value_path(value, path).map_err(|message| RpcError {
                            code: -32005,
                            message,
                            data: Some(json!({"ref":params.r#ref,"path":path})),
                        })?
                    }
                    _ => value.clone(),
                };
                // Slicing is an explicit, targeted ask: apply it at the value
                // level *before* the elision check, so a small slice of a
                // huge list is never spuriously elided (and a slice that is
                // itself still huge still is).
                let sliced = match (params.slice, resolved) {
                    (Some([start, end]), Value::List(items)) => {
                        let start = start.min(items.len());
                        let end = end.max(start).min(items.len());
                        Value::List(items[start..end].to_vec())
                    }
                    (_, other) => other,
                };
                let budget = ElideBudget::from_spec(params.elide.as_ref());
                let uri = short_ref_to_uri(&params.r#ref, params.path.as_deref());
                let wire = elide_wire_value(&sliced, &uri, &budget);
                encode(json!({"ref":params.r#ref,"value":wire}))
            }
            "task.list" => {
                let attachment = attached.as_ref().ok_or_else(not_attached)?;
                let session = &attachment.session;
                let records: Vec<_> = self
                    .tasks
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|task| task.session.id == session.id)
                    .map(task_record)
                    .collect();
                encode(records)
            }
            "task.get" => {
                let attachment = attached.as_ref().ok_or_else(not_attached)?;
                let session = &attachment.session;
                let p: TaskParams = decode(request.params)?;
                let task = self.task(&p.task)?;
                if task.session.id != session.id {
                    return Err(RpcError {
                        code: -32021,
                        message: "unknown task ref".into(),
                        data: None,
                    });
                }
                // Non-blocking snapshot (unlike task.await): the current record.
                encode(task_record(&task))
            }
            "task.await" => {
                let attachment = attached.as_ref().ok_or_else(not_attached)?;
                let session = &attachment.session;
                let p: TaskParams = decode(request.params)?;
                let task = self.task(&p.task)?;
                if task.session.id != session.id {
                    return Err(RpcError {
                        code: -32021,
                        message: "unknown task ref".into(),
                        data: None,
                    });
                }
                let mut inner = task.inner.lock().unwrap();
                while matches!(inner.state, "running" | "cancelling") {
                    inner = task.done.wait(inner).unwrap();
                }
                encode(task_record_locked(&task, &inner))
            }
            "task.cancel" => {
                let attachment = attached.as_ref().ok_or_else(not_attached)?;
                let session = &attachment.session;
                let p: TaskParams = decode(request.params)?;
                let task = self.task(&p.task)?;
                if task.session.id != session.id {
                    return Err(RpcError {
                        code: -32021,
                        message: "unknown task ref".into(),
                        data: None,
                    });
                }
                task.cancel_requested.store(true, Ordering::SeqCst);
                {
                    let mut inner = task.inner.lock().unwrap();
                    if inner.state == "running" {
                        inner.state = "cancelling";
                    }
                }
                task.cancel.cancel();
                encode(json!({"task":p.task,"cancel_requested":true}))
            }
            "task.suspend" => {
                let attachment = attached.as_ref().ok_or_else(not_attached)?;
                let session = &attachment.session;
                let p: TaskParams = decode(request.params)?;
                let task = self.task(&p.task)?;
                if task.session.id != session.id {
                    return Err(RpcError {
                        code: -32021,
                        message: "unknown task ref".into(),
                        data: None,
                    });
                }
                Err(RpcError {
                    code: -32020,
                    message: "task suspension is unavailable for evaluator-owned processes".into(),
                    data: Some(json!({"task":p.task})),
                })
            }
            "plan.apply" => {
                let attachment = attached.as_ref().ok_or_else(not_attached)?;
                let session = &attachment.session;
                let p: PlanApplyParams = decode(request.params)?;
                let plans = self.plans.lock().unwrap();
                let stored = plans.get(&p.plan_ref).ok_or_else(|| RpcError {
                    code: -32012,
                    message: "unknown plan_ref".into(),
                    data: None,
                })?;
                if stored.session != session.id || stored.principal != attachment.principal {
                    return Err(RpcError {
                        code: -32010,
                        message: "plan belongs to another principal/session".into(),
                        data: None,
                    });
                }
                if !stored.approved
                    && self
                        .policy
                        .evaluate_plan(&attachment.principal, &stored.plan)
                        != Verdict::Allow
                {
                    return Err(RpcError {
                        code: -32011,
                        message: "plan approval pending".into(),
                        data: None,
                    });
                }
                let src = stored.src.clone();
                drop(plans);
                let response = self.dispatch(
                    Request {
                        jsonrpc: JSONRPC.into(),
                        id: Json::Null,
                        method: "exec".into(),
                        params: serde_json::to_value(ExecParams {
                            src,
                            mode: "approved".into(),
                            position: "stmt".into(),
                            asynchronous: false,
                            timeout_ms: None,
                            elide: None,
                        })
                        .unwrap(),
                    },
                    client,
                    attached,
                    conn,
                );
                response.result.ok_or_else(|| {
                    response
                        .error
                        .unwrap_or_else(|| internal("plan apply failed"))
                })
            }
            "cap.request" => {
                let p: CapRequestParams = decode(request.params)?;
                let Some(plan_ref) = p.plan_ref else {
                    return Err(RpcError {
                        code: -32602,
                        message: "plan_ref is required".into(),
                        data: None,
                    });
                };
                let mut plans = self.plans.lock().unwrap();
                let stored = plans.get_mut(&plan_ref).ok_or_else(|| RpcError {
                    code: -32012,
                    message: "unknown plan_ref".into(),
                    data: None,
                })?;
                if self.policy.evaluate_plan(&stored.principal, &stored.plan) == Verdict::Deny {
                    return Err(RpcError {
                        code: -32010,
                        message: "policy denies requested effects".into(),
                        data: None,
                    });
                }
                // AGENT-SURFACE §5: if the caller scoped the request to a set
                // of effect kinds, the grant only covers those — a plan that
                // needs an effect the caller did not name stays pending, so an
                // approval can never silently widen past what was asked for.
                let requested: Vec<String> = p
                    .effects
                    .iter()
                    .filter_map(|e| match e {
                        Json::String(s) => Some(s.clone()),
                        other => other.get("kind").and_then(Json::as_str).map(String::from),
                    })
                    .collect();
                if !requested.is_empty() {
                    let requested: Vec<String> = requested.iter().map(|e| norm_effect(e)).collect();
                    let missing: Vec<String> = stored
                        .plan
                        .effects
                        .iter()
                        .map(effect_kind)
                        .filter(|k| !requested.contains(&norm_effect(k)))
                        .collect();
                    if !missing.is_empty() {
                        return encode(json!({
                            "grant": "approval_pending",
                            "plan_ref": plan_ref,
                            "why": "requested effect scope does not cover the plan",
                            "uncovered_effects": missing,
                        }));
                    }
                }
                stored.approved = true;
                encode(
                    json!({"grant":"approved","plan_ref":plan_ref,"enforced":false,"granted_effects":requested}),
                )
            }
            "journal.query" => {
                let p: JournalQueryParams = decode(request.params)?;
                let rows = self
                    .journal
                    .lock()
                    .unwrap()
                    .query(&JournalQuery {
                        since_ts_ns: p.since,
                        principal: p.principal,
                        head: p.head,
                        ok: p.ok,
                        limit: p.limit,
                    })
                    .map_err(internal)?;
                // The journal store filters since/principal/head/ok/limit; the
                // wire also promises `until` (upper time bound) and `effects`
                // (effect-kind subset) — kernel-side post-filters over the
                // returned rows (AGENT-SURFACE §5 / TDD §7).
                // Effect kinds are stored snake_case (`fs_delete`); agents use
                // the dotted convention (`fs.delete`). Normalize so either
                // form matches.
                let want_effects: Vec<String> = p
                    .effects
                    .unwrap_or_default()
                    .iter()
                    .map(|e| norm_effect(e))
                    .collect();
                let entries: Vec<JournalEntry> = rows
                    .into_iter()
                    .filter(|r| p.until.is_none_or(|until| r.ts_ns <= until))
                    .filter(|r| {
                        want_effects.is_empty()
                            || want_effects
                                .iter()
                                .all(|want| r.effects_json.contains(want))
                    })
                    .map(|r| JournalEntry {
                        id: r.id,
                        session: r.session,
                        principal: r.principal,
                        ts: r.ts_ns,
                        dur_ns: r.dur_ns,
                        cwd: WirePath::encode(&std::ffi::OsString::from_vec(r.cwd)),
                        src: r.src,
                        ast: serde_json::from_str(&r.ast_json).unwrap_or(Json::Null),
                        effects: serde_json::from_str(&r.effects_json).unwrap_or(Json::Null),
                        status: r.status,
                        ok: r.ok,
                        opaque: r.opaque,
                        outputs: r
                            .outputs
                            .into_iter()
                            .map(|o| JournalOutput {
                                kind: o.kind,
                                hash: o.hash,
                                len: o.len,
                            })
                            .collect(),
                    })
                    .collect();
                encode(entries)
            }
            "events.read" => {
                attached.as_ref().ok_or_else(not_attached)?;
                let p: EventsReadParams = decode(request.params)?;
                let events = self.events.read(&p.channel, p.since, p.limit);
                encode(json!({"channel": p.channel, "events": events}))
            }
            "events.publish" => {
                attached.as_ref().ok_or_else(not_attached)?;
                let p: EventsPublishParams = decode(request.params)?;
                // AGENT-SURFACE §4: only `user.*` channels are client-writable;
                // the kernel owns the semantic channels.
                if !p.channel.starts_with("user.") {
                    return Err(RpcError {
                        code: -32602,
                        message: "only user.* channels may be published to".into(),
                        data: Some(json!({"channel": p.channel})),
                    });
                }
                let event = self.events.publish(&p.channel, p.payload);
                encode(json!({"channel": event.channel, "seq": event.seq, "ts": event.ts}))
            }
            "events.subscribe" => {
                attached.as_ref().ok_or_else(not_attached)?;
                let p: EventsSubParams = decode(request.params)?;
                let Some(writer) = conn else {
                    return Err(RpcError {
                        code: -32603,
                        message: "subscription requires a live connection".into(),
                        data: None,
                    });
                };
                self.events.subscribe(client, &p.channel, p.since, writer);
                encode(json!({"channel": p.channel, "subscribed": true}))
            }
            "events.unsubscribe" => {
                attached.as_ref().ok_or_else(not_attached)?;
                let p: EventsSubParams = decode(request.params)?;
                self.events.unsubscribe(client, &p.channel);
                encode(json!({"channel": p.channel, "subscribed": false}))
            }
            "blob.get" => {
                attached.as_ref().ok_or_else(not_attached)?;
                let hash = request
                    .params
                    .get("hash")
                    .and_then(Json::as_str)
                    .ok_or_else(|| RpcError {
                        code: -32602,
                        message: "blob.get requires a hash".into(),
                        data: None,
                    })?
                    .to_string();
                let blob = self
                    .journal
                    .lock()
                    .unwrap()
                    .read_blob(&hash)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError {
                        code: -32004,
                        message: "unknown value hash".into(),
                        data: None,
                    })?;
                // Content-addressed value blobs are stored as their `$`-tagged
                // JSON encoding; hand it back structurally. A non-JSON blob
                // (stdout/stderr) comes back as tagged bytes.
                let value = serde_json::from_slice::<Json>(&blob).unwrap_or_else(|_| {
                    json!({
                        "$": "bytes",
                        "len": blob.len(),
                        "v": base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD, &blob),
                    })
                });
                encode(json!({"hash": hash, "value": value}))
            }
            "complete" => {
                let p: CompleteParams = decode(request.params)?;
                let cursor = p.cursor.unwrap_or(p.src.len()).min(p.src.len());
                encode(json!({"candidates": complete_at(&p.src, cursor)}))
            }
            "explain" => {
                let attachment = attached.as_ref().ok_or_else(not_attached)?;
                let session = &attachment.session;
                let p: ExplainParams = decode(request.params)?;
                let ast = if let Some(src) = &p.src {
                    shoal_syntax::parse(src).map_err(|e| RpcError {
                        code: -32001,
                        message: e.msg,
                        data: Some(json!({"span": e.span, "hint": e.hint})),
                    })?
                } else if let Some(ast_json) = p.ast {
                    serde_json::from_value(ast_json).map_err(|e| RpcError {
                        code: -32602,
                        message: format!("invalid ast: {e}"),
                        data: None,
                    })?
                } else {
                    return Err(RpcError {
                        code: -32602,
                        message: "explain requires src or ast".into(),
                        data: None,
                    });
                };
                let ast_json = serde_json::to_string(&ast).map_err(internal)?;
                let plan = {
                    let mut evaluator = session.evaluator.lock().unwrap();
                    derive_plan(&mut evaluator, &ast, &ast_json)
                };
                encode(json!({
                    "ast_version": 1,
                    "ast": ast,
                    "effects": plan.effects,
                    "reversibility": reversibility_from_effects(&plan.effects),
                    "plan_ref": plan.plan_ref,
                }))
            }
            _ => Err(RpcError {
                code: -32601,
                message: "method not found".into(),
                data: None,
            }),
        })();
        match result {
            Ok(value) => Response::ok(id, value),
            Err(error) => Response {
                jsonrpc: JSONRPC.into(),
                id,
                result: None,
                error: Some(error),
            },
        }
    }

    fn session(&self, name: &str) -> io::Result<Arc<Session>> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(name) {
            return Ok(session.clone());
        }
        let cwd = std::env::current_dir()?;
        let session = Arc::new(Session {
            id: name.into(),
            evaluator: Mutex::new(Evaluator::new(cwd)),
            transcript: Mutex::new(HashMap::new()),
            client_it: Mutex::new(HashMap::new()),
            next_value: AtomicU64::new(1),
        });
        sessions.insert(name.into(), session.clone());
        Ok(session)
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
    let who = principal();
    Policy::from_toml(&format!(
        "[principal.\"{who}\"]\nopaque='allow'\nauto_apply='in-grant'\njournal_read=true\n\
         env_read=[\"*\"]\nenv_write=[\"*\"]\nsession_write=true\ntime=true\n\n\
         [principal.\"{who}\".fs]\nread=[\"/**\"]\nwrite=[\"/**\"]\ndelete=[\"/**\"]\n"
    ))
    .expect("built-in policy")
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

/// `value.get`'s `path` grammar (TDD §7): dot fields and `[n]` indexes,
/// e.g. `rows[3].name`, `out.lines[0]`. Structural fields on non-`Record`
/// values (outcome/error/range/task/table) are synthesized so an agent can
/// walk into them the same way it would a plain record.
#[derive(Debug, Clone)]
enum PathSeg {
    Field(String),
    Index(usize),
}

fn parse_value_path(path: &str) -> Result<Vec<PathSeg>, String> {
    let mut segs = Vec::new();
    let bytes = path.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        match bytes[i] {
            b'.' => {
                i += 1;
                continue;
            }
            b'[' => {
                let close = path[i + 1..]
                    .find(']')
                    .map(|p| p + i + 1)
                    .ok_or_else(|| format!("unterminated `[` in path `{path}`"))?;
                let digits = &path[i + 1..close];
                let idx = digits
                    .parse::<usize>()
                    .map_err(|_| format!("bad index `{digits}` in path `{path}`"))?;
                segs.push(PathSeg::Index(idx));
                i = close + 1;
                continue;
            }
            _ => {}
        }
        let start = i;
        while i < n && bytes[i] != b'.' && bytes[i] != b'[' {
            i += 1;
        }
        if i == start {
            return Err(format!("empty path segment in `{path}`"));
        }
        segs.push(PathSeg::Field(path[start..i].to_string()));
    }
    Ok(segs)
}

fn path_field(value: &Value, name: &str) -> Result<Value, String> {
    match value {
        Value::Record(rec) => rec
            .get(name)
            .cloned()
            .ok_or_else(|| format!("record has no field `{name}`")),
        Value::Outcome(o) => Ok(match name {
            "status" => o
                .status
                .map(|s| Value::Int(s as i64))
                .unwrap_or(Value::Null),
            "ok" => Value::Bool(o.ok),
            "signal" => o.signal.clone().map(Value::Str).unwrap_or(Value::Null),
            "out" => o.out_value(),
            "stdout" => Value::Bytes(o.stdout.clone()),
            "stderr" => Value::Bytes(o.stderr.clone()),
            "dur_ns" => Value::Duration(o.dur_ns),
            "pid" => Value::Int(o.pid as i64),
            "cmd" => Value::Str(o.cmd.clone()),
            // Unknown field names forward to the structured `.out` value,
            // mirroring eval's Value::Outcome field-access contract.
            _ => return path_field(&o.out_value(), name),
        }),
        Value::Error(e) => Ok(match name {
            "code" => Value::Str(e.code.clone()),
            "msg" => Value::Str(e.msg.clone()),
            "hint" => e.hint.clone().map(Value::Str).unwrap_or(Value::Null),
            "stderr" => e.stderr.clone().map(Value::Str).unwrap_or(Value::Null),
            "status" => e
                .status
                .map(|s| Value::Int(s as i64))
                .unwrap_or(Value::Null),
            _ => return Err(format!("error has no field `{name}`")),
        }),
        Value::Range(r) => Ok(match name {
            "start" => Value::Int(r.start),
            "end" => Value::Int(r.end),
            "inclusive" => Value::Bool(r.inclusive),
            _ => return Err(format!("range has no field `{name}`")),
        }),
        Value::Task(t) => Ok(match name {
            "id" => Value::Int(t.id as i64),
            "done" => Value::Bool(t.is_done()),
            _ => return Err(format!("task has no field `{name}`")),
        }),
        Value::Table(rows) => {
            if name == "rows" {
                Ok(Value::List(
                    rows.iter().cloned().map(Value::Record).collect(),
                ))
            } else if rows.iter().any(|r| r.contains_key(name)) {
                Ok(Value::List(
                    rows.iter()
                        .map(|r| r.get(name).cloned().unwrap_or(Value::Null))
                        .collect(),
                ))
            } else {
                Err(format!("table has no column `{name}`"))
            }
        }
        other => Err(format!(
            "cannot access field `{name}` on {}",
            other.type_name()
        )),
    }
}

fn path_index(value: &Value, idx: usize) -> Result<Value, String> {
    match value {
        Value::List(items) => items
            .get(idx)
            .cloned()
            .ok_or_else(|| format!("index [{idx}] out of bounds (len {})", items.len())),
        Value::Table(rows) => rows
            .get(idx)
            .cloned()
            .map(Value::Record)
            .ok_or_else(|| format!("index [{idx}] out of bounds (len {})", rows.len())),
        other => Err(format!("cannot index {} with [{idx}]", other.type_name())),
    }
}

fn resolve_value_path(value: &Value, path: &str) -> Result<Value, String> {
    let mut current = value.clone();
    for seg in parse_value_path(path)? {
        current = match seg {
            PathSeg::Field(name) => path_field(&current, &name)?,
            PathSeg::Index(idx) => path_index(&current, idx)?,
        };
    }
    Ok(current)
}

fn wire_value(value: &Value) -> WireValue {
    match value {
        Value::Null => WireValue::Null,
        Value::Bool(v) => WireValue::Bool { v: *v },
        Value::Int(v) => WireValue::Int { v: *v },
        Value::Float(v) => WireValue::Float { v: *v },
        Value::Str(v) => WireValue::Str { v: v.clone() },
        Value::Path(v) => {
            let p = WirePath::encode(v.as_os_str());
            WireValue::Path {
                v: p.display,
                raw: p.raw,
            }
        }
        Value::Glob(g) => WireValue::Glob {
            pattern: g.pattern.clone(),
        },
        Value::Regex(r) => WireValue::Regex { src: r.src.clone() },
        Value::Size(v) => WireValue::Size { v: *v },
        Value::Duration(v) => WireValue::Duration { v: *v },
        Value::DateTime(z) => WireValue::DateTime {
            v: z.timestamp().to_string(),
        },
        Value::Time(t) => WireValue::Time {
            v: format!("{:02}:{:02}:{:02}", t.hour, t.min, t.sec),
        },
        Value::Bytes(v) => WireValue::Bytes {
            v: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &**v),
        },
        Value::List(v) => WireValue::List {
            v: v.iter().map(wire_value).collect(),
        },
        Value::Record(rec) => WireValue::Record {
            v: rec
                .iter()
                .map(|(k, v)| (k.clone(), wire_value(v)))
                .collect(),
        },
        Value::Table(rows) => {
            let mut names: Vec<&String> = Vec::new();
            for row in rows {
                for k in row.keys() {
                    if !names.contains(&k) {
                        names.push(k);
                    }
                }
            }
            let cols = names
                .into_iter()
                .map(|name| {
                    let col = rows
                        .iter()
                        .map(|row| row.get(name).map(wire_value).unwrap_or(WireValue::Null))
                        .collect();
                    (name.clone(), col)
                })
                .collect();
            WireValue::Table {
                cols,
                n: rows.len(),
            }
        }
        Value::Range(r) => WireValue::Range {
            start: r.start,
            end: r.end,
            inclusive: r.inclusive,
        },
        Value::Stream(s) => WireValue::Stream {
            label: s.label.clone(),
        },
        Value::Error(e) => WireValue::Error {
            code: e.code.clone(),
            msg: e.msg.clone(),
            span: e.span.map(|s| WireSpan {
                start: s.start,
                end: s.end,
            }),
            hint: e.hint.clone(),
            stderr: e.stderr.clone(),
        },
        Value::Outcome(o) => WireValue::Outcome {
            status: o.status,
            ok: o.ok,
            signal: o.signal.clone(),
            out: Box::new(wire_value(&o.out_value())),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
            dur_ns: o.dur_ns,
            pid: o.pid,
            cmd: o.cmd.clone(),
            span: None,
        },
        Value::Task(t) => WireValue::Task {
            id: t.id,
            done: t.is_done(),
        },
        Value::Closure(_) | Value::CmdRef(_) => {
            let repr = shoal_value::render::render_inline(value);
            if matches!(value, Value::Closure(_)) {
                WireValue::Closure { repr }
            } else {
                WireValue::Cmd { repr }
            }
        }
        Value::Secret(s) => WireValue::Secret {
            name: s.name.clone(),
        },
    }
}

// ---------------------------------------------------------------------------
// The elision rule (AGENT-SURFACE §3) — wire-level, automatic.
// ---------------------------------------------------------------------------

/// Kernel defaults; a caller's `elide` param may tighten or loosen these, but
/// `max_bytes`/`max_bytes_raw` never loosen past `ELIDE_HARD_CAP`.
const ELIDE_DEFAULT_MAX_BYTES: usize = 8 * 1024;
const ELIDE_DEFAULT_MAX_ROWS: usize = 100;
const ELIDE_DEFAULT_MAX_BYTES_RAW: usize = 4 * 1024;
const ELIDE_DEFAULT_MAX_ITEMS: usize = 500;
/// A misbehaving agent cannot flood itself: no per-call override widens the
/// byte budget past this, regardless of what it asks for.
const ELIDE_HARD_CAP: usize = 64 * 1024;
/// Rows/items kept in the `preview` field and the human `render_head`.
const ELIDE_PREVIEW_ITEMS: usize = 5;
const ELIDE_PREVIEW_BYTES: usize = 256;

#[derive(Clone, Copy)]
struct ElideBudget {
    max_bytes: usize,
    max_rows: usize,
    max_bytes_raw: usize,
    max_items: usize,
}

impl Default for ElideBudget {
    fn default() -> Self {
        Self {
            max_bytes: ELIDE_DEFAULT_MAX_BYTES,
            max_rows: ELIDE_DEFAULT_MAX_ROWS,
            max_bytes_raw: ELIDE_DEFAULT_MAX_BYTES_RAW,
            max_items: ELIDE_DEFAULT_MAX_ITEMS,
        }
    }
}

impl ElideBudget {
    fn from_spec(spec: Option<&ElideSpec>) -> Self {
        let mut budget = Self::default();
        if let Some(spec) = spec {
            if let Some(max_bytes) = spec.max_bytes {
                let clamped = max_bytes.min(ELIDE_HARD_CAP);
                budget.max_bytes = clamped;
                budget.max_bytes_raw = clamped;
            }
            if let Some(max_rows) = spec.max_rows {
                budget.max_rows = max_rows;
            }
            if let Some(max_items) = spec.max_items {
                budget.max_items = max_items;
            }
        }
        budget
    }
}

/// `shoal://kind/id[?path=...]` from a short ref (`kind:id`), per
/// AGENT-SURFACE §1.
fn short_ref_to_uri(r: &Ref, path: Option<&str>) -> String {
    let mut uri = match r.0.split_once(':') {
        Some((kind, rest)) => format!("shoal://{kind}/{rest}"),
        None => format!("shoal://{}", r.0),
    };
    if let Some(path) = path.filter(|p| !p.is_empty()) {
        uri.push_str("?path=");
        uri.push_str(path);
    }
    uri
}

/// A small, bounded stand-in for `value` — first `ELIDE_PREVIEW_ITEMS`
/// rows/items, or the first `ELIDE_PREVIEW_BYTES` bytes/chars — never the
/// full payload, by construction (it never passes an unbounded child
/// through unchanged).
fn preview_value(value: &Value) -> Value {
    match value {
        Value::Table(rows) => {
            Value::Table(rows.iter().take(ELIDE_PREVIEW_ITEMS).cloned().collect())
        }
        Value::List(items) => {
            Value::List(items.iter().take(ELIDE_PREVIEW_ITEMS).cloned().collect())
        }
        Value::Bytes(b) => Value::Bytes(std::sync::Arc::new(
            b.iter().take(ELIDE_PREVIEW_BYTES).copied().collect(),
        )),
        Value::Str(s) => Value::Str(s.chars().take(ELIDE_PREVIEW_BYTES).collect()),
        Value::Record(rec) => Value::Record(
            rec.keys()
                .take(ELIDE_PREVIEW_ITEMS)
                .map(|k| (k.clone(), Value::Null))
                .collect(),
        ),
        _ => Value::Null,
    }
}

/// Column name -> type name, from the first row that carries each key.
fn table_cols(rows: &[shoal_value::Record]) -> std::collections::BTreeMap<String, String> {
    let mut cols = std::collections::BTreeMap::new();
    for row in rows {
        for (k, v) in row {
            cols.entry(k.clone())
                .or_insert_with(|| v.type_name().to_string());
        }
    }
    cols
}

/// `<uri>?path=<sub>`, chaining onto any path already present so a nested
/// drill (e.g. a successful command's `.out`) stays reachable through
/// `value.get`.
fn join_path_uri(uri: &str, sub_path: &str) -> String {
    match uri.split_once("?path=") {
        Some((base, existing)) => format!("{base}?path={existing}.{sub_path}"),
        None => format!("{uri}?path={sub_path}"),
    }
}

/// The elision rule (AGENT-SURFACE §3): if `value`'s wire encoding exceeds
/// `budget`, or it is an over-threshold table/list/bytes, emit an elided
/// `WireValue::Ref` (shape + small preview + render head) instead of the
/// payload. `uri` is how a caller re-fetches the full value later.
///
/// A successful `Outcome` whose structured `.out` is what actually carries
/// size (table/list/bytes/big string) is unwrapped one level for the
/// elision *decision* — mirroring `render_block`'s outcome-unification (P1c):
/// `ls` reads as a table to the elision rule too, not as an opaque
/// `outcome` wrapper. The outer outcome fields (`status`/`ok`/`cmd`/…)
/// always travel; only `.out` itself is replaced with the elided form.
fn elide_wire_value(value: &Value, uri: &str, budget: &ElideBudget) -> WireValue {
    if let Value::Outcome(o) = value
        && o.ok
    {
        let out_value = o.out_value();
        let out_uri = join_path_uri(uri, "out");
        return WireValue::Outcome {
            status: o.status,
            ok: o.ok,
            signal: o.signal.clone(),
            out: Box::new(elide_wire_value(&out_value, &out_uri, budget)),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
            dur_ns: o.dur_ns,
            pid: o.pid,
            cmd: o.cmd.clone(),
            span: None,
        };
    }
    let wire = wire_value(value);
    let encoded_len = serde_json::to_vec(&wire)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    let too_big = encoded_len > budget.max_bytes
        || matches!(value, Value::Table(rows) if rows.len() > budget.max_rows)
        || matches!(value, Value::List(items) if items.len() > budget.max_items)
        || matches!(value, Value::Bytes(b) if b.len() > budget.max_bytes_raw);
    if !too_big {
        return wire;
    }
    let n = match value {
        Value::Table(rows) => rows.len(),
        Value::List(items) => items.len(),
        Value::Bytes(b) => b.len(),
        Value::Str(s) => s.len(),
        Value::Record(rec) => rec.len(),
        _ => 1,
    };
    let cols = match value {
        Value::Table(rows) => Some(table_cols(rows)),
        _ => None,
    };
    let preview = preview_value(value);
    let render_head = shoal_value::render::render_block(&preview, 80)
        .lines()
        .take(10)
        .collect::<Vec<_>>()
        .join("\n");
    WireValue::Ref {
        uri: uri.to_string(),
        of: value.type_name().to_string(),
        n,
        cols,
        preview: Box::new(wire_value(&preview)),
        render_head,
    }
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
