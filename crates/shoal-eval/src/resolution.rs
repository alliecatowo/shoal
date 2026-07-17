//! Typed command-head classification shared by runtime and planning.

use super::*;
use shoal_syntax::commands::{CommandFacts, CommandSource, resolve_command_source};

pub(crate) struct CommandResolution {
    pub(crate) source: CommandSource,
    pub(crate) binding: Option<Value>,
}

impl Evaluator {
    pub(crate) fn resolve_command(&self, call: &CmdCall) -> CommandResolution {
        self.resolve_head(
            &call.head,
            call.forced,
            call.args.is_empty() && call.redirects.is_empty() && call.env_prefix.is_empty(),
        )
    }

    pub(crate) fn resolve_head(
        &self,
        head: &str,
        forced: bool,
        value_eligible: bool,
    ) -> CommandResolution {
        let binding = self.exec.shell.env.get(head);
        let source = resolve_command_source(
            head,
            CommandFacts {
                session_callable: binding.as_ref().is_some_and(Value::is_callable),
                session_value: binding.as_ref().is_some_and(|value| !value.is_callable()),
                value_eligible,
                forced,
                dynamic_run: false,
                runner: false,
                adapter: self.host.adapters.lookup(head).is_some(),
            },
        );
        CommandResolution { source, binding }
    }

    pub(crate) fn resolve_dynamic_run(&self, head: &str, runner: bool) -> CommandResolution {
        let binding = self.exec.shell.env.get(head);
        let source = resolve_command_source(
            head,
            CommandFacts {
                session_callable: binding.as_ref().is_some_and(Value::is_callable),
                session_value: binding.as_ref().is_some_and(|value| !value.is_callable()),
                value_eligible: true,
                forced: false,
                dynamic_run: true,
                runner,
                adapter: self.host.adapters.lookup(head).is_some(),
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
