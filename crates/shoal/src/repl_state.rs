//! Typed projection of the authenticated kernel session state used by the
//! interactive editor between commands.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value as Json;
use shoal_ast::{CmdCall, Span};
use shoal_proto::{WirePath, WireValue};
use shoal_value::{Env, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtocolBinding {
    pub(crate) name: String,
    pub(crate) callable: bool,
    pub(crate) type_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProtocolJobs {
    pub(crate) running: usize,
    pub(crate) suspended: usize,
    pub(crate) total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtocolReefBinding {
    pub(crate) tool: String,
    pub(crate) version: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) scope: Option<String>,
    pub(crate) constrained: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ProtocolSnapshot {
    pub(crate) cwd: PathBuf,
    pub(crate) bindings: Vec<ProtocolBinding>,
    pub(crate) jobs: ProtocolJobs,
    pub(crate) reef: Vec<ProtocolReefBinding>,
    pub(crate) last_value: WireValue,
}

impl ProtocolSnapshot {
    pub(crate) fn parse(value: Json) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "session.snapshot response is not an object".to_string())?;
        let wire_path: WirePath = serde_json::from_value(
            object
                .get("cwd")
                .cloned()
                .ok_or_else(|| "session.snapshot omitted cwd".to_string())?,
        )
        .map_err(|error| format!("session.snapshot cwd: {error}"))?;
        let cwd = PathBuf::from(
            wire_path
                .decode()
                .map_err(|error| format!("session.snapshot cwd encoding: {error}"))?,
        );
        let bindings = object
            .get("bindings")
            .and_then(Json::as_array)
            .ok_or_else(|| "session.snapshot omitted bindings".to_string())?
            .iter()
            .map(|binding| {
                Ok(ProtocolBinding {
                    name: required_str(binding, "name")?.to_string(),
                    callable: binding
                        .get("callable")
                        .and_then(Json::as_bool)
                        .ok_or_else(|| "session.snapshot binding omitted callable".to_string())?,
                    type_name: required_str(binding, "type")?.to_string(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let jobs = object
            .get("jobs")
            .ok_or_else(|| "session.snapshot omitted jobs".to_string())?;
        let jobs = ProtocolJobs {
            running: required_usize(jobs, "running")?,
            suspended: required_usize(jobs, "suspended")?,
            total: required_usize(jobs, "total")?,
        };
        let reef = object
            .get("reef")
            .and_then(|reef| reef.get("bindings"))
            .and_then(Json::as_array)
            .ok_or_else(|| "session.snapshot omitted reef.bindings".to_string())?
            .iter()
            .map(|binding| {
                Ok(ProtocolReefBinding {
                    tool: required_str(binding, "tool")?.to_string(),
                    version: optional_str(binding, "version")?,
                    provider: optional_str(binding, "provider")?,
                    scope: optional_str(binding, "scope")?,
                    constrained: binding
                        .get("constrained")
                        .and_then(Json::as_bool)
                        .unwrap_or(true),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let last_value = serde_json::from_value(
            object
                .get("last_value")
                .cloned()
                .ok_or_else(|| "session.snapshot omitted last_value".to_string())?,
        )
        .map_err(|error| format!("session.snapshot last_value: {error}"))?;
        Ok(Self {
            cwd,
            bindings,
            jobs,
            reef,
            last_value,
        })
    }
}

fn required_str<'a>(value: &'a Json, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("session.snapshot omitted {key}"))
}

fn optional_str(value: &Json, key: &str) -> Result<Option<String>, String> {
    match value.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("session.snapshot {key} is not a string or null")),
    }
}

fn required_usize(value: &Json, key: &str) -> Result<usize, String> {
    value
        .get(key)
        .and_then(Json::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| format!("session.snapshot omitted or overflowed {key}"))
}

#[derive(Default)]
pub(crate) struct RemoteEnvMirror {
    names: BTreeSet<String>,
}

impl RemoteEnvMirror {
    pub(crate) fn apply(
        &mut self,
        snapshot: &ProtocolSnapshot,
        env: &Env,
        cwd: &Arc<Mutex<PathBuf>>,
    ) {
        let incoming = snapshot
            .bindings
            .iter()
            .map(|binding| binding.name.clone())
            .collect::<BTreeSet<_>>();
        for stale in self.names.difference(&incoming) {
            env.remove_local(stale);
        }
        for binding in &snapshot.bindings {
            env.declare(binding.name.clone(), placeholder(binding), false);
        }
        self.names = incoming;
        if let Ok(mut cell) = cwd.lock() {
            *cell = snapshot.cwd.clone();
        }
    }
}

fn placeholder(binding: &ProtocolBinding) -> Value {
    if binding.callable {
        return Value::CmdRef(Arc::new(CmdCall {
            head: binding.name.clone(),
            forced: false,
            args: Vec::new(),
            redirects: Vec::new(),
            env_prefix: Vec::new(),
            background: false,
            trailing: None,
            span: Span::default(),
        }));
    }
    match binding.type_name.as_str() {
        "bool" => Value::Bool(false),
        "int" => Value::Int(0),
        "float" => Value::Float(0.0),
        "str" => Value::Str(String::new()),
        "path" => Value::Path(PathBuf::new()),
        "list" => Value::List(Vec::new()),
        "record" => Value::Record(Default::default()),
        "table" => Value::Table(Vec::new()),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot(bindings: Json) -> ProtocolSnapshot {
        ProtocolSnapshot::parse(json!({
            "cwd":{"display":"/work"},
            "bindings":bindings,
            "jobs":{"running":1,"suspended":0,"total":1},
            "reef":{"bindings":[]},
            "last_value":{"$":"null"}
        }))
        .unwrap()
    }

    #[test]
    fn mirror_refreshes_callable_classification_and_removes_stale_names() {
        let env = Env::root();
        let cwd = Arc::new(Mutex::new(PathBuf::new()));
        let mut mirror = RemoteEnvMirror::default();
        mirror.apply(
            &snapshot(json!([
                {"name":"deploy","callable":true,"type":"command"},
                {"name":"count","callable":false,"type":"int"}
            ])),
            &env,
            &cwd,
        );
        assert!(env.get("deploy").is_some_and(|value| value.is_callable()));
        assert!(matches!(env.get("count"), Some(Value::Int(0))));
        mirror.apply(&snapshot(json!([])), &env, &cwd);
        assert!(env.get("deploy").is_none());
        assert!(env.get("count").is_none());
        assert_eq!(*cwd.lock().unwrap(), PathBuf::from("/work"));
    }

    #[test]
    fn malformed_snapshot_is_a_result_not_a_panic() {
        assert!(ProtocolSnapshot::parse(json!({})).is_err());
    }
}
