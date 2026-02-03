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

/// Challenge-specific state snapshot for hot-reload
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChallengeStateSnapshot {
    /// Snapshot ID
    pub id: uuid::Uuid,

    /// Challenge crate identifier
    pub challenge_id: String,

    /// Challenge crate version at snapshot time
    pub crate_version: String,

    /// Serialized challenge state
    pub state_data: Vec<u8>,

    /// Pending evaluations at snapshot time
    pub pending_evaluations: Vec<ChallengeEvaluationState>,

    /// Snapshot reason (e.g., "pre_update", "scheduled", "manual")
    pub reason: String,

    /// Created timestamp
    pub created_at: DateTime<Utc>,

    /// State hash for integrity verification
    pub state_hash: String,
}

/// State of a challenge evaluation at snapshot time
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChallengeEvaluationState {
    /// Evaluation request ID
    pub request_id: String,

    /// Participant identifier
    pub participant_id: String,

    /// Progress percentage (0-100)
    pub progress: u8,

    /// Evaluation-specific data (JSON serialized as string for bincode compatibility)
    pub data: String,

    /// Started timestamp
    pub started_at: DateTime<Utc>,
}

/// Metadata for a challenge snapshot (without full data)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChallengeSnapshotMeta {
    /// Snapshot ID
    pub id: uuid::Uuid,

    /// Challenge crate identifier
    pub challenge_id: String,

    /// Challenge crate version at snapshot time
    pub crate_version: String,

    /// Snapshot reason
    pub reason: String,

    /// Created timestamp
    pub created_at: DateTime<Utc>,

    /// Size in bytes
    pub size_bytes: u64,

    /// Number of pending evaluations
    pub pending_count: usize,
}

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

    /// Challenge snapshots directory
    challenge_snapshots_dir: PathBuf,

    /// Challenge snapshots metadata
    challenge_snapshots: Vec<ChallengeSnapshotMeta>,
}

