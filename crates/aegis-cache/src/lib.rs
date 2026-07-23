//! # aegis-cache
//!
//! A deterministic, content-addressed cache for **AST compaction** — never for
//! LLM responses.
//!
//! Caching LLM output is a production hazard: cache invalidation is hard, and
//! returning a stale/hallucinated answer because a prompt *looked* similar breaks
//! things silently. Tree-sitter compaction, by contrast, is a pure function of the
//! file's bytes: `sha256(file_contents)` is a perfect key. If a file hasn't
//! changed, its skeleton hasn't either — return the cached result and skip the
//! parser entirely.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use sturdy_compact::{CompactResult, Compactor};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("compaction error: {0}")]
    Compact(#[from] sturdy_compact::CompactError),
    #[error("cache lock poisoned")]
    Poisoned,
}

pub type Result<T> = std::result::Result<T, CacheError>;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ast_cache (
    hash             TEXT PRIMARY KEY,   -- sha256 of the source bytes
    lang             TEXT NOT NULL,
    text             TEXT NOT NULL,      -- the compacted skeleton
    original_tokens  INTEGER NOT NULL,
    compacted_tokens INTEGER NOT NULL,
    elided_bodies    INTEGER NOT NULL,
    created_ms       INTEGER NOT NULL
);
"#;

/// The sha256 content hash used as the cache key.
pub fn content_hash(source: &str) -> String {
    let mut h = Sha256::new();
    h.update(source.as_bytes());
    format!("{:x}", h.finalize())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A SQLite-backed cache of compaction results, keyed by content hash.
pub struct AstCache {
    conn: Mutex<Connection>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl AstCache {
    /// Open (or create) a cache at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// An ephemeral in-memory cache (tests, one-shot runs).
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(AstCache {
            conn: Mutex::new(conn),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    /// Compact `source`, returning a cached result on a content-hash hit and only
    /// invoking the Tree-sitter parser on a miss.
    pub fn outline_cached(&self, compactor: &mut Compactor, source: &str) -> Result<CompactResult> {
        let hash = content_hash(source);
        if let Some(hit) = self.load(&hash)? {
            self.hits.fetch_add(1, Ordering::SeqCst);
            return Ok(hit);
        }
        self.misses.fetch_add(1, Ordering::SeqCst);
        let result = compactor.outline(source)?;
        self.store(&hash, compactor.language().name(), &result)?;
        Ok(result)
    }

    /// Cache hits observed so far.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::SeqCst)
    }

    /// Cache misses (i.e. parses actually run) so far.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::SeqCst)
    }

    fn load(&self, hash: &str) -> Result<Option<CompactResult>> {
        let conn = self.conn.lock().map_err(|_| CacheError::Poisoned)?;
        let row = conn
            .query_row(
                "SELECT text, original_tokens, compacted_tokens, elided_bodies
                 FROM ast_cache WHERE hash = ?1",
                [hash],
                |r| {
                    Ok(CompactResult {
                        text: r.get(0)?,
                        original_tokens: r.get::<_, i64>(1)? as usize,
                        compacted_tokens: r.get::<_, i64>(2)? as usize,
                        elided_bodies: r.get::<_, i64>(3)? as usize,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    fn store(&self, hash: &str, lang: &str, r: &CompactResult) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| CacheError::Poisoned)?;
        conn.execute(
            "INSERT OR REPLACE INTO ast_cache
             (hash, lang, text, original_tokens, compacted_tokens, elided_bodies, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                hash,
                lang,
                r.text,
                r.original_tokens as i64,
                r.compacted_tokens as i64,
                r.elided_bodies as i64,
                now_ms(),
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
        /// A greeter.
        pub fn hello(name: &str) -> String {
            let mut s = String::new();
            s.push_str("hi ");
            s.push_str(name);
            s
        }
    "#;

    #[test]
    fn hash_is_stable_and_content_sensitive() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }

    #[test]
    fn miss_then_hit_skips_the_parser() {
        let cache = AstCache::in_memory().unwrap();
        let mut c = Compactor::rust().unwrap();

        // First call: miss → parses and stores.
        let first = cache.outline_cached(&mut c, SRC).unwrap();
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 0);

        // Second call, identical source: hit → no parse, same skeleton.
        let second = cache.outline_cached(&mut c, SRC).unwrap();
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
        assert_eq!(first.text, second.text);
        assert_eq!(first.compacted_tokens, second.compacted_tokens);

        // A changed file: miss again.
        let changed = format!("{SRC}\n// touched");
        cache.outline_cached(&mut c, &changed).unwrap();
        assert_eq!(cache.misses(), 2);
    }

    #[test]
    fn cache_survives_reopen_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ast.sqlite");
        {
            let cache = AstCache::open(&path).unwrap();
            let mut c = Compactor::rust().unwrap();
            cache.outline_cached(&mut c, SRC).unwrap();
            assert_eq!(cache.misses(), 1);
        }
        // Reopen: the earlier result is a hit, no parse needed.
        let cache = AstCache::open(&path).unwrap();
        let mut c = Compactor::rust().unwrap();
        let r = cache.outline_cached(&mut c, SRC).unwrap();
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 0);
        assert!(r.text.contains("hello"));
    }
}
