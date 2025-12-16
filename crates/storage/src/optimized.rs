//! Optimized storage operations
//!
//! Provides batch operations and performance optimizations.

use parking_lot::RwLock;
use platform_core::{MiniChainError, Result};
use sled::Tree;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::debug;

/// Batch write buffer for optimized writes
pub struct BatchWriter {
    tree: Tree,
    buffer: Vec<(Vec<u8>, Vec<u8>)>,
    buffer_size: usize,
    max_buffer_size: usize,
    last_flush: Instant,
    flush_interval: Duration,
}

impl BatchWriter {
    pub fn new(tree: Tree, max_buffer_size: usize) -> Self {
        Self {
            tree,
            buffer: Vec::with_capacity(max_buffer_size),
            buffer_size: 0,
            max_buffer_size,
            last_flush: Instant::now(),
            flush_interval: Duration::from_millis(100),
        }
    }

    /// Add a write to the batch
    pub fn write(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        self.buffer_size += key.len() + value.len();
        self.buffer.push((key, value));

        // Auto-flush if buffer is full or time elapsed
        if self.buffer.len() >= self.max_buffer_size
            || self.last_flush.elapsed() > self.flush_interval
        {
            self.flush()?;
        }

        Ok(())
    }

    /// Flush all pending writes
    pub fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        let start = Instant::now();
        let count = self.buffer.len();

        // Use sled's batch for atomic writes
        let mut batch = sled::Batch::default();
        for (key, value) in self.buffer.drain(..) {
            batch.insert(key, value);
        }

        self.tree
            .apply_batch(batch)
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;

        self.buffer_size = 0;
        self.last_flush = Instant::now();

        debug!("Batch flush: {} writes in {:?}", count, start.elapsed());
        Ok(())
    }
}

impl Drop for BatchWriter {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            tracing::error!("Failed to flush batch on drop: {}", e);
        }
    }
}

/// LRU Cache for hot data
pub struct LruCache<K, V> {
    map: HashMap<K, (V, Instant)>,
    max_size: usize,
    ttl: Duration,
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> LruCache<K, V> {
    pub fn new(max_size: usize, ttl: Duration) -> Self {
        Self {
            map: HashMap::with_capacity(max_size),
            max_size,
            ttl,
        }
    }

    pub fn get(&self, key: &K) -> Option<V> {
        self.map.get(key).and_then(|(v, t)| {
            if t.elapsed() < self.ttl {
                Some(v.clone())
            } else {
                None
            }
        })
    }

    pub fn insert(&mut self, key: K, value: V) {
        // Evict if full
        if self.map.len() >= self.max_size {
            self.evict_oldest();
        }
        self.map.insert(key, (value, Instant::now()));
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.map.remove(key).map(|(v, _)| v)
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest_key) = self
            .map
            .iter()
            .min_by_key(|(_, (_, t))| *t)
            .map(|(k, _)| k.clone())
        {
            self.map.remove(&oldest_key);
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Remove expired entries
    pub fn cleanup(&mut self) {
        self.map.retain(|_, (_, t)| t.elapsed() < self.ttl);
    }
}

/// Read-through cache wrapper
pub struct CachedTree {
    tree: Tree,
    cache: Arc<RwLock<LruCache<Vec<u8>, Vec<u8>>>>,
    stats: Arc<RwLock<CacheStats>>,
}

#[derive(Default, Debug, Clone)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
}

impl CacheStats {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

impl CachedTree {
    pub fn new(tree: Tree, cache_size: usize, cache_ttl: Duration) -> Self {
        Self {
            tree,
            cache: Arc::new(RwLock::new(LruCache::new(cache_size, cache_ttl))),
            stats: Arc::new(RwLock::new(CacheStats::default())),
        }
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        // Check cache first
        if let Some(value) = self.cache.read().get(&key.to_vec()) {
            self.stats.write().hits += 1;
            return Ok(Some(value));
        }

        self.stats.write().misses += 1;

        // Load from disk
        match self
            .tree
            .get(key)
            .map_err(|e| MiniChainError::Storage(e.to_string()))?
        {
            Some(value) => {
                let value = value.to_vec();
                self.cache.write().insert(key.to_vec(), value.clone());
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    pub fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.tree
            .insert(key, value)
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;
        self.cache.write().insert(key.to_vec(), value.to_vec());
        self.stats.write().writes += 1;
        Ok(())
    }

    pub fn remove(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.cache.write().remove(&key.to_vec());
        self.tree
            .remove(key)
            .map(|opt| opt.map(|v| v.to_vec()))
            .map_err(|e| MiniChainError::Storage(e.to_string()))
    }

    pub fn stats(&self) -> CacheStats {
        self.stats.read().clone()
    }

    pub fn clear_cache(&self) {
        self.cache.write().clear();
    }

    pub fn flush(&self) -> Result<()> {
        self.tree
            .flush()
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;
        Ok(())
    }
}

/// Prefix scanner for efficient range queries
pub struct PrefixScanner<'a> {
    tree: &'a Tree,
    prefix: Vec<u8>,
}

impl<'a> PrefixScanner<'a> {
    pub fn new(tree: &'a Tree, prefix: Vec<u8>) -> Self {
        Self { tree, prefix }
    }

