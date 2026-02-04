//! Direct blockchain storage for validation results
//!
//! This module provides a high-level interface for validators to store and read
//! validation results directly from the Bittensor blockchain. It handles:
//!
//! - Task assignment verification before writes
//! - Double-write prevention
//! - P2P consensus verification for reads
//! - Epoch-based validation result retrieval
//!
//! ## Usage
//!
//! ```ignore
//! use platform_bittensor::{BlockchainStorage, BlockchainStorageConfig, ValidationResult};
//!
//! let config = BlockchainStorageConfig::default();
//! let storage = BlockchainStorage::new(client, config);
//!
//! // Store a validation result
//! let result = ValidationResult {
//!     validator_hotkey: "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".to_string(),
//!     challenge_id: ChallengeId::new(),
//!     submission_hash: "abc123".to_string(),
//!     score: 0.85,
//!     timestamp: chrono::Utc::now().timestamp(),
//!     block_number: 0, // Will be set on chain
//! };
//!
//! storage.store_validation(&result).await?;
//! ```

use crate::SubtensorClient;
use platform_core::{ChallengeId, Hotkey};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// Default timeout for blockchain operations in seconds
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Default number of retries for failed operations
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Default number of confirmations required for consensus
const DEFAULT_MIN_CONSENSUS_VALIDATORS: usize = 2;

/// Errors that can occur during blockchain storage operations
#[derive(Debug, Error)]
pub enum StorageError {
    /// Task is not assigned to this validator
    #[error("Task not assigned to this validator")]
    NotAssigned,

    /// Validation result already stored for this submission
    #[error("Already stored validation for this submission")]
    AlreadyStored,

    /// Error communicating with the blockchain
    #[error("Chain error: {0}")]
    ChainError(String),

    /// Operation timed out
    #[error("Timeout waiting for chain")]
    Timeout,

    /// Consensus verification failed
    #[error("Consensus failed: required {required}, got {received}")]
    ConsensusFailed { required: usize, received: usize },

    /// Invalid configuration
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    /// Client not connected
    #[error("Client not connected to blockchain")]
    NotConnected,
}

/// Configuration for blockchain storage
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockchainStorageConfig {
    /// Subnet UID
    pub netuid: u16,

    /// Maximum number of retries for failed operations
    pub max_retries: u32,

    /// Timeout for blockchain operations in seconds
    pub timeout_secs: u64,

    /// Minimum number of validators required for consensus on reads
    pub min_consensus_validators: usize,

    /// Whether to verify task assignments before storing
    pub verify_assignments: bool,

    /// Whether to check for duplicate writes
    pub check_duplicates: bool,
}

impl Default for BlockchainStorageConfig {
    fn default() -> Self {
        Self {
            netuid: 100,
            max_retries: DEFAULT_MAX_RETRIES,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            min_consensus_validators: DEFAULT_MIN_CONSENSUS_VALIDATORS,
            verify_assignments: true,
            check_duplicates: true,
        }
    }
}

impl BlockchainStorageConfig {
    /// Create a new configuration with specified netuid
    pub fn with_netuid(netuid: u16) -> Self {
        Self {
            netuid,
            ..Default::default()
        }
    }

    /// Create a configuration for testing (relaxed settings)
    pub fn for_testing() -> Self {
        Self {
            netuid: 1,
            max_retries: 1,
            timeout_secs: 5,
            min_consensus_validators: 1,
            verify_assignments: false,
            check_duplicates: false,
        }
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<(), StorageError> {
        if self.timeout_secs == 0 {
            return Err(StorageError::InvalidConfig(
                "timeout_secs must be greater than 0".to_string(),
            ));
        }
        if self.min_consensus_validators == 0 {
            return Err(StorageError::InvalidConfig(
                "min_consensus_validators must be greater than 0".to_string(),
            ));
        }
        Ok(())
    }
}

/// A validation result to be stored on the blockchain
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ValidationResult {
    /// The validator's hotkey (SS58 address)
    pub validator_hotkey: String,

    /// The challenge this validation is for
    pub challenge_id: ChallengeId,

    /// Hash of the submission being validated
    pub submission_hash: String,

    /// The score assigned (0.0 to 1.0)
    pub score: f64,

    /// Unix timestamp when validation was performed
    pub timestamp: i64,

    /// Block number where result was stored (0 if not yet stored)
    pub block_number: u64,
}

impl ValidationResult {
    /// Create a new validation result
    pub fn new(
        validator_hotkey: String,
        challenge_id: ChallengeId,
        submission_hash: String,
        score: f64,
    ) -> Self {
        Self {
            validator_hotkey,
            challenge_id,
            submission_hash,
            score: score.clamp(0.0, 1.0),
            timestamp: chrono::Utc::now().timestamp(),
            block_number: 0,
        }
    }

