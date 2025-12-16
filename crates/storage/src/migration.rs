//! Migration system for blockchain upgrades
//!
//! Provides versioned migrations that run when the blockchain is upgraded.
//! Similar to database migrations but for blockchain state.

use crate::types::{StorageKey, StorageValue};
use platform_core::{MiniChainError, Result};
use serde::{Deserialize, Serialize};
use sled::Tree;
use std::collections::BTreeMap;
use std::time::SystemTime;
use tracing::{info, warn};

/// Migration version number
pub type MigrationVersion = u64;

/// Migration trait - implement this for each migration
pub trait Migration: Send + Sync {
    /// Unique version number (must be sequential)
    fn version(&self) -> MigrationVersion;

    /// Human-readable name for this migration
    fn name(&self) -> &str;

    /// Description of what this migration does
    fn description(&self) -> &str {
        ""
    }

    /// Run the migration (upgrade)
    fn up(&self, ctx: &mut MigrationContext) -> Result<()>;

    /// Rollback the migration (downgrade) - optional
    fn down(&self, _ctx: &mut MigrationContext) -> Result<()> {
        Err(MiniChainError::Storage("Rollback not supported".into()))
    }

    /// Whether this migration can be rolled back
    fn reversible(&self) -> bool {
        false
    }
}

/// Context provided to migrations for reading/writing data
pub struct MigrationContext<'a> {
    /// Access to the dynamic storage tree
    pub storage_tree: &'a Tree,
    /// Access to the state tree
    pub state_tree: &'a Tree,
    /// Current block height
    pub block_height: u64,
    /// Changes made during this migration
    pub changes: Vec<MigrationChange>,
}

impl<'a> MigrationContext<'a> {
    pub fn new(storage_tree: &'a Tree, state_tree: &'a Tree, block_height: u64) -> Self {
        Self {
            storage_tree,
            state_tree,
            block_height,
            changes: Vec::new(),
        }
    }

    /// Get a value from dynamic storage
    pub fn get(&self, key: &StorageKey) -> Result<Option<StorageValue>> {
        let key_bytes = key.to_bytes();
        match self.storage_tree.get(&key_bytes) {
            Ok(Some(data)) => {
                let value: StorageValue = bincode::deserialize(&data)
                    .map_err(|e| MiniChainError::Serialization(e.to_string()))?;
                Ok(Some(value))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(MiniChainError::Storage(e.to_string())),
        }
    }

    /// Set a value in dynamic storage
    pub fn set(&mut self, key: StorageKey, value: StorageValue) -> Result<()> {
        let key_bytes = key.to_bytes();
        let old_value = self.get(&key)?;

        let data =
            bincode::serialize(&value).map_err(|e| MiniChainError::Serialization(e.to_string()))?;

        self.storage_tree
            .insert(&key_bytes, data)
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;

        self.changes.push(MigrationChange {
            key: key.clone(),
            old_value,
            new_value: Some(value),
        });

        Ok(())
    }

    /// Delete a value from dynamic storage
    pub fn delete(&mut self, key: &StorageKey) -> Result<Option<StorageValue>> {
        let key_bytes = key.to_bytes();
        let old_value = self.get(key)?;

        self.storage_tree
            .remove(&key_bytes)
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;

        if old_value.is_some() {
            self.changes.push(MigrationChange {
                key: key.clone(),
                old_value: old_value.clone(),
                new_value: None,
            });
        }

        Ok(old_value)
    }

    /// Scan keys with a prefix
    pub fn scan_prefix(&self, namespace: &str) -> Result<Vec<(StorageKey, StorageValue)>> {
        let prefix = StorageKey::namespace_prefix(namespace);
        let mut results = Vec::new();

        for item in self.storage_tree.scan_prefix(&prefix) {
            let (key_bytes, value_bytes) =
                item.map_err(|e| MiniChainError::Storage(e.to_string()))?;

            // Parse key (simplified - in production use proper parsing)
            let key_str = String::from_utf8_lossy(&key_bytes);
            let parts: Vec<&str> = key_str.split('\0').collect();
            if parts.len() >= 2 {
                let key = StorageKey {
                    namespace: parts[0].to_string(),
                    validator: None, // Simplified
                    key: parts.last().unwrap_or(&"").to_string(),
                };

                let value: StorageValue = bincode::deserialize(&value_bytes)
                    .map_err(|e| MiniChainError::Serialization(e.to_string()))?;

                results.push((key, value));
            }
        }

        Ok(results)
    }

    /// Get raw state data
    pub fn get_state_raw(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.state_tree
            .get(key)
            .map(|opt| opt.map(|v| v.to_vec()))
            .map_err(|e| MiniChainError::Storage(e.to_string()))
    }

    /// Set raw state data
    pub fn set_state_raw(&self, key: &str, value: Vec<u8>) -> Result<()> {
        self.state_tree
            .insert(key, value)
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;
        Ok(())
    }
}

/// Record of a change made during migration
#[derive(Clone, Debug)]
pub struct MigrationChange {
    pub key: StorageKey,
    pub old_value: Option<StorageValue>,
    pub new_value: Option<StorageValue>,
}

/// Record of an applied migration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationRecord {
    pub version: MigrationVersion,
    pub name: String,
    pub applied_at: SystemTime,
    pub block_height: u64,
    pub checksum: [u8; 32],
}

