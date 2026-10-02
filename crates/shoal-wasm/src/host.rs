use super::*;

pub struct Host {
    engine: Engine,
    pub(crate) limits: Limits,
    #[cfg(target_has_atomic = "64")]
    _deadline_ticker: DeadlineTicker,
}

#[derive(Default)]
pub(crate) struct CompilationAdmission {
    pub(crate) active: Mutex<usize>,
    changed: Condvar,
}

static COMPILATION_ADMISSION: std::sync::LazyLock<CompilationAdmission> =
    std::sync::LazyLock::new(CompilationAdmission::default);

pub(crate) struct CompilationLease<'a> {
    admission: &'a CompilationAdmission,
}

impl Drop for CompilationLease<'_> {
    fn drop(&mut self) {
        let mut active = match self.admission.active.lock() {
            Ok(active) => active,
            Err(poisoned) => {
                let active = poisoned.into_inner();
                self.admission.active.clear_poison();
                active
            }
        };
        *active = active.saturating_sub(1);
        self.admission.changed.notify_one();
    }
}

pub(crate) fn acquire_compilation<'a>(
    admission: &'a CompilationAdmission,
    limit: usize,
    wait: Duration,
    name: &str,
) -> Result<CompilationLease<'a>, PluginError> {
    let error = |message: String| PluginError::Component {
        name: name.into(),
        message,
    };
    let mut active = admission.active.lock().map_err(|_| {
        error("WASM compilation admission state is unavailable after an internal failure".into())
    })?;
    let deadline = Instant::now().checked_add(wait).ok_or_else(|| {
        error(format!(
            "WASM compilation admission wait {wait:?} exceeds the clock range"
        ))
    })?;
    while *active >= limit {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(error(format!(
                "WASM compilation admission limit ({limit}) remained full for {wait:?}"
            )));
        }
        let (next, timed) = admission
            .changed
            .wait_timeout(active, remaining)
            .map_err(|_| {
                error(
                    "WASM compilation admission state is unavailable after an internal failure"
                        .into(),
                )
            })?;
        active = next;
        if timed.timed_out() && *active >= limit {
            return Err(error(format!(
                "WASM compilation admission limit ({limit}) remained full for {wait:?}"
            )));
        }
    }
    *active += 1;
    Ok(CompilationLease { admission })
}

#[cfg(target_has_atomic = "64")]
struct DeadlineTicker {
    cancel: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_has_atomic = "64")]
impl DeadlineTicker {
    fn new(engine: Engine, wall_time: Duration) -> Result<Self, PluginError> {
        let tick = wall_time
            .min(Duration::from_millis(10))
            .max(Duration::from_millis(1));
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("shoal-wasm-epoch".into())
            .spawn(move || {
                while cancel_rx.recv_timeout(tick).is_err() {
                    engine.increment_epoch();
                }
            })
            .map_err(|error| PluginError::Component {
                name: "engine".into(),
                message: format!("failed to start WASM deadline ticker: {error}"),
            })?;
        Ok(Self {
            cancel: Some(cancel_tx),
            thread: Some(thread),
        })
    }
}

#[cfg(target_has_atomic = "64")]
impl Drop for DeadlineTicker {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Host {
    pub fn new(limits: Limits) -> Result<Self, PluginError> {
        validate_limits(limits)?;
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true)
            .max_wasm_stack(limits.wasm_stack_bytes)
            // Wasmtime otherwise reserves 4 GiB plus a large guard for every
            // 32-bit memory on 64-bit hosts. Reserve exactly the admitted
            // logical ceiling so repeated short invocations cannot exhaust VA.
            .memory_reservation(limits.memory_bytes as u64)
            .memory_reservation_for_growth(0)
            .memory_guard_size(64 * 1024)
            .memory_may_move(false)
            .wasm_memory64(false);
        let engine = Engine::new(&config).map_err(|error| PluginError::Component {
            name: "engine".into(),
            message: error.to_string(),
        })?;
        #[cfg(target_has_atomic = "64")]
        let deadline_ticker = DeadlineTicker::new(engine.clone(), limits.wall_time)?;
        Ok(Self {
            engine,
            limits,
            #[cfg(target_has_atomic = "64")]
            _deadline_ticker: deadline_ticker,
        })
    }

