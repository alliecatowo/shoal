use super::*;
use shoal_journal::{Journal, JournalOptions};
use shoal_value::{ReadSeek, StdFs};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct LockAfterWriteFs {
    db_path: PathBuf,
    fail_after_write: bool,
    blocker: Mutex<Option<rusqlite::Connection>>,
    writes: AtomicUsize,
}

impl LockAfterWriteFs {
    fn new(db_path: PathBuf, fail_after_write: bool) -> Self {
        Self {
            db_path,
            fail_after_write,
            blocker: Mutex::new(None),
            writes: AtomicUsize::new(0),
        }
    }

    fn lock_journal(&self) -> io::Result<()> {
        let connection = rusqlite::Connection::open(&self.db_path).map_err(io::Error::other)?;
        connection
            .busy_timeout(std::time::Duration::ZERO)
            .map_err(io::Error::other)?;
        connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(io::Error::other)?;
        *self.blocker.lock().unwrap() = Some(connection);
        Ok(())
    }

    fn release(&self) {
        self.blocker.lock().unwrap().take();
    }

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "journal failure test filesystem only supports writes",
        ))
    }
}

impl Fs for LockAfterWriteFs {
    fn read(&self, _path: &Path) -> io::Result<Vec<u8>> {
        Self::unsupported()
    }
    fn read_to_string(&self, _path: &Path) -> io::Result<String> {
        Self::unsupported()
    }
    fn open_read(&self, _path: &Path) -> io::Result<Box<dyn ReadSeek + Send>> {
        Self::unsupported()
    }
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        StdFs.write(path, data)?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.lock_journal()?;
        if self.fail_after_write {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected primary write failure after bytes reached the filesystem",
            ))
        } else {
            Ok(())
        }
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

#[derive(Default)]
struct RecordingDenyAtomicFs {
    operations: Mutex<Vec<String>>,
}

impl RecordingDenyAtomicFs {
    fn record(&self, operation: &str, path: &Path) {
        self.operations
            .lock()
            .unwrap()
            .push(format!("{operation}:{}", path.display()));
    }

    fn operations(&self) -> Vec<String> {
        self.operations.lock().unwrap().clone()
    }
}

impl Fs for RecordingDenyAtomicFs {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.record("read", path);
        StdFs.read(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        StdFs.read_to_string(path)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek + Send>> {
        StdFs.open_read(path)
    }
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        StdFs.write(path, data)
    }
    fn append(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        StdFs.append(path, data)
    }
    fn touch(&self, path: &Path) -> io::Result<()> {
        StdFs.touch(path)
    }
    fn metadata(&self, path: &Path) -> io::Result<std::fs::Metadata> {
        StdFs.metadata(path)
    }
    fn symlink_metadata(&self, path: &Path) -> io::Result<std::fs::Metadata> {
        self.record("symlink_metadata", path);
        StdFs.symlink_metadata(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.record("exists", path);
        StdFs.exists(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        StdFs.is_file(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        StdFs.is_dir(path)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.record("canonicalize", path);
        StdFs.canonicalize(path)
    }
    fn atomic_replace(&self, path: &Path, _data: &[u8]) -> io::Result<()> {
        self.record("atomic_replace", path);
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "test adapter denied atomic replacement",
        ))
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        StdFs.read_dir(path)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.record("create_dir", path);
        StdFs.create_dir(path)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        StdFs.create_dir_all(path)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        StdFs.remove_file(path)
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        StdFs.remove_dir_all(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.record("rename", from);
        StdFs.rename(from, to)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        StdFs.copy(from, to)
    }
    fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()> {
        StdFs.hard_link(src, dst)
    }
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        StdFs.symlink(target, link)
    }
}

/// Build a journaled evaluator rooted at `cwd` with an in-memory journal.
///
/// `cwd` is canonicalized first: `undo_entry` canonicalizes its `root`
/// argument before checking that a recorded undo target is contained in
/// it (`checked_target`), and on macOS a tempdir's path is a symlink
/// alias (`/var/folders/...` -> `/private/var/folders/...`). Rooting the
/// evaluator at the already-canonical path keeps every path it records
/// (via `self.exec.shell.cwd.join(...)`) on the same prefix `undo_entry` compares
/// against — mirroring the fix already applied to shoal-journal's own
/// `root.path().canonicalize()` tests.
fn journaled(cwd: &Path) -> Evaluator {
    let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut ev = Evaluator::new(root);
    ev.set_journal(Journal::in_memory().unwrap(), "s1", "human");
    ev
}

fn run_journaled(ev: &mut Evaluator, src: &str) -> VResult<Value> {
    let program = shoal_syntax::parse(src).expect("parse");
    ev.set_source(src);
    ev.eval_program(&program)
}

#[test]
fn statement_records_entry_finish_and_output() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = journaled(dir.path());
    run_journaled(&mut ev, "echo hi").unwrap();
    let journal = ev.session.journal.as_ref().unwrap();
    let rows = journal.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows.len(), 1, "one entry recorded");
    let entry = &rows[0];
    assert_eq!(entry.src, "echo hi");
    assert_eq!(entry.principal, "human");
    assert_eq!(entry.session, "s1");
    // finish() ran: status/ok/duration are populated.
    assert_eq!(entry.ok, Some(true));
    assert_eq!(entry.status, Some(0));
    assert!(entry.dur_ns.is_some());
    // outputs captured: a render, plus echo's stdout.
    let kinds: Vec<&str> = entry.outputs.iter().map(|o| o.kind.as_str()).collect();
    assert!(kinds.contains(&"render"), "outputs: {kinds:?}");
    assert!(kinds.contains(&"stdout"), "outputs: {kinds:?}");
}

