use std::os::unix::process::CommandExt;
use std::path::PathBuf;

const HELP: &str = "Run a command with Shoal child-only OS controls

Usage:
  shoal-sandbox-exec [OPTIONS] -- COMMAND [ARG...]

Options:
  --deny-net          Deny network access
  --cpu-seconds N     Limit CPU time to a positive number of seconds
  --memory-bytes N    Limit address space to a positive byte count
  --read PATH         Permit reads below PATH; repeatable
  --write PATH        Permit writes below PATH; repeatable
  --delete PATH       Permit deletion below PATH; repeatable
  -h, --help          Print this help and exit
  -V, --version       Print the version and exit

Output:
  On success this process is replaced by COMMAND; its standard streams are unchanged.

Errors:
  Refuses missing commands, malformed limits, unknown options, or unavailable enforcement.

Examples:
  shoal-sandbox-exec --deny-net --read . -- cargo test

Exit status:
  COMMAND determines the status after exec; setup and exec failures return 126.";

/// (name, takes value, repeatable)
const PARSER_OPTIONS: &[(&str, bool, bool)] = &[
    ("--deny-net", false, false),
    ("--cpu-seconds", true, false),
    ("--memory-bytes", true, false),
    ("--read", true, true),
    ("--write", true, true),
    ("--delete", true, true),
];

fn main() {
    let raw = std::env::args_os().skip(1).collect::<Vec<_>>();
    if raw.as_slice() == ["-h"] || raw.as_slice() == ["--help"] {
        println!("{HELP}");
        return;
    }
    if raw.as_slice() == ["-V"] || raw.as_slice() == ["--version"] {
        println!("shoal-sandbox-exec {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let mut a = raw.into_iter();
    let mut s = shoal_leash::FsSandbox::default();
    let mut net = shoal_leash::NetPolicy::Unrestricted;
    let mut limits = shoal_leash::ProcessLimits::default();
    let mut cmd = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(x) = a.next() {
        if x == "--" {
            cmd.extend(a);
            break;
        }
        let Some(name) = x.to_str() else {
            fail("sandbox options must be valid UTF-8");
        };
        let Some((_, _, repeatable)) = PARSER_OPTIONS.iter().find(|(option, _, _)| *option == name)
        else {
            fail(&format!("unknown sandbox option {name}"));
        };
        if !repeatable && !seen.insert(name.to_string()) {
            fail(&format!("{name} may be specified only once"));
        }
        if name == "--deny-net" {
            net = shoal_leash::NetPolicy::Deny;
            continue;
        }
        if name == "--cpu-seconds" {
            limits.cpu_seconds = Some(parse_positive(&mut a, "--cpu-seconds"));
            continue;
        }
        if name == "--memory-bytes" {
            limits.memory_bytes = Some(parse_positive(&mut a, "--memory-bytes"));
            continue;
        }
        let path = PathBuf::from(
            a.next()
                .unwrap_or_else(|| fail("sandbox option requires path")),
        );
        match name {
            "--read" => s.read.push(path),
            "--write" => s.write.push(path),
            "--delete" => s.delete.push(path),
            _ => unreachable!("registry and parser match arms must remain in parity"),
        }
    }
    if cmd.is_empty() {
        fail("missing command")
    }
    if let Err(e) = shoal_leash::apply_process_limits(limits) {
        fail(&format!("process limit enforcement failed: {e}"))
    }
    let os_sandbox_requested = net == shoal_leash::NetPolicy::Deny
        || !s.read.is_empty()
        || !s.write.is_empty()
        || !s.delete.is_empty();
    if os_sandbox_requested && let Err(e) = shoal_leash::apply_sandbox_policy(&s, net) {
        fail(&format!("sandbox enforcement failed: {e}"))
    }
    let e = std::process::Command::new(&cmd[0]).args(&cmd[1..]).exec();
    fail(&format!("exec failed: {e}"))
}
fn parse_positive(args: &mut impl Iterator<Item = std::ffi::OsString>, option: &str) -> u64 {
    let raw = args
        .next()
        .unwrap_or_else(|| fail(&format!("{option} requires an integer")));
    raw.to_str()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| fail(&format!("{option} requires a positive integer")))
}
fn fail(msg: &str) -> ! {
    eprintln!("shoal-sandbox-exec: {msg}");
    std::process::exit(126)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .map(|(name, _, _)| *name)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(documented, registered);
        let man = include_str!("../../../man/shoal-sandbox-exec.1");
        for (name, _, _) in PARSER_OPTIONS {
            assert!(man.contains(&name.replace("--", "\\-\\-")), "{name}");
        }
    }
}
