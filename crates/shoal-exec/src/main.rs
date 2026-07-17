use std::os::unix::process::CommandExt;
use std::path::PathBuf;
fn main() {
    let raw = std::env::args_os().skip(1).collect::<Vec<_>>();
    if raw.as_slice() == ["-h"] || raw.as_slice() == ["--help"] {
        println!(
            "Run a command in a Shoal filesystem sandbox\n\nUsage: shoal-sandbox-exec [--read PATH] [--write PATH] [--delete PATH] -- COMMAND [ARG...]"
        );
        return;
    }
    if raw.as_slice() == ["-V"] || raw.as_slice() == ["--version"] {
        println!("shoal-sandbox-exec {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let mut a = raw.into_iter();
    let mut s = shoal_leash::FsSandbox::default();
    let mut cmd = Vec::new();
    while let Some(x) = a.next() {
        if x == "--" {
            cmd.extend(a);
            break;
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
    if let Err(e) = shoal_leash::apply_sandbox(&s) {
        fail(&format!("sandbox enforcement failed: {e}"))
    }
    let e = std::process::Command::new(&cmd[0]).args(&cmd[1..]).exec();
    fail(&format!("exec failed: {e}"))
}
fn fail(msg: &str) -> ! {
    eprintln!("shoal-sandbox-exec: {msg}");
    std::process::exit(126)
}
