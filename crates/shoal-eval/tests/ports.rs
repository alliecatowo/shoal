//! Port seam proof (docs/ROADMAP.md R4): swapping a fake [`Fs`] adapter proves
//! the evaluator routes every filesystem effect through the port, never through
//! `std::fs` directly. The fake keeps files in memory, so a redirect and a `cat`
//! observe the fake — and the real filesystem is never touched.

use shoal_eval::Evaluator;
use shoal_value::{Fs, ReadSeek, Value};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// An in-memory [`Fs`] used to prove the port seam. Only the operations the
/// tests exercise (read / write / append) are backed by the map; the rest are
/// deliberately unreachable so any unexpected filesystem escape shows up loudly.
#[derive(Default)]
struct FakeFs {
    files: Mutex<HashMap<PathBuf, Vec<u8>>>,
}

impl FakeFs {
    fn get(&self, path: &Path) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(path).cloned()
    }
    fn seed(&self, path: impl Into<PathBuf>, data: impl Into<Vec<u8>>) {
        self.files.lock().unwrap().insert(path.into(), data.into());
    }
}

impl Fs for FakeFs {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such fake file"))
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek + Send>> {
        // An in-memory, seekable reader over the fake's bytes — the `tail`
        // source drives this exactly as it would a real `File`.
        let bytes = self.read(path)?;
        Ok(Box::new(io::Cursor::new(bytes)))
    }
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), data.to_vec());
        Ok(())
    }
    fn append(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.files
            .lock()
            .unwrap()
            .entry(path.to_path_buf())
            .or_default()
            .extend_from_slice(data);
        Ok(())
    }
    fn touch(&self, path: &Path) -> io::Result<()> {
        self.files
            .lock()
            .unwrap()
            .entry(path.to_path_buf())
            .or_default();
        Ok(())
    }
    fn metadata(&self, _path: &Path) -> io::Result<std::fs::Metadata> {
        unreachable!("metadata not exercised by the port-seam test")
    }
    fn symlink_metadata(&self, _path: &Path) -> io::Result<std::fs::Metadata> {
        unreachable!("symlink_metadata not exercised by the port-seam test")
    }
    fn is_file(&self, path: &Path) -> bool {
        // Overridden (the default routes through `metadata`, which this fake
        // can't fabricate) so the `config` reader's existence probe is
        // interposable in memory.
        self.files.lock().unwrap().contains_key(path)
    }
    fn read_dir(&self, _path: &Path) -> io::Result<Vec<PathBuf>> {
        unreachable!("read_dir not exercised by the port-seam test")
    }
    fn create_dir(&self, _path: &Path) -> io::Result<()> {
        unreachable!("create_dir not exercised by the port-seam test")
    }
    fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        unreachable!("create_dir_all not exercised by the port-seam test")
    }
    fn remove_file(&self, _path: &Path) -> io::Result<()> {
        unreachable!("remove_file not exercised by the port-seam test")
    }
    fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
        unreachable!("remove_dir_all not exercised by the port-seam test")
    }
    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        unreachable!("rename not exercised by the port-seam test")
    }
    fn copy(&self, _from: &Path, _to: &Path) -> io::Result<u64> {
        unreachable!("copy not exercised by the port-seam test")
    }
    fn hard_link(&self, _src: &Path, _dst: &Path) -> io::Result<()> {
        unreachable!("hard_link not exercised by the port-seam test")
    }
    fn symlink(&self, _target: &Path, _link: &Path) -> io::Result<()> {
        unreachable!("symlink not exercised by the port-seam test")
    }
}

fn eval_with_fs(src: &str, cwd: &Path, fs: Arc<FakeFs>) -> Value {
    let program = shoal_syntax::parse(src).expect("parse");
    let mut ev = Evaluator::new(cwd.to_path_buf());
    ev.set_fs(fs);
    ev.eval_program(&program).expect("eval")
}

/// A `> file` redirect writes through the [`Fs`] port: the fake captures the
/// bytes and nothing lands on the real disk (the cwd is a bogus path that does
/// not exist, so a real `std::fs::write` would error).
#[test]
fn redirect_out_routes_through_the_fs_port() {
    let cwd = Path::new("/nonexistent-shoal-ports-test");
    let fs = Arc::new(FakeFs::default());
    eval_with_fs("echo hello > out.txt", cwd, fs.clone());

    let written = fs
        .get(&cwd.join("out.txt"))
        .expect("fake fs should have captured the redirect");
    assert_eq!(written, b"hello\n");
    // The real filesystem was never touched.
    assert!(!cwd.join("out.txt").exists());
}

/// `>>` append also routes through the port and accumulates in the fake.
#[test]
fn redirect_append_routes_through_the_fs_port() {
    let cwd = Path::new("/nonexistent-shoal-ports-test");
    let fs = Arc::new(FakeFs::default());
    eval_with_fs("echo a > log\necho b >> log", cwd, fs.clone());

    let written = fs.get(&cwd.join("log")).expect("fake fs captured appends");
    assert_eq!(written, b"a\nb\n");
}

/// The in-language `config` reader routes through the port: it finds and parses
/// a seeded, in-memory `shoal.toml` via `Fs::is_file` + `Fs::read_to_string`,
/// with nothing on the real disk (the cwd does not exist). This pins the fix for
/// the `config` reader's former direct `std::fs::read_to_string` + `is_file`
/// leak (CONTRACTS §8).
#[test]
fn config_reader_routes_through_the_fs_port() {
    let cwd = Path::new("/nonexistent-shoal-ports-test");
    let fs = Arc::new(FakeFs::default());
    fs.seed(cwd.join("shoal.toml"), b"greeting = \"hi\"\n".to_vec());

    let out = eval_with_fs("config.all()", cwd, fs);
    let rec = match out {
        Value::Record(r) => r,
        other => panic!("expected a record, got {}", other.type_name()),
    };
    assert_eq!(rec.get("greeting"), Some(&Value::Str("hi".into())));
}

/// `cat` reads through the port: seed the fake, and `cat` returns its bytes
/// without any real file existing.
#[test]
fn cat_reads_through_the_fs_port() {
    let cwd = Path::new("/nonexistent-shoal-ports-test");
    let fs = Arc::new(FakeFs::default());
    fs.seed(cwd.join("greeting"), b"seeded-bytes".to_vec());

    let out = eval_with_fs("cat greeting", cwd, fs);
    // `cat` yields an outcome whose bytes are the file contents.
    let bytes = match out {
        Value::Outcome(o) => (*o.stdout).clone(),
        other => panic!("expected an outcome, got {}", other.type_name()),
    };
    assert_eq!(bytes, b"seeded-bytes");
}
