use super::*;

/// Delegates reads to [`StdFs`], records and refuses every write-shaped
/// call. The recording side makes the integration tests prove that the
/// evaluator actually reached its injected port, rather than merely
/// observing an unrelated error and an absent file.
struct DenyWrites {
    calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl DenyWrites {
    fn denied<T>(&self, call: &'static str) -> std::io::Result<T> {
        self.calls.lock().unwrap().push(call);
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied by test port",
        ))
    }
}

impl Fs for DenyWrites {
    fn read(&self, path: &std::path::Path) -> std::io::Result<Vec<u8>> {
        StdFs.read(path)
    }
    fn read_to_string(&self, path: &std::path::Path) -> std::io::Result<String> {
        StdFs.read_to_string(path)
    }
    fn open_read(
        &self,
        path: &std::path::Path,
    ) -> std::io::Result<Box<dyn shoal_value::ReadSeek + Send>> {
        StdFs.open_read(path)
    }
    fn write(&self, _: &std::path::Path, _: &[u8]) -> std::io::Result<()> {
        self.denied("write")
    }
    fn append(&self, _: &std::path::Path, _: &[u8]) -> std::io::Result<()> {
        self.denied("append")
    }
    fn open_append(&self, _: &std::path::Path) -> std::io::Result<Box<dyn std::io::Write + Send>> {
        self.denied("open_append")
    }
    fn touch(&self, _: &std::path::Path) -> std::io::Result<()> {
        self.denied("touch")
    }
    fn metadata(&self, path: &std::path::Path) -> std::io::Result<std::fs::Metadata> {
        StdFs.metadata(path)
    }
    fn symlink_metadata(&self, path: &std::path::Path) -> std::io::Result<std::fs::Metadata> {
        StdFs.symlink_metadata(path)
    }
    fn read_dir(&self, path: &std::path::Path) -> std::io::Result<Vec<PathBuf>> {
        StdFs.read_dir(path)
    }
    fn create_dir(&self, _: &std::path::Path) -> std::io::Result<()> {
        self.denied("create_dir")
    }
    fn create_dir_all(&self, _: &std::path::Path) -> std::io::Result<()> {
        self.denied("create_dir_all")
    }
    fn remove_file(&self, _: &std::path::Path) -> std::io::Result<()> {
        self.denied("remove_file")
    }
    fn remove_dir_all(&self, _: &std::path::Path) -> std::io::Result<()> {
        self.denied("remove_dir_all")
    }
    fn rename(&self, _: &std::path::Path, _: &std::path::Path) -> std::io::Result<()> {
        self.denied("rename")
    }
    fn copy(&self, _: &std::path::Path, _: &std::path::Path) -> std::io::Result<u64> {
        self.denied("copy")
    }
    fn hard_link(&self, _: &std::path::Path, _: &std::path::Path) -> std::io::Result<()> {
        self.denied("hard_link")
    }
    fn symlink(&self, _: &std::path::Path, _: &std::path::Path) -> std::io::Result<()> {
        self.denied("symlink")
    }
}

fn evaluator_with_denied_writes(
    cwd: PathBuf,
) -> (Evaluator, Arc<std::sync::Mutex<Vec<&'static str>>>) {
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut ev = Evaluator::new(cwd);
    ev.set_fs(Arc::new(DenyWrites {
        calls: calls.clone(),
    }));
    (ev, calls)
}

/// The Fs-port boundary is enforceable *through the evaluator*: scalar
/// value-method writes resolve to the evaluator's injected port.
#[test]
fn value_method_saves_go_through_the_injected_fs_port() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ev, calls) = evaluator_with_denied_writes(dir.path().to_path_buf());
    let program = shoal_syntax::parse(r#""x".save("p")"#).unwrap();
    let err = ev.eval_program(&program).unwrap_err();
    assert_eq!(err.code, "custom");
    assert!(
        err.msg.contains("denied by test port"),
        "the injected port's exact refusal must surface, got {err:?}"
    );
    assert_eq!(&*calls.lock().unwrap(), &["write"]);
    assert!(
        !dir.path().join("p").exists(),
        "the denied write must never reach the real filesystem"
    );
}

/// Stream sinks use the same evaluator injection seam, but exercise the
/// long-lived `open_append` capability rather than whole-buffer `write`.
#[test]
fn stream_saves_go_through_the_injected_fs_port() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ev, calls) = evaluator_with_denied_writes(dir.path().to_path_buf());
    let program = shoal_syntax::parse(r#"[1, 2].stream().save("events")"#).unwrap();
    let err = ev.eval_program(&program).unwrap_err();
    assert_eq!(err.code, "custom");
    assert!(err.msg.contains("denied by test port"), "got {err:?}");
    assert_eq!(&*calls.lock().unwrap(), &["open_append"]);
    assert!(!dir.path().join("events").exists());
}
