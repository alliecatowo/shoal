use aes_gcm::{Aes256Gcm, KeyInit, aead::Aead};
use base64::Engine as _;
use rand::{TryRng, rngs::SysRng};
use serde::{Deserialize, Deserializer, Serialize, de::MapAccess, de::SeqAccess, de::Visitor};
use std::{
    collections::BTreeMap,
    fs,
    fs::OpenOptions,
    io::{self, Read},
    path::{Path, PathBuf},
};
use zeroize::{Zeroize, Zeroizing};

/// Hard admission limits for the encrypted store and decrypted identity map.
/// A snapshot that exceeds any limit is an integrity error: it is never
/// truncated, partially accepted, or automatically replaced.
pub const MAX_SECRET_STORE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SECRET_PLAINTEXT_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_SECRET_CIPHERTEXT_BYTES: usize = MAX_SECRET_PLAINTEXT_BYTES + 16;
pub const MAX_SECRETS: usize = 4_096;
pub const MAX_SECRET_NAME_BYTES: usize = 128;
pub const MAX_SECRET_VALUE_BYTES: usize = 256 * 1024;
pub const MAX_SECRET_AGGREGATE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SECRET_CIPHERTEXT_B64_BYTES: usize = MAX_SECRET_CIPHERTEXT_BYTES.div_ceil(3) * 4;
const MAX_ENVELOPE_JSON_DEPTH: usize = 4;
const MAX_ENVELOPE_JSON_NODES: usize = 16;
const MAX_PLAINTEXT_JSON_DEPTH: usize = 4;
const MAX_PLAINTEXT_JSON_NODES: usize = MAX_SECRET_AGGREGATE_BYTES + MAX_SECRETS * 3 + 16;

/// Encrypted secret storage whose confidentiality boundary is the containing
/// directory's OS permissions. `master.key` and `secrets.json` are deliberately
/// colocated, so copying that directory copies both key and ciphertext; this is
/// encrypted-at-rest hygiene, not protection from an actor that can read the
/// store directory.
#[derive(Clone)]
pub struct SecretStore {
    dir: PathBuf,
}
impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    nonce: String,
    ciphertext: String,
}

/// Plaintext map with deterministic zeroization of every value on all exits,
/// including parse/save errors and replacement/deletion paths.
#[derive(Default, Serialize)]
#[serde(transparent)]
struct PlainSecrets(BTreeMap<String, Vec<u8>>);

struct SecretBytes(Zeroizing<Vec<u8>>);

impl<'de> Deserialize<'de> for SecretBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SecretBytesVisitor;

        impl<'de> Visitor<'de> for SecretBytesVisitor {
            type Value = SecretBytes;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a bounded JSON byte array")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let capacity = sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_SECRET_VALUE_BYTES);
                let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
                while let Some(byte) = sequence.next_element::<u8>()? {
                    if bytes.len() >= MAX_SECRET_VALUE_BYTES {
                        return Err(serde::de::Error::custom(
                            "secret store value exceeds byte limit",
                        ));
                    }
                    bytes.push(byte);
                }
                Ok(SecretBytes(bytes))
            }
        }

        deserializer.deserialize_seq(SecretBytesVisitor)
    }
}

impl<'de> Deserialize<'de> for PlainSecrets {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PlainSecretsVisitor;

        impl<'de> Visitor<'de> for PlainSecretsVisitor {
            type Value = PlainSecrets;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a bounded map of secret names to byte arrays")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut secrets = PlainSecrets::default();
                let mut aggregate = 0usize;
                while let Some(name) = map.next_key::<String>()? {
                    validate_stored_name(&name).map_err(serde::de::Error::custom)?;
                    if secrets.0.len() >= MAX_SECRETS {
                        return Err(serde::de::Error::custom(
                            "secret store exceeds identity limit",
                        ));
                    }
                    if secrets.0.contains_key(&name) {
                        return Err(serde::de::Error::custom("duplicate secret store identity"));
                    }
                    let mut value = map.next_value::<SecretBytes>()?;
                    validate_stored_value(&value.0).map_err(serde::de::Error::custom)?;
                    aggregate = aggregate.checked_add(value.0.len()).ok_or_else(|| {
                        serde::de::Error::custom("secret store aggregate overflow")
                    })?;
                    if aggregate > MAX_SECRET_AGGREGATE_BYTES {
                        return Err(serde::de::Error::custom(
                            "secret store exceeds aggregate value limit",
                        ));
                    }
                    secrets.0.insert(name, std::mem::take(&mut *value.0));
                }
                Ok(secrets)
            }
        }

        deserializer.deserialize_map(PlainSecretsVisitor)
    }
}