#[test]
fn host_execution_returns_exact_last_entry_and_records_parentage() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = journaled(dir.path());
    let program = shoal_syntax::parse("let first = 1\nfirst + 1").unwrap();
    ev.set_source("let first = 1\nfirst + 1");
    ev.begin_journal_execution(Some(42));
    ev.eval_program(&program).unwrap();
    let last_id = ev
        .take_last_journal_entry()
        .expect("a successful journaled evaluation returns its exact final row");
    assert_eq!(
        ev.take_last_journal_entry(),
        None,
        "entry ids are consumed once"
    );

    let rows = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, last_id);
    assert!(
        rows.iter()
            .all(|row| row.kind == shoal_journal::EntryKind::Statement)
    );
    assert!(rows.iter().all(|row| row.parent_id == Some(42)));
}

fn zero_timeout_journal(state: &Path) -> Journal {
    Journal::open_with_options(
        state,
        JournalOptions {
            busy_timeout: std::time::Duration::ZERO,
            ..JournalOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn installed_journal_begin_failure_prevents_statement_effects() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let journal = zero_timeout_journal(&state);
    let blocker = rusqlite::Connection::open(state.join("journal.db")).unwrap();
    blocker.busy_timeout(std::time::Duration::ZERO).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

    let target = dir.path().join("must-not-exist");
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());
    evaluator.set_journal(journal, "session", "human");
    let error = run_journaled(
        &mut evaluator,
        &format!("save(\"{}\", \"payload\")", target.display()),
    )
    .unwrap_err();

    assert_eq!(error.code, "journal_begin_failed");
    assert!(error.msg.contains("before statement execution"));
    assert!(!target.exists(), "the language effect must not have run");
    drop(blocker);
    let rows = evaluator
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    assert!(rows.is_empty(), "a failed begin cannot invent an entry");
}

#[test]
fn exhausted_journal_budget_refuses_before_filesystem_effects() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let journal = Journal::open_with_options(
        &state,
        JournalOptions {
            database_max_bytes: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let target = dir.path().join("must-not-exist-budget");
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());
    evaluator.set_journal(journal, "session", "human");
    let error = run_journaled(
        &mut evaluator,
        &format!("save(\"{}\", \"payload\")", target.display()),
    )
    .unwrap_err();
    assert_eq!(error.code, "journal_begin_failed");
    assert!(!target.exists(), "begin refusal must precede the effect");
    assert!(
        evaluator
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn exhausted_cas_budget_marks_post_effect_completion_indeterminate() {
    let dir = tempfile::tempdir().unwrap();
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());
    evaluator.set_journal(
        Journal::in_memory_with_options(JournalOptions {
            cas_max_bytes: 1,
            ..Default::default()
        })
        .unwrap(),
        "session",
        "human",
    );
    let error = run_journaled(&mut evaluator, "echo output").unwrap_err();
    assert_eq!(error.code, "journal_commit_indeterminate");
    assert!(error.msg.contains("output"), "{}", error.msg);
    let rows = evaluator
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ok, Some(false));
    assert!(rows[0].outputs.is_empty());
}

#[test]
fn finish_failure_reports_that_effects_may_have_occurred() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let journal = zero_timeout_journal(&state);
    let fs = Arc::new(LockAfterWriteFs::new(state.join("journal.db"), false));
    let target = dir.path().join("written-before-finish");
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());
    evaluator.set_journal(journal, "session", "human");
    evaluator.set_fs(fs.clone());

    let error = run_journaled(
        &mut evaluator,
        &format!("save(\"{}\", \"payload\")", target.display()),
    )
    .unwrap_err();
    assert_eq!(error.code, "journal_commit_indeterminate");
    assert!(error.msg.contains("effects may already have occurred"));
    assert!(error.hint.unwrap().contains("do not blindly retry"));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "payload");
    assert_eq!(fs.writes.load(Ordering::SeqCst), 1);

    fs.release();
    let rows = evaluator
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ok, None, "failed finish leaves an honest open row");
}

