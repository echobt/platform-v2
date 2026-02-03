//! Challenge Template for Platform Network
//!
//! This is a template crate for developing challenges on the Platform Network.
//! Copy this directory and customize for your specific challenge.
//!
//! # Quick Start
//!
//! 1. Copy this template: `cp -r challenges/challenge-template challenges/my-challenge`
//! 2. Update `Cargo.toml`: Change the package name and description
//! 3. Implement the `ServerChallenge` trait with your evaluation logic
//! 4. Add to workspace `Cargo.toml`: `"challenges/my-challenge"`
//!
//! # Architecture
//!
//! Challenges implement the `ServerChallenge` trait which provides:
//! - `evaluate()` - Main evaluation logic, receives submissions and returns scores
//! - `validate()` - Quick validation without full evaluation
//! - `config()` - Challenge configuration and limits
//!
//! ```text
//! ┌─────────────────────────────────────────────────────┐
//! │                  Your Challenge                     │
//! │  ┌─────────────────────────────────────────────┐   │
//! │  │ impl ServerChallenge for MyChallenge        │   │
//! │  │   - evaluate(): Score submissions           │   │
//! │  │   - validate(): Quick syntax check          │   │
//! │  │   - config(): Limits and features           │   │
//! │  └─────────────────────────────────────────────┘   │
//! │  ┌─────────────────────────────────────────────┐   │
//! │  │ ChallengeState                              │   │
//! │  │   - Persistent state for checkpointing     │   │
//! │  │   - Restore state after restart            │   │
//! │  └─────────────────────────────────────────────┘   │
//! └─────────────────────────────────────────────────────┘
//! ```

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use platform_challenge_sdk::{
    ChallengeError, ConfigLimits, ConfigResponse, EvaluationRequest, EvaluationResponse,
    ServerChallenge, ValidationRequest, ValidationResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

// =============================================================================
// CONSTANTS - Customize these for your challenge
// =============================================================================

/// Challenge identifier - must be unique across all challenges
const CHALLENGE_ID: &str = "challenge-template";

/// Human-readable challenge name
const CHALLENGE_NAME: &str = "Challenge Template";

/// Challenge version (semver)
const CHALLENGE_VERSION: &str = "0.1.0";

/// Maximum submission size in bytes (10 MB)
const MAX_SUBMISSION_SIZE: u64 = 10 * 1024 * 1024;

/// Maximum evaluation time in seconds
const MAX_EVALUATION_TIME: u64 = 300;

/// Maximum cost per evaluation (in credits/tokens)
const MAX_COST: f64 = 1.0;

// =============================================================================
// CHALLENGE STATE - For persistence and checkpointing
// =============================================================================

/// State that persists across challenge restarts.
///
/// Customize this struct to store any state your challenge needs to maintain:
/// - Leaderboard data
/// - Historical scores
/// - Task definitions
/// - Cached computations
///
/// # Serialization
///
/// State is serialized to JSON for storage. Ensure all fields implement
/// `Serialize` and `Deserialize`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChallengeState {
    /// Number of evaluations performed
    pub evaluation_count: u64,

    /// Historical scores by participant ID
    pub participant_scores: HashMap<String, Vec<f64>>,

    /// Last checkpoint timestamp
    pub last_checkpoint: Option<DateTime<Utc>>,

    /// Challenge-specific metadata
    pub metadata: HashMap<String, Value>,
}

impl ChallengeState {
    /// Create a new empty state
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new evaluation score for a participant
    pub fn record_score(&mut self, participant_id: &str, score: f64) {
        self.evaluation_count += 1;
        self.participant_scores
            .entry(participant_id.to_string())
            .or_default()
            .push(score);
    }

    /// Get average score for a participant
    pub fn get_average_score(&self, participant_id: &str) -> Option<f64> {
        self.participant_scores.get(participant_id).map(|scores| {
            if scores.is_empty() {
                0.0
            } else {
                scores.iter().sum::<f64>() / scores.len() as f64
            }
        })
    }

    /// Create a checkpoint of current state
    pub fn checkpoint(&mut self) -> ChallengeCheckpoint {
        self.last_checkpoint = Some(Utc::now());
        ChallengeCheckpoint {
            state: self.clone(),
            checkpoint_id: Uuid::new_v4().to_string(),
            created_at: Utc::now(),
        }
    }

