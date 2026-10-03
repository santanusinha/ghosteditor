//! Optional search index seam (plan 4.6). The walker owns text search in
//! v1; this trait is the extension point where a real index (Turso/libSQL,
//! FTS5) plugs in later without touching the tool layer.
//!
//! v1 ships only [`NoopIndex`]: `search` always answers `None`, so every
//! query falls through to the walker. Dogfood numbers behind the decision
//! live in `bench/RESULTS.md` and the plan (section 4.6).

use std::path::Path;

/// One indexed hit, mirroring `walker::Match`.
#[derive(Debug, Clone)]
pub struct IndexedHit {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

/// A search index behind the walker. Implementations must be thread
/// safe; the server may call them from the blocking pool.
pub trait IndexStore: Send + Sync {
    /// Indexed hits for a literal (case-insensitive) query. `None` means
    /// "no data for this query" and the caller falls back to the walker.
    fn search_literal(&self, query: &str, max: usize) -> Option<Vec<IndexedHit>>;

    /// Record or refresh one file's content. Best effort; a no-op backend
    /// ignores it.
    fn record(&self, _path: &Path, _text: &str) {}
}

/// The v1 backend: no index at all. Every query falls through to the
/// walker, which stays the single source of truth for text search.
pub struct NoopIndex;

impl IndexStore for NoopIndex {
    fn search_literal(&self, _query: &str, _max: usize) -> Option<Vec<IndexedHit>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_index_always_falls_through() {
        let index = NoopIndex;
        assert!(index.search_literal("anything", 200).is_none());
        // record() is accepted and does nothing.
        index.record(Path::new("/tmp/x.rs"), "fn main() {}");
    }
}
