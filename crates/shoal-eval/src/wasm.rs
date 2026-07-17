//! WebAssembly component command integration: value boundary, session-scoped
//! capabilities, and typed invocation.

use super::*;
use shoal_wasm::{CapabilityError, CapabilityProvider, PluginError, PluginValue};
use std::io::Read as _;

struct SessionCapabilities {
    fs: Arc<dyn Fs>,
    clock: Arc<dyn Clock>,
    leash: Option<(LeashPolicy, String)>,
    cwd: PathBuf,
    cancel: CancelToken,
}

impl CapabilityProvider for SessionCapabilities {
    fn authorize(&self, effect: &Effect) -> Result<(), CapabilityError> {
        let Some((policy, principal)) = &self.leash else {
            return Ok(());
        };
        let scoped = scope_effect(effect, &self.cwd);
        match policy.evaluate_effect(principal, &scoped) {
            shoal_leash::Verdict::Allow => Ok(()),
            shoal_leash::Verdict::Deny => Err(CapabilityError {
                message: format!("leash denied plugin effect {scoped:?} for `{principal}`"),
            }),
            shoal_leash::Verdict::ApprovalRequired => Err(CapabilityError {
                message: format!("plugin effect {scoped:?} requires approval for `{principal}`"),
            }),
        }
    }

    fn now_ns(&self) -> Result<u64, CapabilityError> {
        Ok(self.clock.now_ns().max(0) as u64)
    }

