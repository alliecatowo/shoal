//! `dispatch` handlers for the interactive-PTY surface (site/content/internals/kernel-protocol.md):
//! `pty.open`, `pty.send`, `pty.read`, `pty.resize`, `pty.close`.
//!
//! An agent attached over the wire is `tty:false` and gets value-capture only;
//! there is no terminal for it to see. These methods give it a *rendered
//! screen* instead: `pty.open` spawns an interactive program (vim, an
//! installer prompt, a REPL, a TUI) on a real PTY as a long-lived, keyed kernel
//! session whose output is fed through a `vt100` terminal emulator; `pty.send`
//! delivers keystrokes (raw text OR named keys like `Enter`/`Escape`/`Ctrl-C`);
//! `pty.read` returns the emulator's `cols×rows` screen grid as text rows plus
//! the cursor position and a `changed` bit — never a wall of escape bytes, and
//! naturally bounded by the grid size. `pty.close` terminates and reaps.
//!
//! Spawning is a [`Effect::ProcSpawn`] gated through the same leash path every
//! other spawn uses (`spawn_pinning_active` guard + `evaluate_effect`, plus
//! `sandbox_for` for OS confinement), so a scoped principal's `bin_hash`/policy
//! applies; the default-permissive human spawns unconfined exactly as before.
use super::*;

use std::ffi::OsString;