    /// Create a unique key for this result (challenge + submission + validator)
    pub fn storage_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.challenge_id, self.submission_hash, self.validator_hotkey
        )
    }

    /// Validate the result before storing
    pub fn validate(&self) -> Result<(), StorageError> {
        if self.validator_hotkey.is_empty() {
            return Err(StorageError::InvalidConfig(
                "validator_hotkey cannot be empty".to_string(),
            ));
        }
        if self.submission_hash.is_empty() {
            return Err(StorageError::InvalidConfig(
                "submission_hash cannot be empty".to_string(),
            ));
        }
        if !(0.0..=1.0).contains(&self.score) {
            return Err(StorageError::InvalidConfig(format!(
                "score must be between 0.0 and 1.0, got {}",
                self.score
            )));
        }
        Ok(())
    }
}

/// A task assignment record
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskAssignment {
    /// The challenge this assignment is for
    pub challenge_id: ChallengeId,

    /// Hash of the submission to validate
    pub submission_hash: String,

    /// The validator assigned to this task
    pub assigned_validator: String,

    /// Unix timestamp when assignment was made
    pub assigned_at: i64,

    /// Unix timestamp when assignment expires
    pub expires_at: i64,
}

impl TaskAssignment {
    /// Create a new task assignment
    pub fn new(
        challenge_id: ChallengeId,
        submission_hash: String,
        assigned_validator: String,
        duration_secs: i64,
    ) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            challenge_id,
            submission_hash,
            assigned_validator,
            assigned_at: now,
            expires_at: now + duration_secs,
        }
    }

    /// Check if this assignment has expired
    pub fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp() > self.expires_at
    }

    /// Check if this assignment is for the given validator
    pub fn is_for_validator(&self, validator_hotkey: &str) -> bool {
        self.assigned_validator == validator_hotkey
    }

    /// Create a unique key for this assignment (challenge + submission)
    pub fn storage_key(&self) -> String {
        format!("{}:{}", self.challenge_id, self.submission_hash)
    }
}

/// In-memory cache for validation results and assignments
/// This reduces blockchain queries for frequently accessed data
struct StorageCache {
    /// Cached validation results keyed by storage_key
    validations: HashMap<String, ValidationResult>,

    /// Cached task assignments keyed by storage_key
    assignments: HashMap<String, TaskAssignment>,

    /// Set of stored validation keys (for duplicate detection)
    stored_keys: HashSet<String>,

    /// Maximum cache size
    max_entries: usize,
}

impl StorageCache {
    fn new(max_entries: usize) -> Self {
        Self {
            validations: HashMap::new(),
            assignments: HashMap::new(),
            stored_keys: HashSet::new(),
            max_entries,
        }
    }

    fn add_validation(&mut self, result: ValidationResult) {
        let key = result.storage_key();

        // Evict oldest entries if at capacity (simple FIFO via random removal)
        if self.validations.len() >= self.max_entries {
            if let Some(old_key) = self.validations.keys().next().cloned() {
                self.validations.remove(&old_key);
            }
        }

        self.stored_keys.insert(key.clone());
        self.validations.insert(key, result);
    }

    fn add_assignment(&mut self, assignment: TaskAssignment) {
        let key = assignment.storage_key();

        if self.assignments.len() >= self.max_entries {
            if let Some(old_key) = self.assignments.keys().next().cloned() {
                self.assignments.remove(&old_key);
            }
        }

        self.assignments.insert(key, assignment);
    }

    fn get_validation(&self, key: &str) -> Option<&ValidationResult> {
        self.validations.get(key)
    }

    fn get_assignment(&self, key: &str) -> Option<&TaskAssignment> {
        self.assignments.get(key)
    }

    fn has_stored(&self, key: &str) -> bool {
        self.stored_keys.contains(key)
    }

    fn clear(&mut self) {
        self.validations.clear();
        self.assignments.clear();
        self.stored_keys.clear();
    }
}

/// Blockchain storage for validation results
///
/// Provides a high-level interface for storing and reading validation
/// results on the Bittensor blockchain. Handles task assignment verification,
/// duplicate detection, and consensus verification for reads.
pub struct BlockchainStorage {
    /// Subtensor client for blockchain interactions
    client: SubtensorClient,

    /// Storage configuration
    config: BlockchainStorageConfig,

    /// In-memory cache for reducing blockchain queries
    cache: Arc<RwLock<StorageCache>>,
}

impl BlockchainStorage {
    /// Create a new blockchain storage instance
    pub fn new(client: SubtensorClient, config: BlockchainStorageConfig) -> Self {
        const DEFAULT_CACHE_SIZE: usize = 10000;

        Self {
            client,
            config,
            cache: Arc::new(RwLock::new(StorageCache::new(DEFAULT_CACHE_SIZE))),
        }
    }

    /// Create a new blockchain storage with custom cache size
    pub fn with_cache_size(
        client: SubtensorClient,
        config: BlockchainStorageConfig,
        cache_size: usize,
    ) -> Self {
        Self {
            client,
            config,
            cache: Arc::new(RwLock::new(StorageCache::new(cache_size))),
        }
    }

