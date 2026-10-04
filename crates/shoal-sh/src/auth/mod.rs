use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

/// Hard admission limits for the authority snapshot and every identity-bearing
/// field. Authority records are never evicted to satisfy these limits: an
/// oversized snapshot is an integrity failure and authenticates nobody.
pub const MAX_TOKEN_STORE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TOKENS: usize = 4_096;
pub const MAX_PRINCIPAL_BYTES: usize = 256;
pub const MAX_PROFILE_BYTES: usize = 128;
pub const MAX_CAPABILITIES_PER_TOKEN: usize = 128;
pub const MAX_CAPABILITY_BYTES: usize = 128;
pub const BEARER_BYTES: usize = 32;
pub const BEARER_ENCODED_BYTES: usize = 43;
const TOKEN_ID_HEX_BYTES: usize = 16;
const TOKEN_DIGEST_HEX_BYTES: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenMeta {
    pub id: String,
    pub principal: String,
    pub profile: String,
    pub caps: Vec<String>,
    pub created_ns: i64,
    pub expires_ns: Option<i64>,
    pub revoked_ns: Option<i64>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u32,
    key: String,
    tokens: Vec<StoredToken>,
}

impl Drop for Stored {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredToken {
    id: String,
    principal: String,
    profile: String,
    caps: Vec<String>,
    created_ns: i64,
    expires_ns: Option<i64>,
    revoked_ns: Option<i64>,
    digest: String,
}

impl StoredToken {
    fn from_meta(meta: TokenMeta, digest: String) -> Self {
        Self {
            id: meta.id,
            principal: meta.principal,
            profile: meta.profile,
            caps: meta.caps,
            created_ns: meta.created_ns,
            expires_ns: meta.expires_ns,
            revoked_ns: meta.revoked_ns,
            digest,
        }
    }

    fn meta(&self) -> TokenMeta {
        TokenMeta {
            id: self.id.clone(),
            principal: self.principal.clone(),
            profile: self.profile.clone(),
            caps: self.caps.clone(),
            created_ns: self.created_ns,
            expires_ns: self.expires_ns,
            revoked_ns: self.revoked_ns,
        }
    }

