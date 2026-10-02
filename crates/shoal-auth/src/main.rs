use shoal_auth::TokenStore;

const HELP: &str = "Shoal capability tokens

Usage:
  shoal-token create PRINCIPAL [PROFILE] [--cap CAP] [--ttl SECONDS]
  shoal-token list
  shoal-token revoke ID

Commands:
  create  Create a token; print its secret exactly once
  list    List token metadata without secrets
  revoke  Prevent a token ID from authenticating new sessions

Options:
  --cap CAP      Add a capability; repeat to grant more than one
  --ttl SECONDS  Set the token expiry offset in seconds
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit

Output:
  create writes the secret to stdout and token ID to stderr; list writes tab-separated metadata.

Errors:
  Rejects unknown commands/options, missing values, invalid TTLs, and unknown revoke IDs.

Examples:
  shoal-token create automation restricted-agent --cap fs.read --ttl 3600
  shoal-token list
  shoal-token revoke TOKEN_ID

Exit status:
  0 on success; 1 when input, storage, encryption, or token lookup fails.";

const CREATE_HELP: &str = "Create a Shoal capability token

Usage:
  shoal-token create PRINCIPAL [PROFILE] [--cap CAP] [--ttl SECONDS]

Options:
  --cap CAP      Add a capability; repeat to grant more than one
  --ttl SECONDS  Set the token expiry offset in seconds
  -h, --help     Print this action help and exit

Output:
  Writes the new bearer secret to stdout exactly once and the token ID to stderr.

Errors:
  Rejects missing principals or option values, invalid TTLs, and token-store failures.

Examples:
  shoal-token create automation restricted-agent --cap fs.read --ttl 3600

Exit status:
  0 on success; 1 for invalid input, storage, or encryption failures.";

const LIST_HELP: &str = "List Shoal capability tokens

Usage:
  shoal-token list

Options:
  -h, --help  Print this action help and exit

Output:
  Writes tab-separated ID, principal, profile, and active/revoked state; never bearer secrets.

Errors:
  Reports token-store read or decryption failures without exposing secret values.

Examples:
  shoal-token list

Exit status:
  0 on success; 1 when the token store cannot be read.";

const REVOKE_HELP: &str = "Revoke a Shoal capability token

Usage:
  shoal-token revoke ID

Options:
  -h, --help  Print this action help and exit

Output:
  Silent on success.

Errors:
  Rejects a missing or unknown token ID and reports token-store write failures.

Examples:
  shoal-token revoke TOKEN_ID

Exit status:
  0 on success; 1 when the ID is unknown or the token store cannot be updated.";

const ACTIONS: &[&str] = &["create", "list", "revoke"];
const CREATE_OPTIONS: &[&str] = &["--cap", "--ttl"];

fn ttl_nanoseconds(value: &str) -> Result<i64, &'static str> {
    let seconds = value
        .parse::<i64>()
        .map_err(|_| "--ttl must be a positive integer number of seconds")?;
    if seconds <= 0 {
        return Err("--ttl must be a positive integer number of seconds");
    }
    seconds
        .checked_mul(1_000_000_000)
        .ok_or("--ttl seconds overflow the supported duration")
}

fn main() {
    if let Err(e) = run() {
        eprintln!("shoal-token: {e}");
        std::process::exit(1)
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::env::args().skip(1).collect::<Vec<_>>();
    if raw.as_slice() == ["-h"] || raw.as_slice() == ["--help"] {
        println!("{HELP}");
        return Ok(());
    }
    if raw.as_slice() == ["-V"] || raw.as_slice() == ["--version"] {
        println!("shoal-token {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let [command, flag] = raw.as_slice()
        && (flag == "-h" || flag == "--help")
    {
        let help = match command.as_str() {
            "create" => CREATE_HELP,
            "list" => LIST_HELP,
            "revoke" => REVOKE_HELP,
            _ => return Err(format!("unknown command {command}").into()),
        };
        println!("{help}");
        return Ok(());
    }
    let mut a = raw.into_iter();
    let cmd = a.next().ok_or("usage: shoal-token create|list|revoke")?;
    if !ACTIONS.contains(&cmd.as_str()) {
        return Err(format!("unknown command {cmd}").into());
    }
    let paths = shoal_paths::ShoalPaths::discover();
    let path = paths.token_store(paths.state_dir());
    match cmd.as_str() {
        "create" => {
            let principal = a.next().ok_or("create PRINCIPAL [PROFILE]")?;
            if principal.is_empty() {
                return Err("create PRINCIPAL must not be empty".into());
            }
            let rest: Vec<String> = a.collect();
            let mut i = 0;
            let mut profile = "default".to_string();
            if rest.first().is_some_and(|v| !v.starts_with("--")) {
                profile = rest[0].clone();
                if profile.is_empty() {
                    return Err("create PROFILE must not be empty".into());
                }
                i = 1;
            }
            let mut caps = Vec::new();
            let mut ttl = None;
            let mut saw_ttl = false;
            while i < rest.len() {
                if !CREATE_OPTIONS.contains(&rest[i].as_str()) {
                    return Err(format!("unknown create option {}", rest[i]).into());
                }
                match rest[i].as_str() {
                    "--cap" => {
                        i += 1;
                        let capability = rest.get(i).ok_or("--cap requires value")?.clone();
                        if capability.is_empty() {
                            return Err("--cap value must not be empty".into());
                        }
                        caps.push(capability)
                    }
                    "--ttl" => {
                        if saw_ttl {
                            return Err("--ttl may be specified only once".into());
                        }
                        saw_ttl = true;
                        i += 1;
                        ttl = Some(ttl_nanoseconds(
                            rest.get(i).ok_or("--ttl requires seconds")?,
                        )?)
                    }
                    x => return Err(format!("unknown create option {x}").into()),
                }
                i += 1;
            }
            let mut s = TokenStore::open(path)?;
            let (secret, m) = s.create(principal, profile, caps, ttl)?;
            println!("{secret}");
            eprintln!("created {} (secret shown once)", m.id)
        }
        "list" => {
            if let Some(extra) = a.next() {
                return Err(format!("list does not accept argument {extra}").into());
            }
            let s = TokenStore::open(path)?;
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
            if let Some(extra) = a.next() {
                return Err(format!("revoke does not accept argument {extra}").into());
            }
            let mut s = TokenStore::open(path)?;
            if !s.revoke(&id)? {
                return Err("unknown token id".into());
            }
        }
        _ => return Err("usage: shoal-token create|list|revoke".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ACTIONS, CREATE_HELP, CREATE_OPTIONS, HELP, ttl_nanoseconds};

    #[test]
    fn ttl_is_positive_and_bounded() {
        assert_eq!(ttl_nanoseconds("1"), Ok(1_000_000_000));
        assert!(ttl_nanoseconds("0").is_err());
        assert!(ttl_nanoseconds("-1").is_err());
        assert!(ttl_nanoseconds("not-a-number").is_err());
        assert!(ttl_nanoseconds(&i64::MAX.to_string()).is_err());
    }

    #[test]
    fn action_and_option_registries_match_help_and_man() {
        let man = include_str!("../../../man/shoal-token.1");
        for action in ACTIONS {
            assert!(HELP.contains(action), "root help omitted {action}");
            assert!(man.contains(action), "man page omitted {action}");
        }
        for option in CREATE_OPTIONS {
            assert!(HELP.contains(option), "root help omitted {option}");
            assert!(CREATE_HELP.contains(option), "action help omitted {option}");
            assert!(
                man.contains(&option.replace("--", "\\-\\-")),
                "man page omitted {option}"
            );
        }
    }
}
