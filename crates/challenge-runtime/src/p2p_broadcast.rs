//! P2P Broadcast for Challenge Results
//!
//! Broadcasts evaluation results to other validators
//! and aggregates received results for consensus.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// P2P message types for challenge
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChallengeMessage {
    /// Broadcast evaluation result
    EvaluationResult {
        agent_hash: String,
        validator_hotkey: String,
        score: f64,
        result_hash: String,
        epoch: u64,
        signature: Vec<u8>,
    },
    /// Request result for an agent
    RequestResult {
        agent_hash: String,
        requester: String,
    },
    /// Consensus reached notification
    ConsensusReached {
        agent_hash: String,
        score: f64,
        validator_count: u32,
        epoch: u64,
    },
    /// Weight commit announcement
    WeightCommit {
        validator_hotkey: String,
        commit_hash: [u8; 32],
        epoch: u64,
    },
    /// Weight reveal announcement
    WeightReveal {
        validator_hotkey: String,
        weights: Vec<(u16, u16)>,
        salt: [u8; 32],
        epoch: u64,
    },
}

/// Received result from a validator
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceivedResult {
    pub agent_hash: String,
    pub validator_hotkey: String,
    pub score: f64,
    pub result_hash: String,
    pub epoch: u64,
    pub received_at: u64,
}

/// P2P broadcast configuration
#[derive(Debug, Clone)]
pub struct P2PBroadcastConfig {
    /// This validator's hotkey
    pub validator_hotkey: String,
    /// Consensus threshold (2/3 = 0.67)
    pub consensus_threshold: f64,
    /// Score tolerance for consensus (10% = 0.10)
    pub score_tolerance: f64,
    /// Result expiry in seconds
    pub result_expiry_secs: u64,
}

impl Default for P2PBroadcastConfig {
    fn default() -> Self {
        Self {
            validator_hotkey: String::new(),
            consensus_threshold: 0.67,
            score_tolerance: 0.10,
            result_expiry_secs: 3600,
        }
    }
}

/// P2P Broadcast handler
pub struct P2PBroadcast {
    config: P2PBroadcastConfig,
    /// Received results: agent_hash -> (validator -> result)
    results: Arc<RwLock<HashMap<String, HashMap<String, ReceivedResult>>>>,
    /// Consensus reached: agent_hash -> consensus score
    consensus: Arc<RwLock<HashMap<String, ConsensusInfo>>>,
    /// Message sender
    message_tx: broadcast::Sender<ChallengeMessage>,
    /// Known validators
    validators: Arc<RwLock<Vec<String>>>,
}

/// Consensus info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusInfo {
    pub agent_hash: String,
    pub score: f64,
    pub validator_count: u32,
    pub validators: Vec<String>,
    pub reached_at: u64,
    pub epoch: u64,
}

