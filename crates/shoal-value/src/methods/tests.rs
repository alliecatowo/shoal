use super::*;
use crate::{RegexVal, StreamVal};
struct C {
    cwd: PathBuf,
}
impl CallCtx for C {
    fn call_closure(&mut self, f: &Value, args: Vec<Value>) -> VResult<Value> {
        match f {
            Value::Str(s) if s == "double" => match args[0] {
                Value::Int(i) => Ok(Value::Int(i * 2)),
                _ => unreachable!(),
            },
            Value::Str(s) if s == "even" => match args[0] {
                Value::Int(i) => Ok(Value::Bool(i % 2 == 0)),
                _ => unreachable!(),
            },
            _ => Err(ErrorVal::new("custom", "bad test callback")),
        }
    }
    fn buffer_stream(&mut self, _stream: StreamVal, _capacity: usize) -> VResult<StreamVal> {
        unreachable!("stream buffer is not exercised by generic method tests")
    }
    fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }
    fn fs(&self) -> &dyn Fs {
        static STD: StdFs = StdFs;
        &STD
    }
}
fn c() -> C {
    C {
        cwd: std::env::temp_dir(),
    }
}
fn a(xs: Vec<Value>) -> CallArgs {
    CallArgs {
        pos: xs,
        named: vec![],
    }
}
fn call(v: Value, n: &str, args: Vec<Value>) -> VResult<Value> {
    call_method(&mut c(), v, n, a(args), Span::default())
}
#[test]
fn collection_basics() {
    let x = Value::List(vec![Value::Int(3), Value::Int(1), Value::Int(3)]);
    assert_eq!(
        call(x.clone(), "sort", vec![]).unwrap(),
        Value::List(vec![Value::Int(1), Value::Int(3), Value::Int(3)])
    );
    assert_eq!(
        call(x, "uniq", vec![]).unwrap(),
        Value::List(vec![Value::Int(3), Value::Int(1)])
    );
}
#[test]
fn higher_order() {
    let x = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    assert_eq!(
        call(x.clone(), "map", vec![Value::Str("double".into())]).unwrap(),
        Value::List(vec![Value::Int(2), Value::Int(4), Value::Int(6)])
    );
    assert_eq!(
        call(x, "where", vec![Value::Str("even".into())]).unwrap(),
        Value::List(vec![Value::Int(2)])
    );
}
#[test]
fn stream_consumption_and_tee() {
    let s = StreamVal::from_iter("int", (0..3).map(|i| Ok(Value::Int(i))));
    let clone = s.clone();
    assert!(
        matches!(call(Value::Stream(s),"collect",vec![]).unwrap(),Value::List(x) if x.len()==3)
    );
    assert_eq!(
        call(Value::Stream(clone), "collect", vec![])
            .unwrap_err()
            .code,
        "stream_consumed"
    );
    let t = StreamVal::from_iter("int", (0..2).map(|i| Ok(Value::Int(i))));
    assert!(
        matches!(call(Value::Stream(t),"tee",vec![Value::Int(2)]).unwrap(),Value::List(x) if x.len()==2)
    );
}
#[test]
fn strings_regex_records() {
    assert_eq!(
        call(Value::Str(" a b ".into()), "trim", vec![]).unwrap(),
        Value::Str("a b".into())
    );
    let re = Value::Regex(std::sync::Arc::new(RegexVal::compile("[0-9]+").unwrap()));
    assert_eq!(
        call(Value::Str("a12b3".into()), "matches", vec![re]).unwrap(),
        Value::List(vec![Value::Str("12".into()), Value::Str("3".into())])
    );
    let mut r = Record::new();
    r.insert("a".into(), Value::Int(1));
    assert_eq!(
        call(Value::Record(r), "get", vec![Value::Str("a".into())]).unwrap(),
        Value::Int(1)
    );
}
#[test]
fn chunks_flatten_sum() {
    let x = Value::List((1..=5).map(Value::Int).collect());
    assert!(
        matches!(call(x.clone(),"chunks",vec![Value::Int(2)]).unwrap(),Value::List(v) if v.len()==3)
    );
    assert_eq!(call(x, "sum", vec![]).unwrap(), Value::Int(15));
    let nested = Value::List(vec![
        Value::List(vec![Value::Int(1)]),
        Value::List(vec![Value::Int(2)]),
    ]);
    assert_eq!(
        call(nested, "flatten", vec![]).unwrap(),
        Value::List(vec![Value::Int(1), Value::Int(2)])
    );
}
#[test]
fn first_last_arity_variants() {
    let x = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    // Zero-arg forms return a single element.
    assert_eq!(call(x.clone(), "first", vec![]).unwrap(), Value::Int(1));
    assert_eq!(call(x.clone(), "last", vec![]).unwrap(), Value::Int(3));
    // `.first(n)`/`.last(n)` return a LIST of n.
    assert_eq!(
        call(x.clone(), "first", vec![Value::Int(2)]).unwrap(),
        Value::List(vec![Value::Int(1), Value::Int(2)])
    );
    assert_eq!(
        call(x.clone(), "last", vec![Value::Int(2)]).unwrap(),
        Value::List(vec![Value::Int(2), Value::Int(3)])
    );
    // Overrun clamps to the collection length (no error).
    assert_eq!(
        call(x, "first", vec![Value::Int(9)]).unwrap(),
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
    );
}