    pub fn validate(&self, manifest: Manifest) -> Result<ValidatedPlugin, PluginError> {
        manifest.validate_shape(&manifest.component)?;
        self.validate_manifest_limits(&manifest)?;
        let bytes =
            read_at_most(&manifest.component, self.limits.component_bytes).map_err(|message| {
                PluginError::Component {
                    name: manifest.name.clone(),
                    message,
                }
            })?;
        if bytes.is_empty() {
            return Err(PluginError::Component {
                name: manifest.name.clone(),
                message: "component file is empty".into(),
            });
        }
        let digest = blake3::hash(&bytes);
        let compilation = acquire_compilation(
            &COMPILATION_ADMISSION,
            self.limits.compilation_jobs,
            self.limits.compilation_wait,
            &manifest.name,
        )?;
        let component =
            Component::new(&self.engine, &bytes).map_err(|error| PluginError::Component {
                name: manifest.name.clone(),
                message: bounded_text(error.to_string(), self.limits.metadata_bytes),
            })?;
        drop(compilation);
        let mut store = self.new_store(
            &manifest.name,
            &manifest.effects,
            Arc::new(DenyAllCapabilities),
        )?;
        let linker = self.new_linker(&manifest.name)?;
        let bindings =
            self.instantiate_with_deadline(&manifest.name, &mut store, &linker, &component)?;
        #[cfg(target_has_atomic = "64")]
        store.set_epoch_deadline(1);
        let commands = self.with_deadline(&manifest.name, "commands metadata", || {
            bindings.call_commands(&mut store)
        })?;
        #[cfg(target_has_atomic = "64")]
        store.set_epoch_deadline(1);
        let methods = self.with_deadline(&manifest.name, "methods metadata", || {
            bindings.call_methods(&mut store)
        })?;
        self.validate_metadata(&manifest, &commands, &methods)?;
        Ok(ValidatedPlugin {
            manifest,
            bytes: bytes.into(),
            digest,
            component,
        })
    }

    pub fn invoke_command(
        &self,
        plugin: &ValidatedPlugin,
        command: &str,
        args: Vec<PluginValue>,
        capabilities: Arc<dyn CapabilityProvider>,
    ) -> Result<PluginValue, PluginError> {
        if !plugin
            .manifest
            .commands
            .iter()
            .any(|declaration| declaration.name == command)
        {
            return Err(PluginError::NotFound(format!(
                "{}::{command}",
                plugin.manifest.name
            )));
        }
        if args.len() > self.limits.arguments {
            return Err(PluginError::Value(format!(
                "argument count exceeds the {}-argument limit",
                self.limits.arguments
            )));
        }
        let mut guest_args = Vec::with_capacity(args.len());
        let mut argument_bytes = 0usize;
        for value in args {
            let value = value.into_guest(
                self.limits.value_bytes,
                self.limits.value_depth,
                self.limits.value_nodes,
            )?;
            argument_bytes = argument_bytes
                .checked_add(value.payload.len())
                .ok_or_else(|| PluginError::Value("argument byte accounting overflowed".into()))?;
            if argument_bytes > self.limits.value_bytes {
                return Err(PluginError::Value(format!(
                    "arguments exceed the {}-byte aggregate limit",
                    self.limits.value_bytes
                )));
            }
            guest_args.push(value);
        }
        let cancellation = capabilities.clone();
        let mut store = self.new_store(
            &plugin.manifest.name,
            &plugin.manifest.effects,
            capabilities,
        )?;
        let linker = self.new_linker(&plugin.manifest.name)?;
        let bindings = self.instantiate_with_deadline(
            &plugin.manifest.name,
            &mut store,
            &linker,
            &plugin.component,
        );
        let bindings = match bindings {
            Ok(bindings) => bindings,
            Err(_) if cancellation.cancelled() => return Err(PluginError::Cancelled),
            Err(error) => return Err(error),
        };
        #[cfg(target_has_atomic = "64")]
        store.set_epoch_deadline(1);
        let result = self.with_deadline(&plugin.manifest.name, "command invocation", || {
            bindings.call_invoke_command(&mut store, command, &guest_args)
        });
        let result = match result {
            Ok(result) => result,
            Err(_) if cancellation.cancelled() => return Err(PluginError::Cancelled),
            Err(error) => return Err(error),
        };
        match result {
            Ok(value) => PluginValue::from_guest(
                value,
                self.limits.value_bytes,
                self.limits.value_depth,
                self.limits.value_nodes,
            ),
            Err(error) => {
                let error_bytes =
                    error.message.len() + error.details_json.as_ref().map_or(0, String::len);
                if error_bytes > self.limits.metadata_bytes {
                    return Err(PluginError::Value(format!(
                        "guest error exceeds the {}-byte limit",
                        self.limits.metadata_bytes
                    )));
                }
                Err(PluginError::Guest {
                    name: plugin.manifest.name.clone(),
                    kind: format!("{:?}", error.kind),
                    message: error.message,
                    details_json: error.details_json,
                })
            }
        }
    }

