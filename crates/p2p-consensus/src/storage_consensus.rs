//! P2P consensus for blockchain storage verification
//!
//! This module provides consensus mechanisms for verifying blockchain storage reads
//! across validators. It uses gossipsub infrastructure to propagate verification
//! requests and collect responses, implementing stake-weighted voting for consensus.

use crate::validator::ValidatorSet;
use parking_lot::RwLock;
use platform_core::{ChallengeId, Hotkey, Keypair};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Message for storage verification requests
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StorageVerifyRequest {
    /// Unique identifier for this verification request
    pub request_id: String,
    /// Challenge being verified
    pub challenge_id: ChallengeId,
    /// Hash of the submission being verified
    pub submission_hash: String,
    /// Hotkey of the validator requesting verification
    pub requester: Hotkey,
    /// Timestamp when request was created (unix millis)
    pub timestamp: i64,
}

/// Response to a storage verification request
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StorageVerifyResponse {
    /// Request ID being responded to
    pub request_id: String,
    /// Validator providing the response
    pub validator: Hotkey,
    /// Stored validation data retrieved from blockchain
    pub values: Vec<StoredValidation>,
    /// Block number where data was read
    pub block_number: u64,
    /// Signature over (request_id, values_hash, block_number)
    pub signature: Vec<u8>,
}

/// Validation data stored on blockchain
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredValidation {
    /// Hotkey of the validator who submitted this validation
    pub validator_hotkey: String,
    /// Score assigned by the validator
    pub score: f64,
    /// Timestamp when validation was submitted
    pub timestamp: i64,
}

/// Result of consensus verification process
#[derive(Clone, Debug)]
pub struct ConsensusResult {
    /// Whether consensus was reached and values are verified
    pub verified: bool,
    /// The agreed-upon values (majority consensus)
    pub values: Vec<StoredValidation>,
    /// Number of validators that agreed on the values
    pub agreeing_validators: usize,
    /// Total number of validators that responded
    pub total_validators: usize,
    /// Total stake across all responding validators
    pub total_stake: u64,
    /// Stake of validators that agreed on the values
    pub agreeing_stake: u64,
}

/// Configuration for storage consensus
pub struct StorageConsensusConfig {
    /// Required percentage of stake for consensus (e.g., 0.67 for 2/3 majority)
    pub quorum_percentage: f64,
    /// Timeout for collecting responses in milliseconds
    pub timeout_ms: u64,
    /// Minimum number of validators required for consensus
    pub min_validators: usize,
}

impl Default for StorageConsensusConfig {
    fn default() -> Self {
        Self {
            quorum_percentage: 0.67,
            timeout_ms: 30_000,
            min_validators: 3,
        }
    }
}

impl StorageConsensusConfig {
    /// Create config for testing with relaxed requirements
    #[cfg(test)]
    pub fn for_testing() -> Self {
        Self {
            quorum_percentage: 0.51,
            timeout_ms: 5_000,
            min_validators: 1,
        }
    }
}

/// Internal state for a pending verification request
struct PendingVerification {
    /// The original request
    request: StorageVerifyRequest,
    /// Responses received so far (validator -> response)
    responses: HashMap<Hotkey, StorageVerifyResponse>,
    /// When this verification was created (unix millis)
    created_at: i64,
}

impl PendingVerification {
    fn new(request: StorageVerifyRequest) -> Self {
        Self {
            request,
            responses: HashMap::new(),
            created_at: chrono::Utc::now().timestamp_millis(),
        }
    }

    fn is_expired(&self, timeout_ms: u64) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        (now - self.created_at) > timeout_ms as i64
    }
}

/// P2P consensus mechanism for verifying blockchain storage reads
///
/// This struct coordinates verification of blockchain storage data across
/// multiple validators using gossipsub for message propagation and
/// stake-weighted voting for consensus.
pub struct StorageConsensus {
    /// Reference to the validator set for stake lookups and signature verification
    validator_set: Arc<ValidatorSet>,
    /// Pending verification requests awaiting responses
    pending_requests: Arc<RwLock<HashMap<String, PendingVerification>>>,
    /// Configuration for consensus parameters
    config: StorageConsensusConfig,
}

