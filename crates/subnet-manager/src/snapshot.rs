//! Snapshot System
//!
//! Creates and manages state snapshots for recovery.

use chrono::{DateTime, Utc};
use platform_core::ChainState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use tracing::{debug, info};

/// Snapshot metadata
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMeta {
    /// Snapshot ID
    pub id: uuid::Uuid,

    /// Snapshot name
    pub name: String,

    /// Block height at snapshot
    pub block_height: u64,

    /// Epoch at snapshot
    pub epoch: u64,

    /// State hash
    pub state_hash: String,

    /// Created at
    pub created_at: DateTime<Utc>,

    /// Size in bytes
    pub size_bytes: u64,

    /// Is this an auto snapshot
    pub auto: bool,

    /// Reason for snapshot
    pub reason: String,
}

/// Snapshot data
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    /// Metadata
    pub meta: SnapshotMeta,

    /// Chain state
    pub chain_state: Vec<u8>,

    /// Challenge databases (challenge_id -> data)
    pub challenge_data: std::collections::HashMap<String, Vec<u8>>,

    /// Configuration
    pub config: Vec<u8>,
}

/// Snapshot manager
pub struct SnapshotManager {
    /// Data directory
    data_dir: PathBuf,

    /// Snapshots directory
    snapshots_dir: PathBuf,

    /// Maximum snapshots to keep
    max_snapshots: u32,

    /// Available snapshots
    snapshots: Vec<SnapshotMeta>,
}

impl SnapshotManager {
    /// Create a new snapshot manager
    pub fn new(data_dir: PathBuf, max_snapshots: u32) -> anyhow::Result<Self> {
        let snapshots_dir = data_dir.join("snapshots");
        fs::create_dir_all(&snapshots_dir)?;

        let mut manager = Self {
            data_dir,
            snapshots_dir,
            max_snapshots,
            snapshots: Vec::new(),
        };

        manager.load_snapshot_index()?;
        Ok(manager)
    }

    /// Load snapshot index
    fn load_snapshot_index(&mut self) -> anyhow::Result<()> {
        let index_path = self.snapshots_dir.join("index.json");

        if index_path.exists() {
            let content = fs::read_to_string(&index_path)?;
            self.snapshots = serde_json::from_str(&content)?;
        }

        Ok(())
    }

    /// Save snapshot index
    fn save_snapshot_index(&self) -> anyhow::Result<()> {
        let index_path = self.snapshots_dir.join("index.json");
        let content = serde_json::to_string_pretty(&self.snapshots)?;
        fs::write(&index_path, content)?;
        Ok(())
    }

    /// Create a new snapshot
    pub fn create_snapshot(
        &mut self,
        name: &str,
        block_height: u64,
        epoch: u64,
        chain_state: &ChainState,
        reason: &str,
        auto: bool,
    ) -> anyhow::Result<uuid::Uuid> {
        info!(
            "Creating snapshot: {} (block={}, epoch={})",
            name, block_height, epoch
        );

        let id = uuid::Uuid::new_v4();

        // Serialize chain state
        let chain_state_bytes = bincode::serialize(chain_state)?;
        let state_hash = Self::compute_hash(&chain_state_bytes);

        // Collect challenge data
        let mut challenge_data = std::collections::HashMap::new();
        let challenges_dir = self.data_dir.join("challenges");

        if challenges_dir.exists() {
            for entry in fs::read_dir(&challenges_dir)? {
                let entry = entry?;
                let challenge_id = entry.file_name().to_string_lossy().to_string();

                // Read challenge database
                let db_path = entry.path().join("db");
                if db_path.exists() {
                    // For sled, we need to copy the directory
                    let mut data = Vec::new();
                    Self::serialize_directory(&db_path, &mut data)?;
                    challenge_data.insert(challenge_id, data);
                }
            }
        }

        // Read config
        let config_path = self.data_dir.join("subnet_config.json");
        let config = if config_path.exists() {
            fs::read(&config_path)?
        } else {
            Vec::new()
        };

        // Create snapshot
        let snapshot = Snapshot {
            meta: SnapshotMeta {
                id,
                name: name.to_string(),
                block_height,
                epoch,
                state_hash: state_hash.clone(),
                created_at: Utc::now(),
                size_bytes: 0, // Will update after saving
                auto,
                reason: reason.to_string(),
            },
            chain_state: chain_state_bytes,
            challenge_data,
            config,
        };

        // Save snapshot
        let snapshot_path = self.snapshots_dir.join(format!("{}.snapshot", id));
        let snapshot_bytes = bincode::serialize(&snapshot)?;
        fs::write(&snapshot_path, &snapshot_bytes)?;

        // Update metadata with size
        let mut meta = snapshot.meta;
        meta.size_bytes = snapshot_bytes.len() as u64;

        self.snapshots.push(meta);
        self.save_snapshot_index()?;

        // Prune old snapshots
        self.prune_snapshots()?;

        info!("Snapshot created: {} ({} bytes)", id, snapshot_bytes.len());
        Ok(id)
    }

