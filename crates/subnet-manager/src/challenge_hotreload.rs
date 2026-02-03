//! Challenge Hot-Reload System
//!
//! Manages hot-reload lifecycle for challenge crates:
//! 1. Pre-update: Create checkpoint, pause new evaluations
//! 2. Update: Load new crate version
//! 3. Post-update: Restore checkpoint, resume evaluations

use crate::{ChallengeEvaluationState, ChallengeStateSnapshot, SnapshotManager};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

/// Status of a challenge during hot-reload
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChallengeReloadStatus {
    /// Normal operation
    Running,
    /// Preparing for update (creating checkpoint)
    PreparingUpdate,
    /// Checkpoint created, waiting for new version
    AwaitingUpdate,
    /// New version loaded, restoring state
    Restoring,
    /// Update failed, rolled back
    RolledBack,
    /// Update completed successfully
    UpdateComplete,
}

/// Hot-reload event for tracking
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HotReloadEvent {
    /// Event ID
    pub id: uuid::Uuid,
    /// Challenge being updated
    pub challenge_id: String,
    /// Old version
    pub old_version: String,
    /// New version
    pub new_version: String,
    /// Event type
    pub event_type: HotReloadEventType,
    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Additional details
    pub details: String,
}

/// Types of hot-reload events
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HotReloadEventType {
    Started,
    CheckpointCreated,
    UpdateApplied,
    RestoreStarted,
    RestoreCompleted,
    RollbackInitiated,
    RollbackCompleted,
    Failed,
}

/// Configuration for hot-reload behavior
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HotReloadConfig {
    /// Maximum time to wait for challenge to become safe to update (seconds)
    pub max_wait_for_safe_secs: u64,
    /// Whether to force update even if challenge is not safe
    pub force_update_if_unsafe: bool,
    /// Maximum checkpoint age for restore (hours)
    pub max_checkpoint_age_hours: u64,
    /// Automatically create checkpoint before update
    pub auto_checkpoint: bool,
    /// Automatically restore from checkpoint after update
    pub auto_restore: bool,
    /// Retry count for failed restorations
    pub restore_retry_count: u32,
}

impl Default for HotReloadConfig {
    fn default() -> Self {
        Self {
            max_wait_for_safe_secs: 300, // 5 minutes
            force_update_if_unsafe: false,
            max_checkpoint_age_hours: 24,
            auto_checkpoint: true,
            auto_restore: true,
            restore_retry_count: 3,
        }
    }
}

/// State of a challenge in the hot-reload system
#[derive(Clone, Debug)]
pub struct ChallengeHotReloadState {
    /// Challenge identifier
    pub challenge_id: String,
    /// Current status
    pub status: ChallengeReloadStatus,
    /// Current version
    pub current_version: String,
    /// Pending version (if updating)
    pub pending_version: Option<String>,
    /// Checkpoint ID (if created for update)
    pub checkpoint_id: Option<uuid::Uuid>,
    /// Last update timestamp
    pub last_updated: chrono::DateTime<chrono::Utc>,
}

/// Result of a hot-reload operation
#[derive(Clone, Debug)]
pub struct HotReloadResult {
    /// Success status
    pub success: bool,
    /// Challenge ID
    pub challenge_id: String,
    /// Old version
    pub old_version: String,
    /// New version (if successful)
    pub new_version: Option<String>,
    /// Number of evaluations restored
    pub evaluations_restored: usize,
    /// Number of evaluations dropped (incompatible)
    pub evaluations_dropped: usize,
    /// Error message (if failed)
    pub error: Option<String>,
    /// Duration in milliseconds
    pub duration_ms: u64,
}

/// Hot-reload error types
#[derive(Debug, thiserror::Error)]
pub enum HotReloadError {
    #[error("Challenge not found: {0}")]
    ChallengeNotFound(String),

    #[error("Challenge not safe to update: {0}")]
    UnsafeToUpdate(String),

    #[error("Checkpoint creation failed: {0}")]
    CheckpointFailed(String),

    #[error("Checkpoint not found: {0}")]
    CheckpointNotFound(String),

