//! LSP backend.
//!
//! Implements `tower_lsp_server::LanguageServer`. Each handler:
//!
//! 1. Converts incoming LSP params to path / position types.
//! 2. Calls the appropriate method on the analysis engine.
//! 3. Converts the result back to LSP wire types and returns it.
//!
//! Heavy lifting lives in [`crate::analyzer`] and [`crate::server::converters`];
//! this file is intentionally thin.

use std::sync::Arc;

use tokio::sync::RwLock;
use tower_lsp_server::jsonrpc::Result as LspResult;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};

use crate::server::analysis::AnalysisHost;
use crate::server::converters::{
    completion_item_to_lsp, diagnostic_to_lsp, document_symbol_to_lsp, hover_to_lsp,
    location_to_lsp, position_from_lsp, signature_help_to_lsp, uri_to_path,
};
use crate::server::documents::DocumentStore;

// ---------------------------------------------------------------------------
// State shared across all async handlers
// ---------------------------------------------------------------------------

/// State shared across all async LSP handlers.
pub struct Backend {
    /// Handle to the LSP client, used to send notifications such as
    /// `textDocument/publishDiagnostics`.
    client: Client,

    /// Open-document store with rope-based incremental sync.
    documents: Arc<DocumentStore>,

    /// Analysis engine. Wrapped in a tokio `RwLock` so async handlers can
    /// share it; CPU-heavy operations are offloaded to the blocking pool via
    /// `tokio::task::spawn_blocking` and grab the lock with `blocking_write`
    /// / `blocking_read`.
    analysis: Arc<RwLock<AnalysisHost>>,
}

impl Backend {
    /// Construct a new backend. Returns `None` if the analysis engine cannot
    /// be initialized (typically because the `PEKO_ROOT_PATH` environment
    /// variable is unset or points at a non-existent directory).
    pub fn new(client: Client) -> Option<Self> {
        Some(Self {
            client,
            documents: Arc::new(DocumentStore::new()),
            analysis: Arc::new(RwLock::new(AnalysisHost::new()?)),
        })
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Feed updated text into the analysis engine and publish fresh diagnostics
    /// back to the client. Called after every `did_open` / `did_change` /
    /// `did_save` event.
    ///
    /// The engine call runs on the blocking thread pool because parsing and
    /// simulating a Peko source file is CPU-bound and would otherwise block
    /// the async executor. The write lock is held across the `update_file`
    /// and `diagnostics` calls so another `did_change` cannot slip in between
    /// them and publish stale diagnostics for the older text.
    async fn reanalyze(&self, uri: &Uri) {
        let path = uri_to_path(uri);

        let text = match self.documents.get_text(uri) {
            Some(t) => t,
            None => {
                tracing::warn!(uri = %uri.as_str(), ?path, "reanalyze: document not found in store");
                return;
            }
        };

        let analysis = Arc::clone(&self.analysis);
        let path_for_task = path.clone();
        let raw_diags = tokio::task::spawn_blocking(move || {
            let mut guard = analysis.blocking_write();
            guard.engine.update_file(&path_for_task, &text);
            guard.engine.diagnostics(&path_for_task)
        })
        .await
        .unwrap_or_default();

        let lsp_diags: Vec<Diagnostic> = raw_diags.iter().map(diagnostic_to_lsp).collect();

        self.client
            .publish_diagnostics(uri.clone(), lsp_diags, None)
            .await;
    }
}

// ---------------------------------------------------------------------------
// LanguageServer implementation
// ---------------------------------------------------------------------------

impl LanguageServer for Backend {
    // ------------------------------------------------------------------
    // Lifecycle
    // ------------------------------------------------------------------

    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        tracing::info!(workspace_folders = ?params.workspace_folders, "initialize");

