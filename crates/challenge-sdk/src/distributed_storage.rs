//! Distributed Challenge Storage
//!
//! On-chain storage for challenge data that is replicated across all validators.
//! Data is stored with consensus validation to prevent spam and ensure consistency.
//!
//! # Storage Model
//!
//! Each challenge has a partition with:
//! - **Submissions**: Agent submissions (encrypted until reveal)
//! - **Evaluations**: Validator evaluation results
//! - **Agents**: Final agent registry (after consensus)
//! - **Logs**: Execution logs (compressed, limited)
//! - **Leaderboard**: Consensus-based rankings
//!
//! # Consensus Rules
//!
//! 1. **Write Validation**: Each write must be signed by a validator with sufficient stake
//! 2. **Size Limits**: Per-entry and total partition limits
//! 3. **Rate Limiting**: Max writes per validator per epoch
//! 4. **Conflict Resolution**: Timestamp + stake weight for conflicts
//! 5. **Garbage Collection**: TTL-based expiry for temporary data

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

// ============================================================================
// STORAGE LIMITS - Anti-spam protection
// ============================================================================

/// Maximum size of a single entry (1 MB)
pub const MAX_ENTRY_SIZE: usize = 1024 * 1024;

/// Maximum total partition size per challenge (100 MB)
pub const MAX_PARTITION_SIZE: usize = 100 * 1024 * 1024;

/// Maximum entries per partition
pub const MAX_ENTRIES_PER_PARTITION: usize = 10_000;

/// Maximum writes per validator per epoch
pub const MAX_WRITES_PER_VALIDATOR_PER_EPOCH: usize = 100;

/// Maximum log size per agent (100 KB compressed)
pub const MAX_LOG_SIZE: usize = 100 * 1024;

/// TTL for temporary data (submissions before reveal) - 1000 blocks (~3 hours)
pub const TEMP_DATA_TTL_BLOCKS: u64 = 1000;

/// TTL for execution logs - 10000 blocks (~1 day)
pub const LOG_TTL_BLOCKS: u64 = 10_000;

// ============================================================================
// STORAGE ENTRY TYPES
// ============================================================================

/// Entry type for categorization
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntryType {
    /// Agent submission (encrypted or decrypted)
    Submission,
    /// Evaluation result from a validator
    Evaluation,
    /// Final agent record (after consensus)
    Agent,
    /// Execution log
    Log,
    /// Leaderboard
    Leaderboard,
    /// Consensus result
    Consensus,
    /// Custom challenge-specific data
    Custom,
}

/// Storage entry metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMetadata {
    /// Entry type
    pub entry_type: EntryType,
    /// Entry key (unique within partition)
    pub key: String,
    /// Size in bytes
    pub size: usize,
    /// Block height when created
    pub created_at_block: u64,
    /// Block height when last updated
    pub updated_at_block: u64,
    /// Block when expires (None = permanent)
    pub expires_at_block: Option<u64>,
    /// Creator validator hotkey
    pub creator: String,
    /// Version (increments on update)
    pub version: u64,
    /// Hash of the value for verification
    pub value_hash: [u8; 32],
    /// Signature from creator
    pub signature: Vec<u8>,
}

/// A storage entry with value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageEntry {
    /// Metadata
    pub metadata: EntryMetadata,
    /// Value (serialized)
    pub value: Vec<u8>,
}

impl StorageEntry {
    /// Create a new entry
    pub fn new(
        entry_type: EntryType,
        key: String,
        value: Vec<u8>,
        creator: String,
        block_height: u64,
        ttl_blocks: Option<u64>,
    ) -> Self {
        let value_hash = Self::compute_hash(&value);
        let expires = ttl_blocks.map(|ttl| block_height + ttl);

        Self {
            metadata: EntryMetadata {
                entry_type,
                key,
                size: value.len(),
                created_at_block: block_height,
                updated_at_block: block_height,
                expires_at_block: expires,
                creator,
                version: 1,
                value_hash,
                signature: vec![],
            },
            value,
        }
    }

