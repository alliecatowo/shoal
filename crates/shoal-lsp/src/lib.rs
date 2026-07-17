use shoal_syntax::commands::{CommandFacts, CommandSource, resolve_command_source};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use tower_lsp::{Client, LanguageServer, jsonrpc::Result, lsp_types::*};

pub struct Backend {
    client: Client,
    docs: Arc<RwLock<HashMap<Url, String>>>,
}
impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            docs: Default::default(),
        }
    }
    async fn publish(&self, uri: Url, text: &str) {
        self.docs.write().await.insert(uri.clone(), text.into());
        let diagnostics = diagnostics(text);
        self.client
            .publish_diagnostics(uri, diagnostics, None)
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
        self.publish(p.text_document.uri, &p.text_document.text)
            .await;
    }
    async fn did_change(&self, p: DidChangeTextDocumentParams) {
        if let Some(c) = p.content_changes.into_iter().last() {
            self.publish(p.text_document.uri, &c.text).await;
        }
    }
    async fn did_close(&self, p: DidCloseTextDocumentParams) {
        self.docs.write().await.remove(&p.text_document.uri);
        self.client
            .publish_diagnostics(p.text_document.uri, vec![], None)
            .await;
    }
    async fn formatting(&self, p: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let docs = self.docs.read().await;
        let Some(text) = docs.get(&p.text_document.uri) else {
            return Ok(None);
        };
        let shoal_syntax::ParseStatus::Complete(ast) = shoal_syntax::parse_status(text) else {
            return Ok(None);
        };
        let end = byte_to_position(text, text.len());
        Ok(Some(vec![TextEdit {
            range: Range::new(Position::new(0, 0), end),
            new_text: shoal_syntax::format_program(&ast),
        }]))
    }
    async fn completion(&self, p: CompletionParams) -> Result<Option<CompletionResponse>> {
        let docs = self.docs.read().await;
        let Some(text) = docs.get(&p.text_document_position.text_document.uri) else {
            return Ok(None);
        };
        let byte = position_to_byte(text, p.text_document_position.position);
        // Completion vocabulary = language keywords ∪ builtin command heads
        // (see `base_vocabulary`) ∪ lexical declarations seen so far.
        let mut names: Vec<String> = base_vocabulary().map(str::to_string).collect();
        names.extend(declarations_before(&text[..byte]));
        names.sort();
        names.dedup();
        Ok(Some(CompletionResponse::Array(
            names
                .into_iter()
                .map(|label| CompletionItem {
                    kind: Some(completion_kind(&label)),
                    label,
                    ..Default::default()
                })
                .collect(),
        )))
    }
    async fn hover(&self, p: HoverParams) -> Result<Option<Hover>> {
        let docs = self.docs.read().await;
        let Some(text) = docs.get(&p.text_document_position_params.text_document.uri) else {
            return Ok(None);
        };
        let pos = position_to_byte(text, p.text_document_position_params.position);
        let word = word_at(text, pos);
        let Some(help) = help(word) else {
            return Ok(None);
        };
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: help.into(),
            }),
            range: None,
        }))
    }
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
fn declarations_before(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let toks = s
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>();
    for pair in toks.windows(2) {
        if matches!(pair[0], "let" | "var" | "fn" | "alias") {
            out.push(pair[1].into())
        }
    }
    out
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
    let b = byte.min(s.len());
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
fn diagnostics(text: &str) -> Vec<Diagnostic> {
    let e = match shoal_syntax::parse_status(text) {
        shoal_syntax::ParseStatus::Complete(_) => return vec![],
        shoal_syntax::ParseStatus::Incomplete(e) | shoal_syntax::ParseStatus::Error(e) => e,
    };
    vec![Diagnostic {
        range: Range::new(
            byte_to_position(text, e.span.start as usize),
            byte_to_position(text, e.span.end as usize),
        ),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("parse_error".into())),
        source: Some("shoal".into()),
        message: e.msg,
        ..Default::default()
    }]
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
        assert_eq!(
            declarations_before("let alpha = 1\nfn beta() {}"),
            vec!["alpha", "beta"]
        )
    }
    #[test]
    fn incomplete_diagnostic() {
        assert_eq!(diagnostics("let x = [1,").len(), 1)
    }
}
