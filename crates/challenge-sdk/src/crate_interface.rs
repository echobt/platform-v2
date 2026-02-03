//! Challenge Crate Interface
//!
//! Defines traits that challenge crates must implement for hot-reload support.
//! These traits enable state persistence, checkpointing, and seamless updates.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a challenge crate
pub type ChallengeCrateId = String;

/// Evaluation state that can be checkpointed
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationCheckpoint {
    /// Unique checkpoint ID
    pub id: Uuid,
    /// Challenge crate identifier
    pub challenge_id: ChallengeCrateId,
    /// Serialized evaluation state
    pub state_data: Vec<u8>,
    /// Pending evaluation requests
    pub pending_evaluations: Vec<PendingEvaluation>,
    /// Checkpoint timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Challenge crate version at checkpoint time
    pub crate_version: String,
}

/// A pending evaluation to be resumed after update
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingEvaluation {
    /// Request ID
    pub request_id: String,
    /// Participant/miner identifier
    pub participant_id: String,
    /// Evaluation data (challenge-specific)
    pub data: serde_json::Value,
    /// Progress percentage (0-100)
    pub progress: u8,
    /// Intermediate results (if any)
    pub intermediate_results: Option<serde_json::Value>,
    /// Started at timestamp
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Result of a checkpoint operation
#[derive(Clone, Debug)]
pub struct CheckpointResult {
    /// Checkpoint ID
    pub checkpoint_id: Uuid,
    /// Size of checkpoint data in bytes
    pub size_bytes: u64,
    /// Number of pending evaluations captured
    pub pending_count: usize,
}

/// Result of a restoration operation
#[derive(Clone, Debug)]
pub struct RestoreResult {
    /// Checkpoint ID that was restored
    pub checkpoint_id: Uuid,
    /// Number of evaluations resumed
    pub resumed_count: usize,
    /// Number of evaluations that couldn't be resumed (incompatible state)
    pub dropped_count: usize,
}

/// Error type for challenge crate operations
#[derive(Debug, thiserror::Error)]
pub enum ChallengeCrateError {
    #[error("Checkpoint creation failed: {0}")]
    CheckpointFailed(String),

    #[error("Restoration failed: {0}")]
    RestoreFailed(String),

    #[error("State serialization error: {0}")]
    SerializationError(String),

    #[error("Incompatible checkpoint version: expected {expected}, got {actual}")]
    IncompatibleVersion { expected: String, actual: String },

    #[error("Evaluation error: {0}")]
    EvaluationError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Trait for challenge crates that support hot-reload
///
/// Challenge crates implement this trait to enable:
/// - State persistence across updates
/// - Checkpoint creation before updates
/// - Restoration of pending evaluations after updates
#[async_trait]
pub trait HotReloadableChallenge: Send + Sync {
    /// Returns the unique challenge crate identifier
    fn challenge_id(&self) -> &str;

    /// Returns the challenge crate version
    fn version(&self) -> &str;

    /// Returns the minimum compatible version for checkpoint restoration
    /// Checkpoints from versions older than this cannot be restored
    fn min_compatible_version(&self) -> &str;

    /// Check if a checkpoint from given version can be restored
    fn is_version_compatible(&self, checkpoint_version: &str) -> bool {
        // Default: simple string comparison, override for semantic versioning
        checkpoint_version >= self.min_compatible_version()
    }

    /// Create a checkpoint of current state
    ///
    /// Called before a challenge crate update to preserve:
    /// - Internal state
    /// - Pending evaluations
    /// - Any in-progress work
    async fn create_checkpoint(&self) -> Result<EvaluationCheckpoint, ChallengeCrateError>;

    /// Restore state from a checkpoint
    ///
    /// Called after a challenge crate update to resume operations.
    /// Returns the number of evaluations that were successfully resumed.
    async fn restore_from_checkpoint(
        &mut self,
        checkpoint: EvaluationCheckpoint,
    ) -> Result<RestoreResult, ChallengeCrateError>;

    /// Get current pending evaluations count
    fn pending_evaluations_count(&self) -> usize;

    /// Check if challenge is ready for update (no critical operations in progress)
    fn is_safe_to_update(&self) -> bool;

    /// Prepare for update - called to gracefully pause new work
    async fn prepare_for_update(&mut self) -> Result<(), ChallengeCrateError>;

    /// Resume after update - called to resume normal operations
    async fn resume_after_update(&mut self) -> Result<(), ChallengeCrateError>;
}

/// Metadata about a challenge crate for registry
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChallengeCrateMetadata {
    /// Crate identifier
    pub id: ChallengeCrateId,
    /// Display name
    pub name: String,
    /// Version string
    pub version: String,
    /// Description
    pub description: String,
    /// Whether hot-reload is supported
    pub supports_hot_reload: bool,
    /// Minimum compatible version for checkpoint restoration
    pub min_compatible_version: String,
    /// Last updated timestamp
    pub last_updated: chrono::DateTime<chrono::Utc>,
}

/// Registry of challenge crates
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChallengeCrateRegistry {
    /// Registered challenge crates
    pub crates: std::collections::HashMap<ChallengeCrateId, ChallengeCrateMetadata>,
}

impl ChallengeCrateRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a challenge crate
    pub fn register(&mut self, metadata: ChallengeCrateMetadata) {
        self.crates.insert(metadata.id.clone(), metadata);
    }

    /// Unregister a challenge crate
    pub fn unregister(&mut self, id: &str) -> Option<ChallengeCrateMetadata> {
        self.crates.remove(id)
    }

    /// Get challenge crate metadata
    pub fn get(&self, id: &str) -> Option<&ChallengeCrateMetadata> {
        self.crates.get(id)
    }

    /// List all registered challenge crates
    pub fn list(&self) -> Vec<&ChallengeCrateMetadata> {
        self.crates.values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evaluation_checkpoint_serialization() {
        let checkpoint = EvaluationCheckpoint {
            id: Uuid::new_v4(),
            challenge_id: "test-challenge".to_string(),
            state_data: vec![1, 2, 3, 4],
            pending_evaluations: vec![],
            created_at: chrono::Utc::now(),
            crate_version: "1.0.0".to_string(),
        };

        let json = serde_json::to_string(&checkpoint).expect("serialization should work");
        let restored: EvaluationCheckpoint =
            serde_json::from_str(&json).expect("deserialization should work");

        assert_eq!(checkpoint.id, restored.id);
        assert_eq!(checkpoint.challenge_id, restored.challenge_id);
    }

    #[test]
    fn test_pending_evaluation_serialization() {
        let pending = PendingEvaluation {
            request_id: "req-123".to_string(),
            participant_id: "miner-abc".to_string(),
            data: serde_json::json!({"task": "test"}),
            progress: 50,
            intermediate_results: Some(serde_json::json!({"partial": true})),
            started_at: chrono::Utc::now(),
        };

        let json = serde_json::to_string(&pending).expect("serialization should work");
        let restored: PendingEvaluation =
            serde_json::from_str(&json).expect("deserialization should work");

        assert_eq!(pending.request_id, restored.request_id);
        assert_eq!(pending.progress, restored.progress);
    }

    #[test]
    fn test_challenge_crate_registry() {
        let mut registry = ChallengeCrateRegistry::new();

        let metadata = ChallengeCrateMetadata {
            id: "test-challenge".to_string(),
            name: "Test Challenge".to_string(),
            version: "1.0.0".to_string(),
            description: "A test challenge".to_string(),
            supports_hot_reload: true,
            min_compatible_version: "0.9.0".to_string(),
            last_updated: chrono::Utc::now(),
        };

        registry.register(metadata.clone());

        assert_eq!(registry.list().len(), 1);
        assert!(registry.get("test-challenge").is_some());

        let removed = registry.unregister("test-challenge");
        assert!(removed.is_some());
        assert!(registry.get("test-challenge").is_none());
    }

    #[test]
    fn test_challenge_crate_error_display() {
        let err = ChallengeCrateError::IncompatibleVersion {
            expected: "2.0.0".to_string(),
            actual: "1.0.0".to_string(),
        };
        let err_str = err.to_string();
        assert!(err_str.contains("expected"));
        assert!(err_str.contains("2.0.0"));
        assert!(err_str.contains("1.0.0"));
        assert!(err_str.contains("Incompatible checkpoint version"));
    }

    #[test]
    fn test_checkpoint_result() {
        let result = CheckpointResult {
            checkpoint_id: Uuid::new_v4(),
            size_bytes: 1024,
            pending_count: 5,
        };

        assert_eq!(result.size_bytes, 1024);
        assert_eq!(result.pending_count, 5);
    }

    #[test]
    fn test_restore_result() {
        let result = RestoreResult {
            checkpoint_id: Uuid::new_v4(),
            resumed_count: 3,
            dropped_count: 1,
        };

        assert_eq!(result.resumed_count, 3);
        assert_eq!(result.dropped_count, 1);
    }

    #[test]
    fn test_challenge_crate_metadata_serialization() {
        let metadata = ChallengeCrateMetadata {
            id: "test-id".to_string(),
            name: "Test Name".to_string(),
            version: "2.0.0".to_string(),
            description: "Test description".to_string(),
            supports_hot_reload: true,
            min_compatible_version: "1.5.0".to_string(),
            last_updated: chrono::Utc::now(),
        };

        let json = serde_json::to_string(&metadata).expect("serialization should work");
        let restored: ChallengeCrateMetadata =
            serde_json::from_str(&json).expect("deserialization should work");

        assert_eq!(metadata.id, restored.id);
        assert_eq!(metadata.name, restored.name);
        assert_eq!(metadata.version, restored.version);
        assert_eq!(metadata.supports_hot_reload, restored.supports_hot_reload);
    }

    #[test]
    fn test_challenge_crate_registry_serialization() {
        let mut registry = ChallengeCrateRegistry::new();

        let metadata1 = ChallengeCrateMetadata {
            id: "challenge-1".to_string(),
            name: "Challenge 1".to_string(),
            version: "1.0.0".to_string(),
            description: "First challenge".to_string(),
            supports_hot_reload: true,
            min_compatible_version: "1.0.0".to_string(),
            last_updated: chrono::Utc::now(),
        };

        let metadata2 = ChallengeCrateMetadata {
            id: "challenge-2".to_string(),
            name: "Challenge 2".to_string(),
            version: "2.0.0".to_string(),
            description: "Second challenge".to_string(),
            supports_hot_reload: false,
            min_compatible_version: "2.0.0".to_string(),
            last_updated: chrono::Utc::now(),
        };

        registry.register(metadata1);
        registry.register(metadata2);

        let json = serde_json::to_string(&registry).expect("serialization should work");
        let restored: ChallengeCrateRegistry =
            serde_json::from_str(&json).expect("deserialization should work");

        assert_eq!(restored.list().len(), 2);
        assert!(restored.get("challenge-1").is_some());
        assert!(restored.get("challenge-2").is_some());
    }

    #[test]
    fn test_challenge_crate_error_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: ChallengeCrateError = io_err.into();
        assert!(matches!(err, ChallengeCrateError::IoError(_)));
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn test_challenge_crate_registry_empty() {
        let registry = ChallengeCrateRegistry::new();
        assert!(registry.list().is_empty());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_challenge_crate_registry_unregister_nonexistent() {
        let mut registry = ChallengeCrateRegistry::new();
        let removed = registry.unregister("nonexistent");
        assert!(removed.is_none());
    }

    #[test]
    fn test_evaluation_checkpoint_with_pending() {
        let pending = PendingEvaluation {
            request_id: "req-1".to_string(),
            participant_id: "participant-1".to_string(),
            data: serde_json::json!({"code": "test"}),
            progress: 25,
            intermediate_results: None,
            started_at: chrono::Utc::now(),
        };

        let checkpoint = EvaluationCheckpoint {
            id: Uuid::new_v4(),
            challenge_id: "my-challenge".to_string(),
            state_data: vec![10, 20, 30],
            pending_evaluations: vec![pending],
            created_at: chrono::Utc::now(),
            crate_version: "1.2.3".to_string(),
        };

        assert_eq!(checkpoint.pending_evaluations.len(), 1);
        assert_eq!(checkpoint.pending_evaluations[0].progress, 25);
    }
}
