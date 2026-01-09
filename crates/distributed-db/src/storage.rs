//! RocksDB storage backend
//! RocksDB storage backend
//!
//! Provides persistent key-value storage with:
//! - Column families for data separation
//! - Atomic batch writes
//! - Efficient iteration
//! - Anti-corruption protections (WAL, sync, atomic flush)

use rocksdb::{
    BoundColumnFamily, ColumnFamilyDescriptor, DBWithThreadMode, IteratorMode, MultiThreaded,
    Options, WriteBatch, WriteOptions,
};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Column families for different data types
pub const CF_CHALLENGES: &str = "challenges";
pub const CF_AGENTS: &str = "agents";
pub const CF_EVALUATIONS: &str = "evaluations";
pub const CF_WEIGHTS: &str = "weights";
pub const CF_TRANSACTIONS: &str = "transactions";
pub const CF_STATE: &str = "state";
pub const CF_INDEXES: &str = "indexes";
pub const CF_METADATA: &str = "metadata";

const ALL_CFS: &[&str] = &[
    CF_CHALLENGES,
    CF_AGENTS,
    CF_EVALUATIONS,
    CF_WEIGHTS,
    CF_TRANSACTIONS,
    CF_STATE,
    CF_INDEXES,
    CF_METADATA,
];

/// State root key in metadata
const STATE_ROOT_KEY: &[u8] = b"state_root";

/// Minimum free disk space (1GB)
const MIN_DISK_SPACE_BYTES: u64 = 1024 * 1024 * 1024;

/// RocksDB storage wrapper with anti-corruption protections
pub struct RocksStorage {
    db: DBWithThreadMode<MultiThreaded>,
    /// Flag to prevent writes during shutdown
    shutdown: AtomicBool,
}

impl RocksStorage {
    /// Open or create the database
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        info!("Opening RocksDB at {:?}", path);

        // Check disk space before opening
        Self::check_disk_space(path)?;

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_max_open_files(256);
        opts.set_keep_log_file_num(3);
        opts.set_max_total_wal_size(64 * 1024 * 1024); // 64MB WAL
        opts.set_write_buffer_size(32 * 1024 * 1024); // 32MB
        opts.set_max_write_buffer_number(3);
        opts.set_target_file_size_base(64 * 1024 * 1024); // 64MB
        opts.set_level_zero_file_num_compaction_trigger(4);
        opts.set_level_zero_slowdown_writes_trigger(20);
        opts.set_level_zero_stop_writes_trigger(30);
        opts.set_compression_type(rocksdb::DBCompressionType::Lz4);

        // Anti-corruption settings
        opts.set_wal_recovery_mode(rocksdb::DBRecoveryMode::AbsoluteConsistency);
        opts.set_atomic_flush(true); // Atomic flush across column families

        // Column family options
        let cf_opts = Options::default();
        let cfs: Vec<ColumnFamilyDescriptor> = ALL_CFS
            .iter()
            .map(|name| ColumnFamilyDescriptor::new(*name, cf_opts.clone()))
            .collect();

        let db = DBWithThreadMode::<MultiThreaded>::open_cf_descriptors(&opts, path, cfs)?;

        info!(
            "RocksDB opened successfully with {} column families",
            ALL_CFS.len()
        );