    /// Count keys with this prefix
    pub fn count(&self) -> Result<usize> {
        let mut count = 0;
        for _ in self.tree.scan_prefix(&self.prefix) {
            count += 1;
        }
        Ok(count)
    }

    /// Get all keys with this prefix
    pub fn keys(&self) -> Result<Vec<Vec<u8>>> {
        let mut keys = Vec::new();
        for item in self.tree.scan_prefix(&self.prefix) {
            let (key, _) = item.map_err(|e| MiniChainError::Storage(e.to_string()))?;
            keys.push(key.to_vec());
        }
        Ok(keys)
    }

    /// Get all values with this prefix
    pub fn values(&self) -> Result<Vec<Vec<u8>>> {
        let mut values = Vec::new();
        for item in self.tree.scan_prefix(&self.prefix) {
            let (_, value) = item.map_err(|e| MiniChainError::Storage(e.to_string()))?;
            values.push(value.to_vec());
        }
        Ok(values)
    }

    /// Iterate with a callback
    pub fn for_each<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(&[u8], &[u8]) -> Result<bool>,
    {
        for item in self.tree.scan_prefix(&self.prefix) {
            let (key, value) = item.map_err(|e| MiniChainError::Storage(e.to_string()))?;
            if !f(&key, &value)? {
                break;
            }
        }
        Ok(())
    }
}

/// Storage metrics collector
#[derive(Debug, Clone, Default)]
pub struct StorageMetrics {
    pub read_ops: u64,
    pub write_ops: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_latency_us: u64,
    pub write_latency_us: u64,
    pub cache_hit_rate: f64,
}

impl StorageMetrics {
    pub fn avg_read_latency_us(&self) -> f64 {
        if self.read_ops == 0 {
            0.0
        } else {
            self.read_latency_us as f64 / self.read_ops as f64
        }
    }

    pub fn avg_write_latency_us(&self) -> f64 {
        if self.write_ops == 0 {
            0.0
        } else {
            self.write_latency_us as f64 / self.write_ops as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_batch_writer() {
        let dir = tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let tree = db.open_tree("test").unwrap();

        let mut writer = BatchWriter::new(tree.clone(), 100);

        for i in 0..50 {
            writer
                .write(
                    format!("key{}", i).into_bytes(),
                    format!("value{}", i).into_bytes(),
                )
                .unwrap();
        }

        writer.flush().unwrap();

        assert!(tree.get("key0").unwrap().is_some());
        assert!(tree.get("key49").unwrap().is_some());
    }

    #[test]
    fn test_lru_cache() {
        let mut cache = LruCache::new(3, Duration::from_secs(60));

        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.insert("c", 3);

        assert_eq!(cache.get(&"a"), Some(1));
        assert_eq!(cache.len(), 3);

        // Insert 4th, should evict oldest
        cache.insert("d", 4);
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn test_cached_tree() {
        let dir = tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let tree = db.open_tree("test").unwrap();

        let cached = CachedTree::new(tree, 100, Duration::from_secs(60));

        cached.insert(b"key1", b"value1").unwrap();

        // First read is from cache (insert caches the value)
        assert_eq!(cached.get(b"key1").unwrap(), Some(b"value1".to_vec()));
        assert_eq!(cached.stats().hits, 1);

        // Second read also from cache
        assert_eq!(cached.get(b"key1").unwrap(), Some(b"value1".to_vec()));
        assert_eq!(cached.stats().hits, 2);

        // Clear cache, next read should be a miss
        cached.clear_cache();
        assert_eq!(cached.get(b"key1").unwrap(), Some(b"value1".to_vec()));
        assert_eq!(cached.stats().misses, 1);
    }
}
