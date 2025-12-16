//! P2P Communication Interface for Challenges
//!
//! Provides traits and types for challenges to communicate with the validator network.

use crate::{
    DecryptionKeyReveal, EncryptedSubmission, SubmissionAck, ValidatorEvaluation,
    VerifiedSubmission, WeightCalculationResult,
};
use async_trait::async_trait;
use platform_core::Hotkey;
use serde::{Deserialize, Serialize};

/// Messages that challenges can send/receive via P2P
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ChallengeP2PMessage {
    /// Encrypted submission from a miner
    EncryptedSubmission(EncryptedSubmission),

    /// Acknowledgment of receiving a submission
    SubmissionAck(SubmissionAck),

    /// Decryption key reveal after quorum
    KeyReveal(DecryptionKeyReveal),

    /// Evaluation result from a validator
    EvaluationResult(EvaluationResultMessage),

    /// Request evaluations for weight calculation
    RequestEvaluations(RequestEvaluationsMessage),

    /// Response with evaluations
    EvaluationsResponse(EvaluationsResponseMessage),

    /// Weight calculation result (for consensus)
    WeightResult(WeightResultMessage),
}

/// Evaluation result message
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationResultMessage {
    /// Challenge ID
    pub challenge_id: String,
    /// The evaluation data
    pub evaluation: ValidatorEvaluation,
    /// Signature from validator
    pub signature: Vec<u8>,
}

/// Request evaluations for an epoch
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestEvaluationsMessage {
    /// Challenge ID
    pub challenge_id: String,
    /// Epoch to get evaluations for
    pub epoch: u64,
    /// Requesting validator
    pub requester: Hotkey,
}

/// Response with evaluations
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationsResponseMessage {
    /// Challenge ID
    pub challenge_id: String,
    /// Epoch
    pub epoch: u64,
    /// All evaluations from this validator
    pub evaluations: Vec<ValidatorEvaluation>,
    /// Signature
    pub signature: Vec<u8>,
}

/// Weight result message for consensus
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WeightResultMessage {
    /// Challenge ID
    pub challenge_id: String,
    /// Epoch
    pub epoch: u64,
    /// Calculated weights
    pub result: WeightCalculationResult,
    /// Validator who calculated
    pub validator: Hotkey,
    /// Signature
    pub signature: Vec<u8>,
}

/// Handler for P2P messages in a challenge
#[async_trait]
pub trait ChallengeP2PHandler: Send + Sync {
    /// Handle an incoming P2P message
    async fn handle_message(
        &self,
        from: Hotkey,
        message: ChallengeP2PMessage,
    ) -> Option<ChallengeP2PMessage>;

    /// Get the challenge ID this handler is for
    fn challenge_id(&self) -> &str;
}

/// Interface for challenges to send P2P messages
#[async_trait]
pub trait P2PBroadcaster: Send + Sync {
    /// Broadcast a message to all validators
    async fn broadcast(&self, message: ChallengeP2PMessage) -> Result<(), P2PError>;

    /// Send a message to a specific validator
    async fn send_to(&self, target: &Hotkey, message: ChallengeP2PMessage) -> Result<(), P2PError>;

    /// Get current validator set with stakes
    async fn get_validators(&self) -> Vec<(Hotkey, u64)>;

    /// Get total network stake
    async fn get_total_stake(&self) -> u64;

    /// Get our own hotkey
    fn our_hotkey(&self) -> &Hotkey;

    /// Get our own stake
    fn our_stake(&self) -> u64;
}

/// Callback for when quorum is reached on a submission
#[async_trait]
pub trait QuorumCallback: Send + Sync {
    /// Called when quorum is reached for a submission
    async fn on_quorum_reached(&self, submission_hash: [u8; 32], acks: Vec<SubmissionAck>);

    /// Called when a submission is fully verified (decrypted)
    async fn on_submission_verified(&self, submission: VerifiedSubmission);

    /// Called when a submission fails
    async fn on_submission_failed(&self, submission_hash: [u8; 32], reason: String);
}

/// Callback for evaluation events
#[async_trait]
pub trait EvaluationCallback: Send + Sync {
    /// Called when we should evaluate a submission
    async fn on_evaluate(&self, submission: &VerifiedSubmission) -> Option<ValidatorEvaluation>;

    /// Called when we receive an evaluation from another validator
    async fn on_remote_evaluation(&self, evaluation: ValidatorEvaluation);
}

/// Callback for weight calculation events
#[async_trait]
pub trait WeightCallback: Send + Sync {
    /// Called when it's time to calculate weights
    async fn on_calculate_weights(&self, epoch: u64) -> Option<WeightCalculationResult>;

    /// Called when we receive weight results from another validator
    async fn on_remote_weights(&self, result: WeightResultMessage);

    /// Called when weight consensus is reached
    async fn on_weight_consensus(&self, epoch: u64, weights: Vec<(String, f64)>);
}

#[derive(Debug, thiserror::Error)]
pub enum P2PError {
    #[error("Not connected to network")]
    NotConnected,
    #[error("Target validator not found")]
    ValidatorNotFound,
    #[error("Broadcast failed: {0}")]
    BroadcastFailed(String),
    #[error("Serialization failed: {0}")]
    SerializationFailed(String),
}

/// Helper to create a signed P2P message
pub fn sign_message(
    message: &ChallengeP2PMessage,
    keypair: &platform_core::Keypair,
) -> Result<Vec<u8>, P2PError> {
    let data =
        bincode::serialize(message).map_err(|e| P2PError::SerializationFailed(e.to_string()))?;

    let signed = keypair.sign(&data);
    Ok(signed.signature)
}

/// Helper to verify a signed P2P message
pub fn verify_signature(message: &ChallengeP2PMessage, signature: &[u8], signer: &Hotkey) -> bool {
    let Ok(data) = bincode::serialize(message) else {
        return false;
    };

    // Verify using ed25519
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let Ok(verifying_key) = VerifyingKey::from_bytes(signer.as_bytes()) else {
        return false;
    };

    let Ok(sig) = Signature::from_slice(signature) else {
        return false;
    };

    verifying_key.verify(&data, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialization() {
        let msg = ChallengeP2PMessage::RequestEvaluations(RequestEvaluationsMessage {
            challenge_id: "test".to_string(),
            epoch: 1,
            requester: Hotkey([1u8; 32]),
        });

        let serialized = bincode::serialize(&msg).unwrap();
        let deserialized: ChallengeP2PMessage = bincode::deserialize(&serialized).unwrap();

        match deserialized {
            ChallengeP2PMessage::RequestEvaluations(req) => {
                assert_eq!(req.challenge_id, "test");
                assert_eq!(req.epoch, 1);
            }
            _ => panic!("Wrong message type"),
        }
    }
}
