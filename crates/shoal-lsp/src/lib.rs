mod analysis;
mod document;
mod scheduler;
pub mod transport;

use analysis::{Symbol, collect_symbols};
use document::*;
use scheduler::{AnalysisAdmission, AnalysisJob, AnalysisQueue};
use shoal_ast::{Program, Span};
use shoal_syntax::commands::{CommandFacts, CommandSource, resolve_command_source};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, RwLock};
use tower_lsp::{Client, LanguageServer, jsonrpc::Result, lsp_types::*};

/// Retained source is identity-bearing editor state: never silently evict an
/// open URI, because a later change could then apply to the wrong baseline.
const MAX_OPEN_DOCUMENTS: usize = 128;
pub(crate) const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_OPEN_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const MAX_DOCUMENT_URI_BYTES: usize = 4 * 1024;
const MAX_SYMBOLS: usize = 1_024;
const MAX_COMPLETIONS: usize = 1_024;
const MAX_DIAGNOSTICS: usize = 256;

pub struct Backend {
    client: Client,
    docs: Arc<RwLock<HashMap<Url, DocumentState>>>,
    updates: Arc<Mutex<()>>,
    analyses: Arc<AnalysisQueue>,
}

#[derive(Clone)]
struct DocumentState {
    text: String,
    version: i32,
    ast: Option<Program>,
    diagnostics: Vec<Diagnostic>,
    symbols: Vec<Symbol>,
}

fn formatting_edit(
    text: &str,
    ast: &Program,
) -> std::result::Result<TextEdit, shoal_syntax::FormatRefusal> {
    let new_text = shoal_syntax::format_source_preserving_trivia(text, ast)?;
    Ok(TextEdit {
        range: Range::new(Position::new(0, 0), byte_to_position(text, text.len())),
        new_text,
    })
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            docs: Default::default(),
            updates: Default::default(),
            analyses: Arc::new(AnalysisQueue::new()),
        }
    }

    fn resource_limit_diagnostic(message: String) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 0)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("shoal-lsp".into()),
            message,
            ..Default::default()
        }
    }
    async fn schedule_analysis(&self, job: AnalysisJob) {
        match self.analyses.admit(&job).await {
            AnalysisAdmission::Start => {
                let client = self.client.clone();
                let docs = Arc::clone(&self.docs);
                let updates = Arc::clone(&self.updates);
                let analyses = Arc::clone(&self.analyses);
                tokio::spawn(run_analysis_worker(job, client, docs, updates, analyses));
            }
            AnalysisAdmission::Rejected(message) => {
                self.client.log_message(MessageType::WARNING, message).await;
            }
            AnalysisAdmission::Queued | AnalysisAdmission::Ignored => {}
        }
    }
}

