//! Frecency-ranked directory jumping (site/content/internals/roadmap-and-priorities.md #3) — the `j`/`jump`
//! builtin and the persistent store every successful `cd` bumps.
//!
//! # Frecency formula (zoxide-compatible)
//!
//! Each stored directory carries a `rank: f64` (accumulated visit weight) and a
//! `last_access` (Unix **seconds**). Every visit does `rank += 1.0` and sets
//! `last_access = now`. The *frecency* used for ranking multiplies the rank by a
//! recency weight of the entry's age (`now - last_access`), in zoxide's buckets:
//!
//! | age                | weight |
//! |--------------------|--------|
//! | `< 1 hour`         | × 4.0  |
//! | `< 1 day`          | × 2.0  |
//! | `< 1 week`         | × 0.5  |
//! | otherwise          | × 0.25 |
//!
//! So a directory visited once an hour ago (`1 × 4 = 4`) outranks one visited
//! three times last month (`3 × 0.25 = 0.75`): recency dominates raw frequency,
//! which is the whole point of frecency over a plain visit counter.
//!
//! To keep the advisory store bounded, Shoal admits at most [`MAX_ENTRIES`]
//! identities, [`MAX_PATH_BYTES`] per serialized path, and
//! [`MAX_TOTAL_PATH_BYTES`] across all paths. Parsed rows are admitted in file
//! order; duplicates of an admitted path are coalesced. A new successful visit
//! evicts the weakest admitted identity when it needs room (lowest rank, then
//! oldest access, then lexically largest path). This makes recovery from a
//! hostile or hand-edited file deterministic without making navigation fail.
//!
//! Rank is bounded too. Before a visit would cross [`MAX_TOTAL_RANK`], existing
//! ranks are aged and faint entries are dropped. Loaded ranks are renormalized
//! in one finite pass, so individually finite rows cannot sum to infinity.
//!
//! # Query matching
//!
//! A query matches an entry when the entry's full (canonical, absolute) path
//! contains the query as a **case-insensitive substring**. Among matches the
//! best-first order is: highest frecency, then a preference for entries whose
//! **last path component** contains the query (the intuitive "jump by leaf
//! name" case), then most-recently-accessed, then lexical path order for a
//! fully deterministic tie-break. `j` with no query jumps to the highest
//! frecency directory overall. Directories that no longer exist are skipped at
//! resolution time (see [`Evaluator::eval_jump`]).
//!
//! # Persistence
//!
//! The store is a small line-based text file (`<rank>\t<last_access>\t<path>`
//! per line) colocated with the journal under the per-user state dir
//! (`<state_dir>/jump.frecency`). Reads stop after [`MAX_STORE_FILE_BYTES`]
//! rather than trusting the host file's size. A missing file, an unreadable
//! file, or any individual malformed line degrades to "not present" rather
//! than an error — a corrupt store never crashes the shell. Writing is bounded,
//! best-effort, and atomically replaces the prior file; a failed write must
//! never fail the `cd` that triggered it.

use super::*;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read as _;

/// Recency-bucket boundaries, in seconds.
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;
const WEEK: i64 = 604_800;

/// Summed-rank ceiling past which the store ages/prunes itself.
const MAX_TOTAL_RANK: f64 = 10_000.0;

/// Maximum bytes read from or written to the host-owned history file.
const MAX_STORE_FILE_BYTES: usize = 1024 * 1024;

/// Maximum number of distinct directory identities retained.
const MAX_ENTRIES: usize = 4096;

/// Maximum UTF-8 bytes in one serialized (lossy, display-form) path.
const MAX_PATH_BYTES: usize = 4096;

/// Maximum serialized path bytes retained across the whole store.
const MAX_TOTAL_PATH_BYTES: usize = 512 * 1024;

/// Basename of the frecency store within the per-user state dir.
const STORE_FILE: &str = "jump.frecency";

