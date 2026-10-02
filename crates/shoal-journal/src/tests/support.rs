use super::*;

pub(super) fn rec(session: &str, principal: &str, ts_ns: i64, src: &str) -> EntryRecord {
    EntryRecord {
        kind: EntryKind::Statement,
        parent_id: None,
        session: session.to_string(),
        principal: principal.to_string(),
        ts_ns,
        cwd: b"/home/user/proj".to_vec(),
        src: src.to_string(),
        ast_json: r#"{"kind":"call","cmd":"x"}"#.to_string(),
        effects_json: r#"["opaque"]"#.to_string(),
        opaque: true,
    }
}

pub(super) fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            n += count_files(&entry.path());
        } else {
            n += 1;
        }
    }
    n
}

pub(super) fn storage_admission(error: &rusqlite::Error) -> Option<&StorageAdmissionError> {
    match error {
        rusqlite::Error::ToSqlConversionFailure(error) => {
            error.downcast_ref::<StorageAdmissionError>()
        }
        _ => None,
    }
}