#[test]
fn outcome_methods_forward_to_out() {
    use crate::OutcomeVal;
    use std::sync::Arc;
    // An outcome whose `.out` is a list forwards collection methods.
    let outcome = Value::Outcome(Arc::new(OutcomeVal {
        status: Some(0),
        signal: None,
        ok: true,
        stdout: Arc::new(Vec::new()),
        stdout_ref: None,
        stderr: Arc::new(Vec::new()),
        dur_ns: 0,
        pid: 0,
        cmd: "x".into(),
        parsed: Some(Value::List(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
        ])),
        streamed: false,
        span: None,
    }));
    assert_eq!(call(outcome.clone(), "len", vec![]).unwrap(), Value::Int(3));
    assert_eq!(
        call(outcome, "first", vec![Value::Int(2)]).unwrap(),
        Value::List(vec![Value::Int(1), Value::Int(2)])
    );
}

#[test]
fn task_lifecycle_methods() {
    let t = crate::TaskVal::new("t");
    t.finish(Ok(Value::Int(42)));
    assert_eq!(
        call(Value::Task(t.clone()), "is_done", vec![]).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        call(Value::Task(t.clone()), "await", vec![]).unwrap(),
        Value::Int(42)
    );
    assert_eq!(call(Value::Task(t), "cancel", vec![]).unwrap(), Value::Null);
    // Wrong receiver type is a type error.
    assert_eq!(
        call(Value::Int(1), "await", vec![]).unwrap_err().code,
        "type_error"
    );
}

#[test]
fn task_suspend_resume_methods() {
    let t = crate::TaskVal::new("t");
    assert_eq!(
        call(Value::Task(t.clone()), "is_suspended", vec![]).unwrap(),
        Value::Bool(false)
    );
    // `.suspend()` returns the task (chainable) and flips the flag.
    assert!(matches!(
        call(Value::Task(t.clone()), "suspend", vec![]).unwrap(),
        Value::Task(_)
    ));
    assert!(t.is_suspended());
    assert_eq!(
        call(Value::Task(t.clone()), "is_suspended", vec![]).unwrap(),
        Value::Bool(true)
    );
    call(Value::Task(t.clone()), "resume", vec![]).unwrap();
    assert!(!t.is_suspended());
    // Suspend/resume hooks fire.
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let f = flag.clone();
    t.on_suspend(Box::new(move || {
        f.store(true, std::sync::atomic::Ordering::SeqCst)
    }));
    t.suspend();
    assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
    // Wrong receiver type is a type error.
    assert_eq!(
        call(Value::Int(1), "suspend", vec![]).unwrap_err().code,
        "type_error"
    );
}