    /// Restore from a snapshot
    pub fn restore_snapshot(&self, snapshot_id: uuid::Uuid) -> anyhow::Result<Snapshot> {
        info!("Restoring snapshot: {}", snapshot_id);

        let snapshot_path = self.snapshots_dir.join(format!("{}.snapshot", snapshot_id));

        if !snapshot_path.exists() {
            anyhow::bail!("Snapshot not found: {}", snapshot_id);
        }

        let snapshot_bytes = fs::read(&snapshot_path)?;
        let snapshot: Snapshot = bincode::deserialize(&snapshot_bytes)?;

        // Verify hash
        let computed_hash = Self::compute_hash(&snapshot.chain_state);
        if computed_hash != snapshot.meta.state_hash {
            anyhow::bail!("Snapshot corrupted: hash mismatch");
        }

        info!(
            "Snapshot loaded: {} (block={})",
            snapshot_id, snapshot.meta.block_height
        );
        Ok(snapshot)
    }

    /// Apply a snapshot to restore state
    pub fn apply_snapshot(&self, snapshot: &Snapshot) -> anyhow::Result<ChainState> {
        info!("Applying snapshot: {}", snapshot.meta.id);

        // Deserialize chain state
        let chain_state: ChainState = bincode::deserialize(&snapshot.chain_state)?;

        // Restore config
        if !snapshot.config.is_empty() {
            let config_path = self.data_dir.join("subnet_config.json");
            fs::write(&config_path, &snapshot.config)?;
        }

        // Restore challenge data
        let challenges_dir = self.data_dir.join("challenges");
        fs::create_dir_all(&challenges_dir)?;

        for (challenge_id, data) in &snapshot.challenge_data {
            let challenge_dir = challenges_dir.join(challenge_id);
            fs::create_dir_all(&challenge_dir)?;

            let db_path = challenge_dir.join("db");
            Self::deserialize_directory(&db_path, data)?;
        }

        info!("Snapshot applied: {}", snapshot.meta.id);
        Ok(chain_state)
    }

    /// Get latest snapshot
    pub fn latest_snapshot(&self) -> Option<&SnapshotMeta> {
        self.snapshots.iter().max_by_key(|s| s.created_at)
    }

    /// Get snapshot by ID
    pub fn get_snapshot(&self, id: uuid::Uuid) -> Option<&SnapshotMeta> {
        self.snapshots.iter().find(|s| s.id == id)
    }

    /// List all snapshots
    pub fn list_snapshots(&self) -> &[SnapshotMeta] {
        &self.snapshots
    }

    /// Delete a snapshot
    pub fn delete_snapshot(&mut self, id: uuid::Uuid) -> anyhow::Result<()> {
        let snapshot_path = self.snapshots_dir.join(format!("{}.snapshot", id));

        if snapshot_path.exists() {
            fs::remove_file(&snapshot_path)?;
        }

        self.snapshots.retain(|s| s.id != id);
        self.save_snapshot_index()?;

        info!("Snapshot deleted: {}", id);
        Ok(())
    }

    /// Prune old snapshots, keeping max_snapshots
    fn prune_snapshots(&mut self) -> anyhow::Result<()> {
        if self.snapshots.len() <= self.max_snapshots as usize {
            return Ok(());
        }

        // Sort by date (oldest first)
        self.snapshots.sort_by_key(|s| s.created_at);

        // Remove oldest
        let to_remove = self.snapshots.len() - self.max_snapshots as usize;
        let removed: Vec<_> = self.snapshots.drain(0..to_remove).collect();

        for meta in removed {
            let path = self.snapshots_dir.join(format!("{}.snapshot", meta.id));
            if path.exists() {
                fs::remove_file(&path)?;
            }
            debug!("Pruned snapshot: {}", meta.id);
        }

        self.save_snapshot_index()?;
        Ok(())
    }

    /// Compute SHA256 hash
    fn compute_hash(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    /// Serialize a directory to bytes (simplified)
    fn serialize_directory(path: &PathBuf, output: &mut Vec<u8>) -> anyhow::Result<()> {
        // For now, just store the path - in production, would tar the directory
        let path_str = path.to_string_lossy();
        output.extend_from_slice(path_str.as_bytes());
        Ok(())
    }

    /// Deserialize bytes to a directory (simplified)
    fn deserialize_directory(_path: &PathBuf, _data: &[u8]) -> anyhow::Result<()> {
        // For now, no-op - in production, would untar the directory
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_core::{Keypair, NetworkConfig};
    use tempfile::tempdir;

    #[test]
    fn test_snapshot_manager() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 3).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        // Create snapshot
        let id = manager
            .create_snapshot("test_snapshot", 100, 1, &state, "test", false)
            .unwrap();

        assert_eq!(manager.list_snapshots().len(), 1);

        // Restore snapshot
        let snapshot = manager.restore_snapshot(id).unwrap();
        assert_eq!(snapshot.meta.block_height, 100);
    }

    #[test]
    fn test_snapshot_pruning() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 2).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        // Create 3 snapshots
        for i in 0..3 {
            manager
                .create_snapshot(&format!("snapshot_{}", i), i * 100, i, &state, "test", true)
                .unwrap();
        }

        // Should only keep 2
        assert_eq!(manager.list_snapshots().len(), 2);
    }
}
