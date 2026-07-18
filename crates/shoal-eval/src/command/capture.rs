//! Capture-spill adoption and lazy journal-CAS value loading.

use super::*;

impl Evaluator {
    /// Adopt a value-position capture's disk spill (site/content/internals/language-conformance-contract.md) into the journal
    /// CAS and hand back a lazy, ref-backed view of the full stdout. `preview`
    /// is the bounded resident prefix (shared with the outcome's `.stdout`).
    ///
    /// Returns `None` only if there is no journal or adoption fails on I/O; the
    /// orphaned spill file is cleaned up in that case. An active statement also
    /// records adoption failure for the journal boundary, which reports an
    /// indeterminate result rather than silently accepting the resident preview.
    /// On success the blob is durable under its real blake3 and pinned so GC
    /// keeps it while the value is live.
    pub(super) fn adopt_capture_spill(
        &mut self,
        spill: &shoal_exec::CaptureSpill,
        preview: Arc<Vec<u8>>,
    ) -> Option<Arc<shoal_value::CasBytesVal>> {
        self.session.journal.as_ref()?;
        let lease = match self
            .session
            .journal
            .as_ref()
            .expect("presence checked")
            .ingest_spill_leased(&spill.path, &spill.hash, spill.len)
        {
            Ok(lease) => lease,
            Err(error) => {
                self.note_journal_failure("capture spill adoption", error);
                let _ = self.host.fs.remove_file(&spill.path);
                return None;
            }
        };
        let loader = CasBytesLoader {
            cas: self
                .session
                .journal
                .as_ref()
                .expect("presence checked")
                .cas(),
            hash: spill.hash.clone(),
            _lease: Some(lease),
        };
        Some(Arc::new(shoal_value::CasBytesVal {
            hash: spill.hash.clone(),
            len: spill.len,
            preview,
            truncated: spill.truncated,
            loader: Arc::new(loader),
        }))
    }

    /// Resolve a `val:blake3:<hash>` content short-ref (the recoverable form
    /// [`shoal_value::CasBytesVal::reference`] / `.ref` yields) into a lazy
    /// [`Value::CasBytes`] backed by this session's journal CAS, so a bare ref
    /// *written as a value* dispatches methods and materializes exactly like the
    /// spill it came from (this is the in-language mirror of the wire
    /// `value.get` resolution).
    ///
    /// Returns `None` when `s` is not a content ref at all — the caller then
    /// dispatches the string through the ordinary string-method path unchanged.
    /// Returns `Some(Err(..))` — a clear `not_found` — when the ref is genuine
    /// but cannot be resolved: no journal/CAS is installed in this session, or
    /// no blob is tracked under that hash.
    pub(crate) fn resolve_content_ref(&self, s: &str, span: Span) -> Option<VResult<Value>> {
        let hash = shoal_value::CasBytesVal::parse_ref(s)?;
        Some(self.load_content_ref(hash).map_err(|e| e.with_span(span)))
    }

    /// The fallible core of [`Self::resolve_content_ref`]: builds the lazy
    /// [`Value::CasBytes`] for `hash`. `.len` is answered from the `blob` table
    /// metadata alone (never loading the content); a bare ref carries no resident
    /// preview, so `render` shows the ref + true length and materialization loads
    /// on demand through the same [`CasBytesLoader`]/[`shoal_journal::Cas`] seam a
    /// fresh spill uses.
    fn load_content_ref(&self, hash: &str) -> VResult<Value> {
        let prefix = shoal_value::CasBytesVal::REF_PREFIX;
        let Some(journal) = self.session.journal.as_ref() else {
            return Err(ErrorVal::new(
                "not_found",
                format!(
                    "cannot resolve content ref {prefix}{hash}: this session has no journal/CAS"
                ),
            ));
        };
        let len = journal
            .blob_len(hash)
            .map_err(|e| {
                ErrorVal::new(
                    "not_found",
                    format!("cannot resolve content ref {prefix}{hash}: {e}"),
                )
            })?
            .ok_or_else(|| {
                ErrorVal::new(
                    "not_found",
                    format!("no CAS blob for content ref {prefix}{hash}"),
                )
            })?;
        let loader = CasBytesLoader::new(journal.cas(), hash.to_string());
        Ok(Value::CasBytes(Arc::new(shoal_value::CasBytesVal {
            hash: hash.to_string(),
            len,
            preview: Arc::new(Vec::new()),
            truncated: false,
            loader: Arc::new(loader),
        })))
    }
}

/// Loads a spilled capture's full bytes from the journal CAS on demand.
/// Holds a DB-independent [`shoal_journal::Cas`] (just a path), so a ref-backed
/// [`shoal_value::CasBytesVal`] stays `Send + Sync` and outlives the borrow of
/// the evaluator that produced it.
///
/// Reused verbatim by [`crate::Evaluator::resolve_content_ref`] to back a bare
/// `val:blake3:<hash>` ref written as a value (see
/// `site/content/internals/persistence.md`) — same CAS seam as a fresh spill,
/// so a recovered ref materializes
/// exactly like the capture it came from.
pub(crate) struct CasBytesLoader {
    cas: shoal_journal::Cas,
    hash: String,
    _lease: Option<shoal_journal::PinLease>,
}

impl CasBytesLoader {
    pub(crate) fn new(cas: shoal_journal::Cas, hash: String) -> Self {
        Self {
            cas,
            hash,
            _lease: None,
        }
    }
}

impl shoal_value::BytesLoad for CasBytesLoader {
    fn load(&self) -> std::io::Result<Vec<u8>> {
        self.cas.read(&self.hash)
    }

    fn open(&self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        self.cas.open_verified(&self.hash)
    }
}