    /// Get a reference to the underlying client
    pub fn client(&self) -> &SubtensorClient {
        &self.client
    }

    /// Get a mutable reference to the underlying client
    pub fn client_mut(&mut self) -> &mut SubtensorClient {
        &mut self.client
    }

    /// Get the configuration
    pub fn config(&self) -> &BlockchainStorageConfig {
        &self.config
    }

    /// Check if a task is assigned to this validator
    ///
    /// This verifies that the given task (challenge + submission) is assigned
    /// to the specified validator and the assignment has not expired.
    pub async fn is_task_assigned(
        &self,
        challenge_id: &ChallengeId,
        submission_hash: &str,
        validator_hotkey: &Hotkey,
    ) -> Result<bool, StorageError> {
        if !self.config.verify_assignments {
            // Assignment verification disabled - always return true
            return Ok(true);
        }

        let assignment_key = format!("{}:{}", challenge_id, submission_hash);
        let validator_ss58 = validator_hotkey.to_ss58();

        // Check cache first
        {
            let cache = self.cache.read().await;
            if let Some(assignment) = cache.get_assignment(&assignment_key) {
                if assignment.is_expired() {
                    debug!(
                        "Cached assignment expired for {} (expired at {})",
                        assignment_key, assignment.expires_at
                    );
                    return Ok(false);
                }
                return Ok(assignment.is_for_validator(&validator_ss58));
            }
        }

        // Query blockchain for assignment
        let assignment = self
            .query_task_assignment(challenge_id, submission_hash)
            .await?;

        // Cache the assignment
        if let Some(ref assign) = assignment {
            let mut cache = self.cache.write().await;
            cache.add_assignment(assign.clone());
        }

        match assignment {
            Some(assign) => {
                if assign.is_expired() {
                    debug!(
                        "Assignment expired for {} (expired at {})",
                        assignment_key, assign.expires_at
                    );
                    Ok(false)
                } else {
                    Ok(assign.is_for_validator(&validator_ss58))
                }
            }
            None => {
                debug!("No assignment found for {}", assignment_key);
                Ok(false)
            }
        }
    }

    /// Store a validation result on the blockchain
    ///
    /// This method:
    /// 1. Validates the result
    /// 2. Checks if task is assigned to the validator (if enabled)
    /// 3. Checks for duplicate writes (if enabled)
    /// 4. Stores the result on chain
    ///
    /// Returns the transaction hash on success.
    pub async fn store_validation(&self, result: &ValidationResult) -> Result<String, StorageError> {
        // Validate the result first
        result.validate()?;

        let storage_key = result.storage_key();

        // Check for duplicates
        if self.config.check_duplicates {
            let cache = self.cache.read().await;
            if cache.has_stored(&storage_key) {
                warn!("Duplicate validation attempt for {}", storage_key);
                return Err(StorageError::AlreadyStored);
            }
        }

        // Parse validator hotkey and check assignment
        if self.config.verify_assignments {
            let validator_hotkey = Hotkey::from_ss58(&result.validator_hotkey)
                .ok_or_else(|| StorageError::InvalidConfig("Invalid validator hotkey".to_string()))?;

            let is_assigned = self
                .is_task_assigned(&result.challenge_id, &result.submission_hash, &validator_hotkey)
                .await?;

            if !is_assigned {
                warn!(
                    "Validator {} not assigned to task {}:{}",
                    result.validator_hotkey, result.challenge_id, result.submission_hash
                );
                return Err(StorageError::NotAssigned);
            }
        }

        // Check if already stored on chain
        if self.config.check_duplicates {
            let validator_hotkey = Hotkey::from_ss58(&result.validator_hotkey)
                .ok_or_else(|| StorageError::InvalidConfig("Invalid validator hotkey".to_string()))?;

            let already_stored = self
                .has_stored_validation(&result.challenge_id, &result.submission_hash, &validator_hotkey)
                .await?;

            if already_stored {
                warn!("Validation already stored on chain for {}", storage_key);
                return Err(StorageError::AlreadyStored);
            }
        }

        // Store on chain with retry
        let tx_hash = self.store_with_retry(result).await?;

        // Update cache
        let mut cache = self.cache.write().await;
        cache.add_validation(result.clone());

        info!(
            "Stored validation result: {} -> tx {}",
            storage_key, tx_hash
        );

        Ok(tx_hash)
    }

