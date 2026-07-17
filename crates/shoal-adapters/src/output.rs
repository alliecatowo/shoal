//! Structured-output decoders for declarative command adapters.

use shoal_value::{Record, Value, json_to_value};

pub fn parse_output(strategy: &str, bytes: &[u8], type_hint: Option<&str>) -> Option<Value> {
    match strategy {
        "none" => None,
        "json" => serde_json::from_slice(bytes)
            .ok()
            .map(|v| json_to_value(&v)),
        "ndjson" => parse_ndjson(bytes),
        "lines" => Some(Value::List(
            text(bytes)?
                .lines()
                .map(|s| Value::Str(s.trim_end_matches('\r').into()))
                .collect(),
        )),
        "kv" => parse_kv(bytes),
        "csv" => parse_delimited(bytes, b',', type_hint),
        "tsv" => parse_delimited(bytes, b'\t', type_hint),
        "z-records" => parse_z_records(bytes, type_hint),
        "porcelain-v2" => parse_porcelain_v2(bytes),
        "cols" => parse_cols(bytes, type_hint),
        "cols2" => parse_cols_n(bytes, type_hint, 2),
        "tsv-headerless" => parse_tsv_headerless(bytes, type_hint),
        _ => None,
    }
}

fn text(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok()
}

fn parse_ndjson(bytes: &[u8]) -> Option<Value> {
    let vals = text(bytes)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).ok().map(|v| json_to_value(&v)))
        .collect::<Option<Vec<_>>>()?;
    rows_or_list(vals)
}

fn parse_kv(bytes: &[u8]) -> Option<Value> {
    let mut r = Record::new();
    for line in text(bytes)?.lines().filter(|l| !l.trim().is_empty()) {
        let (k, v) = line.split_once('=').or_else(|| line.split_once(':'))?;
        r.insert(k.trim().into(), Value::Str(v.trim().into()));
    }
    Some(Value::Record(r))
}

fn parse_delimited(bytes: &[u8], delim: u8, hint: Option<&str>) -> Option<Value> {
    let rows = delimited_rows(bytes, delim)?;
    let (header, data) = rows.split_first()?;
    let mut out = Vec::new();
    for cells in data {
        if cells.len() != header.len() {
            return None;
        }
        let schema = hint_schema(hint);
        let mut record = Record::new();
        for (name, cell) in header.iter().zip(cells) {
            let ty = schema
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, ty)| ty.as_str())
                .unwrap_or("str");
            record.insert(name.clone(), coerce_cell(cell, ty)?);
        }
        out.push(record);
    }
    Some(Value::Table(out))
}

/// RFC4180-enough parser: quoted delimiters, doubled quotes, and embedded newlines.
fn delimited_rows(bytes: &[u8], delim: u8) -> Option<Vec<Vec<String>>> {
    let s = text(bytes)?;
    let b = s.as_bytes();
    let mut rows = vec![];
    let mut row = vec![];
    let mut field = String::new();
    let mut quoted = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' if quoted && b.get(i + 1) == Some(&b'"') => {
                field.push('"');
                i += 1;
            }
            b'"' => quoted = !quoted,
            x if x == delim && !quoted => {
                row.push(std::mem::take(&mut field));
            }
            b'\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            b'\r' if !quoted => {}
            _ => {
                let ch = s[i..].chars().next()?;
                field.push(ch);
                i += ch.len_utf8() - 1;
            }
        }
        i += 1;
    }
    if quoted {
        return None;
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Some(rows)
}

fn hint_schema(hint: Option<&str>) -> Vec<(String, String)> {
    let Some(body) = hint
        .and_then(|h| h.split_once("<{"))
        .and_then(|(_, x)| x.strip_suffix("}>"))
    else {
        return vec![];
    };
    body.split(',')
        .filter_map(|f| {
            f.split_once(':')
                .map(|(n, t)| (n.trim().into(), t.trim().trim_end_matches('?').into()))
        })
        .collect()
}

fn coerce_cell(raw: &str, ty: &str) -> Option<Value> {
    Some(match ty {
        "str" | "datetime" => Value::Str(raw.into()),
        "path" => Value::Path(raw.into()),
        "int" => Value::Int(raw.parse().ok()?),
        "float" => Value::Float(raw.parse().ok()?),
        "bool" => Value::Bool(raw.parse().ok()?),
        "size" => Value::Size(shoal_value::parse_size(raw)?),
        "size_kb" => Value::Size(parse_size_kb(raw)?),
        "duration" => Value::Duration(shoal_value::parse_duration(raw)?),
        "time" => Value::Time(shoal_value::parse_time(raw)?),
        _ => Value::Str(raw.into()),
    })
}