/// One visited directory: an accumulated visit `rank` and the `last_access`
/// time (Unix seconds) used for recency weighting.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    pub path: PathBuf,
    pub rank: f64,
    pub last_access: i64,
}

impl Entry {
    /// Frecency = `rank × recency_weight(age)`. See the module docs for the
    /// bucket table; `now` and `last_access` are both Unix seconds.
    fn frecency(&self, now: i64) -> f64 {
        let age = now.saturating_sub(self.last_access);
        let weight = if age < HOUR {
            4.0
        } else if age < DAY {
            2.0
        } else if age < WEEK {
            0.5
        } else {
            0.25
        };
        self.rank * weight
    }
}

/// The in-memory directory-frecency table. Cheap to load/serialize; each `cd`
/// loads it, [`add`](FrecencyStore::add)s the destination, and saves it back.
/// Publications are atomic; simultaneous load-modify-save cycles are
/// intentionally last-writer-wins because this is advisory navigation history.
#[derive(Clone, Debug, Default)]
pub(crate) struct FrecencyStore {
    entries: Vec<Entry>,
}

impl FrecencyStore {
    /// Record a visit to `dir` at `now` (Unix seconds): bump an existing entry's
    /// rank and stamp its access time, or insert a fresh one at rank `1.0`.
    pub(crate) fn add(&mut self, dir: &Path, now: i64) {
        if !admissible_path(dir) {
            return;
        }
        self.make_rank_headroom();
        match self.entries.iter_mut().find(|e| e.path == dir) {
            Some(e) => {
                e.rank = finite_rank_add(e.rank, 1.0);
                e.last_access = now;
            }
            None => {
                let path_bytes = serialized_path_bytes(dir);
                while self.entries.len() >= MAX_ENTRIES
                    || self.total_path_bytes().saturating_add(path_bytes) > MAX_TOTAL_PATH_BYTES
                {
                    let Some(index) = self.weakest_index() else {
                        return;
                    };
                    self.entries.remove(index);
                }
                self.entries.push(Entry {
                    path: dir.to_path_buf(),
                    rank: 1.0,
                    last_access: now,
                });
            }
        }
    }

    /// Age before adding, so a newly visited identity is not immediately
    /// discarded merely because its `1.0` crossed the total-rank ceiling.
    fn make_rank_headroom(&mut self) {
        let total = self.total_rank();
        if total + 1.0 > MAX_TOTAL_RANK {
            let factor = (MAX_TOTAL_RANK * 0.9 / total).min(0.9);
            for entry in &mut self.entries {
                entry.rank *= factor;
            }
            self.entries.retain(|entry| entry.rank >= 1.0);
        }
    }

    /// Normalize an arbitrary parsed store in one pass. This is deliberately
    /// stronger than ordinary visit aging: hostile finite inputs may otherwise
    /// overflow when summed or require thousands of 0.9 aging passes.
    fn normalize_loaded_ranks(&mut self) {
        let total = self.total_rank();
        if total > MAX_TOTAL_RANK {
            let factor = (MAX_TOTAL_RANK * 0.9) / total;
            for entry in &mut self.entries {
                entry.rank *= factor;
            }
            self.entries.retain(|entry| entry.rank >= 1.0);
        }
    }

    fn total_rank(&self) -> f64 {
        self.entries
            .iter()
            .fold(0.0, |total, entry| total + sanitize_rank(entry.rank))
    }

    fn total_path_bytes(&self) -> usize {
        self.entries.iter().fold(0usize, |total, entry| {
            total.saturating_add(serialized_path_bytes(&entry.path))
        })
    }

