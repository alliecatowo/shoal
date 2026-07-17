mod analysis;

use analysis::{Symbol, collect_symbols};
use shoal_ast::{Program, Span};
use shoal_syntax::commands::{CommandFacts, CommandSource, resolve_command_source};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, RwLock};
use tower_lsp::{Client, LanguageServer, jsonrpc::Result, lsp_types::*};

pub struct Backend {
    client: Client,
    docs: Arc<RwLock<HashMap<Url, DocumentState>>>,
    updates: Arc<Mutex<()>>,
}

#[derive(Clone)]
struct DocumentState {
    text: String,
    version: i32,
    ast: Option<Program>,
    diagnostics: Vec<Diagnostic>,
    symbols: Vec<Symbol>,
}
impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            docs: Default::default(),
            updates: Default::default(),
        }
    }
    async fn publish(&self, uri: Url, text: String, version: i32) {
        let _update = self.updates.lock().await;
        if self
            .docs
            .read()
            .await
            .get(&uri)
            .is_some_and(|current| current.version > version)
        {
            return;
        }
        let analyze_uri = uri.clone();
        let fallback_text = text.clone();
        let state =
            tokio::task::spawn_blocking(move || analyze_document(&analyze_uri, text, version))
                .await
                .unwrap_or_else(|_| analyze_document(&uri, fallback_text, version));
        let diagnostics = state.diagnostics.clone();
        self.docs.write().await.insert(uri.clone(), state);
        self.client
            .publish_diagnostics(uri, diagnostics, Some(version))
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
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
        self.publish(
            p.text_document.uri,
            p.text_document.text,
            p.text_document.version,
        )
        .await;
    }
    async fn did_change(&self, p: DidChangeTextDocumentParams) {
        if let Some(c) = p.content_changes.into_iter().last() {
            self.publish(p.text_document.uri, c.text, p.text_document.version)
                .await;
        }
    }
    async fn did_close(&self, p: DidCloseTextDocumentParams) {
        let _update = self.updates.lock().await;
        self.docs.write().await.remove(&p.text_document.uri);
        self.client
            .publish_diagnostics(p.text_document.uri, vec![], None)
            .await;
    }
    async fn formatting(&self, p: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let docs = self.docs.read().await;
        let Some(doc) = docs.get(&p.text_document.uri) else {
            return Ok(None);
        };
        let Some(ast) = &doc.ast else {
            return Ok(None);
        };
        let end = byte_to_position(&doc.text, doc.text.len());
        Ok(Some(vec![TextEdit {
            range: Range::new(Position::new(0, 0), end),
            new_text: shoal_syntax::format_program(ast),
        }]))
    }
    async fn completion(&self, p: CompletionParams) -> Result<Option<CompletionResponse>> {
        let docs = self.docs.read().await;
        let Some(doc) = docs.get(&p.text_document_position.text_document.uri) else {
            return Ok(None);
        };
        let byte = position_to_byte(&doc.text, p.text_document_position.position);
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
                    CompletionItem {
                        kind: Some(
                            symbol.map_or_else(|| completion_kind(&label), Symbol::completion_kind),
                        ),
                        detail: Some(format!("{} — {}", source.as_str(), source.reason())),
                        documentation: symbol
                            .and_then(|symbol| symbol.doc.clone().map(Documentation::String)),
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
        let Some(symbol) = definition_symbol(&doc.symbols, word, pos) else {
            return Ok(None);
        };
        Ok(Some(GotoDefinitionResponse::Scalar(Location::new(
            p.text_document_position_params.text_document.uri,
            span_range(&doc.text, symbol.span),
        ))))
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

fn symbol_facts(symbol: Option<&Symbol>) -> CommandFacts {
    use analysis::SymbolFlavor;
    CommandFacts {
        session_callable: symbol.is_some_and(|symbol| {
            matches!(symbol.flavor, SymbolFlavor::Function | SymbolFlavor::Alias)
        }),
        session_value: symbol.is_some(),
        value_eligible: true,
        ..Default::default()
    }
}

fn definition_symbol<'a>(symbols: &'a [Symbol], word: &str, pos: usize) -> Option<&'a Symbol> {
    symbols
        .iter()
        .rev()
        .find(|symbol| symbol.name == word && symbol_visible_at(symbol, pos))
}

fn symbol_visible_at(symbol: &Symbol, pos: usize) -> bool {
    symbol.span.start as usize <= pos
        && symbol.scope.start as usize <= pos
        && pos <= symbol.scope.end as usize
}

fn completion_kind(label: &str) -> CompletionItemKind {
    match resolve_command_source(label, CommandFacts::default()) {
        CommandSource::StructuredBuiltin | CommandSource::SpecialBuiltin => {
            CompletionItemKind::FUNCTION
        }
        _ => CompletionItemKind::KEYWORD,
    }
}

/// Language keywords the parser special-cases beyond `shoal_syntax::lexer::
/// RESERVED`: `with`/`spawn` (statement forms) and the `sh { }` verbatim escape
/// hatch. Builtin *command* heads are NOT listed here — they come from the
/// canonical [`builtin_names`](shoal_syntax::commands::builtin_names) registry so
/// this file can't drift from the evaluator's own command-head vocabulary.
const EXTRA_KEYWORDS: &[&str] = &["with", "spawn", "sh"];

