use super::*;
use serde_json::json;
use std::io::Seek;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::Arc;

#[test]
fn secrets_never_persist_and_revoke_expiry_work() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("tokens.json");
    let mut s = TokenStore::open(&p).unwrap();
    let (secret, m) = s
        .create("agent:a".into(), "dev".into(), vec!["fs.read".into()], None)
        .unwrap();
    assert!(
        !String::from_utf8(fs::read(&p).unwrap())
            .unwrap()
            .contains(&secret)
    );
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(s.validate(&secret).unwrap().principal, "agent:a");
    assert!(s.revoke(&m.id).unwrap());
    assert!(s.validate(&secret).is_none());
    let (expired, _) = s
        .create("agent:b".into(), "x".into(), vec![], Some(-1))
        .unwrap();
    assert!(s.validate(&expired).is_none());
}

#[test]
fn reopen_roundtrip_and_cross_store_revoke_are_fresh() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("tokens.json");
    let mut creator = TokenStore::open(&p).unwrap();
    let (bearer, meta) = creator
        .create("agent:roundtrip".into(), "dev".into(), vec![], None)
        .unwrap();

    let reader = TokenStore::open(&p).unwrap();
    assert_eq!(reader.validate(&bearer).unwrap().id, meta.id);
    assert_eq!(reader.refresh_authenticated(&meta).unwrap().id, meta.id);
    let mut revoker = TokenStore::open(&p).unwrap();
    assert!(revoker.revoke(&meta.id).unwrap());
    // validate reloads under a shared file lock, so the already-open reader
    // observes another process/store's revoke rather than trusting a stale
    // startup snapshot.
    assert!(reader.validate(&bearer).is_none());
    assert!(reader.refresh_authenticated(&meta).is_none());
}