#[test]
fn path_pure_component_methods() {
    let p = || Value::Path(PathBuf::from("/a/b/file.tar.gz"));
    assert_eq!(
        call(p(), "name", vec![]).unwrap(),
        Value::Str("file.tar.gz".into())
    );
    assert_eq!(
        call(p(), "stem", vec![]).unwrap(),
        Value::Str("file.tar".into())
    );
    assert_eq!(call(p(), "ext", vec![]).unwrap(), Value::Str("gz".into()));
    assert_eq!(
        call(p(), "parent", vec![]).unwrap(),
        Value::Path(PathBuf::from("/a/b"))
    );
    assert_eq!(
        call(p(), "join", vec![Value::Str("x".into())]).unwrap(),
        Value::Path(PathBuf::from("/a/b/file.tar.gz/x"))
    );
    // An extensionless / rootless path yields nulls where appropriate.
    assert_eq!(
        call(Value::Path(PathBuf::from("README")), "ext", vec![]).unwrap(),
        Value::Null
    );
    assert_eq!(
        call(Value::Path(PathBuf::from("/")), "parent", vec![]).unwrap(),
        Value::Null
    );
    // `.abs()` absolutizes a relative path against the ctx cwd.
    let cwd = std::env::temp_dir();
    assert_eq!(
        call(Value::Path(PathBuf::from("rel/x")), "abs", vec![]).unwrap(),
        Value::Path(cwd.join("rel/x"))
    );
    // `.str()` remains the fallible converter, still reaching a path.
    assert_eq!(
        call(Value::Path(PathBuf::from("/a/b")), "str", vec![]).unwrap(),
        Value::Str("/a/b".into())
    );
}

#[test]
fn unknown_method_carries_did_you_mean_hint() {
    let list = Value::List(vec![Value::Int(1)]);
    let e = call(list.clone(), "length", vec![]).unwrap_err();
    assert_eq!(e.code, "field_missing");
    assert_eq!(e.hint.as_deref(), Some("did you mean .len()?"));
    let e = call(list.clone(), "size", vec![]).unwrap_err();
    assert_eq!(e.hint.as_deref(), Some("did you mean .len()?"));
    let e = call(Value::Str("a".into()), "to_upper", vec![]).unwrap_err();
    assert_eq!(e.hint.as_deref(), Some("did you mean .upper()?"));
    let e = call(Value::Path(PathBuf::from("x")), "read_str", vec![]).unwrap_err();
    assert_eq!(e.hint.as_deref(), Some("did you mean .read()?"));
    let e = call(list.clone(), "push", vec![Value::Int(2)]).unwrap_err();
    assert!(e.hint.unwrap().contains("immutable"));
    let e = call(Value::Str("ab".into()), "substring", vec![Value::Int(1)]).unwrap_err();
    assert!(e.hint.unwrap().contains(".take"));
    // A near-typo resolves by edit distance.
    let e = call(list, "sortt", vec![]).unwrap_err();
    assert_eq!(e.hint.as_deref(), Some("did you mean .sort()?"));
    // Nothing plausible → no hint, same error as before.
    let e = call(Value::Int(1), "frobnicate", vec![]).unwrap_err();
    assert_eq!(e.code, "field_missing");
    assert_eq!(e.hint, None);
}

#[test]
fn scalar_str_renders_canonical_form() {
    assert_eq!(
        call(Value::Int(42), "str", vec![]).unwrap(),
        Value::Str("42".into())
    );
    assert_eq!(
        call(Value::Float(1.5), "str", vec![]).unwrap(),
        Value::Str("1.5".into())
    );
    assert_eq!(
        call(Value::Bool(true), "str", vec![]).unwrap(),
        Value::Str("true".into())
    );
    // Unconverted types keep erroring, now with a teaching hint.
    let e = call(Value::List(vec![]), "str", vec![]).unwrap_err();
    assert_eq!(e.code, "type_error");
    assert!(e.hint.unwrap().contains("interpolation"));
}

#[test]
fn required_str_args_error_when_missing() {
    let s = || Value::Str("hello".into());
    for (method, args) in [
        ("starts_with", vec![]),
        ("ends_with", vec![]),
        ("split", vec![]),
        ("replace", vec![Value::Str("l".into())]),
    ] {
        let e = call(s(), method, args).unwrap_err();
        assert_eq!(e.code, "arg_error", "{method} must require its argument");
    }
    let e = call(Value::Record(Record::new()), "set", vec![]).unwrap_err();
    assert_eq!(e.code, "arg_error");
    assert!(e.msg.contains("key"));
    // Explicit empty-string arguments are still legal.
    assert_eq!(
        call(s(), "starts_with", vec![Value::Str("".into())]).unwrap(),
        Value::Bool(true)
    );
    // `.join()` keeps its deliberate "" default (concatenation).
    assert_eq!(
        call(
            Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
            "join",
            vec![]
        )
        .unwrap(),
        Value::Str("ab".into())
    );
}