    /// Restore state from a checkpoint
    pub fn restore(checkpoint: &ChallengeCheckpoint) -> Self {
        checkpoint.state.clone()
    }
}

/// Checkpoint for state persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeCheckpoint {
    /// The serialized state
    pub state: ChallengeState,

    /// Unique checkpoint identifier
    pub checkpoint_id: String,

    /// When the checkpoint was created
    pub created_at: DateTime<Utc>,
}

// =============================================================================
// CHALLENGE IMPLEMENTATION
// =============================================================================

/// Template challenge implementation.
///
/// This struct holds your challenge's runtime state and implements the
/// `ServerChallenge` trait for integration with the Platform Network.
///
/// # Customization
///
/// 1. Add any configuration fields your challenge needs
/// 2. Implement the evaluation logic in `evaluate()`
/// 3. Add validation rules in `validate()`
/// 4. Configure limits in `config()`
pub struct TemplateChallenge {
    /// Internal state (thread-safe for concurrent evaluations)
    state: Arc<RwLock<ChallengeState>>,

    /// Challenge-specific configuration
    config: TemplateChallengeConfig,
}

/// Configuration for the template challenge
#[derive(Debug, Clone)]
pub struct TemplateChallengeConfig {
    /// Base score awarded to all valid submissions
    pub base_score: f64,

    /// Maximum bonus score that can be earned
    pub max_bonus: f64,

    /// Features supported by this challenge
    pub features: Vec<String>,
}

impl Default for TemplateChallengeConfig {
    fn default() -> Self {
        Self {
            base_score: 0.5,
            max_bonus: 0.5,
            features: vec!["evaluation".to_string(), "validation".to_string()],
        }
    }
}

impl TemplateChallenge {
    /// Create a new challenge instance with default configuration
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(ChallengeState::new())),
            config: TemplateChallengeConfig::default(),
        }
    }

    /// Create a new challenge with custom configuration
    pub fn with_config(config: TemplateChallengeConfig) -> Self {
        Self {
            state: Arc::new(RwLock::new(ChallengeState::new())),
            config,
        }
    }

    /// Restore challenge from a checkpoint
    pub fn from_checkpoint(checkpoint: &ChallengeCheckpoint) -> Self {
        Self {
            state: Arc::new(RwLock::new(ChallengeState::restore(checkpoint))),
            config: TemplateChallengeConfig::default(),
        }
    }

    /// Create a checkpoint of the current state
    pub async fn checkpoint(&self) -> ChallengeCheckpoint {
        let mut state = self.state.write().await;
        state.checkpoint()
    }

    /// Get the current state (read-only)
    pub async fn get_state(&self) -> ChallengeState {
        self.state.read().await.clone()
    }

    /// Evaluate a submission and calculate the score.
    ///
    /// # Customize This Method
    ///
    /// This is where your challenge's core logic lives. Replace this
    /// implementation with your actual evaluation algorithm.
    ///
    /// The example implementation:
    /// 1. Extracts a "bonus" field from the submission data
    /// 2. Adds it to a base score
    /// 3. Clamps the result to [0.0, 1.0]
    async fn calculate_score(&self, data: &Value) -> f64 {
        let base_score = self.config.base_score;

        // Example: Extract bonus from submission data
        // Replace this with your actual scoring logic
        let bonus = data
            .get("bonus")
            .and_then(|v| v.as_f64())
            .map(|b| b.clamp(0.0, self.config.max_bonus))
            .unwrap_or(0.0);

        (base_score + bonus).clamp(0.0, 1.0)
    }

    /// Validate submission data structure.
    ///
    /// # Customize This Method
    ///
    /// Add your validation rules here. Common checks:
    /// - Required fields present
    /// - Data types correct
    /// - Values within expected ranges
    /// - File size limits
    fn validate_data(&self, data: &Value) -> (bool, Vec<String>, Vec<String>) {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        // Check for null or empty data
        if data.is_null() {
            errors.push("Submission data cannot be null".to_string());
            return (false, errors, warnings);
        }

        if data == &json!({}) {
            errors.push("Submission data cannot be empty".to_string());
            return (false, errors, warnings);
        }

        // Example: Check for deprecated field usage
        if data.get("legacy_field").is_some() {
            warnings.push("Field 'legacy_field' is deprecated, use 'new_field' instead".to_string());
        }

        // Example: Type checking (customize for your needs)
        if let Some(bonus) = data.get("bonus") {
            if !bonus.is_number() {
                errors.push("Field 'bonus' must be a number".to_string());
            }
        }

        let valid = errors.is_empty();
        (valid, errors, warnings)
    }
}

