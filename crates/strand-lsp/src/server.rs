//! The JSON-RPC shell.
//!
//! This holds the open documents and translates requests into calls on
//! `features::Document`. There is deliberately no analysis here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};

use crate::features::Document;

#[derive(Debug, Default)]
struct Documents(HashMap<Uri, String>);

pub struct Backend {
    client: Client,
    /// Open files, keyed by URI. A Strand program is a single file — there is no
    /// module system — so there is nothing to invalidate across documents and a
    /// plain map is the whole story.
    documents: Arc<Mutex<Documents>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self { client, documents: Arc::new(Mutex::new(Documents::default())) }
    }

    fn text(&self, uri: &Uri) -> Option<String> {
        self.documents.lock().ok()?.0.get(uri).cloned()
    }

    /// Re-runs the front end and publishes what it found.
    ///
    /// The whole front end is re-run per edit rather than updated
    /// incrementally: it is well under a millisecond for a single file, so a
    /// query system would be complexity bought for nothing.
    async fn publish(&self, uri: Uri, text: &str) {
        let diagnostics = Document::new(text).diagnostics();
        self.client.publish_diagnostics(uri, diagnostics, None).await;
    }

    /// Runs `f` against the parsed document named by `uri`.
    fn with_document<T>(&self, uri: &Uri, f: impl FnOnce(&Document) -> T) -> Option<T> {
        let text = self.text(uri)?;
        Some(f(&Document::new(&text)))
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "strand-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                // Whole-document sync: the files are small and the front end is
                // fast, so there is nothing to gain from patching text ranges.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            // `None` leaves the protocol default of UTF-16, which is what
            // `LineIndex` counts in.
            offset_encoding: None,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client.log_message(MessageType::INFO, "strand-lsp ready").await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        if let Ok(mut documents) = self.documents.lock() {
            documents.0.insert(document.uri.clone(), document.text.clone());
        }
        self.publish(document.uri, &document.text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync, so the last change carries the entire document.
        let Some(change) = params.content_changes.into_iter().next_back() else { return };
        let uri = params.text_document.uri;
        if let Ok(mut documents) = self.documents.lock() {
            documents.0.insert(uri.clone(), change.text.clone());
        }
        self.publish(uri, &change.text).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Ok(mut documents) = self.documents.lock() {
            documents.0.remove(&uri);
        }
        // Clear the squiggles: a closed file has no diagnostics to show.
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position = params.text_document_position_params;
        Ok(self
            .with_document(&position.text_document.uri, |document| {
                document.hover(position.position)
            })
            .flatten())
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let position = params.text_document_position_params;
        let uri = position.text_document.uri;
        let found = self
            .with_document(&uri, |document| document.definition(position.position))
            .flatten();
        Ok(found.map(|range| {
            GotoDefinitionResponse::Scalar(Location { uri: uri.clone(), range })
        }))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let position = params.text_document_position;
        let uri = position.text_document.uri;
        let found = self
            .with_document(&uri, |document| document.references(position.position))
            .unwrap_or_default();
        Ok(Some(
            found
                .into_iter()
                .map(|range| Location { uri: uri.clone(), range })
                .collect(),
        ))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let symbols = self
            .with_document(&params.text_document.uri, |document| document.symbols())
            .unwrap_or_default();
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }
}
