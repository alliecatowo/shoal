use super::*;
use shoal_value::ReadSeek;
use std::collections::HashSet;
use std::io;
use std::sync::{Arc, Barrier, Mutex};

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

#[test]
fn parse_caps_unique_identities_in_file_order() {
    let mut text = String::new();
    for index in 0..(MAX_ENTRIES + 200) {
        writeln!(text, "1\t{index}\t/history/{index:05}").unwrap();
    }
    let store = FrecencyStore::parse(&text);
    assert_eq!(store.entries.len(), MAX_ENTRIES);
    assert_eq!(store.entries[0].path, PathBuf::from("/history/00000"));
    assert_eq!(
        store.entries[MAX_ENTRIES - 1].path,
        PathBuf::from(format!("/history/{:05}", MAX_ENTRIES - 1))
    );
}

#[test]
fn parse_caps_aggregate_path_bytes_deterministically() {
    let mut text = String::new();
    let padding = "x".repeat(240);
    for index in 0..MAX_ENTRIES {
        writeln!(text, "1\t{index}\t/{index:05}-{padding}").unwrap();
    }
    let first = FrecencyStore::parse(&text);
    let second = FrecencyStore::parse(&text);
    assert_eq!(first.entries, second.entries);
    assert!(first.entries.len() < MAX_ENTRIES);
    assert!(first.total_path_bytes() <= MAX_TOTAL_PATH_BYTES);
    assert!(
        first
            .entries
            .iter()
            .all(|entry| serialized_path_bytes(&entry.path) <= MAX_PATH_BYTES)
    );
}

#[test]
fn duplicate_rank_overflow_is_finite_and_coalesced_in_linear_index() {
    let mut text = String::new();
    for index in 0..20_000 {
        writeln!(text, "1e308\t{index}\t/duplicate").unwrap();
    }
    let store = FrecencyStore::parse(&text);
    assert_eq!(store.entries.len(), 1);
    assert_eq!(store.entries[0].path, PathBuf::from("/duplicate"));
    assert!(store.entries[0].rank.is_finite());
    assert!(store.entries[0].rank <= MAX_TOTAL_RANK);
    assert_eq!(store.entries[0].last_access, 19_999);
}

#[test]
fn add_evicts_the_deterministic_weakest_identity() {
    let mut store = FrecencyStore::default();
    for index in 0..MAX_ENTRIES {
        store
            .entries
            .push(entry(&format!("/history/{index:05}"), 1.0, 10));
    }
    let lexical_largest = PathBuf::from(format!("/history/{:05}", MAX_ENTRIES - 1));
    store.add(Path::new("/new-visit"), 20);

    assert_eq!(store.entries.len(), MAX_ENTRIES);
    assert!(
        !store
            .entries
            .iter()
            .any(|entry| entry.path == lexical_largest)
    );
    assert!(
        store
            .entries
            .iter()
            .any(|entry| entry.path == Path::new("/new-visit"))
    );
}

#[test]
fn rank_aging_preserves_new_visit_and_stays_finite() {
    let text = "10000\t1\t/dominant\n";
    let mut store = FrecencyStore::parse(text);
    store.add(Path::new("/new-visit"), 2);

    assert!(
        store
            .entries
            .iter()
            .any(|entry| entry.path == Path::new("/new-visit"))
    );
    assert!(store.entries.iter().all(|entry| entry.rank.is_finite()));
    assert!(store.total_rank() <= MAX_TOTAL_RANK);
}

#[test]
fn oversized_host_file_reads_only_complete_bounded_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized.frecency");
    let mut bytes = b"2\t10\t/kept\n".to_vec();
    bytes.resize(MAX_STORE_FILE_BYTES + 64, b'x');
    std::fs::write(&path, bytes).unwrap();

    let store = FrecencyStore::load_host(&path);
    assert_eq!(store.entries, vec![entry("/kept", 2.0, 10)]);
}

#[test]
fn oversized_single_line_degrades_to_empty_without_amplification() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge-line.frecency");
    std::fs::write(&path, vec![b'x'; MAX_STORE_FILE_BYTES + 64]).unwrap();

    let mut store = FrecencyStore::load_host(&path);
    assert!(store.entries.is_empty());
    store.add(Path::new("/recovered"), 1);
    store.save_host(&path).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() <= MAX_STORE_FILE_BYTES as u64);
    assert_eq!(
        FrecencyStore::load_host(&path).entries,
        vec![entry("/recovered", 1.0, 1)]
    );
}