/// The static completion vocabulary: language keywords (the parser's reserved
/// set plus [`EXTRA_KEYWORDS`]) ∪ builtin command heads (the canonical
/// [`builtin_names`](shoal_syntax::commands::builtin_names) registry, which lives
/// in the leaf `shoal-syntax` crate and is the same list the evaluator dispatches
/// on). Document-local declarations are layered on top per-request in
/// `completion`.
fn base_vocabulary() -> impl Iterator<Item = &'static str> {
    shoal_syntax::lexer::RESERVED
        .iter()
        .chain(EXTRA_KEYWORDS)
        .chain(shoal_syntax::commands::builtin_names())
        .copied()
}
fn help(w: &str) -> Option<&'static str> {
    Some(match w {
        "let" => "`let name = expr` creates an immutable lexical binding.",
        "var" => "`var name = expr` creates a mutable lexical binding.",
        "fn" => "`fn name(params) { ... }` defines a typed command/function.",
        "match" => {
            "`match value { pattern => expr }` performs pattern matching; `|` alternates patterns."
        }
        "with" => "`with cwd: path, env: {...} { ... }` scopes ambient execution state.",
        "spawn" => "`spawn { ... }` starts a structured background task.",
        "sh" => "`sh { ... }` is the explicit verbatim POSIX escape hatch.",
        "it" => "`it` is the current client's last interactive value.",
        _ => return None,
    })
}
fn word_at(s: &str, pos: usize) -> &str {
    let p = pos.min(s.len());
    let mut a = p;
    while a > 0 && (s.as_bytes()[a - 1].is_ascii_alphanumeric() || s.as_bytes()[a - 1] == b'_') {
        a -= 1
    }
    let mut b = p;
    while b < s.len() && (s.as_bytes()[b].is_ascii_alphanumeric() || s.as_bytes()[b] == b'_') {
        b += 1
    }
    &s[a..b]
}
pub fn byte_to_position(s: &str, byte: usize) -> Position {
    let mut b = byte.min(s.len());
    while !s.is_char_boundary(b) {
        b -= 1;
    }
    let before = &s[..b];
    let line = before.bytes().filter(|x| *x == b'\n').count() as u32;
    let tail = before.rsplit_once('\n').map_or(before, |(_, x)| x);
    Position::new(line, tail.encode_utf16().count() as u32)
}
pub fn position_to_byte(s: &str, p: Position) -> usize {
    let mut start = 0;
    for _ in 0..p.line {
        let Some(i) = s[start..].find('\n') else {
            return s.len();
        };
        start += i + 1
    }
    let line = &s[start..s[start..].find('\n').map_or(s.len(), |i| start + i)];
    let mut units = 0;
    for (cidx, c) in line.char_indices() {
        if units >= p.character {
            return start + cidx;
        }
        units += c.len_utf16() as u32
    }
    start + line.len()
}
fn span_range(text: &str, span: Span) -> Range {
    Range::new(
        byte_to_position(text, span.start as usize),
        byte_to_position(text, span.end as usize),
    )
}

fn analyze_document(uri: &Url, text: String, version: i32) -> DocumentState {
    let (ast, mut diagnostics) = match shoal_syntax::parse_status(&text) {
        shoal_syntax::ParseStatus::Complete(ast) => (Some(ast), Vec::new()),
        shoal_syntax::ParseStatus::Incomplete(error) | shoal_syntax::ParseStatus::Error(error) => {
            (None, vec![parse_diagnostic(&text, error)])
        }
    };
    let symbols = ast
        .as_ref()
        .map_or_else(Vec::new, |ast| collect_symbols(ast, &text));
    if let Some(ast) = &ast {
        let cwd = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(std::env::temp_dir);
        let mut evaluator = shoal_eval::Evaluator::new(cwd);
        match evaluator.plan_program(ast) {
            Ok(plan) if plan.effects.contains(&shoal_leash::Effect::Opaque) => {
                diagnostics.push(Diagnostic {
                    range: Range::new(Position::new(0, 0), byte_to_position(&text, text.len())),
                    severity: Some(DiagnosticSeverity::WARNING),
                    code: Some(NumberOrString::String("opaque_effect".into())),
                    source: Some("shoal-planner".into()),
                    message: "planner cannot statically classify every effect in this document"
                        .into(),
                    ..Default::default()
                });
            }
            Err(error) => diagnostics.push(Diagnostic {
                range: error.span.map_or_else(
                    || Range::new(Position::new(0, 0), Position::new(0, 0)),
                    |span| span_range(&text, span),
                ),
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String(error.code)),
                source: Some("shoal-planner".into()),
                message: error.msg,
                ..Default::default()
            }),
            Ok(_) => {}
        }
    }
    DocumentState {
        text,
        version,
        ast,
        diagnostics,
        symbols,
    }
}

fn parse_diagnostic(text: &str, e: shoal_syntax::ParseError) -> Diagnostic {
    Diagnostic {
        range: Range::new(
            byte_to_position(text, e.span.start as usize),
            byte_to_position(text, e.span.end as usize),
        ),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("parse_error".into())),
        source: Some("shoal".into()),
        message: e.msg,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