#[test]
fn zero_arg_aggregates_reject_stray_args() {
    let x = || Value::List(vec![Value::Int(3), Value::Int(1), Value::Int(2)]);
    // A stray arg (classically a projection lambda) is a loud arg_error that
    // names the method and points at the `.map(f).<agg>()` idiom — not a
    // silently-dropped argument.
    for agg in ["sum", "min", "max"] {
        let e = call(x(), agg, vec![Value::Str("f".into())]).unwrap_err();
        assert_eq!(e.code, "arg_error", "{agg} must reject a stray arg");
        assert!(
            e.msg.contains(&format!("{agg} takes no arguments")),
            "{agg} error should name the method: {}",
            e.msg
        );
        assert!(e.msg.contains(".map(f)"), "{agg} error should suggest .map");
    }
    // The no-arg forms are unaffected (the bare `.sum` field->method
    // fallback also lands here with empty args).
    assert_eq!(call(x(), "sum", vec![]).unwrap(), Value::Int(6));
    assert_eq!(call(x(), "min", vec![]).unwrap(), Value::Int(1));
    assert_eq!(call(x(), "max", vec![]).unwrap(), Value::Int(3));
}

#[test]
fn methods_for_agrees_with_dispatch() {
    // Every name `methods_for` advertises for a receiver type must be a name
    // `dispatch` actually recognizes — i.e. calling it never yields the
    // `field_missing` "unknown method" error. (It may error on missing
    // arguments or a type mismatch; that still proves the arm exists.) This
    // is the guard that the completion vocabulary can't drift ahead of the
    // real method table.
    let sample = |ty: &str| -> Value {
        match ty {
            "list" => Value::List(vec![Value::Int(1), Value::Int(2)]),
            "str" => Value::Str("hi".into()),
            "record" => {
                let mut r = Record::new();
                r.insert("a".into(), Value::Int(1));
                Value::Record(r)
            }
            "int" => Value::Int(3),
            "float" => Value::Float(1.5),
            "bytes" => Value::Bytes(std::sync::Arc::new(vec![1, 2, 3])),
            _ => unreachable!(),
        }
    };
    for ty in ["list", "str", "record", "int", "float", "bytes"] {
        for m in methods_for(ty).unwrap() {
            if let Err(e) = call(sample(ty), m, vec![]) {
                assert_ne!(
                    e.code, "field_missing",
                    "methods_for({ty}) advertises `.{m}` but dispatch rejects it as unknown"
                );
            }
        }
    }
}