impl Drop for PlainSecrets {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}

impl SecretStore {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let s = Self { dir: dir.into() };
        fs::create_dir_all(&s.dir)?;
        secure_dir(&s.dir)?;
        // Key creation must share the same interprocess transaction lock as
        // map updates: two first-openers must never install different keys.
        s.with_exclusive_lock(|| {
            if !path_present(&s.key_path())? {
                if path_present(&s.data_path())? {
                    return Err(invalid_data(
                        "secret data exists but the master key is missing",
                    ));
                }
                let mut k = Zeroizing::new([0u8; 32]);
                SysRng.try_fill_bytes(&mut *k).map_err(invalid)?;
                atomic(&s.key_path(), &*k)?
            }
            drop(s.key()?);
            Ok(())
        })?;
        Ok(s)
    }
    pub fn set(&self, name: &str, value: &[u8]) -> io::Result<()> {
        validate_name_input(name)?;
        validate_value_input(value)?;
        self.with_exclusive_lock(|| {
            let mut m = self.load_unlocked()?;
            if !m.0.contains_key(name) && m.0.len() >= MAX_SECRETS {
                return Err(invalid_input("secret store capacity reached"));
            }
            let retained =
                m.0.values()
                    .map(Vec::len)
                    .sum::<usize>()
                    .saturating_sub(m.0.get(name).map_or(0, Vec::len));
            if retained
                .checked_add(value.len())
                .is_none_or(|total| total > MAX_SECRET_AGGREGATE_BYTES)
            {
                return Err(invalid_input("secret store aggregate value limit reached"));
            }
            if let Some(old) = m.0.insert(name.into(), value.to_vec()) {
                drop(Zeroizing::new(old));
            }
            self.save_unlocked(&m)
        })
    }
    /// Checked read: malformed, oversized, or unreadable persisted state is an
    /// error, never a missing-secret answer or a cached/stale fallback.
    pub fn get(&self, name: &str) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
        validate_name_input(name)?;
        self.with_shared_lock(|| Ok(self.load_unlocked()?.0.remove(name).map(Zeroizing::new)))
    }
    /// Checked list with bounded output cardinality. Persisted-state failures
    /// are surfaced rather than collapsed to an empty compatibility result.
    pub fn list(&self) -> io::Result<Vec<String>> {
        self.with_shared_lock(|| Ok(self.load_unlocked()?.0.keys().cloned().collect()))
    }
    /// Checked transactional delete. A corrupt snapshot is preserved and
    /// returned as an error; a valid snapshot without `name` returns `false`.
    pub fn delete(&self, name: &str) -> io::Result<bool> {
        validate_name_input(name)?;
        self.with_exclusive_lock(|| {
            let mut m = self.load_unlocked()?;
            let found = m.0.remove(name).map(Zeroizing::new);
            if found.is_some() {
                self.save_unlocked(&m)?
            }
            Ok(found.is_some())
        })
    }
    fn key_path(&self) -> PathBuf {
        self.dir.join("master.key")
    }
    fn data_path(&self) -> PathBuf {
        self.dir.join("secrets.json")
    }
    fn lock_path(&self) -> PathBuf {
        self.dir.join(".secrets.lock")
    }
    fn key(&self) -> io::Result<Zeroizing<Vec<u8>>> {
        let k = read_bounded_regular(&self.key_path(), 32)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "secret master key is missing")
        })?;
        if k.len() != 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid secret key length",
            ));
        }
        Ok(k)
    }
    fn load_unlocked(&self) -> io::Result<PlainSecrets> {
        let Some(envelope_bytes) = read_bounded_regular(&self.data_path(), MAX_SECRET_STORE_BYTES)?
        else {
            return Ok(PlainSecrets::default());
        };
        validate_json_shape(
            &envelope_bytes,
            MAX_ENVELOPE_JSON_DEPTH,
            MAX_ENVELOPE_JSON_NODES,
            "secret envelope",
        )?;
        let e: Envelope = serde_json::from_slice(&envelope_bytes)
            .map_err(|_| invalid_data("invalid secret envelope JSON"))?;
        if e.version != 1 {
            return Err(invalid_data("unsupported secret store version"));
        }
        if e.nonce.len() != 16 {
            return Err(invalid_data("invalid secret store nonce encoding"));
        }
        if e.ciphertext.len() > MAX_SECRET_CIPHERTEXT_B64_BYTES {
            return Err(invalid_data("secret store ciphertext exceeds byte limit"));
        }
        let nonce_bytes = base64::engine::general_purpose::STANDARD
            .decode(&e.nonce)
            .map_err(|_| invalid_data("invalid secret store nonce encoding"))?;
        let nonce: [u8; 12] = nonce_bytes
            .as_slice()
            .try_into()
            .map_err(|_| invalid_data("invalid secret store nonce length"))?;
        if base64::engine::general_purpose::STANDARD.encode(nonce) != e.nonce {
            return Err(invalid_data("noncanonical secret store nonce encoding"));
        }
        let ct = Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .decode(&e.ciphertext)
                .map_err(|_| invalid_data("invalid secret store ciphertext encoding"))?,
        );
        if ct.len() > MAX_SECRET_CIPHERTEXT_BYTES {
            return Err(invalid_data("secret store ciphertext exceeds byte limit"));
        }
        if base64::engine::general_purpose::STANDARD.encode(ct.as_slice()) != e.ciphertext {
            return Err(invalid_data(
                "noncanonical secret store ciphertext encoding",
            ));
        }
        let key = self.key()?;
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(invalid)?;
        let plain = Zeroizing::new(
            cipher
                .decrypt((&nonce).into(), ct.as_ref())
                .map_err(|_| invalid_data("secret store authentication failed"))?,
        );
        if plain.len() > MAX_SECRET_PLAINTEXT_BYTES {
            return Err(invalid_data("secret store plaintext exceeds byte limit"));
        }
        validate_json_shape(
            &plain,
            MAX_PLAINTEXT_JSON_DEPTH,
            MAX_PLAINTEXT_JSON_NODES,
            "secret plaintext",
        )?;
        serde_json::from_slice(&plain).map_err(|_| invalid_data("invalid secret plaintext JSON"))
    }
    fn save_unlocked(&self, m: &PlainSecrets) -> io::Result<()> {
        validate_plain_secrets(m, io::ErrorKind::InvalidInput)?;
        let plain = Zeroizing::new(serde_json::to_vec(m).map_err(invalid)?);
        if plain.len() > MAX_SECRET_PLAINTEXT_BYTES {
            return Err(invalid_input("secret store plaintext exceeds byte limit"));
        }
        let key = self.key()?;
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(invalid)?;
        let mut nonce = [0u8; 12];
        SysRng.try_fill_bytes(&mut nonce).map_err(invalid)?;
        let ct = Zeroizing::new(
            cipher
                .encrypt((&nonce).into(), plain.as_ref())
                .map_err(invalid)?,
        );
        if ct.len() > MAX_SECRET_CIPHERTEXT_BYTES {
            return Err(invalid_input("secret store ciphertext exceeds byte limit"));
        }
        let e = Envelope {
            version: 1,
            nonce: base64::engine::general_purpose::STANDARD.encode(nonce),
            ciphertext: base64::engine::general_purpose::STANDARD.encode(ct.as_slice()),
        };
        let envelope = serde_json::to_vec(&e).map_err(invalid)?;
        if envelope.len() > MAX_SECRET_STORE_BYTES {
            return Err(invalid_input("secret store exceeds byte limit"));
        }
        atomic(&self.data_path(), &envelope)
    }

    fn with_exclusive_lock<T>(&self, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let file = open_lock_file(&self.lock_path())?;
        let mut lock = fd_lock::RwLock::new(file);
        let _guard = lock.write()?;
        f()
    }

    fn with_shared_lock<T>(&self, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let file = open_lock_file(&self.lock_path())?;
        let lock = fd_lock::RwLock::new(file);
        let _guard = lock.read()?;
        f()
    }
}
fn validate_name_input(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > MAX_SECRET_NAME_BYTES
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "secret name must be 1..=128 bytes of ASCII letters, digits, _ or -",
        ))
    } else {
        Ok(())
    }
}