impl StorageConsensus {
    /// Create a new StorageConsensus instance
    ///
    /// # Arguments
    /// * `validator_set` - Reference to the validator set for stake lookups
    /// * `config` - Configuration parameters for consensus
    pub fn new(validator_set: Arc<ValidatorSet>, config: StorageConsensusConfig) -> Self {
        Self {
            validator_set,
            pending_requests: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    /// Create a verification request for blockchain storage data
    ///
    /// This creates a request that should be broadcast to other validators
    /// via gossipsub. The request includes all necessary information for
    /// validators to look up the data and respond.
    ///
    /// # Arguments
    /// * `challenge_id` - The challenge being verified
    /// * `submission_hash` - Hash of the submission to verify
    /// * `requester` - Hotkey of the requesting validator
    ///
    /// # Returns
    /// A `StorageVerifyRequest` ready to be broadcast to the network
    pub fn create_request(
        &self,
        challenge_id: ChallengeId,
        submission_hash: String,
        requester: Hotkey,
    ) -> StorageVerifyRequest {
        let timestamp = chrono::Utc::now().timestamp_millis();

        // Generate unique request ID from components
        let request_id = generate_request_id(&challenge_id, &submission_hash, &requester, timestamp);

        let request = StorageVerifyRequest {
            request_id: request_id.clone(),
            challenge_id,
            submission_hash,
            requester,
            timestamp,
        };

        // Store as pending
        let pending = PendingVerification::new(request.clone());
        self.pending_requests.write().insert(request_id, pending);

        info!(
            request_id = %request.request_id,
            challenge_id = %request.challenge_id,
            "Created storage verification request"
        );

        request
    }

    /// Handle an incoming verification request from another validator
    ///
    /// This processes a request received via gossipsub and creates a response
    /// containing the local validator's view of the stored data.
    ///
    /// # Arguments
    /// * `request` - The verification request to handle
    /// * `local_values` - The stored validation data from local blockchain read
    /// * `block_number` - The block number where data was read
    /// * `signer` - Keypair for signing the response
    ///
    /// # Returns
    /// A `StorageVerifyResponse` to be broadcast back to the network
    pub fn handle_request(
        &self,
        request: &StorageVerifyRequest,
        local_values: Vec<StoredValidation>,
        block_number: u64,
        signer: &Keypair,
    ) -> StorageVerifyResponse {
        // Compute hash of values for signing
        let values_hash = compute_values_hash(&local_values);

        // Create signing data
        #[derive(Serialize)]
        struct ResponseSigningData<'a> {
            request_id: &'a str,
            values_hash: [u8; 32],
            block_number: u64,
        }

        let signing_data = ResponseSigningData {
            request_id: &request.request_id,
            values_hash,
            block_number,
        };

        let signing_bytes = bincode::serialize(&signing_data).unwrap_or_default();
        let signature = signer.sign_bytes(&signing_bytes).unwrap_or_default();

        let response = StorageVerifyResponse {
            request_id: request.request_id.clone(),
            validator: signer.hotkey(),
            values: local_values,
            block_number,
            signature,
        };

        debug!(
            request_id = %request.request_id,
            validator = %signer.hotkey().to_hex(),
            block_number,
            "Created storage verification response"
        );

        response
    }

    /// Handle an incoming verification response
    ///
    /// This processes a response received via gossipsub and checks if
    /// consensus has been reached. Returns the consensus result if quorum
    /// has been achieved.
    ///
    /// # Arguments
    /// * `response` - The verification response to process
    ///
    /// # Returns
    /// `Some(ConsensusResult)` if consensus has been reached, `None` otherwise
    pub fn handle_response(&self, response: StorageVerifyResponse) -> Option<ConsensusResult> {
        // Verify the response signature
        if !self.verify_response_signature(&response) {
            warn!(
                request_id = %response.request_id,
                validator = %response.validator.to_hex(),
                "Invalid signature on storage verification response"
            );
            return None;
        }

        // Verify the validator is in our validator set
        if !self.validator_set.is_validator(&response.validator) {
            warn!(
                request_id = %response.request_id,
                validator = %response.validator.to_hex(),
                "Response from unknown validator"
            );
            return None;
        }

        let mut pending = self.pending_requests.write();

        // Find the pending request
        let verification = match pending.get_mut(&response.request_id) {
            Some(v) => v,
            None => {
                debug!(
                    request_id = %response.request_id,
                    "No pending request found for response"
                );
                return None;
            }
        };

        // Check for duplicate response
        if verification.responses.contains_key(&response.validator) {
            debug!(
                request_id = %response.request_id,
                validator = %response.validator.to_hex(),
                "Duplicate response from validator, ignoring"
            );
            return None;
        }

        // Add the response
        verification
            .responses
            .insert(response.validator.clone(), response);

        // Check if we have enough responses for consensus
        let request_id = verification.request.request_id.clone();
        drop(pending);

        self.check_consensus(&request_id)
    }

    /// Check if consensus has been reached for a request
    ///
    /// This examines all collected responses and determines if the quorum
    /// requirements have been met. Uses stake-weighted voting to determine
    /// the majority position.
    ///
    /// # Arguments
    /// * `request_id` - The request ID to check
    ///
    /// # Returns
    /// `Some(ConsensusResult)` if consensus can be determined, `None` otherwise
    pub fn check_consensus(&self, request_id: &str) -> Option<ConsensusResult> {
        let pending = self.pending_requests.read();

        let verification = match pending.get(request_id) {
            Some(v) => v,
            None => return None,
        };

        let responses = &verification.responses;

        // Need minimum validators
        if responses.len() < self.config.min_validators {
            debug!(
                request_id,
                responses = responses.len(),
                min_required = self.config.min_validators,
                "Not enough responses for consensus"
            );
            return None;
        }

        // Group responses by their values hash (for comparison)
        let mut value_groups: HashMap<[u8; 32], Vec<(&Hotkey, &StorageVerifyResponse)>> =
            HashMap::new();

        for (validator, response) in responses {
            let values_hash = compute_values_hash(&response.values);
            value_groups
                .entry(values_hash)
                .or_default()
                .push((validator, response));
        }

        // Calculate stake for each group
        let mut group_stakes: Vec<([u8; 32], u64, Vec<StoredValidation>)> = value_groups
            .into_iter()
            .map(|(hash, validators)| {
                let stake: u64 = validators
                    .iter()
                    .filter_map(|(hotkey, _)| {
                        self.validator_set
                            .get_validator(hotkey)
                            .map(|v| v.stake)
                    })
                    .sum();
                let values = validators
                    .first()
                    .map(|(_, r)| r.values.clone())
                    .unwrap_or_default();
                (hash, stake, values)
            })
            .collect();

        // Sort by stake descending
        group_stakes.sort_by(|a, b| b.1.cmp(&a.1));

        // Calculate total stake
        let total_stake: u64 = group_stakes.iter().map(|(_, stake, _)| stake).sum();

        if total_stake == 0 {
            warn!(request_id, "No stake found for responding validators");
            return None;
        }

        // Check if the largest group meets quorum
        let (_, agreeing_stake, consensus_values) = group_stakes.first()?;

        let stake_percentage = *agreeing_stake as f64 / total_stake as f64;
        let agreeing_validators = responses
            .iter()
            .filter(|(_, r)| compute_values_hash(&r.values) == compute_values_hash(&consensus_values))
            .count();

        let verified = stake_percentage >= self.config.quorum_percentage
            && agreeing_validators >= self.config.min_validators;

        let result = ConsensusResult {
            verified,
            values: consensus_values.clone(),
            agreeing_validators,
            total_validators: responses.len(),
            total_stake,
            agreeing_stake: *agreeing_stake,
        };

        if verified {
            info!(
                request_id,
                agreeing_validators,
                total_validators = responses.len(),
                stake_percentage = format!("{:.2}%", stake_percentage * 100.0),
                "Storage verification consensus reached"
            );
        } else {
            debug!(
                request_id,
                agreeing_validators,
                total_validators = responses.len(),
                stake_percentage = format!("{:.2}%", stake_percentage * 100.0),
                quorum_required = format!("{:.2}%", self.config.quorum_percentage * 100.0),
                "Storage verification consensus not yet reached"
            );
        }

        Some(result)
    }

    /// Clean up expired pending requests
    ///
    /// This should be called periodically to remove requests that have
    /// timed out without reaching consensus.
    pub fn cleanup_expired(&self) {
        let timeout_ms = self.config.timeout_ms;
        let mut pending = self.pending_requests.write();

        let expired: Vec<String> = pending
            .iter()
            .filter(|(_, v)| v.is_expired(timeout_ms))
            .map(|(id, _)| id.clone())
            .collect();

        for id in &expired {
            debug!(request_id = %id, "Removing expired storage verification request");
            pending.remove(id);
        }

        if !expired.is_empty() {
            info!(
                count = expired.len(),
                "Cleaned up expired storage verification requests"
            );
        }
    }

    /// Get the number of pending verification requests
    pub fn pending_count(&self) -> usize {
        self.pending_requests.read().len()
    }

    /// Verify the signature on a response
    fn verify_response_signature(&self, response: &StorageVerifyResponse) -> bool {
        let values_hash = compute_values_hash(&response.values);

        #[derive(Serialize)]
        struct ResponseSigningData<'a> {
            request_id: &'a str,
            values_hash: [u8; 32],
            block_number: u64,
        }

        let signing_data = ResponseSigningData {
            request_id: &response.request_id,
            values_hash,
            block_number: response.block_number,
        };

        let signing_bytes = match bincode::serialize(&signing_data) {
            Ok(bytes) => bytes,
            Err(_) => return false,
        };

        self.validator_set
            .verify_signature(&response.validator, &signing_bytes, &response.signature)
            .unwrap_or(false)
    }
}

