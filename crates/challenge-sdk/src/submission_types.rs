//! Submission Types for Commit-Reveal Protocol
//!
//! These are the base types used by the secure submission system.
//! The actual submission management logic is implemented by each challenge.

use platform_core::Hotkey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Encrypted submission from a miner
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncryptedSubmission {
    /// Challenge ID
    pub challenge_id: String,
    /// Miner's hotkey (public identifier)
    pub miner_hotkey: String,
    /// Miner's coldkey (for banning)
    pub miner_coldkey: String,
    /// Encrypted data (AES-256-GCM)
    pub encrypted_data: Vec<u8>,
    /// Hash of the decryption key (for verification)
    pub key_hash: [u8; 32],
    /// Nonce for AES-GCM (24 bytes)
    pub nonce: [u8; 24],
    /// Hash of the submission (encrypted_data + key_hash + nonce)
    pub submission_hash: [u8; 32],
    /// Hash of the ORIGINAL unencrypted content (for ownership verification)
    pub content_hash: [u8; 32],
    /// Signature from miner over (content_hash + miner_hotkey + epoch)
    pub miner_signature: Vec<u8>,
    /// Timestamp of submission
    pub submitted_at: chrono::DateTime<chrono::Utc>,
    /// Epoch when submitted
    pub epoch: u64,
}

impl EncryptedSubmission {
    /// Create a new encrypted submission
    pub fn new(
        challenge_id: String,
        miner_hotkey: String,
        miner_coldkey: String,
        encrypted_data: Vec<u8>,
        key_hash: [u8; 32],
        nonce: [u8; 24],
        content_hash: [u8; 32],
        miner_signature: Vec<u8>,
        epoch: u64,
    ) -> Self {
        let submission_hash = Self::compute_hash(&encrypted_data, &key_hash, &nonce);
        Self {
            challenge_id,
            miner_hotkey,
            miner_coldkey,
            encrypted_data,
            key_hash,
            nonce,
            submission_hash,
            content_hash,
            miner_signature,
            submitted_at: chrono::Utc::now(),
            epoch,
        }
    }

    /// Compute submission hash
    pub fn compute_hash(encrypted_data: &[u8], key_hash: &[u8; 32], nonce: &[u8; 24]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(encrypted_data);
        hasher.update(key_hash);
        hasher.update(nonce);
        hasher.finalize().into()
    }

    /// Compute content hash from original data
    pub fn compute_content_hash(data: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hasher.finalize().into()
    }

    /// Compute the message that must be signed by the miner
    pub fn compute_signature_message(
        content_hash: &[u8; 32],
        miner_hotkey: &str,
        epoch: u64,
    ) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.extend_from_slice(content_hash);
        msg.extend_from_slice(miner_hotkey.as_bytes());
        msg.extend_from_slice(&epoch.to_le_bytes());
        msg
    }

    /// Verify submission hash is correct
    pub fn verify_hash(&self) -> bool {
        let computed = Self::compute_hash(&self.encrypted_data, &self.key_hash, &self.nonce);
        computed == self.submission_hash
    }

    /// Get submission hash as hex string
    pub fn hash_hex(&self) -> String {
        hex::encode(self.submission_hash)
    }

    /// Get content hash as hex string
    pub fn content_hash_hex(&self) -> String {
        hex::encode(self.content_hash)
    }
}