/// Parses a bare kilobyte count (no unit suffix, e.g. `du -k`/`df -kP`'s
/// digit-only `SIZE`/`USED`/`AVAIL` columns) into byte-scaled `Value::Size`.
///
/// This exists because `shoal_value::parse_size` requires an alphabetic unit
/// suffix (`b`/`kb`/`mb`/`kib`/...) and rejects a bare number outright -- but
/// `du -k`/`df -kP` are pinned specifically because they print bare digits
/// with NO suffix on both GNU and BSD, so `"size"` can never be used directly
/// for these columns. `size_kb` bridges that gap: the raw cell is already
/// known (by the adapter's own pinned `-k` invoke flag) to be counted in
/// 1024-byte blocks, so the column type declares that unit directly and this
/// does the fixed `* 1024` scaling in one place. Parses as `f64` (not
/// `u64`) so a cell that happens to carry a decimal (defensive, not
/// something real `-k` output produces) still degrades sensibly by rounding
/// instead of hard-failing the whole row/table the way `"int"` would on the
/// same input; a negative or non-numeric cell still degrades to `None`
/// (mismatch, not a lie -- site/content/internals/language-conformance-contract.md), same as every other typed column here.
fn parse_size_kb(raw: &str) -> Option<u64> {
    let kb: f64 = raw.parse().ok()?;
    if !kb.is_finite() || kb < 0.0 {
        return None;
    }
    Some((kb * 1024.0).round() as u64)
}

/// Parses whitespace-column tables from tools whose header line can't be
/// trusted to determine field count or names (e.g. `df`'s `Mounted on`
/// header is two words over one data column, and `ps`'s `%CPU`/`%MEM`
/// headers aren't stable identifiers across GNU/BSD `ps`). Unlike
/// `csv`/`tsv`, which look up each column by the *header text* found in the
/// bytes, `cols` always discards the first line as a header and takes
/// column identity and order entirely from the `output.type` hint
/// (positionally, like `z-records`) -- this is what lets a portable
/// `-o keyword=CustomHeader` invoke template stay decoupled from whatever
/// the underlying OS happens to print.
///
/// Each remaining line is split on runs of whitespace. A line with fewer
/// fields than the hint degrades the whole parse to `None` (mismatch, not a
/// lie). A line with *more* fields than the hint has its overflow merged
/// (space-joined) into the last column, so a last column that legitimately
/// contains embedded whitespace (a mount path with a space, a multi-word
/// process command) survives instead of desyncing every column after it.
fn parse_cols(bytes: &[u8], hint: Option<&str>) -> Option<Value> {
    parse_cols_n(bytes, hint, 1)
}

/// Generalization of `parse_cols` that discards a fixed number of leading
/// preamble lines (not just the always-exactly-one header `cols` assumes)
/// before every remaining line is treated as a data row. This exists for
/// tools like `vmstat(8)`, whose fixed two-line banner is a *category*
/// header ("procs -----------memory----------...") stacked directly above
/// the real per-column header line, with no flag on any common `vmstat`
/// build to suppress just one of the two (unlike `ps`/`df`, which have a
/// single, cleanly-discardable header line `cols` already handles, or `w
/// -h`, which happens to suppress its column-header line entirely and so
/// already works with plain `cols`'s "discard exactly one line" rule since
/// the remaining first line is the summary line `cols` treats as the
/// discarded header). Exposed as the separate `"cols2"` parser strategy
/// (rather than a parameterized field on `"cols"` in the TOML schema) so
/// this stays a pure addition: every existing `cols`-strategy adapter's
/// `SubSpec`/`parse_output` call sites are untouched, since `parse_output`'s
/// signature (consumed by `shoal-eval`, outside this crate) does not need to
/// grow a new parameter to carry a per-adapter skip count.
fn parse_cols_n(bytes: &[u8], hint: Option<&str>, skip_lines: usize) -> Option<Value> {
    let fields = hint_schema(hint);
    if fields.is_empty() {
        return None;
    }
    let body = text(bytes)?;
    let mut lines = body.lines();
    for _ in 0..skip_lines {
        lines.next(); // preamble/header row: discarded, never consulted for shape or names
    }
    let rows = lines
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < fields.len() {
                return None;
            }
            let last = if parts.len() > fields.len() {
                parts.split_off(fields.len() - 1).join(" ")
            } else {
                parts.pop()?.to_owned()
            };
            let mut r = Record::new();
            for (name, ty) in &fields[..fields.len() - 1] {
                r.insert(name.clone(), coerce_cell(parts.remove(0), ty)?);
            }
            let (last_name, last_ty) = &fields[fields.len() - 1];
            r.insert(last_name.clone(), coerce_cell(&last, last_ty)?);
            Some(r)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Value::Table(rows))
}

