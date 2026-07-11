//! Wave R0 end-to-end tests for the two interactive dealbreakers
//! (docs/ROADMAP.md):
//!   1. statement-position builtins render their output;
//!   2. `exit`/`quit` ends the session with a code.
//!
//! The `-c` tests are deterministic process spawns. The `repl_*` test drives
//! the real interactive REPL over a PTY, answering reedline's cursor-position
//! (DSR) query so the line editor runs headless, and asserts the fixes hold
//! end-to-end: an `echo` result renders exactly once and `exit <code>` sets the
//! process exit status.

use std::io::{Read, Write};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const BIN: &str = env!("CARGO_BIN_EXE_shoal");

fn run_c(src: &str) -> (i32, String) {
    let out = Command::new(BIN)
        .args(["-c", src])
        .env("NO_COLOR", "1")
        .output()
        .expect("spawn shoal -c");
    let code = out.status.code().expect("process exited normally");
    (code, String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn dash_c_exit_with_code() {
    assert_eq!(run_c("exit 3").0, 3);
}

#[test]
fn dash_c_exit_defaults_zero() {
    assert_eq!(run_c("exit").0, 0);
}

#[test]
fn dash_c_quit_alias() {
    assert_eq!(run_c("quit 2").0, 2);
}

#[test]
fn dash_c_exit_halts_remaining_statements() {
    let (code, stdout) = run_c("echo before; exit 7; echo after");
    assert_eq!(code, 7);
    assert!(stdout.contains("before"), "stdout was {stdout:?}");
    assert!(
        !stdout.contains("after"),
        "statement after exit must not run; stdout was {stdout:?}"
    );
}

#[test]
fn dash_c_builtin_renders() {
    // Regression for bug 1: a statement-position builtin renders its output.
    let (_, stdout) = run_c("echo hello");
    assert!(stdout.contains("hello"), "stdout was {stdout:?}");
}

/// Drive the interactive REPL over a PTY: `echo` renders exactly once, an
/// external command prints exactly once, and `exit <code>` sets the status.
#[test]
fn repl_echo_renders_once_and_exit_sets_code() {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let home = tempfile::tempdir().unwrap();
    let mut cmd = CommandBuilder::new(BIN);
    cmd.cwd(home.path());
    cmd.env("NO_COLOR", "1");
    cmd.env("TERM", "xterm");
    // Keep the REPL off the real user's config/history/state.
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path());
    cmd.env("XDG_STATE_HOME", home.path());
    cmd.env_remove("SHOAL_CONFIG");

    let mut child = pair.slave.spawn_command(cmd).expect("spawn repl");
    drop(pair.slave);

    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let reader_buf = Arc::clone(&buf);
    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let reader_thread = std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => reader_buf.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    });

    let mut writer = pair.master.take_writer().expect("take writer");
    let mut dsr_answered = 0usize;

    // Answer every not-yet-answered DSR query. reedline's Device Status Report
    // query is the 4-byte `ESC [ 6 n`; it blocks on the cursor-position reply
    // before drawing each prompt, so answering it is what makes the REPL run.
    let answer_dsr = |writer: &mut Box<dyn Write + Send>, answered: &mut usize| {
        let seen = buf
            .lock()
            .unwrap()
            .windows(4)
            .filter(|w| *w == b"\x1b[6n")
            .count();
        while *answered < seen {
            let _ = writer.write_all(b"\x1b[1;1R");
            let _ = writer.flush();
            *answered += 1;
        }
    };

    // Pump until `marker` bytes appear in the accumulated output (or time out),
    // answering DSR queries along the way. An empty marker waits for the first
    // prompt (first DSR answered).
    let mut pump_until = |writer: &mut Box<dyn Write + Send>, marker: &[u8]| -> bool {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            answer_dsr(writer, &mut dsr_answered);
            if marker.is_empty() {
                if dsr_answered >= 1 {
                    return true;
                }
            } else if buf
                .lock()
                .unwrap()
                .windows(marker.len())
                .any(|w| w == marker)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    // Wait for the first prompt to be drawn (first DSR answered).
    assert!(pump_until(&mut writer, b""), "REPL never drew a prompt");

    // A builtin whose rendered result (42001) differs from the typed input
    // (6*7000+1), so a match proves the *result* rendered, not the echoed line.
    writer.write_all(b"echo (6*7000+1)\r").unwrap();
    writer.flush().unwrap();
    assert!(
        pump_until(&mut writer, b"42001"),
        "echo result did not render"
    );

    // An external command whose output (MK55446MK) differs from the typed line.
    writer
        .write_all(b"/usr/bin/printf 'MK%sMK\\n' 55446\r")
        .unwrap();
    writer.flush().unwrap();
    assert!(
        pump_until(&mut writer, b"MK55446MK"),
        "external output did not appear"
    );

    writer.write_all(b"exit 5\r").unwrap();
    writer.flush().unwrap();

    // Keep answering DSR (reedline queries once more while drawing the prompt
    // for the `exit` line) until the child actually exits.
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        answer_dsr(&mut writer, &mut dsr_answered);
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "REPL did not exit after `exit 5`"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(writer);
    let _ = reader_thread.join();

    let text = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    let count_echo = text.matches("42001").count();
    let count_ext = text.matches("MK55446MK").count();
    assert_eq!(
        count_echo, 1,
        "echo result should render exactly once; transcript:\n{text}"
    );
    assert_eq!(
        count_ext, 1,
        "external should print exactly once; transcript:\n{text}"
    );
    assert_eq!(
        status.exit_code(),
        5,
        "exit 5 should set the process status; transcript:\n{text}"
    );
}

