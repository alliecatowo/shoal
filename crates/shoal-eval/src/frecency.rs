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

    /// Load the store from `path` through the [`Fs`] port. A missing or
    /// unreadable file (or invalid UTF-8) is treated as an empty store — never
    /// an error.
    pub(crate) fn load(fs: &dyn Fs, path: &Path) -> Self {
        match fs.read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::default(),
        }
    }

    /// Atomically persist the store to `path` through the [`Fs`] port (write to
    /// a temp file, then rename). Returns the I/O result; callers on the `cd`
    /// hot path swallow it (best-effort).
    pub(crate) fn save(&self, fs: &dyn Fs, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs.create_dir_all(parent)?;
        }
        // Per-pid temp name so concurrent shells don't stomp each other's
        // in-progress write before the atomic rename publishes it.
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        fs.write(&tmp, self.serialize().as_bytes())?;
        fs.rename(&tmp, path)?;
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
        self.exec.jump_store = Some(crate::journal::default_state_dir().join(STORE_FILE));
    }

    /// Point the frecency store at a specific file (hosts that manage their own
    /// state dir, and hermetic tests). Enables recording, like
    /// [`Evaluator::open_default_jump_history`].
    pub fn set_jump_store(&mut self, path: PathBuf) {
        self.exec.jump_store = Some(path);
    }

    /// The store file to *read* for a `j` query: the installed store when
    /// recording is enabled, else the shared per-user default so a one-shot
    /// `shoal -c 'j foo'` still resolves against real history.
    fn jump_read_path(&self) -> PathBuf {
        self.exec
            .jump_store
            .clone()
            .unwrap_or_else(|| crate::journal::default_state_dir().join(STORE_FILE))
    }

    /// Record a successful `cd`/`j` into the frecency store (best-effort). A
    /// no-op unless recording is enabled; a store write failure is swallowed so
    /// it can never fail the navigation that triggered it. Uses the [`Clock`]
    /// port for "now" so tests can pin recency.
    pub(crate) fn record_cd(&mut self, dir: &Path) {
        let Some(path) = self.exec.jump_store.clone() else {
            return; // recording disabled (scripts / -c / conformance)
        };
        let now = self.host.clock.now_ns() / 1_000_000_000;
        let mut store = FrecencyStore::load(self.host.fs.as_ref(), &path);
        store.add(dir, now);
        let _ = store.save(self.host.fs.as_ref(), &path);
    }

    /// The `j`/`jump` builtin: resolve the best matching stored directory for an
    /// optional query and `cd` there, recording the jump. Behaves as a strict
    /// superset of `cd` — an argument that is itself an existing directory is
    /// jumped to verbatim (zoxide's `z <path>` fast path).
    pub(crate) fn eval_jump(&mut self, call: &CmdCall) -> VResult<Value> {
        // Same session-scope rule as `cd`: mutating the session cwd inside a
        // `fn` body is illegal (use `with cwd:` for a scoped change).
        if self.exec.in_fn_body > 0 {
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
        let canon = target.canonicalize().map_err(|e| {
            ErrorVal::new(
                "not_found",
                format!("jump target {}: {e}", target.display()),
            )
            .with_span(call.span)
        })?;
        // Route through `change_cwd` so a jump also updates OLDPWD (a later
        // `cd -` returns to where the jump left from) and records frecency.
        self.change_cwd(canon);
        Ok(Value::Path(self.exec.cwd.clone()))
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
                self.exec.cwd.join(q)
            };
            if direct.is_dir() {
                return Ok(direct);
            }
        }
        let now = self.host.clock.now_ns() / 1_000_000_000;
        let store = FrecencyStore::load(self.host.fs.as_ref(), &self.jump_read_path());
        for e in store.ranked(query, now) {
            if e.path.is_dir() {
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
        assert!(FrecencyStore::load(&StdFs, &missing).entries.is_empty());
        // A file of pure garbage (no valid line) loads as empty, not an error.
        let corrupt = dir.path().join("corrupt");
        std::fs::write(&corrupt, b"\x00\x01not at all valid\xff\xfe").unwrap();
        assert!(FrecencyStore::load(&StdFs, &corrupt).entries.is_empty());
    }

    #[test]
    fn save_load_roundtrips_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("jump.frecency");
        let mut store = FrecencyStore::default();
        store.add(Path::new("/x/y"), 300);
        store.add(Path::new("/x/y"), 350);
        store.save(&StdFs, &path).unwrap();
        let reloaded = FrecencyStore::load(&StdFs, &path);
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
        let recorded = FrecencyStore::load(&StdFs, &store);
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
        let reloaded = FrecencyStore::load(&StdFs, &store);
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
}