        // Pick the first workspace folder as the project root. Multi-root
        // workspaces are not yet first-class; the analyzer's
        // `set_project_folder` walks parents looking for a Peko project
        // marker, so a single folder is enough in most cases.
        // TODO: Iterate all workspace folders and pick the first one that
        // resolves to a Peko project.
        if let Some(folder) = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
        {
            self.analysis
                .write()
                .await
                .engine
                .update_project_root(&uri_to_path(&folder.uri));
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "peko-language-server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            offset_encoding: None,
            capabilities: ServerCapabilities {
                // Send full document text on every change (simplest).
                // Switch to `TextDocumentSyncKind::INCREMENTAL` once incremental
                // edits are validated end-to-end with the engine.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),

                // Completions are triggered automatically and by `.`, `:`, `(`, `,`.
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".into(), ":".into(), "(".into(), ",".into()]),
                    ..Default::default()
                }),

                hover_provider: Some(HoverProviderCapability::Simple(true)),

                definition_provider: Some(OneOf::Left(true)),

                document_symbol_provider: Some(OneOf::Left(true)),

                document_formatting_provider: Some(OneOf::Left(true)),

                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".into(), ",".into()]),
                    retrigger_characters: Some(vec![")".into()]),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                }),

                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        tracing::info!("client acknowledged initialization");
        self.client
            .log_message(MessageType::INFO, "Language server initialized!")
            .await;
    }

    async fn shutdown(&self) -> LspResult<()> {
        tracing::info!("shutdown");
        Ok(())
    }

    // ------------------------------------------------------------------
    // Document sync
    // ------------------------------------------------------------------

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = &params.text_document.uri;
        let version = params.text_document.version;
        let text = &params.text_document.text;

        tracing::debug!(uri = %uri.as_str(), version, "did_open");
        self.documents.open(uri, version, text);
        self.reanalyze(uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = &params.text_document.uri;
        let version = params.text_document.version;

        tracing::debug!(uri = %uri.as_str(), version, "did_change");
        self.documents.update(uri, version, params.content_changes);
        self.reanalyze(uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = &params.text_document.uri;
        tracing::debug!(uri = %uri.as_str(), "did_save");
        self.reanalyze(uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = &params.text_document.uri;
        let path = uri_to_path(uri);

        tracing::debug!(uri = %uri.as_str(), "did_close");
        self.documents.close(uri);
        self.analysis.write().await.engine.close_file(&path);

        // Clear diagnostics for the closed file.
        self.client
            .publish_diagnostics(uri.clone(), vec![], None)
            .await;
    }

    // ------------------------------------------------------------------
    // Hover
    // ------------------------------------------------------------------

    async fn hover(&self, params: HoverParams) -> LspResult<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let path = uri_to_path(uri);
        let pos = position_from_lsp(params.text_document_position_params.position);

        tracing::debug!(uri = %uri.as_str(), ?pos, "hover");

        let analysis = self.analysis.read().await;
        Ok(analysis.engine.hover(&path, &pos).map(|h| hover_to_lsp(&h)))
    }

    // ------------------------------------------------------------------
    // Completions
    // ------------------------------------------------------------------

    async fn completion(&self, params: CompletionParams) -> LspResult<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let path = uri_to_path(uri);
        let pos = position_from_lsp(params.text_document_position.position);

        tracing::debug!(uri = %uri.as_str(), ?pos, "completion");

        let items: Vec<CompletionItem> = self
            .analysis
            .read()
            .await
            .engine
            .completions(&path, &pos)
            .iter()
            .map(completion_item_to_lsp)
            .collect();

        Ok(Some(CompletionResponse::Array(items)))
    }

    // ------------------------------------------------------------------
    // Go-to definition
    // ------------------------------------------------------------------

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> LspResult<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let path = uri_to_path(uri);
        let pos = position_from_lsp(params.text_document_position_params.position);

        tracing::debug!(uri = %uri.as_str(), ?pos, "goto_definition");

        let locations: Vec<Location> = self
            .analysis
            .read()
            .await
            .engine
            .goto_definition(&path, &pos)
            .iter()
            .map(location_to_lsp)
            .collect();

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(GotoDefinitionResponse::Array(locations)))
        }
    }

    // ------------------------------------------------------------------
    // Document symbols (outline / breadcrumbs)
    // ------------------------------------------------------------------

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> LspResult<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;
        let path = uri_to_path(uri);

        tracing::debug!(uri = %uri.as_str(), "document_symbol");

        let symbols: Vec<DocumentSymbol> = self
            .analysis
            .read()
            .await
            .engine
            .document_symbols(&path)
            .iter()
            .map(document_symbol_to_lsp)
            .collect();

        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    // ------------------------------------------------------------------
    // Signature help (triggered by `(`, `,`, or `)`)
    // ------------------------------------------------------------------

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> LspResult<Option<SignatureHelp>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let path = uri_to_path(uri);
        let pos = position_from_lsp(params.text_document_position_params.position);

        tracing::debug!(uri = %uri.as_str(), ?pos, "signature_help");

        Ok(self
            .analysis
            .read()
            .await
            .engine
            .signature_help(&path, &pos)
            .map(|sh| signature_help_to_lsp(&sh)))
    }

    // ------------------------------------------------------------------
    // Formatting
    // ------------------------------------------------------------------

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> LspResult<Option<Vec<TextEdit>>> {
        let uri = &params.text_document.uri;
        let path = uri_to_path(uri);

        tracing::debug!(uri = %uri.as_str(), "formatting");

        let original = match self.documents.get_text(uri) {
            Some(t) => t,
            None => return Ok(None),
        };

        let formatted = match self.analysis.read().await.engine.format(&path, &original) {
            Some(f) => f,
            None => return Ok(None),
        };

        // Return a single edit that replaces the entire document.
        let end_line = original.lines().count().saturating_sub(1) as u32;
        let end_char = original.lines().last().map(|l| l.len() as u32).unwrap_or(0);

        Ok(Some(vec![TextEdit {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: end_line,
                    character: end_char,
                },
            },
            new_text: formatted,
        }]))
    }
}
