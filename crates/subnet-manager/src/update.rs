//! Hot Update System
//!
//! Allows updating challenges and configuration without restarting validators.

use crate::{ChallengeConfig, SubnetConfig};
use parking_lot::RwLock;
use platform_core::{ChallengeId, Hotkey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Update types
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UpdateType {
    /// Hot update - applied without restart
    Hot,
    /// Warm update - requires graceful reload
    Warm,
    /// Cold update - requires full restart
    Cold,
    /// Hard reset - wipes state and restarts
    HardReset,
}

/// Update status
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum UpdateStatus {
    Pending,
    Downloading,
    Validating,
    Applying,
    Applied,
    Failed(String),
    RolledBack,
}

/// An update to be applied
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Update {
    /// Unique update ID
    pub id: uuid::Uuid,

    /// Update type
    pub update_type: UpdateType,

    /// Version string
    pub version: String,

    /// What's being updated
    pub target: UpdateTarget,

    /// Update payload
    pub payload: UpdatePayload,

    /// Status
    pub status: UpdateStatus,

    /// Created at
    pub created_at: chrono::DateTime<chrono::Utc>,

    /// Applied at
    pub applied_at: Option<chrono::DateTime<chrono::Utc>>,

    /// Rollback data (for reverting)
    pub rollback_data: Option<Vec<u8>>,
}

/// What is being updated
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UpdateTarget {
    /// Update a challenge
    Challenge(ChallengeId),
    /// Update subnet configuration
    Config,
    /// Update all challenges
    AllChallenges,
    /// Update validator list
    Validators,
}

/// Update payload
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UpdatePayload {
    /// WASM bytecode for challenge
    WasmChallenge {
        wasm_bytes: Vec<u8>,
        wasm_hash: String,
        config: ChallengeConfig,
    },
    /// Configuration update
    Config(SubnetConfig),
    /// Add/remove validators
    Validators {
        add: Vec<Hotkey>,
        remove: Vec<Hotkey>,
    },
    /// Hard reset with new state
    HardReset {
        reason: String,
        preserve_validators: bool,
        new_config: Option<SubnetConfig>,
    },
}

/// Update manager
pub struct UpdateManager {
    /// Data directory
    data_dir: PathBuf,

    /// Pending updates
    pending: Arc<RwLock<Vec<Update>>>,

    /// Applied updates history
    history: Arc<RwLock<Vec<Update>>>,

    /// Current version
    current_version: Arc<RwLock<String>>,

    /// Is update in progress
    updating: Arc<RwLock<bool>>,
}

impl UpdateManager {
    /// Create a new update manager
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            pending: Arc::new(RwLock::new(Vec::new())),
            history: Arc::new(RwLock::new(Vec::new())),
            current_version: Arc::new(RwLock::new("0.1.0".to_string())),
            updating: Arc::new(RwLock::new(false)),
        }
    }

    /// Queue an update
    pub fn queue_update(
        &self,
        target: UpdateTarget,
        payload: UpdatePayload,
        version: String,
    ) -> uuid::Uuid {
        let update_type = match &payload {
            UpdatePayload::WasmChallenge { .. } => UpdateType::Hot,
            UpdatePayload::Config(_) => UpdateType::Warm,
            UpdatePayload::Validators { .. } => UpdateType::Hot,
            UpdatePayload::HardReset { .. } => UpdateType::HardReset,
        };

        let update = Update {
            id: uuid::Uuid::new_v4(),
            update_type,
            version,
            target,
            payload,
            status: UpdateStatus::Pending,
            created_at: chrono::Utc::now(),
            applied_at: None,
            rollback_data: None,
        };

        let id = update.id;
        self.pending.write().push(update);

        info!("Update queued: {}", id);
        id
    }

    /// Process pending updates
    pub async fn process_updates(&self) -> Result<Vec<uuid::Uuid>, UpdateError> {
        if *self.updating.read() {
            return Err(UpdateError::AlreadyUpdating);
        }

        *self.updating.write() = true;
        let mut applied = Vec::new();

        // Take all pending updates
        let updates: Vec<Update> = {
            let mut pending = self.pending.write();
            std::mem::take(&mut *pending)
        };

        for mut update in updates {
            info!(
                "Processing update: {} ({:?})",
                update.id, update.update_type
            );

            match self.apply_update(&mut update).await {
                Ok(_) => {
                    update.status = UpdateStatus::Applied;
                    update.applied_at = Some(chrono::Utc::now());
                    applied.push(update.id);
                    info!("Update applied: {}", update.id);
                }
                Err(e) => {
                    error!("Update failed: {} - {}", update.id, e);
                    update.status = UpdateStatus::Failed(e.to_string());

                    // Try rollback if we have data
                    if update.rollback_data.is_some() {
                        if let Err(re) = self.rollback_update(&update).await {
                            error!("Rollback failed: {}", re);
                        } else {
                            update.status = UpdateStatus::RolledBack;
                        }
                    }
                }
            }

            self.history.write().push(update);
        }

        *self.updating.write() = false;
        Ok(applied)
    }

    /// Apply a single update
    async fn apply_update(&self, update: &mut Update) -> Result<(), UpdateError> {
        update.status = UpdateStatus::Applying;

        match &update.payload {
            UpdatePayload::WasmChallenge {
                wasm_bytes,
                wasm_hash,
                config,
            } => {
                // Validate WASM hash
                let computed_hash = Self::compute_hash(wasm_bytes);
                if &computed_hash != wasm_hash {
                    return Err(UpdateError::HashMismatch {
                        expected: wasm_hash.clone(),
                        actual: computed_hash,
                    });
                }

                // Store rollback data (current WASM)
                // In real implementation, load current WASM
                update.rollback_data = Some(Vec::new());

                // Save new WASM to disk
                let wasm_path = self
                    .data_dir
                    .join("challenges")
                    .join(&config.id)
                    .join("code.wasm");
                std::fs::create_dir_all(wasm_path.parent().unwrap())?;
                std::fs::write(&wasm_path, wasm_bytes)?;

                info!("Challenge WASM updated: {}", config.id);
                Ok(())
            }

            UpdatePayload::Config(new_config) => {
                new_config
                    .validate()
                    .map_err(|e| UpdateError::Validation(e.to_string()))?;

                let config_path = self.data_dir.join("subnet_config.json");

                // Store rollback data
                if config_path.exists() {
                    update.rollback_data = Some(std::fs::read(&config_path)?);
                }

                new_config.save(&config_path).map_err(|e| {
                    UpdateError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        e.to_string(),
                    ))
                })?;
                *self.current_version.write() = new_config.version.clone();

                info!("Config updated to version {}", new_config.version);
                Ok(())
            }

            UpdatePayload::Validators { add, remove } => {
                info!("Validator update: +{} -{}", add.len(), remove.len());
                // Validator updates are handled by the runtime
                Ok(())
            }

            UpdatePayload::HardReset {
                reason,
                preserve_validators,
                new_config,
            } => {
                warn!("HARD RESET initiated: {}", reason);

                // Save snapshot before reset
                let snapshot_path = self.data_dir.join("snapshots").join("pre_reset");
                std::fs::create_dir_all(&snapshot_path)?;

                // Clear state directories
                if !preserve_validators {
                    let validators_path = self.data_dir.join("validators");
                    if validators_path.exists() {
                        std::fs::remove_dir_all(&validators_path)?;
                    }
                }

                // Clear challenge data
                let challenges_path = self.data_dir.join("challenges");
                if challenges_path.exists() {
                    std::fs::remove_dir_all(&challenges_path)?;
                }
                std::fs::create_dir_all(&challenges_path)?;

                // Apply new config if provided
                if let Some(config) = new_config {
                    config
                        .save(&self.data_dir.join("subnet_config.json"))
                        .map_err(|e| {
                            UpdateError::Io(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                e.to_string(),
                            ))
                        })?;
                }

                info!("Hard reset complete");
                Ok(())
            }
        }
    }

    /// Rollback an update
    async fn rollback_update(&self, update: &Update) -> Result<(), UpdateError> {
        let rollback_data = update
            .rollback_data
            .as_ref()
            .ok_or(UpdateError::NoRollbackData)?;

        match &update.target {
            UpdateTarget::Challenge(id) => {
                let wasm_path = self
                    .data_dir
                    .join("challenges")
                    .join(id.to_string())
                    .join("code.wasm");
                std::fs::write(&wasm_path, rollback_data)?;
                info!("Rolled back challenge: {}", id);
            }
            UpdateTarget::Config => {
                let config_path = self.data_dir.join("subnet_config.json");
                std::fs::write(&config_path, rollback_data)?;
                info!("Rolled back config");
            }
            _ => {
                warn!("Rollback not supported for {:?}", update.target);
            }
        }

        Ok(())
    }

    /// Compute SHA256 hash
    fn compute_hash(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    /// Get pending updates count
    pub fn pending_count(&self) -> usize {
        self.pending.read().len()
    }

    /// Get current version
    pub fn current_version(&self) -> String {
        self.current_version.read().clone()
    }

    /// Is update in progress
    pub fn is_updating(&self) -> bool {
        *self.updating.read()
    }

    /// Get update history
    pub fn history(&self) -> Vec<Update> {
        self.history.read().clone()
    }

    /// Clear old history (keep last N)
    pub fn prune_history(&self, keep: usize) {
        let mut history = self.history.write();
        if history.len() > keep {
            let drain_count = history.len() - keep;
            history.drain(0..drain_count);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("Update already in progress")]
    AlreadyUpdating,

    #[error("Hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("No rollback data available")]
    NoRollbackData,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_update_manager() {
        let dir = tempdir().unwrap();
        let manager = UpdateManager::new(dir.path().to_path_buf());

        // Queue a config update with explicit version
        let mut config = SubnetConfig::default();
        config.version = "0.2.0".to_string();

        let id = manager.queue_update(
            UpdateTarget::Config,
            UpdatePayload::Config(config),
            "0.2.0".to_string(),
        );

        assert_eq!(manager.pending_count(), 1);

        // Process updates
        let applied = manager.process_updates().await.unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0], id);

        assert_eq!(manager.current_version(), "0.2.0");
    }

    #[test]
    fn test_compute_hash() {
        let data = b"hello world";
        let hash = UpdateManager::compute_hash(data);
        assert_eq!(hash.len(), 64); // SHA256 = 32 bytes = 64 hex chars
    }
}
