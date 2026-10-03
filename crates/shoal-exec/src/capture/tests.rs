use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Barrier, MutexGuard};

static LIMIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn limit_test_guard() -> MutexGuard<'static, ()> {
    LIMIT_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A bounded producer emitting more than `cap` bytes fills the buffer
/// to exactly the cap and reports truncation — the buffer never grows past
/// the bound, so an unbounded child can't OOM the shell.
#[test]
fn drain_capped_stops_at_cap_and_flags_truncation() {
    let _guard = limit_test_guard();
    let cap = 4096;
    let producer = vec![b'x'; cap + 5000];
    let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(producer));
    let (buf, truncated) = drain_capped(r, cap);
    assert_eq!(buf.len(), cap, "buffer must stop growing at the cap");
    assert!(buf.iter().all(|&b| b == b'x'));
    assert!(truncated, "dropping overflow must set the truncated flag");
}

/// Output at or under the cap is captured whole with no truncation flag.
#[test]
fn drain_capped_keeps_output_within_cap() {
    let _guard = limit_test_guard();
    let cap = 4096;
    let producer = vec![b'y'; 1000];
    let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(producer));
    let (buf, truncated) = drain_capped(r, cap);
    assert_eq!(buf.len(), 1000);
    assert!(!truncated);
}

/// Exactly-cap-sized output is not falsely flagged as truncated.
#[test]
fn drain_capped_exact_cap_is_not_truncated() {
    let _guard = limit_test_guard();
    let cap = 4096;
    let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(vec![b'z'; cap]));
    let (buf, truncated) = drain_capped(r, cap);
    assert_eq!(buf.len(), cap);
    assert!(!truncated, "an exact fit is complete, not truncated");
}

/// site/content/internals/process-execution.md disk-spill: a stream past the RAM cap streams the FULL content to a
/// blake3-addressed file, keeps a bounded preview, and reports the true len
/// and hash — nothing is lost (contrast `drain_capped`, which drops it).
#[test]
fn drain_stdout_spills_full_stream_to_disk() {
    let _guard = limit_test_guard();
    let cap = 4096;
    let dir = tempfile::tempdir().unwrap();
    let payload = vec![b'x'; cap + 5000];
    let expect_hash = blake3::hash(&payload).to_hex().to_string();
    let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(payload.clone()));
    let out = drain_stdout(
        r,
        cap,
        Some(SpillConfig {
            dir: dir.path().to_path_buf(),
        }),
        1 << 30,
    );
    assert!(!out.truncated, "spilled output is preserved, not truncated");
    assert_eq!(out.buf.len(), cap, "resident buffer is a bounded preview");
    let spill = out.spill.expect("overflow must produce a spill");
    assert_eq!(spill.len, payload.len() as u64, "spill len is the TRUE len");
    assert!(!spill.truncated);
    assert_eq!(spill.hash, expect_hash, "hash addresses the full content");
    // The on-disk file is exactly the full stream.
    let on_disk = std::fs::read(&spill.path).unwrap();
    assert_eq!(on_disk, payload);
    assert_eq!(blake3::hash(&on_disk).to_hex().to_string(), expect_hash);
}

/// Sub-cap output with a spill configured stays fully resident and never
/// touches disk — zero regression for the common case.
#[test]
fn drain_stdout_under_cap_stays_resident_no_spill() {
    let _guard = limit_test_guard();
    let cap = 4096;
    let dir = tempfile::tempdir().unwrap();
    let payload = vec![b'y'; 1000];
    let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(payload.clone()));
    let out = drain_stdout(
        r,
        cap,
        Some(SpillConfig {
            dir: dir.path().to_path_buf(),
        }),
        1 << 30,
    );
    assert_eq!(out.buf, payload);
    assert!(out.spill.is_none(), "sub-cap output must not spill");
    assert!(!out.truncated);
    // The spill dir stays empty (no file created).
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

/// A spill that itself exceeds `spill_cap` is bounded on disk too, and flags
/// its own truncation — `let x = (yes)` fills neither RAM nor disk.
#[test]
fn drain_stdout_spill_is_bounded_by_spill_cap() {
    let _guard = limit_test_guard();
    let cap = 1024;
    let spill_cap = 8192u64;
    let dir = tempfile::tempdir().unwrap();
    let payload = vec![b'z'; 100_000];
    let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(payload));
    let out = drain_stdout(
        r,
        cap,
        Some(SpillConfig {
            dir: dir.path().to_path_buf(),
        }),
        spill_cap,
    );
    let spill = out.spill.expect("overflow must produce a spill");
    assert_eq!(spill.len, spill_cap, "disk is bounded by the spill cap");
    assert!(spill.truncated, "hitting the spill cap flags truncation");
    let on_disk = std::fs::read(&spill.path).unwrap();
    assert_eq!(on_disk.len(), spill_cap as usize);
    assert_eq!(spill.hash, blake3::hash(&on_disk).to_hex().to_string());
}