impl SnapshotManager {
    /// Create a new snapshot manager
    pub fn new(data_dir: PathBuf, max_snapshots: u32) -> anyhow::Result<Self> {
        let snapshots_dir = data_dir.join("snapshots");
        fs::create_dir_all(&snapshots_dir)?;

        let challenge_snapshots_dir = data_dir.join("challenge_snapshots");
        fs::create_dir_all(&challenge_snapshots_dir)?;

        let mut manager = Self {
            data_dir,
            snapshots_dir,
            max_snapshots,
            snapshots: Vec::new(),
            challenge_snapshots_dir,
            challenge_snapshots: Vec::new(),
        };

        manager.load_snapshot_index()?;
        manager.load_challenge_snapshot_index()?;
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

    /// Load challenge snapshot index
    fn load_challenge_snapshot_index(&mut self) -> anyhow::Result<()> {
        let index_path = self.challenge_snapshots_dir.join("index.json");

        if index_path.exists() {
            let content = fs::read_to_string(&index_path)?;
            self.challenge_snapshots = serde_json::from_str(&content)?;
        }

        Ok(())
    }

    /// Save challenge snapshot index
    fn save_challenge_snapshot_index(&self) -> anyhow::Result<()> {
        let index_path = self.challenge_snapshots_dir.join("index.json");
        let content = serde_json::to_string_pretty(&self.challenge_snapshots)?;
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

    /// Create a challenge-specific state snapshot
    pub fn create_challenge_snapshot(
        &mut self,
        challenge_id: &str,
        crate_version: &str,
        state_data: Vec<u8>,
        pending_evaluations: Vec<ChallengeEvaluationState>,
        reason: &str,
    ) -> anyhow::Result<uuid::Uuid> {
        info!(
            "Creating challenge snapshot: {} (version={})",
            challenge_id, crate_version
        );

        let id = uuid::Uuid::new_v4();
        let state_hash = Self::compute_hash(&state_data);
        let pending_count = pending_evaluations.len();

        let snapshot = ChallengeStateSnapshot {
            id,
            challenge_id: challenge_id.to_string(),
            crate_version: crate_version.to_string(),
            state_data,
            pending_evaluations,
            reason: reason.to_string(),
            created_at: Utc::now(),
            state_hash,
        };

        // Save snapshot to disk
        let snapshot_path = self
            .challenge_snapshots_dir
            .join(format!("{}.challenge_snapshot", id));
        let snapshot_bytes = bincode::serialize(&snapshot)
            .expect("Failed to serialize challenge snapshot - snapshot data may be corrupted");
        fs::write(&snapshot_path, &snapshot_bytes)?;

        // Create metadata
        let meta = ChallengeSnapshotMeta {
            id,
            challenge_id: challenge_id.to_string(),
            crate_version: crate_version.to_string(),
            reason: reason.to_string(),
            created_at: snapshot.created_at,
            size_bytes: snapshot_bytes.len() as u64,
            pending_count,
        };

        self.challenge_snapshots.push(meta);
        self.save_challenge_snapshot_index()?;

        info!(
            "Challenge snapshot created: {} ({} bytes, {} pending evaluations)",
            id,
            snapshot_bytes.len(),
            pending_count
        );
        Ok(id)
    }

    /// Restore challenge state from snapshot
    pub fn restore_challenge_snapshot(
        &self,
        snapshot_id: uuid::Uuid,
    ) -> anyhow::Result<ChallengeStateSnapshot> {
        info!("Restoring challenge snapshot: {}", snapshot_id);

        let snapshot_path = self
            .challenge_snapshots_dir
            .join(format!("{}.challenge_snapshot", snapshot_id));

        if !snapshot_path.exists() {
            anyhow::bail!("Challenge snapshot not found: {}", snapshot_id);
        }

        let snapshot_bytes = fs::read(&snapshot_path)?;
        let snapshot: ChallengeStateSnapshot = bincode::deserialize(&snapshot_bytes)
            .expect("Failed to deserialize challenge snapshot - snapshot file may be corrupted");

        // Verify hash
        let computed_hash = Self::compute_hash(&snapshot.state_data);
        if computed_hash != snapshot.state_hash {
            anyhow::bail!("Challenge snapshot corrupted: hash mismatch");
        }

        info!(
            "Challenge snapshot loaded: {} (challenge={}, version={})",
            snapshot_id, snapshot.challenge_id, snapshot.crate_version
        );
        Ok(snapshot)
    }

    /// List challenge snapshots
    pub fn list_challenge_snapshots(
        &self,
        challenge_id: Option<&str>,
    ) -> Vec<ChallengeSnapshotMeta> {
        match challenge_id {
            Some(id) => self
                .challenge_snapshots
                .iter()
                .filter(|s| s.challenge_id == id)
                .cloned()
                .collect(),
            None => self.challenge_snapshots.clone(),
        }
    }

    /// Delete challenge snapshot
    pub fn delete_challenge_snapshot(&mut self, snapshot_id: uuid::Uuid) -> anyhow::Result<()> {
        let snapshot_path = self
            .challenge_snapshots_dir
            .join(format!("{}.challenge_snapshot", snapshot_id));

        if snapshot_path.exists() {
            fs::remove_file(&snapshot_path)?;
        }

        self.challenge_snapshots.retain(|s| s.id != snapshot_id);
        self.save_challenge_snapshot_index()?;

        info!("Challenge snapshot deleted: {}", snapshot_id);
        Ok(())
    }

    /// Get latest challenge snapshot for a specific challenge
    pub fn latest_challenge_snapshot(&self, challenge_id: &str) -> Option<ChallengeSnapshotMeta> {
        self.challenge_snapshots
            .iter()
            .filter(|s| s.challenge_id == challenge_id)
            .max_by_key(|s| s.created_at)
            .cloned()
    }

    /// Compute SHA256 hash
    fn compute_hash(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    /// Serialize a directory to bytes (simplified)
    fn serialize_directory(path: &std::path::Path, output: &mut Vec<u8>) -> anyhow::Result<()> {
        // For now, just store the path - in production, would tar the directory
        // NOTE: deserialize_directory writes the stored bytes back as raw data.
        // This asymmetry is intentional for now, so document it until full tar support exists.
        let path_str = path.to_string_lossy();
        output.extend_from_slice(path_str.as_bytes());
        Ok(())
    }

    /// Deserialize bytes to a directory (simplified)
    fn deserialize_directory(path: &PathBuf, data: &[u8]) -> anyhow::Result<()> {
        // Placeholder implementation: recreate directory and store raw bytes
        fs::create_dir_all(path)?;
        if !data.is_empty() {
            let file_path = path.join("data.bin");
            fs::write(file_path, data)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_core::{Keypair, NetworkConfig};
    use tempfile::tempdir;

    #[test]
    fn test_load_snapshot_index_success() {
        let dir = tempdir().unwrap();
        let snapshots_dir = dir.path().join("snapshots");
        std::fs::create_dir_all(&snapshots_dir).unwrap();

        let meta = SnapshotMeta {
            id: uuid::Uuid::new_v4(),
            name: "indexed".into(),
            block_height: 123,
            epoch: 4,
            state_hash: "abc123".into(),
            created_at: Utc::now(),
            size_bytes: 42,
            auto: false,
            reason: "preloaded".into(),
        };

        let index_path = snapshots_dir.join("index.json");
        let content = serde_json::to_string_pretty(&vec![meta.clone()]).unwrap();
        std::fs::write(index_path, content).unwrap();

        let manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();
        let snapshots = manager.list_snapshots();
        assert_eq!(snapshots.len(), 1);
        let loaded = &snapshots[0];
        assert_eq!(loaded.id, meta.id);
        assert_eq!(loaded.name, "indexed");
        assert_eq!(loaded.block_height, 123);
        assert_eq!(loaded.reason, "preloaded");
    }

    #[test]
    fn test_create_snapshot_reads_config_file() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("subnet_config.json");
        std::fs::write(&config_path, b"{\"dummy\":true}").unwrap();

        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 3).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        let id = manager
            .create_snapshot("config_snapshot", 50, 2, &state, "test", false)
            .unwrap();

        let snapshot = manager.restore_snapshot(id).unwrap();
        assert_eq!(snapshot.config, b"{\"dummy\":true}".to_vec());
    }

    #[test]
    fn test_restore_snapshot_detects_hash_mismatch() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 3).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        let id = manager
            .create_snapshot("corruptible", 42, 1, &state, "test", false)
            .unwrap();

        // Corrupt the stored snapshot by altering the recorded hash
        let snapshot_path = dir
            .path()
            .join("snapshots")
            .join(format!("{}.snapshot", id));
        let bytes = std::fs::read(&snapshot_path).unwrap();
        let mut snapshot: Snapshot = bincode::deserialize(&bytes).unwrap();
        snapshot.meta.state_hash = "bad-hash".into();
        let corrupt = bincode::serialize(&snapshot).unwrap();
        std::fs::write(&snapshot_path, corrupt).unwrap();

        let result = manager.restore_snapshot(id);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("hash mismatch"));
    }