impl P2PBroadcast {
    pub fn new(config: P2PBroadcastConfig) -> Self {
        let (message_tx, _) = broadcast::channel(1000);

        Self {
            config,
            results: Arc::new(RwLock::new(HashMap::new())),
            consensus: Arc::new(RwLock::new(HashMap::new())),
            message_tx,
            validators: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Subscribe to outgoing messages
    pub fn subscribe(&self) -> broadcast::Receiver<ChallengeMessage> {
        self.message_tx.subscribe()
    }

    /// Update known validators
    pub fn set_validators(&self, validators: Vec<String>) {
        *self.validators.write() = validators;
    }

    /// Broadcast our evaluation result
    pub fn broadcast_result(
        &self,
        agent_hash: &str,
        score: f64,
        result_hash: &str,
        epoch: u64,
        signature: Vec<u8>,
    ) {
        let msg = ChallengeMessage::EvaluationResult {
            agent_hash: agent_hash.to_string(),
            validator_hotkey: self.config.validator_hotkey.clone(),
            score,
            result_hash: result_hash.to_string(),
            epoch,
            signature,
        };

        if let Err(e) = self.message_tx.send(msg) {
            warn!("Failed to broadcast result: {}", e);
        }

        // Also store our own result
        self.handle_received_result(
            agent_hash,
            &self.config.validator_hotkey,
            score,
            result_hash,
            epoch,
        );
    }

    /// Handle received result from another validator
    pub fn handle_received_result(
        &self,
        agent_hash: &str,
        validator: &str,
        score: f64,
        result_hash: &str,
        epoch: u64,
    ) -> Option<ConsensusInfo> {
        debug!(
            "Received result from {} for {}: score={:.4}",
            validator, agent_hash, score
        );

        // Store result
        let result = ReceivedResult {
            agent_hash: agent_hash.to_string(),
            validator_hotkey: validator.to_string(),
            score,
            result_hash: result_hash.to_string(),
            epoch,
            received_at: chrono::Utc::now().timestamp() as u64,
        };

        {
            let mut results = self.results.write();
            results
                .entry(agent_hash.to_string())
                .or_default()
                .insert(validator.to_string(), result);
        }

        // Check for consensus
        self.try_reach_consensus(agent_hash, epoch)
    }

    /// Try to reach consensus for an agent
    fn try_reach_consensus(&self, agent_hash: &str, epoch: u64) -> Option<ConsensusInfo> {
        // Already have consensus?
        if self.consensus.read().contains_key(agent_hash) {
            return self.consensus.read().get(agent_hash).cloned();
        }

        let results = self.results.read();
        let agent_results = results.get(agent_hash)?;

        let total_validators = self.validators.read().len();
        if total_validators == 0 {
            return None;
        }

        let required = (total_validators as f64 * self.config.consensus_threshold).ceil() as usize;

        // Group results by similar scores (within tolerance)
        let scores: Vec<f64> = agent_results.values().map(|r| r.score).collect();

        // Find the most common score group
        let mut best_group: Vec<&ReceivedResult> = Vec::new();
        let mut best_avg = 0.0;

        for result in agent_results.values() {
            let similar: Vec<&ReceivedResult> = agent_results
                .values()
                .filter(|r| {
                    let diff = (r.score - result.score).abs();
                    let max_score = result.score.max(r.score).max(0.01);
                    diff / max_score <= self.config.score_tolerance
                })
                .collect();

            if similar.len() > best_group.len() {
                best_group = similar;
                best_avg =
                    best_group.iter().map(|r| r.score).sum::<f64>() / best_group.len() as f64;
            }
        }

        // Check if we have consensus
        if best_group.len() >= required {
            let validators: Vec<String> = best_group
                .iter()
                .map(|r| r.validator_hotkey.clone())
                .collect();

            let consensus = ConsensusInfo {
                agent_hash: agent_hash.to_string(),
                score: best_avg,
                validator_count: best_group.len() as u32,
                validators,
                reached_at: chrono::Utc::now().timestamp() as u64,
                epoch,
            };

            // Store consensus
            self.consensus
                .write()
                .insert(agent_hash.to_string(), consensus.clone());

            // Broadcast consensus reached
            let msg = ChallengeMessage::ConsensusReached {
                agent_hash: agent_hash.to_string(),
                score: best_avg,
                validator_count: best_group.len() as u32,
                epoch,
            };
            let _ = self.message_tx.send(msg);

            info!(
                "Consensus reached for {}: score={:.4} ({} validators)",
                agent_hash,
                best_avg,
                best_group.len()
            );

            return Some(consensus);
        }

        None
    }

    /// Get consensus for an agent
    pub fn get_consensus(&self, agent_hash: &str) -> Option<ConsensusInfo> {
        self.consensus.read().get(agent_hash).cloned()
    }

    /// Get all results for an agent
    pub fn get_results(&self, agent_hash: &str) -> HashMap<String, ReceivedResult> {
        self.results
            .read()
            .get(agent_hash)
            .cloned()
            .unwrap_or_default()
    }

    /// Get all consensus results
    pub fn get_all_consensus(&self) -> HashMap<String, ConsensusInfo> {
        self.consensus.read().clone()
    }

    /// Broadcast weight commit
    pub fn broadcast_commit(&self, commit_hash: [u8; 32], epoch: u64) {
        let msg = ChallengeMessage::WeightCommit {
            validator_hotkey: self.config.validator_hotkey.clone(),
            commit_hash,
            epoch,
        };
        let _ = self.message_tx.send(msg);
    }

    /// Broadcast weight reveal
    pub fn broadcast_reveal(&self, weights: Vec<(u16, u16)>, salt: [u8; 32], epoch: u64) {
        let msg = ChallengeMessage::WeightReveal {
            validator_hotkey: self.config.validator_hotkey.clone(),
            weights,
            salt,
            epoch,
        };
        let _ = self.message_tx.send(msg);
    }

    /// Handle incoming message
    pub fn handle_message(&self, msg: ChallengeMessage) -> Option<ConsensusInfo> {
        match msg {
            ChallengeMessage::EvaluationResult {
                agent_hash,
                validator_hotkey,
                score,
                result_hash,
                epoch,
                ..
            } => self.handle_received_result(
                &agent_hash,
                &validator_hotkey,
                score,
                &result_hash,
                epoch,
            ),
            ChallengeMessage::ConsensusReached {
                agent_hash,
                score,
                validator_count,
                epoch,
            } => {
                // Verify we have the same consensus
                if let Some(our_consensus) = self.consensus.read().get(&agent_hash) {
                    if (our_consensus.score - score).abs() < 0.01 {
                        debug!("Confirmed consensus for {}", agent_hash);
                    } else {
                        warn!(
                            "Consensus mismatch for {}: ours={:.4}, theirs={:.4}",
                            agent_hash, our_consensus.score, score
                        );
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Clear old results
    pub fn cleanup(&self, max_age_secs: u64) {
        let now = chrono::Utc::now().timestamp() as u64;

        let mut results = self.results.write();
        for agent_results in results.values_mut() {
            agent_results.retain(|_, r| now - r.received_at < max_age_secs);
        }
        results.retain(|_, v| !v.is_empty());
    }

    /// Clear all state (for new epoch)
    pub fn clear(&self) {
        self.results.write().clear();
        self.consensus.write().clear();
    }
}

/// Wire protocol for P2P messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMessage {
    /// Message type identifier
    pub msg_type: u8,
    /// Serialized payload
    pub payload: Vec<u8>,
    /// Sender hotkey
    pub sender: String,
    /// Timestamp
    pub timestamp: u64,
    /// Signature
    pub signature: Vec<u8>,
}

impl WireMessage {
    /// Create wire message from challenge message
    pub fn from_challenge_message(msg: &ChallengeMessage, sender: &str) -> Result<Self, String> {
        let (msg_type, payload) = match msg {
            ChallengeMessage::EvaluationResult { .. } => (1, serde_json::to_vec(msg)),
            ChallengeMessage::RequestResult { .. } => (2, serde_json::to_vec(msg)),
            ChallengeMessage::ConsensusReached { .. } => (3, serde_json::to_vec(msg)),
            ChallengeMessage::WeightCommit { .. } => (4, serde_json::to_vec(msg)),
            ChallengeMessage::WeightReveal { .. } => (5, serde_json::to_vec(msg)),
        };

        let payload = payload.map_err(|e| format!("Failed to serialize: {}", e))?;

        Ok(Self {
            msg_type,
            payload,
            sender: sender.to_string(),
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
            signature: vec![], // Signature pending
        })
    }

    /// Parse wire message to challenge message
    pub fn to_challenge_message(&self) -> Result<ChallengeMessage, String> {
        serde_json::from_slice(&self.payload).map_err(|e| format!("Failed to deserialize: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_result_broadcast() {
        let config = P2PBroadcastConfig {
            validator_hotkey: "validator1".to_string(),
            consensus_threshold: 0.67,
            score_tolerance: 0.10,
            ..Default::default()
        };

        let broadcast = P2PBroadcast::new(config);
        broadcast.set_validators(vec![
            "validator1".to_string(),
            "validator2".to_string(),
            "validator3".to_string(),
        ]);

        // Broadcast result
        broadcast.broadcast_result("agent1", 0.85, "hash1", 1, vec![]);

        // Check result stored
        let results = broadcast.get_results("agent1");
        assert_eq!(results.len(), 1);
        assert!(results.contains_key("validator1"));
    }

    #[test]
    fn test_consensus() {
        let config = P2PBroadcastConfig {
            validator_hotkey: "validator1".to_string(),
            consensus_threshold: 0.5, // Need 2 out of 3 (50% with ceil = 2)
            score_tolerance: 0.10,
            ..Default::default()
        };

        let broadcast = P2PBroadcast::new(config);
        broadcast.set_validators(vec![
            "validator1".to_string(),
            "validator2".to_string(),
            "validator3".to_string(),
        ]);

        // First result - no consensus yet
        let result1 = broadcast.handle_received_result("agent1", "validator1", 0.85, "h1", 1);
        assert!(result1.is_none());

        // Second result (similar score) - should reach consensus
        let result2 = broadcast.handle_received_result("agent1", "validator2", 0.86, "h2", 1);
        assert!(result2.is_some());

        let consensus = result2.unwrap();
        assert!((consensus.score - 0.855).abs() < 0.01);
        assert_eq!(consensus.validator_count, 2);
    }

    #[test]
    fn test_no_consensus_divergent_scores() {
        let config = P2PBroadcastConfig {
            validator_hotkey: "validator1".to_string(),
            consensus_threshold: 0.67,
            score_tolerance: 0.10,
            ..Default::default()
        };

        let broadcast = P2PBroadcast::new(config);
        broadcast.set_validators(vec![
            "validator1".to_string(),
            "validator2".to_string(),
            "validator3".to_string(),
        ]);

        // Divergent scores - no consensus
        broadcast.handle_received_result("agent1", "validator1", 0.50, "h1", 1);
        broadcast.handle_received_result("agent1", "validator2", 0.90, "h2", 1);

        let consensus = broadcast.get_consensus("agent1");
        assert!(consensus.is_none());
    }
}