    fn new_store(
        &self,
        name: &str,
        declared_effects: &[Effect],
        capabilities: Arc<dyn CapabilityProvider>,
    ) -> Result<Store<State>, PluginError> {
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.memory_bytes)
            .memories(self.limits.memories)
            .table_elements(self.limits.table_elements)
            .tables(self.limits.tables)
            .instances(self.limits.instances)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(
            &self.engine,
            State {
                limits: store_limits,
                capabilities,
                hostcall_bytes: self.limits.hostcall_bytes,
                hostcall_remaining_bytes: self.limits.hostcall_total_bytes,
                hostcall_remaining_calls: self.limits.hostcall_calls,
                declared_effects: declared_effects.to_vec(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|error| PluginError::Component {
                name: name.to_string(),
                message: bounded_text(error.to_string(), self.limits.metadata_bytes),
            })?;
        #[cfg(target_has_atomic = "64")]
        {
            let wall_time = self.limits.wall_time;
            let started = std::time::Instant::now();
            let cancellation = store.data().capabilities.clone();
            store.epoch_deadline_callback(move |_| {
                Ok(
                    if cancellation.cancelled() || started.elapsed() >= wall_time {
                        UpdateDeadline::Interrupt
                    } else {
                        UpdateDeadline::Continue(1)
                    },
                )
            });
            store.set_epoch_deadline(1);
        }
        Ok(store)
    }

    fn new_linker(&self, name: &str) -> Result<Linker<State>, PluginError> {
        let mut linker = Linker::new(&self.engine);
        abi::Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state).map_err(
            |error| PluginError::Component {
                name: name.to_string(),
                message: format!("cannot configure Shoal ABI v1 imports: {error}"),
            },
        )?;
        Ok(linker)
    }

    fn instantiate_with_deadline(
        &self,
        name: &str,
        store: &mut Store<State>,
        linker: &Linker<State>,
        component: &Component,
    ) -> Result<abi::Plugin, PluginError> {
        self.with_deadline(name, "Shoal ABI v1 instantiation", || {
            abi::Plugin::instantiate(store, component, linker)
        })
    }

    fn with_deadline<T>(
        &self,
        name: &str,
        operation: &str,
        call: impl FnOnce() -> wasmtime::Result<T>,
    ) -> Result<T, PluginError> {
        call().map_err(|error| PluginError::Component {
            name: name.to_string(),
            message: bounded_text(
                format!("{operation} failed: {error}"),
                self.limits.metadata_bytes,
            ),
        })
    }

