const HELP: &str = "Diagnose the Shoal installation

Usage:
  shoal-doctor [--json]

Options:
  --json         Emit the complete machine-readable report
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit

Output:
  A human checklist by default, or a JSON report containing every check and severity.

Errors:
  Failed checks remain in the report so one broken dependency does not hide later findings.

Examples:
  shoal-doctor
  shoal-doctor --json

Exit status:
  0 when required checks pass; 1 when required checks fail; 2 for invalid arguments or rendering.";

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
        println!("{HELP}");
        return;
    }
    if args.as_slice() == ["-V"] || args.as_slice() == ["--version"] {
        println!("shoal-doctor {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let json = match args.as_slice() {
        [] => false,
        [argument] if argument == "--json" => true,
        _ => {
            eprintln!("shoal-doctor: expected one optional --json (try --help)");
            std::process::exit(2);
        }
    };
    let report = shoal_doctor::run(&shoal_doctor::Options::from_env());
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(rendered) => println!("{rendered}"),
            Err(error) => {
                eprintln!("shoal-doctor: rendering JSON report: {error}");
                std::process::exit(2);
            }
        }
    } else {
        print!("{report}")
    }
    std::process::exit(report.exit_code())
}
