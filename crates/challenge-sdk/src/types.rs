//! Core types for challenges

use platform_core::Hotkey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Unique challenge identifier
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChallengeId(pub uuid::Uuid);

impl ChallengeId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    pub fn from_uuid(uuid: uuid::Uuid) -> Self {
        Self(uuid)
    }

    pub fn from_str(s: &str) -> Option<Self> {
        uuid::Uuid::parse_str(s).ok().map(Self)
    }
}

impl Default for ChallengeId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ChallengeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Challenge({})", &self.0.to_string()[..8])
    }
}

impl std::fmt::Display for ChallengeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Challenge metadata
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChallengeMetadata {
    pub id: ChallengeId,
    pub name: String,
    pub description: String,
    pub version: String,
    pub owner: Hotkey,
    pub emission_weight: f64, // Percentage of total emissions (0.0 - 1.0)
    pub config: ChallengeConfig,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub is_active: bool,
}

/// Challenge configuration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChallengeConfig {
    /// Mechanism ID on Bittensor (1, 2, 3... - 0 is reserved)
    /// Each challenge has its own mechanism for weight setting
    pub mechanism_id: u8,
    /// Evaluation timeout in seconds
    pub evaluation_timeout_secs: u64,
    /// Maximum memory per evaluation (MB)
    pub max_memory_mb: u64,
    /// Minimum validators required for weight consensus
    pub min_validators_for_weights: usize,
    /// Weight smoothing factor (0.0 = no smoothing, 1.0 = max smoothing)
    pub weight_smoothing: f64,
    /// Custom parameters as JSON
    pub params: String,
}

impl Default for ChallengeConfig {
    fn default() -> Self {
        Self {
            mechanism_id: 1,
            evaluation_timeout_secs: 300,
            max_memory_mb: 512,
            min_validators_for_weights: 3,
            weight_smoothing: 0.3,
            params: "{}".to_string(),
        }
    }
}

impl ChallengeConfig {
    /// Create config with specific mechanism ID
    pub fn with_mechanism(mechanism_id: u8) -> Self {
        Self {
            mechanism_id,
            ..Default::default()
        }
    }
}

/// Agent information
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentInfo {
    pub hash: String,
    pub name: Option<String>,
    pub owner: Option<Hotkey>,
    pub version: Option<String>,
    pub metadata_json: String, // Stored as JSON string for bincode compatibility
    pub submitted_at: chrono::DateTime<chrono::Utc>,
}

impl AgentInfo {
    pub fn new(hash: String) -> Self {
        Self {
            hash,
            name: None,
            owner: None,
            version: None,
            metadata_json: "{}".to_string(),
            submitted_at: chrono::Utc::now(),
        }
    }

    /// Get metadata as JSON Value
    pub fn metadata(&self) -> serde_json::Value {
        serde_json::from_str(&self.metadata_json).unwrap_or(serde_json::Value::Null)
    }

    /// Set metadata from JSON Value
    pub fn set_metadata(&mut self, value: serde_json::Value) {
        self.metadata_json = serde_json::to_string(&value).unwrap_or_default();
    }
}

/// Evaluation job
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationJob {
    pub id: uuid::Uuid,
    pub challenge_id: ChallengeId,
    pub agent_hash: String,
    pub job_type: String,
    pub payload: serde_json::Value,
    pub status: JobStatus,
    pub result: Option<EvaluationResult>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub validator: Option<Hotkey>,
}

impl EvaluationJob {
    pub fn new(
        challenge_id: ChallengeId,
        agent_hash: String,
        job_type: String,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            challenge_id,
            agent_hash,
            job_type,
            payload,
            status: JobStatus::Pending,
            result: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            validator: None,
        }
    }
}

/// Job status
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Timeout,
    Cancelled,
}

/// Evaluation result
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationResult {
    pub job_id: uuid::Uuid,
    pub agent_hash: String,
    pub score: f64,
    pub metrics: HashMap<String, f64>,
    pub logs: Option<String>,
    pub execution_time_ms: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl EvaluationResult {
    pub fn new(job_id: uuid::Uuid, agent_hash: String, score: f64) -> Self {
        Self {
            job_id,
            agent_hash,
            score: score.clamp(0.0, 1.0),
            metrics: HashMap::new(),
            logs: None,
            execution_time_ms: 0,
            timestamp: chrono::Utc::now(),
        }
    }

    pub fn with_metrics(mut self, metrics: HashMap<String, f64>) -> Self {
        self.metrics = metrics;
        self
    }

    pub fn with_logs(mut self, logs: String) -> Self {
        self.logs = Some(logs);
        self
    }

    pub fn with_execution_time(mut self, ms: u64) -> Self {
        self.execution_time_ms = ms;
        self
    }
}

/// Weight assignment for an agent
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WeightAssignment {
    pub agent_hash: String,
    pub weight: f64,
    pub confidence: f64,
    pub reason: Option<String>,
}

impl WeightAssignment {
    pub fn new(agent_hash: String, weight: f64) -> Self {
        Self {
            agent_hash,
            weight: weight.clamp(0.0, 1.0),
            confidence: 1.0,
            reason: None,
        }
    }

    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence.clamp(0.0, 1.0);
        self
    }

    pub fn with_reason(mut self, reason: String) -> Self {
        self.reason = Some(reason);
        self
    }
}

