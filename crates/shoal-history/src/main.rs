use shoal_history::{QueryFilter, entry, entry_json, gc, query, render_human, undo};
use shoal_journal::{EntryKind, Journal};
use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "Shoal journal history

Usage:
  shoal-history [--state-dir PATH] [--json] COMMAND [OPTIONS]

Commands:
  query   Filter journal entries (the default command)
  show    Show one entry by numeric ID
  pin     Retain a CAS object by hash
  unpin   Release a retained CAS object
  gc      Preview or apply storage collection
  status  Show database and CAS use
  undo    Apply an entry's inverse operations below an explicit root

Options:
  --state-dir PATH  Override layered journal.state_dir and the XDG state root
  --json            Emit structured JSON where supported
  query: --since NS --principal NAME --kind KIND --effects EFFECT --head HEAD
         --status ok|failed --limit N
  gc:    --ttl SECONDS --budget BYTES --apply
  undo:  --root PATH
  -h, --help        Print this help and exit
  -V, --version     Print the version and exit

Output:
  Human history/status text by default; --json emits stable structured records.

Errors:
  Rejects malformed filters, missing IDs/hashes/roots, unknown commands, and journal failures.

Examples:
  shoal-history query --status failed --limit 20
  shoal-history --json status
  shoal-history gc --ttl 604800 --apply

Exit status:
  0 on success; 1 for missing records or operational failures; 2 for invalid arguments.";

const ACTIONS: &[&str] = &["query", "show", "pin", "unpin", "gc", "status", "undo"];
const GLOBAL_OPTIONS: &[&str] = &["--state-dir", "--json"];
const QUERY_OPTIONS: &[&str] = &[
    "--since",
    "--principal",
    "--kind",
    "--effects",
    "--head",
    "--status",
    "--limit",
];
const GC_OPTIONS: &[&str] = &["--ttl", "--budget", "--apply"];
const UNDO_OPTIONS: &[&str] = &["--root"];

fn action_help(command: &str) -> Option<&'static str> {
    match command {
        "query" => Some(
            "Query Shoal journal entries

Usage:
  shoal-history query [FILTERS]

Options:
  --since NS --principal NAME --kind KIND --effects EFFECT --head HEAD
  --status ok|failed --limit N
  -h, --help  Print this action help and exit

Output:
  Matching human records, or structured records when global --json is present.

Errors:
  Rejects missing or malformed filter values and reports journal read failures.

Examples:
  shoal-history query --status failed --limit 20

Exit status:
  0 on success; 1 for journal failures; 2 for invalid filters.",
        ),
        "show" => Some(
            "Show one Shoal journal entry

Usage:
  shoal-history show ID

Options:
  -h, --help  Print this action help and exit

Output:
  The complete human entry, or a structured record when global --json is present.

Errors:
  Rejects malformed IDs and reports missing entries or journal failures.

Examples:
  shoal-history --json show 42

Exit status:
  0 on success; 1 when the entry is missing or unreadable; 2 for an invalid ID.",
        ),
        "pin" => Some(
            "Pin a Shoal content-addressed object

Usage:
  shoal-history pin HASH

Options:
  -h, --help  Print this action help and exit

Output:
  Silent on success.

Errors:
  Rejects a missing hash and reports journal or CAS update failures.

Examples:
  shoal-history pin BLOB_HASH

Exit status:
  0 on success; 1 for storage failures; 2 when HASH is missing.",
        ),
        "unpin" => Some(
            "Unpin a Shoal content-addressed object

Usage:
  shoal-history unpin HASH

Options:
  -h, --help  Print this action help and exit

Output:
  Silent on success.

Errors:
  Rejects a missing hash and reports journal or CAS update failures.

Examples:
  shoal-history unpin BLOB_HASH

Exit status:
  0 on success; 1 for storage failures; 2 when HASH is missing.",
        ),
        "gc" => Some(
            "Collect Shoal journal storage

Usage:
  shoal-history gc [--ttl SECONDS] [--budget BYTES] [--apply]

Options:
  --ttl SECONDS  Select objects older than the duration
  --budget BYTES Select enough objects to satisfy the storage budget
  --apply        Delete candidates; omission is a dry run
  -h, --help     Print this action help and exit

Output:
  A JSON summary with dry-run, candidate, deletion, and byte counts.

Errors:
  Rejects malformed numbers and reports journal or CAS collection failures.

Examples:
  shoal-history gc --ttl 604800 --apply

Exit status:
  0 on success; 1 for storage failures; 2 for invalid options.",
        ),
        "status" => Some(
            "Show Shoal journal storage use

Usage:
  shoal-history status

Options:
  -h, --help  Print this action help and exit

Output:
  Database, WAL, CAS, spill, pin, and admission totals in human or global --json form.

Errors:
  Reports journal metadata and filesystem accounting failures.

Examples:
  shoal-history --json status

Exit status:
  0 on success; 1 when storage status cannot be read.",
        ),
        "undo" => Some(
            "Apply a Shoal journal entry's inverse operations

Usage:
  shoal-history undo ID --root PATH

Options:
  --root PATH   Require inverse effects to remain below PATH
  -h, --help    Print this action help and exit

Output:
  Applied step count in human form, or step statuses when global --json is present.

Errors:
  Rejects malformed IDs or missing roots and reports containment or inverse-operation failures.

Examples:
  shoal-history undo 42 --root ./workspace

Exit status:
  0 on success; 1 when undo fails; 2 for an invalid ID or missing root.",
        ),
        _ => None,
    }
}