    /// Compute hash of value
    pub fn compute_hash(value: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(value);
        hasher.finalize().into()
    }

    /// Verify entry integrity
    pub fn verify_integrity(&self) -> bool {
        self.metadata.value_hash == Self::compute_hash(&self.value)
            && self.metadata.size == self.value.len()
    }

    /// Check if expired
    pub fn is_expired(&self, current_block: u64) -> bool {
        self.metadata
            .expires_at_block
            .map(|e| current_block >= e)
            .unwrap_or(false)
    }

    /// Update value
    pub fn update(&mut self, new_value: Vec<u8>, block_height: u64) {
        self.value = new_value;
        self.metadata.size = self.value.len();
        self.metadata.value_hash = Self::compute_hash(&self.value);
        self.metadata.updated_at_block = block_height;
        self.metadata.version += 1;
    }

    /// Sign the entry
    pub fn sign(&mut self, signature: Vec<u8>) {
        self.metadata.signature = signature;
    }
}

// ============================================================================
// WRITE REQUEST & VALIDATION
// ============================================================================

/// Request to write data to distributed storage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteRequest {
    /// Challenge ID
    pub challenge_id: String,
    /// Entry type
    pub entry_type: EntryType,
    /// Key
    pub key: String,
    /// Value to store
    pub value: Vec<u8>,
    /// TTL in blocks (None = use default for entry type)
    pub ttl_blocks: Option<u64>,
    /// Validator making the request
    pub validator: String,
    /// Validator's stake (for priority)
    pub validator_stake: u64,
    /// Block height
    pub block_height: u64,
    /// Epoch
    pub epoch: u64,
    /// Signature
    pub signature: Vec<u8>,
}

impl WriteRequest {
    /// Create a new write request
    pub fn new(
        challenge_id: String,
        entry_type: EntryType,
        key: String,
        value: Vec<u8>,
        validator: String,
        validator_stake: u64,
        block_height: u64,
        epoch: u64,
    ) -> Self {
        Self {
            challenge_id,
            entry_type,
            key,
            value,
            ttl_blocks: None,
            validator,
            validator_stake,
            block_height,
            epoch,
            signature: vec![],
        }
    }

    /// Set TTL
    pub fn with_ttl(mut self, blocks: u64) -> Self {
        self.ttl_blocks = Some(blocks);
        self
    }

    /// Sign the request
    pub fn sign(mut self, signature: Vec<u8>) -> Self {
        self.signature = signature;
        self
    }

    /// Compute hash for signing
    pub fn compute_sign_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.challenge_id.as_bytes());
        hasher.update(&[self.entry_type as u8]);
        hasher.update(self.key.as_bytes());
        hasher.update(&self.value);
        hasher.update(self.validator.as_bytes());
        hasher.update(self.block_height.to_le_bytes());
        hasher.update(self.epoch.to_le_bytes());
        hasher.finalize().into()
    }
}

/// Result of write validation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WriteValidation {
    /// Accept the write
    Accept,
    /// Reject with reason
    Reject(WriteRejection),
}

/// Reasons for rejecting a write
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WriteRejection {
    /// Entry too large
    EntryTooLarge { size: usize, max: usize },
    /// Partition full
    PartitionFull { current: usize, max: usize },
    /// Rate limit exceeded
    RateLimitExceeded { count: usize, max: usize },
    /// Invalid signature
    InvalidSignature,
    /// Insufficient stake
    InsufficientStake { stake: u64, required: u64 },
    /// Invalid entry type
    InvalidEntryType,
    /// Key already exists (and no update allowed)
    KeyExists,
    /// Invalid value format
    InvalidFormat(String),
    /// Validator is banned
    ValidatorBanned,
    /// Challenge not found
    ChallengeNotFound,
    /// Custom rejection
    Custom(String),
}

