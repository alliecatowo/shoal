//! Long-lived Unix-socket host for the shoal evaluator (TDD §10).

use serde_json::{Value as Json, json};
use shoal_auth::TokenStore;
use shoal_eval::Evaluator;
use shoal_journal::{EntryRecord, Journal, JournalQuery};
use shoal_leash::{Effect, Estimates, Plan, Policy, Reversibility, Verdict};
use shoal_proto::*;
use shoal_value::Value;
use std::collections::HashMap;
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
        let mut writer = stream;
        let mut attached: Option<Attachment> = None;
        while let Some(request) = read_frame(&mut reader)? {
            let id = request.id.clone();
            let response = if request.jsonrpc != JSONRPC {
                Response::err(id, -32600, "invalid JSON-RPC version", None)
            } else {
                self.dispatch(request, client, &mut attached)
            };
            write_frame(&mut writer, &response)?;
        }
        Ok(())
    }

    fn dispatch(
        self: &Arc<Self>,
        request: Request,
        client: u64,
        attached: &mut Option<Attachment>,
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
                if params.asynchronous {
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
                    let kernel = self.clone();
                    let mut task_attached = Some(attachment.clone());
                    std::thread::spawn(move || {
                        let response = kernel.dispatch(
                            Request {
                                jsonrpc: JSONRPC.into(),
                                id: Json::Null,
                                method: "exec".into(),
                                params: serde_json::to_value(ExecParams {
                                    asynchronous: false,
                                    ..params
                                })
                                .unwrap(),
                            },
                            client,
                            &mut task_attached,
                        );
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
                        task.done.notify_all();
                    });
                    return encode(json!({"task":task_ref}));
                }
                if params.mode == "plan" {
                    shoal_syntax::parse(&params.src).map_err(|e| RpcError {
                        code: -32001,
                        message: e.msg,
                        data: Some(json!({"span":e.span,"hint":e.hint})),
                    })?;
                    let plan = Plan::new(
                        vec![Effect::Opaque],
                        Reversibility::Unknown,
                        Estimates::default(),
                    );
                    let verdict = self.policy.evaluate_plan(&actor, &plan);
                    let result = PlanResult {
                        plan_ref: plan.plan_ref.clone(),
                        effects: plan
                            .effects
                            .iter()
                            .map(|e| serde_json::to_value(e).unwrap())
                            .collect(),
                        reversibility: "unknown".into(),
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
                if params.mode == "run" {
                    let direct_plan = Plan::new(
                        vec![Effect::Opaque],
                        Reversibility::Unknown,
                        Estimates::default(),
                    );
                    match self.policy.evaluate_plan(&actor, &direct_plan) {
                        Verdict::Deny => {
                            return Err(RpcError {
                                code: -32010,
                                message: "leash denied opaque execution".into(),
                                data: None,
                            });
                        }
                        Verdict::ApprovalRequired => {
                            return Err(RpcError {
                                code: -32011,
                                message: "approval required; plan first".into(),
                                data: None,
                            });
                        }
                        Verdict::Allow => {}
                    }
                }
                let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
                    code: -32001,
                    message: e.msg,
                    data: Some(json!({"span":e.span,"hint":e.hint})),
                })?;
                let mut evaluator = session.evaluator.lock().unwrap();
                evaluator.interactive = false;
                let started = Instant::now();
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
                        ast_json: serde_json::to_string(&ast).map_err(internal)?,
                        effects_json: "[\"opaque\"]".into(),
                        opaque: true,
                    })
                    .map_err(internal)?;
                let value = evaluator.eval_program(&ast).map_err(|e| {
                    let journal = self.journal.lock().unwrap();
                    let _ = journal.finish(entry_id, e.status, false, elapsed_ns(started));
                    if let Some(stderr) = &e.stderr { let _ = journal.record_output(entry_id, "stderr", stderr.as_bytes()); }
                    RpcError { code: -32002, message: e.msg, data: Some(json!({"code":e.code,"span":e.span,"hint":e.hint,"status":e.status,"stderr":e.stderr})) }
                })?;
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
                encode(ExecResult {
                    r#ref: value_ref,
                    value: Some(wire_value(&value)),
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
                let mut wire = wire_value(value);
                if let (Some([start, end]), WireValue::List { v: items }) =
                    (params.slice, &mut wire)
                {
                    *items = items
                        .get(start.min(items.len())..end.min(items.len()))
                        .unwrap_or(&[])
                        .to_vec();
                }
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
                        })
                        .unwrap(),
                    },
                    client,
                    attached,
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
                stored.approved = true;
                encode(json!({"grant":"approved","plan_ref":plan_ref,"enforced":false}))
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
                let entries: Vec<JournalEntry> = rows
                    .into_iter()
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
    Policy::from_toml(&format!(
        "[principal.\"{}\"]\nopaque='allow'\nauto_apply='in-grant'\njournal_read=true\n",
        principal()
    ))
    .expect("built-in policy")
}
fn verdict_name(v: Verdict) -> &'static str {
    match v {
        Verdict::Allow => "allow",
        Verdict::Deny => "deny",
        Verdict::ApprovalRequired => "approval_required",
    }
}
unsafe fn libc_geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
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
        Value::Size(v) => WireValue::Size { v: *v },
        Value::Duration(v) => WireValue::Duration { v: *v },
        Value::Bytes(v) => WireValue::Bytes {
            v: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &**v),
        },
        Value::List(v) => WireValue::List {
            v: v.iter().map(wire_value).collect(),
        },
        _ => WireValue::Str {
            v: shoal_value::render::render_inline(value),
        },
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
                json!({"src":"1 + 2","mode":"plan","position":"stmt"}),
            );
            let result = planned.result.unwrap();
            assert_eq!(result["verdict"], expected);
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
                assert_eq!(applied.result.unwrap()["value"]["v"], 3);
            } else {
                assert!(grant.error.is_some());
            }
            drop(client);
            drop(reader);
            thread.join().unwrap();
        }
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
}
