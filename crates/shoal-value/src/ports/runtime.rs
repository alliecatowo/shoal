use crate::{Record, Value};

// ---------------------------------------------------------------------------

/// Wall-clock time, isolated so journal timestamps are deterministic under test.
pub trait Clock: Send + Sync {
    /// Nanoseconds since the Unix epoch, clamped to `i64::MAX`, matching the
    /// journal's original `SystemTime::now().duration_since(UNIX_EPOCH)` call.
    fn now_ns(&self) -> i64;
}

/// The default [`Clock`]: the system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdClock;

impl Clock for StdClock {
    fn now_ns(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(i64::MAX as u128) as i64
    }
}

// ---------------------------------------------------------------------------
// SecretPort — secret store read port
// ---------------------------------------------------------------------------

/// Read access to the secret store backing `secret.get(name)`. The trait lives
/// here; the concrete adapter (over `shoal-secret`) lives in `shoal-eval` so
/// `shoal-value` keeps no dependency on the secret crate.
pub trait SecretPort: Send + Sync {
    /// Fetch a secret's raw bytes by name. `Ok(None)` means "no such secret";
    /// `Err(msg)` is a store-open/permission failure (the caller maps it to a
    /// `permission` error).
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String>;
}

// ---------------------------------------------------------------------------
// BytesLoad — content-addressed bytes loader port (site/content/internals/language-conformance-contract.md)
// ---------------------------------------------------------------------------

/// Loads the full content behind a lazy, CAS-backed [`crate::Value::CasBytes`]
/// (site/content/internals/language-conformance-contract.md disk-spill). A value produced when a command's captured output
/// overflowed the RAM cap holds one of these plus a bounded preview; methods
/// that explicitly request resident bytes call [`load`] on demand. Incremental
/// consumers (`.stream()`, `.save()`, `.append()`, stream feed) call [`BytesLoad::open`],
/// while `.len` and `render` stay cheap and never load.
///
/// The trait lives here so `shoal-value` keeps no dependency on `shoal-journal`;
/// the concrete adapter (over `shoal_journal::Cas`) lives in `shoal-eval`. It is
/// `Send + Sync` so a ref-backed value is as freely shareable as any other.
///
/// [`load`]: BytesLoad::load
pub trait BytesLoad: Send + Sync {
    /// Materialize the full content. Errors are I/O or integrity failures
    /// (a missing/corrupt CAS blob); the caller maps them to an `io_error`.
    fn load(&self) -> std::io::Result<Vec<u8>>;

    /// Open a bounded-memory reader over the content. The default fails closed:
    /// silently implementing an incremental consumer by calling [`load`](Self::load)
    /// would reintroduce whole-blob materialization at `.feed`, stream, save,
    /// and HTTP boundaries. Blob-store adapters must provide a real reader.
    fn open(&self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this CAS adapter does not provide incremental reads",
        ))
    }
}

#[cfg(test)]
mod bytes_load_tests {
    use super::BytesLoad;

    struct MaterializingOnly;

    impl BytesLoad for MaterializingOnly {
        fn load(&self) -> std::io::Result<Vec<u8>> {
            Ok(vec![1, 2, 3])
        }
    }

    #[test]
    fn materializing_adapter_cannot_masquerade_as_incremental() {
        let error = match MaterializingOnly.open() {
            Ok(_) => panic!("the default reader must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }
}

// ---------------------------------------------------------------------------
// ConfigPort — resolved-config snapshot read port
// ---------------------------------------------------------------------------

/// Read access to the resolved, host-applied configuration backing the
/// in-language `config` namespace (`config.get(key)`, `config.all`). The
/// evaluator holds a `dyn ConfigPort` and reads the snapshot from it instead
/// of walking the filesystem to re-parse `shoal.toml` on its own — which would
/// bypass the host's layering/env-override/validation (all of which live in
/// `shoal-config`, a crate `shoal-value`/`shoal-eval` deliberately do not
/// depend on). The host injects a [`ConfigSnapshot`] built from the *same*
/// resolved `Config` it applies to itself, so in-language `config.get` and the
/// host-applied config can never disagree.
///
/// The default adapter is the **empty** [`ConfigSnapshot`] (`Default`): a
/// kernel-less/`-c`/test evaluator that never had a config injected reports an
/// empty record, so `config.get(key)` degrades to `null` — never a filesystem
/// walk. This mirrors how the other ports degrade to their inert default.
pub trait ConfigPort: Send + Sync {
    /// The whole resolved config as a record [`Value`] (`config.all`);
    /// `config.get(key)` reads one top-level key out of it. An adapter with no
    /// injected config returns an empty record.
    fn snapshot(&self) -> &Value;
}

/// The default [`ConfigPort`] adapter: a plain resolved-config snapshot. Holds
/// the config as a record [`Value`] (what `config.all` returns); the host
/// builds one from `shoal_config::load`'s resolved `Config` and injects it via
/// `Evaluator::set_config`. [`ConfigSnapshot::default`] (an empty record) is
/// the no-config, zero-regression default the evaluator starts with.
#[derive(Debug, Clone)]
pub struct ConfigSnapshot {
    value: Value,
}

impl ConfigSnapshot {
    /// Wrap an already-resolved config record. `value` is normally a
    /// [`Value::Record`] (the serialized `Config`); anything else makes every
    /// `config.get(key)` resolve to `null`, exactly like an empty snapshot.
    pub fn new(value: Value) -> Self {
        Self { value }
    }

    /// The empty snapshot: an empty record. `config.get(key)` on it is always
    /// `null`, and `config.all` is `{}`.
    pub fn empty() -> Self {
        Self {
            value: Value::Record(Record::new()),
        }
    }
}

impl Default for ConfigSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

impl ConfigPort for ConfigSnapshot {
    fn snapshot(&self) -> &Value {
        &self.value
    }
}
