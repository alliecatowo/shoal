//! Evaluator-facing channel methods and stream sinks.

use super::*;

impl Evaluator {
    /// `channel(name).emit/.events/.latest/.take` (site/content/internals/streams-channels.md).
    pub(crate) fn eval_channel_method(
        &mut self,
        chan: &str,
        name: &str,
        args: CallArgs,
    ) -> VResult<Value> {
        let bus = self.bus();
        match name {
            "emit" => {
                let payload = args
                    .pos
                    .first()
                    .cloned()
                    .ok_or_else(|| ErrorVal::arg_error("emit expects a value to publish"))?;
                bus.emit(chan, payload)?;
                Ok(Value::Null)
            }
            "events" => {
                let since = match args.get_named("since").or_else(|| args.pos.first()) {
                    Some(Value::Int(s)) if *s >= 0 => Some(*s as u64),
                    None => None,
                    Some(v) => {
                        return Err(ErrorVal::type_error(format!(
                            "events `since` expects an int seq, found {}",
                            v.type_name()
                        )));
                    }
                };
                Ok(Value::Stream(bus.event_stream(chan, since)?))
            }
            "latest" => bus.latest(chan),
            "take" => {
                let timeout = match args.get_named("timeout").or_else(|| args.pos.first()) {
                    Some(Value::Duration(ns)) if *ns >= 0 => Some(Duration::from_nanos(*ns as u64)),
                    None => None,
                    Some(v) => {
                        return Err(ErrorVal::type_error(format!(
                            "take `timeout` expects a duration, found {}",
                            v.type_name()
                        )));
                    }
                };
                let cancel = self.cancellation_token();
                bus.take_cancelled(chan, timeout, Some(&cancel))
            }
            _ => Err(ErrorVal::new(
                "field_missing",
                format!("unknown channel method `.{name}`"),
            )),
        }
    }

    /// Stream sinks needing the evaluator: `.into(channel(name))` republishes each
    /// item as an event; `.render()` drives the stream to the statement sink as a
    /// live view (site/content/internals/streams-channels.md). Both drive with `self` as the `CallCtx` directly
    /// (a manual pull loop) so each item can also reach an evaluator-only
    /// destination between pulls.
    pub(crate) fn eval_stream_sink(
        &mut self,
        recv: Value,
        name: &str,
        args: CallArgs,
    ) -> VResult<Value> {
        use shoal_value::Pull;
        let Value::Stream(s) = recv else {
            return Err(ErrorVal::type_error("stream sink on a non-stream"));
        };
        let target = if name == "into" {
            Some(
                args.pos
                    .first()
                    .and_then(as_channel)
                    .ok_or_else(|| {
                        ErrorVal::arg_error("into expects a channel: `.into(channel(\"name\"))`")
                    })?
                    .to_string(),
            )
        } else {
            None
        };
        let bus = self.bus();
        let mut up = s.take_upstream()?;
        loop {
            match up.pull(self, None)? {
                Pull::Item(v) => match &target {
                    Some(chan) => {
                        bus.emit(chan, v)?;
                    }
                    None => self.sink_value(&v),
                },
                Pull::End => break,
                Pull::Timeout => continue,
            }
        }
        Ok(Value::Null)
    }

    /// `on(channel(name) | name, handler)` (site/content/internals/streams-channels.md) — spawn a background task
    /// that runs `handler(event)` for every event on the channel. This is the
    /// in-language spelling of `spawn { channel(name).events().each(handler) }`
    /// (the bare `on channel(x){ev=>…}` keyword sugar needs a grammar change,
    /// which lives outside this crate). Returns the spawned `task`.
    pub(crate) fn builtin_on(&mut self, args: &Args) -> VResult<Value> {
        let a = self.eval_args(args)?;
        let chan = a
            .pos
            .first()
            .and_then(|v| match v {
                Value::Str(s) => Some(s.clone()),
                v => as_channel(v).map(str::to_string),
            })
            .ok_or_else(|| {
                ErrorVal::arg_error("on expects a channel (or channel name) then a handler")
            })?;
        let handler =
            a.pos.get(1).cloned().ok_or_else(|| {
                ErrorVal::arg_error("on expects a handler: `on(channel(\"x\"), f)`")
            })?;

        // Reserve before subscribing or creating task state. A rejected
        // handler leaves neither an idle subscriber nor a job row behind.
        let lease = self.host.native_workers.acquire()?;
        // Subscribe now (before spawning) so no event emitted between here and the
        // task starting is missed.
        let rx = self.bus().events(&chan, None)?;

        let task = TaskVal::new(format!("on channel({chan})"));
        // A FRESH cancel token wired to the task's cancel hook, so cancelling the
        // task interrupts the handler's exec tokens.
        let child_cancel = CancelToken::new();
        let hook_cancel = child_cancel.clone();
        task.on_cancel(Box::new(move || hook_cancel.cancel()));
        let worker = task.clone();
        // The one authoritative child constructor (HR-B1): the handler task runs
        // in a child that inherits the audited session context — leash policy/
        // principal, reef state, config, all effect ports, the event bus, and
        // session identity. The old hand-copy shared only the ports and bus,
        // dropping leash/reef/config (audit B1–B4). `Inherit` scope: the handler
        // sees the caller's bindings.
        let ctx = self.child_context();
        // Registry visibility must precede launch: a fast worker may otherwise
        // finish before its TaskVal can be discovered through jobs/task APIs.
        // A Builder failure completes this already-registered task below.
        self.exec.jobs.register(task.clone());
        let launch = std::thread::Builder::new()
            .name(format!("shoal-on-{chan}"))
            .spawn(move || {
                let _lease = lease;
                let mut ev = ctx.build(ChildKind::OnHandler, child_cancel.clone());
                let result = loop {
                    let event = match rx.recv(None, Some(&child_cancel)) {
                        Received::Event(event) => event,
                        Received::Gap(gap) => overflow_record(&chan, gap),
                        Received::Timeout => continue,
                        Received::Closed | Received::Cancelled => break Ok(Value::Null),
                        Received::Poisoned => break Err(channel_poisoned("subscriber queue")),
                    };
                    if let Err(e) = ev.call_closure(&handler, vec![event]) {
                        break Err(e);
                    }
                };
                worker.finish(result);
            });
        if let Err(error) = launch {
            let failure = ErrorVal::new(
                "task_spawn",
                format!("could not start channel handler task: {error}"),
            );
            task.finish(Err(failure.clone()));
            return Err(failure);
        }
        Ok(Value::Task(task))
    }
}
