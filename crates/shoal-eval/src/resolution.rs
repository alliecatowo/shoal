//! Typed command-head classification shared by runtime and planning.

use super::*;
use shoal_syntax::commands::{CommandFacts, CommandSource, resolve_command_source};

pub(crate) struct CommandResolution {
    pub(crate) source: CommandSource,
    pub(crate) binding: Option<Value>,
}

impl Evaluator {
    pub(crate) fn resolve_command(&self, call: &CmdCall) -> CommandResolution {
        let binding = self.exec.shell.env.get(&call.head);
        let source = resolve_command_source(
            &call.head,
            CommandFacts {
                session_callable: binding.as_ref().is_some_and(Value::is_callable),
                session_value: binding.as_ref().is_some_and(|value| !value.is_callable()),
                value_eligible: call.args.is_empty()
                    && call.redirects.is_empty()
                    && call.env_prefix.is_empty(),
                forced: call.forced,
                adapter: self.host.adapters.lookup(&call.head).is_some(),
            },
        );
        CommandResolution { source, binding }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(head: &str) -> CmdCall {
        CmdCall {
            head: head.into(),
            args: Vec::new(),
            redirects: Vec::new(),
            env_prefix: Vec::new(),
            background: false,
            trailing: None,
            forced: false,
            span: Span::default(),
        }
    }

    #[test]
    fn bare_bound_value_does_not_fall_through_to_spawn() {
        let mut evaluator = Evaluator::new(PathBuf::from("/"));
        evaluator.env_mut().declare("shadow", Value::Int(42), false);
        assert_eq!(
            evaluator.resolve_command(&call("shadow")).source,
            CommandSource::BoundValue
        );
    }
}