    fn into_meta(self) -> TokenMeta {
        TokenMeta {
            id: self.id,
            principal: self.principal,
            profile: self.profile,
            caps: self.caps,
            created_ns: self.created_ns,
            expires_ns: self.expires_ns,
            revoked_ns: self.revoked_ns,
        }
    }
}
pub struct TokenStore {
    path: PathBuf,
    key: Zeroizing<[u8; 32]>,
    tokens: Vec<StoredToken>,
}

impl TokenStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "token store needs a parent directory",
            )
        })?;
        fs::create_dir_all(parent)?;
        secure_dir(parent)?;

        // Initialization participates in the same transaction lock as writes:
        // simultaneous first-openers must share one keyed-hash key, not race to
        // replace the store with incompatible keys.
        let lock_target = path.clone();
        with_exclusive_lock(&lock_target, || {
            if path.exists() {
                secure_file(&path)?;
                let (key, tokens) = load_unlocked(&path)?;
                Ok(Self { path, key, tokens })
            } else {
                let mut key = Zeroizing::new([0; 32]);
                getrandom::fill(&mut *key).map_err(|e| io::Error::other(e.to_string()))?;
                let store = Self {
                    path,
                    key,
                    tokens: vec![],
                };
                store.persist_tokens_unlocked(&store.tokens, io::ErrorKind::InvalidData)?;
                Ok(store)
            }
        })
    }

    /// Create and persist a bearer while holding the whole read/modify/write
    /// transaction lock. The store is reloaded after lock acquisition so a
    /// token created by another process can never be overwritten by a stale
    /// in-memory snapshot.
    pub fn create(
        &mut self,
        principal: String,
        profile: String,
        mut caps: Vec<String>,
        ttl_ns: Option<i64>,
    ) -> io::Result<(String, TokenMeta)> {
        validate_text_input("principal", &principal, MAX_PRINCIPAL_BYTES)?;
        validate_text_input("profile", &profile, MAX_PROFILE_BYTES)?;
        validate_capabilities_input(&mut caps)?;
        let path = self.path.clone();
        with_exclusive_lock(&path, || {
            self.reload_unlocked()?;
            if self.tokens.len() >= MAX_TOKENS {
                return Err(invalid_input("token store capacity reached"));
            }
            let mut secret = Zeroizing::new([0; 32]);
            getrandom::fill(&mut *secret).map_err(|e| io::Error::other(e.to_string()))?;
            // The returned bearer is necessarily an ordinary String for API
            // compatibility; callers own and must dispose of that final copy.
            let bearer = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(*secret);
            let now = now_ns();
            let digest = self.digest(bearer.as_bytes());
            let meta = TokenMeta {
                id: hex(&digest[..8]),
                principal,
                profile,
                caps,
                created_ns: now,
                expires_ns: ttl_ns.map(|t| now.saturating_add(t)),
                revoked_ns: None,
            };
            let token = StoredToken::from_meta(meta.clone(), hex(&digest));
            let mut candidate = self.tokens.clone();
            candidate.push(token);
            validate_tokens(&mut candidate)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
            self.persist_tokens_unlocked(&candidate, io::ErrorKind::InvalidInput)?;
            self.tokens = candidate;
            Ok((bearer, meta))
        })
    }

    /// Validate against a freshly locked disk snapshot and surface storage or
    /// integrity failures to callers that need to distinguish them from an
    /// invalid bearer. No cached authority is consulted on failure.
    pub fn validate_checked(&self, bearer: &str) -> io::Result<Option<TokenMeta>> {
        if !bearer_is_canonical(bearer) {
            return Err(invalid_input("invalid bearer encoding"));
        }
        let (key, tokens) = with_shared_lock(&self.path, || load_unlocked(&self.path))?;
        let digest = digest_with(&key, bearer.as_bytes());
        let now = now_ns();
        Ok(tokens
            .iter()
            .find(|t| {
                // `load_unlocked` validates every digest before exposing the
                // snapshot, so conversion cannot fail here.
                unhex(&t.digest).is_ok_and(|stored| bool::from(stored.as_slice().ct_eq(&digest)))
            })
            .filter(|t| t.revoked_ns.is_none() && t.expires_ns.is_none_or(|e| e > now))
            .map(StoredToken::meta))
    }

    /// Compatibility authentication API. Storage, locking, and integrity
    /// errors deliberately collapse to unauthenticated; they never restore a
    /// token from the startup snapshot.
    pub fn validate(&self, bearer: &str) -> Option<TokenMeta> {
        self.validate_checked(bearer).ok().flatten()
    }

    /// Refresh the status of a token that was already authenticated with its
    /// bearer. This deliberately accepts the prior private [`TokenMeta`]
    /// record rather than a public token id alone, so it cannot become an
    /// alternate bearer-authentication path. The disk snapshot is fresh and
    /// storage failures, revocation, expiry, or identity replacement all fail
    /// closed.
    pub fn refresh_authenticated_checked(
        &self,
        attached: &TokenMeta,
    ) -> io::Result<Option<TokenMeta>> {
        validate_meta_input(attached)?;
        let (_, tokens) = with_shared_lock(&self.path, || load_unlocked(&self.path))?;
        let now = now_ns();
        Ok(tokens
            .into_iter()
            .find(|token| {
                token.id == attached.id
                    && token.created_ns == attached.created_ns
                    && token.principal == attached.principal
                    && token.profile == attached.profile
                    && token.caps == attached.caps
                    && token.expires_ns == attached.expires_ns
            })
            .filter(|token| {
                token.revoked_ns.is_none() && token.expires_ns.is_none_or(|expires| expires > now)
            })
            .map(StoredToken::into_meta))
    }

    /// Fail-closed compatibility wrapper around
    /// [`Self::refresh_authenticated_checked`].
    pub fn refresh_authenticated(&self, attached: &TokenMeta) -> Option<TokenMeta> {
        self.refresh_authenticated_checked(attached).ok().flatten()
    }

    /// Fallible, fresh list for callers that need storage errors surfaced.
    pub fn try_list(&self) -> io::Result<Vec<TokenMeta>> {
        with_shared_lock(&self.path, || {
            let (_, tokens) = load_unlocked(&self.path)?;
            Ok(tokens.into_iter().map(StoredToken::into_meta).collect())
        })
    }

    /// Compatibility list. New code should prefer [`Self::try_list`] so an
    /// on-disk integrity/I/O error is not hidden. On failure this returns an
    /// empty list rather than stale startup state.
    pub fn list(&self) -> Vec<TokenMeta> {
        self.try_list().unwrap_or_default()
    }

    pub fn revoke(&mut self, id: &str) -> io::Result<bool> {
        if !is_canonical_hex(id, TOKEN_ID_HEX_BYTES) {
            return Err(invalid_input("invalid token id encoding"));
        }
        let path = self.path.clone();
        with_exclusive_lock(&path, || {
            self.reload_unlocked()?;
            let Some(index) = self.tokens.iter().position(|token| token.id == id) else {
                return Ok(false);
            };
            let mut candidate = self.tokens.clone();
            candidate[index].revoked_ns = Some(now_ns());
            self.persist_tokens_unlocked(&candidate, io::ErrorKind::InvalidData)?;
            self.tokens = candidate;
            Ok(true)
        })
    }

    fn reload_unlocked(&mut self) -> io::Result<()> {
        let (key, tokens) = load_unlocked(&self.path)?;
        self.key = key;
        self.tokens = tokens;
        Ok(())
    }

    fn digest(&self, secret: &[u8]) -> [u8; 32] {
        digest_with(&self.key, secret)
    }

    fn persist_tokens_unlocked(
        &self,
        tokens: &[StoredToken],
        overflow_kind: io::ErrorKind,
    ) -> io::Result<()> {
        let mut doc = Stored {
            version: 1,
            key: base64::engine::general_purpose::STANDARD.encode(*self.key),
            tokens: tokens.to_vec(),
        };
        let bytes = Zeroizing::new(serde_json::to_vec_pretty(&doc).map_err(io::Error::other)?);
        // Wipe the additional base64 key copy as soon as serialization is done.
        doc.key.zeroize();
        if bytes.len() > MAX_TOKEN_STORE_BYTES {
            return Err(io::Error::new(
                overflow_kind,
                "token store exceeds byte limit",
            ));
        }
        atomic_replace(&self.path, &bytes)
    }
}