#[test]
fn overlong_and_multiline_paths_are_not_persisted() {
    let mut store = FrecencyStore::default();
    store.add(Path::new(&format!("/{}", "x".repeat(MAX_PATH_BYTES))), 1);
    store.add(Path::new("/line\nbreak"), 2);
    store.add(Path::new("/ordinary"), 3);

    assert_eq!(store.entries, vec![entry("/ordinary", 1.0, 3)]);
    assert!(store.serialize().len() <= MAX_STORE_FILE_BYTES);
}

#[test]
fn concurrent_atomic_saves_leave_one_complete_bounded_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jump.frecency");
    let barrier = Arc::new(Barrier::new(4));
    let mut workers = Vec::new();
    for index in 0..4 {
        let path = path.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let mut store = FrecencyStore::default();
            store.add(Path::new(&format!("/worker-{index}")), index);
            barrier.wait();
            store.save_host(&path)
        }));
    }
    for worker in workers {
        worker.join().unwrap().unwrap();
    }

    let loaded = FrecencyStore::load_host(&path);
    assert_eq!(loaded.entries.len(), 1);
    assert!(
        loaded.entries[0]
            .path
            .to_string_lossy()
            .starts_with("/worker-")
    );
    assert!(std::fs::metadata(path).unwrap().len() <= MAX_STORE_FILE_BYTES as u64);
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
        /// `None` intentionally keys the exception by its exact semantic
        /// token instead of a formatting-sensitive source line.
        line: Option<usize>,
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
            line: Some(179),
            text: "if m.is_dir() {",
        },
        ExpectedLine {
            file: "builtins.rs",
            line: Some(183),
            text: "} else if m.is_file() {",
        },
        ExpectedLine {
            file: "builtins.rs",
            line: Some(315),
            text: "if m.is_dir() {",
        },
        ExpectedLine {
            file: "builtins.rs",
            line: Some(372),
            text: "if meta.is_dir() {",
        },
        ExpectedLine {
            file: "builtins.rs",
            line: Some(506),
            text: "if !metadata.is_dir() || metadata.uid() != effective_uid || metadata.mode() & 0o077 != 0 {",
        },
        ExpectedLine {
            file: "builtins.rs",
            line: Some(520),
            text: "if fs.symlink_metadata(path)?.is_dir() {",
        },
        ExpectedLine {
            file: "builtins.rs",
            line: Some(567),
            text: "if !metadata.is_dir()",
        },
        ExpectedLine {
            file: "expr_access.rs",
            line: Some(415),
            text: ".map(|m| m.is_dir())",
        },
        ExpectedLine {
            file: "expr_access.rs",
            line: Some(422),
            text: ".map(|m| m.is_file())",
        },
        ExpectedLine {
            file: "script.rs",
            line: None,
            text: "metadata.is_file() && metadata.permissions().mode() & 0o111 != 0",
        },
    ];

    // The only ambient filesystem operations in evaluator production:
    // three calls implement the explicitly host-owned frecency database,
    // and one sets permissions on a TempDir the Rust-script runner itself
    // just created. Language-selected paths may not join this inventory.
    const AMBIENT_ALLOWLIST: &[ExpectedLine] = &[
        ExpectedLine {
            file: "frecency.rs",
            line: Some(322),
            text: "let Ok(reader) = StdFs.open_read(path) else {",
        },
        ExpectedLine {
            file: "frecency.rs",
            line: Some(349),
            text: "StdFs.create_dir_all(parent)?;",
        },
        ExpectedLine {
            file: "frecency.rs",
            line: Some(353),
            text: "StdFs.atomic_replace(path, output.as_bytes())",
        },
        ExpectedLine {
            file: "script.rs",
            line: None,
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
        expected.iter().position(|item| {
            item.file == file
                && item.line.is_none_or(|expected| expected == line)
                && item.text == text.trim()
        })
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
        if relative == "tests.rs" || relative.ends_with("/tests.rs") {
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