/// Weights submission from a validator for an epoch
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WeightsSubmission {
    pub challenge_id: ChallengeId,
    pub validator: Hotkey,
    pub epoch: u64,
    pub weights: Vec<WeightAssignment>,
    pub commitment_hash: String, // Hash for commit-reveal
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub signature: Vec<u8>,
}

/// Epoch information
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EpochInfo {
    pub number: u64,
    pub start_block: u64,
    pub end_block: u64,
    pub phase: EpochPhase,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Epoch phases for commit-reveal weight scheme
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EpochPhase {
    /// Validators are evaluating and preparing weights
    Evaluation,
    /// Validators commit weight hashes
    Commit,
    /// Validators reveal actual weights
    Reveal,
    /// Weights are being aggregated and finalized
    Finalization,
}

/// Aggregated weights after smoothing
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AggregatedWeights {
    pub challenge_id: ChallengeId,
    pub epoch: u64,
    pub weights: Vec<WeightAssignment>,
    pub validator_submissions: usize,
    pub smoothing_applied: bool,
    pub finalized_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_challenge_config_default() {
        let config = ChallengeConfig::default();
        assert_eq!(config.mechanism_id, 1);
        assert_eq!(config.evaluation_timeout_secs, 300);
        assert_eq!(config.max_memory_mb, 512);
    }

    #[test]
    fn test_evaluation_job_creation() {
        let id = ChallengeId::new();
        let job = EvaluationJob::new(
            id,
            "agent1".to_string(),
            "eval".to_string(),
            serde_json::json!({}),
        );
        assert_eq!(job.agent_hash, "agent1");
        assert_eq!(job.job_type, "eval");
        assert_eq!(job.status, JobStatus::Pending);
    }

    #[test]
    fn test_job_status_variants() {
        assert_ne!(JobStatus::Pending, JobStatus::Running);
        assert_ne!(JobStatus::Completed, JobStatus::Failed);
    }

    #[test]
    fn test_evaluation_result() {
        let result = EvaluationResult::new(uuid::Uuid::new_v4(), "agent".to_string(), 0.85);
        assert_eq!(result.score, 0.85);
        assert!(result.logs.is_none());
    }

    #[test]
    fn test_evaluation_result_builders() {
        let mut metrics = HashMap::new();
        metrics.insert("accuracy".to_string(), 0.95);

        let result = EvaluationResult::new(uuid::Uuid::new_v4(), "agent".to_string(), 0.9)
            .with_metrics(metrics)
            .with_logs("test logs".to_string())
            .with_execution_time(1000);

        assert_eq!(result.metrics.get("accuracy"), Some(&0.95));
        assert_eq!(result.logs, Some("test logs".to_string()));
        assert_eq!(result.execution_time_ms, 1000);
    }

    #[test]
    fn test_evaluation_result_score_clamping() {
        let result1 = EvaluationResult::new(uuid::Uuid::new_v4(), "a".to_string(), 1.5);
        assert_eq!(result1.score, 1.0);

        let result2 = EvaluationResult::new(uuid::Uuid::new_v4(), "a".to_string(), -0.5);
        assert_eq!(result2.score, 0.0);
    }

    #[test]
    fn test_weight_assignment() {
        let wa = WeightAssignment::new("agent".to_string(), 0.7);
        assert_eq!(wa.weight, 0.7);
        assert_eq!(wa.confidence, 1.0);
        assert!(wa.reason.is_none());
    }

    #[test]
    fn test_weight_assignment_builders() {
        let wa = WeightAssignment::new("agent".to_string(), 0.8)
            .with_confidence(0.9)
            .with_reason("high performance".to_string());

        assert_eq!(wa.confidence, 0.9);
        assert_eq!(wa.reason, Some("high performance".to_string()));
    }

    #[test]
    fn test_weight_assignment_clamping() {
        let wa1 = WeightAssignment::new("a".to_string(), 2.0);
        assert_eq!(wa1.weight, 1.0);

        let wa2 = WeightAssignment::new("a".to_string(), -1.0);
        assert_eq!(wa2.weight, 0.0);
    }

    #[test]
    fn test_epoch_phase_variants() {
        assert_ne!(EpochPhase::Evaluation, EpochPhase::Commit);
        assert_ne!(EpochPhase::Reveal, EpochPhase::Finalization);
    }

    #[test]
    fn test_challenge_id_new() {
        let id1 = ChallengeId::new();
        let id2 = ChallengeId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_challenge_id_from_uuid() {
        let uuid = uuid::Uuid::new_v4();
        let id1 = ChallengeId::from_uuid(uuid);
        let id2 = ChallengeId::from_uuid(uuid);
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_challenge_id_from_str() {
        let valid = ChallengeId::from_str("550e8400-e29b-41d4-a716-446655440000");
        assert!(valid.is_some());

        let invalid = ChallengeId::from_str("not-a-uuid");
        assert!(invalid.is_none());
    }

    #[test]
    fn test_challenge_id_display() {
        let id = ChallengeId::new();
        let display = format!("{}", id);
        assert!(!display.is_empty());
    }
}
