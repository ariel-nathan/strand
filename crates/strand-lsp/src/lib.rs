//! The Strand language server (§8.4).
//!
//! §8.2 built diagnostics as a product surface from M1 — span, label and a
//! suggested fix where one genuinely exists — so most of what an editor wants
//! was already there. This crate carries it over the wire and answers the
//! position-based questions on top: what type is this, and where does it come
//! from.
//!
//! The analysis lives in `strandc`; `features` turns it into protocol answers
//! and `server` handles the connection.

pub mod features;
pub mod server;

use tower_lsp_server::{LspService, Server};

/// Serves the protocol over stdin/stdout until the client disconnects.
///
/// stdio is what every editor launches a server with, and it keeps the whole
/// thing inside the one `strand` binary that §8.1 asks for.
pub async fn serve() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(server::Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
