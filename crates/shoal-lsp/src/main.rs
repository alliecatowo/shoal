use shoal_lsp::{Backend, transport::bounded_lsp_input};
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
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
