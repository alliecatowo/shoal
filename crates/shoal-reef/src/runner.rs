//! Runners (site/content/internals/reef-resolution.md): content-type resolution keyed on file extension,
//! falling back to a shebang sniff. `resolve_runner` answers "how do I run
//! `./script.py`" by mapping the extension (or `#!` line) to a tool name plus an
//! argv template. The named tool is itself resolved through reef by the caller.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

/// A runner binding: the tool to invoke and a leading argv template inserted
/// before the script path (`{ tool: "deno", args: ["run"] }` → `deno run x.ts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub tool: String,
    pub args_template: Vec<String>,
}

impl Invocation {
    pub fn tool(tool: impl Into<String>) -> Invocation {
        Invocation {
            tool: tool.into(),
            args_template: Vec::new(),
        }
    }
}

/// A map of extension → [`Invocation`]. Scopes merge with nearest-wins via
/// [`RunnerTable::overlay`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunnerTable {
    map: BTreeMap<String, Invocation>,
}

impl RunnerTable {
    /// An empty table (no defaults).
    pub fn empty() -> RunnerTable {
        RunnerTable {
            map: BTreeMap::new(),
        }
    }

    /// The default runner table shipped with reef: `py js ts sh shl rb lua`.
    /// `rs` intentionally has **no default** (compile-vs-script ambiguity).
    pub fn defaults() -> RunnerTable {
        let mut t = RunnerTable::empty();
        t.insert("py", Invocation::tool("python"));
        t.insert("js", Invocation::tool("node"));
        t.insert(
            "ts",
            Invocation {
                tool: "deno".into(),
                args_template: vec!["run".into()],
            },
        );
        t.insert("sh", Invocation::tool("sh"));
        t.insert("shl", Invocation::tool("self"));
        t.insert("rb", Invocation::tool("ruby"));
        t.insert("lua", Invocation::tool("lua"));
        t
    }

    pub fn insert(&mut self, ext: impl Into<String>, inv: Invocation) {
        let ext = ext.into();
        if self.map.contains_key(&ext) || self.map.len() < crate::input::REEF_MAX_RUNNERS {
            self.map.insert(ext, inv);
        }
    }

    pub fn get(&self, ext: &str) -> Option<&Invocation> {
        self.map.get(ext)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &Invocation)> {
        self.map.iter()
    }

    /// Overlay `higher` on top of `self`, with `higher` winning ties. Used to
    /// stack scope tables (nearest last so it wins).
    pub fn overlay(&mut self, higher: &RunnerTable) {
        for (k, v) in &higher.map {
            self.map.insert(k.clone(), v.clone());
        }
    }
}

/// Resolve the runner for a path. Tries the file extension first, then sniffs a
/// `#!` shebang and maps the interpreter's basename to a bare tool invocation.
///
/// Returns `None` when neither the extension nor a shebang is recognized. Reads
/// at most the first line of the file for the shebang sniff.
pub fn resolve_runner(path: &Path, table: &RunnerTable) -> Option<Invocation> {
    if let Some(ext) = path.extension().and_then(|e| e.to_str())
        && let Some(inv) = table.get(ext)
    {
        return Some(inv.clone());
    }
    sniff_shebang(path)
}

/// Read the first line of a file and, if it is a `#!` shebang, return an
/// invocation of the interpreter's basename (handling `/usr/bin/env tool`).
pub fn sniff_shebang(path: &Path) -> Option<Invocation> {
    let file = std::fs::File::open(path).ok()?;
    const MAX_SHEBANG_BYTES: usize = 8 * 1024;
    let mut bytes = Vec::with_capacity(256);
    file.take((MAX_SHEBANG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let end = bytes.iter().position(|byte| *byte == b'\n');
    if end.is_none() && bytes.len() > MAX_SHEBANG_BYTES {
        return None;
    }
    let line = std::str::from_utf8(&bytes[..end.unwrap_or(bytes.len())])
        .ok()?
        .trim_end();
    let rest = line.strip_prefix("#!")?.trim();
    if rest.is_empty() {
        return None;
    }
    let mut toks = rest.split_whitespace();
    let first = toks.next()?;
    let first_base = Path::new(first).file_name()?.to_str()?;
    // `#!/usr/bin/env python3` → the tool is the next token.
    let tool = if first_base == "env" {
        toks.next()?.to_string()
    } else {
        first_base.to_string()
    };
    Some(Invocation::tool(tool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn defaults_cover_expected_extensions() {
        let t = RunnerTable::defaults();
        assert_eq!(t.get("py").unwrap().tool, "python");
        assert_eq!(t.get("js").unwrap().tool, "node");
        assert_eq!(t.get("ts").unwrap().tool, "deno");
        assert_eq!(t.get("ts").unwrap().args_template, vec!["run".to_string()]);
        assert_eq!(t.get("sh").unwrap().tool, "sh");
        assert_eq!(t.get("shl").unwrap().tool, "self");
        assert_eq!(t.get("rb").unwrap().tool, "ruby");
        assert_eq!(t.get("lua").unwrap().tool, "lua");
    }

    #[test]
    fn no_default_for_rs() {
        assert!(RunnerTable::defaults().get("rs").is_none());
    }

    #[test]
    fn resolves_by_extension() {
        let t = RunnerTable::defaults();
        let inv = resolve_runner(Path::new("/x/script.py"), &t).unwrap();
        assert_eq!(inv.tool, "python");
    }

    #[test]
    fn falls_back_to_shebang() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("script");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "#!/usr/bin/env python3\nprint(1)").unwrap();
        let inv = resolve_runner(&p, &RunnerTable::defaults()).unwrap();
        assert_eq!(inv.tool, "python3");
    }

    #[test]
    fn shebang_direct_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("script");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "#!/bin/bash\necho hi").unwrap();
        let inv = sniff_shebang(&p).unwrap();
        assert_eq!(inv.tool, "bash");
    }

    #[test]
    fn shebang_first_line_is_bounded_and_binary_body_is_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("script");
        std::fs::write(&path, b"#!/usr/bin/env python\n\xff\xfe").unwrap();
        assert_eq!(sniff_shebang(&path).unwrap().tool, "python");
        std::fs::write(&path, format!("#!{}", "x".repeat(9 * 1024))).unwrap();
        assert!(sniff_shebang(&path).is_none());
    }

    #[test]
    fn extension_beats_shebang() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("thing.py");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "#!/bin/bash\n").unwrap();
        let inv = resolve_runner(&p, &RunnerTable::defaults()).unwrap();
        assert_eq!(inv.tool, "python", "extension wins over shebang");
    }

    #[test]
    fn unknown_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("thing.xyz");
        std::fs::write(&p, b"no shebang here").unwrap();
        assert!(resolve_runner(&p, &RunnerTable::defaults()).is_none());
    }

    #[test]
    fn overlay_nearest_wins() {
        let mut base = RunnerTable::defaults();
        let mut higher = RunnerTable::empty();
        higher.insert("py", Invocation::tool("python3.12"));
        base.overlay(&higher);
        assert_eq!(base.get("py").unwrap().tool, "python3.12");
    }
}