/// docs/ROADMAP.md R3: `undo out[n]` resolves via the host's `out[n] ->
/// journal entry id` map. Drives the real REPL over a PTY: `rm victim`
/// journals a reversible trash-move as `out[3]` (after three prior
/// statements fill `out[0..2]`), then `undo out[3]` must restore the file —
/// proving the rewrite from `out[N]` to a real entry id actually reached the
/// eval's `undo <id>` path (a bare, un-rewritten `out[3]` value is not a
/// valid undo target and would raise instead).
#[test]
fn repl_undo_out_n_resolves_via_journal() {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let home = tempfile::tempdir().unwrap();
    let victim = home.path().join("victim");
    std::fs::write(&victim, b"payload").unwrap();

    let mut cmd = CommandBuilder::new(BIN);
    cmd.cwd(home.path());
    cmd.env("NO_COLOR", "1");
    cmd.env("TERM", "xterm");
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path());
    cmd.env("XDG_STATE_HOME", home.path());
    cmd.env_remove("SHOAL_CONFIG");

    let mut child = pair.slave.spawn_command(cmd).expect("spawn repl");
    drop(pair.slave);

    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let reader_buf = Arc::clone(&buf);
    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let reader_thread = std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => reader_buf.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    });

    let mut writer = pair.master.take_writer().expect("take writer");
    let mut dsr_answered = 0usize;
    let answer_dsr = |writer: &mut Box<dyn Write + Send>, answered: &mut usize| {
        let seen = buf
            .lock()
            .unwrap()
            .windows(4)
            .filter(|w| *w == b"\x1b[6n")
            .count();
        while *answered < seen {
            let _ = writer.write_all(b"\x1b[1;1R");
            let _ = writer.flush();
            *answered += 1;
        }
    };
    let mut pump_until = |writer: &mut Box<dyn Write + Send>, marker: &[u8]| -> bool {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            answer_dsr(writer, &mut dsr_answered);
            if marker.is_empty() {
                if dsr_answered >= 1 {
                    return true;
                }
            } else if buf
                .lock()
                .unwrap()
                .windows(marker.len())
                .any(|w| w == marker)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    assert!(pump_until(&mut writer, b""), "REPL never drew a prompt");

    // Three statements to fill out[0..2], each awaited via a distinct marker
    // before moving on (the REPL processes one line at a time).
    for marker in ["811001", "811002", "811003"] {
        writer
            .write_all(format!("echo {marker}\r").as_bytes())
            .unwrap();
        writer.flush().unwrap();
        assert!(
            pump_until(&mut writer, marker.as_bytes()),
            "dummy echo {marker} did not render"
        );
    }

    // `rm victim` becomes out[3]; wait for it via a trailing sentinel rather
    // than rm's own (empty) output.
    writer.write_all(b"rm victim\r").unwrap();
    writer.flush().unwrap();
    writer.write_all(b"echo 811004\r").unwrap();
    writer.flush().unwrap();
    assert!(
        pump_until(&mut writer, b"811004"),
        "rm victim did not complete"
    );
    assert!(!victim.exists(), "rm should have trashed the file");

    // The fix under test: `out[3]` resolves to `rm`'s journal entry id.
    writer.write_all(b"undo out[3]\r").unwrap();
    writer.flush().unwrap();
    writer.write_all(b"echo 811005\r").unwrap();
    writer.flush().unwrap();
    assert!(
        pump_until(&mut writer, b"811005"),
        "undo out[3] did not complete"
    );

    writer.write_all(b"exit 0\r").unwrap();
    writer.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        answer_dsr(&mut writer, &mut dsr_answered);
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "REPL did not exit after `exit 0`"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(writer);
    let _ = reader_thread.join();

    let text = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    assert_eq!(status.exit_code(), 0, "transcript:\n{text}");
    assert!(
        victim.exists(),
        "undo out[3] should have restored the file; transcript:\n{text}"
    );
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        b"payload",
        "restored file should have its original bytes; transcript:\n{text}"
    );
    assert!(
        !text.contains("undo target must be a journal entry id"),
        "out[3] should have been rewritten to a real entry id, not fallen through \
         to the unresolved-value error; transcript:\n{text}"
    );
}
