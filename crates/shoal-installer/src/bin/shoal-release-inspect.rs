use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut arguments = std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let json = arguments
        .first()
        .is_some_and(|argument| argument == "--json");
    if json {
        arguments.remove(0);
    }
    let [archive, sbom] = arguments.as_slice() else {
        eprintln!("usage: shoal-release-inspect [--json] ARCHIVE SBOM");
        return ExitCode::FAILURE;
    };
    match shoal_installer::release::verify(archive, sbom) {
        Ok(verification) => {
            if json {
                match serde_json::to_string(&verification) {
                    Ok(encoded) => println!("{encoded}"),
                    Err(error) => {
                        eprintln!("shoal release inspector: {error}");
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                println!("{}", verification.summary);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("shoal release inspector: {error}");
            ExitCode::FAILURE
        }
    }
}