async fn run_analysis_worker(
    mut job: AnalysisJob,
    client: Client,
    docs: Arc<RwLock<HashMap<Url, DocumentState>>>,
    updates: Arc<Mutex<()>>,
    analyses: Arc<AnalysisQueue>,
) {
    loop {
        let permit = match Arc::clone(&analyses.permits).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        let analyze_job = job.clone();
        let analyzed = tokio::task::spawn_blocking(move || {
            analyze_document(&analyze_job.uri, analyze_job.text, analyze_job.version)
        })
        .await;
        drop(permit);
        match analyzed {
            Ok(state) => {
                let publish = {
                    let _update = updates.lock().await;
                    let current = docs.read().await.get(&job.uri).cloned();
                    if current
                        .as_ref()
                        .is_some_and(|current| analysis_is_current(current, &state))
                    {
                        let diagnostics = state.diagnostics.clone();
                        docs.write().await.insert(job.uri.clone(), state);
                        Some(diagnostics)
                    } else {
                        None
                    }
                };
                if let Some(diagnostics) = publish {
                    client
                        .publish_diagnostics(job.uri.clone(), diagnostics, Some(job.version))
                        .await;
                }
            }
            Err(_) => {
                client
                    .log_message(MessageType::ERROR, "document analysis task failed")
                    .await;
            }
        }
        let completed_uri = job.uri.clone();
        let completed_version = job.version;
        let Some(next) = analyses.complete(&completed_uri, completed_version).await else {
            return;
        };
        job = next;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                document_formatting_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions::default()),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "shoal-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
        })
    }
    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "shoal language server ready")
            .await;
    }
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
    async fn did_open(&self, p: DidOpenTextDocumentParams) {
        let uri = p.text_document.uri;
        let text = p.text_document.text;
        let version = p.text_document.version;
        let admission = {
            let _update = self.updates.lock().await;
            let mut docs = self.docs.write().await;
            admit_document(
                &mut docs,
                uri.clone(),
                pending_document(text.clone(), version),
            )
        };
        if let Err(message) = admission {
            self.client
                .log_message(MessageType::ERROR, message.clone())
                .await;
            self.client
                .publish_diagnostics(
                    uri,
                    vec![Self::resource_limit_diagnostic(message)],
                    Some(version),
                )
                .await;
            return;
        }
        self.schedule_analysis(AnalysisJob { uri, text, version })
            .await;
    }
    async fn did_change(&self, p: DidChangeTextDocumentParams) {
        let uri = p.text_document.uri;
        let version = p.text_document.version;
        let staged = {
            let _update = self.updates.lock().await;
            let mut docs = self.docs.write().await;
            let Some(current) = docs.get(&uri) else {
                return;
            };
            if version <= current.version {
                return;
            }
            match apply_content_changes(&current.text, &p.content_changes) {
                Ok(next) => admit_document(
                    &mut docs,
                    uri.clone(),
                    pending_document(next.clone(), version),
                )
                .map(|()| next),
                Err(message) => Err(message),
            }
        };
        let next_text = match staged {
            Ok(text) => text,
            Err(message) => {
                self.client.log_message(MessageType::WARNING, message).await;
                return;
            }
        };
        self.schedule_analysis(AnalysisJob {
            uri,
            text: next_text,
            version,
        })
        .await;
    }
    async fn did_close(&self, p: DidCloseTextDocumentParams) {
        self.analyses.cancel_pending(&p.text_document.uri).await;
        let _update = self.updates.lock().await;
        self.docs.write().await.remove(&p.text_document.uri);
        self.client
            .publish_diagnostics(p.text_document.uri, vec![], None)
            .await;
    }
    async fn formatting(&self, p: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let decision = {
            let docs = self.docs.read().await;
            let Some(doc) = docs.get(&p.text_document.uri) else {
                return Ok(None);
            };
            let Some(ast) = &doc.ast else {
                return Ok(None);
            };
            match formatting_edit(&doc.text, ast) {
                Ok(edit) => Ok(edit),
                Err(refusal) => Err((
                    doc.version,
                    doc.text.clone(),
                    doc.diagnostics.clone(),
                    refusal,
                )),
            }
        };
        match decision {
            Ok(edit) => Ok(Some(vec![edit])),
            Err((version, text, mut diagnostics, refusal)) => {
                diagnostics.truncate(MAX_DIAGNOSTICS.saturating_sub(1));
                diagnostics.push(Diagnostic {
                    range: span_range(&text, refusal.span),
                    severity: Some(DiagnosticSeverity::WARNING),
                    code: Some(NumberOrString::String("format_trivia".into())),
                    source: Some("shoal-formatter".into()),
                    message: refusal.message.into(),
                    ..Default::default()
                });
                self.client
                    .publish_diagnostics(p.text_document.uri, diagnostics, Some(version))
                    .await;
                Ok(None)
            }
        }
    }
    async fn completion(&self, p: CompletionParams) -> Result<Option<CompletionResponse>> {
        let docs = self.docs.read().await;
        let Some(doc) = docs.get(&p.text_document_position.text_document.uri) else {
            return Ok(None);
        };
        let byte = position_to_byte(&doc.text, p.text_document_position.position);
        let (replace_start, replace_end) = word_bounds_at(&doc.text, byte);
        let replace_range = Range::new(
            byte_to_position(&doc.text, replace_start),
            byte_to_position(&doc.text, replace_end),
        );
        // Completion vocabulary = language keywords ∪ builtin command heads
        // (see `base_vocabulary`) ∪ lexical declarations seen so far.
        let mut names: Vec<String> = base_vocabulary().map(str::to_string).collect();
        names.extend(
            doc.symbols
                .iter()
                .filter(|symbol| symbol_visible_at(symbol, byte))
                .map(|symbol| symbol.name.clone()),
        );
        names.sort();
        names.dedup();
        names.truncate(MAX_COMPLETIONS);
        Ok(Some(CompletionResponse::Array(
            names
                .into_iter()
                .map(|label| {
                    let symbol = doc
                        .symbols
                        .iter()
                        .rev()
                        .find(|symbol| symbol.name == label && symbol_visible_at(symbol, byte));
                    let facts = symbol_facts(symbol);
                    let source = resolve_command_source(&label, facts);
                    let keyword = is_keyword(&label) && symbol.is_none();
                    CompletionItem {
                        kind: Some(
                            symbol.map_or_else(|| completion_kind(&label), Symbol::completion_kind),
                        ),
                        detail: Some(if keyword {
                            "language keyword".into()
                        } else {
                            format!("{} — {}", source.as_str(), source.reason())
                        }),
                        documentation: symbol
                            .and_then(|symbol| symbol.doc.clone().map(Documentation::String)),
                        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                            range: replace_range,
                            new_text: label.clone(),
                        })),
                        label,
                        ..Default::default()
                    }
                })
                .collect(),
        )))
    }
    async fn hover(&self, p: HoverParams) -> Result<Option<Hover>> {
        let docs = self.docs.read().await;
        let Some(doc) = docs.get(&p.text_document_position_params.text_document.uri) else {
            return Ok(None);
        };
        let pos = position_to_byte(&doc.text, p.text_document_position_params.position);
        let word = word_at(&doc.text, pos);
        let symbol = definition_symbol(&doc.symbols, word, pos);
        let value = if let Some(symbol) = symbol {
            let mut value = format!("**{}** — {}", symbol.name, symbol.detail);
            if let Some(doc) = &symbol.doc {
                value.push_str("\n\n");
                value.push_str(doc);
            }
            value
        } else if let Some(help) = help(word) {
            help.into()
        } else {
            let source = resolve_command_source(word, CommandFacts::default());
            if source == CommandSource::External {
                return Ok(None);
            }
            format!(
                "`{word}` resolves as **{}**: {}",
                source.as_str(),
                source.reason()
            )
        };
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }

    async fn goto_definition(
        &self,
        p: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let docs = self.docs.read().await;
        let Some(doc) = docs.get(&p.text_document_position_params.text_document.uri) else {
            return Ok(None);
        };
        let pos = position_to_byte(&doc.text, p.text_document_position_params.position);
        let word = word_at(&doc.text, pos);
        if let Some(symbol) = definition_symbol(&doc.symbols, word, pos) {
            return Ok(Some(GotoDefinitionResponse::Scalar(Location::new(
                p.text_document_position_params.text_document.uri,
                span_range(&doc.text, symbol.span),
            ))));
        }
        let source_uri = &p.text_document_position_params.text_document.uri;
        Ok(definition_in_used_module(source_uri, doc, pos, word, &docs)
            .map(GotoDefinitionResponse::Scalar))
    }

    async fn document_symbol(
        &self,
        p: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let docs = self.docs.read().await;
        let Some(doc) = docs.get(&p.text_document.uri) else {
            return Ok(None);
        };
        Ok(Some(DocumentSymbolResponse::Nested(
            analysis::document_symbols(&doc.text, &doc.symbols, span_range),
        )))
    }
}

