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
//! To keep the store bounded, once the summed rank crosses [`MAX_TOTAL_RANK`]
//! every rank is scaled by `0.9` and entries that fall below `1.0` are dropped
//! (zoxide's aging), so a long-lived shell never grows the file without limit.
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
//! (`<state_dir>/jump.frecency`). A missing file, an unreadable file, or any
//! individual malformed line degrades to "not present" rather than an error —
//! a corrupt store never crashes the shell. Writing is best-effort and atomic
//! (temp file + rename); a failed write must never fail the `cd` that triggered
//! it.

use super::*;
use std::cmp::Ordering;

/// Recency-bucket boundaries, in seconds.
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;
const WEEK: i64 = 604_800;

/// Summed-rank ceiling past which the store ages/prunes itself.
const MAX_TOTAL_RANK: f64 = 10_000.0;

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
/// loads it, [`add`](FrecencyStore::add)s the destination, and saves it back so
/// concurrent shells converge (last write wins, each write is load-then-modify).
#[derive(Clone, Debug, Default)]
pub(crate) struct FrecencyStore {
    entries: Vec<Entry>,
}

impl FrecencyStore {
    /// Record a visit to `dir` at `now` (Unix seconds): bump an existing entry's
    /// rank and stamp its access time, or insert a fresh one at rank `1.0`.
    pub(crate) fn add(&mut self, dir: &Path, now: i64) {
        match self.entries.iter_mut().find(|e| e.path == dir) {
            Some(e) => {
                e.rank += 1.0;
                e.last_access = now;
            }
            None => self.entries.push(Entry {
                path: dir.to_path_buf(),
                rank: 1.0,
                last_access: now,
            }),
        }
        self.age();
    }

    /// zoxide-style aging: once the store's total rank exceeds
    /// [`MAX_TOTAL_RANK`], decay every rank and forget the faintest entries so
    /// the file stays bounded no matter how long a session runs.
    fn age(&mut self) {
        let total: f64 = self.entries.iter().map(|e| e.rank).sum();
        if total > MAX_TOTAL_RANK {
            for e in &mut self.entries {
                e.rank *= 0.9;
            }
            self.entries.retain(|e| e.rank >= 1.0);
        }
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

    /// Serialize to the `<rank>\t<last_access>\t<path>` line format.
    fn serialize(&self) -> String {
        let mut s = String::new();
        for e in &self.entries {
            s.push_str(&format!(
                "{:.6}\t{}\t{}\n",
                e.rank,
                e.last_access,
                e.path.display()
            ));
        }
        s
    }

    /// Parse the line format, skipping any blank or malformed line so a
    /// partially-corrupt file still yields every entry it can.
    fn parse(text: &str) -> Self {
        let mut store = Self::default();
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
            if !rank.is_finite() || rank < 0.0 {
                continue;
            }
            store.merge(PathBuf::from(p), rank, last_access);
        }
        store
    }

    /// Fold a parsed row in, coalescing duplicate paths (sum rank, keep the
    /// newest access) so a hand-edited or double-written file never yields the
    /// same directory twice in the ranking.
    fn merge(&mut self, path: PathBuf, rank: f64, last_access: i64) {
        match self.entries.iter_mut().find(|e| e.path == path) {
            Some(e) => {
                e.rank += rank;
                e.last_access = e.last_access.max(last_access);
            }
            None => self.entries.push(Entry {
                path,
                rank,
                last_access,
            }),
        }
    }

    /// Load the host-owned history database. This persistence path is control
    /// plane state chosen by the embedding host, not a language-selected path,
    /// so it deliberately uses the ambient adapter rather than the evaluator's
    /// sandboxed/in-memory [`Fs`] capability. A missing or unreadable file (or
    /// invalid UTF-8) is treated as an empty store — never an error.
    pub(crate) fn load_host(path: &Path) -> Self {
        match StdFs.read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::default(),
        }
    }

    /// Atomically persist the host-owned database (write to a temp file, then
    /// rename). Candidate directories never use this ambient service; they are
    /// always probed through the evaluator's inherited [`Fs`] below. Returns
    /// the I/O result; callers on the `cd` hot path swallow it (best-effort).
    pub(crate) fn save_host(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            StdFs.create_dir_all(parent)?;
        }
        // Per-pid temp name so concurrent shells don't stomp each other's
        // in-progress write before the atomic rename publishes it.
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        StdFs.write(&tmp, self.serialize().as_bytes())?;
        StdFs.rename(&tmp, path)?;
        Ok(())
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
mod tests {
    use super::*;
    use shoal_value::ReadSeek;
    use std::collections::HashSet;
    use std::io;
    use std::sync::Mutex;

