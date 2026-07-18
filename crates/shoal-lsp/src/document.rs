//! Document snapshots, incremental edits, diagnostics, completion, and navigation helpers.

use super::*;
use std::io::Read;

pub(super) fn symbol_facts(symbol: Option<&Symbol>) -> CommandFacts {
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

pub(super) fn definition_symbol<'a>(
    symbols: &'a [Symbol],
    word: &str,
    pos: usize,
) -> Option<&'a Symbol> {
    symbols
        .iter()
        .rev()
        .find(|symbol| symbol.name == word && symbol_visible_at(symbol, pos))
}

pub(super) fn symbol_visible_at(symbol: &Symbol, pos: usize) -> bool {
    (symbol.visible_from <= pos
        || (symbol.span.start as usize <= pos && pos <= symbol.span.end as usize))
        && symbol.scope.start as usize <= pos
        && pos <= symbol.scope.end as usize
}

pub(super) fn analysis_is_current(current: &DocumentState, analyzed: &DocumentState) -> bool {
    current.version == analyzed.version && current.text == analyzed.text
}

pub(super) fn pending_document(text: String, version: i32) -> DocumentState {
    DocumentState {
        text,
        version,
        ast: None,
        diagnostics: Vec::new(),
        symbols: Vec::new(),
    }
}

pub(super) fn apply_content_changes(
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

pub(super) fn position_to_byte_strict(s: &str, p: Position) -> Option<usize> {
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

pub(super) fn resolve_module_path(source_uri: &Url, module: &str) -> Option<std::path::PathBuf> {
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

pub(super) fn use_path_span(text: &str, stmt_span: Span, path: &str) -> Option<Span> {
    let start = stmt_span.start as usize;
    let end = (stmt_span.end as usize).min(text.len());
    text.get(start..end)?
        .rfind(path)
        .map(|offset| Span::new(start + offset, start + offset + path.len()))
}

pub(super) fn qualified_name_at(text: &str, pos: usize) -> Option<(&str, &str)> {
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

pub(super) fn exported_symbol(program: &Program, text: &str, name: &str) -> Option<Span> {
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

pub(super) fn definition_in_used_module(
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
            let text = read_source_bounded(&target_path)?;
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

/// Read unopened navigation targets under the same per-document source cap as
/// editor-provided text. `take` closes the metadata/read race: a file that
/// grows after open is still sampled by at most one byte over the ceiling.
pub(super) fn read_source_bounded(path: &std::path::Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = file.take(MAX_DOCUMENT_BYTES as u64 + 1);
    let mut text = String::new();
    reader.read_to_string(&mut text).ok()?;
    (text.len() <= MAX_DOCUMENT_BYTES).then_some(text)
}

pub(super) fn completion_kind(label: &str) -> CompletionItemKind {
    if is_namespace_method(label) {
        return CompletionItemKind::FUNCTION;
    }
    match resolve_command_source(label, CommandFacts::default()) {
        CommandSource::StructuredBuiltin | CommandSource::SpecialBuiltin => {
            CompletionItemKind::FUNCTION
        }
        _ => CompletionItemKind::KEYWORD,
    }
}

pub(super) fn is_namespace_method(label: &str) -> bool {
    shoal_eval::all_namespace_method_names().any(|method| method == label)
}

pub(super) fn is_keyword(label: &str) -> bool {
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
pub(super) fn base_vocabulary() -> impl Iterator<Item = &'static str> {
    shoal_syntax::lexer::RESERVED
        .iter()
        .chain(EXTRA_KEYWORDS)
        .chain(shoal_syntax::commands::builtin_names())
        .copied()
        .chain(shoal_eval::all_namespace_method_names())
}
pub(super) fn help(w: &str) -> Option<&'static str> {
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
pub(super) fn word_at(s: &str, pos: usize) -> &str {
    let (a, b) = word_bounds_at(s, pos);
    &s[a..b]
}

pub(super) fn word_bounds_at(s: &str, pos: usize) -> (usize, usize) {
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
pub(super) fn span_range(text: &str, span: Span) -> Range {
    Range::new(
        byte_to_position(text, span.start as usize),
        byte_to_position(text, span.end as usize),
    )
}

pub(super) fn analyze_document(uri: &Url, text: String, version: i32) -> DocumentState {
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
    diagnostics.truncate(MAX_DIAGNOSTICS);
    for diagnostic in &mut diagnostics {
        if diagnostic.message.len() > 4 * 1024 {
            let mut end = 4 * 1024;
            while !diagnostic.message.is_char_boundary(end) {
                end -= 1;
            }
            diagnostic.message.truncate(end);
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

pub(super) fn parse_diagnostic(text: &str, e: shoal_syntax::ParseError) -> Diagnostic {
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
