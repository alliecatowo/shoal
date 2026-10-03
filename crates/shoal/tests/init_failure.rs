//! Regression (audit M9): a missing init file used to abort startup.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const BIN: &str = env!("CARGO_BIN_EXE_shoal");

#[test]
fn missing_init_file_warns_and_the_shell_still_starts() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("shoal");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("shoal.toml"),
        "[init]\nfiles = [\"/nonexistent/init.shl\"]\n",
    )
    .unwrap();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(BIN);
    cmd.arg("--standalone");
    cmd.cwd(home.path());
    for (k, v) in [("NO_COLOR", "1"), ("TERM", "xterm")] {
        cmd.env(k, v);
    }
    for k in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        cmd.env(k, home.path());
    }
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = Arc::clone(&buf);
    let mut reader = pair.master.try_clone_reader().unwrap();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });
    let mut writer = pair.master.take_writer().unwrap();
    let (mut answered, mut exited) = (0usize, None);
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        let seen = buf
            .lock()
            .unwrap()
            .windows(4)
            .filter(|w| *w == b"\x1b[6n")
            .count();
        while answered < seen {
            writer.write_all(b"\x1b[1;1R").unwrap();
            writer.flush().unwrap();
            answered += 1;
            if answered == 1 {
                writer.write_all(b"exit 5\r").unwrap();
                writer.flush().unwrap();
            }
        }
        if let Some(status) = child.try_wait().unwrap() {
            exited = Some(status.exit_code());
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    assert!(output.contains("init failed"), "no warning: {output}");
    assert_eq!(
        exited,
        Some(5),
        "shell did not start and exit cleanly: {output}"
    );
}
