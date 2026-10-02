use super::*;

/// A deliberately hostile host: every effect-shaped port records the attempt
/// and refuses it. Canonical builtin help must succeed for all command heads
/// while this counter remains exactly zero.
#[derive(Default)]
struct HelpDenyFs {
    calls: std::sync::atomic::AtomicUsize,
}

impl HelpDenyFs {
    fn deny<T>(&self) -> std::io::Result<T> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "help touched the filesystem port",
        ))
    }
}

impl Fs for HelpDenyFs {
    fn read(&self, _: &Path) -> std::io::Result<Vec<u8>> {
        self.deny()
    }
    fn read_to_string(&self, _: &Path) -> std::io::Result<String> {
        self.deny()
    }
    fn open_read(&self, _: &Path) -> std::io::Result<Box<dyn shoal_value::ReadSeek + Send>> {
        self.deny()
    }
    fn write(&self, _: &Path, _: &[u8]) -> std::io::Result<()> {
        self.deny()
    }
    fn append(&self, _: &Path, _: &[u8]) -> std::io::Result<()> {
        self.deny()
    }
    fn open_append(&self, _: &Path) -> std::io::Result<Box<dyn std::io::Write + Send>> {
        self.deny()
    }
    fn open_write(&self, _: &Path) -> std::io::Result<Box<dyn std::io::Write + Send>> {
        self.deny()
    }
    fn touch(&self, _: &Path) -> std::io::Result<()> {
        self.deny()
    }
    fn metadata(&self, _: &Path) -> std::io::Result<std::fs::Metadata> {
        self.deny()
    }
    fn symlink_metadata(&self, _: &Path) -> std::io::Result<std::fs::Metadata> {
        self.deny()
    }
    fn exists(&self, _: &Path) -> bool {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        false
    }
    fn is_file(&self, _: &Path) -> bool {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        false
    }
    fn is_dir(&self, _: &Path) -> bool {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        false
    }
    fn canonicalize(&self, _: &Path) -> std::io::Result<PathBuf> {
        self.deny()
    }
    fn atomic_replace(&self, _: &Path, _: &[u8]) -> std::io::Result<()> {
        self.deny()
    }
    fn read_dir(&self, _: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.deny()
    }
    fn read_dir_bounded(&self, _: &Path, _: usize) -> std::io::Result<Vec<PathBuf>> {
        self.deny()
    }
    fn read_dir_limited(&self, _: &Path, _: usize, _: usize) -> std::io::Result<Vec<PathBuf>> {
        self.deny()
    }
    fn read_dir_prefix(&self, _: &Path, _: usize) -> std::io::Result<Vec<PathBuf>> {
        self.deny()
    }
    fn create_dir(&self, _: &Path) -> std::io::Result<()> {
        self.deny()
    }
    fn create_dir_all(&self, _: &Path) -> std::io::Result<()> {
        self.deny()
    }
    fn create_private_dir_all(&self, _: &Path) -> std::io::Result<()> {
        self.deny()
    }
    fn remove_file(&self, _: &Path) -> std::io::Result<()> {
        self.deny()
    }
    fn remove_dir_all(&self, _: &Path) -> std::io::Result<()> {
        self.deny()
    }
    fn rename(&self, _: &Path, _: &Path) -> std::io::Result<()> {
        self.deny()
    }
    fn copy(&self, _: &Path, _: &Path) -> std::io::Result<u64> {
        self.deny()
    }
    fn hard_link(&self, _: &Path, _: &Path) -> std::io::Result<()> {
        self.deny()
    }
    fn symlink(&self, _: &Path, _: &Path) -> std::io::Result<()> {
        self.deny()
    }
}

#[derive(Default)]
struct HelpDenyExec(std::sync::atomic::AtomicUsize);

impl crate::ports::Exec for HelpDenyExec {
    fn run(
        &self,
        _: shoal_exec::ExecSpec,
        _: &shoal_exec::CancelToken,
    ) -> std::io::Result<shoal_exec::ExecResult> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "help touched the execution port",
        ))
    }

    fn run_bounded(
        &self,
        _: shoal_exec::ExecSpec,
        _: std::time::Duration,
        _: usize,
        _: &shoal_exec::CancelToken,
    ) -> std::io::Result<shoal_exec::BoundedCommandOutput> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "help touched bounded execution",
        ))
    }
}

#[derive(Default)]
struct HelpDenyClock(std::sync::atomic::AtomicUsize);

impl shoal_value::Clock for HelpDenyClock {
    fn now_ns(&self) -> i64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        0
    }
}

#[derive(Default)]
struct HelpDenyOpener(std::sync::atomic::AtomicUsize);

impl shoal_value::Opener for HelpDenyOpener {
    fn open(&self, _: &Path) -> std::io::Result<()> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(std::io::Error::other("help touched the opener port"))
    }
}

