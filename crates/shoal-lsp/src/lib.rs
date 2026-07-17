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
    async fn analyze_and_publish(&self, uri: Url, text: String, version: i32) {
        let analyze_uri = uri.clone();
        let fallback_text = text.clone();
        let state =
            tokio::task::spawn_blocking(move || analyze_document(&analyze_uri, text, version))
                .await
                .unwrap_or_else(|_| analyze_document(&uri, fallback_text, version));
        let _update = self.updates.lock().await;
        let current = self.docs.read().await.get(&uri).cloned();
        if !current
            .as_ref()
            .is_some_and(|current| analysis_is_current(current, &state))
        {
            return;
        }
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
        {
            let _update = self.updates.lock().await;
            self.docs
                .write()
                .await
                .insert(uri.clone(), pending_document(text.clone(), version));
        }
        self.analyze_and_publish(uri, text, version).await;
    }
    async fn did_change(&self, p: DidChangeTextDocumentParams) {
        let uri = p.text_document.uri;
        let version = p.text_document.version;
        let staged = {
            let _update = self.updates.lock().await;
            let docs = self.docs.read().await;
            let Some(current) = docs.get(&uri) else {
                return;
            };
            if version <= current.version {
                return;
            }
            let next = apply_content_changes(&current.text, &p.content_changes);
            drop(docs);
            if let Ok(next) = &next {
                self.docs
                    .write()
                    .await
                    .insert(uri.clone(), pending_document(next.clone(), version));
            }
            next
        };
        let next_text = match staged {
            Ok(text) => text,
            Err(message) => {
                self.client.log_message(MessageType::WARNING, message).await;
                return;
            }
        };
        self.analyze_and_publish(uri, next_text, version).await;
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
    (symbol.visible_from <= pos
        || (symbol.span.start as usize <= pos && pos <= symbol.span.end as usize))
        && symbol.scope.start as usize <= pos
        && pos <= symbol.scope.end as usize
}

fn analysis_is_current(current: &DocumentState, analyzed: &DocumentState) -> bool {
    current.version == analyzed.version && current.text == analyzed.text
}

fn pending_document(text: String, version: i32) -> DocumentState {
    DocumentState {
        text,
        version,
        ast: None,
        diagnostics: Vec::new(),
        symbols: Vec::new(),
    }
}

fn apply_content_changes(
    original: &str,
    changes: &[TextDocumentContentChangeEvent],
) -> std::result::Result<String, String> {
    let mut text = original.to_string();
    for change in changes {
        let Some(range) = change.range else {
            text = change.text.clone();
            continue;
        };
        let start = position_to_byte_strict(&text, range.start)
            .ok_or_else(|| format!("invalid incremental edit start: {:?}", range.start))?;
        let end = position_to_byte_strict(&text, range.end)
            .ok_or_else(|| format!("invalid incremental edit end: {:?}", range.end))?;
        if start > end {
            return Err("incremental edit range is reversed".into());
        }
        if let Some(expected) = change.range_length {
            let actual = text[start..end].encode_utf16().count() as u32;
            if actual != expected {
                return Err(format!(
                    "incremental edit range_length mismatch: client={expected}, actual={actual}"
                ));
            }
        }
        text.replace_range(start..end, &change.text);
    }
    Ok(text)
}

fn position_to_byte_strict(s: &str, p: Position) -> Option<usize> {
    let mut start = 0usize;
    for _ in 0..p.line {
        let offset = s[start..].find('\n')?;
        start += offset + 1;
    }
    let line_end = s[start..].find('\n').map_or(s.len(), |i| start + i);
    let line = &s[start..line_end];
    let mut units = 0u32;
    for (offset, ch) in line.char_indices() {
        if units == p.character {
            return Some(start + offset);
        }
        let next = units + ch.len_utf16() as u32;
        if p.character < next {
            return None;
        }
        units = next;
    }
    (units == p.character).then_some(line_end)
}

fn resolve_module_path(source_uri: &Url, module: &str) -> Option<std::path::PathBuf> {
    let source = source_uri.to_file_path().ok()?;
    let module = std::path::PathBuf::from(module);
    let base = if module.is_absolute() {
        module
    } else {
        source.parent()?.join(module)
    };
    let candidates = if base.extension().is_some() {
        vec![base]
    } else {
        vec![base.with_extension("shl"), base]
    };
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .and_then(|path| path.canonicalize().ok())
}

