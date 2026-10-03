use super::*;
use std::io::{Seek, Write};
use std::process::Command;
use std::sync::Arc;

fn write_plaintext(store: &SecretStore, plain: &[u8]) -> Vec<u8> {
    let key = store.key().unwrap();
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    let nonce = [7u8; 12];
    let ciphertext = cipher.encrypt((&nonce).into(), plain).unwrap();
    assert_eq!(ciphertext.len(), plain.len() + 16);
    let envelope = Envelope {
        version: 1,
        nonce: base64::engine::general_purpose::STANDARD.encode(nonce),
        ciphertext: base64::engine::general_purpose::STANDARD.encode(ciphertext),
    };
    let bytes = serde_json::to_vec(&envelope).unwrap();
    atomic(&store.data_path(), &bytes).unwrap();
    bytes
}

fn assert_snapshot_failure_is_stable(store: &SecretStore, bytes: &[u8]) {
    for _ in 0..3 {
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            store.get("TOKEN").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            store.set("TOKEN", b"replacement").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            store.delete("TOKEN").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(store.data_path()).unwrap(), bytes);
    }
}

fn assert_snapshot_failure_once(store: &SecretStore, bytes: &[u8]) {
    assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(store.data_path()).unwrap(), bytes);
}

#[test]
fn lifecycle_and_redaction() {
    let t = tempfile::tempdir().unwrap();
    let s = SecretStore::open(t.path()).unwrap();
    s.set("TOKEN", b"super-secret").unwrap();
    assert_eq!(&*s.get("TOKEN").unwrap().unwrap(), b"super-secret");
    assert_eq!(s.list().unwrap(), ["TOKEN"]);
    let disk = fs::read_to_string(t.path().join("secrets.json")).unwrap();
    assert!(!disk.contains("super-secret"));
    assert!(!format!("{s:?}").contains("super-secret"));
    assert!(s.delete("TOKEN").unwrap());
    assert!(s.get("TOKEN").unwrap().is_none())
}
#[test]
fn tamper_detected() {
    let t = tempfile::tempdir().unwrap();
    let s = SecretStore::open(t.path()).unwrap();
    s.set("A", b"x").unwrap();
    let p = t.path().join("secrets.json");
    let mut b = fs::read(&p).unwrap();
    let n = b.len();
    b[n - 2] ^= 1;
    fs::write(p, b).unwrap();
    assert!(s.list().is_err())
}

#[test]
fn malformed_nonce_is_typed_under_repeated_concurrent_access() {
    let t = tempfile::tempdir().unwrap();
    let store = Arc::new(SecretStore::open(t.path()).unwrap());
    store.set("TOKEN", b"authority").unwrap();
    let path = t.path().join("secrets.json");
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    envelope["nonce"] =
        serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode([0u8; 11]));
    fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();

    for _ in 0..8 {
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            store.get("TOKEN").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            store.set("TOKEN", b"replacement").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            store.delete("TOKEN").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    let workers: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                for _ in 0..16 {
                    assert!(store.list().is_err());
                    assert!(store.get("TOKEN").is_err());
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("secret-store worker must not panic");
    }
}

