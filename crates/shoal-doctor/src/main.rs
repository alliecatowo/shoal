fn main() {
    let json = std::env::args().any(|a| a == "--json");
    let report = shoal_doctor::run(&shoal_doctor::Options::from_env());
    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap())
    } else {
        print!("{report}")
    }
    std::process::exit(report.exit_code())
}
