//! Peko Language Server entry point.
//!
//! Boots the tokio runtime, sets up tracing to stderr (so it does not corrupt
//! the JSON-RPC stream on stdout), and hands the rest of the lifecycle to
//! `tower_lsp_server::Server`.

mod analyzer;
mod server;

use tower_lsp_server::{LspService, Server};
use tracing_subscriber::{EnvFilter, fmt};

use server::backend::Backend;

#[tokio::main]
async fn main() {
    // LSP clients communicate over stdio. Tracing must go to stderr or it
    // would corrupt the JSON-RPC stream that the editor reads on stdout.
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("starting Peko language server");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(|client| {
        Backend::new(client).expect(
            "failed to initialize Peko analyzer. Check that `PEKO_ROOT_PATH` is set and points at a valid Peko installation directory",
        )
    })
    .finish();

    Server::new(stdin, stdout, socket).serve(service).await;
}