/// With no spill configured, stdout draining is byte-identical to the
/// pre-spill `drain_capped` behavior: bounded + flagged.
#[test]
fn drain_stdout_without_spill_matches_capped() {
    let _guard = limit_test_guard();
    let cap = 4096;
    let payload = vec![b'x'; cap + 100];
    let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(payload));
    let out = drain_stdout(r, cap, None, 1 << 30);
    assert_eq!(out.buf.len(), cap);
    assert!(out.truncated);
    assert!(out.spill.is_none());
}

/// The configurable cap resolves (default or override) to a positive bound.
#[test]
fn capture_hard_cap_is_positive_and_overridable() {
    let _guard = limit_test_guard();
    let old_hard = crate::capture_hard_cap();
    let old_spill = crate::capture_spill_cap();
    let old_memory = crate::capture_aggregate_memory_cap();
    let old_aggregate_spill = crate::capture_aggregate_spill_cap();
    assert!(crate::capture_hard_cap() > 0);
    crate::set_capture_hard_cap(1234);
    assert_eq!(crate::capture_hard_cap(), 1234);
    crate::set_capture_hard_cap(0);
    assert_eq!(
        crate::capture_hard_cap(),
        1,
        "zero is clamped to a positive cap"
    );
    crate::set_capture_hard_cap(usize::MAX);
    assert_eq!(crate::capture_hard_cap(), crate::MAX_CAPTURE_HARD_CAP);
    crate::set_capture_spill_cap(u64::MAX);
    assert_eq!(crate::capture_spill_cap(), crate::MAX_CAPTURE_SPILL_CAP);
    crate::set_capture_aggregate_memory_cap(usize::MAX);
    assert_eq!(
        crate::capture_aggregate_memory_cap(),
        crate::MAX_CAPTURE_AGGREGATE_MEMORY_CAP
    );
    crate::set_capture_aggregate_spill_cap(u64::MAX);
    assert_eq!(
        crate::capture_aggregate_spill_cap(),
        crate::MAX_CAPTURE_AGGREGATE_SPILL_CAP
    );
    crate::set_capture_hard_cap(old_hard);
    crate::set_capture_spill_cap(old_spill);
    crate::set_capture_aggregate_memory_cap(old_memory);
    crate::set_capture_aggregate_spill_cap(old_aggregate_spill);
}

#[test]
fn concurrent_memory_reservations_are_bounded_and_reclaimed() {
    let _guard = limit_test_guard();
    let old = crate::capture_aggregate_memory_cap();
    crate::set_capture_aggregate_memory_cap(8192);
    let barrier = Arc::new(Barrier::new(3));
    let (tx, rx) = std::sync::mpsc::channel();
    let threads = (0..2)
        .map(|_| {
            let barrier = barrier.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let mut lease = MemoryLease::empty();
                let granted = lease.reserve_up_to(8192);
                tx.send(granted).ok();
                barrier.wait();
                drop(lease);
            })
        })
        .collect::<Vec<_>>();
    drop(tx);
    let granted = rx.iter().take(2).collect::<Vec<_>>();
    assert_eq!(granted.iter().sum::<usize>(), 8192);
    assert!(granted.contains(&0));
    assert_eq!(crate::capture_budget::usage().0, 8192);
    barrier.wait();
    for thread in threads {
        thread.join().expect("reservation worker");
    }
    assert_eq!(crate::capture_budget::usage().0, 0);
    crate::set_capture_aggregate_memory_cap(old);
}

struct HoldAfterChunk {
    chunk: Option<Vec<u8>>,
    barrier: Arc<Barrier>,
}

impl Read for HoldAfterChunk {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(chunk) = self.chunk.take() {
            let count = chunk.len().min(buf.len());
            buf[..count].copy_from_slice(&chunk[..count]);
            return Ok(count);
        }
        self.barrier.wait();
        Ok(0)
    }
}

#[test]
fn concurrent_tiny_drains_charge_actual_bytes_not_their_maxima() {
    let _guard = limit_test_guard();
    let old = crate::capture_aggregate_memory_cap();
    crate::set_capture_aggregate_memory_cap(8);
    let barrier = Arc::new(Barrier::new(3));
    let workers = (0..2)
        .map(|_| {
            let barrier = barrier.clone();
            thread::spawn(move || {
                drain_capped(
                    Box::new(HoldAfterChunk {
                        chunk: Some(vec![b'x'; 4]),
                        barrier,
                    }),
                    8,
                )
            })
        })
        .collect::<Vec<_>>();
    while crate::capture_budget::usage().0 != 8 {
        thread::yield_now();
    }
    barrier.wait();
    for worker in workers {
        let (bytes, truncated) = worker.join().expect("tiny drain worker");
        assert_eq!(bytes, vec![b'x'; 4]);
        assert!(!truncated);
    }
    assert_eq!(crate::capture_budget::usage().0, 0);
    crate::set_capture_aggregate_memory_cap(old);
}

