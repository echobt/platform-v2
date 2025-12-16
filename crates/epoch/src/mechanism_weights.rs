//! Mechanism Weight Manager
//!
//! Groups weights from multiple challenges by mechanism_id for batch submission to Bittensor.
//! Each challenge is mapped to a mechanism (1:1 relationship).

use parking_lot::RwLock;
use platform_challenge_sdk::{ChallengeId, WeightAssignment};
use std::collections::HashMap;
use tracing::{debug, info};

/// Weight data for a single mechanism
#[derive(Clone, Debug)]
pub struct MechanismWeights {
    /// Mechanism ID (u8 as per Bittensor)
    pub mechanism_id: u8,
    /// Challenge that produced these weights
    pub challenge_id: ChallengeId,
    /// Agent UIDs (converted from hashes)
    pub uids: Vec<u16>,
    /// Normalized weights (u16 format for Bittensor)
    pub weights: Vec<u16>,
    /// Original float weights for reference
    pub raw_weights: Vec<WeightAssignment>,
}

impl MechanismWeights {
    pub fn new(
        mechanism_id: u8,
        challenge_id: ChallengeId,
        assignments: Vec<WeightAssignment>,
    ) -> Self {
        let (uids, weights) = Self::convert_to_bittensor_format(&assignments);
        Self {
            mechanism_id,
            challenge_id,
            uids,
            weights,
            raw_weights: assignments,
        }
    }

    /// Convert WeightAssignment to Bittensor format (uids: Vec<u16>, weights: Vec<u16>)
    fn convert_to_bittensor_format(assignments: &[WeightAssignment]) -> (Vec<u16>, Vec<u16>) {
        // Normalize weights to sum to 65535 (u16 max for Bittensor)
        let total: f64 = assignments.iter().map(|a| a.weight).sum();

        if total <= 0.0 || assignments.is_empty() {
            return (vec![], vec![]);
        }

        let mut uids = Vec::with_capacity(assignments.len());
        let mut weights = Vec::with_capacity(assignments.len());

        for (idx, assignment) in assignments.iter().enumerate() {
            // Use index as UID (in real implementation, map agent_hash to neuron UID)
            uids.push(idx as u16);
            // Normalize to u16 range (0-65535)
            let normalized = ((assignment.weight / total) * 65535.0).round() as u16;
            weights.push(normalized);
        }

        (uids, weights)
    }

    /// Get weights as tuple for batch submission
    pub fn as_batch_tuple(&self) -> (u8, Vec<u16>, Vec<u16>) {
        (self.mechanism_id, self.uids.clone(), self.weights.clone())
    }
}

/// Manages weights grouped by mechanism for an epoch
pub struct MechanismWeightManager {
    /// Epoch number
    epoch: u64,
    /// Weights per mechanism (mechanism_id -> MechanismWeights)
    weights: RwLock<HashMap<u8, MechanismWeights>>,
    /// Challenge to mechanism mapping
    challenge_mechanism_map: RwLock<HashMap<ChallengeId, u8>>,
}

impl MechanismWeightManager {
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch,
            weights: RwLock::new(HashMap::new()),
            challenge_mechanism_map: RwLock::new(HashMap::new()),
        }
    }

    /// Register a challenge with its mechanism
    pub fn register_challenge(&self, challenge_id: ChallengeId, mechanism_id: u8) {
        self.challenge_mechanism_map
            .write()
            .insert(challenge_id, mechanism_id);
        debug!(
            "Registered challenge {:?} with mechanism {}",
            challenge_id, mechanism_id
        );
    }

    /// Submit weights from a challenge
    pub fn submit_weights(
        &self,
        challenge_id: ChallengeId,
        mechanism_id: u8,
        weights: Vec<WeightAssignment>,
    ) {
        let mech_weights = MechanismWeights::new(mechanism_id, challenge_id, weights);
        self.weights.write().insert(mechanism_id, mech_weights);

        info!(
            "Submitted weights for mechanism {} from challenge {:?}: {} agents",
            mechanism_id,
            challenge_id,
            self.weights
                .read()
                .get(&mechanism_id)
                .map(|w| w.uids.len())
                .unwrap_or(0)
        );
    }

    /// Get all mechanism weights for batch submission
    pub fn get_all_mechanism_weights(&self) -> Vec<(u8, Vec<u16>, Vec<u16>)> {
        self.weights
            .read()
            .values()
            .map(|w| w.as_batch_tuple())
            .collect()
    }

    /// Get weights for a specific mechanism
    pub fn get_mechanism_weights(&self, mechanism_id: u8) -> Option<MechanismWeights> {
        self.weights.read().get(&mechanism_id).cloned()
    }

    /// Get mechanism ID for a challenge
    pub fn get_mechanism_for_challenge(&self, challenge_id: &ChallengeId) -> Option<u8> {
        self.challenge_mechanism_map
            .read()
            .get(challenge_id)
            .copied()
    }

    /// Get all registered mechanisms
    pub fn list_mechanisms(&self) -> Vec<u8> {
        self.weights.read().keys().copied().collect()
    }

    /// Clear all weights (for new epoch)
    pub fn clear(&self) {
        self.weights.write().clear();
    }

    /// Get epoch
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Get number of mechanisms with weights
    pub fn mechanism_count(&self) -> usize {
        self.weights.read().len()
    }
}

/// Commit data for a mechanism
#[derive(Clone, Debug)]
pub struct MechanismCommitment {
    pub mechanism_id: u8,
    pub epoch: u64,
    pub commit_hash: [u8; 32],
    pub salt: Vec<u8>,
}

impl MechanismCommitment {
    pub fn new(mechanism_id: u8, epoch: u64, weights: &MechanismWeights, salt: &[u8]) -> Self {
        let commit_hash = Self::compute_hash(&weights.uids, &weights.weights, salt);
        Self {
            mechanism_id,
            epoch,
            commit_hash,
            salt: salt.to_vec(),
        }
    }