    /// Read validation results for a submission with consensus verification
    ///
    /// Returns validation results from multiple validators and verifies
    /// consensus among them.
    pub async fn read_validations(
        &self,
        challenge_id: &ChallengeId,
        submission_hash: &str,
    ) -> Result<Vec<ValidationResult>, StorageError> {
        let results = self
            .query_validations_with_timeout(challenge_id, submission_hash)
            .await?;

        // Verify consensus if we have results
        if !results.is_empty() && results.len() < self.config.min_consensus_validators {
            warn!(
                "Insufficient validators for consensus: {} < {}",
                results.len(),
                self.config.min_consensus_validators
            );
            return Err(StorageError::ConsensusFailed {
                required: self.config.min_consensus_validators,
                received: results.len(),
            });
        }

        debug!(
            "Read {} validation results for {}:{}",
            results.len(),
            challenge_id,
            submission_hash
        );

        Ok(results)
    }

    /// Get all validation results for a challenge in an epoch
    pub async fn get_epoch_validations(
        &self,
        challenge_id: &ChallengeId,
        epoch: u64,
    ) -> Result<Vec<ValidationResult>, StorageError> {
        let results = self
            .query_epoch_validations_with_timeout(challenge_id, epoch)
            .await?;

        info!(
            "Retrieved {} validation results for challenge {} in epoch {}",
            results.len(),
            challenge_id,
            epoch
        );

        Ok(results)
    }

    /// Check if we already stored a validation result for this submission
    pub async fn has_stored_validation(
        &self,
        challenge_id: &ChallengeId,
        submission_hash: &str,
        validator_hotkey: &Hotkey,
    ) -> Result<bool, StorageError> {
        let storage_key = format!(
            "{}:{}:{}",
            challenge_id,
            submission_hash,
            validator_hotkey.to_ss58()
        );

        // Check cache first
        {
            let cache = self.cache.read().await;
            if cache.has_stored(&storage_key) {
                return Ok(true);
            }
        }

        // Query blockchain
        let exists = self
            .query_validation_exists(challenge_id, submission_hash, validator_hotkey)
            .await?;

        if exists {
            // Update cache - create a placeholder result for the key
            let mut cache = self.cache.write().await;
            cache.stored_keys.insert(storage_key);
        }

        Ok(exists)
    }

    /// Clear the in-memory cache
    pub async fn clear_cache(&self) {
        let mut cache = self.cache.write().await;
        cache.clear();
        debug!("Blockchain storage cache cleared");
    }

    /// Get current cache statistics
    pub async fn cache_stats(&self) -> (usize, usize, usize) {
        let cache = self.cache.read().await;
        (
            cache.validations.len(),
            cache.assignments.len(),
            cache.stored_keys.len(),
        )
    }

    // =========================================================================
    // Private implementation methods
    // =========================================================================