    #[error("Checkpoint too old: age {actual_hours}h exceeds max {max_hours}h")]
    CheckpointTooOld { actual_hours: u64, max_hours: u64 },

    #[error("Restoration failed: {0}")]
    RestoreFailed(String),

    #[error("Version incompatible: {0}")]
    VersionIncompatible(String),

    #[error("Timeout waiting for safe state")]
    Timeout,

    #[error("Already updating: {0}")]
    AlreadyUpdating(String),

    #[error("Snapshot error: {0}")]
    SnapshotError(String),
}

/// Manager for challenge hot-reload operations
pub struct ChallengeHotReloadManager {
    /// Configuration
    config: HotReloadConfig,
    /// Snapshot manager for state persistence
    snapshots: Arc<RwLock<SnapshotManager>>,
    /// Challenge states
    challenges: Arc<RwLock<HashMap<String, ChallengeHotReloadState>>>,
    /// Event history
    events: Arc<RwLock<Vec<HotReloadEvent>>>,
    /// Data directory
    #[allow(dead_code)]
    data_dir: PathBuf,
}

impl ChallengeHotReloadManager {
    /// Create a new hot-reload manager
    pub fn new(
        config: HotReloadConfig,
        snapshots: Arc<RwLock<SnapshotManager>>,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            config,
            snapshots,
            challenges: Arc::new(RwLock::new(HashMap::new())),
            events: Arc::new(RwLock::new(Vec::new())),
            data_dir,
        }
    }

    /// Register a challenge for hot-reload management
    pub fn register_challenge(&self, challenge_id: &str, version: &str) {
        let state = ChallengeHotReloadState {
            challenge_id: challenge_id.to_string(),
            status: ChallengeReloadStatus::Running,
            current_version: version.to_string(),
            pending_version: None,
            checkpoint_id: None,
            last_updated: chrono::Utc::now(),
        };
        self.challenges
            .write()
            .insert(challenge_id.to_string(), state);
        info!(
            "Registered challenge {} v{} for hot-reload",
            challenge_id, version
        );
    }

    /// Unregister a challenge
    pub fn unregister_challenge(&self, challenge_id: &str) -> Option<ChallengeHotReloadState> {
        let removed = self.challenges.write().remove(challenge_id);
        if removed.is_some() {
            info!("Unregistered challenge {} from hot-reload", challenge_id);
        }
        removed
    }

    /// Get challenge state
    pub fn get_challenge_state(&self, challenge_id: &str) -> Option<ChallengeHotReloadState> {
        self.challenges.read().get(challenge_id).cloned()
    }

    /// List all registered challenges
    pub fn list_challenges(&self) -> Vec<ChallengeHotReloadState> {
        self.challenges.read().values().cloned().collect()
    }

    /// Prepare a challenge for update (Phase 1)
    /// Creates checkpoint and marks challenge as preparing for update
    pub async fn prepare_for_update(
        &self,
        challenge_id: &str,
        state_data: Vec<u8>,
        pending_evaluations: Vec<ChallengeEvaluationState>,
    ) -> Result<uuid::Uuid, HotReloadError> {
        // Check if challenge exists
        let mut challenges = self.challenges.write();
        let state = challenges
            .get_mut(challenge_id)
            .ok_or_else(|| HotReloadError::ChallengeNotFound(challenge_id.to_string()))?;

        // Check if already updating
        if state.status != ChallengeReloadStatus::Running {
            return Err(HotReloadError::AlreadyUpdating(challenge_id.to_string()));
        }

        // Update status
        state.status = ChallengeReloadStatus::PreparingUpdate;
        let version = state.current_version.clone();
        drop(challenges); // Release lock before async operation

        // Create checkpoint
        let checkpoint_id = {
            let mut snapshots = self.snapshots.write();
            snapshots
                .create_challenge_snapshot(
                    challenge_id,
                    &version,
                    state_data,
                    pending_evaluations,
                    "pre_update",
                )
                .map_err(|e| HotReloadError::CheckpointFailed(e.to_string()))?
        };

        // Update state with checkpoint ID
        let mut challenges = self.challenges.write();
        if let Some(state) = challenges.get_mut(challenge_id) {
            state.status = ChallengeReloadStatus::AwaitingUpdate;
            state.checkpoint_id = Some(checkpoint_id);
        }

        // Record event
        self.record_event(
            challenge_id,
            &version,
            &version,
            HotReloadEventType::CheckpointCreated,
            format!("Checkpoint {} created", checkpoint_id),
        );

        info!(
            "Challenge {} prepared for update, checkpoint {}",
            challenge_id, checkpoint_id
        );
        Ok(checkpoint_id)
    }

    /// Apply update and begin restoration (Phase 2)
    pub async fn apply_update(
        &self,
        challenge_id: &str,
        new_version: &str,
    ) -> Result<(), HotReloadError> {
        let mut challenges = self.challenges.write();
        let state = challenges
            .get_mut(challenge_id)
            .ok_or_else(|| HotReloadError::ChallengeNotFound(challenge_id.to_string()))?;

        if state.status != ChallengeReloadStatus::AwaitingUpdate {
            return Err(HotReloadError::AlreadyUpdating(format!(
                "Expected AwaitingUpdate, got {:?}",
                state.status
            )));
        }

        let old_version = state.current_version.clone();
        state.pending_version = Some(new_version.to_string());
        state.status = ChallengeReloadStatus::Restoring;

        self.record_event(
            challenge_id,
            &old_version,
            new_version,
            HotReloadEventType::UpdateApplied,
            "Update applied".to_string(),
        );

        info!(
            "Challenge {} update applied, now restoring state",
            challenge_id
        );
        Ok(())
    }

    /// Complete update with restoration results (Phase 3)
    pub async fn complete_update(
        &self,
        challenge_id: &str,
        new_version: &str,
        evaluations_restored: usize,
        evaluations_dropped: usize,
    ) -> Result<HotReloadResult, HotReloadError> {
        let start = std::time::Instant::now();

        let mut challenges = self.challenges.write();
        let state = challenges
            .get_mut(challenge_id)
            .ok_or_else(|| HotReloadError::ChallengeNotFound(challenge_id.to_string()))?;

        let old_version = state.current_version.clone();
        state.current_version = new_version.to_string();
        state.pending_version = None;
        state.status = ChallengeReloadStatus::UpdateComplete;
        state.last_updated = chrono::Utc::now();

        // Clear checkpoint ID as it's no longer needed
        state.checkpoint_id = None;

        self.record_event(
            challenge_id,
            &old_version,
            new_version,
            HotReloadEventType::RestoreCompleted,
            format!(
                "Restored {} evaluations, dropped {}",
                evaluations_restored, evaluations_dropped
            ),
        );

        let result = HotReloadResult {
            success: true,
            challenge_id: challenge_id.to_string(),
            old_version: old_version.clone(),
            new_version: Some(new_version.to_string()),
            evaluations_restored,
            evaluations_dropped,
            error: None,
            duration_ms: start.elapsed().as_millis() as u64,
        };

        info!(
            "Challenge {} hot-reload complete: {} -> {}",
            challenge_id, old_version, new_version
        );
        Ok(result)
    }

    /// Rollback a failed update
    pub async fn rollback_update(
        &self,
        challenge_id: &str,
        reason: &str,
    ) -> Result<(), HotReloadError> {
        let mut challenges = self.challenges.write();
        let state = challenges
            .get_mut(challenge_id)
            .ok_or_else(|| HotReloadError::ChallengeNotFound(challenge_id.to_string()))?;

        let version = state.current_version.clone();
        state.status = ChallengeReloadStatus::RolledBack;
        state.pending_version = None;

        self.record_event(
            challenge_id,
            &version,
            &version,
            HotReloadEventType::RollbackCompleted,
            reason.to_string(),
        );

        warn!("Challenge {} update rolled back: {}", challenge_id, reason);
        Ok(())
    }

    /// Resume normal operation after update or rollback
    pub fn resume_operation(&self, challenge_id: &str) -> Result<(), HotReloadError> {
        let mut challenges = self.challenges.write();
        let state = challenges
            .get_mut(challenge_id)
            .ok_or_else(|| HotReloadError::ChallengeNotFound(challenge_id.to_string()))?;

        state.status = ChallengeReloadStatus::Running;
        info!("Challenge {} resumed normal operation", challenge_id);
        Ok(())
    }

    /// Get checkpoint for restoration
    pub fn get_checkpoint(
        &self,
        challenge_id: &str,
    ) -> Result<ChallengeStateSnapshot, HotReloadError> {
        let challenges = self.challenges.read();
        let state = challenges
            .get(challenge_id)
            .ok_or_else(|| HotReloadError::ChallengeNotFound(challenge_id.to_string()))?;

        let checkpoint_id = state
            .checkpoint_id
            .ok_or_else(|| HotReloadError::CheckpointNotFound(challenge_id.to_string()))?;

        drop(challenges); // Release lock before accessing snapshots

        let snapshots = self.snapshots.read();
        snapshots
            .restore_challenge_snapshot(checkpoint_id)
            .map_err(|e| HotReloadError::SnapshotError(e.to_string()))
    }

    /// Record a hot-reload event
    fn record_event(
        &self,
        challenge_id: &str,
        old_version: &str,
        new_version: &str,
        event_type: HotReloadEventType,
        details: String,
    ) {
        let event = HotReloadEvent {
            id: uuid::Uuid::new_v4(),
            challenge_id: challenge_id.to_string(),
            old_version: old_version.to_string(),
            new_version: new_version.to_string(),
            event_type,
            timestamp: chrono::Utc::now(),
            details,
        };
        self.events.write().push(event);
    }

    /// Get recent events
    pub fn recent_events(&self, limit: usize) -> Vec<HotReloadEvent> {
        let events = self.events.read();
        events.iter().rev().take(limit).cloned().collect()
    }

    /// Get events for a specific challenge
    pub fn challenge_events(&self, challenge_id: &str) -> Vec<HotReloadEvent> {
        let events = self.events.read();
        events
            .iter()
            .filter(|e| e.challenge_id == challenge_id)
            .cloned()
            .collect()
    }

    /// Clear old events (keep last N)
    pub fn prune_events(&self, keep: usize) {
        let mut events = self.events.write();
        if events.len() > keep {
            let drain_count = events.len() - keep;
            events.drain(0..drain_count);
        }
    }

    /// Get configuration
    pub fn config(&self) -> &HotReloadConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_test_manager() -> (ChallengeHotReloadManager, tempfile::TempDir) {
        let dir = tempdir().expect("create temp dir");
        let snapshots = Arc::new(RwLock::new(
            SnapshotManager::new(dir.path().to_path_buf(), 5).expect("create snapshot manager"),
        ));
        let manager = ChallengeHotReloadManager::new(
            HotReloadConfig::default(),
            snapshots,
            dir.path().to_path_buf(),
        );
        (manager, dir)
    }

    #[test]
    fn test_register_and_list_challenges() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");

        let challenges = manager.list_challenges();
        assert_eq!(challenges.len(), 1);
        assert_eq!(challenges[0].challenge_id, "test-challenge");
        assert_eq!(challenges[0].current_version, "1.0.0");
        assert_eq!(challenges[0].status, ChallengeReloadStatus::Running);
    }

    #[test]
    fn test_unregister_challenge() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");
        let removed = manager.unregister_challenge("test-challenge");

        assert!(removed.is_some());
        assert_eq!(manager.list_challenges().len(), 0);
    }

    #[test]
    fn test_get_challenge_state() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");

        let state = manager.get_challenge_state("test-challenge");
        assert!(state.is_some());

        let state = manager.get_challenge_state("nonexistent");
        assert!(state.is_none());
    }

    #[tokio::test]
    async fn test_prepare_for_update() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");

        let pending = vec![ChallengeEvaluationState {
            request_id: "req-1".to_string(),
            participant_id: "miner-1".to_string(),
            progress: 50,
            data: "{}".to_string(),
            started_at: chrono::Utc::now(),
        }];

        let checkpoint_id = manager
            .prepare_for_update("test-challenge", vec![1, 2, 3], pending)
            .await
            .expect("prepare should succeed");

        let state = manager
            .get_challenge_state("test-challenge")
            .expect("challenge exists");
        assert_eq!(state.status, ChallengeReloadStatus::AwaitingUpdate);
        assert_eq!(state.checkpoint_id, Some(checkpoint_id));
    }

    #[tokio::test]
    async fn test_prepare_for_update_not_found() {
        let (manager, _dir) = create_test_manager();

        let result = manager
            .prepare_for_update("nonexistent", vec![], vec![])
            .await;
        assert!(matches!(result, Err(HotReloadError::ChallengeNotFound(_))));
    }

    #[tokio::test]
    async fn test_apply_update() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");
        manager
            .prepare_for_update("test-challenge", vec![], vec![])
            .await
            .expect("prepare");

        manager
            .apply_update("test-challenge", "2.0.0")
            .await
            .expect("apply update");

        let state = manager
            .get_challenge_state("test-challenge")
            .expect("exists");
        assert_eq!(state.status, ChallengeReloadStatus::Restoring);
        assert_eq!(state.pending_version, Some("2.0.0".to_string()));
    }

    #[tokio::test]
    async fn test_complete_update() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");
        manager
            .prepare_for_update("test-challenge", vec![], vec![])
            .await
            .expect("prepare");
        manager
            .apply_update("test-challenge", "2.0.0")
            .await
            .expect("apply");

        let result = manager
            .complete_update("test-challenge", "2.0.0", 5, 1)
            .await
            .expect("complete");

        assert!(result.success);
        assert_eq!(result.evaluations_restored, 5);
        assert_eq!(result.evaluations_dropped, 1);

        let state = manager
            .get_challenge_state("test-challenge")
            .expect("exists");
        assert_eq!(state.current_version, "2.0.0");
        assert_eq!(state.status, ChallengeReloadStatus::UpdateComplete);
    }

    #[tokio::test]
    async fn test_rollback_update() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");
        manager
            .prepare_for_update("test-challenge", vec![], vec![])
            .await
            .expect("prepare");

        manager
            .rollback_update("test-challenge", "test failure")
            .await
            .expect("rollback");

        let state = manager
            .get_challenge_state("test-challenge")
            .expect("exists");
        assert_eq!(state.status, ChallengeReloadStatus::RolledBack);
        assert_eq!(state.current_version, "1.0.0");
    }

    #[test]
    fn test_resume_operation() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");

        // Manually set a non-Running status
        {
            let mut challenges = manager.challenges.write();
            let state = challenges.get_mut("test-challenge").expect("exists");
            state.status = ChallengeReloadStatus::UpdateComplete;
        }

        manager.resume_operation("test-challenge").expect("resume");

        let state = manager
            .get_challenge_state("test-challenge")
            .expect("exists");
        assert_eq!(state.status, ChallengeReloadStatus::Running);
    }

    #[tokio::test]
    async fn test_get_checkpoint() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");
        manager
            .prepare_for_update("test-challenge", vec![1, 2, 3], vec![])
            .await
            .expect("prepare");

        let checkpoint = manager
            .get_checkpoint("test-challenge")
            .expect("get checkpoint");
        assert_eq!(checkpoint.challenge_id, "test-challenge");
        assert_eq!(checkpoint.state_data, vec![1, 2, 3]);
    }

    #[test]
    fn test_events() {
        let (manager, _dir) = create_test_manager();

        manager.record_event(
            "challenge-1",
            "1.0",
            "2.0",
            HotReloadEventType::Started,
            "test".to_string(),
        );
        manager.record_event(
            "challenge-1",
            "1.0",
            "2.0",
            HotReloadEventType::UpdateApplied,
            "done".to_string(),
        );

        let events = manager.recent_events(10);
        assert_eq!(events.len(), 2);

        let challenge_events = manager.challenge_events("challenge-1");
        assert_eq!(challenge_events.len(), 2);
    }

    #[test]
    fn test_prune_events() {
        let (manager, _dir) = create_test_manager();

        for i in 0..10 {
            manager.record_event(
                &format!("challenge-{}", i),
                "1.0",
                "2.0",
                HotReloadEventType::Started,
                "test".to_string(),
            );
        }

        manager.prune_events(5);

        let events = manager.recent_events(100);
        assert_eq!(events.len(), 5);
    }

    #[test]
    fn test_config_defaults() {
        let config = HotReloadConfig::default();
        assert_eq!(config.max_wait_for_safe_secs, 300);
        assert!(!config.force_update_if_unsafe);
        assert_eq!(config.max_checkpoint_age_hours, 24);
        assert!(config.auto_checkpoint);
        assert!(config.auto_restore);
        assert_eq!(config.restore_retry_count, 3);
    }

    #[test]
    fn test_hot_reload_error_display() {
        let err = HotReloadError::CheckpointTooOld {
            actual_hours: 48,
            max_hours: 24,
        };
        assert!(err.to_string().contains("48h"));
        assert!(err.to_string().contains("24h"));
    }

    #[test]
    fn test_challenge_reload_status_serialization() {
        let statuses = vec![
            ChallengeReloadStatus::Running,
            ChallengeReloadStatus::PreparingUpdate,
            ChallengeReloadStatus::AwaitingUpdate,
            ChallengeReloadStatus::Restoring,
            ChallengeReloadStatus::RolledBack,
            ChallengeReloadStatus::UpdateComplete,
        ];

        for status in statuses {
            let json = serde_json::to_string(&status).expect("serialize");
            let restored: ChallengeReloadStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(status, restored);
        }
    }

    #[test]
    fn test_hot_reload_event_serialization() {
        let event = HotReloadEvent {
            id: uuid::Uuid::new_v4(),
            challenge_id: "test".to_string(),
            old_version: "1.0".to_string(),
            new_version: "2.0".to_string(),
            event_type: HotReloadEventType::Started,
            timestamp: chrono::Utc::now(),
            details: "test event".to_string(),
        };

        let json = serde_json::to_string(&event).expect("serialize");
        let restored: HotReloadEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event.id, restored.id);
    }

    #[tokio::test]
    async fn test_prepare_for_update_already_updating() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");
        manager
            .prepare_for_update("test-challenge", vec![], vec![])
            .await
            .expect("first prepare should succeed");

        // Second prepare should fail
        let result = manager
            .prepare_for_update("test-challenge", vec![], vec![])
            .await;
        assert!(matches!(result, Err(HotReloadError::AlreadyUpdating(_))));
    }

    #[tokio::test]
    async fn test_apply_update_wrong_state() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");

        // Try to apply update without preparing first
        let result = manager.apply_update("test-challenge", "2.0.0").await;
        assert!(matches!(result, Err(HotReloadError::AlreadyUpdating(_))));
    }

    #[test]
    fn test_get_checkpoint_not_found() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("test-challenge", "1.0.0");

        // No checkpoint exists yet
        let result = manager.get_checkpoint("test-challenge");
        assert!(matches!(result, Err(HotReloadError::CheckpointNotFound(_))));
    }

    #[test]
    fn test_get_checkpoint_challenge_not_found() {
        let (manager, _dir) = create_test_manager();

        let result = manager.get_checkpoint("nonexistent");
        assert!(matches!(result, Err(HotReloadError::ChallengeNotFound(_))));
    }

    #[test]
    fn test_resume_operation_not_found() {
        let (manager, _dir) = create_test_manager();

        let result = manager.resume_operation("nonexistent");
        assert!(matches!(result, Err(HotReloadError::ChallengeNotFound(_))));
    }

    #[tokio::test]
    async fn test_rollback_update_not_found() {
        let (manager, _dir) = create_test_manager();

        let result = manager.rollback_update("nonexistent", "test").await;
        assert!(matches!(result, Err(HotReloadError::ChallengeNotFound(_))));
    }

    #[tokio::test]
    async fn test_complete_update_not_found() {
        let (manager, _dir) = create_test_manager();

        let result = manager.complete_update("nonexistent", "2.0.0", 0, 0).await;
        assert!(matches!(result, Err(HotReloadError::ChallengeNotFound(_))));
    }

    #[tokio::test]
    async fn test_apply_update_not_found() {
        let (manager, _dir) = create_test_manager();

        let result = manager.apply_update("nonexistent", "2.0.0").await;
        assert!(matches!(result, Err(HotReloadError::ChallengeNotFound(_))));
    }

    #[test]
    fn test_hot_reload_result_fields() {
        let result = HotReloadResult {
            success: true,
            challenge_id: "test".to_string(),
            old_version: "1.0.0".to_string(),
            new_version: Some("2.0.0".to_string()),
            evaluations_restored: 10,
            evaluations_dropped: 2,
            error: None,
            duration_ms: 150,
        };

        assert!(result.success);
        assert_eq!(result.challenge_id, "test");
        assert_eq!(result.old_version, "1.0.0");
        assert_eq!(result.new_version, Some("2.0.0".to_string()));
        assert_eq!(result.evaluations_restored, 10);
        assert_eq!(result.evaluations_dropped, 2);
        assert!(result.error.is_none());
        assert_eq!(result.duration_ms, 150);
    }

    #[test]
    fn test_hot_reload_result_with_error() {
        let result = HotReloadResult {
            success: false,
            challenge_id: "test".to_string(),
            old_version: "1.0.0".to_string(),
            new_version: None,
            evaluations_restored: 0,
            evaluations_dropped: 0,
            error: Some("Failed to load new version".to_string()),
            duration_ms: 50,
        };

        assert!(!result.success);
        assert!(result.error.is_some());
        assert_eq!(result.error.unwrap(), "Failed to load new version");
    }

    #[test]
    fn test_register_multiple_challenges() {
        let (manager, _dir) = create_test_manager();

        manager.register_challenge("challenge-1", "1.0.0");
        manager.register_challenge("challenge-2", "2.0.0");
        manager.register_challenge("challenge-3", "3.0.0");

        let challenges = manager.list_challenges();
        assert_eq!(challenges.len(), 3);

        // Check each challenge is registered
        assert!(manager.get_challenge_state("challenge-1").is_some());
        assert!(manager.get_challenge_state("challenge-2").is_some());
        assert!(manager.get_challenge_state("challenge-3").is_some());
    }

    #[test]
    fn test_events_ordered_by_time() {
        let (manager, _dir) = create_test_manager();

        manager.record_event(
            "challenge-1",
            "1.0",
            "2.0",
            HotReloadEventType::Started,
            "first".to_string(),
        );
        manager.record_event(
            "challenge-1",
            "1.0",
            "2.0",
            HotReloadEventType::CheckpointCreated,
            "second".to_string(),
        );
        manager.record_event(
            "challenge-1",
            "1.0",
            "2.0",
            HotReloadEventType::UpdateApplied,
            "third".to_string(),
        );

        // recent_events returns in reverse order (most recent first)
        let events = manager.recent_events(10);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].details, "third");
        assert_eq!(events[1].details, "second");
        assert_eq!(events[2].details, "first");
    }

    #[test]
    fn test_challenge_events_filtered() {
        let (manager, _dir) = create_test_manager();

        manager.record_event(
            "challenge-1",
            "1.0",
            "2.0",
            HotReloadEventType::Started,
            "c1 event".to_string(),
        );
        manager.record_event(
            "challenge-2",
            "1.0",
            "2.0",
            HotReloadEventType::Started,
            "c2 event".to_string(),
        );
        manager.record_event(
            "challenge-1",
            "1.0",
            "2.0",
            HotReloadEventType::UpdateApplied,
            "c1 event 2".to_string(),
        );

        let c1_events = manager.challenge_events("challenge-1");
        assert_eq!(c1_events.len(), 2);
        assert!(c1_events.iter().all(|e| e.challenge_id == "challenge-1"));

        let c2_events = manager.challenge_events("challenge-2");
        assert_eq!(c2_events.len(), 1);
        assert_eq!(c2_events[0].challenge_id, "challenge-2");
    }

    #[test]
    fn test_all_error_variants_display() {
        let errors = vec![
            HotReloadError::ChallengeNotFound("test".to_string()),
            HotReloadError::UnsafeToUpdate("test".to_string()),
            HotReloadError::CheckpointFailed("test".to_string()),
            HotReloadError::CheckpointNotFound("test".to_string()),
            HotReloadError::CheckpointTooOld {
                actual_hours: 48,
                max_hours: 24,
            },
            HotReloadError::RestoreFailed("test".to_string()),
            HotReloadError::VersionIncompatible("test".to_string()),
            HotReloadError::Timeout,
            HotReloadError::AlreadyUpdating("test".to_string()),
            HotReloadError::SnapshotError("test".to_string()),
        ];

        for err in errors {
            // Just verify Display trait works
            let _ = err.to_string();
        }
    }

    #[test]
    fn test_get_config() {
        let (manager, _dir) = create_test_manager();

        let config = manager.config();
        assert_eq!(config.max_wait_for_safe_secs, 300);
        assert!(!config.force_update_if_unsafe);
    }

    #[test]
    fn test_custom_config() {
        let dir = tempdir().expect("create temp dir");
        let snapshots = Arc::new(RwLock::new(
            SnapshotManager::new(dir.path().to_path_buf(), 5).expect("create snapshot manager"),
        ));

        let custom_config = HotReloadConfig {
            max_wait_for_safe_secs: 600,
            force_update_if_unsafe: true,
            max_checkpoint_age_hours: 48,
            auto_checkpoint: false,
            auto_restore: false,
            restore_retry_count: 5,
        };

        let manager = ChallengeHotReloadManager::new(
            custom_config.clone(),
            snapshots,
            dir.path().to_path_buf(),
        );

        let config = manager.config();
        assert_eq!(config.max_wait_for_safe_secs, 600);
        assert!(config.force_update_if_unsafe);
        assert_eq!(config.max_checkpoint_age_hours, 48);
        assert!(!config.auto_checkpoint);
        assert!(!config.auto_restore);
        assert_eq!(config.restore_retry_count, 5);
    }

    #[tokio::test]
    async fn test_full_hot_reload_cycle() {
        let (manager, _dir) = create_test_manager();

        // Register challenge
        manager.register_challenge("full-cycle-test", "1.0.0");

        // Prepare for update
        let pending = vec![
            ChallengeEvaluationState {
                request_id: "req-1".to_string(),
                participant_id: "miner-1".to_string(),
                progress: 50,
                data: r#"{"task": "task1"}"#.to_string(),
                started_at: chrono::Utc::now(),
            },
            ChallengeEvaluationState {
                request_id: "req-2".to_string(),
                participant_id: "miner-2".to_string(),
                progress: 75,
                data: r#"{"task": "task2"}"#.to_string(),
                started_at: chrono::Utc::now(),
            },
        ];

        let checkpoint_id = manager
            .prepare_for_update("full-cycle-test", vec![1, 2, 3, 4, 5], pending)
            .await
            .expect("prepare should succeed");

        // Verify checkpoint can be retrieved
        let checkpoint = manager
            .get_checkpoint("full-cycle-test")
            .expect("checkpoint should exist");
        assert_eq!(checkpoint.state_data, vec![1, 2, 3, 4, 5]);
        assert_eq!(checkpoint.pending_evaluations.len(), 2);

        // Apply update
        manager
            .apply_update("full-cycle-test", "2.0.0")
            .await
            .expect("apply should succeed");

        // Complete update
        let result = manager
            .complete_update("full-cycle-test", "2.0.0", 2, 0)
            .await
            .expect("complete should succeed");

        assert!(result.success);
        assert_eq!(result.old_version, "1.0.0");
        assert_eq!(result.new_version, Some("2.0.0".to_string()));

        // Resume normal operation
        manager
            .resume_operation("full-cycle-test")
            .expect("resume should succeed");

        let final_state = manager
            .get_challenge_state("full-cycle-test")
            .expect("challenge should exist");
        assert_eq!(final_state.status, ChallengeReloadStatus::Running);
        assert_eq!(final_state.current_version, "2.0.0");
        assert!(final_state.checkpoint_id.is_none());

        // Verify events were recorded
        let events = manager.challenge_events("full-cycle-test");
        assert!(events.len() >= 2); // At least checkpoint created and restore completed
    }
}
