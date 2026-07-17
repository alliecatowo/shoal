use shoal_auth::TokenStore;
use std::path::PathBuf;
fn main() {
    if let Err(e) = run() {
        eprintln!("shoal-token: {e}");
        std::process::exit(1)
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let cmd = a.next().ok_or("usage: shoal-token create|list|revoke")?;
    let path = std::env::var_os("SHOAL_TOKEN_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
                .unwrap_or_else(|| PathBuf::from("."))
                .join("shoal/tokens.json")
        });
    let mut s = TokenStore::open(path)?;
    match cmd.as_str() {
        "create" => {
            let principal = a.next().ok_or("create PRINCIPAL [PROFILE]")?;
            let rest: Vec<String> = a.collect();
            let mut i = 0;
            let mut profile = "default".to_string();
            if rest.first().is_some_and(|v| !v.starts_with("--")) {
                profile = rest[0].clone();
                i = 1;
            }
            let mut caps = Vec::new();
            let mut ttl = None;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--cap" => {
                        i += 1;
                        caps.push(rest.get(i).ok_or("--cap requires value")?.clone())
                    }
                    "--ttl" => {
                        i += 1;
                        let secs: i64 = rest.get(i).ok_or("--ttl requires seconds")?.parse()?;
                        ttl = Some(secs.saturating_mul(1_000_000_000))
                    }
                    x => return Err(format!("unknown create option {x}").into()),
                }
                i += 1;
            }
            let (secret, m) = s.create(principal, profile, caps, ttl)?;
            println!("{secret}");
            eprintln!("created {} (secret shown once)", m.id)
        }
        "list" => {
            for m in s.try_list()? {
                println!(
                    "{}\t{}\t{}\t{}",
                    m.id,
                    m.principal,
                    m.profile,
                    if m.revoked_ns.is_some() {
                        "revoked"
                    } else {
                        "active"
                    }
                )
            }
        }
        "revoke" => {
            let id = a.next().ok_or("revoke ID")?;
            if !s.revoke(&id)? {
                return Err("unknown token id".into());
            }
        }
        _ => return Err("usage: shoal-token create|list|revoke".into()),
    }
    Ok(())
}