impl Default for TemplateChallenge {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ServerChallenge for TemplateChallenge {
    /// Returns the unique challenge identifier.
    ///
    /// # Customize
    ///
    /// Change `CHALLENGE_ID` constant at the top of this file.
    fn challenge_id(&self) -> &str {
        CHALLENGE_ID
    }

    /// Returns the human-readable challenge name.
    ///
    /// # Customize
    ///
    /// Change `CHALLENGE_NAME` constant at the top of this file.
    fn name(&self) -> &str {
        CHALLENGE_NAME
    }

    /// Returns the challenge version (semver).
    ///
    /// # Customize
    ///
    /// Change `CHALLENGE_VERSION` constant at the top of this file.
    fn version(&self) -> &str {
        CHALLENGE_VERSION
    }

    /// Evaluate a submission and return a score.
    ///
    /// # Arguments
    ///
    /// * `request` - Contains submission data, participant ID, and metadata
    ///
    /// # Returns
    ///
    /// * `Ok(EvaluationResponse)` - Score between 0.0 and 1.0 with results
    /// * `Err(ChallengeError)` - If evaluation fails
    ///
    /// # Customize
    ///
    /// Replace `calculate_score()` with your evaluation logic.
    async fn evaluate(
        &self,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, ChallengeError> {
        info!(
            request_id = %request.request_id,
            participant_id = %request.participant_id,
            "Evaluating submission"
        );

        // Calculate the score using your custom logic
        let score = self.calculate_score(&request.data).await;

        // Update state with the new score
        {
            let mut state = self.state.write().await;
            state.record_score(&request.participant_id, score);
        }

        debug!(
            request_id = %request.request_id,
            score = score,
            "Evaluation complete"
        );

        // Build response with detailed results
        let results = json!({
            "participant_id": request.participant_id,
            "submission_id": request.submission_id,
            "epoch": request.epoch,
            "metadata_provided": request.metadata.is_some(),
        });

        Ok(EvaluationResponse::success(&request.request_id, score, results))
    }

    /// Validate submission data without full evaluation.
    ///
    /// Use this for quick syntax/structure checks before committing to
    /// a full evaluation.
    ///
    /// # Arguments
    ///
    /// * `request` - Contains the data to validate
    ///
    /// # Returns
    ///
    /// * `Ok(ValidationResponse)` - Validation result with errors/warnings
    /// * `Err(ChallengeError)` - If validation process itself fails
    ///
    /// # Customize
    ///
    /// Replace `validate_data()` with your validation rules.
    async fn validate(
        &self,
        request: ValidationRequest,
    ) -> Result<ValidationResponse, ChallengeError> {
        debug!("Validating submission data");

        let (valid, errors, warnings) = self.validate_data(&request.data);

        if !valid {
            warn!(errors = ?errors, "Validation failed");
        }

        Ok(ValidationResponse {
            valid,
            errors,
            warnings,
        })
    }

    /// Return challenge configuration and limits.
    ///
    /// # Customize
    ///
    /// Update the constants at the top of this file:
    /// - `MAX_SUBMISSION_SIZE`
    /// - `MAX_EVALUATION_TIME`
    /// - `MAX_COST`
    ///
    /// Add any additional features your challenge supports.
    fn config(&self) -> ConfigResponse {
        ConfigResponse {
            challenge_id: self.challenge_id().to_string(),
            name: self.name().to_string(),
            version: self.version().to_string(),
            config_schema: Some(json!({
                "type": "object",
                "properties": {
                    "bonus": {
                        "type": "number",
                        "description": "Bonus score multiplier (0.0 to 0.5)",
                        "minimum": 0.0,
                        "maximum": 0.5
                    }
                }
            })),
            features: self.config.features.clone(),
            limits: ConfigLimits {
                max_submission_size: Some(MAX_SUBMISSION_SIZE),
                max_evaluation_time: Some(MAX_EVALUATION_TIME),
                max_cost: Some(MAX_COST),
            },
        }
    }
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_request(request_id: &str) -> EvaluationRequest {
        EvaluationRequest {
            request_id: request_id.to_string(),
            submission_id: "sub-test".to_string(),
            participant_id: "participant-test".to_string(),
            data: json!({"bonus": 0.2}),
            metadata: None,
            epoch: 1,
            deadline: None,
        }
    }

    #[test]
    fn test_challenge_id() {
        let challenge = TemplateChallenge::new();
        assert_eq!(challenge.challenge_id(), CHALLENGE_ID);
    }

    #[test]
    fn test_challenge_name() {
        let challenge = TemplateChallenge::new();
        assert_eq!(challenge.name(), CHALLENGE_NAME);
    }

    #[test]
    fn test_challenge_version() {
        let challenge = TemplateChallenge::new();
        assert_eq!(challenge.version(), CHALLENGE_VERSION);
    }

    #[tokio::test]
    async fn test_evaluate_with_bonus() {
        let challenge = TemplateChallenge::new();
        let request = create_test_request("test-001");

        let response = challenge.evaluate(request).await.expect("evaluation should succeed");

        assert!(response.success);
        assert_eq!(response.request_id, "test-001");
        // Base score (0.5) + bonus (0.2) = 0.7
        assert!((response.score - 0.7).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_evaluate_without_bonus() {
        let challenge = TemplateChallenge::new();
        let request = EvaluationRequest {
            request_id: "test-002".to_string(),
            submission_id: "sub-test".to_string(),
            participant_id: "participant-test".to_string(),
            data: json!({"other_field": "value"}),
            metadata: None,
            epoch: 1,
            deadline: None,
        };

        let response = challenge.evaluate(request).await.expect("evaluation should succeed");

        assert!(response.success);
        // Base score only (0.5)
        assert!((response.score - 0.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_evaluate_with_max_bonus() {
        let challenge = TemplateChallenge::new();
        let request = EvaluationRequest {
            request_id: "test-003".to_string(),
            submission_id: "sub-test".to_string(),
            participant_id: "participant-test".to_string(),
            data: json!({"bonus": 0.5}),
            metadata: None,
            epoch: 1,
            deadline: None,
        };

        let response = challenge.evaluate(request).await.expect("evaluation should succeed");

        assert!(response.success);
        // Base (0.5) + max bonus (0.5) = 1.0
        assert!((response.score - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_evaluate_with_excessive_bonus() {
        let challenge = TemplateChallenge::new();
        let request = EvaluationRequest {
            request_id: "test-004".to_string(),
            submission_id: "sub-test".to_string(),
            participant_id: "participant-test".to_string(),
            data: json!({"bonus": 1.0}), // Exceeds max bonus
            metadata: None,
            epoch: 1,
            deadline: None,
        };

        let response = challenge.evaluate(request).await.expect("evaluation should succeed");

        assert!(response.success);
        // Should be clamped to 1.0
        assert!((response.score - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_validate_valid_data() {
        let challenge = TemplateChallenge::new();
        let request = ValidationRequest {
            data: json!({"bonus": 0.3}),
        };

        let response = challenge.validate(request).await.expect("validation should succeed");

        assert!(response.valid);
        assert!(response.errors.is_empty());
    }

    #[tokio::test]
    async fn test_validate_null_data() {
        let challenge = TemplateChallenge::new();
        let request = ValidationRequest { data: json!(null) };

        let response = challenge.validate(request).await.expect("validation should succeed");

        assert!(!response.valid);
        assert!(!response.errors.is_empty());
        assert!(response.errors[0].contains("null"));
    }

    #[tokio::test]
    async fn test_validate_empty_data() {
        let challenge = TemplateChallenge::new();
        let request = ValidationRequest { data: json!({}) };

        let response = challenge.validate(request).await.expect("validation should succeed");

        assert!(!response.valid);
        assert!(response.errors[0].contains("empty"));
    }

    #[tokio::test]
    async fn test_validate_invalid_bonus_type() {
        let challenge = TemplateChallenge::new();
        let request = ValidationRequest {
            data: json!({"bonus": "not a number"}),
        };

        let response = challenge.validate(request).await.expect("validation should succeed");

        assert!(!response.valid);
        assert!(response.errors[0].contains("number"));
    }

    #[tokio::test]
    async fn test_validate_deprecated_field_warning() {
        let challenge = TemplateChallenge::new();
        let request = ValidationRequest {
            data: json!({"legacy_field": "old value", "valid": true}),
        };

        let response = challenge.validate(request).await.expect("validation should succeed");

        assert!(response.valid);
        assert!(!response.warnings.is_empty());
        assert!(response.warnings[0].contains("deprecated"));
    }

    #[test]
    fn test_config_response() {
        let challenge = TemplateChallenge::new();
        let config = challenge.config();

        assert_eq!(config.challenge_id, CHALLENGE_ID);
        assert_eq!(config.name, CHALLENGE_NAME);
        assert_eq!(config.version, CHALLENGE_VERSION);
        assert!(config.config_schema.is_some());
        assert_eq!(config.limits.max_submission_size, Some(MAX_SUBMISSION_SIZE));
        assert_eq!(config.limits.max_evaluation_time, Some(MAX_EVALUATION_TIME));
        assert_eq!(config.limits.max_cost, Some(MAX_COST));
    }

    #[test]
    fn test_challenge_state_new() {
        let state = ChallengeState::new();

        assert_eq!(state.evaluation_count, 0);
        assert!(state.participant_scores.is_empty());
        assert!(state.last_checkpoint.is_none());
    }

    #[test]
    fn test_challenge_state_record_score() {
        let mut state = ChallengeState::new();

        state.record_score("participant-1", 0.8);
        state.record_score("participant-1", 0.9);
        state.record_score("participant-2", 0.7);

        assert_eq!(state.evaluation_count, 3);
        assert_eq!(state.participant_scores.get("participant-1").map(|v| v.len()), Some(2));
        assert_eq!(state.participant_scores.get("participant-2").map(|v| v.len()), Some(1));
    }

    #[test]
    fn test_challenge_state_get_average_score() {
        let mut state = ChallengeState::new();

        state.record_score("participant-1", 0.8);
        state.record_score("participant-1", 0.6);

        let avg = state.get_average_score("participant-1").expect("participant should exist");
        assert!((avg - 0.7).abs() < f64::EPSILON);

        assert!(state.get_average_score("unknown").is_none());
    }

    #[test]
    fn test_challenge_state_checkpoint() {
        let mut state = ChallengeState::new();
        state.record_score("participant-1", 0.8);

        let checkpoint = state.checkpoint();

        assert!(!checkpoint.checkpoint_id.is_empty());
        assert!(state.last_checkpoint.is_some());
        assert_eq!(checkpoint.state.evaluation_count, 1);
    }

    #[test]
    fn test_challenge_state_restore() {
        let mut state = ChallengeState::new();
        state.record_score("participant-1", 0.8);
        let checkpoint = state.checkpoint();

        let restored = ChallengeState::restore(&checkpoint);

        assert_eq!(restored.evaluation_count, 1);
        assert!(restored.participant_scores.contains_key("participant-1"));
    }

    #[tokio::test]
    async fn test_challenge_checkpoint_and_restore() {
        let challenge = TemplateChallenge::new();

        // Perform some evaluations
        let request = create_test_request("test-checkpoint");
        challenge.evaluate(request).await.expect("evaluation should succeed");

        // Create checkpoint
        let checkpoint = challenge.checkpoint().await;
        assert_eq!(checkpoint.state.evaluation_count, 1);

        // Restore to new challenge instance
        let restored_challenge = TemplateChallenge::from_checkpoint(&checkpoint);
        let restored_state = restored_challenge.get_state().await;

        assert_eq!(restored_state.evaluation_count, 1);
    }

    #[test]
    fn test_custom_config() {
        let config = TemplateChallengeConfig {
            base_score: 0.3,
            max_bonus: 0.7,
            features: vec!["custom_feature".to_string()],
        };

        let challenge = TemplateChallenge::with_config(config);
        let challenge_config = challenge.config();

        assert!(challenge_config.features.contains(&"custom_feature".to_string()));
    }

    #[tokio::test]
    async fn test_concurrent_evaluations() {
        let challenge = Arc::new(TemplateChallenge::new());

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let c = Arc::clone(&challenge);
                tokio::spawn(async move {
                    let request = EvaluationRequest {
                        request_id: format!("concurrent-{}", i),
                        submission_id: format!("sub-{}", i),
                        participant_id: format!("participant-{}", i),
                        data: json!({"bonus": 0.1}),
                        metadata: None,
                        epoch: 1,
                        deadline: None,
                    };
                    c.evaluate(request).await
                })
            })
            .collect();

        for handle in handles {
            let result = handle.await.expect("task should complete").expect("evaluation should succeed");
            assert!(result.success);
        }

        // Verify all evaluations were recorded
        let state = challenge.get_state().await;
        assert_eq!(state.evaluation_count, 10);
    }
}