#[test]
fn spill_reservation_and_private_file_follow_raii_ownership() {
    let _guard = limit_test_guard();
    let old = crate::capture_aggregate_spill_cap();
    crate::set_capture_aggregate_spill_cap(8192);
    let dir = tempfile::tempdir().expect("spill dir");
    let spill_config = Some(SpillConfig {
        dir: dir.path().to_path_buf(),
    });
    let payload = vec![b'x'; 32 * 1024];
    let mut first = drain_stdout(
        Box::new(io::Cursor::new(payload.clone())),
        1024,
        spill_config.clone(),
        8192,
    );
    assert!(first.truncated);
    let spill = first.spill.take().expect("first spill");
    assert_eq!(crate::capture_budget::usage().1, 8192);
    assert_eq!(crate::capture_budget::usage().2, 1);
    assert_eq!(
        std::fs::metadata(&spill.path)
            .expect("spill metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let path = spill.path.clone();
    let clone = spill.clone();
    drop(spill);
    assert!(path.exists(), "a live clone retains spill ownership");

    let second = drain_stdout(
        Box::new(io::Cursor::new(payload.clone())),
        1024,
        spill_config.clone(),
        8192,
    );
    assert!(second.spill.is_none());
    assert!(second.truncated);
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("spill entries")
            .count(),
        1
    );

    drop(clone);
    assert!(!path.exists());
    assert_eq!(crate::capture_budget::usage().1, 0);
    assert_eq!(crate::capture_budget::usage().2, 0);
    let third = drain_stdout(Box::new(io::Cursor::new(payload)), 1024, spill_config, 8192);
    assert!(third.spill.is_some(), "released budget is reusable");
    drop(third);
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("spill entries")
            .count(),
        0
    );
    assert_eq!(crate::capture_budget::usage(), (0, 0, 0));
    crate::set_capture_aggregate_spill_cap(old);
}

#[test]
fn active_spill_file_count_is_bounded_and_reclaimed() {
    let _guard = limit_test_guard();
    let dir = tempfile::tempdir().expect("spill dir");
    let mut sinks = (0..crate::MAX_ACTIVE_CAPTURE_SPILL_FILES)
        .map(|_| SpillSink::create(dir.path()).expect("file slot within bound"))
        .collect::<Vec<_>>();
    assert_eq!(
        crate::capture_budget::usage().2,
        crate::MAX_ACTIVE_CAPTURE_SPILL_FILES
    );
    assert!(
        SpillSink::create(dir.path()).is_err(),
        "one process cannot retain an unbounded number of spill files"
    );
    sinks.clear();
    assert_eq!(crate::capture_budget::usage(), (0, 0, 0));
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("spill entries")
            .count(),
        0,
        "dropping unfinished sinks removes every private temp file"
    );
}

#[test]
fn a_nontruncated_spill_is_not_complete_in_the_resident_stdout() {
    let _guard = limit_test_guard();
    let dir = tempfile::tempdir().expect("spill dir");
    let out = drain_stdout(
        Box::new(io::Cursor::new(vec![b'x'; 8192])),
        1024,
        Some(SpillConfig {
            dir: dir.path().to_path_buf(),
        }),
        16 * 1024,
    );
    assert!(!out.truncated);
    assert!(out.spill.is_some());
    let result = ExecResult {
        status: Some(0),
        signal: None,
        stdout: out.buf,
        stderr: Vec::new(),
        truncated: out.truncated,
        stdout_spill: out.spill,
        dur: Duration::ZERO,
        pid: 1,
        pgid: 1,
        stopped: false,
        enforcement: None,
    };
    assert!(
        !result.stdout_is_complete(),
        "structured consumers see that stdout is only a preview"
    );
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }
}

#[test]
fn spill_write_failure_deletes_temp_and_never_claims_completeness() {
    let _guard = limit_test_guard();
    let dir = tempfile::tempdir().expect("spill dir");
    let payload = vec![b'x'; 16 * 1024];
    let out = drain_stdout_with(
        Box::new(io::Cursor::new(payload)),
        1024,
        Some(SpillConfig {
            dir: dir.path().to_path_buf(),
        }),
        8192,
        |dir| {
            let mut sink = SpillSink::create(dir)?;
            sink.writer = BufWriter::new(Box::new(FailingWriter));
            Ok(sink)
        },
    );
    assert!(out.truncated);
    assert!(out.spill.is_none());
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("spill entries")
            .count(),
        0
    );
    assert_eq!(crate::capture_budget::usage(), (0, 0, 0));
}

struct ReadThenError {
    bytes: Option<Vec<u8>>,
}

impl Read for ReadThenError {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(bytes) = self.bytes.take() {
            let count = bytes.len().min(buf.len());
            buf[..count].copy_from_slice(&bytes[..count]);
            return Ok(count);
        }
        Err(io::Error::other("injected pipe read failure"))
    }
}

#[test]
fn pipe_read_failure_is_truncated_and_cleans_in_progress_spill() {
    let _guard = limit_test_guard();
    let dir = tempfile::tempdir().expect("spill dir");
    let out = drain_stdout(
        Box::new(ReadThenError {
            bytes: Some(vec![b'x'; 16 * 1024]),
        }),
        1024,
        Some(SpillConfig {
            dir: dir.path().to_path_buf(),
        }),
        8192,
    );
    assert!(out.truncated);
    assert!(out.spill.is_none());
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("spill entries")
            .count(),
        0
    );
    assert_eq!(crate::capture_budget::usage(), (0, 0, 0));
}
