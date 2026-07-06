//! Peko Language Server.
//!
//! The analysis engine and LSP backend, exposed as a library so the `peko`
//! CLI can run the server in-process as `peko lsp`. The server speaks LSP over
//! stdio: it reads requests on stdin and writes the JSON-RPC stream on stdout,
//! so nothing else may write to stdout while it runs.
#![allow(clippy::too_many_arguments)]

pub mod analyzer;
pub mod server;

use tower_lsp_server::{LspService, Server};
use tracing_subscriber::{EnvFilter, fmt};

use server::backend::Backend;

/// Run the language server over stdio until the client disconnects.
///
/// Tracing is routed to stderr so it does not corrupt the JSON-RPC stream on
/// stdout. Installing the tracing subscriber is best-effort: a caller that has
/// already set a global subscriber keeps its own.
pub async fn serve() {
    let _ = fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();

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
