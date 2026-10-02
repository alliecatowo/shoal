const HELP: &str = "Shoal MCP server

Usage:
  shoal-mcp [OPTIONS]

Options:
  --socket PATH   Connect to an explicit kernel Unix socket
  --session NAME  Select the session used for socket discovery
  --token TOKEN   Authenticate as the capability-token principal
  -h, --help      Print this help and exit
  -V, --version   Print the version and exit

Output:
  Reads and writes Model Context Protocol messages over stdin and stdout.

Errors:
  Reports invalid arguments, connection, protocol, and kernel failures.

Examples:
  SHOAL_TOKEN=secret shoal-mcp --session automation
  shoal-mcp --socket /run/user/1000/shoal/default.sock

Exit status:
  0 after an orderly protocol shutdown; 1 for runtime failures; 2 for invalid arguments.";

const PARSER_OPTIONS: &[(&str, bool)] =
    &[("--socket", true), ("--session", true), ("--token", true)];

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
        println!("{HELP}");
        return;
    }
    if args.as_slice() == ["-V"] || args.as_slice() == ["--version"] {
        println!("shoal-mcp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|argument| argument == "--local-human") {
        retired_local_human();
    }
    let mut config = shoal_mcp::Config {
        socket: std::env::var_os("SHOAL_SOCKET")
            .map(Into::into)
            .unwrap_or_default(),
        session: std::env::var("SHOAL_SESSION").ok(),
        token: std::env::var("SHOAL_TOKEN").ok(),
    };
    if let Err(error) = apply_args(&mut config, args) {
        eprintln!("shoal-mcp: {error}");
        usage();
    }
    if config.socket.as_os_str().is_empty() {
        let session = config.session.as_deref().unwrap_or("default");
        config.socket = shoal_mcp::discover_socket(session);
    }
    if let Err(error) = shoal_mcp::run_stdio(&config) {
        eprintln!("shoal-mcp: {error}");
        std::process::exit(1);
    }
}

fn apply_args(config: &mut shoal_mcp::Config, args: Vec<String>) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        let Some((_, takes_value)) = PARSER_OPTIONS.iter().find(|(name, _)| *name == argument)
        else {
            return Err(format!("unknown argument {argument}"));
        };
        if !seen.insert(argument.clone()) {
            return Err(format!("{argument} may be specified only once"));
        }
        let value = if *takes_value {
            Some(
                args.next()
                    .ok_or_else(|| format!("{argument} requires a value"))?,
            )
        } else {
            None
        };
        if value.as_ref().is_some_and(String::is_empty) {
            return Err(format!("{argument} requires a non-empty value"));
        }
        match argument.as_str() {
            "--socket" => config.socket = value.expect("registry requires value").into(),
            "--session" => config.session = value,
            "--token" => config.token = value,
            _ => unreachable!("registry and parser match arms must remain in parity"),
        }
    }
    Ok(())
}
fn usage() -> ! {
    eprintln!("usage: shoal-mcp [--socket PATH] [--session NAME] [--token TOKEN]");
    std::process::exit(2)
}

fn retired_local_human() -> ! {
    eprintln!(
        "shoal-mcp: --local-human was removed because named/public sockets cannot prove human \
         presence; use a policy-scoped bearer token, or use the private interactive Shoal REPL"
    );
    std::process::exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> shoal_mcp::Config {
        shoal_mcp::Config {
            socket: Default::default(),
            session: None,
            token: None,
        }
    }

    #[test]
    fn parser_help_and_man_share_the_option_registry() {
        let documented = HELP
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("--"))
            .filter_map(|line| line.split_whitespace().next())
            .collect::<std::collections::BTreeSet<_>>();
        let registered = PARSER_OPTIONS
            .iter()
            .map(|(name, _)| *name)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(documented, registered);
        let man = include_str!("../../../man/shoal-mcp.1");
        for (name, _) in PARSER_OPTIONS {
            assert!(man.contains(&name.replace("--", "\\-\\-")), "{name}");
            let mut parsed = config();
            apply_args(&mut parsed, vec![(*name).into(), "value".into()]).unwrap();
        }
        assert!(!HELP.contains("--local-human"));
        assert!(!registered.contains("--local-human"));
    }

    #[test]
    fn duplicate_and_mixed_control_options_are_rejected() {
        let mut parsed = config();
        let error = apply_args(
            &mut parsed,
            vec![
                "--session".into(),
                "a".into(),
                "--session".into(),
                "b".into(),
            ],
        )
        .unwrap_err();
        assert!(error.contains("only once"));
        let error = apply_args(&mut parsed, vec!["--help".into()]).unwrap_err();
        assert!(error.contains("unknown"));
    }
}