fn main() {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => {}
        Err((code, msg)) => {
            eprintln!("shoal-history: {msg}");
            std::process::exit(code)
        }
    }
}
fn run(mut args: Vec<String>) -> Result<(), (i32, String)> {
    if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
        println!("{HELP}");
        return Ok(());
    }
    if args.as_slice() == ["-V"] || args.as_slice() == ["--version"] {
        println!("shoal-history {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut state_override = None;
    let mut json = false;
    let mut seen_global = std::collections::BTreeSet::new();
    while let Some(option) = args.first().cloned() {
        if !GLOBAL_OPTIONS.contains(&option.as_str()) {
            break;
        }
        match option.as_str() {
            "--state-dir" => {
                if !seen_global.insert(option.clone()) {
                    return Err((2, format!("{option} may be specified only once")));
                }
                if args.len() < 2 {
                    return Err((2, "--state-dir requires PATH".into()));
                }
                state_override = Some(PathBuf::from(args.remove(1)));
                args.remove(0);
            }
            "--json" => {
                if !seen_global.insert(option.clone()) {
                    return Err((2, format!("{option} may be specified only once")));
                }
                json = true;
                args.remove(0);
            }
            _ => unreachable!("global registry and parser match arms must remain in parity"),
        }
    }
    if let [command, flag] = args.as_slice()
        && (flag == "-h" || flag == "--help")
    {
        let help = action_help(command).ok_or((2, format!("unknown command {command}")))?;
        println!("{help}");
        return Ok(());
    }
    if json
        && matches!(
            args.first().map(String::as_str).unwrap_or("query"),
            "pin" | "unpin" | "gc"
        )
    {
        return Err((
            2,
            "--json is supported by query, show, status, and undo only".into(),
        ));
    }
    validate_action_shape(&args)?;
    let state = match state_override {
        Some(state) => state,
        None => {
            let cwd = std::env::current_dir().map_err(op)?;
            let fallback = shoal_paths::ShoalPaths::discover()
                .state_dir()
                .to_path_buf();
            configured_state_dir(&cwd, fallback, shoal_config::LoadOptions::discover(&cwd))?
        }
    };
    let command = args.first().map(String::as_str).unwrap_or("query");
    let journal = Journal::open(&state).map_err(op)?;
    match command {
        "query" => {
            let f = parse_query(args.get(1..).unwrap_or(&[]))?;
            let rows = query(&journal, &f).map_err(op)?;
            if json {
                let value = rows
                    .iter()
                    .map(|row| entry_json(&journal, row))
                    .collect::<Vec<_>>();
                println!("{}", serde_json::to_string_pretty(&value).map_err(op)?)
            } else {
                print!("{}", render_human(&journal, &rows, false))
            }
        }
        "show" => {
            let id = parse_id(args.get(1))?;
            let row = entry(&journal, id)
                .map_err(op)?
                .ok_or((1, format!("entry {id} not found")))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&entry_json(&journal, &row)).map_err(op)?
                )
            } else {
                print!("{}", render_human(&journal, &[row], true))
            }
        }
        "pin" => {
            let h = args.get(1).ok_or((2, "pin requires HASH".into()))?;
            journal.pin(h).map_err(op)?;
        }
        "unpin" => {
            let h = args.get(1).ok_or((2, "unpin requires HASH".into()))?;
            journal.unpin(h).map_err(op)?;
        }
        "gc" => {
            let options = parse_gc(&args[1..])?;
            let r = gc(&journal, options.ttl, options.budget, options.apply).map_err(op)?;
            println!(
                "{}",
                serde_json::json!({"dry_run":!options.apply,"candidates":r.candidates.len(),"deleted":r.deleted.len(),"reclaimed_bytes":r.reclaimed_bytes,"remaining_bytes":r.remaining_bytes})
            )
        }
        "status" => {
            let status = journal.storage_status().map_err(op)?;
            let value = serde_json::json!({
                "database_bytes": status.database_bytes,
                "wal_bytes": status.wal_bytes,
                "shm_bytes": status.shm_bytes,
                "database_admission_bytes": status.database_admission_bytes(),
                "database_max_bytes": status.database_max_bytes,
                "cas_logical_bytes": status.cas_logical_bytes,
                "cas_physical_bytes": status.cas_physical_bytes,
                "spill_physical_bytes": status.spill_physical_bytes,
                "cas_admission_bytes": status.cas_admission_bytes(),
                "cas_max_bytes": status.cas_max_bytes,
                "pinned_logical_bytes": status.pinned_logical_bytes,
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&value).map_err(op)?);
            } else {
                println!(
                    "database: {} / {} bytes (main {}, WAL {}, SHM {})\nCAS: {} / {} bytes (logical {}, physical {}, spill {}, pinned {})",
                    status.database_admission_bytes(),
                    status.database_max_bytes,
                    status.database_bytes,
                    status.wal_bytes,
                    status.shm_bytes,
                    status.cas_admission_bytes(),
                    status.cas_max_bytes,
                    status.cas_logical_bytes,
                    status.cas_physical_bytes,
                    status.spill_physical_bytes,
                    status.pinned_logical_bytes,
                );
            }
        }
        "undo" => {
            let id = parse_id(args.get(1))?;
            let root = PathBuf::from(&args[3]);
            let r = undo(&journal, id, &root).map_err(|e| (1, e.to_string()))?;
            if json {
                let steps = r
                    .steps
                    .iter()
                    .map(|step| {
                        serde_json::to_value(&step.inverse).map(|inverse| {
                            serde_json::json!({
                                "status": format!("{:?}", step.status).to_lowercase(),
                                "inverse": inverse,
                            })
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(op)?;
                println!(
                    "{}",
                    serde_json::json!({"entry_id":r.entry_id,"steps":steps})
                );
            } else {
                println!("undid entry {} ({} steps)", r.entry_id, r.steps.len())
            }
        }
        _ => return Err((2, format!("unknown command {command}"))),
    }
    Ok(())
}

fn validate_action_shape(args: &[String]) -> Result<(), (i32, String)> {
    let command = args.first().map(String::as_str).unwrap_or("query");
    if !ACTIONS.contains(&command) {
        return Err((2, format!("unknown command {command}")));
    }
    match command {
        "query" => parse_query(args.get(1..).unwrap_or(&[])).map(|_| ()),
        "show" => exact_arity(args, 2, "show ID").and_then(|()| parse_id(args.get(1)).map(|_| ())),
        "pin" => exact_arity(args, 2, "pin HASH"),
        "unpin" => exact_arity(args, 2, "unpin HASH"),
        "gc" => parse_gc(&args[1..]).map(|_| ()),
        "status" => exact_arity(args, 1, "status"),
        "undo" => {
            exact_arity(args, 4, "undo ID --root PATH")?;
            parse_id(args.get(1))?;
            if !UNDO_OPTIONS.contains(&args[2].as_str()) {
                return Err((2, "undo requires `undo ID --root PATH`".into()));
            }
            Ok(())
        }
        _ => Err((2, format!("unknown command {command}"))),
    }
}

fn exact_arity(args: &[String], expected: usize, usage: &str) -> Result<(), (i32, String)> {
    if args.len() == expected {
        Ok(())
    } else {
        Err((2, format!("usage: shoal-history {usage}")))
    }
}

struct GcArgs {
    ttl: Option<Duration>,
    budget: Option<u64>,
    apply: bool,
}

fn parse_gc(args: &[String]) -> Result<GcArgs, (i32, String)> {
    let mut parsed = GcArgs {
        ttl: None,
        budget: None,
        apply: false,
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut i = 0;
    while i < args.len() {
        let option = args[i].as_str();
        if !GC_OPTIONS.contains(&option) {
            return Err((2, format!("unknown gc option {option}")));
        }
        if !seen.insert(option) {
            return Err((2, format!("{option} may be specified only once")));
        }
        match option {
            "--ttl" => {
                parsed.ttl = Some(Duration::from_secs(parse_u64(args.get(i + 1), "ttl")?));
                i += 2
            }
            "--budget" => {
                parsed.budget = Some(parse_u64(args.get(i + 1), "budget")?);
                i += 2
            }
            "--apply" => {
                parsed.apply = true;
                i += 1
            }
            x => return Err((2, format!("unknown gc option {x}"))),
        }
    }
    Ok(parsed)
}

fn configured_state_dir(
    cwd: &std::path::Path,
    fallback: PathBuf,
    options: shoal_config::LoadOptions,
) -> Result<PathBuf, (i32, String)> {
    let loaded =
        shoal_config::load(&options).map_err(|error| (1, format!("configuration: {error}")))?;
    Ok(match loaded.config.journal.state_dir {
        Some(path) if path.is_absolute() => path,
        Some(path) => cwd.join(path),
        None => fallback,
    })
}
fn parse_query(args: &[String]) -> Result<QueryFilter, (i32, String)> {
    let mut f = QueryFilter::default();
    let mut seen = std::collections::BTreeSet::new();
    let mut i = 0;
    while i < args.len() {
        if !QUERY_OPTIONS.contains(&args[i].as_str()) {
            return Err((2, format!("unknown query option {}", args[i])));
        }
        if !seen.insert(args[i].as_str()) {
            return Err((2, format!("{} may be specified only once", args[i])));
        }
        match args[i].as_str() {
            "--since" => {
                f.since_ns = Some(parse_i64(args.get(i + 1), "since")?);
                i += 2
            }
            "--principal" => {
                f.principal = Some(value(args.get(i + 1), "principal")?);
                i += 2
            }
            "--kind" => {
                f.kind = Some(
                    value(args.get(i + 1), "kind")?
                        .parse::<EntryKind>()
                        .map_err(|error| (2, error))?,
                );
                i += 2
            }
            "--effects" => {
                f.effect = Some(value(args.get(i + 1), "effects")?);
                i += 2
            }
            "--head" => {
                f.head = Some(value(args.get(i + 1), "head")?);
                i += 2
            }
            "--status" => {
                f.ok = Some(match value(args.get(i + 1), "status")?.as_str() {
                    "ok" => true,
                    "failed" => false,
                    _ => return Err((2, "status must be ok|failed".into())),
                });
                i += 2
            }
            "--limit" => {
                f.limit = parse_u64(args.get(i + 1), "limit")? as usize;
                i += 2
            }
            x => return Err((2, format!("unknown query option {x}"))),
        }
    }
    Ok(f)
}
fn value(v: Option<&String>, n: &str) -> Result<String, (i32, String)> {
    v.cloned().ok_or((2, format!("--{n} requires value")))
}
fn parse_u64(v: Option<&String>, n: &str) -> Result<u64, (i32, String)> {
    value(v, n)?
        .parse()
        .map_err(|_| (2, format!("invalid {n}")))
}
fn parse_i64(v: Option<&String>, n: &str) -> Result<i64, (i32, String)> {
    value(v, n)?
        .parse()
        .map_err(|_| (2, format!("invalid {n}")))
}
fn parse_id(v: Option<&String>) -> Result<i64, (i32, String)> {
    parse_i64(v, "entry id")
}
fn op(e: impl std::fmt::Display) -> (i32, String) {
    (1, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_and_option_registries_match_help_and_man() {
        let man = include_str!("../../../man/shoal-history.1");
        for action in ACTIONS {
            assert!(HELP.contains(action), "root help omitted {action}");
            assert!(
                action_help(action).is_some(),
                "action help omitted {action}"
            );
            assert!(man.contains(action), "man page omitted {action}");
        }
        for option in GLOBAL_OPTIONS {
            assert!(HELP.contains(option), "root help omitted {option}");
            assert!(
                man.contains(&option.replace("--", "\\-\\-")),
                "man page omitted {option}"
            );
        }
        for (action, options) in [
            ("query", QUERY_OPTIONS),
            ("gc", GC_OPTIONS),
            ("undo", UNDO_OPTIONS),
        ] {
            let help = action_help(action).unwrap();
            for option in options {
                assert!(help.contains(option), "{action} help omitted {option}");
                assert!(
                    man.contains(&option.replace("--", "\\-\\-")),
                    "man page omitted {action} {option}"
                );
            }
        }
    }

    #[test]
    fn action_shapes_reject_trailing_and_repeated_arguments() {
        for invalid in [
            vec!["show", "1", "extra"],
            vec!["pin", "hash", "extra"],
            vec!["unpin", "hash", "extra"],
            vec!["status", "extra"],
            vec!["undo", "1", "--root", ".", "extra"],
            vec!["gc", "--apply", "--apply"],
            vec!["query", "--limit", "1", "--limit", "2"],
        ] {
            let owned = invalid.into_iter().map(String::from).collect::<Vec<_>>();
            assert!(
                validate_action_shape(&owned).is_err(),
                "accepted invalid invocation: {owned:?}"
            );
        }
        assert!(validate_action_shape(&[]).is_ok());
        assert!(validate_action_shape(&["status".into()]).is_ok());
        assert!(
            validate_action_shape(&["undo".into(), "1".into(), "--root".into(), ".".into()])
                .is_ok()
        );
    }

    #[test]
    fn production_cli_has_no_json_serialization_panics() {
        let source = include_str!("main.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in ["serde_json::to_string_pretty", "serde_json::to_value"] {
            for line in production.lines().filter(|line| line.contains(forbidden)) {
                assert!(
                    !line.contains("unwrap") && !line.contains("expect"),
                    "{line}"
                );
            }
        }
        assert!(!production.contains("serializable inverse"));
    }

    #[test]
    fn layered_journal_state_dir_resolves_from_startup_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(".shoal.toml");
        std::fs::write(&config, "[journal]\nstate_dir = 'relative-state'\n").unwrap();
        let options = shoal_config::LoadOptions {
            system: None,
            user: None,
            project: Some(config),
            env: vec![],
        };
        assert_eq!(
            configured_state_dir(dir.path(), dir.path().join("fallback"), options).unwrap(),
            dir.path().join("relative-state")
        );
    }

    #[test]
    fn invalid_layered_config_fails_instead_of_opening_fallback_journal() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(".shoal.toml");
        std::fs::write(&config, "[journal]\nstate_dir = 5\n").unwrap();
        let options = shoal_config::LoadOptions {
            system: None,
            user: None,
            project: Some(config),
            env: vec![],
        };
        let error =
            configured_state_dir(dir.path(), dir.path().join("fallback"), options).unwrap_err();
        assert_eq!(error.0, 1);
        assert!(error.1.contains("configuration:"));
        assert!(error.1.contains("journal.state_dir"));
    }
}