impl WriteValidation {
    pub fn is_accepted(&self) -> bool {
        matches!(self, WriteValidation::Accept)
    }

    pub fn reject_too_large(size: usize, max: usize) -> Self {
        WriteValidation::Reject(WriteRejection::EntryTooLarge { size, max })
    }

    pub fn reject_rate_limit(count: usize, max: usize) -> Self {
        WriteValidation::Reject(WriteRejection::RateLimitExceeded { count, max })
    }

    pub fn reject_invalid_format(reason: impl Into<String>) -> Self {
        WriteValidation::Reject(WriteRejection::InvalidFormat(reason.into()))
    }

    pub fn reject_custom(reason: impl Into<String>) -> Self {
        WriteValidation::Reject(WriteRejection::Custom(reason.into()))
    }
}

// ============================================================================
// CHALLENGE PARTITION
// ============================================================================

/// A partition of storage for a specific challenge
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengePartition {
    /// Challenge ID
    pub challenge_id: String,
    /// Entries by key
    pub entries: HashMap<String, StorageEntry>,
    /// Total size in bytes
    pub total_size: usize,
    /// Write counts per validator per epoch
    pub write_counts: HashMap<(String, u64), usize>,
    /// Created at block
    pub created_at_block: u64,
    /// Last modified block
    pub last_modified_block: u64,
}

impl ChallengePartition {
    /// Create a new partition
    pub fn new(challenge_id: String, block_height: u64) -> Self {
        Self {
            challenge_id,
            entries: HashMap::new(),
            total_size: 0,
            write_counts: HashMap::new(),
            created_at_block: block_height,
            last_modified_block: block_height,
        }
    }

    /// Validate a write request
    pub fn validate_write(&self, request: &WriteRequest, min_stake: u64) -> WriteValidation {
        // Check signature is present
        if request.signature.is_empty() {
            return WriteValidation::Reject(WriteRejection::InvalidSignature);
        }

        // Check stake
        if request.validator_stake < min_stake {
            return WriteValidation::Reject(WriteRejection::InsufficientStake {
                stake: request.validator_stake,
                required: min_stake,
            });
        }

        // Check entry size
        if request.value.len() > MAX_ENTRY_SIZE {
            return WriteValidation::reject_too_large(request.value.len(), MAX_ENTRY_SIZE);
        }

        // Check partition size (accounting for new entry)
        let new_total = self.total_size + request.value.len();
        if new_total > MAX_PARTITION_SIZE {
            return WriteValidation::Reject(WriteRejection::PartitionFull {
                current: self.total_size,
                max: MAX_PARTITION_SIZE,
            });
        }

        // Check entry count
        if self.entries.len() >= MAX_ENTRIES_PER_PARTITION
            && !self.entries.contains_key(&request.key)
        {
            return WriteValidation::Reject(WriteRejection::PartitionFull {
                current: self.entries.len(),
                max: MAX_ENTRIES_PER_PARTITION,
            });
        }

        // Check rate limit
        let write_key = (request.validator.clone(), request.epoch);
        let write_count = self.write_counts.get(&write_key).copied().unwrap_or(0);
        if write_count >= MAX_WRITES_PER_VALIDATOR_PER_EPOCH {
            return WriteValidation::reject_rate_limit(
                write_count,
                MAX_WRITES_PER_VALIDATOR_PER_EPOCH,
            );
        }

        // Entry type specific validation
        match request.entry_type {
            EntryType::Log => {
                if request.value.len() > MAX_LOG_SIZE {
                    return WriteValidation::reject_too_large(request.value.len(), MAX_LOG_SIZE);
                }
            }
            _ => {}
        }

        WriteValidation::Accept
    }