/// Migration runner - manages and executes migrations
pub struct MigrationRunner {
    migrations: BTreeMap<MigrationVersion, Box<dyn Migration>>,
    migrations_tree: Tree,
}

impl MigrationRunner {
    /// Create a new migration runner
    pub fn new(db: &sled::Db) -> Result<Self> {
        let migrations_tree = db.open_tree("migrations").map_err(|e| {
            MiniChainError::Storage(format!("Failed to open migrations tree: {}", e))
        })?;

        Ok(Self {
            migrations: BTreeMap::new(),
            migrations_tree,
        })
    }

    /// Register a migration
    pub fn register(&mut self, migration: Box<dyn Migration>) {
        let version = migration.version();
        if self.migrations.contains_key(&version) {
            warn!("Migration version {} already registered, skipping", version);
            return;
        }
        info!("Registered migration {}: {}", version, migration.name());
        self.migrations.insert(version, migration);
    }

    /// Get the current schema version
    pub fn current_version(&self) -> Result<MigrationVersion> {
        match self
            .migrations_tree
            .get("current_version")
            .map_err(|e| MiniChainError::Storage(e.to_string()))?
        {
            Some(data) => {
                let version: MigrationVersion = bincode::deserialize(&data)
                    .map_err(|e| MiniChainError::Serialization(e.to_string()))?;
                Ok(version)
            }
            None => Ok(0),
        }
    }

