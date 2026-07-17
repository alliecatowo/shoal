use shoal_history::{QueryFilter, entry, entry_json, gc, query, render_human, undo};
use shoal_journal::Journal;
use std::path::PathBuf;
use std::time::Duration;

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
        println!(
            "Shoal journal history\n\nUsage: shoal-history [--state-dir PATH] [--json] COMMAND [OPTIONS]\n\nCommands:\n  query   Filter journal entries\n  show    Show one entry\n  pin     Retain a CAS object\n  unpin   Release a CAS object\n  gc      Collect journal storage\n  status  Show storage use\n  undo    Apply an entry's inverse operations"
        );
        return Ok(());
    }
    if args.as_slice() == ["-V"] || args.as_slice() == ["--version"] {
        println!("shoal-history {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut state = shoal_paths::ShoalPaths::discover()
        .state_dir()
        .to_path_buf();
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                if i + 1 >= args.len() {
                    return Err((2, "--state-dir requires PATH".into()));
                }
                state = PathBuf::from(args.remove(i + 1));
                args.remove(i);
            }
            "--json" => {
                json = true;
                args.remove(i);
            }
            _ => i += 1,
        }
    }
    let command = args.first().map(String::as_str).unwrap_or("query");
    let journal = Journal::open(&state).map_err(op)?;
    match command {
        "query" => {
            let f = parse_query(&args[1..])?;
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
            let mut ttl = None;
            let mut budget = None;
            let mut apply = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--ttl" => {
                        ttl = Some(Duration::from_secs(parse_u64(args.get(i + 1), "ttl")?));
                        i += 2
                    }
                    "--budget" => {
                        budget = Some(parse_u64(args.get(i + 1), "budget")?);
                        i += 2
                    }
                    "--apply" => {
                        apply = true;
                        i += 1
                    }
                    x => return Err((2, format!("unknown gc option {x}"))),
                }
            }
            let r = gc(&journal, ttl, budget, apply).map_err(op)?;
            println!(
                "{}",
                serde_json::json!({"dry_run":!apply,"candidates":r.candidates.len(),"deleted":r.deleted.len(),"reclaimed_bytes":r.reclaimed_bytes,"remaining_bytes":r.remaining_bytes})
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
            let root = args
                .windows(2)
                .find(|w| w[0] == "--root")
                .map(|w| PathBuf::from(&w[1]))
                .ok_or((2, "undo requires --root PATH".into()))?;
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
fn parse_query(args: &[String]) -> Result<QueryFilter, (i32, String)> {
    let mut f = QueryFilter::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--since" => {
                f.since_ns = Some(parse_i64(args.get(i + 1), "since")?);
                i += 2
            }
            "--principal" => {
                f.principal = Some(value(args.get(i + 1), "principal")?);
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
}
