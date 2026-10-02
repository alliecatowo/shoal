use shoal_lsp::{Backend, transport::bounded_lsp_input};
use tower_lsp::{LspService, Server};

const HELP: &str = "Shoal language server

Usage:
  shoal-lsp

Options:
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit

Output:
  Reads and writes framed Language Server Protocol messages over stdin and stdout.

Errors:
  Invalid CLI arguments return 2; malformed or oversized transport input closes the session.

Examples:
  shoal-lsp

Exit status:
  0 after an orderly protocol shutdown; 2 for invalid command-line arguments.";

#[tokio::main]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("-h" | "--help") if std::env::args().len() == 2 => {
            println!("{HELP}");
            return;
        }
        Some("-V" | "--version") if std::env::args().len() == 2 => {
            println!("shoal-lsp {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some(_) => {
            eprintln!("shoal-lsp: unexpected argument (try --help)");
            std::process::exit(2);
        }
        None => {}
    }
    let (stdin, pump) = bounded_lsp_input(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
    if !pump.is_finished() {
        pump.abort();
    }
    if let Ok(Err(error)) = pump.await {
        eprintln!("shoal-lsp: input transport closed: {error}");
    }
}