#[test]
fn primary_and_journal_failures_are_reported_deterministically() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let journal = zero_timeout_journal(&state);
    let fs = Arc::new(LockAfterWriteFs::new(state.join("journal.db"), true));
    let target = dir.path().join("possibly-written");
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());
    evaluator.set_journal(journal, "session", "human");
    evaluator.set_fs(fs.clone());

    let error = run_journaled(
        &mut evaluator,
        &format!("save(\"{}\", \"payload\")", target.display()),
    )
    .unwrap_err();
    assert_eq!(error.code, "journal_commit_indeterminate");
    assert!(error.msg.contains("primary error was custom"));
    assert!(error.msg.contains("injected primary write failure"));
    assert!(error.msg.contains("effects may already have occurred"));
    assert_eq!(std::fs::read_to_string(target).unwrap(), "payload");
    fs.release();
}

#[test]
fn disabled_language_journal_preserves_ordinary_evaluation() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("ordinary");
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());
    let source = format!("save(\"{}\", \"payload\")", target.display());
    let value = evaluator
        .eval_program(&shoal_syntax::parse(&source).unwrap())
        .unwrap();
    assert_eq!(value, Value::Str("payload".into()));
    assert_eq!(std::fs::read_to_string(target).unwrap(), "payload");
}

#[test]
fn journal_entry_text_and_json_fields_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let mut evaluator = Evaluator::new(dir.path().to_path_buf());
    evaluator.set_journal(
        Journal::in_memory().unwrap(),
        "s".repeat(MAX_JOURNAL_IDENTITY_BYTES * 2),
        "p".repeat(MAX_JOURNAL_IDENTITY_BYTES * 2),
    );
    let source = format!("\"{}\"", "x".repeat(MAX_JOURNAL_AST_BYTES + 1024));
    run_journaled(&mut evaluator, &source).unwrap();

    let rows = evaluator
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].session.len() <= MAX_JOURNAL_IDENTITY_BYTES);
    assert!(rows[0].principal.len() <= MAX_JOURNAL_IDENTITY_BYTES);
    assert!(rows[0].src.len() <= MAX_JOURNAL_SOURCE_BYTES);
    assert!(rows[0].src.contains("journal field truncated"));
    assert!(rows[0].ast_json.len() <= MAX_JOURNAL_AST_BYTES);
    assert!(rows[0].ast_json.contains("AST exceeds journal byte limit"));
    assert!(rows[0].effects_json.len() <= MAX_JOURNAL_EFFECT_BYTES);
}

#[test]
fn rm_records_trash_undo_and_undo_restores() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("victim"), b"payload").unwrap();
    let mut ev = journaled(dir.path());
    run_journaled(&mut ev, "rm victim").unwrap();
    assert!(!dir.path().join("victim").exists(), "rm trashed the file");
    // A trash inverse was recorded.
    let rows = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    let undos = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .undos_for(rows[0].id)
        .unwrap();
    assert_eq!(undos.len(), 1);
    assert_eq!(undos[0].0, "trash_move");
    // `undo` moves it back with its original bytes.
    let report = run_journaled(&mut ev, "undo").unwrap();
    assert!(dir.path().join("victim").exists(), "undo restored the file");
    assert_eq!(
        std::fs::read(dir.path().join("victim")).unwrap(),
        b"payload"
    );
    assert!(matches!(report, Value::Record(_)));
}