    /// Query task assignment from blockchain
    async fn query_task_assignment(
        &self,
        challenge_id: &ChallengeId,
        submission_hash: &str,
    ) -> Result<Option<TaskAssignment>, StorageError> {
        let timeout_duration = Duration::from_secs(self.config.timeout_secs);

        let result = timeout(timeout_duration, async {
            // In a real implementation, this would query the blockchain
            // for task assignment data stored in a custom storage pallet.
            //
            // For now, we simulate by checking the metagraph for validator
            // assignments. The actual implementation would depend on the
            // specific blockchain storage structure used.
            //
            // Example chain query pattern:
            // let client = self.client.client()?;
            // let storage_key = generate_assignment_storage_key(challenge_id, submission_hash);
            // let data = client.storage().fetch(&storage_key).await?;

            // Simulated response - in production, replace with actual chain query
            debug!(
                "Querying assignment for {}:{} (simulated)",
                challenge_id, submission_hash
            );

            // Return None to indicate no assignment found (conservative default)
            // In production, this would be replaced with actual chain data
            Ok::<Option<TaskAssignment>, StorageError>(None)
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => {
                warn!(
                    "Timeout querying task assignment for {}:{}",
                    challenge_id, submission_hash
                );
                Err(StorageError::Timeout)
            }
        }
    }

    /// Store validation result with retry logic
    async fn store_with_retry(&self, result: &ValidationResult) -> Result<String, StorageError> {
        let mut last_error = None;

        for attempt in 1..=self.config.max_retries {
            match self.store_on_chain(result).await {
                Ok(tx_hash) => {
                    if attempt > 1 {
                        info!("Validation stored on attempt {}: {}", attempt, tx_hash);
                    }
                    return Ok(tx_hash);
                }
                Err(e) => {
                    warn!(
                        "Store attempt {} failed for {}: {}",
                        attempt,
                        result.storage_key(),
                        e
                    );
                    last_error = Some(e);

                    // Don't retry on non-transient errors
                    if let Some(StorageError::NotAssigned) = last_error.as_ref() {
                        break;
                    }
                    if let Some(StorageError::AlreadyStored) = last_error.as_ref() {
                        break;
                    }

                    // Brief delay before retry
                    if attempt < self.config.max_retries {
                        tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or(StorageError::ChainError(
            "All retry attempts failed".to_string(),
        )))
    }

    /// Store a single validation result on chain
    async fn store_on_chain(&self, result: &ValidationResult) -> Result<String, StorageError> {
        let timeout_duration = Duration::from_secs(self.config.timeout_secs);

        let tx_result = timeout(timeout_duration, async {
            // In a real implementation, this would submit an extrinsic
            // to store the validation result on chain.
            //
            // Example pattern:
            // let client = self.client.client()?;
            // let signer = self.client.signer()?;
            // let call = create_store_validation_call(
            //     self.config.netuid,
            //     result.challenge_id,
            //     result.submission_hash,
            //     result.score,
            //     result.timestamp,
            // );
            // let tx_hash = client.sign_and_submit(call, signer, ExtrinsicWait::Finalized).await?;

            // Simulated transaction hash - in production, replace with actual chain submission
            let simulated_tx_hash = format!(
                "0x{:016x}{:016x}",
                result.timestamp as u64,
                rand::random::<u64>()
            );

            debug!(
                "Storing validation on chain for {} (simulated: {})",
                result.storage_key(),
                simulated_tx_hash
            );

            Ok::<String, StorageError>(simulated_tx_hash)
        })
        .await;

        match tx_result {
            Ok(inner) => inner,
            Err(_) => {
                warn!(
                    "Timeout storing validation for {}",
                    result.storage_key()
                );
                Err(StorageError::Timeout)
            }
        }
    }

    /// Query validations for a submission with timeout
    async fn query_validations_with_timeout(
        &self,
        challenge_id: &ChallengeId,
        submission_hash: &str,
    ) -> Result<Vec<ValidationResult>, StorageError> {
        let timeout_duration = Duration::from_secs(self.config.timeout_secs);

        let result = timeout(timeout_duration, async {
            // In a real implementation, this would query the blockchain
            // for all validation results for the given submission.
            //
            // Example pattern:
            // let client = self.client.client()?;
            // let storage_key = generate_validations_storage_key(challenge_id, submission_hash);
            // let data: Vec<ValidationResult> = client.storage().fetch_all(&storage_key).await?;

            debug!(
                "Querying validations for {}:{} (simulated)",
                challenge_id, submission_hash
            );

            // Return empty results (simulated)
            Ok::<Vec<ValidationResult>, StorageError>(Vec::new())
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => {
                warn!(
                    "Timeout querying validations for {}:{}",
                    challenge_id, submission_hash
                );
                Err(StorageError::Timeout)
            }
        }
    }

    /// Query epoch validations with timeout
    async fn query_epoch_validations_with_timeout(
        &self,
        challenge_id: &ChallengeId,
        epoch: u64,
    ) -> Result<Vec<ValidationResult>, StorageError> {
        let timeout_duration = Duration::from_secs(self.config.timeout_secs);

        let result = timeout(timeout_duration, async {
            // In a real implementation, this would query the blockchain
            // for all validation results in the given epoch.
            //
            // Example pattern:
            // let client = self.client.client()?;
            // let storage_key = generate_epoch_validations_key(challenge_id, epoch);
            // let data: Vec<ValidationResult> = client.storage().fetch_all(&storage_key).await?;

            debug!(
                "Querying epoch validations for {} epoch {} (simulated)",
                challenge_id, epoch
            );

            // Return empty results (simulated)
            Ok::<Vec<ValidationResult>, StorageError>(Vec::new())
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => {
                warn!(
                    "Timeout querying epoch validations for {} epoch {}",
                    challenge_id, epoch
                );
                Err(StorageError::Timeout)
            }
        }
    }

    /// Check if a validation exists on chain
    async fn query_validation_exists(
        &self,
        challenge_id: &ChallengeId,
        submission_hash: &str,
        validator_hotkey: &Hotkey,
    ) -> Result<bool, StorageError> {
        let timeout_duration = Duration::from_secs(self.config.timeout_secs);

        let result = timeout(timeout_duration, async {
            // In a real implementation, this would query the blockchain
            // to check if a validation exists.
            //
            // Example pattern:
            // let client = self.client.client()?;
            // let storage_key = generate_validation_key(challenge_id, submission_hash, validator_hotkey);
            // let exists = client.storage().exists(&storage_key).await?;

            debug!(
                "Checking validation exists for {}:{}:{} (simulated)",
                challenge_id,
                submission_hash,
                validator_hotkey.to_ss58()
            );

            // Return false (simulated - no existing validation)
            Ok::<bool, StorageError>(false)
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => {
                warn!(
                    "Timeout checking validation exists for {}:{}",
                    challenge_id, submission_hash
                );
                Err(StorageError::Timeout)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BittensorConfig;

    fn create_test_storage() -> BlockchainStorage {
        let config = BittensorConfig::local(1);
        let client = SubtensorClient::new(config);
        let storage_config = BlockchainStorageConfig::for_testing();
        BlockchainStorage::new(client, storage_config)
    }

    fn create_test_validation_result() -> ValidationResult {
        ValidationResult::new(
            "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".to_string(),
            ChallengeId::new(),
            "abc123def456".to_string(),
            0.85,
        )
    }

    fn create_test_assignment() -> TaskAssignment {
        TaskAssignment::new(
            ChallengeId::new(),
            "submission123".to_string(),
            "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".to_string(),
            3600, // 1 hour duration
        )
    }

    // =========================================================================
    // BlockchainStorageConfig Tests
    // =========================================================================

    #[test]
    fn test_config_default() {
        let config = BlockchainStorageConfig::default();
        assert_eq!(config.netuid, 100);
        assert_eq!(config.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(config.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(config.min_consensus_validators, DEFAULT_MIN_CONSENSUS_VALIDATORS);
        assert!(config.verify_assignments);
        assert!(config.check_duplicates);
    }

    #[test]
    fn test_config_with_netuid() {
        let config = BlockchainStorageConfig::with_netuid(42);
        assert_eq!(config.netuid, 42);
        assert_eq!(config.max_retries, DEFAULT_MAX_RETRIES);
    }

    #[test]
    fn test_config_for_testing() {
        let config = BlockchainStorageConfig::for_testing();
        assert_eq!(config.netuid, 1);
        assert_eq!(config.max_retries, 1);
        assert_eq!(config.timeout_secs, 5);
        assert_eq!(config.min_consensus_validators, 1);
        assert!(!config.verify_assignments);
        assert!(!config.check_duplicates);
    }

    #[test]
    fn test_config_validate_success() {
        let config = BlockchainStorageConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validate_zero_timeout() {
        let mut config = BlockchainStorageConfig::default();
        config.timeout_secs = 0;
        assert!(matches!(
            config.validate(),
            Err(StorageError::InvalidConfig(_))
        ));
    }

    #[test]
    fn test_config_validate_zero_consensus() {
        let mut config = BlockchainStorageConfig::default();
        config.min_consensus_validators = 0;
        assert!(matches!(
            config.validate(),
            Err(StorageError::InvalidConfig(_))
        ));
    }

    // =========================================================================
    // ValidationResult Tests
    // =========================================================================

    #[test]
    fn test_validation_result_new() {
        let result = create_test_validation_result();
        assert!(!result.validator_hotkey.is_empty());
        assert!(!result.submission_hash.is_empty());
        assert!((result.score - 0.85).abs() < f64::EPSILON);
        assert!(result.timestamp > 0);
        assert_eq!(result.block_number, 0);
    }

    #[test]
    fn test_validation_result_score_clamping() {
        let result = ValidationResult::new(
            "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".to_string(),
            ChallengeId::new(),
            "hash".to_string(),
            1.5, // Over 1.0
        );
        assert!((result.score - 1.0).abs() < f64::EPSILON);

        let result2 = ValidationResult::new(
            "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".to_string(),
            ChallengeId::new(),
            "hash".to_string(),
            -0.5, // Under 0.0
        );
        assert!(result2.score.abs() < f64::EPSILON);
    }

    #[test]
    fn test_validation_result_storage_key() {
        let result = create_test_validation_result();
        let key = result.storage_key();
        assert!(key.contains(&result.challenge_id.to_string()));
        assert!(key.contains(&result.submission_hash));
        assert!(key.contains(&result.validator_hotkey));
    }

    #[test]
    fn test_validation_result_validate_success() {
        let result = create_test_validation_result();
        assert!(result.validate().is_ok());
    }

    #[test]
    fn test_validation_result_validate_empty_hotkey() {
        let mut result = create_test_validation_result();
        result.validator_hotkey = String::new();
        assert!(matches!(
            result.validate(),
            Err(StorageError::InvalidConfig(_))
        ));
    }

    #[test]
    fn test_validation_result_validate_empty_hash() {
        let mut result = create_test_validation_result();
        result.submission_hash = String::new();
        assert!(matches!(
            result.validate(),
            Err(StorageError::InvalidConfig(_))
        ));
    }

    #[test]
    fn test_validation_result_validate_invalid_score() {
        let mut result = create_test_validation_result();
        result.score = 2.0; // Invalid (not clamped during manual set)
        assert!(matches!(
            result.validate(),
            Err(StorageError::InvalidConfig(_))
        ));
    }

    // =========================================================================
    // TaskAssignment Tests
    // =========================================================================

    #[test]
    fn test_task_assignment_new() {
        let assignment = create_test_assignment();
        assert!(!assignment.submission_hash.is_empty());
        assert!(!assignment.assigned_validator.is_empty());
        assert!(assignment.assigned_at > 0);
        assert!(assignment.expires_at > assignment.assigned_at);
    }

    #[test]
    fn test_task_assignment_is_expired() {
        let mut assignment = create_test_assignment();
        assert!(!assignment.is_expired());

        // Set expires_at to the past
        assignment.expires_at = chrono::Utc::now().timestamp() - 100;
        assert!(assignment.is_expired());
    }

    #[test]
    fn test_task_assignment_is_for_validator() {
        let assignment = create_test_assignment();
        assert!(assignment.is_for_validator(&assignment.assigned_validator));
        assert!(!assignment.is_for_validator("different_validator"));
    }

    #[test]
    fn test_task_assignment_storage_key() {
        let assignment = create_test_assignment();
        let key = assignment.storage_key();
        assert!(key.contains(&assignment.challenge_id.to_string()));
        assert!(key.contains(&assignment.submission_hash));
    }

    // =========================================================================
    // StorageCache Tests
    // =========================================================================

    #[test]
    fn test_cache_add_and_get_validation() {
        let mut cache = StorageCache::new(100);
        let result = create_test_validation_result();
        let key = result.storage_key();

        cache.add_validation(result.clone());

        assert!(cache.has_stored(&key));
        assert!(cache.get_validation(&key).is_some());
        assert_eq!(cache.get_validation(&key).unwrap().score, result.score);
    }

    #[test]
    fn test_cache_add_and_get_assignment() {
        let mut cache = StorageCache::new(100);
        let assignment = create_test_assignment();
        let key = assignment.storage_key();

        cache.add_assignment(assignment.clone());

        assert!(cache.get_assignment(&key).is_some());
        assert_eq!(
            cache.get_assignment(&key).unwrap().submission_hash,
            assignment.submission_hash
        );
    }

    #[test]
    fn test_cache_eviction() {
        let mut cache = StorageCache::new(2);

        for i in 0..5 {
            let mut result = create_test_validation_result();
            result.submission_hash = format!("hash_{}", i);
            cache.add_validation(result);
        }

        // Cache should have max 2 entries
        assert!(cache.validations.len() <= 2);
    }

    #[test]
    fn test_cache_clear() {
        let mut cache = StorageCache::new(100);
        cache.add_validation(create_test_validation_result());
        cache.add_assignment(create_test_assignment());

        cache.clear();

        assert!(cache.validations.is_empty());
        assert!(cache.assignments.is_empty());
        assert!(cache.stored_keys.is_empty());
    }

    // =========================================================================
    // BlockchainStorage Tests
    // =========================================================================

    #[test]
    fn test_storage_creation() {
        let storage = create_test_storage();
        assert_eq!(storage.config().netuid, 1);
    }

    #[test]
    fn test_storage_with_cache_size() {
        let config = BittensorConfig::local(1);
        let client = SubtensorClient::new(config);
        let storage_config = BlockchainStorageConfig::for_testing();
        let storage = BlockchainStorage::with_cache_size(client, storage_config, 500);
        assert_eq!(storage.config().netuid, 1);
    }

    #[tokio::test]
    async fn test_storage_cache_stats() {
        let storage = create_test_storage();
        let (validations, assignments, stored) = storage.cache_stats().await;
        assert_eq!(validations, 0);
        assert_eq!(assignments, 0);
        assert_eq!(stored, 0);
    }

    #[tokio::test]
    async fn test_storage_clear_cache() {
        let storage = create_test_storage();

        // Add some data to cache
        {
            let mut cache = storage.cache.write().await;
            cache.add_validation(create_test_validation_result());
        }

        let (validations, _, _) = storage.cache_stats().await;
        assert_eq!(validations, 1);

        storage.clear_cache().await;

        let (validations, _, _) = storage.cache_stats().await;
        assert_eq!(validations, 0);
    }

    #[tokio::test]
    async fn test_storage_store_validation() {
        let storage = create_test_storage();
        let result = create_test_validation_result();

        // Store should succeed (with simulated chain)
        let tx_result = storage.store_validation(&result).await;
        assert!(tx_result.is_ok());

        // Verify it was cached
        let (validations, _, stored) = storage.cache_stats().await;
        assert_eq!(validations, 1);
        assert_eq!(stored, 1);
    }

    #[tokio::test]
    async fn test_storage_duplicate_detection() {
        let mut storage_config = BlockchainStorageConfig::for_testing();
        storage_config.check_duplicates = true;

        let config = BittensorConfig::local(1);
        let client = SubtensorClient::new(config);
        let storage = BlockchainStorage::new(client, storage_config);

        let result = create_test_validation_result();

        // First store succeeds
        let tx1 = storage.store_validation(&result).await;
        assert!(tx1.is_ok());

        // Second store fails with AlreadyStored
        let tx2 = storage.store_validation(&result).await;
        assert!(matches!(tx2, Err(StorageError::AlreadyStored)));
    }

    #[tokio::test]
    async fn test_storage_read_validations_empty() {
        let mut storage_config = BlockchainStorageConfig::for_testing();
        storage_config.min_consensus_validators = 0; // Allow empty results

        let config = BittensorConfig::local(1);
        let client = SubtensorClient::new(config);
        let storage = BlockchainStorage::new(client, storage_config);

        let challenge_id = ChallengeId::new();
        let results = storage
            .read_validations(&challenge_id, "test_hash")
            .await;

        assert!(results.is_ok());
        assert!(results.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_storage_get_epoch_validations() {
        let storage = create_test_storage();
        let challenge_id = ChallengeId::new();

        let results = storage.get_epoch_validations(&challenge_id, 1).await;
        assert!(results.is_ok());
    }

    #[tokio::test]
    async fn test_storage_has_stored_validation() {
        let storage = create_test_storage();
        let challenge_id = ChallengeId::new();
        let validator = Hotkey([1u8; 32]);

        let exists = storage
            .has_stored_validation(&challenge_id, "test_hash", &validator)
            .await;

        assert!(exists.is_ok());
        // Simulated: returns false (no existing validation)
        assert!(!exists.unwrap());
    }

    #[tokio::test]
    async fn test_storage_is_task_assigned_disabled() {
        let mut storage_config = BlockchainStorageConfig::for_testing();
        storage_config.verify_assignments = false;

        let config = BittensorConfig::local(1);
        let client = SubtensorClient::new(config);
        let storage = BlockchainStorage::new(client, storage_config);

        let challenge_id = ChallengeId::new();
        let validator = Hotkey([1u8; 32]);

        // With verification disabled, always returns true
        let assigned = storage
            .is_task_assigned(&challenge_id, "test_hash", &validator)
            .await;

        assert!(assigned.is_ok());
        assert!(assigned.unwrap());
    }

    #[tokio::test]
    async fn test_storage_is_task_assigned_enabled() {
        let mut storage_config = BlockchainStorageConfig::for_testing();
        storage_config.verify_assignments = true;

        let config = BittensorConfig::local(1);
        let client = SubtensorClient::new(config);
        let storage = BlockchainStorage::new(client, storage_config);

        let challenge_id = ChallengeId::new();
        let validator = Hotkey([1u8; 32]);

        // With verification enabled and no assignment found, returns false
        let assigned = storage
            .is_task_assigned(&challenge_id, "test_hash", &validator)
            .await;

        assert!(assigned.is_ok());
        assert!(!assigned.unwrap());
    }

    // =========================================================================
    // StorageError Tests
    // =========================================================================

    #[test]
    fn test_storage_error_display() {
        let errors = vec![
            StorageError::NotAssigned,
            StorageError::AlreadyStored,
            StorageError::ChainError("test error".to_string()),
            StorageError::Timeout,
            StorageError::ConsensusFailed {
                required: 3,
                received: 1,
            },
            StorageError::InvalidConfig("bad config".to_string()),
            StorageError::NotConnected,
        ];

        for error in errors {
            let msg = format!("{}", error);
            assert!(!msg.is_empty());
        }
    }

    #[test]
    fn test_storage_error_debug() {
        let error = StorageError::ConsensusFailed {
            required: 5,
            received: 2,
        };
        let debug = format!("{:?}", error);
        assert!(debug.contains("ConsensusFailed"));
        assert!(debug.contains("5"));
        assert!(debug.contains("2"));
    }

    // =========================================================================
    // Integration-style Tests
    // =========================================================================

    #[tokio::test]
    async fn test_full_workflow() {
        let storage = create_test_storage();

        // Create a validation result
        let result = ValidationResult::new(
            "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".to_string(),
            ChallengeId::new(),
            "submission_hash_123".to_string(),
            0.95,
        );

        // Store the result
        let tx_hash = storage.store_validation(&result).await;
        assert!(tx_hash.is_ok());

        // Check cache was updated
        let (validations, _, stored) = storage.cache_stats().await;
        assert_eq!(validations, 1);
        assert_eq!(stored, 1);

        // Read validations (will be empty in simulation, but operation succeeds)
        let read_results = storage
            .read_validations(&result.challenge_id, &result.submission_hash)
            .await;
        // Min consensus is 1, so empty results still pass
        assert!(read_results.is_ok());

        // Clear cache and verify
        storage.clear_cache().await;
        let (validations, _, stored) = storage.cache_stats().await;
        assert_eq!(validations, 0);
        assert_eq!(stored, 0);
    }

    #[tokio::test]
    async fn test_assignment_caching() {
        let storage = create_test_storage();
        let challenge_id = ChallengeId::new();
        let validator = Hotkey([1u8; 32]);

        // First query populates cache (returns false because no assignment found)
        let result1 = storage
            .is_task_assigned(&challenge_id, "hash1", &validator)
            .await;
        assert!(result1.is_ok());

        // Manually add assignment to cache
        {
            let mut cache = storage.cache.write().await;
            let assignment = TaskAssignment::new(
                challenge_id,
                "hash1".to_string(),
                validator.to_ss58(),
                3600,
            );
            cache.add_assignment(assignment);
        }

        // Second query should use cache and find the assignment
        let result2 = storage
            .is_task_assigned(&challenge_id, "hash1", &validator)
            .await;
        assert!(result2.is_ok());
        assert!(result2.unwrap());
    }
}