fn use_path_span(text: &str, stmt_span: Span, path: &str) -> Option<Span> {
    let start = stmt_span.start as usize;
    let end = (stmt_span.end as usize).min(text.len());
    text.get(start..end)?
        .rfind(path)
        .map(|offset| Span::new(start + offset, start + offset + path.len()))
}

fn qualified_name_at(text: &str, pos: usize) -> Option<(&str, &str)> {
    let (member_start, member_end) = word_bounds_at(text, pos);
    if member_start == 0 || text.as_bytes().get(member_start - 1) != Some(&b'.') {
        return None;
    }
    let qualifier_end = member_start - 1;
    let (qualifier_start, _) = word_bounds_at(text, qualifier_end);
    let qualifier = text.get(qualifier_start..qualifier_end)?;
    let member = text.get(member_start..member_end)?;
    (!qualifier.is_empty() && !member.is_empty()).then_some((qualifier, member))
}

fn exported_symbol(program: &Program, text: &str, name: &str) -> Option<Span> {
    for stmt in &program.stmts {
        match stmt {
            shoal_ast::Stmt::Fn { decl } if decl.exported && decl.name == name => {
                return collect_symbols(program, text)
                    .into_iter()
                    .find(|symbol| symbol.name == name && symbol.span.start >= decl.span.start)
                    .map(|symbol| symbol.span);
            }
            shoal_ast::Stmt::Let {
                exported: true,
                span,
                ..
            } => {
                if let Some(symbol) = collect_symbols(program, text).into_iter().find(|symbol| {
                    symbol.name == name
                        && symbol.span.start >= span.start
                        && symbol.span.end <= span.end
                }) {
                    return Some(symbol.span);
                }
            }
            _ => {}
        }
    }
    None
}

fn definition_in_used_module(
    source_uri: &Url,
    source: &DocumentState,
    pos: usize,
    word: &str,
    docs: &HashMap<Url, DocumentState>,
) -> Option<Location> {
    let program = source.ast.as_ref()?;
    let qualified = qualified_name_at(&source.text, pos);
    for stmt in &program.stmts {
        let shoal_ast::Stmt::Use { path, span } = stmt else {
            continue;
        };
        let path_span = use_path_span(&source.text, *span, path)?;
        let stem = std::path::Path::new(path).file_stem()?.to_str()?;
        let on_path = path_span.start as usize <= pos && pos <= path_span.end as usize;
        let on_module_name = word == stem;
        let member = qualified
            .filter(|(qualifier, _)| *qualifier == stem)
            .map(|(_, member)| member);
        if !on_path && !on_module_name && member.is_none() {
            continue;
        }
        let target_path = resolve_module_path(source_uri, path)?;
        let target_uri = Url::from_file_path(&target_path).ok()?;
        let (target_text, target_ast) = if let Some(target) = docs.get(&target_uri) {
            (target.text.clone(), target.ast.clone())
        } else {
            let text = std::fs::read_to_string(&target_path).ok()?;
            let ast = shoal_syntax::parse(&text).ok();
            (text, ast)
        };
        let range = match member {
            Some(name) => span_range(
                &target_text,
                exported_symbol(target_ast.as_ref()?, &target_text, name)?,
            ),
            None => Range::new(Position::new(0, 0), Position::new(0, 0)),
        };
        return Some(Location::new(target_uri, range));
    }
    None
}

fn completion_kind(label: &str) -> CompletionItemKind {
    match resolve_command_source(label, CommandFacts::default()) {
        CommandSource::StructuredBuiltin | CommandSource::SpecialBuiltin => {
            CompletionItemKind::FUNCTION
        }
        _ => CompletionItemKind::KEYWORD,
    }
}

fn is_keyword(label: &str) -> bool {
    shoal_syntax::lexer::RESERVED.contains(&label) || EXTRA_KEYWORDS.contains(&label)
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
    let (a, b) = word_bounds_at(s, pos);
    &s[a..b]
}

fn word_bounds_at(s: &str, pos: usize) -> (usize, usize) {
    let mut p = pos.min(s.len());
    while !s.is_char_boundary(p) {
        p -= 1;
    }
    let mut a = p;
    while a > 0 && (s.as_bytes()[a - 1].is_ascii_alphanumeric() || s.as_bytes()[a - 1] == b'_') {
        a -= 1
    }
    let mut b = p;
    while b < s.len() && (s.as_bytes()[b].is_ascii_alphanumeric() || s.as_bytes()[b] == b'_') {
        b += 1
    }
    (a, b)
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