#[test]
fn overwrite_records_restore_bytes_and_undo_restores_prior() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), b"original").unwrap();
    let mut ev = journaled(dir.path());
    run_journaled(&mut ev, r#"save("f.txt", "replacement")"#).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("f.txt")).unwrap(),
        b"replacement"
    );
    let rows = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    let undos = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .undos_for(rows[0].id)
        .unwrap();
    assert_eq!(undos[0].0, "restore_bytes");
    // `undo` brings back the prior contents.
    run_journaled(&mut ev, "undo").unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("f.txt")).unwrap(),
        b"original"
    );
}

#[test]
fn undo_replay_is_mediated_and_atomic_replace_denial_is_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    // macOS exposes temporary paths through a `/var` symlink whose canonical
    // identity begins `/private/var`. Journal inverses are root-canonical, so
    // compare adapter observations against that same authority identity.
    let root = dir.path().canonicalize().unwrap();
    let target = root.join("f.txt");
    std::fs::write(&target, b"original").unwrap();
    let mut ev = journaled(&root);
    run_journaled(&mut ev, r#"save("f.txt", "replacement")"#).unwrap();

    let fs = Arc::new(RecordingDenyAtomicFs::default());
    ev.set_fs(fs.clone());
    let error = run_journaled(&mut ev, "undo").unwrap_err();
    assert_eq!(error.code, "custom");
    assert!(
        error.msg.contains("test adapter denied atomic replacement"),
        "{}",
        error.msg
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"replacement");

    let operations = fs.operations();
    assert!(
        operations
            .iter()
            .any(|op| op == &format!("exists:{}", target.display())),
        "existence probe escaped the adapter: {operations:?}"
    );
    assert!(
        operations
            .iter()
            .any(|op| op == &format!("read:{}", target.display())),
        "fingerprint read escaped the adapter: {operations:?}"
    );
    assert!(
        operations
            .iter()
            .any(|op| op == &format!("atomic_replace:{}", target.display())),
        "restore escaped the adapter: {operations:?}"
    );
}

/// When prior contents exceed the journal's `output_hard_cap`,
/// the undo snapshot would be stored truncated (partial bytes + marker).
/// `snapshot_prior` must refuse to record a replayable `RestoreBytes`
/// inverse in that case, so `undo` can never restore corrupt partial content
/// over the user's file. The op is simply left non-reversible.
#[test]
fn overwrite_larger_than_cap_records_no_reversible_inverse() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir
        .path()
        .canonicalize()
        .unwrap_or_else(|_| dir.path().to_path_buf());
    // Prior contents (500 bytes) exceed the 128-byte cap → snapshot truncates.
    std::fs::write(root.join("f.txt"), vec![b'a'; 500]).unwrap();
    let mut ev = Evaluator::new(root.clone());
    ev.set_journal(
        Journal::in_memory_with_options(JournalOptions {
            output_hard_cap: 128,
            ..Default::default()
        })
        .unwrap(),
        "s1",
        "human",
    );

    run_journaled(&mut ev, r#"save("f.txt", "small-replacement")"#).unwrap();
    assert_eq!(
        std::fs::read(root.join("f.txt")).unwrap(),
        b"small-replacement"
    );

    // No restore_bytes inverse was recorded for the truncated snapshot.
    let rows = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    let undos = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .undos_for(rows[0].id)
        .unwrap();
    assert!(
        undos.is_empty(),
        "a truncated snapshot must not key a replayable inverse; got {undos:?}"
    );

    // And `undo` therefore has nothing to restore — it never writes the
    // truncated+marker bytes over the file.
    let err = run_journaled(&mut ev, "undo").unwrap_err();
    assert_eq!(err.code, "custom", "{}", err.msg);
    assert!(err.msg.contains("nothing to undo"), "{}", err.msg);
}