fn validate_value_input(value: &[u8]) -> io::Result<()> {
    if value.len() > MAX_SECRET_VALUE_BYTES {
        return Err(invalid_input("secret value exceeds byte limit"));
    }
    Ok(())
}

fn validate_stored_name(name: &str) -> io::Result<()> {
    validate_name_input(name).map_err(|_| invalid_data("invalid secret store identity"))
}

fn validate_stored_value(value: &[u8]) -> io::Result<()> {
    validate_value_input(value).map_err(|_| invalid_data("secret store value exceeds byte limit"))
}

fn validate_plain_secrets(m: &PlainSecrets, kind: io::ErrorKind) -> io::Result<()> {
    if m.0.len() > MAX_SECRETS {
        return Err(io::Error::new(kind, "secret store exceeds identity limit"));
    }
    let mut aggregate = 0usize;
    for (name, value) in &m.0 {
        validate_stored_name(name).map_err(|error| io::Error::new(kind, error.to_string()))?;
        validate_stored_value(value).map_err(|error| io::Error::new(kind, error.to_string()))?;
        aggregate = aggregate
            .checked_add(value.len())
            .ok_or_else(|| io::Error::new(kind, "secret store aggregate overflow"))?;
        if aggregate > MAX_SECRET_AGGREGATE_BYTES {
            return Err(io::Error::new(
                kind,
                "secret store exceeds aggregate value limit",
            ));
        }
    }
    Ok(())
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn read_bounded_regular(path: &Path, max_bytes: usize) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(invalid_data("secret store input is not a regular file"));
    }
    check_mode(path)?;
    if metadata.len() > max_bytes as u64 {
        return Err(invalid_data("secret store input exceeds byte limit"));
    }
    let mut file = fs::File::open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() {
        return Err(invalid_data("secret store input is not a regular file"));
    }
    check_open_file_mode(&opened_metadata)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(max_bytes.min(8 * 1024)));
    Read::by_ref(&mut file)
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(invalid_data("secret store input exceeds byte limit"));
    }
    Ok(Some(bytes))
}