/// Parses tab-column data from tools whose output has **no header line at
/// all** -- unlike `cols` (which always discards a first line as a header)
/// and unlike `tsv`/`csv` (which read column names from a header line
/// found in the bytes themselves). `du -h`'s `<size>\t<path>` per line is
/// exactly this shape: every line is data, with no header row to discard or
/// to read names from. This is the genuine bug this parser exists to fix
/// (Real bug #2): `adapters/du.toml` used to declare `parse = "tsv"`,
/// which treats the very FIRST real `du` line as a header — silently
/// swallowing it as fake column names (e.g. an actual size like `"4.0K"`
/// and an actual path becoming the promised `size`/`path` keys' stand-ins)
/// instead of a genuine `{size, path}` row, and losing that row from the
/// table entirely. Column identity/order instead comes entirely from the
/// `output.type` hint (positionally, like `z-records`/`cols`), and the
/// delimiter is a literal tab -- not a whitespace run like `cols` uses --
/// so a last-field value containing ordinary spaces (a path with a space in
/// it) survives without needing `cols`'s overflow-merge trick. A line that
/// doesn't split into exactly the hint's field count degrades the whole
/// parse to `None` (mismatch, not a lie), same as `csv`/`tsv`'s exact
/// column-count check.
fn parse_tsv_headerless(bytes: &[u8], hint: Option<&str>) -> Option<Value> {
    let fields = hint_schema(hint);
    if fields.is_empty() {
        return None;
    }
    let body = text(bytes)?;
    let rows = body
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() != fields.len() {
                return None;
            }
            let mut r = Record::new();
            for ((name, ty), raw) in fields.iter().zip(&parts) {
                r.insert(name.clone(), coerce_cell(raw, ty)?);
            }
            Some(r)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Value::Table(rows))
}

fn parse_z_records(bytes: &[u8], hint: Option<&str>) -> Option<Value> {
    let fields = hint_schema(hint);
    if fields.is_empty() {
        return None;
    }
    // No output at all (e.g. `git log -z` on a path with no history) is a
    // valid, empty table, not a parse failure.
    if bytes.is_empty() {
        return Some(Value::Table(Vec::new()));
    }
    let mut cells = bytes.split(|b| *b == 0).collect::<Vec<_>>();
    // A stream of nothing but NUL bytes carries no field data at all: an empty
    // table, not a row of empty fields.
    if cells.iter().all(|x| x.is_empty()) {
        return Some(Value::Table(Vec::new()));
    }
    // A well-formed NUL-*terminated* stream of N complete records splits into
    // exactly ONE trailing empty cell (the final record terminator), giving
    // `N * fields + 1` cells. Pop only that single terminator -- and only when
    // it is genuinely a stray terminator (`len % fields == 1`), NEVER a
    // legitimately-empty FINAL field. The old loop popped *every* trailing
    // empty, so a real `git log ... -z` whose most-recent commit has an empty
    // subject (`...\0author\0date\0<empty subject>\0`) lost its trailing empty
    // field, breaking `len % fields` and degrading the whole table to raw
    // bytes. Popping exactly one keeps that `subject: ""` field intact. (A
    // stream with more than one stray trailing separator is genuinely
    // malformed and degrades to bytes below, per site/content/internals/language-conformance-contract.md "mismatch degrades to
    // bytes rather than lying" -- it can't be told apart from an empty final
    // field without lying about one of the two.)
    if cells.len() % fields.len() == 1 && cells.last().is_some_and(|x| x.is_empty()) {
        cells.pop();
    }
    if cells.is_empty() {
        return Some(Value::Table(Vec::new()));
    }
    if cells.len() % fields.len() != 0 {
        return None;
    }
    let mut rows = Vec::new();
    for chunk in cells.chunks(fields.len()) {
        let mut r = Record::new();
        for ((name, ty), raw) in fields.iter().zip(chunk) {
            r.insert(
                name.clone(),
                coerce_cell(std::str::from_utf8(raw).ok()?, ty)?,
            );
        }
        rows.push(r);
    }
    Some(Value::Table(rows))
}