    fn validate_metadata(
        &self,
        manifest: &Manifest,
        commands: &[Declaration],
        methods: &[MethodDeclaration],
    ) -> Result<(), PluginError> {
        let reject = |message: String| PluginError::Component {
            name: manifest.name.clone(),
            message,
        };
        let declarations = commands
            .len()
            .checked_add(methods.len())
            .ok_or_else(|| reject("guest declaration accounting overflowed".into()))?;
        if declarations > self.limits.declarations {
            return Err(reject(format!(
                "guest metadata exceeds the {}-declaration limit",
                self.limits.declarations
            )));
        }
        let metadata_bytes = commands
            .iter()
            .map(|declaration| declaration.name.len() + declaration.signature_json.len())
            .chain(methods.iter().map(|declaration| {
                declaration.type_name.len()
                    + declaration.name.len()
                    + declaration.signature_json.len()
            }))
            .try_fold(0usize, usize::checked_add)
            .ok_or_else(|| reject("guest metadata accounting overflowed".into()))?;
        if metadata_bytes > self.limits.metadata_bytes {
            return Err(reject(format!(
                "guest metadata exceeds the {}-byte limit",
                self.limits.metadata_bytes
            )));
        }
        let guest_commands = commands
            .iter()
            .map(|declaration| {
                let signature =
                    serde_json::from_str::<serde_json::Value>(&declaration.signature_json)
                        .map_err(|error| {
                            reject(format!(
                                "guest command `{}` returned invalid signature JSON: {error}",
                                declaration.name
                            ))
                        })?;
                Ok((declaration.name.as_str(), signature))
            })
            .collect::<Result<BTreeMap<_, _>, PluginError>>()?;
        if guest_commands.len() != commands.len() {
            return Err(reject("guest returned duplicate command metadata".into()));
        }
        let manifest_commands = manifest
            .commands
            .iter()
            .map(|declaration| {
                Ok((
                    declaration.name.as_str(),
                    serde_json::from_str::<serde_json::Value>(&declaration.signature).map_err(
                        |error| {
                            reject(format!(
                                "manifest command `{}` has invalid signature JSON: {error}",
                                declaration.name
                            ))
                        },
                    )?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, PluginError>>()?;
        if guest_commands != manifest_commands {
            return Err(reject(
                "guest command metadata does not match its manifest".into(),
            ));
        }
        let guest_methods = methods
            .iter()
            .map(|declaration| {
                let signature =
                    serde_json::from_str::<serde_json::Value>(&declaration.signature_json)
                        .map_err(|error| {
                            reject(format!(
                                "guest method `{}.{}` returned invalid signature JSON: {error}",
                                declaration.type_name, declaration.name
                            ))
                        })?;
                Ok((
                    (declaration.type_name.as_str(), declaration.name.as_str()),
                    signature,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, PluginError>>()?;
        if guest_methods.len() != methods.len() {
            return Err(reject("guest returned duplicate method metadata".into()));
        }
        let manifest_methods = manifest
            .methods
            .iter()
            .map(|declaration| {
                Ok((
                    (declaration.type_name.as_str(), declaration.name.as_str()),
                    serde_json::from_str::<serde_json::Value>(&declaration.signature).map_err(
                        |error| {
                            reject(format!(
                                "manifest method `{}.{}` has invalid signature JSON: {error}",
                                declaration.type_name, declaration.name
                            ))
                        },
                    )?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, PluginError>>()?;
        if guest_methods != manifest_methods {
            return Err(reject(
                "guest method metadata does not match its manifest".into(),
            ));
        }
        Ok(())
    }

    fn validate_manifest_limits(&self, manifest: &Manifest) -> Result<(), PluginError> {
        let reject = |message: String| PluginError::Manifest {
            path: manifest.component.clone(),
            message,
        };
        let declarations = manifest
            .commands
            .len()
            .checked_add(manifest.methods.len())
            .ok_or_else(|| reject("manifest declaration accounting overflowed".into()))?;
        if declarations > self.limits.declarations {
            return Err(reject(format!(
                "manifest exceeds the {}-declaration limit",
                self.limits.declarations
            )));
        }
        if manifest.effects.len() > self.limits.declarations {
            return Err(reject(format!(
                "manifest exceeds the {}-effect limit",
                self.limits.declarations
            )));
        }
        let metadata_bytes = manifest
            .commands
            .iter()
            .map(|declaration| declaration.name.len() + declaration.signature.len())
            .chain(manifest.methods.iter().map(|declaration| {
                declaration.type_name.len() + declaration.name.len() + declaration.signature.len()
            }))
            .try_fold(0usize, usize::checked_add)
            .ok_or_else(|| reject("manifest metadata accounting overflowed".into()))?;
        if metadata_bytes > self.limits.metadata_bytes {
            return Err(reject(format!(
                "manifest metadata exceeds the {}-byte limit",
                self.limits.metadata_bytes
            )));
        }
        Ok(())
    }
}