fn path_present(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn validate_json_shape(
    bytes: &[u8],
    max_depth: usize,
    max_nodes: usize,
    label: &'static str,
) -> io::Result<()> {
    let mut depth = 0usize;
    let mut nodes = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                nodes += 1;
                if depth > max_depth {
                    return Err(invalid_data(match label {
                        "secret envelope" => "secret envelope exceeds JSON depth limit",
                        _ => "secret plaintext exceeds JSON depth limit",
                    }));
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            b',' => nodes += 1,
            _ => {}
        }
        if nodes > max_nodes {
            return Err(invalid_data(match label {
                "secret envelope" => "secret envelope exceeds JSON node limit",
                _ => "secret plaintext exceeds JSON node limit",
            }));
        }
    }
    Ok(())
}
fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}
#[cfg(unix)]
fn secure_dir(p: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o700))
}
#[cfg(not(unix))]
fn secure_dir(_: &Path) -> io::Result<()> {
    Ok(())
}
#[cfg(unix)]
fn check_mode(p: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if fs::metadata(p)?.permissions().mode() & 0o077 != 0 {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "secret file permissions must be 0600",
        ))
    } else {
        Ok(())
    }
}
#[cfg(not(unix))]
fn check_mode(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn check_open_file_mode(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "secret file permissions must be 0600",
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn check_open_file_mode(_: &fs::Metadata) -> io::Result<()> {
    Ok(())
}

fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(invalid_data("secret store lock is not a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid_data("secret store lock is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "secret store file needs a parent directory",
        )
    })?;
    let mut t = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        t.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?
    }
    io::Write::write_all(&mut t, data)?;
    t.as_file().sync_all()?;
    t.persist(path).map_err(|e| e.error)?;
    // Persist the directory entry as well as the temporary file contents.
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
#[cfg(test)]
mod tests {
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
}