    #[test]
    fn test_apply_snapshot_restores_config_and_challenges() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 3).unwrap();

        // Prepare snapshot contents
        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());
        let mut snapshot = Snapshot {
            meta: SnapshotMeta {
                id: uuid::Uuid::new_v4(),
                name: "apply_test".into(),
                block_height: 1,
                epoch: 1,
                state_hash: "hash".into(),
                created_at: Utc::now(),
                size_bytes: 0,
                auto: false,
                reason: "test".into(),
            },
            chain_state: bincode::serialize(&state).unwrap(),
            challenge_data: {
                let mut map = std::collections::HashMap::new();
                map.insert("challengeA".into(), b"placeholder".to_vec());
                map
            },
            config: br#"{"foo":"bar"}"#.to_vec(),
        };

        // Save the snapshot file and metadata index manually
        let snapshot_path = manager
            .snapshots_dir
            .join(format!("{}.snapshot", snapshot.meta.id));
        snapshot.meta.state_hash = SnapshotManager::compute_hash(&snapshot.chain_state);
        let bytes = bincode::serialize(&snapshot).unwrap();
        std::fs::write(&snapshot_path, &bytes).unwrap();
        snapshot.meta.size_bytes = bytes.len() as u64;
        manager.snapshots.push(snapshot.meta.clone());
        manager.save_snapshot_index().unwrap();

        let restored = manager.restore_snapshot(snapshot.meta.id).unwrap();
        let new_state = manager.apply_snapshot(&restored).unwrap();
        assert_eq!(new_state.block_height, state.block_height);

        // Config file should exist with snapshot contents
        let config_path = manager.data_dir.join("subnet_config.json");
        let config_contents = std::fs::read(&config_path).unwrap();
        assert_eq!(config_contents, br#"{"foo":"bar"}"#);

        // Challenge directory should be recreated with db contents
        let challenge_dir = manager.data_dir.join("challenges").join("challengeA");
        let db_path = challenge_dir.join("db");
        let data_file = db_path.join("data.bin");
        assert!(db_path.exists());
        assert_eq!(std::fs::read(data_file).unwrap(), b"placeholder".to_vec());
    }

    #[test]
    fn test_deserialize_directory_creates_structure() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("nested").join("db");

        SnapshotManager::deserialize_directory(&target, b"hello").unwrap();

        let data_path = target.join("data.bin");
        assert!(target.exists());
        assert!(data_path.exists());
        assert_eq!(std::fs::read(data_path).unwrap(), b"hello");
    }

    #[test]
    fn test_snapshot_meta_fields() {
        let meta = SnapshotMeta {
            id: uuid::Uuid::new_v4(),
            name: "test_snapshot".into(),
            block_height: 1000,
            epoch: 10,
            state_hash: "abc123".into(),
            created_at: Utc::now(),
            size_bytes: 1024,
            auto: true,
            reason: "Auto snapshot".into(),
        };

        assert_eq!(meta.name, "test_snapshot");
        assert_eq!(meta.block_height, 1000);
        assert_eq!(meta.epoch, 10);
        assert!(meta.auto);
    }

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

    #[test]
    fn test_manual_snapshot() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        let id = manager
            .create_snapshot(
                "manual_backup",
                500,
                5,
                &state,
                "Manual backup before update",
                false,
            )
            .unwrap();

        let snapshots = manager.list_snapshots();
        assert_eq!(snapshots.len(), 1);

        let meta = &snapshots[0];
        assert!(!meta.auto);
        assert_eq!(meta.reason, "Manual backup before update");
    }

    #[test]
    fn test_auto_snapshot() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        let id = manager
            .create_snapshot(
                "auto_snapshot_epoch_10",
                1000,
                10,
                &state,
                "Automatic snapshot",
                true,
            )
            .unwrap();

        let snapshots = manager.list_snapshots();
        let meta = &snapshots[0];
        assert!(meta.auto);
    }

    #[test]
    fn test_snapshot_with_challenge_data() {
        let dir = tempdir().unwrap();

        // Create a challenges directory with some data
        let challenges_dir = dir.path().join("challenges");
        std::fs::create_dir_all(&challenges_dir).unwrap();

        let challenge_dir = challenges_dir.join("test_challenge");
        std::fs::create_dir_all(&challenge_dir).unwrap();
        let db_dir = challenge_dir.join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("test.dat"), b"test data").unwrap();

        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 3).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        let id = manager
            .create_snapshot("with_challenges", 100, 1, &state, "test", false)
            .unwrap();

        let snapshot = manager.restore_snapshot(id).unwrap();
        assert!(!snapshot.challenge_data.is_empty());
    }

    #[test]
    fn test_snapshot_list_ordering() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 10).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        // Create multiple snapshots
        for i in 0..5 {
            manager
                .create_snapshot(&format!("snap_{}", i), i * 100, i, &state, "test", true)
                .unwrap();
        }

        let snapshots = manager.list_snapshots();
        assert_eq!(snapshots.len(), 5);

        // Verify they're tracked
        for i in 0..5 {
            assert_eq!(snapshots[i].block_height, (i * 100) as u64);
        }
    }

    #[test]
    fn test_snapshot_size_tracking() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 3).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        let id = manager
            .create_snapshot("size_test", 100, 1, &state, "test", false)
            .unwrap();

        let snapshots = manager.list_snapshots();
        let meta = &snapshots[0];

        // Size should be set after saving
        assert!(meta.size_bytes > 0);
    }

    #[test]
    fn test_snapshot_hash_computation() {
        let data = b"test data for hashing";
        let hash = SnapshotManager::compute_hash(data);

        // SHA256 hash should be 64 hex characters
        assert_eq!(hash.len(), 64);

        // Same data should produce same hash
        let hash2 = SnapshotManager::compute_hash(data);
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_delete_snapshot() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        let id = manager
            .create_snapshot("to_delete", 100, 1, &state, "test", false)
            .unwrap();

        assert_eq!(manager.list_snapshots().len(), 1);

        manager.delete_snapshot(id).unwrap();
        assert_eq!(manager.list_snapshots().len(), 0);
    }

    #[test]
    fn test_latest_snapshot() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        assert!(manager.latest_snapshot().is_none());

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        manager
            .create_snapshot("snap1", 100, 1, &state, "test", false)
            .unwrap();
        manager
            .create_snapshot("snap2", 200, 2, &state, "test", false)
            .unwrap();

        let latest = manager.latest_snapshot().unwrap();
        assert_eq!(latest.name, "snap2");
        assert_eq!(latest.block_height, 200);
    }

    #[test]
    fn test_get_snapshot() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        let id = manager
            .create_snapshot("test_snap", 100, 1, &state, "test", false)
            .unwrap();

        let snapshot = manager.get_snapshot(id).unwrap();
        assert_eq!(snapshot.name, "test_snap");
        assert_eq!(snapshot.block_height, 100);
    }

    #[test]
    fn test_get_nonexistent_snapshot() {
        let dir = tempdir().unwrap();
        let manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let result = manager.get_snapshot(uuid::Uuid::new_v4());
        assert!(result.is_none());
    }

    #[test]
    fn test_snapshot_retention_limit() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 3).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        // Create 5 snapshots with retention limit of 3
        for i in 0..5 {
            manager
                .create_snapshot(
                    &format!("snap{}", i),
                    (i + 1) * 100,
                    i + 1,
                    &state,
                    "test",
                    false,
                )
                .unwrap();
        }

        // Should only keep the 3 most recent
        let snapshots = manager.list_snapshots();
        assert_eq!(snapshots.len(), 3);

        // Just verify we have exactly 3 snapshots (pruning worked)
        // The exact order/names may vary based on pruning implementation
    }

    #[test]
    fn test_snapshot_metadata_fields() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        let id = manager
            .create_snapshot("metadata_test", 12345, 67, &state, "test reason", true)
            .unwrap();

        let snapshot = manager.get_snapshot(id).unwrap();
        assert_eq!(snapshot.name, "metadata_test");
        assert_eq!(snapshot.block_height, 12345);
        assert_eq!(snapshot.epoch, 67);
        assert_eq!(snapshot.reason, "test reason");
        assert!(snapshot.auto);
        assert!(snapshot.size_bytes > 0);
    }

    #[test]
    fn test_delete_nonexistent_snapshot() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        // Deleting nonexistent snapshot should succeed (no-op)
        let result = manager.delete_snapshot(uuid::Uuid::new_v4());
        assert!(result.is_ok());
    }

    #[test]
    fn test_snapshot_ordering_by_time() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 10).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        // Create snapshots in order
        for i in 0..3 {
            manager
                .create_snapshot(
                    &format!("snap{}", i),
                    (i + 1) * 100,
                    i + 1,
                    &state,
                    "test",
                    false,
                )
                .unwrap();
        }

        let snapshots = manager.list_snapshots();
        assert_eq!(snapshots.len(), 3);

        // Verify deterministic ordering without relying on timing jitter
        let names: Vec<&str> = snapshots.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["snap0", "snap1", "snap2"]);
    }

    // ============================================================================
    // Challenge Snapshot Tests
    // ============================================================================

    #[test]
    fn test_challenge_snapshot_struct_fields() {
        let snapshot = ChallengeStateSnapshot {
            id: uuid::Uuid::new_v4(),
            challenge_id: "test_challenge".into(),
            crate_version: "1.0.0".into(),
            state_data: vec![1, 2, 3, 4],
            pending_evaluations: vec![],
            reason: "pre_update".into(),
            created_at: Utc::now(),
            state_hash: "abc123".into(),
        };

        assert_eq!(snapshot.challenge_id, "test_challenge");
        assert_eq!(snapshot.crate_version, "1.0.0");
        assert_eq!(snapshot.state_data, vec![1, 2, 3, 4]);
        assert_eq!(snapshot.reason, "pre_update");
    }

    #[test]
    fn test_challenge_evaluation_state_struct() {
        let eval_state = ChallengeEvaluationState {
            request_id: "req-123".into(),
            participant_id: "participant-456".into(),
            progress: 75,
            data: r#"{"score": 100}"#.into(),
            started_at: Utc::now(),
        };

        assert_eq!(eval_state.request_id, "req-123");
        assert_eq!(eval_state.participant_id, "participant-456");
        assert_eq!(eval_state.progress, 75);
        
        // Parse the data as JSON to verify
        let parsed: serde_json::Value = serde_json::from_str(&eval_state.data).unwrap();
        assert_eq!(parsed["score"], 100);
    }

    #[test]
    fn test_challenge_snapshot_meta_struct() {
        let meta = ChallengeSnapshotMeta {
            id: uuid::Uuid::new_v4(),
            challenge_id: "my_challenge".into(),
            crate_version: "2.0.0".into(),
            reason: "scheduled".into(),
            created_at: Utc::now(),
            size_bytes: 4096,
            pending_count: 5,
        };

        assert_eq!(meta.challenge_id, "my_challenge");
        assert_eq!(meta.crate_version, "2.0.0");
        assert_eq!(meta.reason, "scheduled");
        assert_eq!(meta.size_bytes, 4096);
        assert_eq!(meta.pending_count, 5);
    }

    #[test]
    fn test_create_challenge_snapshot() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let state_data = b"serialized_challenge_state".to_vec();
        let pending_evaluations = vec![ChallengeEvaluationState {
            request_id: "req-001".into(),
            participant_id: "user-123".into(),
            progress: 50,
            data: r#"{"task": "solve_puzzle"}"#.into(),
            started_at: Utc::now(),
        }];

        let id = manager
            .create_challenge_snapshot(
                "challenge_alpha",
                "1.2.3",
                state_data.clone(),
                pending_evaluations,
                "pre_update",
            )
            .unwrap();

        // Verify metadata was added
        let snapshots = manager.list_challenge_snapshots(None);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].id, id);
        assert_eq!(snapshots[0].challenge_id, "challenge_alpha");
        assert_eq!(snapshots[0].crate_version, "1.2.3");
        assert_eq!(snapshots[0].reason, "pre_update");
        assert_eq!(snapshots[0].pending_count, 1);
        assert!(snapshots[0].size_bytes > 0);
    }

    #[test]
    fn test_restore_challenge_snapshot() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let state_data = b"important_state_data".to_vec();
        let pending_evaluations = vec![
            ChallengeEvaluationState {
                request_id: "req-001".into(),
                participant_id: "user-123".into(),
                progress: 30,
                data: "{}".into(),
                started_at: Utc::now(),
            },
            ChallengeEvaluationState {
                request_id: "req-002".into(),
                participant_id: "user-456".into(),
                progress: 80,
                data: r#"{"status": "almost_done"}"#.into(),
                started_at: Utc::now(),
            },
        ];

        let id = manager
            .create_challenge_snapshot(
                "challenge_beta",
                "2.0.0",
                state_data.clone(),
                pending_evaluations,
                "manual",
            )
            .unwrap();

        // Restore the snapshot
        let restored = manager.restore_challenge_snapshot(id).unwrap();

        assert_eq!(restored.id, id);
        assert_eq!(restored.challenge_id, "challenge_beta");
        assert_eq!(restored.crate_version, "2.0.0");
        assert_eq!(restored.state_data, state_data);
        assert_eq!(restored.pending_evaluations.len(), 2);
        assert_eq!(restored.reason, "manual");
    }

    #[test]
    fn test_restore_challenge_snapshot_not_found() {
        let dir = tempdir().unwrap();
        let manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let result = manager.restore_challenge_snapshot(uuid::Uuid::new_v4());
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("not found"));
    }

    #[test]
    fn test_restore_challenge_snapshot_hash_mismatch() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let state_data = b"original_state".to_vec();
        let id = manager
            .create_challenge_snapshot("challenge_corrupt", "1.0.0", state_data, vec![], "test")
            .unwrap();

        // Corrupt the snapshot by modifying the state_hash in the file
        let snapshot_path = dir
            .path()
            .join("challenge_snapshots")
            .join(format!("{}.challenge_snapshot", id));
        let bytes = std::fs::read(&snapshot_path).unwrap();
        let mut snapshot: ChallengeStateSnapshot = bincode::deserialize(&bytes).unwrap();
        snapshot.state_hash = "corrupted_hash".into();
        let corrupt_bytes = bincode::serialize(&snapshot).unwrap();
        std::fs::write(&snapshot_path, corrupt_bytes).unwrap();

        let result = manager.restore_challenge_snapshot(id);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("hash mismatch"));
    }

    #[test]
    fn test_list_challenge_snapshots_all() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 10).unwrap();

        // Create snapshots for different challenges
        manager
            .create_challenge_snapshot("challenge_a", "1.0.0", vec![1], vec![], "test")
            .unwrap();
        manager
            .create_challenge_snapshot("challenge_b", "1.0.0", vec![2], vec![], "test")
            .unwrap();
        manager
            .create_challenge_snapshot("challenge_a", "1.1.0", vec![3], vec![], "update")
            .unwrap();

        let all_snapshots = manager.list_challenge_snapshots(None);
        assert_eq!(all_snapshots.len(), 3);
    }

    #[test]
    fn test_list_challenge_snapshots_filtered() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 10).unwrap();

        // Create snapshots for different challenges
        manager
            .create_challenge_snapshot("challenge_a", "1.0.0", vec![1], vec![], "test")
            .unwrap();
        manager
            .create_challenge_snapshot("challenge_b", "1.0.0", vec![2], vec![], "test")
            .unwrap();
        manager
            .create_challenge_snapshot("challenge_a", "1.1.0", vec![3], vec![], "update")
            .unwrap();

        // Filter by challenge_a
        let challenge_a_snapshots = manager.list_challenge_snapshots(Some("challenge_a"));
        assert_eq!(challenge_a_snapshots.len(), 2);
        assert!(challenge_a_snapshots
            .iter()
            .all(|s| s.challenge_id == "challenge_a"));

        // Filter by challenge_b
        let challenge_b_snapshots = manager.list_challenge_snapshots(Some("challenge_b"));
        assert_eq!(challenge_b_snapshots.len(), 1);
        assert_eq!(challenge_b_snapshots[0].challenge_id, "challenge_b");

        // Filter by non-existent challenge
        let empty = manager.list_challenge_snapshots(Some("challenge_c"));
        assert!(empty.is_empty());
    }

    #[test]
    fn test_delete_challenge_snapshot() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let id = manager
            .create_challenge_snapshot("challenge_to_delete", "1.0.0", vec![1, 2, 3], vec![], "test")
            .unwrap();

        assert_eq!(manager.list_challenge_snapshots(None).len(), 1);

        // Delete the snapshot
        manager.delete_challenge_snapshot(id).unwrap();

        assert_eq!(manager.list_challenge_snapshots(None).len(), 0);

        // Verify file is also deleted
        let snapshot_path = dir
            .path()
            .join("challenge_snapshots")
            .join(format!("{}.challenge_snapshot", id));
        assert!(!snapshot_path.exists());
    }

    #[test]
    fn test_delete_nonexistent_challenge_snapshot() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        // Deleting non-existent snapshot should succeed (no-op)
        let result = manager.delete_challenge_snapshot(uuid::Uuid::new_v4());
        assert!(result.is_ok());
    }

    #[test]
    fn test_latest_challenge_snapshot() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 10).unwrap();

        // No snapshots yet
        assert!(manager.latest_challenge_snapshot("challenge_x").is_none());

        // Create multiple snapshots for the same challenge
        manager
            .create_challenge_snapshot("challenge_x", "1.0.0", vec![1], vec![], "first")
            .unwrap();
        manager
            .create_challenge_snapshot("challenge_x", "1.1.0", vec![2], vec![], "second")
            .unwrap();
        manager
            .create_challenge_snapshot("challenge_y", "1.0.0", vec![3], vec![], "other")
            .unwrap();
        manager
            .create_challenge_snapshot("challenge_x", "1.2.0", vec![4], vec![], "third")
            .unwrap();

        // Get latest for challenge_x
        let latest = manager.latest_challenge_snapshot("challenge_x").unwrap();
        assert_eq!(latest.challenge_id, "challenge_x");
        assert_eq!(latest.crate_version, "1.2.0");
        assert_eq!(latest.reason, "third");

        // Get latest for challenge_y
        let latest_y = manager.latest_challenge_snapshot("challenge_y").unwrap();
        assert_eq!(latest_y.challenge_id, "challenge_y");
        assert_eq!(latest_y.crate_version, "1.0.0");

        // Non-existent challenge
        assert!(manager.latest_challenge_snapshot("challenge_z").is_none());
    }

    #[test]
    fn test_challenge_snapshot_persistence() {
        let dir = tempdir().unwrap();

        let id1;
        let id2;
        {
            let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();
            id1 = manager
                .create_challenge_snapshot("persistent_challenge", "1.0.0", vec![10, 20, 30], vec![], "persist_test")
                .unwrap();
            id2 = manager
                .create_challenge_snapshot("persistent_challenge", "1.1.0", vec![40, 50], vec![], "persist_test_2")
                .unwrap();
        }

        // Create a new manager instance and verify persistence
        let manager2 = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();
        let snapshots = manager2.list_challenge_snapshots(None);
        assert_eq!(snapshots.len(), 2);

        // Verify we can restore the snapshots
        let restored1 = manager2.restore_challenge_snapshot(id1).unwrap();
        assert_eq!(restored1.state_data, vec![10, 20, 30]);
        assert_eq!(restored1.crate_version, "1.0.0");

        let restored2 = manager2.restore_challenge_snapshot(id2).unwrap();
        assert_eq!(restored2.state_data, vec![40, 50]);
        assert_eq!(restored2.crate_version, "1.1.0");
    }

    #[test]
    fn test_challenge_snapshot_with_pending_evaluations() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let pending = vec![
            ChallengeEvaluationState {
                request_id: "eval-001".into(),
                participant_id: "miner-alice".into(),
                progress: 25,
                data: r#"{"task_id": "task_1", "intermediate_score": 50}"#.into(),
                started_at: Utc::now(),
            },
            ChallengeEvaluationState {
                request_id: "eval-002".into(),
                participant_id: "miner-bob".into(),
                progress: 90,
                data: r#"{"task_id": "task_2", "intermediate_score": 95}"#.into(),
                started_at: Utc::now(),
            },
        ];

        let id = manager
            .create_challenge_snapshot(
                "eval_challenge",
                "3.0.0",
                b"state_with_evals".to_vec(),
                pending,
                "pre_update",
            )
            .unwrap();

        // Verify metadata
        let snapshots = manager.list_challenge_snapshots(Some("eval_challenge"));
        assert_eq!(snapshots[0].pending_count, 2);

        // Restore and verify evaluations
        let restored = manager.restore_challenge_snapshot(id).unwrap();
        assert_eq!(restored.pending_evaluations.len(), 2);

        let eval1 = &restored.pending_evaluations[0];
        assert_eq!(eval1.request_id, "eval-001");
        assert_eq!(eval1.participant_id, "miner-alice");
        assert_eq!(eval1.progress, 25);
        let data1: serde_json::Value = serde_json::from_str(&eval1.data).unwrap();
        assert_eq!(data1["task_id"], "task_1");

        let eval2 = &restored.pending_evaluations[1];
        assert_eq!(eval2.request_id, "eval-002");
        assert_eq!(eval2.participant_id, "miner-bob");
        assert_eq!(eval2.progress, 90);
    }

    #[test]
    fn test_challenge_snapshot_empty_state() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let id = manager
            .create_challenge_snapshot("empty_challenge", "0.1.0", vec![], vec![], "initial")
            .unwrap();

        let restored = manager.restore_challenge_snapshot(id).unwrap();
        assert!(restored.state_data.is_empty());
        assert!(restored.pending_evaluations.is_empty());
    }

    #[test]
    fn test_challenge_snapshot_large_state_data() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        // Create a large state (1MB)
        let large_state: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();

        let id = manager
            .create_challenge_snapshot("large_challenge", "1.0.0", large_state.clone(), vec![], "large_test")
            .unwrap();

        let snapshots = manager.list_challenge_snapshots(Some("large_challenge"));
        assert!(snapshots[0].size_bytes > 1_000_000); // Should be at least 1MB

        let restored = manager.restore_challenge_snapshot(id).unwrap();
        assert_eq!(restored.state_data.len(), 1_000_000);
        assert_eq!(restored.state_data, large_state);
    }

    #[test]
    fn test_challenge_snapshot_version_tracking() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 10).unwrap();

        // Simulate version upgrades
        manager
            .create_challenge_snapshot("versioned_challenge", "1.0.0", vec![1], vec![], "initial")
            .unwrap();
        manager
            .create_challenge_snapshot("versioned_challenge", "1.0.1", vec![2], vec![], "patch")
            .unwrap();
        manager
            .create_challenge_snapshot("versioned_challenge", "1.1.0", vec![3], vec![], "minor")
            .unwrap();
        manager
            .create_challenge_snapshot("versioned_challenge", "2.0.0", vec![4], vec![], "major")
            .unwrap();

        let snapshots = manager.list_challenge_snapshots(Some("versioned_challenge"));
        let versions: Vec<&str> = snapshots.iter().map(|s| s.crate_version.as_str()).collect();
        assert_eq!(versions, vec!["1.0.0", "1.0.1", "1.1.0", "2.0.0"]);

        // Latest should be 2.0.0
        let latest = manager.latest_challenge_snapshot("versioned_challenge").unwrap();
        assert_eq!(latest.crate_version, "2.0.0");
    }

    #[test]
    fn test_challenge_snapshots_dir_creation() {
        let dir = tempdir().unwrap();
        let challenge_snapshots_dir = dir.path().join("challenge_snapshots");

        // Directory should not exist yet
        assert!(!challenge_snapshots_dir.exists());

        // Create manager - should create the directory
        let _manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        // Now it should exist
        assert!(challenge_snapshots_dir.exists());
    }

    #[test]
    fn test_challenge_snapshot_index_persistence() {
        let dir = tempdir().unwrap();

        // Create snapshots with first manager
        {
            let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();
            manager
                .create_challenge_snapshot("index_test", "1.0.0", vec![1], vec![], "first")
                .unwrap();
            manager
                .create_challenge_snapshot("index_test", "2.0.0", vec![2], vec![], "second")
                .unwrap();
        }

        // Verify index file exists
        let index_path = dir.path().join("challenge_snapshots").join("index.json");
        assert!(index_path.exists());

        // Load with new manager and verify index was loaded
        let manager2 = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();
        let snapshots = manager2.list_challenge_snapshots(Some("index_test"));
        assert_eq!(snapshots.len(), 2);
    }

    #[test]
    fn test_challenge_snapshot_does_not_affect_regular_snapshots() {
        let dir = tempdir().unwrap();
        let mut manager = SnapshotManager::new(dir.path().to_path_buf(), 5).unwrap();

        let kp = Keypair::generate();
        let state = ChainState::new(kp.hotkey(), NetworkConfig::default());

        // Create regular snapshot
        let regular_id = manager
            .create_snapshot("regular_snap", 100, 1, &state, "test", false)
            .unwrap();

        // Create challenge snapshot
        let challenge_id = manager
            .create_challenge_snapshot("isolated_challenge", "1.0.0", vec![1, 2, 3], vec![], "test")
            .unwrap();

        // Verify they are in separate lists
        assert_eq!(manager.list_snapshots().len(), 1);
        assert_eq!(manager.list_challenge_snapshots(None).len(), 1);

        // Deleting one should not affect the other
        manager.delete_challenge_snapshot(challenge_id).unwrap();
        assert_eq!(manager.list_snapshots().len(), 1);
        assert_eq!(manager.list_challenge_snapshots(None).len(), 0);

        manager.delete_snapshot(regular_id).unwrap();
        assert_eq!(manager.list_snapshots().len(), 0);
    }
}
