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
mod tests;