#[test]
fn sparse_prior_above_snapshot_wall_is_non_reversible_and_journal_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir
        .path()
        .canonicalize()
        .unwrap_or_else(|_| dir.path().to_path_buf());
    let target = root.join("sparse.bin");
    let file = std::fs::File::create(&target).unwrap();
    file.set_len((MAX_JOURNAL_UNDO_SNAPSHOT_BYTES + 1) as u64)
        .unwrap();
    let mut ev = journaled(&root);

    run_journaled(&mut ev, r#"save("sparse.bin", "replacement")"#).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
    let rows = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    assert!(
        ev.session
            .journal
            .as_ref()
            .unwrap()
            .undos_for(rows[0].id)
            .unwrap()
            .is_empty(),
        "an oversized sparse prior must never receive a truncated inverse"
    );

    run_journaled(&mut ev, "echo recovered").expect("journal remains usable after refusal");
}

#[test]
fn cp_overwrite_records_restore_bytes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("src"), b"newbytes").unwrap();
    std::fs::write(dir.path().join("dst"), b"prior-dst").unwrap();
    let mut ev = journaled(dir.path());
    run_journaled(&mut ev, "cp src dst").unwrap();
    assert_eq!(std::fs::read(dir.path().join("dst")).unwrap(), b"newbytes");
    run_journaled(&mut ev, "undo").unwrap();
    assert_eq!(std::fs::read(dir.path().join("dst")).unwrap(), b"prior-dst");
}

#[test]
fn redirect_out_overwrite_records_restore_bytes_and_undo_restores() {
    // `echo x > f` clobbers an existing file's contents exactly like `cp`,
    // so it must record a `RestoreBytes` inverse and be reversible.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), b"original\n").unwrap();
    let mut ev = journaled(dir.path());
    run_journaled(&mut ev, "echo replaced > f.txt").unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("f.txt")).unwrap(),
        b"replaced\n"
    );
    let rows = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    let undos = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .undos_for(rows[0].id)
        .unwrap();
    assert_eq!(undos[0].0, "restore_bytes");
    run_journaled(&mut ev, "undo").unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("f.txt")).unwrap(),
        b"original\n"
    );
}

#[test]
fn redirect_append_records_restore_bytes_and_undo_restores_prior() {
    // `>>` grows the file; undo restores the full prior contents (dropping
    // the appended bytes) via the same overwrite `RestoreBytes` inverse.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("log.txt"), b"first\n").unwrap();
    let mut ev = journaled(dir.path());
    run_journaled(&mut ev, "echo second >> log.txt").unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("log.txt")).unwrap(),
        b"first\nsecond\n"
    );
    let rows = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    let undos = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .undos_for(rows[0].id)
        .unwrap();
    assert_eq!(undos[0].0, "restore_bytes");
    run_journaled(&mut ev, "undo").unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("log.txt")).unwrap(),
        b"first\n"
    );
}

#[test]
fn external_redirect_overwrite_records_restore_bytes_and_undo_restores() {
    // The external-command redirect site (`command.rs`) must wrap its write
    // too: `some-cmd > f` / `sh -c '…' > f` is reversible like a builtin's.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), b"original").unwrap();
    let mut ev = journaled(dir.path());
    run_journaled(&mut ev, "sh -c 'printf replaced' > f.txt").unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("f.txt")).unwrap(),
        b"replaced"
    );
    let rows = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    let undos = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .undos_for(rows[0].id)
        .unwrap();
    assert_eq!(undos[0].0, "restore_bytes");
    run_journaled(&mut ev, "undo").unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("f.txt")).unwrap(),
        b"original"
    );
}

#[test]
fn redirect_to_new_file_records_no_inverse() {
    // A redirect that CREATES a new file has no reversible inverse yet:
    // `UndoInverse` carries no create/delete variant, so create-new is left
    // non-reversible (documented follow-up) rather than faked.
    let dir = tempfile::tempdir().unwrap();
    let mut ev = journaled(dir.path());
    run_journaled(&mut ev, "echo fresh > new.txt").unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("new.txt")).unwrap(),
        b"fresh\n"
    );
    let rows = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    let undos = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .undos_for(rows[0].id)
        .unwrap();
    assert!(
        undos.is_empty(),
        "create-new redirect must not fake an inverse; got {undos:?}"
    );
    let err = run_journaled(&mut ev, "undo").unwrap_err();
    assert!(err.msg.contains("nothing to undo"), "{}", err.msg);
}

