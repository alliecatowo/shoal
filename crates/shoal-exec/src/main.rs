use std::os::unix::process::CommandExt;
use std::path::PathBuf;
fn main() {
    let raw = std::env::args_os().skip(1).collect::<Vec<_>>();
    if raw.as_slice() == ["-h"] || raw.as_slice() == ["--help"] {
        println!(
            "Run a command with Shoal child-only OS controls\n\nUsage: shoal-sandbox-exec [--deny-net] [--cpu-seconds N] [--memory-bytes N] [--read PATH] [--write PATH] [--delete PATH] -- COMMAND [ARG...]"
        );
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
    while let Some(x) = a.next() {
        if x == "--" {
            cmd.extend(a);
            break;
        }
        if x == "--deny-net" {
            net = shoal_leash::NetPolicy::Deny;
            continue;
        }
        if x == "--cpu-seconds" {
            limits.cpu_seconds = Some(parse_positive(&mut a, "--cpu-seconds"));
            continue;
        }
        if x == "--memory-bytes" {
            limits.memory_bytes = Some(parse_positive(&mut a, "--memory-bytes"));
            continue;
        }
        let path = PathBuf::from(
            a.next()
                .unwrap_or_else(|| fail("sandbox option requires path")),
        );
        match x.to_str() {
            Some("--read") => s.read.push(path),
            Some("--write") => s.write.push(path),
            Some("--delete") => s.delete.push(path),
            _ => fail("unknown sandbox option"),
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