fn load_unlocked(path: &Path) -> io::Result<(Zeroizing<[u8; 32]>, Vec<StoredToken>)> {
    secure_file(path)?;
    let mut file = fs::File::open(path)?;
    if file.metadata()?.len() > MAX_TOKEN_STORE_BYTES as u64 {
        return Err(invalid_data("token store exceeds byte limit"));
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(8 * 1024));
    Read::by_ref(&mut file)
        .take(MAX_TOKEN_STORE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_TOKEN_STORE_BYTES {
        return Err(invalid_data("token store exceeds byte limit"));
    }
    let mut doc: Stored =
        serde_json::from_slice(&bytes).map_err(|_| invalid_data("invalid token store JSON"))?;
    if doc.version != 1 {
        return Err(invalid_data("unsupported token store version"));
    }
    if doc.tokens.len() > MAX_TOKENS {
        return Err(invalid_data("token store exceeds token limit"));
    }
    let raw = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(&doc.key)
            .map_err(|_| invalid_data("invalid token store key encoding"))?,
    );
    let key = Zeroizing::new(
        raw.as_slice()
            .try_into()
            .map_err(|_| invalid_data("invalid token store key length"))?,
    );
    if base64::engine::general_purpose::STANDARD.encode(*key) != doc.key {
        return Err(invalid_data("noncanonical token store key encoding"));
    }
    let mut tokens = std::mem::take(&mut doc.tokens);
    validate_tokens(&mut tokens)?;
    Ok((key, tokens))
}

fn validate_tokens(tokens: &mut [StoredToken]) -> io::Result<()> {
    let mut ids = BTreeSet::new();
    let mut digests = BTreeSet::new();
    for token in tokens {
        validate_stored_text("principal", &token.principal, MAX_PRINCIPAL_BYTES)?;
        validate_stored_text("profile", &token.profile, MAX_PROFILE_BYTES)?;
        validate_stored_capabilities(&mut token.caps)?;
        if !is_canonical_hex(&token.id, TOKEN_ID_HEX_BYTES) {
            return Err(invalid_data("invalid token id encoding"));
        }
        if !is_canonical_hex(&token.digest, TOKEN_DIGEST_HEX_BYTES) {
            return Err(invalid_data("invalid token digest encoding"));
        }
        let digest =
            unhex(&token.digest).map_err(|()| invalid_data("invalid token digest encoding"))?;
        if digest.len() != 32 {
            return Err(invalid_data("invalid token digest length"));
        }
        if token.id != hex(&digest[..8]) {
            return Err(invalid_data("token id does not match digest"));
        }
        if !ids.insert(&token.id) || !digests.insert(&token.digest) {
            return Err(invalid_data("duplicate token identity"));
        }
    }
    Ok(())
}