#[test]
fn save_and_append() {
    let d = std::env::temp_dir().join(format!("shoal-methods-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let mut ctx = C { cwd: d.clone() };
    call_method(
        &mut ctx,
        Value::Str("a".into()),
        "save",
        a(vec![Value::Str("x".into())]),
        Span::default(),
    )
    .unwrap();
    call_method(
        &mut ctx,
        Value::Str("b".into()),
        "append",
        a(vec![Value::Str("x".into())]),
        Span::default(),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(d.join("x")).unwrap(), "ab");
    std::fs::remove_dir_all(d).unwrap();
}

/// HR-C4: `path`/value `.save`/`.append` and stream `.save`/`.append` must
/// cross the injected [`Fs`] port, so a recording adapter observes every
/// write and a denying adapter can refuse it — with nothing landing on the
/// real filesystem behind the port's back.
mod fs_port_boundary {
    use super::*;
    use crate::StreamVal;
    use crate::ports::{Fs, ReadSeek};
    use std::io;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// One observed filesystem write, recorded by [`SpyFs`].
    #[derive(Debug, Clone, PartialEq)]
    enum FsEvent {
        Write(PathBuf, Vec<u8>),
        OpenAppend(PathBuf),
        Append(PathBuf, Vec<u8>),
    }

    /// A test [`Fs`] adapter that records the write sinks (`write`,
    /// `append`, `open_append`) into a shared log and, when `deny` is set,
    /// refuses them with a `PermissionDenied` error. Every other method is
    /// `unreachable!` so any unexpected filesystem escape shows up loudly.
    #[derive(Clone)]
    struct SpyFs {
        events: Arc<Mutex<Vec<FsEvent>>>,
        deny: bool,
    }

    impl SpyFs {
        fn recording() -> Self {
            Self {
                events: Arc::default(),
                deny: false,
            }
        }
        fn denying() -> Self {
            Self {
                events: Arc::default(),
                deny: true,
            }
        }
        fn events(&self) -> Vec<FsEvent> {
            self.events.lock().unwrap().clone()
        }
        fn deny_err() -> io::Error {
            io::Error::new(io::ErrorKind::PermissionDenied, "denied by policy")
        }
    }

    /// The writer [`SpyFs::open_append`] hands back: each `write` is logged
    /// as an `Append`, exactly modeling the incremental stream sink.
    struct SpyWriter {
        events: Arc<Mutex<Vec<FsEvent>>>,
        path: PathBuf,
    }
    impl io::Write for SpyWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.events
                .lock()
                .unwrap()
                .push(FsEvent::Append(self.path.clone(), buf.to_vec()));
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Fs for SpyFs {
        fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
            if self.deny {
                return Err(Self::deny_err());
            }
            self.events
                .lock()
                .unwrap()
                .push(FsEvent::Write(path.to_path_buf(), data.to_vec()));
            Ok(())
        }
        fn append(&self, path: &Path, data: &[u8]) -> io::Result<()> {
            if self.deny {
                return Err(Self::deny_err());
            }
            self.events
                .lock()
                .unwrap()
                .push(FsEvent::Append(path.to_path_buf(), data.to_vec()));
            Ok(())
        }
        fn open_append(&self, path: &Path) -> io::Result<Box<dyn io::Write + Send>> {
            if self.deny {
                return Err(Self::deny_err());
            }
            self.events
                .lock()
                .unwrap()
                .push(FsEvent::OpenAppend(path.to_path_buf()));
            Ok(Box::new(SpyWriter {
                events: self.events.clone(),
                path: path.to_path_buf(),
            }))
        }
        fn read(&self, _p: &Path) -> io::Result<Vec<u8>> {
            unreachable!("read not exercised by the write-sink port test")
        }
        fn read_to_string(&self, _p: &Path) -> io::Result<String> {
            unreachable!("read_to_string not exercised")
        }
        fn open_read(&self, _p: &Path) -> io::Result<Box<dyn ReadSeek + Send>> {
            unreachable!("open_read not exercised")
        }
        fn touch(&self, _p: &Path) -> io::Result<()> {
            unreachable!("touch not exercised")
        }
        fn metadata(&self, _p: &Path) -> io::Result<std::fs::Metadata> {
            unreachable!("metadata not exercised")
        }
        fn symlink_metadata(&self, _p: &Path) -> io::Result<std::fs::Metadata> {
            unreachable!("symlink_metadata not exercised")
        }
        fn read_dir(&self, _p: &Path) -> io::Result<Vec<PathBuf>> {
            unreachable!("read_dir not exercised")
        }
        fn create_dir(&self, _p: &Path) -> io::Result<()> {
            unreachable!("create_dir not exercised")
        }
        fn create_dir_all(&self, _p: &Path) -> io::Result<()> {
            unreachable!("create_dir_all not exercised")
        }
        fn remove_file(&self, _p: &Path) -> io::Result<()> {
            unreachable!("remove_file not exercised")
        }
        fn remove_dir_all(&self, _p: &Path) -> io::Result<()> {
            unreachable!("remove_dir_all not exercised")
        }
        fn rename(&self, _f: &Path, _t: &Path) -> io::Result<()> {
            unreachable!("rename not exercised")
        }
        fn copy(&self, _f: &Path, _t: &Path) -> io::Result<u64> {
            unreachable!("copy not exercised")
        }
        fn hard_link(&self, _s: &Path, _d: &Path) -> io::Result<()> {
            unreachable!("hard_link not exercised")
        }
        fn symlink(&self, _t: &Path, _l: &Path) -> io::Result<()> {
            unreachable!("symlink not exercised")
        }
    }

    /// A `CallCtx` whose `fs()` returns the injected [`SpyFs`], so the
    /// value-method write sinks resolve to it.
    struct SpyCtx {
        fs: SpyFs,
        cwd: PathBuf,
    }
    impl CallCtx for SpyCtx {
        fn call_closure(&mut self, _f: &Value, _args: Vec<Value>) -> VResult<Value> {
            Err(ErrorVal::new(
                "custom",
                "no closures in the port-boundary test",
            ))
        }
        fn buffer_stream(&mut self, _stream: StreamVal, _capacity: usize) -> VResult<StreamVal> {
            unreachable!("stream buffer is not exercised by filesystem port tests")
        }
        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }
        fn fs(&self) -> &dyn Fs {
            &self.fs
        }
    }

    fn int_stream() -> StreamVal {
        StreamVal::from_iter("int", (1..=2).map(|i| Ok(Value::Int(i))))
    }

    #[test]
    fn path_save_and_append_are_observed_by_the_port() {
        let fs = SpyFs::recording();
        let mut ctx = SpyCtx {
            fs: fs.clone(),
            cwd: PathBuf::from("/spy-root"),
        };
        call_method(
            &mut ctx,
            Value::Str("hello".into()),
            "save",
            a(vec![Value::Str("out.txt".into())]),
            Span::default(),
        )
        .unwrap();
        call_method(
            &mut ctx,
            Value::Str("more".into()),
            "append",
            a(vec![Value::Str("out.txt".into())]),
            Span::default(),
        )
        .unwrap();
        let p = PathBuf::from("/spy-root/out.txt");
        assert_eq!(
            fs.events(),
            vec![
                FsEvent::Write(p.clone(), b"hello".to_vec()),
                FsEvent::Append(p, b"more".to_vec()),
            ],
            "path .save/.append must cross the Fs port with the resolved path + bytes"
        );
    }

    #[test]
    fn stream_save_is_observed_by_the_port() {
        let fs = SpyFs::recording();
        let mut ctx = SpyCtx {
            fs: fs.clone(),
            cwd: PathBuf::from("/spy-root"),
        };
        call_method(
            &mut ctx,
            Value::Stream(int_stream()),
            "save",
            a(vec![Value::Str("log".into())]),
            Span::default(),
        )
        .unwrap();
        let p = PathBuf::from("/spy-root/log");
        assert_eq!(
            fs.events(),
            vec![
                FsEvent::OpenAppend(p.clone()),
                FsEvent::Append(p.clone(), b"1\n".to_vec()),
                FsEvent::Append(p, b"2\n".to_vec()),
            ],
            "stream .save must open once through the port and append each item"
        );
    }

    #[test]
    fn a_denying_port_refuses_path_save_and_writes_nothing_to_disk() {
        // A REAL, writable tempdir: if the write bypassed the (denying) port
        // and hit `std::fs` directly, the file WOULD appear here.
        let dir = std::env::temp_dir().join(format!("shoal-lanec-deny-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = SpyCtx {
            fs: SpyFs::denying(),
            cwd: dir.clone(),
        };
        let err = call_method(
            &mut ctx,
            Value::Str("secret".into()),
            "save",
            a(vec![Value::Str("leak.txt".into())]),
            Span::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, "custom");
        assert!(
            !dir.join("leak.txt").exists(),
            "a denied .save must not write directly to disk"
        );
        // Append is denied and leaves nothing behind either.
        let err = call_method(
            &mut ctx,
            Value::Str("secret".into()),
            "append",
            a(vec![Value::Str("leak.txt".into())]),
            Span::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, "custom");
        assert!(!dir.join("leak.txt").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_denying_port_refuses_stream_save_and_writes_nothing_to_disk() {
        let dir = std::env::temp_dir().join(format!("shoal-lanec-deny-s-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = SpyCtx {
            fs: SpyFs::denying(),
            cwd: dir.clone(),
        };
        let err = call_method(
            &mut ctx,
            Value::Stream(int_stream()),
            "save",
            a(vec![Value::Str("leak.log".into())]),
            Span::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, "custom");
        assert!(
            !dir.join("leak.log").exists(),
            "a denied stream .save must not write directly to disk"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Pin the explicit ambient-authority choice: an embedding that wants
    /// ordinary host behavior can return `StdFs`, but `CallCtx` provides no
    /// implicit real-filesystem default.
    #[test]
    fn call_ctx_can_explicitly_choose_std_fs() {
        struct ExplicitStd;
        impl CallCtx for ExplicitStd {
            fn call_closure(&mut self, _f: &Value, _a: Vec<Value>) -> VResult<Value> {
                unreachable!()
            }
            fn buffer_stream(
                &mut self,
                _stream: StreamVal,
                _capacity: usize,
            ) -> VResult<StreamVal> {
                unreachable!()
            }
            fn cwd(&self) -> PathBuf {
                std::env::temp_dir()
            }
            fn fs(&self) -> &dyn Fs {
                static STD: StdFs = StdFs;
                &STD
            }
        }
        let dir = std::env::temp_dir().join(format!("shoal-lanec-std-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("real.txt");
        ExplicitStd.fs().write(&p, b"on disk").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "on disk");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn group_by_is_an_alias_for_group() {
    let x = Value::List(vec![
        Value::Int(1),
        Value::Int(2),
        Value::Int(3),
        Value::Int(4),
    ]);
    let key = Value::Str("even".into());
    assert_eq!(
        call(x.clone(), "group_by", vec![key.clone()]).unwrap(),
        call(x, "group", vec![key]).unwrap()
    );
}

// -------------------------------------------------------------------
// CasBytes chokepoint tests: the dispatch-level end-to-end
// proof that `cheap_method` and `json_preview` are actually wired in,
// on top of the direct unit tests in `value_types.rs`/`json.rs`.
// -------------------------------------------------------------------
mod cas_bytes_chokepoint {
    use super::*;
    use crate::value_types::test_support;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn probe() -> (Value, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = test_support::cas_bytes(b"hel", b"hello world", calls.clone());
        (Value::CasBytes(Arc::new(c)), calls)
    }

    #[test]
    fn cheap_methods_never_load_through_dispatch() {
        let (v, calls) = probe();
        assert_eq!(call(v.clone(), "len", vec![]).unwrap(), Value::Int(11));
        assert_eq!(call(v.clone(), "count", vec![]).unwrap(), Value::Int(11));
        assert_eq!(
            call(v.clone(), "is_empty", vec![]).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            call(v, "ref", vec![]).unwrap(),
            Value::Str("val:blake3:deadbeefcafef00d".into())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn load_and_bytes_materialize_exactly_once() {
        let (v, calls) = probe();
        assert_eq!(
            call(v, "load", vec![]).unwrap(),
            Value::Bytes(Arc::new(b"hello world".to_vec()))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Existing behavior preserved: a bare `.json()` METHOD call on a
    /// CasBytes value still fully materializes (unlike the nested case),
    /// because it falls through `dispatch`'s `_ =>` arm to a resolved
    /// `Value::Bytes` before `value_to_json` is ever consulted.
    #[test]
    fn bare_json_method_still_fully_materializes() {
        let (v, calls) = probe();
        let out = call(v, "json", vec![]).unwrap();
        assert_eq!(out, Value::Str("\"hello world\"".into()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// The actual fix, exercised through the real dispatch path a shell
    /// user hits: `.json()` on a RECORD whose field is a spilled capture
    /// does not load the CAS just to serialize the record.
    #[test]
    fn json_on_a_record_with_a_nested_cas_bytes_field_does_not_load() {
        let (v, calls) = probe();
        let mut r = Record::new();
        r.insert("out".into(), v);
        let out = call(Value::Record(r), "json", vec![]).unwrap();
        let Value::Str(s) = out else {
            panic!("expected str");
        };
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let j: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(j["out"]["$"], "bytes_ref");
        assert_eq!(j["out"]["len"], 11);
    }

    /// `render()` stays cheap too (unchanged — verified end to end).
    #[test]
    fn render_does_not_load() {
        let (v, calls) = probe();
        let Value::CasBytes(c) = &v else {
            unreachable!()
        };
        let rendered = crate::render::render_inline(&Value::CasBytes(c.clone()));
        assert!(rendered.contains("val:blake3:deadbeefcafef00d"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