impl Kernel {
    /// `pty.open {cmd, args?, cols?, rows?, env?}` → `{pty_id, pid, cols, rows,
    /// cmd}`. Spawns `cmd` on a fresh PTY sized `cols×rows` (defaults 80×24,
    /// clamped to the emulator bounds), registers the live session under a
    /// `pty:{id}` ref scoped to the caller, and starts rendering its output.
    pub(crate) fn handle_pty_open(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = attachment.session.clone();
        let actor = attachment.principal.clone();
        let p: PtyOpenParams = decode(params)?;
        if p.cmd.is_empty() {
            return Err(RpcError {
                code: INVALID_PARAMS,
                message: "pty.open requires a non-empty cmd".into(),
                data: None,
            });
        }

        // Build argv and the child's environment from the session's own
        // environment (so PATH etc. resolve like an in-session spawn), with the
        // caller's `env` overrides layered on top.
        let mut argv: Vec<OsString> = Vec::with_capacity(1 + p.args.len());
        argv.push(OsString::from(&p.cmd));
        argv.extend(p.args.iter().map(OsString::from));
        let (cwd, mut env) = {
            let evaluator = session.evaluator.lock().unwrap();
            (evaluator.cwd().to_path_buf(), evaluator.env_vars().to_vec())
        };
        for (k, v) in &p.env {
            let key = OsString::from(k);
            env.retain(|(ek, _)| ek != &key);
            env.push((key, OsString::from(v)));
        }

        let cols = p.cols.unwrap_or(shoal_exec::PTY_DEFAULT_COLS);
        let rows = p.rows.unwrap_or(shoal_exec::PTY_DEFAULT_ROWS);
        let display = argv
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");

        // Leash gate (site/content/internals/language-conformance-contract.md), mirroring the evaluator's `spawn_gate`: only
        // consult `evaluate_effect` once the principal has actually opted into
        // spawn pinning (a non-empty `proc_spawn` allowlist) — otherwise an
        // empty allowlist would default-deny every spawn. `sandbox_for` returns
        // `None` for the permissive human, so the child runs unconfined.
        if self.policy.spawn_pinning_active(&actor) {
            let bin_hash = shoal_exec::resolve_and_hash(&argv, &env).unwrap_or_default();
            let effect = Effect::ProcSpawn {
                bin_hash,
                argv0: p.cmd.clone(),
            };
            match self.policy.evaluate_effect(&actor, &effect) {
                Verdict::Allow => {}
                Verdict::ApprovalRequired => {
                    return Err(RpcError {
                        code: APPROVAL_REQUIRED,
                        message: "approval required to spawn this program on a pty".into(),
                        data: Some(json!({"cmd": p.cmd})),
                    });
                }
                Verdict::Deny => {
                    return Err(RpcError {
                        code: LEASH_DENIED,
                        message: format!(
                            "leash: spawn of `{}` on a pty denied — not in principal `{actor}`'s proc_spawn allowlist",
                            p.cmd
                        ),
                        data: Some(json!({"cmd": p.cmd})),
                    });
                }
            }
        }
        let sandbox = self.policy.sandbox_for(&actor);

        let owner = session.key.owner();
        self.reap_terminal_ptys(&owner);
        let active_slot = self.pty_slots.reserve(
            &owner,
            self.max_ptys_per_session.load(Ordering::Relaxed),
            "ptys_per_session",
            "PTY",
        )?;

        let pty_session = shoal_exec::PtySession::open(shoal_exec::PtyOpenSpec {
            argv,
            cwd,
            env,
            cols,
            rows,
            sandbox,
        })
        .map_err(|e| RpcError {
            code: PTY_SPAWN_FAILED,
            message: e.to_string(),
            data: Some(json!({"cmd": p.cmd})),
        })?;
        let pid = pty_session.pid();
        // The emulator may have clamped the requested cols/rows; report what
        // actually took (without an initial read, so the agent's first
        // `pty.read` still reports `changed:true`).
        let (cols, rows) = pty_session.size();

        let pty_id = self.next_pty.fetch_add(1, Ordering::Relaxed);
        let pty_ref = Ref::new("pty", pty_id);
        let entry = Arc::new(PtyEntry {
            owner,
            cmd: display.clone(),
            session: Mutex::new(pty_session),
            lifecycle: Mutex::new(PtyLifecycle {
                session_lease: Some(session),
                active_slot: Some(active_slot),
                terminal_since: None,
            }),
        });
        self.ptys
            .lock()
            .unwrap()
            .insert(pty_ref.clone(), entry.clone());

        // A child may exit without another client request. A lightweight
        // watcher releases active quota and the session lease promptly; the
        // final rendered screen remains available in the bounded registry.
        let weak_kernel = Arc::downgrade(self);
        let watched_ref = pty_ref.clone();
        let watched_owner = entry.owner.clone();
        let watched_entry = entry.clone();
        if let Err(error) = std::thread::Builder::new()
            .name(format!("shoal-pty-watch-{pty_id}"))
            .spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    let Some(kernel) = weak_kernel.upgrade() else {
                        break;
                    };
                    if watched_entry.session.lock().unwrap().alive() {
                        continue;
                    }
                    watched_entry.mark_terminal();
                    kernel.reap_terminal_ptys(&watched_owner);
                    break;
                }
            })
        {
            self.ptys.lock().unwrap().remove(&watched_ref);
            let _ = entry.session.lock().unwrap().close();
            entry.mark_terminal();
            return Err(internal(error));
        }
        encode(json!({
            "pty_id": pty_ref,
            "pid": pid,
            "cols": cols,
            "rows": rows,
            "cmd": display,
        }))
    }

    /// `pty.send {pty_id, input}` → `{pty_id, sent}`. Encodes `input` (raw
    /// string / named-key objects / an array mixing them) into terminal bytes
    /// and writes them to the PTY master.
    pub(crate) fn handle_pty_send(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let owner = attachment.session.key.owner();
        let p: PtySendParams = decode(params)?;
        let entry = self.pty(&p.pty_id, &owner)?;
        let bytes = encode_input(&p.input).map_err(|message| RpcError {
            code: INVALID_PARAMS,
            message,
            data: None,
        })?;
        entry
            .session
            .lock()
            .unwrap()
            .send(&bytes)
            .map_err(|e| RpcError {
                code: INTERNAL_ERROR,
                message: format!("pty write failed: {e}"),
                data: None,
            })?;
        encode(json!({"pty_id": p.pty_id, "sent": bytes.len()}))
    }

    /// `pty.read {pty_id}` → the rendered screen: `{pty_id, cols, rows, cursor,
    /// screen, changed, alive, exit, pid}`. `screen` is one plain-text string
    /// per row (bounded by `cols×rows`); `cursor` is `{row, col, hidden}`;
    /// `changed` says whether the rendered screen differs from the previous
    /// read; `exit` is null while alive, else `{status, signal}`.
    pub(crate) fn handle_pty_read(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let owner = attachment.session.key.owner();
        let p: PtyRefParams = decode(params)?;
        let entry = self.pty(&p.pty_id, &owner)?;
        let snap = entry.session.lock().unwrap().read_screen();
        if !snap.alive {
            entry.mark_terminal();
            self.reap_terminal_ptys(&owner);
        }
        encode(json!({
            "pty_id": p.pty_id,
            "cmd": entry.cmd,
            "cols": snap.cols,
            "rows": snap.rows,
            "cursor": {
                "row": snap.cursor_row,
                "col": snap.cursor_col,
                "hidden": snap.cursor_hidden,
            },
            "screen": snap.rows_text,
            "changed": snap.changed,
            "alive": snap.alive,
            "exit": exit_json(snap.exit_status, snap.exit_signal),
            "pid": snap.pid,
        }))
    }

    /// `pty.resize {pty_id, cols, rows}` → `{pty_id, cols, rows}`. Pushes a new
    /// window size to the child (`SIGWINCH`) and resizes the emulator grid.
    pub(crate) fn handle_pty_resize(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let owner = attachment.session.key.owner();
        let p: PtyResizeParams = decode(params)?;
        let entry = self.pty(&p.pty_id, &owner)?;
        entry
            .session
            .lock()
            .unwrap()
            .resize(p.cols, p.rows)
            .map_err(|e| RpcError {
                code: INTERNAL_ERROR,
                message: format!("pty resize failed: {e}"),
                data: None,
            })?;
        let snap = entry.session.lock().unwrap().read_screen();
        if !snap.alive {
            entry.mark_terminal();
            self.reap_terminal_ptys(&owner);
        }
        encode(json!({"pty_id": p.pty_id, "cols": snap.cols, "rows": snap.rows}))
    }

    /// `pty.close {pty_id}` → `{pty_id, closed:true, exit}`. Removes the session
    /// from the registry and terminates + reaps its child (no leak); dropping
    /// the entry runs the same teardown as a backstop.
    pub(crate) fn handle_pty_close(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let owner = attachment.session.key.owner();
        let p: PtyRefParams = decode(params)?;
        // Ownership check and removal are one registry transaction: exactly
        // one concurrent closer owns teardown, and foreign refs stay opaque.
        let entry = self.take_pty(&p.pty_id, &owner)?;
        let (status, signal) = entry.session.lock().unwrap().close();
        entry.mark_terminal();
        encode(json!({
            "pty_id": p.pty_id,
            "closed": true,
            "exit": exit_json(status, signal),
        }))
    }

    /// `pty.list {}` → `{ptys:[{pty_id, cmd, pid, cols, rows, alive}]}`.
    /// Enumerates the OPEN interactive PTY sessions for the ATTACHED session
    /// ONLY — the same session-scoping `pty.send`/`read`/`resize`/`close`
    /// enforce, so another session's ptys are invisible here exactly as
    /// `task.list` scopes tasks. This is the read side of the `shoal://pty`
    /// resource root: it makes open ptys first-class on the agent surface
    /// (discoverable + drill-in-able via `shoal://pty/{id}`), mirroring how an
    /// exec'd value becomes an addressable `shoal://` noun. Screen-free by
    /// design (a small enumeration, never a wall of grids) — an agent drills
    /// into one session's rendered screen with `pty.read` / `shoal://pty/{id}`.
    pub(crate) fn handle_pty_list(
        self: &Arc<Self>,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let owner = attachment.session.key.owner();
        // Snapshot the matching entries (clone the Arcs, drop the registry lock)
        // before touching any per-session lock, so this never holds `ptys` and a
        // `PtyEntry::session` lock at once.
        let mut entries: Vec<(u64, Arc<PtyEntry>)> = self
            .ptys
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, entry)| entry.owner == owner)
            .map(|(pty_ref, entry)| (pty_id_num(pty_ref), entry.clone()))
            .collect();
        // Stable, ascending order (open order) so the list is deterministic.
        entries.sort_by_key(|(id, _)| *id);
        let ptys: Vec<Json> = entries
            .iter()
            .map(|(id, entry)| {
                let mut session = entry.session.lock().unwrap();
                let (cols, rows) = session.size();
                let pid = session.pid();
                let alive = session.alive();
                drop(session);
                if !alive {
                    entry.mark_terminal();
                }
                json!({
                    "pty_id": Ref::new("pty", id),
                    "cmd": entry.cmd,
                    "pid": pid,
                    "cols": cols,
                    "rows": rows,
                    "alive": alive,
                })
            })
            .collect();
        self.reap_terminal_ptys(&owner);
        encode(json!({ "ptys": ptys }))
    }
}