#[test]
fn redirect_overwrite_larger_than_cap_records_no_reversible_inverse() {
    // The truncation guard applies to redirects too: prior contents that
    // exceed the CAS cap would snapshot truncated, so `snapshot_prior`
    // refuses and no replayable `RestoreBytes` is keyed — undo can never
    // overwrite the file with corrupt partial bytes.
    let dir = tempfile::tempdir().unwrap();
    let root = dir
        .path()
        .canonicalize()
        .unwrap_or_else(|_| dir.path().to_path_buf());
    std::fs::write(root.join("f.txt"), vec![b'a'; 500]).unwrap();
    let mut ev = Evaluator::new(root.clone());
    ev.set_journal(
        Journal::in_memory_with_options(JournalOptions {
            output_hard_cap: 128,
            ..Default::default()
        })
        .unwrap(),
        "s1",
        "human",
    );
    run_journaled(&mut ev, "echo small > f.txt").unwrap();
    let rows = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .query(&JournalQuery::default())
        .unwrap();
    let undos = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .undos_for(rows[0].id)
        .unwrap();
    assert!(
        undos.is_empty(),
        "a truncated prior snapshot must not key a replayable inverse; got {undos:?}"
    );
}

#[test]
fn stale_target_makes_undo_refuse() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), b"original").unwrap();
    let mut ev = journaled(dir.path());
    run_journaled(&mut ev, r#"save("f.txt", "replacement")"#).unwrap();
    // Someone else changes the file after the recorded op.
    std::fs::write(dir.path().join("f.txt"), b"tampered-by-someone-else").unwrap();
    let err = run_journaled(&mut ev, "undo").unwrap_err();
    assert_eq!(err.code, "stale_undo", "{}", err.msg);
    // The tampered content is untouched — undo refused rather than clobber.
    assert_eq!(
        std::fs::read(dir.path().join("f.txt")).unwrap(),
        b"tampered-by-someone-else"
    );
}

#[test]
fn journal_builtin_returns_entries_as_table() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = journaled(dir.path());
    run_journaled(&mut ev, "echo one").unwrap();
    run_journaled(&mut ev, "echo two").unwrap();
    let table = run_journaled(&mut ev, "journal").unwrap();
    let Value::Table(rows) = table else {
        panic!("journal should be a table, got {table:?}")
    };
    // Two prior statements are present (the `journal` call itself is the
    // third, newest-first).
    assert!(rows.len() >= 3, "rows: {}", rows.len());
    assert!(rows.iter().all(|r| r.get("id").is_some()));
    // The `src` column carries the FULL statement source, not just the head
    // word (regression: the view used to slice off everything after the
    // first space, so a populated `src` still rendered as good as empty).
    assert!(
        rows.iter()
            .any(|r| r.get("src") == Some(&Value::Str("echo one".into()))),
        "src column should show the full source line: {rows:?}"
    );
}

#[test]
fn journal_view_src_column_shows_full_source_not_head() {
    // Regression (BUG: empty/head-only `src` column): the `history`/
    // `journal` view must render the ENTIRE recorded source under `src`,
    // not just the first whitespace-delimited word. A multi-token command
    // whose head alone is uninformative proves the full line survives.
    let dir = tempfile::tempdir().unwrap();
    let mut ev = journaled(dir.path());
    run_journaled(&mut ev, "echo alpha beta gamma").unwrap();
    let table = run_journaled(&mut ev, "journal").unwrap();
    let Value::Table(rows) = table else {
        panic!("journal should be a table, got {table:?}")
    };
    assert!(
        rows.iter()
            .any(|r| r.get("src") == Some(&Value::Str("echo alpha beta gamma".into()))),
        "full source expected in the src column, got: {rows:?}"
    );
}

#[test]
fn no_journal_evaluator_records_nothing_and_view_is_empty() {
    // The zero-regression path: a plain evaluator journals nothing and the
    // `journal` builtin returns an empty table instead of crashing.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"x").unwrap();
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    assert!(!ev.has_journal());
    let program = shoal_syntax::parse("rm x\njournal").unwrap();
    let out = ev.eval_program(&program).unwrap();
    assert_eq!(out, Value::Table(Vec::new()), "empty journal view");
    assert!(
        !dir.path().join("x").exists(),
        "rm still works with no journal"
    );
}