    /// Apply a write request (after validation)
    pub fn apply_write(&mut self, request: WriteRequest) -> Option<StorageEntry> {
        let ttl = request.ttl_blocks.or_else(|| {
            match request.entry_type {
                EntryType::Submission => Some(TEMP_DATA_TTL_BLOCKS),
                EntryType::Log => Some(LOG_TTL_BLOCKS),
                _ => None, // Permanent
            }
        });

        let entry = StorageEntry::new(
            request.entry_type,
            request.key.clone(),
            request.value,
            request.validator.clone(),
            request.block_height,
            ttl,
        );

        // Update size tracking
        if let Some(old) = self.entries.get(&request.key) {
            self.total_size -= old.metadata.size;
        }
        self.total_size += entry.metadata.size;

        // Update write count
        let write_key = (request.validator, request.epoch);
        *self.write_counts.entry(write_key).or_insert(0) += 1;

        self.last_modified_block = request.block_height;
        self.entries.insert(request.key, entry.clone());

        Some(entry)
    }

    /// Get an entry
    pub fn get(&self, key: &str) -> Option<&StorageEntry> {
        self.entries.get(key)
    }

    /// Get entries by type
    pub fn get_by_type(&self, entry_type: EntryType) -> Vec<&StorageEntry> {
        self.entries
            .values()
            .filter(|e| e.metadata.entry_type == entry_type)
            .collect()
    }

    /// Get entries by validator
    pub fn get_by_validator(&self, validator: &str) -> Vec<&StorageEntry> {
        self.entries
            .values()
            .filter(|e| e.metadata.creator == validator)
            .collect()
    }

    /// Remove expired entries
    pub fn cleanup_expired(&mut self, current_block: u64) -> usize {
        let expired_keys: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.is_expired(current_block))
            .map(|(k, _)| k.clone())
            .collect();

        for key in &expired_keys {
            if let Some(entry) = self.entries.remove(key) {
                self.total_size -= entry.metadata.size;
            }
        }

        // Cleanup old write counts
        self.write_counts.retain(|(_, epoch), _| {
            // Keep last 10 epochs
            *epoch + 10 > current_block / 100
        });

        expired_keys.len()
    }

    /// Get partition stats
    pub fn stats(&self) -> PartitionStats {
        let mut by_type: HashMap<EntryType, usize> = HashMap::new();
        for entry in self.entries.values() {
            *by_type.entry(entry.metadata.entry_type).or_insert(0) += 1;
        }

        PartitionStats {
            challenge_id: self.challenge_id.clone(),
            total_entries: self.entries.len(),
            total_size: self.total_size,
            entries_by_type: by_type,
            created_at_block: self.created_at_block,
            last_modified_block: self.last_modified_block,
        }
    }
}

/// Partition statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionStats {
    pub challenge_id: String,
    pub total_entries: usize,
    pub total_size: usize,
    pub entries_by_type: HashMap<EntryType, usize>,
    pub created_at_block: u64,
    pub last_modified_block: u64,
}

// ============================================================================
// P2P SYNC MESSAGES
// ============================================================================

/// Message for syncing storage between validators
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StorageSyncMessage {
    /// Announce a new write
    WriteAnnounce {
        challenge_id: String,
        entry_key: String,
        entry_hash: [u8; 32],
        entry_type: EntryType,
        block_height: u64,
        validator: String,
    },
    /// Request a specific entry
    RequestEntry {
        challenge_id: String,
        entry_key: String,
    },
    /// Response with entry data
    EntryResponse {
        challenge_id: String,
        entry: Option<StorageEntry>,
    },
    /// Request partition snapshot (hash only for comparison)
    RequestPartitionHash { challenge_id: String },
    /// Partition hash response
    PartitionHashResponse {
        challenge_id: String,
        entries_hash: [u8; 32],
        entry_count: usize,
        total_size: usize,
    },
    /// Request full partition sync (for new validators)
    RequestFullSync {
        challenge_id: String,
        from_block: u64,
    },
    /// Full sync response (paginated)
    FullSyncResponse {
        challenge_id: String,
        entries: Vec<StorageEntry>,
        has_more: bool,
        next_key: Option<String>,
    },
}