/// The numeric id from a `pty:{id}` ref (0 if unparseable — never happens for a
/// ref the kernel minted, but keeps this total).
fn pty_id_num(pty_ref: &Ref) -> u64 {
    pty_ref
        .0
        .split_once(':')
        .and_then(|(_, id)| id.parse().ok())
        .unwrap_or(0)
}

/// Build the `exit` field: `null` while the child is alive, else `{status,
/// signal}` (one of them is set once it has terminated and been reaped).
fn exit_json(status: Option<i32>, signal: Option<String>) -> Json {
    if status.is_none() && signal.is_none() {
        Json::Null
    } else {
        json!({"status": status, "signal": signal})
    }
}

/// Encode a `pty.send` `input` into terminal bytes (site/content/internals/kernel-protocol.md
/// key-name protocol). Accepts:
/// - a **string** → sent verbatim as UTF-8;
/// - an **object** with exactly one of `key` (a named key → [`named_key`]),
///   `text` (literal), or `bytes` (base64);
/// - an **array** of any of the above, concatenated in order — so a caller can
///   express "type `i`, `hello`, Escape, `:wq`, Enter" as
///   `["i","hello",{"key":"Escape"},":wq",{"key":"Enter"}]`.
fn encode_input(input: &Json) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    encode_into(input, &mut out)?;
    Ok(out)
}