/// Parses `git status --porcelain=v2` records (see git-status(1)):
///
///   `? <path>`                                                     untracked
///   `! <path>`                                                     ignored
///   `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`                  ordinary (9 fields)
///   `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\t<orig>` renamed/copied (10 fields)
///
/// Every shape is validated exactly, not assumed: a line that merely starts
/// with the right marker byte but doesn't match the rest of the shape
/// (including the *real* bug this guards against — short-format `"?? path"`
/// lines, which put a second marker character where porcelain v2 puts a
/// space, silently shifting the path slice by one byte) degrades the whole
/// parse to `None` instead of emitting a `path` with corrupted bytes baked
/// in. Unmerged (`u`) records and any other unrecognized non-comment line
/// degrade the same way, since this adapter does not model their shape and
/// silently dropping them would misrepresent the status as complete. Per
/// site/content/internals/language-conformance-contract.md: "mismatch degrades to bytes + warning rather than lying."
///
/// Beyond the raw `status` field (porcelain's two-character `XY` code for
/// `1`/`2` rows, or the bare `?`/`!` marker), every row also gets a semantic
/// `state: str` so `(git status).where(.state == "modified")` reads
/// naturally instead of requiring callers to know porcelain's XY alphabet
/// (see `xy_state` for the `1`/`2` mapping). `?`/`!` rows get the fixed
/// `"untracked"`/`"ignored"` state directly, since they have no `XY` pair to
/// derive from. `status` itself is unchanged/untouched by this — existing
/// consumers of the raw code keep working exactly as before.
fn parse_porcelain_v2(bytes: &[u8]) -> Option<Value> {
    let mut rows = Vec::new();
    for line in text(bytes)?.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let raw = line.as_bytes();
        let mut r = Record::new();
        match raw[0] {
            b'?' | b'!' => {
                // Must be exactly `<marker><space><path>`. A short-format
                // line has a second marker byte at index 1 instead of a
                // space, which is exactly how a leading space used to get
                // baked into the path -- refuse to slice unless the
                // separator is genuinely there.
                if raw.get(1) != Some(&b' ') || line.len() < 3 {
                    return None;
                }
                let state = if raw[0] == b'?' {
                    "untracked"
                } else {
                    "ignored"
                };
                r.insert("status".into(), Value::Str(line[..1].into()));
                r.insert("state".into(), Value::Str(state.into()));
                r.insert("path".into(), Value::Path(line[2..].into()));
            }
            b'1' => {
                let parts: Vec<&str> = line.splitn(9, ' ').collect();
                if parts.len() != 9 {
                    return None;
                }
                r.insert("status".into(), Value::Str(parts[1].into()));
                r.insert("state".into(), Value::Str(xy_state(parts[1]).into()));
                r.insert("path".into(), Value::Path(parts[8].into()));
            }
            b'2' => {
                let parts: Vec<&str> = line.splitn(10, ' ').collect();
                if parts.len() != 10 {
                    return None;
                }
                r.insert("status".into(), Value::Str(parts[1].into()));
                r.insert("state".into(), Value::Str(xy_state(parts[1]).into()));
                let (path, orig) = match parts[9].split_once('\t') {
                    Some((p, o)) => (p, Some(o)),
                    None => (parts[9], None),
                };
                r.insert("path".into(), Value::Path(path.into()));
                if let Some(orig) = orig {
                    r.insert("orig".into(), Value::Path(orig.into()));
                }
            }
            _ => return None,
        }
        rows.push(r);
    }
    Some(Value::Table(rows))
}

/// Maps a porcelain-v2 `XY` code's two characters (`X` = staged/index state,
/// `Y` = worktree state; see git-status(1)) to a single semantic state word.
///
/// Rule: the worktree half (`Y`, second char) wins if it is anything other
/// than unmodified (`.`); otherwise the staged half (`X`, first char) is
/// used. Concretely this means a file changed in the worktree — whether or
/// not it's *also* staged (`.M` or `MM`) — reads as `"modified"`, while a
/// file that is staged-only with no further worktree change (`A.`) reads as
/// `"added"`, not `"unmodified"`. Each letter maps to a word: `M` ->
/// `"modified"`, `A` -> `"added"`, `D` -> `"deleted"`, `R` -> `"renamed"`,
/// `C` -> `"copied"`, `T` -> `"typechange"`, `U` -> `"unmerged"`. A missing
/// or unrecognized char (including `.`/`.`, i.e. truly no change) falls back
/// to `"unmodified"`; real git never emits a no-op `..` row, so this
/// fallback is a defensive default rather than an expected case.
fn xy_state(xy: &str) -> &'static str {
    fn word(c: char) -> &'static str {
        match c {
            'M' => "modified",
            'A' => "added",
            'D' => "deleted",
            'R' => "renamed",
            'C' => "copied",
            'T' => "typechange",
            'U' => "unmerged",
            _ => "unmodified",
        }
    }
    let mut chars = xy.chars();
    let x = chars.next().unwrap_or('.');
    let y = chars.next().unwrap_or('.');
    if y != '.' { word(y) } else { word(x) }
}

fn rows_or_list(vals: Vec<Value>) -> Option<Value> {
    if vals.iter().all(|v| matches!(v, Value::Record(_))) {
        Some(Value::Table(
            vals.into_iter()
                .map(|v| {
                    if let Value::Record(r) = v {
                        r
                    } else {
                        unreachable!()
                    }
                })
                .collect(),
        ))
    } else {
        Some(Value::List(vals))
    }
}
