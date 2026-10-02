//! Static command-alias composition shared with effect planning.

use super::*;

/// Runtime `CmdRef` invocation appends caller arguments to the saved target.
/// Planning must preserve that shape so later flags cannot escape attribution.
pub(super) fn merged(target: &CmdCall, invocation: &CmdCall) -> CmdCall {
    let mut expanded = target.clone();
    expanded.args.extend(invocation.args.iter().cloned());
    expanded
}