/// Return whether a bearer has the exact canonical representation emitted by
/// [`TokenStore::create`]. Callers may use this cheap check before locking or
/// reading the authority snapshot.
pub fn bearer_is_canonical(bearer: &str) -> bool {
    if bearer.len() != BEARER_ENCODED_BYTES || !bearer.is_ascii() {
        return false;
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(bearer)
        .is_ok_and(|raw| {
            raw.len() == BEARER_BYTES
                && base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw) == bearer
        })
}

fn validate_meta_input(meta: &TokenMeta) -> io::Result<()> {
    if !is_canonical_hex(&meta.id, TOKEN_ID_HEX_BYTES) {
        return Err(invalid_input("invalid token id encoding"));
    }
    validate_text_input("principal", &meta.principal, MAX_PRINCIPAL_BYTES)?;
    validate_text_input("profile", &meta.profile, MAX_PROFILE_BYTES)?;
    let mut caps = meta.caps.clone();
    validate_capabilities_input(&mut caps)?;
    if caps != meta.caps {
        return Err(invalid_input("noncanonical token capabilities"));
    }
    Ok(())
}

fn validate_capabilities_input(caps: &mut [String]) -> io::Result<()> {
    if caps.len() > MAX_CAPABILITIES_PER_TOKEN {
        return Err(invalid_input("too many token capabilities"));
    }
    for cap in caps.iter() {
        validate_text_input("capability", cap, MAX_CAPABILITY_BYTES)?;
    }
    caps.sort();
    if caps.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid_input("duplicate token capability"));
    }
    Ok(())
}

fn validate_stored_capabilities(caps: &mut [String]) -> io::Result<()> {
    if caps.len() > MAX_CAPABILITIES_PER_TOKEN {
        return Err(invalid_data("too many token capabilities"));
    }
    for cap in caps.iter() {
        validate_stored_text("capability", cap, MAX_CAPABILITY_BYTES)?;
    }
    caps.sort();
    if caps.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid_data("duplicate token capability"));
    }
    Ok(())
}

fn validate_text_input(field: &str, value: &str, max_bytes: usize) -> io::Result<()> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(invalid_input(match field {
            "principal" => "invalid token principal",
            "profile" => "invalid token profile",
            _ => "invalid token capability",
        }));
    }
    Ok(())
}

fn validate_stored_text(field: &str, value: &str, max_bytes: usize) -> io::Result<()> {
    validate_text_input(field, value, max_bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

fn is_canonical_hex(value: &str, expected_bytes: usize) -> bool {
    value.len() == expected_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn digest_with(key: &[u8; 32], secret: &[u8]) -> [u8; 32] {
    *blake3::keyed_hash(key, secret).as_bytes()
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    let p = lock_path(path);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&p)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn with_exclusive_lock<T>(path: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let file = open_lock_file(path)?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.write()?;
    f()
}

fn with_shared_lock<T>(path: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let file = open_lock_file(path)?;
    let lock = fd_lock::RwLock::new(file);
    let _guard = lock.read()?;
    f()
}

fn secure_dir(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn secure_file(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "token store needs a parent directory",
        )
    })?;
    let tmp = tempfile_path(path);
    // The exclusive store lock permits one active writer, but a crashed writer
    // can leave this process-specific file behind.
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn tempfile_path(path: &Path) -> PathBuf {
    path.with_extension(format!("tmp.{}", std::process::id()))
}
fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Result<Vec<u8>, ()> {
    fn nibble(byte: u8) -> Result<u8, ()> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            _ => Err(()),
        }
    }

    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(());
    }
    bytes
        .chunks_exact(2)
        .map(|pair| Ok((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests;