impl StorageSyncMessage {
    /// Create write announce message
    pub fn announce_write(entry: &StorageEntry, challenge_id: &str) -> Self {
        StorageSyncMessage::WriteAnnounce {
            challenge_id: challenge_id.to_string(),
            entry_key: entry.metadata.key.clone(),
            entry_hash: entry.metadata.value_hash,
            entry_type: entry.metadata.entry_type,
            block_height: entry.metadata.created_at_block,
            validator: entry.metadata.creator.clone(),
        }
    }
}

// ============================================================================
// AGENT STORAGE TYPES (for term-challenge)
// ============================================================================

/// Stored agent submission
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSubmission {
    /// Unique submission ID
    pub submission_id: String,
    /// Agent hash (SHA256 of source)
    pub agent_hash: String,
    /// Miner hotkey
    pub miner_hotkey: String,
    /// Content hash (for verification)
    pub content_hash: [u8; 32],
    /// Encrypted source (before reveal)
    pub encrypted_source: Option<Vec<u8>>,
    /// Decrypted source (after reveal)
    pub source_code: Option<String>,
    /// Source code size
    pub source_size: usize,
    /// Epoch when submitted
    pub epoch: u64,
    /// Block when submitted
    pub submitted_at_block: u64,
    /// Timestamp
    pub submitted_at: u64,
    /// Whether revealed
    pub revealed: bool,
    /// Miner signature
    pub signature: Vec<u8>,
}

/// Stored evaluation result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvaluation {
    /// Agent hash being evaluated
    pub agent_hash: String,
    /// Validator who performed evaluation
    pub validator_hotkey: String,
    /// Epoch
    pub epoch: u64,
    /// Final score (0.0 - 1.0)
    pub score: f64,
    /// Total tasks
    pub total_tasks: u32,
    /// Tasks passed
    pub passed_tasks: u32,
    /// Tasks failed
    pub failed_tasks: u32,
    /// Total cost USD
    pub total_cost_usd: f64,
    /// Per-task results
    pub task_results: Vec<StoredTaskResult>,
    /// Block when evaluated
    pub evaluated_at_block: u64,
    /// Timestamp
    pub evaluated_at: u64,
    /// Results hash for verification
    pub results_hash: [u8; 32],
    /// Validator signature
    pub signature: Vec<u8>,
}

/// Stored task result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTaskResult {
    pub task_id: String,
    pub passed: bool,
    pub score: f64,
    pub cost_usd: f64,
    pub execution_time_ms: u64,
}

/// Stored execution log (compressed)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredLog {
    /// Agent hash
    pub agent_hash: String,
    /// Validator hotkey
    pub validator_hotkey: String,
    /// Task ID (if task-specific)
    pub task_id: Option<String>,
    /// Compressed log data (gzip)
    pub compressed_log: Vec<u8>,
    /// Original size
    pub original_size: usize,
    /// Block height
    pub block_height: u64,
    /// Timestamp
    pub timestamp: u64,
}