    #[derive(Default)]
    struct JumpProbeFs {
        directories: HashSet<PathBuf>,
        probes: Mutex<Vec<String>>,
    }

    impl JumpProbeFs {
        fn with_directory(path: PathBuf) -> Self {
            Self {
                directories: HashSet::from([path]),
                probes: Mutex::new(Vec::new()),
            }
        }

        fn probes(&self) -> Vec<String> {
            self.probes.lock().unwrap().clone()
        }

        fn unsupported<T>() -> io::Result<T> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "jump test filesystem denies unmediated operation",
            ))
        }
    }

    impl Fs for JumpProbeFs {
        fn read(&self, _path: &Path) -> io::Result<Vec<u8>> {
            Self::unsupported()
        }
        fn read_to_string(&self, _path: &Path) -> io::Result<String> {
            Self::unsupported()
        }
        fn open_read(&self, _path: &Path) -> io::Result<Box<dyn ReadSeek + Send>> {
            Self::unsupported()
        }
        fn write(&self, _path: &Path, _data: &[u8]) -> io::Result<()> {
            Self::unsupported()
        }
        fn append(&self, _path: &Path, _data: &[u8]) -> io::Result<()> {
            Self::unsupported()
        }
        fn touch(&self, _path: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn metadata(&self, _path: &Path) -> io::Result<std::fs::Metadata> {
            Self::unsupported()
        }
        fn symlink_metadata(&self, _path: &Path) -> io::Result<std::fs::Metadata> {
            Self::unsupported()
        }
        fn is_dir(&self, path: &Path) -> bool {
            self.probes
                .lock()
                .unwrap()
                .push(format!("is_dir:{}", path.display()));
            self.directories.contains(path)
        }
        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.probes
                .lock()
                .unwrap()
                .push(format!("canonicalize:{}", path.display()));
            Self::unsupported()
        }
        fn read_dir(&self, _path: &Path) -> io::Result<Vec<PathBuf>> {
            Self::unsupported()
        }
        fn create_dir(&self, _path: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn remove_file(&self, _path: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn copy(&self, _from: &Path, _to: &Path) -> io::Result<u64> {
            Self::unsupported()
        }
        fn hard_link(&self, _src: &Path, _dst: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn symlink(&self, _target: &Path, _link: &Path) -> io::Result<()> {
            Self::unsupported()
        }
    }

    fn entry(path: &str, rank: f64, last_access: i64) -> Entry {
        Entry {
            path: PathBuf::from(path),
            rank,
            last_access,
        }
    }

    /// A single recent visit outranks many stale ones: recency beats raw
    /// frequency, which is the defining property of frecency.
    #[test]
    fn recency_outweighs_stale_frequency() {
        let now = 1_000_000_000;
        let mut store = FrecencyStore::default();
        // Ten visits, but all over a week old (→ ×0.25): frecency 10 × 0.25 = 2.5.
        store.entries.push(entry("/home/old", 10.0, now - 2 * WEEK));
        // One visit within the hour (→ ×4): frecency 1 × 4 = 4.0.
        store.entries.push(entry("/home/fresh", 1.0, now - 60));
        let ranked = store.ranked(None, now);
        assert_eq!(ranked[0].path, PathBuf::from("/home/fresh"));
        assert_eq!(ranked[1].path, PathBuf::from("/home/old"));
    }

    #[test]
    fn frecency_bucket_weights() {
        let now = 1_000_000_000;
        assert_eq!(entry("/a", 1.0, now - 60).frecency(now), 4.0); // < 1h
        assert_eq!(entry("/a", 1.0, now - 2 * HOUR).frecency(now), 2.0); // < 1d
        assert_eq!(entry("/a", 1.0, now - 2 * DAY).frecency(now), 0.5); // < 1w
        assert_eq!(entry("/a", 1.0, now - 2 * WEEK).frecency(now), 0.25); // older
    }

    #[test]
    fn add_bumps_rank_and_access() {
        let mut store = FrecencyStore::default();
        store.add(Path::new("/proj"), 100);
        store.add(Path::new("/proj"), 200);
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.entries[0].rank, 2.0);
        assert_eq!(store.entries[0].last_access, 200);
    }

    #[test]
    fn query_is_case_insensitive_substring() {
        let now = 500;
        let mut store = FrecencyStore::default();
        store.add(Path::new("/home/allie/Develop/Shoal"), now);
        store.add(Path::new("/home/allie/downloads"), now);
        let hit = store.ranked(Some("shoal"), now);
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].path, PathBuf::from("/home/allie/Develop/Shoal"));
        // A substring that matches nothing yields no candidates.
        assert!(store.ranked(Some("zzz"), now).is_empty());
    }

    #[test]
    fn last_component_match_breaks_frecency_ties() {
        let now = 1000;
        let mut store = FrecencyStore::default();
        // Equal rank + access → equal frecency; the leaf-name match wins.
        store
            .entries
            .push(entry("/work/api/service", 3.0, now - 10));
        store
            .entries
            .push(entry("/work/service/api", 3.0, now - 10));
        let ranked = store.ranked(Some("service"), now);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].path, PathBuf::from("/work/api/service"));
    }

    #[test]
    fn serialize_parse_roundtrip() {
        let mut store = FrecencyStore::default();
        store.add(Path::new("/a/b"), 100);
        store.add(Path::new("/a/b"), 150);
        store.add(Path::new("/c/d"), 120);
        let text = store.serialize();
        let reloaded = FrecencyStore::parse(&text);
        let mut a = store.entries.clone();
        let mut b = reloaded.entries.clone();
        a.sort_by(|x, y| x.path.cmp(&y.path));
        b.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(a, b);
    }

    #[test]
    fn parse_skips_malformed_lines() {
        let text = "\
2.5\t100\t/good/one
garbage line with no tabs
notanumber\t100\t/bad/rank
1.0\tnotanint\t/bad/access
3.0\t200\t/good/two
";
        let store = FrecencyStore::parse(text);
        let mut paths: Vec<_> = store.entries.iter().map(|e| e.path.clone()).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![PathBuf::from("/good/one"), PathBuf::from("/good/two")]
        );
    }

    #[test]
    fn load_missing_or_corrupt_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file.
        let missing = dir.path().join("does-not-exist");
        assert!(FrecencyStore::load_host(&missing).entries.is_empty());
        // A file of pure garbage (no valid line) loads as empty, not an error.
        let corrupt = dir.path().join("corrupt");
        std::fs::write(&corrupt, b"\x00\x01not at all valid\xff\xfe").unwrap();
        assert!(FrecencyStore::load_host(&corrupt).entries.is_empty());
    }

    #[test]
    fn save_load_roundtrips_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("jump.frecency");
        let mut store = FrecencyStore::default();
        store.add(Path::new("/x/y"), 300);
        store.add(Path::new("/x/y"), 350);
        store.save_host(&path).unwrap();
        let reloaded = FrecencyStore::load_host(&path);
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries[0].path, PathBuf::from("/x/y"));
        assert_eq!(reloaded.entries[0].rank, 2.0);
        assert_eq!(reloaded.entries[0].last_access, 350);
    }

    #[test]
    fn merge_coalesces_duplicate_paths() {
        let text = "\
1.0\t100\t/dup
2.0\t250\t/dup
";
        let store = FrecencyStore::parse(text);
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.entries[0].rank, 3.0);
        assert_eq!(store.entries[0].last_access, 250);
    }

    // --- end-to-end through the evaluator (cd records, j resolves) ----------

    /// A clock pinned to a fixed instant so recency weighting is deterministic.
    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now_ns(&self) -> i64 {
            self.0
        }
    }

    fn run(ev: &mut Evaluator, src: &str) {
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse {src:?}: {e:?}"));
        ev.eval_program(&program)
            .unwrap_or_else(|e| panic!("eval {src:?}: {}", e.msg));
    }

    fn evaluator_at(root: &Path, store: &Path, now_secs: i64) -> Evaluator {
        let mut ev = Evaluator::new(root.to_path_buf());
        ev.set_jump_store(store.to_path_buf());
        ev.set_clock(std::sync::Arc::new(FixedClock(now_secs * 1_000_000_000)));
        ev
    }

    #[test]
    fn direct_jump_candidate_cannot_bypass_inherited_fs_directory_denial() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let ambient_only = root.join("ambient-only-jump-target");
        std::fs::create_dir(&ambient_only).unwrap();
        let fs = Arc::new(JumpProbeFs::default());
        let mut parent = Evaluator::new(root.clone());
        parent.set_fs(fs.clone());
        let child = parent
            .child_context()
            .build(ChildKind::Spawn, CancelToken::new());

        let error = child
            .jump_resolve(Some("ambient-only-jump-target"))
            .unwrap_err();
        assert_eq!(error.code, "not_found");
        assert_eq!(
            fs.probes(),
            vec![format!("is_dir:{}", ambient_only.display())]
        );
    }

    #[test]
    fn history_candidate_uses_inherited_fs_and_canonicalization_denial() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let history_target = root.join("history-shoal-project");
        std::fs::create_dir(&history_target).unwrap();
        let store_path = root.join("jump.frecency");
        let mut history = FrecencyStore::default();
        history.add(&history_target, 1_000);
        history.save_host(&store_path).unwrap();

        let fs = Arc::new(JumpProbeFs::with_directory(history_target.clone()));
        let mut evaluator = evaluator_at(&root, &store_path, 1_001);
        evaluator.set_fs(fs.clone());
        let error = evaluator
            .eval_program(&shoal_syntax::parse("j shoal-project").unwrap())
            .unwrap_err();

        assert_eq!(error.code, "not_found");
        assert_eq!(evaluator.cwd(), root);
        assert_eq!(
            fs.probes(),
            vec![
                format!("is_dir:{}", root.join("shoal-project").display()),
                format!("is_dir:{}", history_target.display()),
                format!("canonicalize:{}", history_target.display()),
            ]
        );
    }

    #[test]
    fn cd_records_and_jump_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::create_dir_all(root.join("beta/shoal-project")).unwrap();
        let store = root.join("jump.frecency");

        let mut ev = evaluator_at(&root, &store, 1_000);
        run(&mut ev, "cd alpha");
        run(&mut ev, "cd ../beta/shoal-project");

        // Both destinations were recorded by the plain `cd`s.
        let recorded = FrecencyStore::load_host(&store);
        assert_eq!(
            recorded.entries.len(),
            2,
            "each cd should record its target"
        );

        // Jump by a leaf-name substring, then by a mid-path substring.
        run(&mut ev, "j shoal");
        assert_eq!(ev.cwd(), root.join("beta/shoal-project").as_path());
        run(&mut ev, "j alpha");
        assert_eq!(ev.cwd(), root.join("alpha").as_path());

        // A query that matches nothing is a clean `not_found`, never a panic.
        let err = ev
            .eval_program(&shoal_syntax::parse("j no-such-dir-xyz").unwrap())
            .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn history_survives_across_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let store = root.join("jump.frecency");

        // Session A records a visit, then goes away.
        {
            let mut a = evaluator_at(&root, &store, 500);
            run(&mut a, "cd workspace");
        }
        // Session B, a fresh evaluator on the same store, jumps using A's history.
        let mut b = evaluator_at(&root, &store, 600);
        run(&mut b, "j work");
        assert_eq!(b.cwd(), root.join("workspace").as_path());
    }

    #[test]
    fn corrupt_store_does_not_break_navigation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("dir1")).unwrap();
        let store = root.join("jump.frecency");
        // Pre-seed the store with pure garbage.
        std::fs::write(&store, b"\xff\xfegarbage\x00not valid").unwrap();

        let mut ev = evaluator_at(&root, &store, 1_000);
        // cd still works despite the corrupt store, and rewrites it cleanly.
        run(&mut ev, "cd dir1");
        assert_eq!(ev.cwd(), root.join("dir1").as_path());
        let reloaded = FrecencyStore::load_host(&store);
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries[0].path, root.join("dir1"));
    }

    #[test]
    fn recording_off_by_default_keeps_store_untouched() {
        // A fresh `Evaluator` (the `-c`/script/conformance path) must not write
        // any jump store when it `cd`s — recording is opt-in per host.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let mut ev = Evaluator::new(root.clone());
        run(&mut ev, "cd sub");
        assert_eq!(ev.cwd(), root.join("sub").as_path());
        // No store file was created anywhere under the temp root.
        assert!(!root.join("jump.frecency").exists());
    }

    #[test]
    fn production_evaluator_has_only_explicit_ambient_filesystem_exceptions() {
        #[derive(Clone, Copy)]
        struct ExpectedLine {
            file: &'static str,
            line: usize,
            text: &'static str,
        }

        // These are not ambient probes: each receiver is Metadata previously
        // obtained through Fs::metadata/Fs::symlink_metadata. Keeping this
        // exact inventory lets the lexical scan reject every new no-argument
        // `.is_dir()`/`.is_file()` call, whose receiver would otherwise be
        // impossible to classify without a Rust type checker.
        const MEDIATED_METADATA_CLASSIFICATION: &[ExpectedLine] = &[
            ExpectedLine {
                file: "builtins.rs",
                line: 179,
                text: "if m.is_dir() {",
            },
            ExpectedLine {
                file: "builtins.rs",
                line: 183,
                text: "} else if m.is_file() {",
            },
            ExpectedLine {
                file: "builtins.rs",
                line: 315,
                text: "if m.is_dir() {",
            },
            ExpectedLine {
                file: "builtins.rs",
                line: 372,
                text: "if meta.is_dir() {",
            },
            ExpectedLine {
                file: "builtins.rs",
                line: 506,
                text: "if !metadata.is_dir() || metadata.uid() != effective_uid || metadata.mode() & 0o077 != 0 {",
            },
            ExpectedLine {
                file: "builtins.rs",
                line: 520,
                text: "if fs.symlink_metadata(path)?.is_dir() {",
            },
            ExpectedLine {
                file: "builtins.rs",
                line: 567,
                text: "if !metadata.is_dir()",
            },
            ExpectedLine {
                file: "expr_access.rs",
                line: 415,
                text: ".map(|m| m.is_dir())",
            },
            ExpectedLine {
                file: "expr_access.rs",
                line: 422,
                text: ".map(|m| m.is_file())",
            },
        ];

        // The only ambient filesystem operations in evaluator production:
        // four calls implement the explicitly host-owned frecency database,
        // and one sets permissions on a TempDir the Rust-script runner itself
        // just created. Language-selected paths may not join this inventory.
        const AMBIENT_ALLOWLIST: &[ExpectedLine] = &[
            ExpectedLine {
                file: "frecency.rs",
                line: 220,
                text: "match StdFs.read_to_string(path) {",
            },
            ExpectedLine {
                file: "frecency.rs",
                line: 232,
                text: "StdFs.create_dir_all(parent)?;",
            },
            ExpectedLine {
                file: "frecency.rs",
                line: 237,
                text: "StdFs.write(&tmp, self.serialize().as_bytes())?;",
            },
            ExpectedLine {
                file: "frecency.rs",
                line: 238,
                text: "StdFs.rename(&tmp, path)?;",
            },
            ExpectedLine {
                file: "script.rs",
                line: 367,
                text: "std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;",
            },
        ];

        fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect_rust_files(&path, out);
                } else if path.extension() == Some(OsStr::new("rs")) {
                    out.push(path);
                }
            }
        }

        fn expected_index(
            expected: &[ExpectedLine],
            file: &str,
            line: usize,
            text: &str,
        ) -> Option<usize> {
            expected
                .iter()
                .position(|item| item.file == file && item.line == line && item.text == text.trim())
        }

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rust_files(&src, &mut files);
        files.sort();
        let mut seen_metadata = vec![false; MEDIATED_METADATA_CLASSIFICATION.len()];
        let mut seen_ambient = vec![false; AMBIENT_ALLOWLIST.len()];
        let mut unexpected = Vec::new();

        for path in files {
            let relative = path.strip_prefix(&src).unwrap().to_string_lossy();
            if relative == "tests.rs" {
                continue; // the whole file is included only under cfg(test)
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let production = source
                .find("\n#[cfg(test)]\nmod ")
                .map_or(source.as_str(), |index| &source[..index]);
            for (offset, line) in production.lines().enumerate() {
                let line_number = offset + 1;
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                let metadata_classification =
                    trimmed.contains(".is_dir()") || trimmed.contains(".is_file()");
                if metadata_classification {
                    if let Some(index) = expected_index(
                        MEDIATED_METADATA_CLASSIFICATION,
                        &relative,
                        line_number,
                        trimmed,
                    ) {
                        seen_metadata[index] = true;
                    } else {
                        unexpected.push(format!(
                            "{relative}:{line_number}: unclassified no-argument path probe: {trimmed}"
                        ));
                    }
                }

                let without_type_names = trimmed
                    .replace("std::fs::Metadata", "")
                    .replace("std::fs::Permissions", "");
                let ambient = trimmed.contains(".exists()")
                    || trimmed.contains(".canonicalize()")
                    || [
                        "Path::exists(",
                        "Path::is_file(",
                        "Path::is_dir(",
                        "Path::canonicalize(",
                    ]
                    .iter()
                    .any(|needle| trimmed.contains(needle))
                    || without_type_names.contains("std::fs")
                    || trimmed.contains("StdFs.");
                if ambient {
                    if let Some(index) =
                        expected_index(AMBIENT_ALLOWLIST, &relative, line_number, trimmed)
                    {
                        seen_ambient[index] = true;
                    } else {
                        unexpected.push(format!(
                            "{relative}:{line_number}: ambient filesystem access: {trimmed}"
                        ));
                    }
                }
            }
        }

        assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
        assert!(
            seen_metadata.iter().all(|seen| *seen),
            "mediated metadata inventory is stale: {seen_metadata:?}"
        );
        assert!(
            seen_ambient.iter().all(|seen| *seen),
            "ambient filesystem allowlist is stale: {seen_ambient:?}"
        );
    }
}