    fn weakest_index(&self) -> Option<usize> {
        self.entries
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.rank
                    .total_cmp(&b.rank)
                    .then_with(|| a.last_access.cmp(&b.last_access))
                    // On a full tie, evict the lexically largest identity.
                    .then_with(|| b.path.cmp(&a.path))
            })
            .map(|(index, _)| index)
    }

    /// All entries matching `query` (or every entry when `query` is `None`),
    /// ordered best-first per the module's ranking rules. Pure: does no
    /// filesystem I/O, so callers filter out vanished directories themselves.
    pub(crate) fn ranked(&self, query: Option<&str>, now: i64) -> Vec<&Entry> {
        let needle = query.map(str::to_lowercase);
        let mut out: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| match &needle {
                None => true,
                Some(q) => path_contains(&e.path, q),
            })
            .collect();
        out.sort_by(|a, b| {
            // Primary: frecency, descending.
            b.frecency(now)
                .partial_cmp(&a.frecency(now))
                .unwrap_or(Ordering::Equal)
                // Prefer a last-component (leaf) match over a mid-path one.
                .then_with(|| {
                    last_component_match(b, needle.as_deref())
                        .cmp(&last_component_match(a, needle.as_deref()))
                })
                // Then the more recently visited directory.
                .then_with(|| b.last_access.cmp(&a.last_access))
                // Finally a stable lexical order so ties are deterministic.
                .then_with(|| a.path.cmp(&b.path))
        });
        out
    }

    /// Serialize to the `<rank>\t<last_access>\t<path>` line format. The
    /// explicit output ceiling is a final defense even if an internal test or
    /// future caller constructs an invalid store without using [`Self::add`].
    fn serialize(&self) -> String {
        let mut output = String::with_capacity(MAX_STORE_FILE_BYTES.min(8192));
        let mut identities = 0usize;
        let mut path_bytes = 0usize;
        for entry in &self.entries {
            if identities >= MAX_ENTRIES || !admissible_path(&entry.path) {
                continue;
            }
            let path = entry.path.to_string_lossy();
            if path_bytes.saturating_add(path.len()) > MAX_TOTAL_PATH_BYTES {
                continue;
            }
            let rank = sanitize_rank(entry.rank);
            let mut line = String::with_capacity(path.len().saturating_add(48));
            let _ = writeln!(line, "{rank:.6}\t{}\t{path}", entry.last_access);
            if output.len().saturating_add(line.len()) > MAX_STORE_FILE_BYTES {
                break;
            }
            output.push_str(&line);
            identities += 1;
            path_bytes += path.len();
        }
        output
    }

    /// Parse the line format, skipping any blank or malformed line so a
    /// partially-corrupt file still yields every entry it can.
    fn parse(text: &str) -> Self {
        let mut store = Self::default();
        let mut indexes = HashMap::<PathBuf, usize>::new();
        let mut path_bytes = 0usize;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // `splitn(3)` keeps the whole (possibly tab-bearing) path as the
            // final field — only the two leading numeric fields are delimited.
            let mut it = line.splitn(3, '\t');
            let (Some(r), Some(t), Some(p)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            let (Ok(rank), Ok(last_access)) = (r.parse::<f64>(), t.parse::<i64>()) else {
                continue;
            };
            if !rank.is_finite() || rank <= 0.0 {
                continue;
            }
            if p.is_empty() || p.len() > MAX_PATH_BYTES || p.contains('\n') || p.contains('\r') {
                continue;
            }
            let borrowed = Path::new(p);
            if let Some(&index) = indexes.get(borrowed) {
                let entry = &mut store.entries[index];
                entry.rank = finite_rank_add(entry.rank, rank);
                entry.last_access = entry.last_access.max(last_access);
                continue;
            }
            if store.entries.len() >= MAX_ENTRIES
                || path_bytes.saturating_add(p.len()) > MAX_TOTAL_PATH_BYTES
            {
                continue;
            }
            let path = PathBuf::from(p);
            indexes.insert(path.clone(), store.entries.len());
            store.entries.push(Entry {
                path,
                rank: sanitize_rank(rank),
                last_access,
            });
            path_bytes += p.len();
        }
        store.normalize_loaded_ranks();
        store
    }

    /// Load the host-owned history database. This persistence path is control
    /// plane state chosen by the embedding host, not a language-selected path,
    /// so it deliberately uses the ambient adapter rather than the evaluator's
    /// sandboxed/in-memory [`Fs`] capability. A missing or unreadable file (or
    /// invalid UTF-8) is treated as an empty store — never an error. Oversized
    /// files contribute only complete lines from their bounded prefix.
    pub(crate) fn load_host(path: &Path) -> Self {
        let Ok(reader) = StdFs.open_read(path) else {
            return Self::default();
        };
        let mut bytes = Vec::with_capacity(8192);
        if reader
            .take((MAX_STORE_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return Self::default();
        }
        if bytes.len() > MAX_STORE_FILE_BYTES {
            bytes.truncate(MAX_STORE_FILE_BYTES);
            let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
                return Self::default();
            };
            bytes.truncate(last_newline + 1);
        }
        std::str::from_utf8(&bytes).map_or_else(|_| Self::default(), Self::parse)
    }

    /// Atomically persist the host-owned database (write to a temp file, then
    /// rename). Candidate directories never use this ambient service; they are
    /// always probed through the evaluator's inherited [`Fs`] below. Returns
    /// the I/O result; callers on the `cd` hot path swallow it (best-effort).
    pub(crate) fn save_host(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            StdFs.create_dir_all(parent)?;
        }
        let output = self.serialize();
        debug_assert!(output.len() <= MAX_STORE_FILE_BYTES);
        StdFs.atomic_replace(path, output.as_bytes())
    }
}