#[test]
fn encrypted_snapshot_file_wall_precedes_decode_and_preserves_evidence() {
    let t = tempfile::tempdir().unwrap();
    let store = SecretStore::open(t.path()).unwrap();
    store.set("TOKEN", b"authority").unwrap();
    let path = store.data_path();
    let valid = fs::read(&path).unwrap();

    let mut sparse = fs::OpenOptions::new().write(true).open(&path).unwrap();
    sparse
        .seek(std::io::SeekFrom::Start(MAX_SECRET_STORE_BYTES as u64))
        .unwrap();
    sparse.write_all(&[0]).unwrap();
    sparse.sync_all().unwrap();
    let oversized_len = sparse.metadata().unwrap().len();
    drop(sparse);

    for _ in 0..3 {
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::metadata(&path).unwrap().len(), oversized_len);
        assert!(SecretStore::open(t.path()).is_ok());
        assert_eq!(fs::metadata(&path).unwrap().len(), oversized_len);
    }

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);
    fs::remove_dir(&path).unwrap();
    atomic(&path, &valid).unwrap();
    assert_eq!(&*store.get("TOKEN").unwrap().unwrap(), b"authority");

    let source = include_str!("lib.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    assert!(!production.contains("fs::read("));
    assert!(!production.contains("read_to_string"));
}

#[test]
fn envelope_schema_encoding_and_shape_fail_closed_without_secret_echo() {
    let t = tempfile::tempdir().unwrap();
    let store = SecretStore::open(t.path()).unwrap();
    store.set("TOKEN", b"do-not-echo-this-secret").unwrap();
    let valid = fs::read(store.data_path()).unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&valid).unwrap();
    let nonce = envelope["nonce"].as_str().unwrap();
    let ciphertext = envelope["ciphertext"].as_str().unwrap();
    let corruptions = [
        format!(
            "{{\"version\":1,\"nonce\":{nonce:?},\"ciphertext\":{ciphertext:?},\"unknown\":true}}"
        ),
        format!(
            "{{\"version\":1,\"version\":1,\"nonce\":{nonce:?},\"ciphertext\":{ciphertext:?}}}"
        ),
        format!("{{\"version\":1,\"nonce\":\"{nonce}=\",\"ciphertext\":{ciphertext:?}}}"),
        "{\"unknown\":[[[[[0]]]]]}".to_string(),
        "{\"version\":1,\"nonce\":\"AAAAAAAAAAAAAAAA\",\"ciphertext\":\"AAAA\"}".to_string(),
    ];
    for corruption in corruptions {
        fs::write(store.data_path(), corruption.as_bytes()).unwrap();
        let error = store.list().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!error.to_string().contains("do-not-echo-this-secret"));
        assert_eq!(fs::read(store.data_path()).unwrap(), corruption.as_bytes());
    }

    let huge_ciphertext = format!(
        "{{\"version\":1,\"nonce\":\"AAAAAAAAAAAAAAAA\",\"ciphertext\":\"{}\"}}",
        "A".repeat(MAX_SECRET_CIPHERTEXT_B64_BYTES + 1)
    );
    assert!(huge_ciphertext.len() < MAX_SECRET_STORE_BYTES);
    fs::write(store.data_path(), huge_ciphertext.as_bytes()).unwrap();
    assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        fs::read(store.data_path()).unwrap(),
        huge_ciphertext.as_bytes()
    );

    fs::write(store.data_path(), valid).unwrap();
    let key_path = store.key_path();
    let original_key = fs::read(&key_path).unwrap();
    fs::write(&key_path, [0u8; 33]).unwrap();
    let error = store.list().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(&key_path).unwrap(), vec![0u8; 33]);
    fs::write(&key_path, &original_key).unwrap();

    let valid_data = fs::read(store.data_path()).unwrap();
    fs::remove_file(&key_path).unwrap();
    let error = SecretStore::open(t.path()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(!key_path.exists());
    assert_eq!(fs::read(store.data_path()).unwrap(), valid_data);
    atomic(&key_path, &original_key).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let data_path = store.data_path();
        let real_path = t.path().join("real-secrets.json");
        fs::rename(&data_path, &real_path).unwrap();
        symlink(&real_path, &data_path).unwrap();
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(
            fs::symlink_metadata(&data_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[test]
fn plaintext_schema_identity_and_value_limits_reject_the_whole_snapshot() {
    let t = tempfile::tempdir().unwrap();
    let store = SecretStore::open(t.path()).unwrap();

    let duplicate = write_plaintext(&store, br#"{"TOKEN":[1],"TOKEN":[2]}"#);
    assert_snapshot_failure_is_stable(&store, &duplicate);

    let long_name = serde_json::to_vec(&BTreeMap::from([(
        "N".repeat(MAX_SECRET_NAME_BYTES + 1),
        vec![1u8],
    )]))
    .unwrap();
    let bytes = write_plaintext(&store, &long_name);
    assert_snapshot_failure_is_stable(&store, &bytes);

    let long_value = serde_json::to_vec(&BTreeMap::from([(
        "TOKEN".to_string(),
        vec![7u8; MAX_SECRET_VALUE_BYTES + 1],
    )]))
    .unwrap();
    let bytes = write_plaintext(&store, &long_value);
    assert_snapshot_failure_once(&store, &bytes);

    let too_many: BTreeMap<_, _> = (0..=MAX_SECRETS)
        .map(|index| (format!("S{index:04}"), vec![1u8]))
        .collect();
    let bytes = write_plaintext(&store, &serde_json::to_vec(&too_many).unwrap());
    assert_snapshot_failure_is_stable(&store, &bytes);

    let aggregate: BTreeMap<_, _> = (0..9)
        .map(|index| {
            let len = if index < 8 { MAX_SECRET_VALUE_BYTES } else { 1 };
            (format!("A{index}"), vec![9u8; len])
        })
        .collect();
    let bytes = write_plaintext(&store, &serde_json::to_vec(&aggregate).unwrap());
    let error = store.list().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "invalid secret plaintext JSON");
    assert_eq!(fs::read(store.data_path()).unwrap(), bytes);

    let invalid_value_shape = write_plaintext(&store, br#"{"TOKEN":{"nested":[1]}}"#);
    assert_snapshot_failure_is_stable(&store, &invalid_value_shape);
}

#[test]
fn set_admission_is_transactional_and_replacement_works_at_capacity() {
    let t = tempfile::tempdir().unwrap();
    let store = SecretStore::open(t.path()).unwrap();
    store.set("SEED", b"seed").unwrap();
    let before = fs::read(store.data_path()).unwrap();
    for error in [
        store.set(&"N".repeat(MAX_SECRET_NAME_BYTES + 1), b"x"),
        store.set("TOO_BIG", &vec![0u8; MAX_SECRET_VALUE_BYTES + 1]),
    ] {
        assert_eq!(error.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(store.data_path()).unwrap(), before);
    }

    let full = PlainSecrets(
        (0..MAX_SECRETS)
            .map(|index| (format!("S{index:04}"), vec![index as u8]))
            .collect(),
    );
    store.save_unlocked(&full).unwrap();
    let full_bytes = fs::read(store.data_path()).unwrap();
    assert_eq!(
        store.set("OVERFLOW", b"x").unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(fs::read(store.data_path()).unwrap(), full_bytes);

    store.set("S0000", b"replacement").unwrap();
    assert_eq!(&*store.get("S0000").unwrap().unwrap(), b"replacement");
    assert_eq!(store.list().unwrap().len(), MAX_SECRETS);
    assert!(store.delete("S4095").unwrap());
    store.set("NEW_SLOT", b"new").unwrap();
    assert_eq!(store.list().unwrap().len(), MAX_SECRETS);

    let corrupt = write_plaintext(&store, br#"{"S0000":[999]}"#);
    assert_eq!(
        store
            .set("X", &vec![0u8; MAX_SECRET_VALUE_BYTES + 1])
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(fs::read(store.data_path()).unwrap(), corrupt);
}

#[test]
fn concurrent_readers_observe_only_complete_writer_snapshots() {
    let t = tempfile::tempdir().unwrap();
    let store = Arc::new(SecretStore::open(t.path()).unwrap());
    store.set("BASE", b"base").unwrap();
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                for _ in 0..32 {
                    assert_eq!(&*store.get("BASE").unwrap().unwrap(), b"base");
                    let names = store.list().unwrap();
                    assert!(names.iter().any(|name| name == "BASE"));
                }
            })
        })
        .collect();
    for index in 0..32 {
        store
            .set(
                &format!("WRITER_{index}"),
                format!("value-{index}").as_bytes(),
            )
            .unwrap();
    }
    for reader in readers {
        reader.join().unwrap();
    }
    assert_eq!(store.list().unwrap().len(), 33);
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
            "production secret synchronization must return typed errors: {forbidden}"
        );
    }
}

#[cfg(unix)]
#[test]
fn store_permissions_are_the_confidentiality_boundary() {
    use std::os::unix::fs::PermissionsExt;

    let t = tempfile::tempdir().unwrap();
    let s = SecretStore::open(t.path()).unwrap();
    s.set("TOKEN", b"secret").unwrap();
    assert_eq!(
        fs::metadata(t.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for name in ["master.key", "secrets.json", ".secrets.lock"] {
        assert_eq!(
            fs::metadata(t.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "{name} must be owner-only"
        );
    }
    // The encrypted file omits plaintext, but the same protected directory
    // also contains its decryption key: OS ownership/mode is the boundary.
    assert!(t.path().join("master.key").exists());
    assert!(t.path().join("secrets.json").exists());
}

#[test]
fn concurrent_process_writers_preserve_every_secret() {
    const WORKER_ENV: &str = "SHOAL_SECRET_LOCK_TEST_WORKER";
    const DIR_ENV: &str = "SHOAL_SECRET_LOCK_TEST_DIR";

    if let Some(name) = std::env::var_os(WORKER_ENV) {
        let dir = PathBuf::from(std::env::var_os(DIR_ENV).expect("worker store dir"));
        let store = SecretStore::open(dir).unwrap();
        let name = name.to_string_lossy();
        store.set(&name, name.as_bytes()).unwrap();
        return;
    }

    let t = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for i in 0..12 {
        let name = format!("SECRET_{i}");
        children.push(
            Command::new(&exe)
                .args([
                    "--exact",
                    "tests::concurrent_process_writers_preserve_every_secret",
                    "--nocapture",
                ])
                .env(WORKER_ENV, &name)
                .env(DIR_ENV, t.path())
                .spawn()
                .unwrap(),
        );
    }
    for child in children {
        assert!(child.wait_with_output().unwrap().status.success());
    }

    let store = SecretStore::open(t.path()).unwrap();
    let names = store.list().unwrap();
    assert_eq!(names.len(), 12, "a concurrent update was lost: {names:?}");
    for i in 0..12 {
        let name = format!("SECRET_{i}");
        assert_eq!(&*store.get(&name).unwrap().unwrap(), name.as_bytes());
    }
}
