fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
        println!("Diagnose the Shoal installation\n\nUsage: shoal-doctor [--json]");
        return;
    }
    if args.as_slice() == ["-V"] || args.as_slice() == ["--version"] {
        println!("shoal-doctor {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|arg| arg != "--json") {
        eprintln!("shoal-doctor: unexpected argument (try --help)");
        std::process::exit(2);
    }
    let json = !args.is_empty();
    let report = shoal_doctor::run(&shoal_doctor::Options::from_env());
    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap())
    } else {
        print!("{report}")
    }
    std::process::exit(report.exit_code())
}
