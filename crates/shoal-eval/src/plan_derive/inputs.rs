//! Recursive lowering of command-owned expression subtrees.
//!
//! Command arguments, flag values, environment prefixes, redirect targets,
//! and trailing blocks execute before or around dispatch. Treating them as
//! inert syntax lets nested effects bypass the plan even when the command's
//! own declared effect is correct.

use super::*;

impl Evaluator {
    pub(super) fn plan_command_inputs(
        &mut self,
        call: &CmdCall,
        functions: &Functions,
        aliases: &Aliases,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        for argument in &call.args {
            self.plan_cmd_arg(argument, functions, aliases, out, depth)?;
        }
        for prefix in &call.env_prefix {
            self.plan_cmd_arg(&prefix.value, functions, aliases, out, depth)?;
        }
        for redirect in &call.redirects {
            self.plan_cmd_arg(&redirect.target, functions, aliases, out, depth)?;
        }
        if let Some(block) = &call.trailing {
            self.plan_block(block, functions, aliases, out, depth)?;
        }
        Ok(())
    }

    fn plan_cmd_arg(
        &mut self,
        argument: &CmdArg,
        functions: &Functions,
        aliases: &Aliases,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        match argument {
            CmdArg::Str { expr, .. } | CmdArg::Expr { expr, .. } => {
                self.plan_expr(expr, functions, aliases, out, depth)
            }
            CmdArg::FlagLong {
                value: Some(value), ..
            } => self.plan_cmd_arg(value, functions, aliases, out, depth),
            CmdArg::Word { .. }
            | CmdArg::Path { .. }
            | CmdArg::Glob { .. }
            | CmdArg::FlagLong { value: None, .. }
            | CmdArg::FlagShort { .. }
            | CmdArg::DashDash { .. }
            | CmdArg::Dash { .. } => Ok(()),
        }
    }
}