#[test]
fn every_builtin_help_form_is_useful_planned_pure_and_zero_effect() {
    use std::sync::atomic::Ordering;

    for spec in shoal_syntax::commands::builtin_specs() {
        for help_flag in ["--help", "-h"] {
            let dir = tempfile::tempdir().unwrap();
            let redirect = dir.path().join("must-not-exist");
            let source = format!(
                "{} {help_flag} hostile-*.input > {} &",
                spec.name,
                redirect.display()
            );
            let program = shoal_syntax::parse(&source)
                .unwrap_or_else(|error| panic!("{} help did not parse: {error}", spec.name));
            let fs = Arc::new(HelpDenyFs::default());
            let exec = Arc::new(HelpDenyExec::default());
            let clock = Arc::new(HelpDenyClock::default());
            let opener = Arc::new(HelpDenyOpener::default());
            let mut evaluator = Evaluator::new(dir.path().to_path_buf());
            evaluator.set_fs(fs.clone());
            evaluator.set_exec(exec.clone());
            evaluator.set_clock(clock.clone());
            evaluator.set_opener(opener.clone());
            let captured: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
            let sink = captured.clone();
            evaluator.set_statement_sink(Box::new(move |value| {
                sink.lock().unwrap().push(value.clone());
            }));

            let value = evaluator
                .eval_program(&program)
                .unwrap_or_else(|error| panic!("{} help executed: {error:?}", spec.name));
            assert_eq!(value, Value::Null, "{} {help_flag}", spec.name);
            let output = captured.lock().unwrap();
            assert_eq!(output.len(), 1, "{} {help_flag}: {output:?}", spec.name);
            let Value::Str(help) = &output[0] else {
                panic!("{} help was not text: {output:?}", spec.name);
            };
            assert!(help.starts_with(spec.name), "{}: {help}", spec.name);
            for section in ["Usage:", "Options:", "Returns:", "Errors:", "Examples:"] {
                assert!(help.contains(section), "{} help lacks {section}", spec.name);
            }
            drop(output);

            assert_eq!(
                fs.calls.load(Ordering::SeqCst),
                0,
                "{} filesystem",
                spec.name
            );
            assert_eq!(exec.0.load(Ordering::SeqCst), 0, "{} execution", spec.name);
            assert_eq!(clock.0.load(Ordering::SeqCst), 0, "{} clock", spec.name);
            assert_eq!(opener.0.load(Ordering::SeqCst), 0, "{} opener", spec.name);
            assert_eq!(evaluator.take_exit(), None, "{} exit mutation", spec.name);
            assert_eq!(
                evaluator.jobs_snapshot().total,
                0,
                "{} background task",
                spec.name
            );
            assert!(!redirect.exists(), "{} applied its redirect", spec.name);

            let mut planner = Evaluator::new(dir.path().to_path_buf());
            planner.set_fs(fs.clone());
            planner.set_exec(exec.clone());
            planner.set_clock(clock.clone());
            planner.set_opener(opener.clone());
            let plan = planner
                .plan_program(&program)
                .unwrap_or_else(|error| panic!("{} help did not plan: {error:?}", spec.name));
            assert!(
                plan.effects.is_empty(),
                "{} help effects: {:?}",
                spec.name,
                plan.effects
            );
            assert_eq!(
                fs.calls.load(Ordering::SeqCst),
                0,
                "{} planned filesystem",
                spec.name
            );
            assert_eq!(
                exec.0.load(Ordering::SeqCst),
                0,
                "{} planned execution",
                spec.name
            );
            assert_eq!(
                clock.0.load(Ordering::SeqCst),
                0,
                "{} planned clock",
                spec.name
            );
            assert_eq!(
                opener.0.load(Ordering::SeqCst),
                0,
                "{} planned opener",
                spec.name
            );
        }
    }
}

#[test]
fn callable_shadow_wins_over_builtin_help_and_dashdash_ends_help_options() {
    let (result, captured) = run_capturing("fn ls(item: str) { item }\nls --help");
    assert_eq!(result.unwrap(), Value::Null);
    assert!(
        matches!(captured.as_slice(), [Value::Str(help)] if help.contains("item") && !help.contains("typed filesystem metadata")),
        "callable help must win: {captured:?}"
    );

    let call = CmdCall {
        head: "rm".into(),
        args: vec![
            CmdArg::DashDash {
                span: Span::default(),
            },
            CmdArg::FlagLong {
                name: "help".into(),
                value: None,
                span: Span::default(),
            },
        ],
        redirects: vec![],
        env_prefix: vec![],
        background: false,
        trailing: None,
        forced: false,
        span: Span::default(),
    };
    assert!(!crate::command::call_requests_help(&call));
}