/// Final agent record (after consensus)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAgent {
    /// Agent hash
    pub agent_hash: String,
    /// Miner hotkey
    pub miner_hotkey: String,
    /// Source code (permanent storage)
    pub source_code: String,
    /// Consensus score
    pub consensus_score: f64,
    /// Number of evaluations
    pub evaluation_count: u32,
    /// Validators who evaluated
    pub evaluated_by: Vec<String>,
    /// Best rank achieved
    pub best_rank: Option<u32>,
    /// First submitted epoch
    pub first_epoch: u64,
    /// Last evaluated epoch
    pub last_epoch: u64,
    /// Block when first submitted
    pub created_at_block: u64,
    /// Block when last updated
    pub updated_at_block: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_entry() {
        let entry = StorageEntry::new(
            EntryType::Submission,
            "sub:123".to_string(),
            vec![1, 2, 3, 4],
            "validator1".to_string(),
            100,
            Some(1000),
        );

        assert!(entry.verify_integrity());
        assert_eq!(entry.metadata.size, 4);
        assert_eq!(entry.metadata.expires_at_block, Some(1100));
        assert!(!entry.is_expired(500));
        assert!(entry.is_expired(1200));
    }

    #[test]
    fn test_partition_validation() {
        let partition = ChallengePartition::new("term-bench".to_string(), 0);

        // Valid request
        let request = WriteRequest::new(
            "term-bench".to_string(),
            EntryType::Submission,
            "sub:123".to_string(),
            vec![1, 2, 3],
            "validator1".to_string(),
            1_000_000_000_000, // 1000 TAO
            100,
            1,
        )
        .sign(vec![1, 2, 3]); // Dummy signature

        let validation = partition.validate_write(&request, 100_000_000_000);
        assert!(validation.is_accepted());

        // Request without signature
        let request_no_sig = WriteRequest::new(
            "term-bench".to_string(),
            EntryType::Submission,
            "sub:456".to_string(),
            vec![1, 2, 3],
            "validator1".to_string(),
            1_000_000_000_000,
            100,
            1,
        );

        let validation = partition.validate_write(&request_no_sig, 100_000_000_000);
        assert!(!validation.is_accepted());
    }

    #[test]
    fn test_partition_write() {
        let mut partition = ChallengePartition::new("term-bench".to_string(), 0);

        let request = WriteRequest::new(
            "term-bench".to_string(),
            EntryType::Submission,
            "sub:123".to_string(),
            vec![1, 2, 3, 4, 5],
            "validator1".to_string(),
            1_000_000_000_000,
            100,
            1,
        )
        .sign(vec![1, 2, 3]);

        let entry = partition.apply_write(request);
        assert!(entry.is_some());
        assert_eq!(partition.total_size, 5);
        assert_eq!(partition.entries.len(), 1);
    }

    #[test]
    fn test_rate_limiting() {
        let mut partition = ChallengePartition::new("term-bench".to_string(), 0);

        // Fill up rate limit
        for i in 0..MAX_WRITES_PER_VALIDATOR_PER_EPOCH {
            let request = WriteRequest::new(
                "term-bench".to_string(),
                EntryType::Submission,
                format!("sub:{}", i),
                vec![1, 2, 3],
                "validator1".to_string(),
                1_000_000_000_000,
                100,
                1,
            )
            .sign(vec![1, 2, 3]);

            partition.apply_write(request);
        }

        // Next write should be rate limited
        let request = WriteRequest::new(
            "term-bench".to_string(),
            EntryType::Submission,
            "sub:overflow".to_string(),
            vec![1, 2, 3],
            "validator1".to_string(),
            1_000_000_000_000,
            100,
            1,
        )
        .sign(vec![1, 2, 3]);

        let validation = partition.validate_write(&request, 100_000_000_000);
        assert!(!validation.is_accepted());
    }

    #[test]
    fn test_cleanup_expired() {
        let mut partition = ChallengePartition::new("term-bench".to_string(), 0);

        // Add entry with short TTL
        let request = WriteRequest::new(
            "term-bench".to_string(),
            EntryType::Log,
            "log:123".to_string(),
            vec![1, 2, 3],
            "validator1".to_string(),
            1_000_000_000_000,
            100,
            1,
        )
        .with_ttl(10)
        .sign(vec![1, 2, 3]);

        partition.apply_write(request);
        assert_eq!(partition.entries.len(), 1);

        // Not expired yet
        let removed = partition.cleanup_expired(105);
        assert_eq!(removed, 0);

        // Now expired
        let removed = partition.cleanup_expired(115);
        assert_eq!(removed, 1);
        assert_eq!(partition.entries.len(), 0);
    }
}