fn encode_into(item: &Json, out: &mut Vec<u8>) -> Result<(), String> {
    match item {
        Json::String(s) => out.extend_from_slice(s.as_bytes()),
        Json::Array(items) => {
            for it in items {
                encode_into(it, out)?;
            }
        }
        Json::Object(map) => {
            if let Some(key) = map.get("key").and_then(Json::as_str) {
                let bytes = shoal_exec::named_key(key)
                    .ok_or_else(|| format!("unknown key name {key:?} in pty.send input"))?;
                out.extend_from_slice(&bytes);
            } else if let Some(text) = map.get("text").and_then(Json::as_str) {
                out.extend_from_slice(text.as_bytes());
            } else if let Some(b64) = map.get("bytes").and_then(Json::as_str) {
                let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                    .map_err(|e| format!("pty.send bytes is not valid base64: {e}"))?;
                out.extend_from_slice(&bytes);
            } else {
                return Err(
                    "pty.send input object must have one of `key`, `text`, or `bytes`".into(),
                );
            }
        }
        other => {
            return Err(format!(
                "pty.send input must be a string, object, or array; got {other}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_input_handles_string_object_and_array() {
        assert_eq!(encode_input(&json!("hi")).unwrap(), b"hi");
        assert_eq!(encode_input(&json!({"key":"Enter"})).unwrap(), b"\r");
        assert_eq!(encode_input(&json!({"text":"abc"})).unwrap(), b"abc");
        // The canonical vim-editing sequence.
        assert_eq!(
            encode_input(&json!(["i", "hello", {"key":"Escape"}, ":wq", {"key":"Enter"}])).unwrap(),
            b"ihello\x1b:wq\r".to_vec()
        );
        // base64 bytes.
        assert_eq!(
            encode_input(&json!({"bytes":"aGVsbG8="})).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn encode_input_rejects_unknown_key_and_bad_shape() {
        assert!(encode_input(&json!({"key":"Nope"})).is_err());
        assert!(encode_input(&json!({"wat":"x"})).is_err());
        assert!(encode_input(&json!(42)).is_err());
    }

    #[test]
    fn exit_json_is_null_while_alive() {
        assert_eq!(exit_json(None, None), Json::Null);
        assert_eq!(exit_json(Some(0), None), json!({"status":0,"signal":null}));
        assert_eq!(
            exit_json(None, Some("SIGKILL".into())),
            json!({"status":null,"signal":"SIGKILL"})
        );
    }
}