#[test]
fn undo_without_journal_errors_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    let program = shoal_syntax::parse("undo").unwrap();
    let err = ev.eval_program(&program).unwrap_err();
    assert_eq!(err.code, "custom");
    assert!(err.msg.contains("journaled session"), "{}", err.msg);
}

/// site/content/internals/language-conformance-contract.md disk-spill: a value-position capture whose stdout exceeds the
/// RAM cap is preserved to the CAS as a ref-backed value — `.len` is the
/// true (full) length, the blob exists and its blake3 matches, render shows
/// a bounded preview + the `val:blake3:…` ref (not the whole thing), and
/// materialization loads the correct full bytes. Nothing is lost.
#[test]
fn value_capture_over_cap_spills_to_cas_ref_backed() {
    let dir = tempfile::tempdir().unwrap();
    let prev_cap = shoal_exec::capture_hard_cap();
    shoal_exec::set_capture_hard_cap(4096);
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.set_journal(Journal::open(dir.path()).unwrap(), "s1", "human");

    // A deterministic 200_000-byte capture (200_000 NUL bytes) past the cap.
    let v = run_journaled(&mut ev, "let x = sh { head -c 200000 /dev/zero }\nx.stdout").unwrap();

    // The value is ref-backed, carrying the true length + a bounded preview.
    let Value::CasBytes(c) = &v else {
        shoal_exec::set_capture_hard_cap(prev_cap);
        panic!("expected a ref-backed CasBytes, got {}", v.type_name());
    };
    assert_eq!(c.len, 200_000, ".len is the true length, not the preview");
    assert!(
        c.preview.len() <= 4096,
        "preview stays bounded by the RAM cap"
    );
    assert!(!c.truncated, "the full stream fit under the spill cap");
    let hash = c.hash.clone();

    // `.len` answers the TRUE length through method dispatch, without loading.
    assert_eq!(
        run_journaled(&mut ev, "x.stdout.len").unwrap(),
        Value::Int(200_000)
    );

    // Render is a bounded preview + the recoverable ref — not the full 200KB.
    let inline = shoal_value::render::render_inline(&v);
    assert!(
        inline.contains("val:blake3:") && inline.contains(&hash),
        "inline render carries the ref: {inline}"
    );
    let block = shoal_value::render::render_block(&v, 80);
    assert!(block.contains(&hash), "block render carries the ref");
    assert!(
        block.len() < 100_000,
        "render shows a preview, not the whole content ({} bytes)",
        block.len()
    );

    let expected = vec![0u8; 200_000];

    // Incremental language surfaces use the CAS reader instead of first
    // resolving the full blob. This unframed-but-sub-line-limit capture yields
    // one logical line, and `.save` copies the exact bytes to the Fs sink.
    let streamed = run_journaled(&mut ev, "x.stdout.stream().collect()").unwrap();
    assert!(matches!(streamed, Value::List(lines) if
        matches!(lines.as_slice(), [Value::Str(line)] if line.as_bytes() == expected)));
    let saved_path = dir.path().join("spilled-output.bin");
    run_journaled(&mut ev, "x.stdout.save(path(\"spilled-output.bin\"))").unwrap();
    assert_eq!(std::fs::read(saved_path).unwrap(), expected);

    // The CAS blob exists and its blake3 matches (Cas::read re-hashes and
    // verifies the content against `hash` before returning it).
    let cas = ev.session.journal.as_ref().unwrap().cas();
    assert_eq!(
        cas.read(&hash).unwrap(),
        expected,
        "the CAS blob is the full, verbatim capture"
    );

    // Materialization loads the correct full bytes from the CAS.
    let loaded = run_journaled(&mut ev, "x.stdout.load()").unwrap();
    assert_eq!(loaded, Value::Bytes(std::sync::Arc::new(expected)));

    // The lazy value, not the evaluator/session, owns the final lease. It
    // remains protected after its originating evaluator is gone and releases
    // automatically when the last ref-backed value clone is dropped.
    drop(ev);
    let observer = shoal_journal::Journal::open(dir.path()).unwrap();
    assert_eq!(observer.protected_hashes().unwrap(), vec![hash.clone()]);
    drop(observer);
    drop(v);
    let observer = shoal_journal::Journal::open(dir.path()).unwrap();
    assert!(observer.protected_hashes().unwrap().is_empty());

    shoal_exec::set_capture_hard_cap(prev_cap);
}

