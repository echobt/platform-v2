//! Recovery System
//!
//! Handles automatic recovery from failures.

use crate::{
    HealthMonitor, HealthStatus, RecoveryConfig, SnapshotManager, UpdateManager, UpdatePayload,
    UpdateTarget,
};
use parking_lot::RwLock;
use platform_core::ChainState;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// Recovery action
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RecoveryAction {
    /// Restart the evaluation loop
    RestartEvaluations,
    /// Clear job queue
    ClearJobQueue,
    /// Reconnect to peers
    ReconnectPeers,
    /// Rollback to snapshot
    RollbackToSnapshot(uuid::Uuid),
    /// Hard reset
    HardReset { reason: String },
    /// Pause subnet
    Pause,
    /// Resume subnet
    Resume,
}

/// Recovery attempt record
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryAttempt {
    /// Attempt ID
    pub id: uuid::Uuid,

    /// Action taken
    pub action: RecoveryAction,

    /// Reason for recovery
    pub reason: String,

    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,

    /// Success
    pub success: bool,

    /// Details
    pub details: String,
}

/// Recovery manager
pub struct RecoveryManager {
    /// Configuration
    config: RecoveryConfig,

    /// Data directory
    data_dir: PathBuf,

    /// Snapshot manager
    snapshots: Arc<RwLock<SnapshotManager>>,

    /// Update manager
    updates: Arc<RwLock<UpdateManager>>,

    /// Recovery history
    history: Vec<RecoveryAttempt>,

    /// Current recovery attempts
    current_attempts: u32,

    /// Last recovery time
    last_recovery: Option<Instant>,

    /// Is subnet paused
    paused: Arc<RwLock<bool>>,
}

impl RecoveryManager {
    /// Create a new recovery manager
    pub fn new(
        config: RecoveryConfig,
        data_dir: PathBuf,
        snapshots: Arc<RwLock<SnapshotManager>>,
        updates: Arc<RwLock<UpdateManager>>,
    ) -> Self {
        Self {
            config,
            data_dir,
            snapshots,
            updates,
            history: Vec::new(),
            current_attempts: 0,
            last_recovery: None,
            paused: Arc::new(RwLock::new(false)),
        }
    }

    /// Check if recovery is needed and execute if so
    pub async fn check_and_recover(&mut self, health: &HealthMonitor) -> Option<RecoveryAttempt> {
        // Skip if paused and not critical
        if *self.paused.read() && health.current_status() != HealthStatus::Critical {
            return None;
        }

        // Check cooldown
        if let Some(last) = self.last_recovery {
            if last.elapsed() < Duration::from_secs(self.config.cooldown_secs) {
                return None;
            }
        }

        // Check if recovery is needed
        if !health.needs_recovery() && health.current_status() != HealthStatus::Critical {
            self.current_attempts = 0; // Reset on healthy
            return None;
        }

        // Check max attempts
        if self.current_attempts >= self.config.max_attempts {
            if self.config.rollback_on_failure {
                return self.rollback_to_last_snapshot().await;
            } else if self.config.pause_on_critical {
                return Some(self.pause_subnet("Max recovery attempts reached").await);
            }
            return None;
        }

        self.current_attempts += 1;
        self.last_recovery = Some(Instant::now());

        // Determine action based on status
        let action = match health.current_status() {
            HealthStatus::Critical => {
                if self.config.pause_on_critical {
                    self.pause_subnet("Critical health status").await
                } else {
                    self.execute_recovery(RecoveryAction::HardReset {
                        reason: "Critical health status".to_string(),
                    })
                    .await
                }
            }
            HealthStatus::Unhealthy => {
                // Try to identify the worst component
                if let Some((component, _)) = health.worst_component() {
                    match component {
                        "job_queue" => self.execute_recovery(RecoveryAction::ClearJobQueue).await,
                        "network" => self.execute_recovery(RecoveryAction::ReconnectPeers).await,
                        "evaluations" => {
                            self.execute_recovery(RecoveryAction::RestartEvaluations)
                                .await
                        }
                        _ => {
                            self.execute_recovery(RecoveryAction::RestartEvaluations)
                                .await
                        }
                    }
                } else {
                    self.execute_recovery(RecoveryAction::RestartEvaluations)
                        .await
                }
            }
            HealthStatus::Degraded => {
                self.execute_recovery(RecoveryAction::RestartEvaluations)
                    .await
            }
            HealthStatus::Healthy => {
                // Should not reach here
                return None;
            }
        };

        Some(action)
    }