fn serialized_path_bytes(path: &Path) -> usize {
    path.to_string_lossy().len()
}

fn admissible_path(path: &Path) -> bool {
    let path = path.to_string_lossy();
    !path.is_empty() && path.len() <= MAX_PATH_BYTES && !path.contains('\n') && !path.contains('\r')
}

fn sanitize_rank(rank: f64) -> f64 {
    if rank.is_finite() && rank >= 0.0 {
        rank.min(MAX_TOTAL_RANK)
    } else {
        0.0
    }
}

fn finite_rank_add(left: f64, right: f64) -> f64 {
    let sum = sanitize_rank(left) + sanitize_rank(right);
    if sum.is_finite() {
        sum.min(MAX_TOTAL_RANK)
    } else {
        MAX_TOTAL_RANK
    }
}

fn path_contains(path: &Path, needle_lower: &str) -> bool {
    path.to_string_lossy().to_lowercase().contains(needle_lower)
}

fn last_component_match(entry: &Entry, needle_lower: Option<&str>) -> bool {
    let Some(needle) = needle_lower else {
        return false;
    };
    entry
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase().contains(needle))
        .unwrap_or(false)
}

impl Evaluator {
    /// Enable directory-frecency recording against the shared per-user store
    /// (`<state_dir>/jump.frecency`), mirroring [`Evaluator::open_default_journal`].
    /// Interactive hosts (the REPL, the kernel's long-lived sessions) call this
    /// once so `cd` builds up jump history; `-c`/scripts/conformance leave it
    /// off and never write.
    pub fn open_default_jump_history(&mut self) {
        self.exec.shell.jump_store = Some(crate::journal::default_state_dir().join(STORE_FILE));
    }

    /// Point the frecency store at a specific file (hosts that manage their own
    /// state dir, and hermetic tests). Enables recording, like
    /// [`Evaluator::open_default_jump_history`].
    pub fn set_jump_store(&mut self, path: PathBuf) {
        self.exec.shell.jump_store = Some(path);
    }

    /// The store file to *read* for a `j` query: the installed store when
    /// recording is enabled, else the shared per-user default so a one-shot
    /// `shoal -c 'j foo'` still resolves against real history.
    fn jump_read_path(&self) -> PathBuf {
        self.exec
            .shell
            .jump_store
            .clone()
            .unwrap_or_else(|| crate::journal::default_state_dir().join(STORE_FILE))
    }