    /// Compute commit hash for commit-reveal
    fn compute_hash(uids: &[u16], weights: &[u16], salt: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();

        // Hash UIDs
        for uid in uids {
            hasher.update(uid.to_le_bytes());
        }

        // Hash weights
        for w in weights {
            hasher.update(w.to_le_bytes());
        }

        // Hash salt
        hasher.update(salt);

        let result = hasher.finalize();
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&result);
        hash
    }

    /// Get commit hash as hex string
    pub fn hash_hex(&self) -> String {
        hex::encode(self.commit_hash)
    }
}

/// Manages commit-reveal per mechanism
pub struct MechanismCommitRevealManager {
    /// Current epoch
    epoch: RwLock<u64>,
    /// Commitments per mechanism
    commitments: RwLock<HashMap<u8, MechanismCommitment>>,
    /// Revealed weights per mechanism
    revealed: RwLock<HashMap<u8, MechanismWeights>>,
}

impl MechanismCommitRevealManager {
    pub fn new() -> Self {
        Self {
            epoch: RwLock::new(0),
            commitments: RwLock::new(HashMap::new()),
            revealed: RwLock::new(HashMap::new()),
        }
    }

    /// Start new epoch
    pub fn new_epoch(&self, epoch: u64) {
        *self.epoch.write() = epoch;
        self.commitments.write().clear();
        self.revealed.write().clear();
        info!("MechanismCommitReveal: new epoch {}", epoch);
    }

    /// Commit weights for a mechanism
    pub fn commit(&self, commitment: MechanismCommitment) {
        debug!(
            "Committing weights for mechanism {} epoch {}: hash={}",
            commitment.mechanism_id,
            commitment.epoch,
            commitment.hash_hex()
        );
        self.commitments
            .write()
            .insert(commitment.mechanism_id, commitment);
    }

    /// Reveal weights for a mechanism
    pub fn reveal(&self, mechanism_id: u8, weights: MechanismWeights) -> Result<(), String> {
        let commitment = self
            .commitments
            .read()
            .get(&mechanism_id)
            .cloned()
            .ok_or_else(|| format!("No commitment for mechanism {}", mechanism_id))?;

        // Verify hash matches
        let expected_hash =
            MechanismCommitment::compute_hash(&weights.uids, &weights.weights, &commitment.salt);

        if expected_hash != commitment.commit_hash {
            return Err(format!(
                "Commitment mismatch for mechanism {}: expected {}, got {}",
                mechanism_id,
                hex::encode(commitment.commit_hash),
                hex::encode(expected_hash)
            ));
        }

        debug!(
            "Revealed weights for mechanism {}: {} weights",
            mechanism_id,
            weights.weights.len()
        );
        self.revealed.write().insert(mechanism_id, weights);
        Ok(())
    }

    /// Get all revealed weights for batch submission
    pub fn get_revealed_weights(&self) -> Vec<(u8, Vec<u16>, Vec<u16>)> {
        self.revealed
            .read()
            .values()
            .map(|w| w.as_batch_tuple())
            .collect()
    }

    /// Check if all committed mechanisms have been revealed
    pub fn all_revealed(&self) -> bool {
        let commitments = self.commitments.read();
        let revealed = self.revealed.read();
        commitments.keys().all(|m| revealed.contains_key(m))
    }

    /// Get commitment for a mechanism
    pub fn get_commitment(&self, mechanism_id: u8) -> Option<MechanismCommitment> {
        self.commitments.read().get(&mechanism_id).cloned()
    }

    /// Get all commitments
    pub fn get_all_commitments(&self) -> Vec<MechanismCommitment> {
        self.commitments.read().values().cloned().collect()
    }
}

impl Default for MechanismCommitRevealManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mechanism_weights_conversion() {
        let assignments = vec![
            WeightAssignment::new("agent1".to_string(), 0.6),
            WeightAssignment::new("agent2".to_string(), 0.4),
        ];

        let mech_weights = MechanismWeights::new(1, ChallengeId::new(), assignments);

        assert_eq!(mech_weights.uids.len(), 2);
        assert_eq!(mech_weights.weights.len(), 2);

        // Check normalization (should sum close to 65535)
        let sum: u32 = mech_weights.weights.iter().map(|w| *w as u32).sum();
        assert!(sum >= 65530 && sum <= 65540); // Allow small rounding error
    }

    #[test]
    fn test_mechanism_weight_manager() {
        let manager = MechanismWeightManager::new(1);

        let challenge1 = ChallengeId::new();
        let challenge2 = ChallengeId::new();

        manager.register_challenge(challenge1, 1);
        manager.register_challenge(challenge2, 2);

        let weights1 = vec![WeightAssignment::new("a".to_string(), 0.5)];
        let weights2 = vec![WeightAssignment::new("b".to_string(), 0.5)];

        manager.submit_weights(challenge1, 1, weights1);
        manager.submit_weights(challenge2, 2, weights2);

        let all_weights = manager.get_all_mechanism_weights();
        assert_eq!(all_weights.len(), 2);
    }

    #[test]
    fn test_commit_reveal() {
        let manager = MechanismCommitRevealManager::new();
        manager.new_epoch(1);

        let weights = MechanismWeights::new(
            1,
            ChallengeId::new(),
            vec![WeightAssignment::new("agent".to_string(), 1.0)],
        );

        let salt = b"test_salt".to_vec();
        let commitment = MechanismCommitment::new(1, 1, &weights, &salt);

        manager.commit(commitment);
        assert!(manager.reveal(1, weights).is_ok());
        assert!(manager.all_revealed());
    }
}