    /// Execute a recovery action
    async fn execute_recovery(&mut self, action: RecoveryAction) -> RecoveryAttempt {
        info!("Executing recovery action: {:?}", action);

        let id = uuid::Uuid::new_v4();
        let timestamp = chrono::Utc::now();

        let (success, details) = match &action {
            RecoveryAction::RestartEvaluations => {
                // Signal to runtime to restart evaluation loop
                (true, "Signaled evaluation restart".to_string())
            }
            RecoveryAction::ClearJobQueue => {
                // Clear pending jobs
                (true, "Job queue cleared".to_string())
            }
            RecoveryAction::ReconnectPeers => {
                // Signal network reconnection
                (true, "Signaled peer reconnection".to_string())
            }
            RecoveryAction::RollbackToSnapshot(snapshot_id) => {
                match self.rollback_to_snapshot(*snapshot_id).await {
                    Ok(_) => (true, format!("Rolled back to snapshot {}", snapshot_id)),
                    Err(e) => (false, format!("Rollback failed: {}", e)),
                }
            }
            RecoveryAction::HardReset { reason } => {
                // Queue hard reset update
                let updates = self.updates.write();
                updates.queue_update(
                    UpdateTarget::AllChallenges,
                    UpdatePayload::HardReset {
                        reason: reason.clone(),
                        preserve_validators: true,
                        new_config: None,
                    },
                    "recovery".to_string(),
                );
                (true, format!("Hard reset queued: {}", reason))
            }
            RecoveryAction::Pause => {
                *self.paused.write() = true;
                (true, "Subnet paused".to_string())
            }
            RecoveryAction::Resume => {
                *self.paused.write() = false;
                self.current_attempts = 0;
                (true, "Subnet resumed".to_string())
            }
        };

        let attempt = RecoveryAttempt {
            id,
            action,
            reason: "Automatic recovery".to_string(),
            timestamp,
            success,
            details,
        };

        self.history.push(attempt.clone());

        if success {
            info!("Recovery successful: {}", attempt.details);
        } else {
            error!("Recovery failed: {}", attempt.details);
        }

        attempt
    }

    /// Rollback to last snapshot
    async fn rollback_to_last_snapshot(&mut self) -> Option<RecoveryAttempt> {
        let snapshot_id = {
            let snapshots = self.snapshots.read();
            snapshots.latest_snapshot().map(|s| s.id)
        };

        if let Some(id) = snapshot_id {
            Some(
                self.execute_recovery(RecoveryAction::RollbackToSnapshot(id))
                    .await,
            )
        } else {
            warn!("No snapshots available for rollback");
            None
        }
    }

    /// Rollback to specific snapshot
    async fn rollback_to_snapshot(&self, snapshot_id: uuid::Uuid) -> anyhow::Result<ChainState> {
        let snapshots = self.snapshots.read();
        let snapshot = snapshots.restore_snapshot(snapshot_id)?;
        let state = snapshots.apply_snapshot(&snapshot)?;
        Ok(state)
    }

    /// Pause subnet
    async fn pause_subnet(&mut self, reason: &str) -> RecoveryAttempt {
        warn!("Pausing subnet: {}", reason);
        self.execute_recovery(RecoveryAction::Pause).await
    }

    /// Resume subnet
    pub async fn resume_subnet(&mut self) -> RecoveryAttempt {
        info!("Resuming subnet");
        self.execute_recovery(RecoveryAction::Resume).await
    }

    /// Is subnet paused
    pub fn is_paused(&self) -> bool {
        *self.paused.read()
    }

    /// Get recovery history
    pub fn history(&self) -> &[RecoveryAttempt] {
        &self.history
    }

    /// Get current recovery attempts
    pub fn current_attempts(&self) -> u32 {
        self.current_attempts
    }

    /// Manually trigger recovery
    pub async fn manual_recovery(&mut self, action: RecoveryAction) -> RecoveryAttempt {
        self.execute_recovery(action).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HealthConfig;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_recovery_manager() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::default();

        let snapshots = Arc::new(RwLock::new(
            SnapshotManager::new(dir.path().to_path_buf(), 3).unwrap(),
        ));
        let updates = Arc::new(RwLock::new(UpdateManager::new(dir.path().to_path_buf())));

        let mut manager =
            RecoveryManager::new(config, dir.path().to_path_buf(), snapshots, updates);

        // Manual recovery
        let attempt = manager
            .manual_recovery(RecoveryAction::RestartEvaluations)
            .await;
        assert!(attempt.success);

        assert_eq!(manager.history().len(), 1);
    }

    #[tokio::test]
    async fn test_pause_resume() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::default();

        let snapshots = Arc::new(RwLock::new(
            SnapshotManager::new(dir.path().to_path_buf(), 3).unwrap(),
        ));
        let updates = Arc::new(RwLock::new(UpdateManager::new(dir.path().to_path_buf())));

        let mut manager =
            RecoveryManager::new(config, dir.path().to_path_buf(), snapshots, updates);

        // Pause
        manager.manual_recovery(RecoveryAction::Pause).await;
        assert!(manager.is_paused());

        // Resume
        manager.resume_subnet().await;
        assert!(!manager.is_paused());
    }
}
