//! Session completion vocabulary and scoped-name discovery.

/// Completion candidates at a cursor byte offset (the kernel `complete`
/// method). Keywords/builtins plus any `let`/`var`/`fn`/`alias` names declared
/// before the cursor, filtered by the partial word under the cursor.
pub(super) fn complete_at(src: &str, cursor: usize) -> Vec<String> {
    const WORDS: &[&str] = &[
        "let", "var", "fn", "alias", "use", "export", "return", "break", "continue", "if", "else",
        "match", "for", "in", "while", "try", "catch", "true", "false", "null", "spawn", "with",
        "sh", "ls", "cd", "pwd", "cp", "mv", "rm", "mkdir", "cat", "echo", "run", "parallel",
        "pick", "interact", "explain",
    ];
    let before = &src[..cursor];
    // The partial identifier immediately left of the cursor.
    let start = before
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let partial = &before[start..];
    let mut names: Vec<String> = WORDS.iter().map(|s| s.to_string()).collect();
    // Declarations already in scope (`let x`, `fn y`, …).
    let toks: Vec<&str> = before
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|s| !s.is_empty())
        .collect();
    for pair in toks.windows(2) {
        if matches!(pair[0], "let" | "var" | "fn" | "alias") {
            names.push(pair[1].to_string());
        }
    }
    names.retain(|n| n.starts_with(partial));
    names.sort();
    names.dedup();
    names
}