    /// Record a successful `cd`/`j` into the frecency store (best-effort). A
    /// no-op unless recording is enabled; a store write failure is swallowed so
    /// it can never fail the navigation that triggered it. Uses the [`Clock`]
    /// port for "now" so tests can pin recency.
    pub(crate) fn record_cd(&mut self, dir: &Path) {
        let Some(path) = self.exec.shell.jump_store.clone() else {
            return; // recording disabled (scripts / -c / conformance)
        };
        let now = self.host.clock.now_ns() / 1_000_000_000;
        let mut store = FrecencyStore::load_host(&path);
        store.add(dir, now);
        let _ = store.save_host(&path);
    }

    /// The `j`/`jump` builtin: resolve the best matching stored directory for an
    /// optional query and `cd` there, recording the jump. Behaves as a strict
    /// superset of `cd` — an argument that is itself an existing directory is
    /// jumped to verbatim (zoxide's `z <path>` fast path).
    pub(crate) fn eval_jump(&mut self, call: &CmdCall) -> VResult<Value> {
        // Same session-scope rule as `cd`: mutating the session cwd inside a
        // `fn` body is illegal (use `with cwd:` for a scoped change).
        if self.exec.control.in_fn_body > 0 {
            return Err(ErrorVal::new(
                "custom",
                "jump is only allowed at session top level; use `with cwd:` inside a fn body",
            )
            .with_span(call.span));
        }
        if call.args.len() > 1 {
            return Err(ErrorVal::arg_error("jump takes at most one query").with_span(call.span));
        }
        let query = match call.args.first() {
            None => None,
            Some(a) => Some(match self.cmd_arg_value(a)? {
                Value::Str(s) => s,
                Value::Path(p) => p.to_string_lossy().into_owned(),
                v => {
                    return Err(ErrorVal::arg_error(format!(
                        "jump expects a text query, found {}",
                        v.type_name()
                    ))
                    .with_span(call.span));
                }
            }),
        };
        let target = self
            .jump_resolve(query.as_deref())
            .map_err(|e| e.or_span(call.span))?;
        let canon = self.host.fs.canonicalize(&target).map_err(|e| {
            ErrorVal::new(
                "not_found",
                format!("jump target {}: {e}", target.display()),
            )
            .with_span(call.span)
        })?;
        // Route through `change_cwd` so a jump also updates OLDPWD (a later
        // `cd -` returns to where the jump left from) and records frecency.
        self.change_cwd(canon);
        Ok(Value::Path(self.exec.shell.cwd.clone()))
    }

    /// Resolve the directory a `j <query>` should land in, without changing the
    /// cwd. Tries the existing-directory fast path first, then the frecency
    /// ranking (skipping vanished directories). `None` query → highest overall.
    fn jump_resolve(&self, query: Option<&str>) -> VResult<PathBuf> {
        // Fast path: a query naming an actually-existing directory jumps there
        // directly, so `j` never regresses a plain `cd <path>`.
        if let Some(q) = query {
            let direct = if Path::new(q).is_absolute() {
                PathBuf::from(q)
            } else {
                self.exec.shell.cwd.join(q)
            };
            if self.host.fs.is_dir(&direct) {
                return Ok(direct);
            }
        }
        let now = self.host.clock.now_ns() / 1_000_000_000;
        let store = FrecencyStore::load_host(&self.jump_read_path());
        for e in store.ranked(query, now) {
            if self.host.fs.is_dir(&e.path) {
                return Ok(e.path.clone());
            }
        }
        Err(match query {
            Some(q) => ErrorVal::new(
                "not_found",
                format!("jump: no matching directory for {q:?}"),
            )
            .with_hint("cd into directories first to build up jump history"),
            None => ErrorVal::new("not_found", "jump: no directory history yet")
                .with_hint("cd into directories first to build up jump history"),
        })
    }
}

#[cfg(test)]
#[path = "frecency/tests.rs"]
mod tests;