/// Admit or replace one open document without exceeding retained identity or
/// source-byte budgets. Existing state is left untouched on rejection.
fn admit_document(
    docs: &mut HashMap<Url, DocumentState>,
    uri: Url,
    state: DocumentState,
) -> std::result::Result<(), String> {
    if uri.as_str().len() > MAX_DOCUMENT_URI_BYTES {
        return Err(format!(
            "document URI is {} bytes; shoal-lsp accepts at most {MAX_DOCUMENT_URI_BYTES} bytes",
            uri.as_str().len()
        ));
    }
    let source_bytes = state.text.len();
    if source_bytes > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "document is {source_bytes} bytes; shoal-lsp accepts at most {MAX_DOCUMENT_BYTES} bytes per open document"
        ));
    }
    if !docs.contains_key(&uri) && docs.len() >= MAX_OPEN_DOCUMENTS {
        return Err(format!(
            "shoal-lsp already retains {MAX_OPEN_DOCUMENTS} open documents; close one before opening another"
        ));
    }
    let retained_without_uri = docs
        .iter()
        .filter(|(open_uri, _)| *open_uri != &uri)
        .fold(0usize, |total, (_, doc)| {
            total.saturating_add(doc.text.len())
        });
    if retained_without_uri.saturating_add(source_bytes) > MAX_OPEN_SOURCE_BYTES {
        return Err(format!(
            "open source would exceed shoal-lsp's {MAX_OPEN_SOURCE_BYTES}-byte retained-source budget"
        ));
    }
    docs.insert(uri, state);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn change(
        range: Range,
        range_length: Option<u32>,
        text: &str,
    ) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(range),
            range_length,
            text: text.into(),
        }
    }

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("shoal-lsp-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn formatting_refuses_trivia_but_formats_hashes_inside_strings() {
        let commented = "#!/usr/bin/env shoal\nlet x=1 # keep\n";
        let ast = shoal_syntax::parse(commented).unwrap();
        let refusal = formatting_edit(commented, &ast).unwrap_err();
        assert_eq!(refusal.span.start, 0);
        assert!(refusal.message.contains("cannot yet be preserved"));

        let semantic_hash = "let hash=\"#\"";
        let ast = shoal_syntax::parse(semantic_hash).unwrap();
        let edit = formatting_edit(semantic_hash, &ast).unwrap();
        assert_eq!(edit.new_text, "let hash = \"#\"\n");
        assert_eq!(edit.range.end, Position::new(0, 12));
    }

    #[test]
    fn hostile_symbol_volume_is_bounded_before_responses() {
        let mut text = String::new();
        for index in 0..(MAX_SYMBOLS * 2) {
            text.push_str(&format!("let symbol_{index} = {index}\n"));
        }
        let state = analyze_document(
            &Url::parse("file:///tmp/many-symbols.shl").unwrap(),
            text,
            1,
        );
        assert_eq!(state.symbols.len(), MAX_SYMBOLS);
        assert!(state.diagnostics.len() <= MAX_DIAGNOSTICS);
    }

    #[test]
    fn utf16_positions() {
        let s = "a🦀b\n雪x";
        assert_eq!(byte_to_position(s, 5), Position::new(0, 3));
        assert_eq!(position_to_byte(s, Position::new(0, 3)), 5);
        assert_eq!(byte_to_position(s, s.len()), Position::new(1, 2));
    }
    #[test]
    fn base_vocabulary_has_keywords_and_registry_builtins() {
        let vocab: Vec<&str> = base_vocabulary().collect();
        // Language keywords (RESERVED + the parser's extra statement forms).
        for kw in ["let", "fn", "match", "with", "spawn", "sh"] {
            assert!(vocab.contains(&kw), "missing keyword `{kw}`");
        }
        // Builtin command heads sourced from the canonical
        // `shoal_syntax::commands` registry (the same list eval dispatches on),
        // including ones the old hand-copied WORDS list omitted.
        for head in ["cd", "ls", "reef", "jobs", "history", "undo", "plan"] {
            assert!(vocab.contains(&head), "missing builtin `{head}`");
        }
    }

    #[test]
    fn completion_kinds_use_shared_command_classification() {
        assert_eq!(completion_kind("ls"), CompletionItemKind::FUNCTION);
        assert_eq!(completion_kind("cd"), CompletionItemKind::FUNCTION);
        assert_eq!(completion_kind("let"), CompletionItemKind::KEYWORD);
    }

    #[test]
    fn declarations_are_lexical() {
        let text = "let alpha = 1\nfn beta() {}";
        let ast = shoal_syntax::parse(text).unwrap();
        let names = collect_symbols(&ast, text)
            .into_iter()
            .map(|symbol| symbol.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["alpha", "beta"])
    }
    #[test]
    fn incomplete_diagnostic() {
        let state = analyze_document(
            &Url::parse("file:///tmp/test.shl").unwrap(),
            "let x = [1,".into(),
            1,
        );
        assert_eq!(state.diagnostics.len(), 1)
    }

    #[test]
    fn byte_positions_clamp_to_utf8_boundaries() {
        let s = "a🦀b";
        assert_eq!(byte_to_position(s, 2), Position::new(0, 1));
    }

    #[test]
    fn definition_prefers_the_latest_prior_shadow() {
        let text = "let x = 1\nlet x = 2\nx";
        let ast = shoal_syntax::parse(text).unwrap();
        let symbols = collect_symbols(&ast, text);
        let symbol = definition_symbol(&symbols, "x", text.len()).unwrap();
        assert_eq!(
            &text[symbol.span.start as usize..symbol.span.end as usize],
            "x"
        );
        assert!(symbol.span.start > 10);
    }

    #[test]
    fn binding_is_not_visible_inside_its_own_initializer() {
        let text = "let x = x\nx";
        let ast = shoal_syntax::parse(text).unwrap();
        let symbols = collect_symbols(&ast, text);
        let initializer = text.find("= x").unwrap() + 2;
        assert!(definition_symbol(&symbols, "x", initializer).is_none());
        assert!(definition_symbol(&symbols, "x", text.len()).is_some());
    }

    #[test]
    fn function_parameter_does_not_leak_outside_its_scope() {
        let text = "fn f(x) { x }\nx";
        let ast = shoal_syntax::parse(text).unwrap();
        let symbols = collect_symbols(&ast, text);
        assert!(definition_symbol(&symbols, "x", text.len()).is_none());
    }

    #[test]
    fn planner_marks_opaque_effects_without_executing() {
        let state = analyze_document(
            &Url::parse("file:///tmp/test.shl").unwrap(),
            "use ./unknown-module".into(),
            7,
        );
        assert!(state.ast.is_some());
        assert!(state.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == Some(NumberOrString::String("opaque_effect".into()))
                && diagnostic.source.as_deref() == Some("shoal-planner")
        }));
    }

    #[test]
    fn declaration_span_uses_identifier_boundaries() {
        let text = "fn n() {}";
        let ast = shoal_syntax::parse(text).unwrap();
        let symbol = collect_symbols(&ast, text).remove(0);
        assert_eq!(
            &text[symbol.span.start as usize..symbol.span.end as usize],
            "n"
        );
    }

    #[test]
    fn incremental_changes_are_sequential_and_utf16_aware() {
        let changes = vec![
            change(
                Range::new(Position::new(0, 1), Position::new(0, 3)),
                Some(2),
                "X",
            ),
            change(
                Range::new(Position::new(1, 0), Position::new(1, 6)),
                Some(6),
                "done",
            ),
        ];
        assert_eq!(
            apply_content_changes("a🦀b\nsecond", &changes).unwrap(),
            "aXb\ndone"
        );
    }

    #[test]
    fn incremental_changes_reject_malformed_ranges_without_mutating_input() {
        let surrogate_middle = change(
            Range::new(Position::new(0, 2), Position::new(0, 3)),
            None,
            "X",
        );
        assert!(apply_content_changes("a🦀b", &[surrogate_middle]).is_err());

        let missing_line = change(
            Range::new(Position::new(9, 0), Position::new(9, 0)),
            None,
            "X",
        );
        assert!(apply_content_changes("one", &[missing_line]).is_err());

        let reversed = change(
            Range::new(Position::new(0, 2), Position::new(0, 1)),
            None,
            "X",
        );
        assert!(apply_content_changes("abc", &[reversed]).is_err());

        let wrong_length = change(
            Range::new(Position::new(0, 1), Position::new(0, 3)),
            Some(1),
            "X",
        );
        assert!(apply_content_changes("a🦀b", &[wrong_length]).is_err());
    }

    #[test]
    fn stale_analysis_requires_matching_version_and_text() {
        let current = pending_document("let current = 1".into(), 8);
        let same = pending_document("let current = 1".into(), 8);
        let old_version = pending_document("let current = 1".into(), 7);
        let wrong_text = pending_document("let stale = 1".into(), 8);
        assert!(analysis_is_current(&current, &same));
        assert!(!analysis_is_current(&current, &old_version));
        assert!(!analysis_is_current(&current, &wrong_text));
    }

    #[test]
    fn document_admission_rejects_identity_growth_without_evicting_open_uris() {
        let mut docs = HashMap::new();
        for index in 0..MAX_OPEN_DOCUMENTS {
            let uri = Url::parse(&format!("file:///tmp/open-{index}.shl")).unwrap();
            admit_document(&mut docs, uri, pending_document("x".into(), 1)).unwrap();
        }
        let first = Url::parse("file:///tmp/open-0.shl").unwrap();
        let rejected = Url::parse("file:///tmp/rejected.shl").unwrap();
        assert!(
            admit_document(
                &mut docs,
                rejected.clone(),
                pending_document("new".into(), 1)
            )
            .is_err()
        );
        assert_eq!(docs.len(), MAX_OPEN_DOCUMENTS);
        assert!(docs.contains_key(&first));
        assert!(!docs.contains_key(&rejected));

        // Reopening the same identity is a replacement, not count growth.
        admit_document(
            &mut docs,
            first.clone(),
            pending_document("replacement".into(), 2),
        )
        .unwrap();
        assert_eq!(docs[&first].text, "replacement");
    }

    #[test]
    fn document_admission_rejects_oversize_source_without_mutating_baseline() {
        let uri = Url::parse("file:///tmp/bounded.shl").unwrap();
        let mut docs = HashMap::new();
        admit_document(
            &mut docs,
            uri.clone(),
            pending_document("let safe = 1".into(), 1),
        )
        .unwrap();
        let error = admit_document(
            &mut docs,
            uri.clone(),
            pending_document("x".repeat(MAX_DOCUMENT_BYTES + 1), 2),
        )
        .unwrap_err();
        assert!(error.contains("per open document"));
        assert_eq!(docs[&uri].text, "let safe = 1");
        assert_eq!(docs[&uri].version, 1);
    }

    #[test]
    fn document_admission_enforces_aggregate_source_and_uri_budgets() {
        let mut docs = HashMap::new();
        for index in 0..(MAX_OPEN_SOURCE_BYTES / MAX_DOCUMENT_BYTES) {
            admit_document(
                &mut docs,
                Url::parse(&format!("file:///tmp/full-{index}.shl")).unwrap(),
                pending_document("x".repeat(MAX_DOCUMENT_BYTES), 1),
            )
            .unwrap();
        }
        let retained = docs.len();
        assert!(
            admit_document(
                &mut docs,
                Url::parse("file:///tmp/over-total.shl").unwrap(),
                pending_document("x".into(), 1),
            )
            .unwrap_err()
            .contains("retained-source budget")
        );
        assert_eq!(docs.len(), retained);

        let huge_uri = Url::parse(&format!(
            "file:///tmp/{}.shl",
            "u".repeat(MAX_DOCUMENT_URI_BYTES)
        ))
        .unwrap();
        assert!(
            admit_document(
                &mut HashMap::new(),
                huge_uri,
                pending_document("x".into(), 1)
            )
            .unwrap_err()
            .contains("document URI")
        );
    }

    #[test]
    fn completion_replaces_the_whole_identifier_at_cursor() {
        let text = "let deployment = dep_suffix";
        let cursor = text.find("_suffix").unwrap();
        let (start, end) = word_bounds_at(text, cursor);
        assert_eq!(&text[start..end], "dep_suffix");
        assert_eq!(byte_to_position(text, start), Position::new(0, 17));
    }

    #[test]
    fn use_definition_resolves_paths_and_exported_members() {
        let dir = unique_temp_dir("definition");
        let source_path = dir.join("main.shl");
        let module_path = dir.join("tools.shl");
        let source_text = "use ./tools\ntools.build()";
        let module_text = "export fn build() {}\nfn private() {}";
        std::fs::write(&source_path, source_text).unwrap();
        std::fs::write(&module_path, module_text).unwrap();
        let source_uri = Url::from_file_path(&source_path).unwrap();
        let source = analyze_document(&source_uri, source_text.into(), 1);
        let docs = HashMap::new();

        let member_pos = source_text.rfind("build").unwrap() + 2;
        let member = definition_in_used_module(
            &source_uri,
            &source,
            member_pos,
            word_at(source_text, member_pos),
            &docs,
        )
        .unwrap();
        assert_eq!(
            member.uri.to_file_path().unwrap(),
            module_path.canonicalize().unwrap()
        );
        assert_eq!(member.range.start, Position::new(0, 10));

        let path_pos = source_text.find("tools").unwrap() + 2;
        let module = definition_in_used_module(
            &source_uri,
            &source,
            path_pos,
            word_at(source_text, path_pos),
            &docs,
        )
        .unwrap();
        assert_eq!(module.range.start, Position::new(0, 0));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unopened_definition_target_reads_are_source_bounded() {
        let dir = unique_temp_dir("bounded-definition");
        let module_path = dir.join("huge.shl");
        std::fs::write(&module_path, vec![b'x'; MAX_DOCUMENT_BYTES + 1]).unwrap();
        assert!(document::read_source_bounded(&module_path).is_none());
        std::fs::write(&module_path, "export fn bounded() {}").unwrap();
        assert_eq!(
            document::read_source_bounded(&module_path).as_deref(),
            Some("export fn bounded() {}")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn planner_analysis_has_no_filesystem_side_effects() {
        let dir = unique_temp_dir("planner-purity");
        let keep = dir.join("keep");
        std::fs::write(&keep, "sentinel").unwrap();
        let uri = Url::from_file_path(dir.join("main.shl")).unwrap();
        let before = std::fs::read_dir(&dir).unwrap().count();

        let _state = analyze_document(&uri, "rm keep\ntouch created".into(), 1);

        assert_eq!(std::fs::read_to_string(&keep).unwrap(), "sentinel");
        assert!(!dir.join("created").exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn destructuring_patterns_expose_each_binding() {
        let text = "match [1, 2] { [first, ...rest] => first }\n\
                    match {name: \"n\"} { {name} => name }";
        let ast = shoal_syntax::parse(text).unwrap();
        let names = collect_symbols(&ast, text)
            .into_iter()
            .map(|symbol| {
                let declared =
                    text[symbol.span.start as usize..symbol.span.end as usize].to_string();
                (symbol.name, declared)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("first".into(), "first".into()),
                ("rest".into(), "rest".into()),
                ("name".into(), "name".into())
            ]
        );
    }
}