#[test]
fn malformed_snapshot_is_typed_and_never_reuses_cached_authority() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("tokens.json");
    let mut creator = TokenStore::open(&path).unwrap();
    let (bearer, meta) = creator
        .create("agent:cached".into(), "dev".into(), vec![], None)
        .unwrap();
    let reader = Arc::new(TokenStore::open(&path).unwrap());

    let mut stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    // Even byte length plus a multibyte character exercised a prior UTF-8
    // slicing panic in the hex decoder.
    stored["tokens"][0]["digest"] = serde_json::Value::String("aéx".into());
    fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

    for _ in 0..8 {
        assert_eq!(
            reader.validate_checked(&bearer).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(reader.validate(&bearer).is_none());
        assert_eq!(
            reader
                .refresh_authenticated_checked(&meta)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(reader.refresh_authenticated(&meta).is_none());
        assert!(reader.try_list().is_err());
        assert!(reader.list().is_empty());
    }

    let workers: Vec<_> = (0..8)
        .map(|_| {
            let reader = Arc::clone(&reader);
            let bearer = bearer.clone();
            std::thread::spawn(move || {
                for _ in 0..16 {
                    assert!(reader.validate_checked(&bearer).is_err());
                    assert!(reader.validate(&bearer).is_none());
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("authentication worker must not panic");
    }
}

#[test]
fn authority_snapshot_limits_and_schema_fail_closed_without_rewriting() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("tokens.json");
    let mut creator = TokenStore::open(&path).unwrap();
    let (bearer, _) = creator
        .create(
            "agent:bounded".into(),
            "default".into(),
            vec!["proc.spawn".into(), "fs.read".into()],
            None,
        )
        .unwrap();
    let reader = TokenStore::open(&path).unwrap();
    let valid = fs::read(&path).unwrap();

    type Corruption = Box<dyn Fn(&mut serde_json::Value)>;
    let corruptions: Vec<Corruption> = vec![
        Box::new(|doc| doc["unknown"] = json!(true)),
        Box::new(|doc| doc["tokens"][0]["unknown"] = json!({"nested": [1, 2, 3]})),
        Box::new(|doc| doc["tokens"][0]["principal"] = json!("p".repeat(MAX_PRINCIPAL_BYTES + 1))),
        Box::new(|doc| doc["tokens"][0]["profile"] = json!("p".repeat(MAX_PROFILE_BYTES + 1))),
        Box::new(|doc| doc["tokens"][0]["caps"] = json!(vec!["x"; MAX_CAPABILITIES_PER_TOKEN + 1])),
        Box::new(|doc| doc["tokens"][0]["caps"] = json!(["fs.read", "fs.read"])),
        Box::new(|doc| {
            let digest = doc["tokens"][0]["digest"].as_str().unwrap().to_uppercase();
            doc["tokens"][0]["digest"] = json!(digest);
        }),
        Box::new(|doc| {
            let id = doc["tokens"][0]["id"].as_str().unwrap().to_uppercase();
            doc["tokens"][0]["id"] = json!(id);
        }),
    ];
    for corrupt in corruptions {
        let mut doc: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        corrupt(&mut doc);
        let corrupt_bytes = serde_json::to_vec(&doc).unwrap();
        fs::write(&path, &corrupt_bytes).unwrap();
        for _ in 0..3 {
            assert_eq!(
                reader.validate_checked(&bearer).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert!(reader.validate(&bearer).is_none());
            assert_eq!(fs::read(&path).unwrap(), corrupt_bytes);
        }
    }

    fs::write(&path, &valid).unwrap();
    let mut sparse = fs::OpenOptions::new().write(true).open(&path).unwrap();
    sparse
        .seek(std::io::SeekFrom::Start(MAX_TOKEN_STORE_BYTES as u64))
        .unwrap();
    sparse.write_all(&[0]).unwrap();
    sparse.sync_all().unwrap();
    let oversized_len = sparse.metadata().unwrap().len();
    drop(sparse);
    for _ in 0..3 {
        assert_eq!(
            reader.validate_checked(&bearer).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), oversized_len);
    }
}

#[test]
fn create_admission_is_canonical_bounded_and_transactional() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("tokens.json");
    let mut store = TokenStore::open(&path).unwrap();
    let before = fs::read(&path).unwrap();

    for result in [
        store.create("".into(), "default".into(), vec![], None),
        store.create(
            "p".repeat(MAX_PRINCIPAL_BYTES + 1),
            "default".into(),
            vec![],
            None,
        ),
        store.create(
            "agent:a".into(),
            "p".repeat(MAX_PROFILE_BYTES + 1),
            vec![],
            None,
        ),
        store.create(
            "agent:a".into(),
            "default".into(),
            vec!["fs.read".into(), "fs.read".into()],
            None,
        ),
    ] {
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(store.tokens.is_empty());
    }

    let (_, meta) = store
        .create(
            "agent:a".into(),
            "default".into(),
            vec!["proc.spawn".into(), "fs.read".into()],
            None,
        )
        .unwrap();
    assert_eq!(meta.caps, ["fs.read", "proc.spawn"]);

    // Force atomic replacement to fail after admission. The candidate is
    // not installed in memory and the last good authority file survives.
    let good = fs::read(&path).unwrap();
    fs::create_dir(tempfile_path(&path)).unwrap();
    let count = store.tokens.len();
    assert!(
        store
            .create("agent:b".into(), "default".into(), vec![], None)
            .is_err()
    );
    assert_eq!(store.tokens.len(), count);
    assert_eq!(fs::read(&path).unwrap(), good);
}

#[test]
fn absurd_bearer_is_rejected_before_authority_io() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("tokens.json");
    let store = TokenStore::open(&path).unwrap();
    fs::write(&path, b"malformed").unwrap();
    for bearer in ["short".to_string(), "x".repeat(1_000_000)] {
        assert_eq!(
            store.validate_checked(&bearer).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[test]
fn token_count_limit_rejects_whole_snapshot_and_capacity_create() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("tokens.json");
    let mut store = TokenStore::open(&path).unwrap();
    let (_, _) = store
        .create("agent:seed".into(), "default".into(), vec![], None)
        .unwrap();
    let mut doc: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let seed = doc["tokens"][0].clone();
    let mut tokens = Vec::with_capacity(MAX_TOKENS + 1);
    for index in 0..MAX_TOKENS {
        let mut token = seed.clone();
        let mut digest = [0_u8; 32];
        digest[..8].copy_from_slice(&(index as u64).to_be_bytes());
        token["id"] = json!(hex(&digest[..8]));
        token["digest"] = json!(hex(&digest));
        token["principal"] = json!(format!("agent:{index}"));
        tokens.push(token);
    }
    doc["tokens"] = json!(tokens);
    let full = serde_json::to_vec(&doc).unwrap();
    assert!(full.len() < MAX_TOKEN_STORE_BYTES);
    fs::write(&path, &full).unwrap();
    store.reload_unlocked().unwrap();
    let before = fs::read(&path).unwrap();
    assert_eq!(
        store
            .create("agent:overflow".into(), "default".into(), vec![], None)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(store.tokens.len(), MAX_TOKENS);

    let mut over: serde_json::Value = serde_json::from_slice(&before).unwrap();
    let extra = over["tokens"][0].clone();
    over["tokens"].as_array_mut().unwrap().push(extra);
    let over_bytes = serde_json::to_vec(&over).unwrap();
    fs::write(&path, &over_bytes).unwrap();
    assert_eq!(
        store.try_list().unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(fs::read(&path).unwrap(), over_bytes);
}

#[test]
fn production_has_no_raw_panicking_lock_access() {
    let production = include_str!("lib.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap_or_default();
    let compact: String = production.chars().filter(|c| !c.is_whitespace()).collect();
    for forbidden in [
        ".lock().unwrap()",
        ".read().unwrap()",
        ".write().unwrap()",
        ".lock().expect(",
        ".read().expect(",
        ".write().expect(",
    ] {
        assert!(
            !compact.contains(forbidden),
            "production auth synchronization must return typed errors: {forbidden}"
        );
    }
}

#[test]
fn token_store_permissions_are_owner_only() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("state").join("tokens.json");
    let mut store = TokenStore::open(&p).unwrap();
    store
        .create("agent:a".into(), "dev".into(), vec![], None)
        .unwrap();
    assert_eq!(
        fs::metadata(p.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for path in [&p, &lock_path(&p)] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600,
            "{} must be owner-only",
            path.display()
        );
    }
}

#[test]
fn concurrent_process_writers_preserve_every_token() {
    const WORKER_ENV: &str = "SHOAL_AUTH_LOCK_TEST_WORKER";
    const PATH_ENV: &str = "SHOAL_AUTH_LOCK_TEST_PATH";

    if let Some(principal) = std::env::var_os(WORKER_ENV) {
        let path = PathBuf::from(std::env::var_os(PATH_ENV).expect("worker store path"));
        let mut store = TokenStore::open(path).unwrap();
        store
            .create(
                principal.to_string_lossy().into_owned(),
                "test".into(),
                vec![],
                None,
            )
            .unwrap();
        return;
    }

    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("state").join("tokens.json");
    let exe = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for i in 0..12 {
        children.push(
            Command::new(&exe)
                .args([
                    "--exact",
                    "tests::concurrent_process_writers_preserve_every_token",
                    "--nocapture",
                ])
                .env(WORKER_ENV, format!("agent:{i}"))
                .env(PATH_ENV, &path)
                .spawn()
                .unwrap(),
        );
    }
    for child in children {
        assert!(child.wait_with_output().unwrap().status.success());
    }

    let store = TokenStore::open(&path).unwrap();
    let mut principals: Vec<_> = store
        .try_list()
        .unwrap()
        .into_iter()
        .map(|m| m.principal)
        .collect();
    principals.sort();
    assert_eq!(principals.len(), 12, "a concurrent update was lost");
    for i in 0..12 {
        assert!(principals.contains(&format!("agent:{i}")));
    }
}

#[test]
fn concurrent_readers_and_writer_keep_valid_snapshots_live() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("tokens.json");
    let mut writer = TokenStore::open(&path).unwrap();
    let (bearer, _) = writer
        .create("agent:stable".into(), "test".into(), vec![], None)
        .unwrap();
    let reader = Arc::new(TokenStore::open(&path).unwrap());
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let reader = Arc::clone(&reader);
            let bearer = bearer.clone();
            std::thread::spawn(move || {
                for _ in 0..64 {
                    assert_eq!(
                        reader.validate_checked(&bearer).unwrap().unwrap().principal,
                        "agent:stable"
                    );
                }
            })
        })
        .collect();
    for index in 0..32 {
        writer
            .create(format!("agent:{index}"), "test".into(), vec![], None)
            .unwrap();
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(reader.try_list().unwrap().len(), 33);
}
