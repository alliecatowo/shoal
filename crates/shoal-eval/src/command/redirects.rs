//! Single-evaluation redirect preparation.
//!
//! Redirect targets are executable command arguments. Preparing them before
//! dispatch prevents a command from mutating state or spawning and only then
//! discovering that its target is invalid. The prepared paths are reused by
//! stdin setup and output commit, so dynamic targets execute exactly once.

use super::*;

#[derive(Debug)]
pub(crate) struct PreparedOutputRedirect {
    pub(crate) kind: RedirectKind,
    pub(crate) path: PathBuf,
}

#[derive(Debug, Default)]
pub(crate) struct PreparedRedirects {
    input: Option<PathBuf>,
    output: Option<PreparedOutputRedirect>,
}

impl PreparedRedirects {
    pub(crate) fn stdin_spec(&self) -> StdinSpec {
        self.input
            .as_ref()
            .map_or(StdinSpec::Null, |path| StdinSpec::File(path.clone()))
    }

    pub(crate) fn has_output(&self) -> bool {
        self.output.is_some()
    }

    pub(crate) fn output(&self) -> Option<&PreparedOutputRedirect> {
        self.output.as_ref()
    }
}

impl Evaluator {
    pub(crate) fn prepare_redirects(
        &mut self,
        call: &CmdCall,
        allow_input: bool,
    ) -> VResult<PreparedRedirects> {
        if !allow_input
            && let Some(redirect) = call
                .redirects
                .iter()
                .find(|redirect| redirect.kind == RedirectKind::In)
        {
            return Err(ErrorVal::new(
                "arg_error",
                format!("builtin `{}` does not consume redirected stdin", call.head),
            )
            .with_hint("use `.feed(command)` with a process command, or pass an explicit path")
            .with_span(redirect.span));
        }

        let mut prepared = PreparedRedirects::default();
        for redirect in &call.redirects {
            let path = self.arg_path(&redirect.target)?;
            match redirect.kind {
                RedirectKind::In => prepared.input = Some(path),
                RedirectKind::Out | RedirectKind::Append => {
                    prepared.output = Some(PreparedOutputRedirect {
                        kind: redirect.kind,
                        path,
                    });
                }
            }
        }
        Ok(prepared)
    }
}
