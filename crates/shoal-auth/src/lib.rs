use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
struct Stored {
    version: u32,
    key: String,
    tokens: Vec<StoredToken>,
}
#[derive(Serialize, Deserialize)]
struct StoredToken {
    #[serde(flatten)]
    meta: TokenMeta,
    digest: String,
}
pub struct TokenStore {
    path: PathBuf,
    key: [u8; 32],
    tokens: Vec<StoredToken>,
}

impl TokenStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        if path.exists() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            let bytes = fs::read(&path)?;
            let doc: Stored = serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let raw = base64::engine::general_purpose::STANDARD
                .decode(doc.key)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let key: [u8; 32] = raw
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid token key"))?;
            Ok(Self {
                path,
                key,
                tokens: doc.tokens,
            })
        } else {
            let mut key = [0; 32];
            getrandom::fill(&mut key).map_err(|e| io::Error::other(e.to_string()))?;
            let store = Self {
                path,
                key,
                tokens: vec![],
            };
            store.persist()?;
            Ok(store)
        }
    }
    pub fn create(
        &mut self,
        principal: String,
        profile: String,
        caps: Vec<String>,
        ttl_ns: Option<i64>,
    ) -> io::Result<(String, TokenMeta)> {
        let mut secret = [0; 32];
        getrandom::fill(&mut secret).map_err(|e| io::Error::other(e.to_string()))?;
        let bearer = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
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
        self.tokens.push(StoredToken {
            meta: meta.clone(),
            digest: hex(&digest),
        });
        self.persist()?;
        Ok((bearer, meta))
    }
    pub fn validate(&self, bearer: &str) -> Option<TokenMeta> {
        let digest = self.digest(bearer.as_bytes());
        let now = now_ns();
        self.tokens
            .iter()
            .find(|t| {
                let Ok(stored) = unhex(&t.digest) else {
                    return false;
                };
                stored.len() == 32 && bool::from(stored.as_slice().ct_eq(&digest))
            })
            .filter(|t| t.meta.revoked_ns.is_none() && t.meta.expires_ns.is_none_or(|e| e > now))
            .map(|t| t.meta.clone())
    }
    pub fn list(&self) -> Vec<TokenMeta> {
        self.tokens.iter().map(|t| t.meta.clone()).collect()
    }
    pub fn revoke(&mut self, id: &str) -> io::Result<bool> {
        let Some(t) = self.tokens.iter_mut().find(|t| t.meta.id == id) else {
            return Ok(false);
        };
        t.meta.revoked_ns = Some(now_ns());
        self.persist()?;
        Ok(true)
    }
    fn digest(&self, secret: &[u8]) -> [u8; 32] {
        *blake3::keyed_hash(&self.key, secret).as_bytes()
    }
    fn persist(&self) -> io::Result<()> {
        if let Some(p) = self.path.parent() {
            fs::create_dir_all(p)?;
        }
        let doc = Stored {
            version: 1,
            key: base64::engine::general_purpose::STANDARD.encode(self.key),
            tokens: self
                .tokens
                .iter()
                .map(|t| StoredToken {
                    meta: t.meta.clone(),
                    digest: t.digest.clone(),
                })
                .collect(),
        };
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(&doc).map_err(io::Error::other)?)?;
        file.sync_all()?;
        drop(file);
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
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
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
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
}