/// Zero-regression: a sub-cap value-position capture stays fully resident —
/// a plain `bytes`, no spill, no CAS blob — exactly as before capture spill was added.
#[test]
fn value_capture_under_cap_stays_resident() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = journaled(dir.path());
    let v = run_journaled(&mut ev, "let y = sh { head -c 100 /dev/zero }\ny.stdout").unwrap();
    assert!(
        matches!(v, Value::Bytes(_)),
        "sub-cap output is fully resident, got {}",
        v.type_name()
    );
    assert_eq!(
        run_journaled(&mut ev, "y.stdout.len").unwrap(),
        Value::Int(100)
    );
    assert!(
        ev.session
            .journal
            .as_ref()
            .unwrap()
            .protected_hashes()
            .unwrap()
            .is_empty(),
        "no spill blob is pinned for a sub-cap capture"
    );
}

/// site/content/internals/language-conformance-contract.md in-language dispatch follow-up: a bare `val:blake3:<hash>`
/// content ref *written as a value* (the short-ref `.ref` yields) is
/// resolvable in-language — calling a method on it loads the bytes from the
/// session CAS and dispatches on the resulting lazy `bytes`, so a recovered
/// ref answers `.len`, materializes, and round-trips `.ref` exactly like the
/// capture it came from. An unknown hash is a clean `not_found`, and an
/// ordinary (non-ref) string still dispatches string methods unchanged.
#[test]
fn val_blake3_ref_string_dispatches_through_the_cas() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = journaled(dir.path());
    // Seed the session CAS with a known blob directly (no spill needed):
    // `record_output` writes the blake3-addressed blob and its `blob` row,
    // which is exactly what a spilled capture leaves behind.
    let content = b"hello, cas-backed world!\n".repeat(40); // 1000 bytes, valid UTF-8
    let hash = ev
        .session
        .journal
        .as_ref()
        .unwrap()
        .record_output(1, "value", &content)
        .unwrap();
    let reference = format!("val:blake3:{hash}");

    // `.len` answers the TRUE content length from the blob metadata — the
    // ref string is resolved to a lazy CAS-backed `bytes`, never measured as
    // a plain string (which would report the 75-odd characters of the ref).
    assert_eq!(
        run_journaled(&mut ev, &format!("\"{reference}\".len")).unwrap(),
        Value::Int(content.len() as i64)
    );
    // Materialization loads the exact bytes from the CAS.
    assert_eq!(
        run_journaled(&mut ev, &format!("\"{reference}\".load.len")).unwrap(),
        Value::Int(content.len() as i64)
    );
    assert_eq!(
        run_journaled(
            &mut ev,
            &format!("\"{reference}\".str().starts_with(\"hello\")")
        )
        .unwrap(),
        Value::Bool(true)
    );
    // `.ref` round-trips the recoverable handle unchanged.
    assert_eq!(
        run_journaled(&mut ev, &format!("\"{reference}\".ref")).unwrap(),
        Value::Str(reference.clone())
    );
    // An unknown hash is a clean `not_found`, not a wrong string-length.
    let unknown = format!("val:blake3:{}", "0".repeat(64));
    let err = run_journaled(&mut ev, &format!("\"{unknown}\".len")).unwrap_err();
    assert_eq!(err.code, "not_found", "unknown hash: {}", err.msg);
    // A non-ref string still dispatches string methods verbatim.
    assert_eq!(
        run_journaled(&mut ev, "\"hello\".len").unwrap(),
        Value::Int(5)
    );
}

/// The same ref grammar is inert without a journal/CAS: a `val:blake3:`
/// string then errors clearly rather than silently measuring itself as a
/// string (the corpus / `-c` path installs no journal, so this is the
/// common case for a stray ref-shaped literal).
#[test]
fn val_blake3_ref_without_journal_errors_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    let program = shoal_syntax::parse(&format!("\"val:blake3:{}\".len", "a".repeat(64))).unwrap();
    let err = ev.eval_program(&program).unwrap_err();
    assert_eq!(err.code, "not_found");
    assert!(err.msg.contains("journal/CAS"), "{}", err.msg);
}