/// Acknowledgment from a validator that they received a submission
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmissionAck {
    /// Hash of the submission being acknowledged
    pub submission_hash: [u8; 32],
    /// Validator's hotkey
    pub validator_hotkey: Hotkey,
    /// Validator's stake (for weighted quorum)
    pub validator_stake: u64,
    /// Signature proving validator received it
    pub signature: Vec<u8>,
    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl SubmissionAck {
    pub fn new(
        submission_hash: [u8; 32],
        validator_hotkey: Hotkey,
        validator_stake: u64,
        signature: Vec<u8>,
    ) -> Self {
        Self {
            submission_hash,
            validator_hotkey,
            validator_stake,
            signature,
            timestamp: chrono::Utc::now(),
        }
    }

    pub fn submission_hash_hex(&self) -> String {
        hex::encode(self.submission_hash)
    }
}

/// Decryption key reveal from miner after quorum reached
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecryptionKeyReveal {
    /// Hash of the submission
    pub submission_hash: [u8; 32],
    /// The actual decryption key
    pub decryption_key: Vec<u8>,
    /// Miner's signature proving they own the key
    pub miner_signature: Vec<u8>,
    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl DecryptionKeyReveal {
    pub fn new(
        submission_hash: [u8; 32],
        decryption_key: Vec<u8>,
        miner_signature: Vec<u8>,
    ) -> Self {
        Self {
            submission_hash,
            decryption_key,
            miner_signature,
            timestamp: chrono::Utc::now(),
        }
    }

    /// Verify the key matches the hash from the original submission
    pub fn verify_key_hash(&self, expected_hash: &[u8; 32]) -> bool {
        let mut hasher = Sha256::new();
        hasher.update(&self.decryption_key);
        let computed: [u8; 32] = hasher.finalize().into();
        &computed == expected_hash
    }
}

/// Decrypted and verified submission (after key reveal)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifiedSubmission {
    /// Original submission hash
    pub submission_hash: [u8; 32],
    /// Hash of the decrypted content (for duplicate detection)
    pub content_hash: [u8; 32],
    /// Challenge ID
    pub challenge_id: String,
    /// Miner's hotkey
    pub miner_hotkey: String,
    /// Miner's coldkey
    pub miner_coldkey: String,
    /// Decrypted data (e.g., source code)
    pub data: Vec<u8>,
    /// Epoch when submitted
    pub epoch: u64,
    /// Original submission timestamp (for priority in case of duplicate)
    pub submitted_at: chrono::DateTime<chrono::Utc>,
    /// When the submission was verified
    pub verified_at: chrono::DateTime<chrono::Utc>,
    /// Whether ownership was verified (content_hash matches signed hash)
    pub ownership_verified: bool,
}

/// Errors that can occur during submission processing
#[derive(Debug, Clone, thiserror::Error)]
pub enum SubmissionError {
    #[error("Miner is banned")]
    MinerBanned,
    #[error("Invalid submission hash")]
    InvalidHash,
    #[error("Submission already exists")]
    AlreadyExists,
    #[error("Submission not found")]
    NotFound,
    #[error("Invalid state for operation")]
    InvalidState,
    #[error("Quorum not reached")]
    QuorumNotReached,
    #[error("Invalid decryption key")]
    InvalidKey,
    #[error("Decryption failed")]
    DecryptionFailed,
    #[error("Encryption failed")]
    EncryptionFailed,
    #[error("Signature verification failed")]
    SignatureInvalid,
    #[error("Ownership verification failed - content hash does not match signed hash")]
    OwnershipVerificationFailed,
    #[error("Duplicate content detected - same code already submitted")]
    DuplicateContent,
}

// ============== Crypto Helpers ==============

/// Encrypt data using AES-256-GCM
pub fn encrypt_data(
    data: &[u8],
    key: &[u8; 32],
    nonce: &[u8; 24],
) -> Result<Vec<u8>, SubmissionError> {
    use aes_gcm::{
        aead::{Aead, KeyInit},
        Aes256Gcm, Nonce,
    };

    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| SubmissionError::EncryptionFailed)?;

    let nonce = Nonce::from_slice(&nonce[..12]);

    cipher
        .encrypt(nonce, data)
        .map_err(|_| SubmissionError::EncryptionFailed)
}

/// Decrypt data using AES-256-GCM
pub fn decrypt_data(
    encrypted: &[u8],
    key: &[u8],
    nonce: &[u8; 24],
) -> Result<Vec<u8>, SubmissionError> {
    use aes_gcm::{
        aead::{Aead, KeyInit},
        Aes256Gcm, Nonce,
    };

    if key.len() != 32 {
        return Err(SubmissionError::InvalidKey);
    }

    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| SubmissionError::DecryptionFailed)?;

    let nonce = Nonce::from_slice(&nonce[..12]);

    cipher
        .decrypt(nonce, encrypted)
        .map_err(|_| SubmissionError::DecryptionFailed)
}

/// Generate a random encryption key
pub fn generate_key() -> [u8; 32] {
    use rand::RngCore;
    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    key
}

/// Generate a random nonce
pub fn generate_nonce() -> [u8; 24] {
    use rand::RngCore;
    let mut nonce = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

/// Hash a key for commit-reveal
pub fn hash_key(key: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(key);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt() {
        let key = generate_key();
        let nonce = generate_nonce();
        let data = b"Hello, World!";

        let encrypted = encrypt_data(data, &key, &nonce).unwrap();
        let decrypted = decrypt_data(&encrypted, &key, &nonce).unwrap();

        assert_eq!(data.as_slice(), decrypted.as_slice());
    }

    #[test]
    fn test_key_hash() {
        let key = generate_key();
        let hash = hash_key(&key);

        assert_eq!(hash, hash_key(&key));

        let key2 = generate_key();
        assert_ne!(hash, hash_key(&key2));
    }

    #[test]
    fn test_submission_hash() {
        let key = generate_key();
        let nonce = generate_nonce();
        let key_hash = hash_key(&key);
        let data = b"test code";
        let content_hash = EncryptedSubmission::compute_content_hash(data);
        let encrypted = encrypt_data(data, &key, &nonce).unwrap();

        let submission = EncryptedSubmission::new(
            "challenge-1".to_string(),
            "miner-hotkey".to_string(),
            "miner-coldkey".to_string(),
            encrypted,
            key_hash,
            nonce,
            content_hash,
            vec![],
            1,
        );

        assert!(submission.verify_hash());
        assert_eq!(submission.content_hash, content_hash);
    }

    #[test]
    fn test_content_hash_verification() {
        let data = b"my agent source code";
        let content_hash = EncryptedSubmission::compute_content_hash(data);

        let content_hash2 = EncryptedSubmission::compute_content_hash(data);
        assert_eq!(content_hash, content_hash2);

        let different_data = b"different code";
        let different_hash = EncryptedSubmission::compute_content_hash(different_data);
        assert_ne!(content_hash, different_hash);
    }
}