    /// Set the current schema version
    fn set_current_version(&self, version: MigrationVersion) -> Result<()> {
        let data = bincode::serialize(&version)
            .map_err(|e| MiniChainError::Serialization(e.to_string()))?;
        self.migrations_tree
            .insert("current_version", data)
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Get list of applied migrations
    pub fn applied_migrations(&self) -> Result<Vec<MigrationRecord>> {
        let mut records = Vec::new();

        for item in self.migrations_tree.scan_prefix(b"applied:") {
            let (_, data) = item.map_err(|e| MiniChainError::Storage(e.to_string()))?;
            let record: MigrationRecord = bincode::deserialize(&data)
                .map_err(|e| MiniChainError::Serialization(e.to_string()))?;
            records.push(record);
        }

        records.sort_by_key(|r| r.version);
        Ok(records)
    }

    /// Check if a migration has been applied
    pub fn is_applied(&self, version: MigrationVersion) -> Result<bool> {
        let key = format!("applied:{}", version);
        self.migrations_tree
            .contains_key(key)
            .map_err(|e| MiniChainError::Storage(e.to_string()))
    }

    /// Record that a migration was applied
    fn record_applied(&self, record: MigrationRecord) -> Result<()> {
        let key = format!("applied:{}", record.version);
        let data = bincode::serialize(&record)
            .map_err(|e| MiniChainError::Serialization(e.to_string()))?;
        self.migrations_tree
            .insert(key, data)
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Get pending migrations
    pub fn pending_migrations(&self) -> Result<Vec<MigrationVersion>> {
        let current = self.current_version()?;
        Ok(self
            .migrations
            .keys()
            .filter(|&&v| v > current)
            .copied()
            .collect())
    }

    /// Run all pending migrations
    pub fn run_pending(
        &self,
        storage_tree: &Tree,
        state_tree: &Tree,
        block_height: u64,
    ) -> Result<Vec<MigrationVersion>> {
        let pending = self.pending_migrations()?;

        if pending.is_empty() {
            info!("No pending migrations");
            return Ok(vec![]);
        }

        info!("Running {} pending migrations", pending.len());
        let mut applied = Vec::new();

        for version in pending {
            if let Some(migration) = self.migrations.get(&version) {
                info!("Running migration {}: {}", version, migration.name());

                let mut ctx = MigrationContext::new(storage_tree, state_tree, block_height);

                migration.up(&mut ctx)?;

                // Calculate checksum of changes
                let checksum = self.calculate_checksum(&ctx.changes);

                // Record the migration
                let record = MigrationRecord {
                    version,
                    name: migration.name().to_string(),
                    applied_at: SystemTime::now(),
                    block_height,
                    checksum,
                };

                self.record_applied(record)?;
                self.set_current_version(version)?;

                info!(
                    "Migration {} completed ({} changes)",
                    version,
                    ctx.changes.len()
                );
                applied.push(version);
            }
        }

        // Flush changes
        self.migrations_tree
            .flush()
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;
        storage_tree
            .flush()
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;
        state_tree
            .flush()
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;

        Ok(applied)
    }

    /// Rollback to a specific version
    pub fn rollback_to(
        &self,
        target_version: MigrationVersion,
        storage_tree: &Tree,
        state_tree: &Tree,
        block_height: u64,
    ) -> Result<Vec<MigrationVersion>> {
        let current = self.current_version()?;

        if target_version >= current {
            return Err(MiniChainError::Storage(format!(
                "Cannot rollback to version {} (current is {})",
                target_version, current
            )));
        }

        let mut rolled_back = Vec::new();

        // Rollback in reverse order
        for version in (target_version + 1..=current).rev() {
            if let Some(migration) = self.migrations.get(&version) {
                if !migration.reversible() {
                    return Err(MiniChainError::Storage(format!(
                        "Migration {} is not reversible",
                        version
                    )));
                }

                info!("Rolling back migration {}: {}", version, migration.name());

                let mut ctx = MigrationContext::new(storage_tree, state_tree, block_height);
                migration.down(&mut ctx)?;

                // Remove the applied record
                let key = format!("applied:{}", version);
                self.migrations_tree
                    .remove(key)
                    .map_err(|e| MiniChainError::Storage(e.to_string()))?;

                rolled_back.push(version);
            }
        }

        self.set_current_version(target_version)?;
        self.migrations_tree
            .flush()
            .map_err(|e| MiniChainError::Storage(e.to_string()))?;

        Ok(rolled_back)
    }

    /// Calculate checksum of migration changes
    fn calculate_checksum(&self, changes: &[MigrationChange]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();

        for change in changes {
            hasher.update(change.key.to_bytes());
            if let Some(ref v) = change.old_value {
                if let Ok(data) = bincode::serialize(v) {
                    hasher.update(&data);
                }
            }
            if let Some(ref v) = change.new_value {
                if let Ok(data) = bincode::serialize(v) {
                    hasher.update(&data);
                }
            }
        }

        hasher.finalize().into()
    }
}

// === Built-in Migrations ===

/// Initial migration - sets up base storage schema
pub struct InitialMigration;

impl Migration for InitialMigration {
    fn version(&self) -> MigrationVersion {
        1
    }
    fn name(&self) -> &str {
        "initial_setup"
    }
    fn description(&self) -> &str {
        "Initial storage schema setup"
    }

    fn up(&self, ctx: &mut MigrationContext) -> Result<()> {
        // Set schema version
        ctx.set(StorageKey::system("schema_version"), StorageValue::U64(1))?;

        // Set creation timestamp
        ctx.set(
            StorageKey::system("created_at"),
            StorageValue::U64(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            ),
        )?;

        // Initialize counters
        ctx.set(StorageKey::system("total_challenges"), StorageValue::U64(0))?;
        ctx.set(StorageKey::system("total_validators"), StorageValue::U64(0))?;
        ctx.set(StorageKey::system("total_jobs"), StorageValue::U64(0))?;

        Ok(())
    }
}

/// Migration to add challenge metrics storage
pub struct AddChallengeMetricsMigration;

impl Migration for AddChallengeMetricsMigration {
    fn version(&self) -> MigrationVersion {
        2
    }
    fn name(&self) -> &str {
        "add_challenge_metrics"
    }
    fn description(&self) -> &str {
        "Add per-challenge metrics storage"
    }

    fn up(&self, ctx: &mut MigrationContext) -> Result<()> {
        // Add metrics enabled flag
        ctx.set(
            StorageKey::system("metrics_enabled"),
            StorageValue::Bool(true),
        )?;

        // Add default retention period (7 days in seconds)
        ctx.set(
            StorageKey::system("metrics_retention_secs"),
            StorageValue::U64(7 * 24 * 60 * 60),
        )?;

        Ok(())
    }

    fn down(&self, ctx: &mut MigrationContext) -> Result<()> {
        ctx.delete(&StorageKey::system("metrics_enabled"))?;
        ctx.delete(&StorageKey::system("metrics_retention_secs"))?;
        Ok(())
    }

    fn reversible(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_migration_runner() {
        let dir = tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let storage_tree = db.open_tree("dynamic_storage").unwrap();
        let state_tree = db.open_tree("state").unwrap();

        let mut runner = MigrationRunner::new(&db).unwrap();

        // Register migrations
        runner.register(Box::new(InitialMigration));
        runner.register(Box::new(AddChallengeMetricsMigration));

        // Check pending
        let pending = runner.pending_migrations().unwrap();
        assert_eq!(pending.len(), 2);

        // Run migrations
        let applied = runner.run_pending(&storage_tree, &state_tree, 0).unwrap();
        assert_eq!(applied.len(), 2);

        // Check version
        assert_eq!(runner.current_version().unwrap(), 2);

        // Check no pending
        let pending = runner.pending_migrations().unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn test_migration_context() {
        let dir = tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let storage_tree = db.open_tree("dynamic_storage").unwrap();
        let state_tree = db.open_tree("state").unwrap();

        let mut ctx = MigrationContext::new(&storage_tree, &state_tree, 0);

        // Set and get
        let key = StorageKey::system("test_key");
        ctx.set(key.clone(), StorageValue::U64(42)).unwrap();

        let value = ctx.get(&key).unwrap();
        assert_eq!(value.unwrap().as_u64(), Some(42));

        // Delete
        ctx.delete(&key).unwrap();
        assert!(ctx.get(&key).unwrap().is_none());
    }
}
