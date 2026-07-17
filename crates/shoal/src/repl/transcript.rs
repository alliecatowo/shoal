//! Interactive journal wiring and the host-only `out[n]` to entry-ID mirror.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use shoal_ast::{CmdArg, Expr, Program, Stmt, UnOp};
use shoal_journal::{Journal, JournalQuery};

pub(super) const REPL_PRINCIPAL: &str = "human";
pub(super) const REPL_SESSION: &str = "default";

fn shoal_state_dir() -> PathBuf {
    shoal_paths::ShoalPaths::discover()
        .state_dir()
        .to_path_buf()
}

pub(super) fn effective_journal_state_dir(configured: Option<&Path>, cwd: &Path) -> PathBuf {
    match configured {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => cwd.join(path),
        None => shoal_state_dir(),
    }
}

pub(super) fn language_journal_requested(configured: bool, protocol_backed: bool) -> bool {
    configured && !protocol_backed
}

pub(super) fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

/// Newest entry for `principal` at or after `since_ns`, used to associate a
/// successful interactive value with the journal record just written.
pub(super) fn latest_entry_id(journal: &Journal, principal: &str, since_ns: i64) -> Option<i64> {
    let rows = journal
        .query(&JournalQuery {
            since_ts_ns: Some(since_ns),
            principal: Some(principal.to_string()),
            limit: 1,
            ..Default::default()
        })
        .ok()?;
    rows.first().map(|row| row.id)
}

/// Rewrite literal `undo out[N]` / `undo out[-N]` through the host's bounded
/// journal-ID mirror. Other shapes remain evaluator-owned and unchanged.
pub(super) fn resolve_out_undo(program: &mut Program, out_entries: &VecDeque<Option<i64>>) {
    for stmt in &mut program.stmts {
        let Stmt::Expr {
            expr: Expr::Cmd { call, .. },
            ..
        } = stmt
        else {
            continue;
        };
        if call.head != "undo" || call.args.len() != 1 {
            continue;
        }
        let Some(n) = out_index_literal(&call.args[0]) else {
            continue;
        };
        let idx = if n >= 0 {
            let Ok(index) = usize::try_from(n) else {
                continue;
            };
            index
        } else {
            let Some(distance) = n
                .checked_abs()
                .and_then(|distance| usize::try_from(distance).ok())
            else {
                continue;
            };
            let Some(idx) = out_entries.len().checked_sub(distance) else {
                continue;
            };
            idx
        };
        let Some(Some(entry_id)) = out_entries.get(idx) else {
            continue;
        };
        let span = call.args[0].span();
        call.args[0] = CmdArg::Expr {
            expr: Expr::Int {
                value: *entry_id,
                span,
            },
            span,
        };
    }
}

/// Keep the host mirror aligned with the evaluator's bounded `out` window.
pub(super) fn push_out_entry(out_entries: &mut VecDeque<Option<i64>>, entry_id: Option<i64>) {
    if out_entries.len() >= shoal_eval::MAX_REPL_TRANSCRIPT_VALUES {
        out_entries.pop_front();
    }
    out_entries.push_back(entry_id);
}

fn out_index_literal(arg: &CmdArg) -> Option<i64> {
    let CmdArg::Expr {
        expr: Expr::Index { recv, index, .. },
        ..
    } = arg
    else {
        return None;
    };
    let Expr::Var { name, .. } = recv.as_ref() else {
        return None;
    };
    if name != "out" {
        return None;
    }
    match index.as_ref() {
        Expr::Int { value, .. } => Some(*value),
        Expr::Unary {
            op: UnOp::Neg,
            expr,
            ..
        } => match expr.as_ref() {
            Expr::Int { value, .. } => value.checked_neg(),
            _ => None,
        },
        _ => None,
    }
}