/// Generate a unique request ID from request components
fn generate_request_id(
    challenge_id: &ChallengeId,
    submission_hash: &str,
    requester: &Hotkey,
    timestamp: i64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(challenge_id.to_string().as_bytes());
    hasher.update(submission_hash.as_bytes());
    hasher.update(&requester.0);
    hasher.update(&timestamp.to_le_bytes());
    let hash: [u8; 32] = hasher.finalize().into();
    hex::encode(&hash[..16]) // Use first 16 bytes for reasonable length
}

/// Compute a hash of stored validation values for comparison
fn compute_values_hash(values: &[StoredValidation]) -> [u8; 32] {
    #[derive(Serialize)]
    struct HashableValues<'a> {
        values: &'a [StoredValidation],
    }

    let hashable = HashableValues { values };
    let bytes = bincode::serialize(&hashable).unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator::ValidatorRecord;

    fn create_test_validator_set() -> Arc<ValidatorSet> {
        let keypair = Keypair::generate();
        let set = Arc::new(ValidatorSet::new(keypair.clone(), 0));

        // Register the local keypair as validator
        let record = ValidatorRecord::new(keypair.hotkey(), 10_000);
        set.register_validator(record).unwrap();

        set
    }

    fn create_test_validators(count: usize) -> (Arc<ValidatorSet>, Vec<Keypair>) {
        let local_keypair = Keypair::generate();
        let set = Arc::new(ValidatorSet::new(local_keypair.clone(), 0));

        let mut keypairs = vec![local_keypair];

        for i in 0..count {
            let kp = Keypair::generate();
            let record = ValidatorRecord::new(kp.hotkey(), 10_000 + (i as u64 * 1_000));
            set.register_validator(record).unwrap();
            keypairs.push(kp);
        }

        (set, keypairs)
    }

    #[test]
    fn test_create_request() {
        let validator_set = create_test_validator_set();
        let consensus = StorageConsensus::new(validator_set, StorageConsensusConfig::for_testing());

        let challenge_id = ChallengeId::new();
        let submission_hash = "abc123".to_string();
        let requester = Hotkey([1u8; 32]);

        let request = consensus.create_request(challenge_id, submission_hash.clone(), requester.clone());

        assert!(!request.request_id.is_empty());
        assert_eq!(request.challenge_id, challenge_id);
        assert_eq!(request.submission_hash, submission_hash);
        assert_eq!(request.requester, requester);
        assert!(request.timestamp > 0);

        // Should be tracked as pending
        assert_eq!(consensus.pending_count(), 1);
    }

    #[test]
    fn test_handle_request() {
        let validator_set = create_test_validator_set();
        let consensus = StorageConsensus::new(validator_set, StorageConsensusConfig::for_testing());

        let request = StorageVerifyRequest {
            request_id: "test-request-1".to_string(),
            challenge_id: ChallengeId::new(),
            submission_hash: "abc123".to_string(),
            requester: Hotkey([1u8; 32]),
            timestamp: chrono::Utc::now().timestamp_millis(),
        };

        let local_values = vec![StoredValidation {
            validator_hotkey: "validator1".to_string(),
            score: 0.85,
            timestamp: 1234567890,
        }];

        let signer = Keypair::generate();
        let response = consensus.handle_request(&request, local_values.clone(), 100, &signer);

        assert_eq!(response.request_id, request.request_id);
        assert_eq!(response.validator, signer.hotkey());
        assert_eq!(response.values, local_values);
        assert_eq!(response.block_number, 100);
        assert!(!response.signature.is_empty());
    }

    #[test]
    fn test_consensus_reached() {
        let (validator_set, keypairs) = create_test_validators(3);
        let consensus = StorageConsensus::new(validator_set.clone(), StorageConsensusConfig::for_testing());

        // Create a request
        let challenge_id = ChallengeId::new();
        let request = consensus.create_request(
            challenge_id,
            "submission123".to_string(),
            keypairs[0].hotkey(),
        );

        // All validators agree on the same values
        let agreed_values = vec![StoredValidation {
            validator_hotkey: "validator1".to_string(),
            score: 0.9,
            timestamp: 1234567890,
        }];

        // Collect responses from all validators
        for kp in &keypairs {
            let response = consensus.handle_request(&request, agreed_values.clone(), 100, kp);
            let result = consensus.handle_response(response);

            // After last response, we should have consensus
            if result.is_some() {
                let res = result.unwrap();
                assert!(res.verified);
                assert_eq!(res.values, agreed_values);
                assert!(res.agreeing_validators >= 1);
                return;
            }
        }
    }

    #[test]
    fn test_no_consensus_with_disagreement() {
        let (validator_set, keypairs) = create_test_validators(3);
        let mut config = StorageConsensusConfig::for_testing();
        config.quorum_percentage = 0.67;
        config.min_validators = 2;

        let consensus = StorageConsensus::new(validator_set.clone(), config);

        // Create a request
        let challenge_id = ChallengeId::new();
        let request = consensus.create_request(
            challenge_id,
            "submission123".to_string(),
            keypairs[0].hotkey(),
        );

        // Each validator reports different values
        for (i, kp) in keypairs.iter().enumerate() {
            let values = vec![StoredValidation {
                validator_hotkey: format!("validator{}", i),
                score: 0.5 + (i as f64 * 0.1),
                timestamp: 1234567890 + i as i64,
            }];

            let response = consensus.handle_request(&request, values, 100, kp);
            let result = consensus.handle_response(response);

            // With all different values, consensus should not be verified
            if let Some(res) = result {
                assert!(!res.verified || res.agreeing_validators < 2);
            }
        }
    }

    #[test]
    fn test_cleanup_expired() {
        let validator_set = create_test_validator_set();
        let mut config = StorageConsensusConfig::for_testing();
        config.timeout_ms = 1; // 1ms timeout for testing

        let consensus = StorageConsensus::new(validator_set, config);

        // Create a request
        let challenge_id = ChallengeId::new();
        consensus.create_request(challenge_id, "test".to_string(), Hotkey([1u8; 32]));

        assert_eq!(consensus.pending_count(), 1);

        // Wait for expiration
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Cleanup should remove it
        consensus.cleanup_expired();

        assert_eq!(consensus.pending_count(), 0);
    }

    #[test]
    fn test_duplicate_response_ignored() {
        let (validator_set, keypairs) = create_test_validators(2);
        let consensus = StorageConsensus::new(validator_set.clone(), StorageConsensusConfig::for_testing());

        let challenge_id = ChallengeId::new();
        let request = consensus.create_request(
            challenge_id,
            "submission123".to_string(),
            keypairs[0].hotkey(),
        );

        let values = vec![StoredValidation {
            validator_hotkey: "validator1".to_string(),
            score: 0.85,
            timestamp: 1234567890,
        }];

        // First response from validator
        let response1 = consensus.handle_request(&request, values.clone(), 100, &keypairs[0]);
        let _result1 = consensus.handle_response(response1.clone());

        // Duplicate response should return None
        let result2 = consensus.handle_response(response1);
        assert!(result2.is_none());
    }

    #[test]
    fn test_response_from_unknown_validator() {
        let validator_set = create_test_validator_set();
        let consensus = StorageConsensus::new(validator_set, StorageConsensusConfig::for_testing());

        let challenge_id = ChallengeId::new();
        let request = consensus.create_request(
            challenge_id,
            "submission123".to_string(),
            Hotkey([1u8; 32]),
        );

        // Create response from unknown validator
        let unknown_kp = Keypair::generate();
        let values = vec![StoredValidation {
            validator_hotkey: "validator1".to_string(),
            score: 0.85,
            timestamp: 1234567890,
        }];

        let response = consensus.handle_request(&request, values, 100, &unknown_kp);
        let result = consensus.handle_response(response);

        // Should be rejected
        assert!(result.is_none());
    }

    #[test]
    fn test_stored_validation_equality() {
        let v1 = StoredValidation {
            validator_hotkey: "test".to_string(),
            score: 0.5,
            timestamp: 100,
        };

        let v2 = StoredValidation {
            validator_hotkey: "test".to_string(),
            score: 0.5,
            timestamp: 100,
        };

        let v3 = StoredValidation {
            validator_hotkey: "test".to_string(),
            score: 0.6,
            timestamp: 100,
        };

        assert_eq!(v1, v2);
        assert_ne!(v1, v3);
    }

    #[test]
    fn test_consensus_result_fields() {
        let result = ConsensusResult {
            verified: true,
            values: vec![StoredValidation {
                validator_hotkey: "test".to_string(),
                score: 0.9,
                timestamp: 1234567890,
            }],
            agreeing_validators: 3,
            total_validators: 4,
            total_stake: 100_000,
            agreeing_stake: 80_000,
        };

        assert!(result.verified);
        assert_eq!(result.agreeing_validators, 3);
        assert_eq!(result.total_validators, 4);
        assert_eq!(result.total_stake, 100_000);
        assert_eq!(result.agreeing_stake, 80_000);
    }

    #[test]
    fn test_default_config() {
        let config = StorageConsensusConfig::default();
        assert!((config.quorum_percentage - 0.67).abs() < f64::EPSILON);
        assert_eq!(config.timeout_ms, 30_000);
        assert_eq!(config.min_validators, 3);
    }

    #[test]
    fn test_generate_request_id_deterministic() {
        let challenge_id = ChallengeId::new();
        let submission_hash = "test123";
        let requester = Hotkey([1u8; 32]);
        let timestamp = 1234567890i64;

        let id1 = generate_request_id(&challenge_id, submission_hash, &requester, timestamp);
        let id2 = generate_request_id(&challenge_id, submission_hash, &requester, timestamp);

        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 32); // 16 bytes hex encoded
    }

    #[test]
    fn test_compute_values_hash_deterministic() {
        let values = vec![
            StoredValidation {
                validator_hotkey: "v1".to_string(),
                score: 0.8,
                timestamp: 100,
            },
            StoredValidation {
                validator_hotkey: "v2".to_string(),
                score: 0.9,
                timestamp: 200,
            },
        ];

        let hash1 = compute_values_hash(&values);
        let hash2 = compute_values_hash(&values);

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_compute_values_hash_different_for_different_values() {
        let values1 = vec![StoredValidation {
            validator_hotkey: "v1".to_string(),
            score: 0.8,
            timestamp: 100,
        }];

        let values2 = vec![StoredValidation {
            validator_hotkey: "v1".to_string(),
            score: 0.9,
            timestamp: 100,
        }];

        let hash1 = compute_values_hash(&values1);
        let hash2 = compute_values_hash(&values2);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_check_consensus_not_enough_validators() {
        let validator_set = create_test_validator_set();
        let mut config = StorageConsensusConfig::for_testing();
        config.min_validators = 5; // Require more validators than we have

        let consensus = StorageConsensus::new(validator_set, config);

        let challenge_id = ChallengeId::new();
        let request = consensus.create_request(
            challenge_id,
            "submission123".to_string(),
            Hotkey([1u8; 32]),
        );

        // Check consensus without any responses - should return None
        let result = consensus.check_consensus(&request.request_id);
        assert!(result.is_none());
    }

    #[test]
    fn test_stake_weighted_consensus() {
        let local_keypair = Keypair::generate();
        let set = Arc::new(ValidatorSet::new(local_keypair.clone(), 0));

        // Create validators with different stakes
        let high_stake_kp = Keypair::generate();
        let low_stake_kp = Keypair::generate();

        // High stake validator
        let high_stake_record = ValidatorRecord::new(high_stake_kp.hotkey(), 100_000);
        set.register_validator(high_stake_record).unwrap();

        // Low stake validator
        let low_stake_record = ValidatorRecord::new(low_stake_kp.hotkey(), 10_000);
        set.register_validator(low_stake_record).unwrap();

        let mut config = StorageConsensusConfig::for_testing();
        config.min_validators = 1;
        config.quorum_percentage = 0.5;

        let consensus = StorageConsensus::new(set, config);

        let challenge_id = ChallengeId::new();
        let request = consensus.create_request(
            challenge_id,
            "submission123".to_string(),
            local_keypair.hotkey(),
        );

        // High stake validator's values
        let high_stake_values = vec![StoredValidation {
            validator_hotkey: "validator1".to_string(),
            score: 0.9,
            timestamp: 1234567890,
        }];

        // Low stake validator's different values
        let low_stake_values = vec![StoredValidation {
            validator_hotkey: "validator1".to_string(),
            score: 0.5,
            timestamp: 1234567890,
        }];

        // Submit high stake response
        let high_response = consensus.handle_request(&request, high_stake_values.clone(), 100, &high_stake_kp);
        consensus.handle_response(high_response);

        // Submit low stake response
        let low_response = consensus.handle_request(&request, low_stake_values, 100, &low_stake_kp);
        let result = consensus.handle_response(low_response);

        // High stake values should win
        if let Some(res) = result {
            if res.verified {
                assert_eq!(res.values, high_stake_values);
            }
        }
    }
}