    fn read_file(&self, path: &Path, max_bytes: usize) -> Result<Vec<u8>, CapabilityError> {
        let path = absolute_from(&self.cwd, path);
        let mut reader = self.fs.open_read(&path).map_err(|error| CapabilityError {
            message: format!("cannot open {}: {error}", path.display()),
        })?;
        let read_limit = max_bytes.saturating_add(1);
        let mut bytes = Vec::with_capacity(read_limit.min(64 * 1024));
        reader
            .by_ref()
            .take(u64::try_from(read_limit).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)
            .map_err(|error| CapabilityError {
                message: format!("cannot read {}: {error}", path.display()),
            })?;
        if bytes.len() > max_bytes {
            return Err(CapabilityError {
                message: format!(
                    "{} exceeds the {max_bytes}-byte plugin hostcall limit",
                    path.display()
                ),
            });
        }
        Ok(bytes)
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

fn absolute_from(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn scope_effect(effect: &Effect, cwd: &Path) -> Effect {
    let paths = |paths: &[PathBuf]| paths.iter().map(|path| absolute_from(cwd, path)).collect();
    match effect {
        Effect::FsRead { paths: requested } => Effect::FsRead {
            paths: paths(requested),
        },
        Effect::FsWrite { paths: requested } => Effect::FsWrite {
            paths: paths(requested),
        },
        Effect::FsDelete { paths: requested } => Effect::FsDelete {
            paths: paths(requested),
        },
        other => other.clone(),
    }
}

const PLUGIN_JSON_DEPTH: usize = 64;
const PLUGIN_JSON_NODES: usize = 65_536;

fn json_value(value: &Value) -> VResult<serde_json::Value> {
    let mut nodes = 0;
    json_value_bounded(value, 1, &mut nodes)
}

fn json_value_bounded(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
) -> VResult<serde_json::Value> {
    enter_plugin_json(depth, nodes)?;
    match value {
        Value::Null => Ok(serde_json::Value::Null),
        Value::Bool(value) => Ok((*value).into()),
        Value::Int(value) => Ok((*value).into()),
        Value::Float(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| ErrorVal::type_error("non-finite float cannot cross the plugin ABI")),
        Value::Str(value) => Ok(value.clone().into()),
        Value::Path(value) => value
            .to_str()
            .map(|value| serde_json::Value::String(value.to_owned()))
            .ok_or_else(|| ErrorVal::type_error("non-UTF-8 path cannot cross the plugin ABI")),
        Value::List(values) => values
            .iter()
            .map(|value| json_value_bounded(value, depth + 1, nodes))
            .collect::<VResult<Vec<_>>>()
            .map(serde_json::Value::Array),
        Value::Record(record) => record
            .iter()
            .map(|(name, value)| Ok((name.clone(), json_value_bounded(value, depth + 1, nodes)?)))
            .collect::<VResult<serde_json::Map<_, _>>>()
            .map(serde_json::Value::Object),
        Value::Table(rows) => rows
            .iter()
            .map(|row| {
                enter_plugin_json(depth + 1, nodes)?;
                row.iter()
                    .map(|(name, value)| {
                        Ok((name.clone(), json_value_bounded(value, depth + 2, nodes)?))
                    })
                    .collect::<VResult<serde_json::Map<_, _>>>()
                    .map(serde_json::Value::Object)
            })
            .collect::<VResult<Vec<_>>>()
            .map(serde_json::Value::Array),
        other => Err(ErrorVal::type_error(format!(
            "{} cannot be nested in a plugin JSON value",
            other.type_name()
        ))),
    }
}

fn enter_plugin_json(depth: usize, nodes: &mut usize) -> VResult<()> {
    if depth > PLUGIN_JSON_DEPTH {
        return Err(ErrorVal::new(
            "plugin_error",
            format!("plugin JSON exceeds the {PLUGIN_JSON_DEPTH}-level depth limit"),
        ));
    }
    *nodes = nodes
        .checked_add(1)
        .ok_or_else(|| ErrorVal::new("plugin_error", "plugin JSON node accounting overflowed"))?;
    if *nodes > PLUGIN_JSON_NODES {
        return Err(ErrorVal::new(
            "plugin_error",
            format!("plugin JSON exceeds the {PLUGIN_JSON_NODES}-node limit"),
        ));
    }
    Ok(())
}

fn into_plugin_value(value: Value) -> VResult<PluginValue> {
    match value {
        Value::Null => Ok(PluginValue::Null),
        Value::Bool(value) => Ok(PluginValue::Bool(value)),
        Value::Int(value) => Ok(PluginValue::Signed(value)),
        Value::Float(value) if value.is_finite() => Ok(PluginValue::Float(value)),
        Value::Float(_) => Err(ErrorVal::type_error(
            "non-finite float cannot cross the plugin ABI",
        )),
        Value::Str(value) => Ok(PluginValue::Text(value)),
        Value::Path(value) => value
            .into_os_string()
            .into_string()
            .map(PluginValue::Text)
            .map_err(|_| ErrorVal::type_error("non-UTF-8 path cannot cross the plugin ABI")),
        Value::Size(value) => Ok(PluginValue::Unsigned(value)),
        Value::Duration(value) => Ok(PluginValue::Signed(value)),
        Value::Bytes(value) => Ok(PluginValue::Bytes(value.as_ref().clone())),
        Value::List(_) | Value::Record(_) | Value::Table(_) => {
            json_value(&value).map(PluginValue::Json)
        }
        other => Err(ErrorVal::type_error(format!(
            "{} cannot cross the plugin ABI",
            other.type_name()
        ))),
    }
}

fn from_plugin_value(value: PluginValue) -> VResult<Value> {
    match value {
        PluginValue::Null => Ok(Value::Null),
        PluginValue::Bool(value) => Ok(Value::Bool(value)),
        PluginValue::Signed(value) => Ok(Value::Int(value)),
        PluginValue::Unsigned(value) => i64::try_from(value).map(Value::Int).map_err(|_| {
            ErrorVal::new(
                "plugin_error",
                "plugin returned an unsigned integer outside Shoal's i64 range",
            )
        }),
        PluginValue::Float(value) if value.is_finite() => Ok(Value::Float(value)),
        PluginValue::Float(_) => Err(ErrorVal::new(
            "plugin_error",
            "plugin returned a non-finite float",
        )),
        PluginValue::Text(value) => Ok(Value::Str(value)),
        PluginValue::Bytes(value) => Ok(Value::Bytes(Arc::new(value))),
        PluginValue::Json(value) => Ok(shoal_value::json_to_value(&value)),
    }
}

fn plugin_error(error: PluginError) -> ErrorVal {
    match error {
        PluginError::Cancelled => ErrorVal::new("cancelled", "plugin invocation cancelled"),
        PluginError::Guest {
            name,
            kind,
            message,
            details_json,
        } => {
            let error = ErrorVal::new(
                "plugin_error",
                format!("plugin `{name}` returned {kind}: {message}"),
            );
            match details_json {
                Some(details) => error.with_stderr(details),
                None => error,
            }
        }
        other => ErrorVal::new("plugin_error", other.to_string()),
    }
}

impl Evaluator {
    pub(crate) fn eval_wasm_command(&mut self, call: &CmdCall) -> VResult<Value> {
        if !call.env_prefix.is_empty() {
            return Err(ErrorVal::new(
                "plugin_error",
                "plugin ABI v1 does not expose command environment prefixes",
            ));
        }
        if call.trailing.is_some() {
            return Err(ErrorVal::new(
                "plugin_error",
                "plugin ABI v1 does not expose trailing block arguments",
            ));
        }
        if call
            .redirects
            .iter()
            .any(|redirect| redirect.kind == RedirectKind::In)
        {
            return Err(ErrorVal::new(
                "plugin_error",
                "plugin ABI v1 does not expose redirected stdin",
            ));
        }

        let registry = self
            .host
            .wasm
            .clone()
            .ok_or_else(|| ErrorVal::new("plugin_error", "plugin registry is unavailable"))?;
        let argument_limit = registry.argument_limit();
        let push_arg = |args: &mut Vec<PluginValue>, value: PluginValue| -> VResult<()> {
            if args.len() >= argument_limit {
                return Err(ErrorVal::new(
                    "plugin_error",
                    format!("plugin argument count exceeds the {argument_limit}-argument limit"),
                ));
            }
            args.push(value);
            Ok(())
        };
        let mut args = Vec::new();
        for argument in &call.args {
            match argument {
                CmdArg::FlagLong { name, value, .. } => {
                    push_arg(&mut args, PluginValue::Text(format!("--{name}")))?;
                    if let Some(value) = value {
                        for value in self.expand_arg(value)? {
                            push_arg(&mut args, into_plugin_value(value)?)?;
                        }
                    }
                }
                CmdArg::FlagShort { chars, .. } => {
                    push_arg(&mut args, PluginValue::Text(format!("-{chars}")))?;
                }
                CmdArg::DashDash { .. } => {
                    push_arg(&mut args, PluginValue::Text("--".into()))?;
                }
                _ => {
                    for value in self.expand_arg(argument)? {
                        push_arg(&mut args, into_plugin_value(value)?)?;
                    }
                }
            }
        }
        let capabilities = Arc::new(SessionCapabilities {
            fs: self.host.fs.clone(),
            clock: self.host.clock.clone(),
            leash: self.session.leash.clone(),
            cwd: self.exec.shell.cwd.clone(),
            cancel: self.exec.control.cancel.clone(),
        });
        let value = registry
            .invoke_declared_command(&call.head, args, capabilities)
            .map_err(plugin_error)
            .and_then(from_plugin_value)?;

        for redirect in &call.redirects {
            let target = self.arg_path(&redirect.target)?;
            let bytes = shoal_value::feed_bytes(&value)?;
            match redirect.kind {
                RedirectKind::Out => {
                    let undo = self.redirect_undo_pre(&target);
                    self.host
                        .fs
                        .write(&target, &bytes)
                        .map_err(|error| ErrorVal::new("io_error", error.to_string()))?;
                    self.overwrite_undo_post(undo);
                }
                RedirectKind::Append => self
                    .host
                    .fs
                    .append(&target, &bytes)
                    .map_err(|error| ErrorVal::new("io_error", error.to_string()))?,
                RedirectKind::In => unreachable!("input redirects rejected above"),
            }
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
    use wit_parser::{ManglingAndAbi, Resolve};

    fn set_func_i32_result(wat: &mut String, index: usize, result: u32) {
        let function = wat.find(&format!("(func (;{index};)")).unwrap();
        let body = wat[function..].find("i32.const 0").unwrap() + function;
        wat.replace_range(
            body..body + "i32.const 0".len(),
            &format!("i32.const {result}"),
        );
    }

    fn wat_data(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
    }

    fn invokable_component(command: &str, call_time: bool) -> Vec<u8> {
        let mut resolve = Resolve::new();
        let package = resolve
            .push_str(
                "shoal-plugin.wit",
                include_str!("../../shoal-wasm/wit/shoal-plugin.wit"),
            )
            .unwrap();
        let world = resolve.select_world(&[package], Some("plugin")).unwrap();
        let core = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
        let mut wat = wasmprinter::print_bytes(core)
            .unwrap()
            .replace("(memory (;0;) 0)", "(memory (;0;) 1)")
            .replace("unreachable", "i32.const 0");
        set_func_i32_result(&mut wat, 4, 16);
        set_func_i32_result(&mut wat, 6, 128);
        if call_time {
            let function = wat.find("(func (;6;)").unwrap();
            let result = wat[function..].find("i32.const 128").unwrap() + function;
            wat.insert_str(result, "i32.const 200\ncall 0\n");
        }

        let mut declaration = Vec::new();
        declaration.extend_from_slice(&64_u32.to_le_bytes());
        declaration.extend_from_slice(&(command.len() as u32).to_le_bytes());
        declaration.extend_from_slice(&(64_u32 + command.len() as u32).to_le_bytes());
        declaration.extend_from_slice(&2_u32.to_le_bytes());
        let metadata = format!("{command}{{}}");
        let mut null_result = [0_u8; 20];
        null_result[4] = 1;
        let end = wat.rfind(')').unwrap();
        wat.insert_str(
            end,
            &format!(
                concat!(
                    "(data (i32.const 0) \"{}\")\n",
                    "(data (i32.const 32) \"{}\")\n",
                    "(data (i32.const 64) \"{}\")\n",
                    "(data (i32.const 128) \"{}\")\n",
                ),
                wat_data(&[32, 0, 0, 0, 1, 0, 0, 0]),
                wat_data(&declaration),
                metadata,
                wat_data(&null_result),
            ),
        );
        let mut core = wat::parse_str(wat).unwrap();
        embed_component_metadata(&mut core, &resolve, world, StringEncoding::UTF8).unwrap();
        ComponentEncoder::default()
            .module(&core)
            .unwrap()
            .validate(true)
            .encode()
            .unwrap()
    }

    fn evaluator_with_plugin(
        command: &str,
        effects: &str,
        call_time: bool,
    ) -> (tempfile::TempDir, Evaluator) {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("plugin.wasm"),
            invokable_component(command, call_time),
        )
        .unwrap();
        fs::write(
            temp.path().join("plugin.toml"),
            format!(
                "name='fixture'\nversion='1'\nabi_version=1\ncomponent='plugin.wasm'\neffects={effects}\n\
                 [[commands]]\nname='{command}'\nsignature='{{}}'\n"
            ),
        )
        .unwrap();
        let mut evaluator = Evaluator::new(temp.path().to_path_buf());
        assert!(
            evaluator
                .load_wasm_plugins(&[temp.path().to_path_buf()])
                .unwrap()
                .is_empty()
        );
        (temp, evaluator)
    }

    #[derive(Default)]
    struct CountingClock(AtomicUsize);

    impl Clock for CountingClock {
        fn now_ns(&self) -> i64 {
            self.0.fetch_add(1, Ordering::SeqCst);
            42
        }
    }

    #[test]
    fn plugin_json_conversion_rejects_nested_secret_values() {
        let mut record = Record::new();
        record.insert(
            "token".into(),
            Value::Secret(shoal_value::SecretVal {
                name: "x".into(),
                value: Arc::from("classified"),
            }),
        );
        assert!(into_plugin_value(Value::Record(record)).is_err());
    }

    #[test]
    fn plugin_json_conversion_rejects_hostile_depth_before_allocating_json() {
        let mut value = Value::Null;
        for _ in 0..=PLUGIN_JSON_DEPTH {
            value = Value::List(vec![value]);
        }
        let error = into_plugin_value(value).unwrap_err();
        assert_eq!(error.code, "plugin_error");
        assert!(error.msg.contains("depth limit"));
    }

    #[test]
    fn session_file_capability_reads_only_a_bounded_prefix() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("large"), vec![0; 4096]).unwrap();
        let capabilities = SessionCapabilities {
            fs: Arc::new(shoal_value::StdFs),
            clock: Arc::new(shoal_value::StdClock),
            leash: None,
            cwd: temp.path().to_path_buf(),
            cancel: CancelToken::new(),
        };
        let error = capabilities.read_file(Path::new("large"), 16).unwrap_err();
        assert!(error.message.contains("16-byte"));
    }

    #[test]
    fn plugin_unsigned_overflow_is_not_lossy() {
        assert!(from_plugin_value(PluginValue::Unsigned(u64::MAX)).is_err());
    }

    #[test]
    fn registry_drives_resolution_exact_planning_and_typed_invocation() {
        let (_temp, mut evaluator) = evaluator_with_plugin("plug", "[{kind='time'}]", false);
        assert_eq!(
            evaluator.resolve_head("plug", false, false).source,
            shoal_syntax::commands::CommandSource::Plugin
        );
        assert_eq!(
            evaluator.resolve_head("plug", true, false).source,
            shoal_syntax::commands::CommandSource::External
        );

        let program = shoal_syntax::parse("plug").unwrap();
        assert_eq!(
            evaluator.plan_program(&program).unwrap().effects,
            vec![Effect::Time]
        );
        assert!(matches!(
            evaluator.eval_program(&program).unwrap(),
            Value::Null
        ));

        evaluator
            .env_mut()
            .declare("plug", Value::Int(7), false)
            .unwrap();
        assert_eq!(
            evaluator.resolve_head("plug", false, true).source,
            shoal_syntax::commands::CommandSource::BoundValue
        );
    }

    #[test]
    fn builtins_precede_colliding_plugin_commands() {
        let (_temp, evaluator) = evaluator_with_plugin("echo", "[]", false);
        assert_eq!(
            evaluator.resolve_head("echo", false, false).source,
            shoal_syntax::commands::CommandSource::StructuredBuiltin
        );
    }

    #[test]
    fn denied_session_capability_never_reaches_the_clock_port() {
        let (_temp, mut evaluator) = evaluator_with_plugin("plug", "[{kind='time'}]", true);
        let clock = Arc::new(CountingClock::default());
        evaluator.set_clock(clock.clone());
        evaluator.set_leash_policy(
            LeashPolicy::from_toml("[principal.agent]\ntime=false\n").unwrap(),
            "agent",
        );
        let program = shoal_syntax::parse("plug").unwrap();
        assert!(matches!(
            evaluator.eval_program(&program).unwrap(),
            Value::Null
        ));
        assert_eq!(clock.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn expanded_plugin_arguments_stop_at_the_registry_limit() {
        let (_temp, mut evaluator) = evaluator_with_plugin("plug", "[]", false);
        let source = format!("plug {}", "x ".repeat(257));
        let program = shoal_syntax::parse(&source).unwrap();
        let error = evaluator.eval_program(&program).unwrap_err();
        assert_eq!(error.code, "plugin_error");
        assert!(error.msg.contains("256-argument limit"));
    }
}
