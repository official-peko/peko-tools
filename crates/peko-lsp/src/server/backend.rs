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

use std::path::Path;
use std::sync::{Arc, OnceLock};

use tokio::sync::RwLock;
use tower_lsp_server::jsonrpc::Result as LspResult;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};

use crate::server::analysis::{self, AnalysisHost};
use crate::server::converters::{
    completion_item_to_lsp, diagnostic_to_lsp, document_symbol_to_lsp, hover_to_lsp,
    location_to_lsp, semantic_tokens_to_lsp, signature_help_to_lsp, uri_to_path,
};
use crate::server::documents::DocumentStore;
use crate::server::encoding::{LineIndex, PosMapper, WireEncoding};

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

    /// Position encoding negotiated with the client during `initialize`.
    /// Defaults to UTF-16 (the mandatory LSP default) until negotiation runs.
    encoding: OnceLock<WireEncoding>,
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
            encoding: OnceLock::new(),
        })
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Position encoding negotiated with the client, or the UTF-16 default if
    /// negotiation has not run yet.
    fn encoding(&self) -> WireEncoding {
        self.encoding.get().copied().unwrap_or_default()
    }

    /// Build a line index over the current text of an open document. Falls back
    /// to an empty index when the document is not tracked.
    fn line_index_for_uri(&self, uri: &Uri) -> LineIndex {
        LineIndex::new(&self.documents.get_text(uri).unwrap_or_default())
    }

    /// Build a line index over a file's text for mapping positions that point
    /// into it. Prefers the open-document copy, then the on-disk contents, then
    /// an empty index.
    fn line_index_for_path(&self, path: &Path) -> LineIndex {
        let text = self
            .documents
            .get_text_by_path(path)
            .or_else(|| std::fs::read_to_string(path).ok())
            .unwrap_or_default();
        LineIndex::new(&text)
    }

    /// Convert a wire position from the client into an internal char-based
    /// position, using the text of the open document `uri`.
    fn to_internal(&self, uri: &Uri, wire: Position) -> analysis::Position {
        self.line_index_for_uri(uri)
            .wire_to_internal(wire, self.encoding())
    }

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
        let text_for_task = text.clone();
        let raw_diags = tokio::task::spawn_blocking(move || {
            let mut guard = analysis.blocking_write();
            guard.engine.update_file(&path_for_task, &text_for_task);
            guard.engine.diagnostics(&path_for_task)
        })
        .await
        .unwrap_or_default();

        // Diagnostics are scoped to this file, so map them with an index over
        // the same text that produced them.
        let index = LineIndex::new(&text);
        let map = PosMapper::new(&index, self.encoding());
        let lsp_diags: Vec<Diagnostic> = raw_diags.iter().map(|d| diagnostic_to_lsp(d, &map)).collect();

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

        // Negotiate the position encoding. The client advertises the encodings
        // it supports under `general.position_encodings`; pick one (preferring
        // UTF-8) and advertise the same choice back below.
        let encoding = WireEncoding::negotiate(
            params
                .capabilities
                .general
                .as_ref()
                .and_then(|general| general.position_encodings.as_deref()),
        );
        let _ = self.encoding.set(encoding);
        tracing::info!(?encoding, "negotiated position encoding");

        // Pick the first workspace folder as the project root. Multi-root
        // workspaces are not yet first-class; the analyzer's
        // `set_project_folder` walks parents looking for a Peko project
        // marker, so a single folder is enough in most cases.
        // TODO: Iterate all workspace folders and pick the first one that
        // resolves to a Peko project.
        let workspace_root = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
            .map(|folder| uri_to_path(&folder.uri));

        if let Some(path) = &workspace_root {
            self.analysis
                .write()
                .await
                .engine
                .update_project_root(path);
        }

        // Preload and memoize the project's packages on a background task so
        // initialize returns immediately instead of blocking on the (multi-
        // second) preload. Requests that arrive before it finishes load modules
        // on demand; once it completes they reuse the memoized modules.
        if workspace_root.is_some() {
            let analysis = Arc::clone(&self.analysis);
            tokio::task::spawn_blocking(move || {
                analysis.blocking_write().engine.preload_packages();
            });
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "peko-language-server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            offset_encoding: None,
            capabilities: ServerCapabilities {
                // Advertise the negotiated position encoding so the client
                // sends and receives `character` offsets in the same units.
                position_encoding: Some(encoding.as_kind()),

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

                references_provider: Some(OneOf::Left(true)),

                document_symbol_provider: Some(OneOf::Left(true)),

                document_formatting_provider: Some(OneOf::Left(true)),

                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".into(), ",".into()]),
                    retrigger_characters: Some(vec![")".into()]),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                }),

                // Whole-file semantic highlighting. The legend order matches the
                // token-type ids the engine emits.
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                        legend: SemanticTokensLegend {
                            token_types: analysis::SEMANTIC_TOKEN_TYPES
                                .iter()
                                .map(|t| SemanticTokenType::new(t))
                                .collect(),
                            token_modifiers: analysis::SEMANTIC_TOKEN_MODIFIERS
                                .iter()
                                .map(|m| SemanticTokenModifier::new(m))
                                .collect(),
                        },
                        full: Some(SemanticTokensFullOptions::Bool(true)),
                        range: Some(false),
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: None,
                        },
                    }),
                ),

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
        let pos = self.to_internal(uri, params.text_document_position_params.position);

        tracing::debug!(uri = %uri.as_str(), ?pos, "hover");

        let index = self.line_index_for_uri(uri);
        let map = PosMapper::new(&index, self.encoding());
        let analysis = self.analysis.read().await;
        Ok(analysis
            .engine
            .hover(&path, &pos)
            .map(|h| hover_to_lsp(&h, &map)))
    }

    // ------------------------------------------------------------------
    // Completions
    // ------------------------------------------------------------------

    async fn completion(&self, params: CompletionParams) -> LspResult<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let path = uri_to_path(uri);
        let pos = self.to_internal(uri, params.text_document_position.position);

        tracing::debug!(uri = %uri.as_str(), ?pos, "completion");

        let index = self.line_index_for_uri(uri);
        let map = PosMapper::new(&index, self.encoding());
        let items: Vec<CompletionItem> = self
            .analysis
            .read()
            .await
            .engine
            .completions(&path, &pos)
            .iter()
            .map(|item| completion_item_to_lsp(item, &map))
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
        let pos = self.to_internal(uri, params.text_document_position_params.position);

        tracing::debug!(uri = %uri.as_str(), ?pos, "goto_definition");

        let raw_locations = self
            .analysis
            .read()
            .await
            .engine
            .goto_definition(&path, &pos);

        // Each location may point into a different file, so map its range with
        // an index built from that file's own text.
        let encoding = self.encoding();
        let locations: Vec<Location> = raw_locations
            .iter()
            .map(|loc| {
                let index = self.line_index_for_path(&loc.file);
                location_to_lsp(loc, &PosMapper::new(&index, encoding))
            })
            .collect();

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(GotoDefinitionResponse::Array(locations)))
        }
    }

    // ------------------------------------------------------------------
    // Find references
    // ------------------------------------------------------------------

    async fn references(&self, params: ReferenceParams) -> LspResult<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let path = uri_to_path(uri);
        let pos = self.to_internal(uri, params.text_document_position.position);

        tracing::debug!(uri = %uri.as_str(), ?pos, "references");

        // The buffer comes from the document store when open, else disk (which
        // matches the buffer at open time), mirroring semantic_tokens_full.
        let text = match self.documents.get_text(uri) {
            Some(t) => t,
            None => std::fs::read_to_string(&path).unwrap_or_default(),
        };
        if text.is_empty() {
            return Ok(None);
        }

        let raw = self
            .analysis
            .read()
            .await
            .engine
            .references(&path, &text, &pos);

        let encoding = self.encoding();
        let locations: Vec<Location> = raw
            .iter()
            .map(|loc| {
                let index = self.line_index_for_path(&loc.file);
                location_to_lsp(loc, &PosMapper::new(&index, encoding))
            })
            .collect();

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
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

        let index = self.line_index_for_uri(uri);
        let map = PosMapper::new(&index, self.encoding());
        let symbols: Vec<DocumentSymbol> = self
            .analysis
            .read()
            .await
            .engine
            .document_symbols(&path)
            .iter()
            .map(|s| document_symbol_to_lsp(s, &map))
            .collect();

        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    // ------------------------------------------------------------------
    // Semantic tokens (whole-file highlighting)
    // ------------------------------------------------------------------

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> LspResult<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;
        let path = uri_to_path(uri);

        tracing::debug!(uri = %uri.as_str(), "semantic_tokens_full");

        // The buffer comes from the document store when the file is open. The
        // editor can request tokens before its did_open is processed, so fall
        // back to the file on disk, which matches the buffer at open time.
        let text = match self.documents.get_text(uri) {
            Some(t) => t,
            None => std::fs::read_to_string(&path).unwrap_or_default(),
        };
        if text.is_empty() {
            return Ok(None);
        }
        let index = LineIndex::new(&text);
        let map = PosMapper::new(&index, self.encoding());
        let tokens = self.analysis.read().await.engine.semantic_tokens(&path, &text);
        let data = semantic_tokens_to_lsp(&tokens, &map);

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
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
        let pos = self.to_internal(uri, params.text_document_position_params.position);

        tracing::debug!(uri = %uri.as_str(), ?pos, "signature_help");

        let encoding = self.encoding();
        Ok(self
            .analysis
            .read()
            .await
            .engine
            .signature_help(&path, &pos)
            .map(|sh| signature_help_to_lsp(&sh, encoding)))
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

        let indent_size = params.options.tab_size.max(1) as usize;
        let use_spaces = params.options.insert_spaces;
        let formatted = match self.analysis.read().await.engine.format(
            &path,
            &original,
            indent_size,
            use_spaces,
        ) {
            Some(f) => f,
            None => return Ok(None),
        };

        // Return a single edit that replaces the entire document. The end of
        // the replacement range is the end of the source, expressed in the
        // negotiated wire encoding.
        let index = LineIndex::new(&original);
        let end = index.end_of_source(self.encoding());

        Ok(Some(vec![TextEdit {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end,
            },
            new_text: formatted,
        }]))
    }
}