        Ok(Self {
            db,
            shutdown: AtomicBool::new(false),
        })
    }

    /// Check disk space before operations
    fn check_disk_space(path: &Path) -> anyhow::Result<()> {
        // Get the directory to check (create if needed for new DBs)
        let check_path = if path.exists() {
            path.to_path_buf()
        } else if let Some(parent) = path.parent() {
            parent.to_path_buf()
        } else {
            return Ok(());
        };

        #[cfg(unix)]
        {
            if check_path.exists() {
                // Use statvfs for disk space on Unix
                let output = std::process::Command::new("df")
                    .arg("-B1")
                    .arg(&check_path)
                    .output();

                if let Ok(output) = output {
                    if let Ok(stdout) = String::from_utf8(output.stdout) {
                        // Parse df output (second line, 4th column is available)
                        if let Some(line) = stdout.lines().nth(1) {
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if parts.len() >= 4 {
                                if let Ok(avail) = parts[3].parse::<u64>() {
                                    if avail < MIN_DISK_SPACE_BYTES {
                                        return Err(anyhow::anyhow!(
                                            "Insufficient disk space: {} bytes available, {} required",
                                            avail,
                                            MIN_DISK_SPACE_BYTES
                                        ));
                                    }
                                    debug!("Disk space check passed: {} bytes available", avail);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Mark shutdown to prevent new writes
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        info!("RocksStorage marked for shutdown");
        // Flush WAL before shutdown
        if let Err(e) = self.db.flush_wal(true) {
            warn!("Failed to flush WAL on shutdown: {}", e);
        }
    }

    /// Check if shutdown is in progress
    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Get column family handle
    fn cf(&self, name: &str) -> anyhow::Result<Arc<BoundColumnFamily<'_>>> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| anyhow::anyhow!("Column family '{}' not found", name))
    }

    /// Get value by key
    pub fn get(&self, collection: &str, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        let cf = self.cf(collection)?;
        Ok(self.db.get_cf(&cf, key)?)
    }

    /// Put value (async - buffered by WAL)
    pub fn put(&self, collection: &str, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
        if self.is_shutdown() {
            return Err(anyhow::anyhow!("Storage is shutting down"));
        }
        let cf = self.cf(collection)?;
        self.db.put_cf(&cf, key, value)?;
        debug!(
            "Put {}:{} ({} bytes)",
            collection,
            hex::encode(&key[..key.len().min(8)]),
            value.len()
        );
        Ok(())
    }

    /// Put value with sync (for critical data - waits for disk write)
    pub fn put_sync(&self, collection: &str, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
        if self.is_shutdown() {
            return Err(anyhow::anyhow!("Storage is shutting down"));
        }
        let cf = self.cf(collection)?;
        let mut opts = WriteOptions::default();
        opts.set_sync(true); // Force sync to disk
        self.db.put_cf_opt(&cf, key, value, &opts)?;
        debug!(
            "Put (sync) {}:{} ({} bytes)",
            collection,
            hex::encode(&key[..key.len().min(8)]),
            value.len()
        );
        Ok(())
    }

    /// Delete value
    pub fn delete(&self, collection: &str, key: &[u8]) -> anyhow::Result<()> {
        let cf = self.cf(collection)?;
        self.db.delete_cf(&cf, key)?;
        debug!(
            "Delete {}:{}",
            collection,
            hex::encode(&key[..key.len().min(8)])
        );
        Ok(())
    }

    /// Batch write operations
    pub fn write_batch(&self, operations: Vec<BatchOp>) -> anyhow::Result<()> {
        let mut batch = WriteBatch::default();

        for op in operations {
            match op {
                BatchOp::Put {
                    collection,
                    key,
                    value,
                } => {
                    let cf = self.cf(&collection)?;
                    batch.put_cf(&cf, &key, &value);
                }
                BatchOp::Delete { collection, key } => {
                    let cf = self.cf(&collection)?;
                    batch.delete_cf(&cf, &key);
                }
            }
        }

        self.db.write(batch)?;
        Ok(())
    }

    /// Iterate over a collection
    pub fn iter_collection(&self, collection: &str) -> anyhow::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let cf = self.cf(collection)?;
        let iter = self.db.iterator_cf(&cf, IteratorMode::Start);

        let mut results = Vec::new();
        for item in iter {
            let (key, value) = item?;
            results.push((key.to_vec(), value.to_vec()));
        }

        Ok(results)
    }

    /// Iterate with prefix
    pub fn iter_prefix(
        &self,
        collection: &str,
        prefix: &[u8],
    ) -> anyhow::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let cf = self.cf(collection)?;
        let iter = self.db.prefix_iterator_cf(&cf, prefix);

        let mut results = Vec::new();
        for item in iter {
            let (key, value) = item?;
            if !key.starts_with(prefix) {
                break;
            }
            results.push((key.to_vec(), value.to_vec()));
        }

        Ok(results)
    }

    /// List all collections
    pub fn list_collections(&self) -> anyhow::Result<Vec<String>> {
        Ok(ALL_CFS.iter().map(|s| s.to_string()).collect())
    }

    /// Get collection size (approximate)
    pub fn collection_size(&self, collection: &str) -> anyhow::Result<u64> {
        let cf = self.cf(collection)?;
        let props = self
            .db
            .property_int_value_cf(&cf, "rocksdb.estimate-num-keys")?;
        Ok(props.unwrap_or(0))
    }

    /// Get state root
    pub fn get_state_root(&self) -> anyhow::Result<Option<[u8; 32]>> {
        let cf = self.cf(CF_METADATA)?;
        if let Some(value) = self.db.get_cf(&cf, STATE_ROOT_KEY)? {
            if value.len() == 32 {
                let mut root = [0u8; 32];
                root.copy_from_slice(&value);
                return Ok(Some(root));
            }
        }
        Ok(None)
    }

    /// Set state root
    pub fn set_state_root(&self, root: &[u8; 32]) -> anyhow::Result<()> {
        let cf = self.cf(CF_METADATA)?;
        self.db.put_cf(&cf, STATE_ROOT_KEY, root)?;
        Ok(())
    }

    /// Store confirmed transaction
    pub fn store_confirmed_tx(
        &self,
        tx: &super::Transaction,
        receipt: &super::TransactionReceipt,
        block: u64,
    ) -> anyhow::Result<()> {
        let cf = self.cf(CF_TRANSACTIONS)?;

        let key = tx.id();
        let value = bincode::serialize(&(tx, receipt, block))?;

        self.db.put_cf(&cf, key, &value)?;

        // Also index by block
        let block_key = format!("block:{}:{}", block, hex::encode(key));
        self.db.put_cf(&cf, block_key.as_bytes(), key)?;

        Ok(())
    }

    /// Get transactions for block
    pub fn get_block_transactions(&self, block: u64) -> anyhow::Result<Vec<[u8; 32]>> {
        let prefix = format!("block:{}:", block);
        let entries = self.iter_prefix(CF_TRANSACTIONS, prefix.as_bytes())?;

        let mut tx_ids = Vec::new();
        for (_, value) in entries {
            if value.len() == 32 {
                let mut id = [0u8; 32];
                id.copy_from_slice(&value);
                tx_ids.push(id);
            }
        }

        Ok(tx_ids)
    }

    /// Compact database
    pub fn compact(&self) -> anyhow::Result<()> {
        info!("Compacting database...");
        for cf_name in ALL_CFS {
            if let Ok(cf) = self.cf(cf_name) {
                self.db.compact_range_cf(&cf, None::<&[u8]>, None::<&[u8]>);
            }
        }
        info!("Database compaction complete");
        Ok(())
    }

    /// Get database stats
    pub fn stats(&self) -> StorageStats {
        let mut stats = StorageStats::default();

        for cf_name in ALL_CFS {
            if let Ok(size) = self.collection_size(cf_name) {
                stats.collection_sizes.insert(cf_name.to_string(), size);
                stats.total_keys += size;
            }
        }

        stats
    }
}

/// Batch operation
#[derive(Debug, Clone)]
pub enum BatchOp {
    Put {
        collection: String,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        collection: String,
        key: Vec<u8>,
    },
}

/// Storage statistics
#[derive(Debug, Clone, Default)]
pub struct StorageStats {
    pub total_keys: u64,
    pub collection_sizes: std::collections::HashMap<String, u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_basic_operations() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        // Put
        storage
            .put(CF_CHALLENGES, b"test-key", b"test-value")
            .unwrap();

        // Get
        let value = storage.get(CF_CHALLENGES, b"test-key").unwrap();
        assert_eq!(value, Some(b"test-value".to_vec()));

        // Delete
        storage.delete(CF_CHALLENGES, b"test-key").unwrap();
        let value = storage.get(CF_CHALLENGES, b"test-key").unwrap();
        assert!(value.is_none());
    }

    #[test]
    fn test_batch_write() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        let ops = vec![
            BatchOp::Put {
                collection: CF_CHALLENGES.to_string(),
                key: b"key1".to_vec(),
                value: b"value1".to_vec(),
            },
            BatchOp::Put {
                collection: CF_CHALLENGES.to_string(),
                key: b"key2".to_vec(),
                value: b"value2".to_vec(),
            },
        ];

        storage.write_batch(ops).unwrap();

        assert_eq!(
            storage.get(CF_CHALLENGES, b"key1").unwrap(),
            Some(b"value1".to_vec())
        );
        assert_eq!(
            storage.get(CF_CHALLENGES, b"key2").unwrap(),
            Some(b"value2".to_vec())
        );
    }

    #[test]
    fn test_iteration() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        storage.put(CF_AGENTS, b"agent:1", b"data1").unwrap();
        storage.put(CF_AGENTS, b"agent:2", b"data2").unwrap();
        storage.put(CF_AGENTS, b"other:1", b"other").unwrap();

        // Iterate all
        let all = storage.iter_collection(CF_AGENTS).unwrap();
        assert_eq!(all.len(), 3);

        // Iterate prefix
        let agents = storage.iter_prefix(CF_AGENTS, b"agent:").unwrap();
        assert_eq!(agents.len(), 2);
    }

    #[test]
    fn test_storage_open() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();
        assert!(!storage.is_shutdown());
    }

    #[test]
    fn test_put_sync() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        storage.put_sync(CF_CHALLENGES, b"key1", b"value1").unwrap();
        let value = storage.get(CF_CHALLENGES, b"key1").unwrap();
        assert_eq!(value, Some(b"value1".to_vec()));
    }

    #[test]
    fn test_shutdown() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        assert!(!storage.is_shutdown());
        storage.shutdown();
        assert!(storage.is_shutdown());

        // Operations should fail after shutdown
        let result = storage.put(CF_CHALLENGES, b"key", b"value");
        assert!(result.is_err());
    }

    #[test]
    fn test_shutdown_put_sync() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        storage.shutdown();
        let result = storage.put_sync(CF_CHALLENGES, b"key", b"value");
        assert!(result.is_err());
    }

    #[test]
    fn test_state_root_operations() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        // Initially no state root
        assert!(storage.get_state_root().unwrap().is_none());

        // Set state root
        let root = [42u8; 32];
        storage.set_state_root(&root).unwrap();

        // Get state root
        let retrieved = storage.get_state_root().unwrap();
        assert_eq!(retrieved, Some(root));
    }

    #[test]
    fn test_list_collections() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        let collections = storage.list_collections().unwrap();
        assert!(collections.contains(&CF_CHALLENGES.to_string()));
        assert!(collections.contains(&CF_AGENTS.to_string()));
        assert!(collections.contains(&CF_METADATA.to_string()));
        assert_eq!(collections.len(), ALL_CFS.len());
    }

    #[test]
    fn test_collection_size() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        // Initially empty
        let size = storage.collection_size(CF_CHALLENGES).unwrap();
        assert_eq!(size, 0);

        // Add some data
        storage.put(CF_CHALLENGES, b"key1", b"value1").unwrap();
        storage.put(CF_CHALLENGES, b"key2", b"value2").unwrap();

        let size = storage.collection_size(CF_CHALLENGES).unwrap();
        assert!(size >= 2);
    }

    #[test]
    fn test_store_confirmed_tx() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        let hotkey = platform_core::Hotkey::from_bytes(&[1u8; 32]).unwrap();
        let tx = crate::Transaction::new(
            hotkey,
            crate::Operation::Put {
                collection: "test".to_string(),
                key: b"key".to_vec(),
                value: b"value".to_vec(),
            },
        );

        let receipt = crate::TransactionReceipt {
            tx_id: tx.id(),
            success: true,
            execution_time_us: 100,
            state_root: [0u8; 32],
        };

        storage.store_confirmed_tx(&tx, &receipt, 100).unwrap();

        // Verify it was stored
        let tx_ids = storage.get_block_transactions(100).unwrap();
        assert_eq!(tx_ids.len(), 1);
        assert_eq!(tx_ids[0], tx.id());
    }

    #[test]
    fn test_get_block_transactions() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        let hotkey = platform_core::Hotkey::from_bytes(&[1u8; 32]).unwrap();

        // Store multiple transactions for the same block
        for i in 0..3 {
            let tx = crate::Transaction::new(
                hotkey.clone(),
                crate::Operation::Put {
                    collection: "test".to_string(),
                    key: format!("key{}", i).into_bytes(),
                    value: b"value".to_vec(),
                },
            );

            let receipt = crate::TransactionReceipt {
                tx_id: tx.id(),
                success: true,
                execution_time_us: 100,
                state_root: [0u8; 32],
            };

            storage.store_confirmed_tx(&tx, &receipt, 50).unwrap();
        }

        let tx_ids = storage.get_block_transactions(50).unwrap();
        assert_eq!(tx_ids.len(), 3);

        // Different block should return empty
        let tx_ids_other = storage.get_block_transactions(99).unwrap();
        assert_eq!(tx_ids_other.len(), 0);
    }

    #[test]
    fn test_batch_write_with_delete() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        // First put some data
        storage.put(CF_AGENTS, b"key1", b"value1").unwrap();
        storage.put(CF_AGENTS, b"key2", b"value2").unwrap();

        // Batch operations with mix of put and delete
        let ops = vec![
            BatchOp::Put {
                collection: CF_AGENTS.to_string(),
                key: b"key3".to_vec(),
                value: b"value3".to_vec(),
            },
            BatchOp::Delete {
                collection: CF_AGENTS.to_string(),
                key: b"key1".to_vec(),
            },
        ];

        storage.write_batch(ops).unwrap();

        // key1 should be deleted
        assert!(storage.get(CF_AGENTS, b"key1").unwrap().is_none());
        // key2 should still exist
        assert_eq!(
            storage.get(CF_AGENTS, b"key2").unwrap(),
            Some(b"value2".to_vec())
        );
        // key3 should be added
        assert_eq!(
            storage.get(CF_AGENTS, b"key3").unwrap(),
            Some(b"value3".to_vec())
        );
    }

    #[test]
    fn test_compact() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        // Add and delete some data to create fragmentation
        for i in 0..100 {
            storage
                .put(CF_CHALLENGES, format!("key{}", i).as_bytes(), b"value")
                .unwrap();
        }
        for i in 0..50 {
            storage
                .delete(CF_CHALLENGES, format!("key{}", i).as_bytes())
                .unwrap();
        }

        // Compact should succeed
        storage.compact().unwrap();
    }

    #[test]
    fn test_stats() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        storage.put(CF_CHALLENGES, b"key1", b"value1").unwrap();
        storage.put(CF_AGENTS, b"key2", b"value2").unwrap();

        let stats = storage.stats();
        assert!(stats.total_keys >= 2);
        assert!(stats.collection_sizes.contains_key(CF_CHALLENGES));
    }

    #[test]
    fn test_batch_op_variants() {
        let put_op = BatchOp::Put {
            collection: "test".to_string(),
            key: b"key".to_vec(),
            value: b"value".to_vec(),
        };

        let delete_op = BatchOp::Delete {
            collection: "test".to_string(),
            key: b"key".to_vec(),
        };

        // Just verify they can be created
        match put_op {
            BatchOp::Put { .. } => {}
            _ => panic!("Expected Put"),
        }

        match delete_op {
            BatchOp::Delete { .. } => {}
            _ => panic!("Expected Delete"),
        }
    }

    #[test]
    fn test_storage_stats_default() {
        let stats = StorageStats::default();
        assert_eq!(stats.total_keys, 0);
        assert!(stats.collection_sizes.is_empty());
    }

    #[test]
    fn test_iter_prefix_empty() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        let results = storage.iter_prefix(CF_AGENTS, b"nonexistent:").unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_get_nonexistent_key() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        let value = storage.get(CF_CHALLENGES, b"nonexistent").unwrap();
        assert!(value.is_none());
    }

    #[test]
    fn test_delete_nonexistent_key() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        // Should not error
        storage.delete(CF_CHALLENGES, b"nonexistent").unwrap();
    }

    #[test]
    fn test_iter_collection_empty() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        let results = storage.iter_collection(CF_WEIGHTS).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_cf_invalid_name() {
        let dir = tempdir().unwrap();
        let storage = RocksStorage::open(dir.path()).unwrap();

        let result = storage.get("invalid_cf", b"key");
        assert!(result.is_err());
    }
}
