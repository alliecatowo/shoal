use std::io;
use std::path::Path;
use std::sync::Arc;

use shoal_eval::Evaluator;
use shoal_value::Opener;

struct UnsupportedOpener;

impl Opener for UnsupportedOpener {
    fn open(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "desktop integration unavailable in this host adapter",
        ))
    }
}

struct SaturatedOpener;

impl Opener for SaturatedOpener {
    fn open(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "desktop opener admission is saturated",
        ))
    }
}

#[test]
fn unsupported_host_adapter_reaches_the_language_as_a_typed_error() {
    let root = tempfile::tempdir().unwrap();
    let program = shoal_syntax::parse("open(\"artifact.txt\")").unwrap();
    let mut evaluator = Evaluator::new(root.path().to_path_buf());
    evaluator.set_opener(Arc::new(UnsupportedOpener));

    let error = evaluator.eval_program(&program).unwrap_err();
    assert_eq!(error.code, "unsupported");
    assert!(error.msg.contains("desktop integration unavailable"));
}

#[test]
fn saturated_host_adapter_reaches_the_language_as_resource_busy() {
    let root = tempfile::tempdir().unwrap();
    let program = shoal_syntax::parse("open(\"artifact.txt\")").unwrap();
    let mut evaluator = Evaluator::new(root.path().to_path_buf());
    evaluator.set_opener(Arc::new(SaturatedOpener));

    let error = evaluator.eval_program(&program).unwrap_err();
    assert_eq!(error.code, "resource_busy");
    assert!(error.msg.contains("saturated"));
}
