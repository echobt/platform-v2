//! Challenge Storage Client Framework
//!
//! Generic framework for challenges to interact with distributed blockchain storage.
//! Challenges define their own:
//! - Data types
//! - Query methods
//! - **Validation rules** (via StorageRules trait)
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                     VALIDATOR NODE                              │
//! │                                                                 │
//! │  ┌─────────────────────────────────────────────────────────┐   │
//! │  │              DistributedStorage (core)                   │   │
//! │  │  - Sled DB + LZ4 compression                            │   │
//! │  │  - 50% consensus                                         │   │
//! │  │  - Calls challenge.validate_write() before commit       │   │
//! │  └─────────────────────────────────────────────────────────┘   │
//! │                           │                                     │
//! │                           ▼                                     │
//! │  ┌─────────────────────────────────────────────────────────┐   │
//! │  │              Challenge (loaded dynamically)              │   │
//! │  │  impl StorageRules for TermChallenge {                  │   │
//! │  │      fn validate_write(...) -> WriteValidation          │   │
//! │  │      fn on_write_committed(...)                          │   │
//! │  │      fn consensus_threshold() -> f64                     │   │
//! │  │  }                                                       │   │
//! │  └─────────────────────────────────────────────────────────┘   │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::sync::Arc;

// ============================================================================
// STORAGE CATEGORIES
// ============================================================================

/// Data category for organization
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Category {
    /// Agent submissions (before consensus)
    Submission = 0,
    /// Finalized agents (after consensus)
    Agent = 1,
    /// Evaluation results (per validator)
    Evaluation = 2,
    /// Consensus results
    Consensus = 3,
    /// Execution logs
    Log = 4,
    /// Index data
    Index = 5,
    /// Metadata
    Meta = 6,
    /// Custom challenge-specific data
    Custom = 7,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Submission => "sub",
            Category::Agent => "agt",
            Category::Evaluation => "evl",
            Category::Consensus => "cns",
            Category::Log => "log",
            Category::Index => "idx",
            Category::Meta => "meta",
            Category::Custom => "cust",
        }
    }
}

// ============================================================================
// WRITE REQUEST (what gets validated)
// ============================================================================

/// A write request to be validated by the challenge
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteRequest {
    /// Category of data
    pub category: Category,
    /// Key within category
    pub key: String,
    /// Serialized value (challenge can deserialize to validate)
    pub value: Vec<u8>,
    /// Size in bytes
    pub size: usize,
    /// Creator validator hotkey
    pub creator: String,
    /// Creator's stake (in RAO)
    pub creator_stake: u64,
    /// Current block height
    pub block: u64,
    /// Current epoch
    pub epoch: u64,
    /// Is this an update to existing key?
    pub is_update: bool,
    /// Previous value hash (if update)
    pub previous_hash: Option<[u8; 32]>,
    /// Signature from creator
    pub signature: Vec<u8>,
}

impl WriteRequest {
    /// Deserialize the value
    pub fn deserialize_value<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.value)
    }
}

// ============================================================================
// WRITE VALIDATION (challenge returns this)
// ============================================================================

/// Result of write validation by challenge
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WriteValidation {
    /// Accept the write
    Accept,
    /// Reject with reason
    Reject(RejectReason),
}

impl WriteValidation {
    pub fn is_accepted(&self) -> bool {
        matches!(self, WriteValidation::Accept)
    }

    pub fn accept() -> Self {
        WriteValidation::Accept
    }

    pub fn reject(reason: RejectReason) -> Self {
        WriteValidation::Reject(reason)
    }

    pub fn reject_msg(msg: &str) -> Self {
        WriteValidation::Reject(RejectReason::Custom(msg.to_string()))
    }
}

/// Reasons for rejecting a write
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RejectReason {
    /// Insufficient stake
    InsufficientStake { required: u64, actual: u64 },
    /// Entry too large
    TooLarge { max: usize, actual: usize },
    /// Rate limited
    RateLimited { max_per_epoch: usize },
    /// Invalid data format
    InvalidData(String),
    /// Duplicate entry
    Duplicate(String),
    /// Unauthorized (e.g., updating another validator's data)
    Unauthorized(String),
    /// Invalid signature
    InvalidSignature,
    /// Category not allowed
    CategoryNotAllowed(Category),
    /// Custom rejection reason
    Custom(String),
}

// ============================================================================
// STORAGE RULES TRAIT (challenges implement this)
// ============================================================================

/// Trait for challenge-specific storage rules
/// Each challenge implements this to define validation logic
pub trait StorageRules: Send + Sync {
    /// Challenge identifier
    fn challenge_id(&self) -> &str;

    /// Validate a write request before it's committed
    /// Called by DistributedStorage before consensus vote
    fn validate_write(
        &self,
        request: &WriteRequest,
        context: &ValidationContext,
    ) -> WriteValidation;

    /// Called after a write is successfully committed (consensus reached)
    /// Use this for side effects (e.g., trigger agent finalization)
    fn on_write_committed(&self, request: &WriteRequest, context: &ValidationContext) {
        // Default: no-op
        let _ = (request, context);
    }

    /// Consensus threshold (default 50%)
    fn consensus_threshold(&self) -> f64 {
        0.50
    }

    /// Minimum stake required to write (in RAO)
    fn min_write_stake(&self) -> u64 {
        0 // Default: no minimum
    }

    /// Maximum entry size in bytes
    fn max_entry_size(&self) -> usize {
        10 * 1024 * 1024 // 10 MB default
    }

    /// Maximum writes per validator per epoch
    fn max_writes_per_epoch(&self) -> usize {
        1000 // Default
    }

    /// Categories this challenge allows
    fn allowed_categories(&self) -> Vec<Category> {
        vec![
            Category::Submission,
            Category::Agent,
            Category::Evaluation,
            Category::Consensus,
            Category::Log,
            Category::Custom,
        ]
    }

    /// Can this validator update this key?
    /// Default: only creator can update
    fn can_update(&self, request: &WriteRequest, existing_creator: &str) -> bool {
        request.creator == existing_creator
    }
}

/// Context provided during validation
#[derive(Debug, Clone)]
pub struct ValidationContext {
    /// Current block height
    pub block: u64,
    /// Current epoch
    pub epoch: u64,
    /// Total validators in network
    pub total_validators: usize,
    /// Our validator hotkey
    pub our_validator: String,
    /// Writes by this validator this epoch
    pub writes_this_epoch: usize,
    /// Total entries in this category
    pub category_entry_count: usize,
}

// ============================================================================
// STORAGE BACKEND TRAIT (low-level operations)
// ============================================================================

/// Base trait for storage operations
#[async_trait::async_trait]
pub trait StorageBackend: Send + Sync {
    /// Get challenge ID
    fn challenge_id(&self) -> &str;

    /// Store a value (proposes write with consensus)
    async fn store<T: Serialize + Send + Sync>(
        &self,
        category: Category,
        key: &str,
        value: &T,
    ) -> Result<(), StorageError>;

    /// Get a value by key
    async fn get<T: DeserializeOwned>(
        &self,
        category: Category,
        key: &str,
    ) -> Result<Option<T>, StorageError>;

    /// Check if key exists
    async fn exists(&self, category: Category, key: &str) -> Result<bool, StorageError>;

    /// Delete a key
    async fn delete(&self, category: Category, key: &str) -> Result<bool, StorageError>;

    /// List keys by category
    async fn list_keys(
        &self,
        category: Category,
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, StorageError>;

    /// List entries by category
    async fn list<T: DeserializeOwned>(
        &self,
        category: Category,
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, T)>, StorageError>;

    /// Count entries in category
    async fn count(&self, category: Category) -> Result<usize, StorageError>;

    /// Get storage stats
    async fn stats(&self) -> Result<StorageStats, StorageError>;

    /// Check sync status
    async fn is_synced(&self) -> Result<bool, StorageError>;
}

/// Storage statistics
#[derive(Debug, Clone, Default)]
pub struct StorageStats {
    pub total_entries: u64,
    pub total_size_bytes: u64,
    pub compression_ratio: f64,
    pub entries_by_category: Vec<(Category, u64)>,
}

/// Storage error
#[derive(Debug, Clone)]
pub enum StorageError {
    NotFound(String),
    Serialization(String),
    Storage(String),
    ConsensusNotReached,
    TooLarge(usize, usize),
    RateLimited,
    ValidationFailed(RejectReason),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::NotFound(k) => write!(f, "Not found: {}", k),
            StorageError::Serialization(e) => write!(f, "Serialization error: {}", e),
            StorageError::Storage(e) => write!(f, "Storage error: {}", e),
            StorageError::ConsensusNotReached => write!(f, "Consensus not reached"),
            StorageError::TooLarge(s, m) => write!(f, "Too large: {} > {}", s, m),
            StorageError::RateLimited => write!(f, "Rate limited"),
            StorageError::ValidationFailed(r) => write!(f, "Validation failed: {:?}", r),
        }
    }
}

impl std::error::Error for StorageError {}

// ============================================================================
// QUERY BUILDER
// ============================================================================

/// Query builder for flexible queries
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub category: Option<Category>,
    pub key_prefix: Option<String>,
    pub creator: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

impl Query {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn category(mut self, cat: Category) -> Self {
        self.category = Some(cat);
        self
    }

    pub fn prefix(mut self, prefix: &str) -> Self {
        self.key_prefix = Some(prefix.to_string());
        self
    }

    pub fn creator(mut self, creator: &str) -> Self {
        self.creator = Some(creator.to_string());
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }
}

// ============================================================================
// ENTRY METADATA
// ============================================================================

/// Metadata for stored entries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMeta {
    pub key: String,
    pub category: Category,
    pub size_bytes: usize,
    pub created_block: u64,
    pub creator: String,
    pub version: u64,
}

// ============================================================================
// TEST UTILITIES
// ============================================================================

#[cfg(test)]
pub mod test_utils {
    use super::*;
    use parking_lot::RwLock;
    use std::collections::HashMap;

    /// Test storage backend
    pub struct TestStorageBackend {
        challenge_id: String,
        data: RwLock<HashMap<(Category, String), Vec<u8>>>,
    }

    impl TestStorageBackend {
        pub fn new(challenge_id: &str) -> Self {
            Self {
                challenge_id: challenge_id.to_string(),
                data: RwLock::new(HashMap::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl StorageBackend for TestStorageBackend {
        fn challenge_id(&self) -> &str {
            &self.challenge_id
        }

        async fn store<T: Serialize + Send + Sync>(
            &self,
            category: Category,
            key: &str,
            value: &T,
        ) -> Result<(), StorageError> {
            let bytes = serde_json::to_vec(value)
                .map_err(|e| StorageError::Serialization(e.to_string()))?;
            self.data.write().insert((category, key.to_string()), bytes);
            Ok(())
        }

        async fn get<T: DeserializeOwned>(
            &self,
            category: Category,
            key: &str,
        ) -> Result<Option<T>, StorageError> {
            match self.data.read().get(&(category, key.to_string())) {
                Some(bytes) => {
                    let value = serde_json::from_slice(bytes)
                        .map_err(|e| StorageError::Serialization(e.to_string()))?;
                    Ok(Some(value))
                }
                None => Ok(None),
            }
        }

        async fn exists(&self, category: Category, key: &str) -> Result<bool, StorageError> {
            Ok(self.data.read().contains_key(&(category, key.to_string())))
        }

        async fn delete(&self, category: Category, key: &str) -> Result<bool, StorageError> {
            Ok(self
                .data
                .write()
                .remove(&(category, key.to_string()))
                .is_some())
        }

        async fn list_keys(
            &self,
            category: Category,
            prefix: Option<&str>,
            limit: usize,
        ) -> Result<Vec<String>, StorageError> {
            let data = self.data.read();
            let keys: Vec<String> = data
                .keys()
                .filter(|(cat, key)| {
                    *cat == category && prefix.map(|p| key.starts_with(p)).unwrap_or(true)
                })
                .map(|(_, key)| key.clone())
                .take(limit)
                .collect();
            Ok(keys)
        }

        async fn list<T: DeserializeOwned>(
            &self,
            category: Category,
            prefix: Option<&str>,
            limit: usize,
        ) -> Result<Vec<(String, T)>, StorageError> {
            let data = self.data.read();
            let mut results = Vec::new();

            for ((cat, key), bytes) in data.iter() {
                if *cat == category && prefix.map(|p| key.starts_with(p)).unwrap_or(true) {
                    if let Ok(value) = serde_json::from_slice(bytes) {
                        results.push((key.clone(), value));
                        if results.len() >= limit {
                            break;
                        }
                    }
                }
            }
            Ok(results)
        }

        async fn count(&self, category: Category) -> Result<usize, StorageError> {
            Ok(self
                .data
                .read()
                .keys()
                .filter(|(cat, _)| *cat == category)
                .count())
        }

        async fn stats(&self) -> Result<StorageStats, StorageError> {
            let data = self.data.read();
            let total_entries = data.len() as u64;
            let total_size: usize = data.values().map(|v| v.len()).sum();

            Ok(StorageStats {
                total_entries,
                total_size_bytes: total_size as u64,
                compression_ratio: 1.0,
                entries_by_category: vec![],
            })
        }

        async fn is_synced(&self) -> Result<bool, StorageError> {
            Ok(true)
        }
    }

    /// Test rules
    pub struct TestStorageRules {
        pub challenge_id: String,
        pub min_stake: u64,
        pub max_size: usize,
    }

    impl TestStorageRules {
        pub fn new(challenge_id: &str) -> Self {
            Self {
                challenge_id: challenge_id.to_string(),
                min_stake: 0,
                max_size: 10 * 1024 * 1024,
            }
        }
    }

    impl StorageRules for TestStorageRules {
        fn challenge_id(&self) -> &str {
            &self.challenge_id
        }

        fn validate_write(
            &self,
            request: &WriteRequest,
            _context: &ValidationContext,
        ) -> WriteValidation {
            // Check stake
            if request.creator_stake < self.min_stake {
                return WriteValidation::reject(RejectReason::InsufficientStake {
                    required: self.min_stake,
                    actual: request.creator_stake,
                });
            }

            // Check size
            if request.size > self.max_size {
                return WriteValidation::reject(RejectReason::TooLarge {
                    max: self.max_size,
                    actual: request.size,
                });
            }

            WriteValidation::Accept
        }

        fn min_write_stake(&self) -> u64 {
            self.min_stake
        }

        fn max_entry_size(&self) -> usize {
            self.max_size
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_utils::{TestStorageBackend, TestStorageRules};
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestData {
        name: String,
        value: u64,
    }

    #[tokio::test]
    async fn test_storage_backend() {
        let storage = TestStorageBackend::new("test-challenge");

        let data = TestData {
            name: "test".to_string(),
            value: 42,
        };

        storage
            .store(Category::Agent, "agent1", &data)
            .await
            .unwrap();

        let retrieved: Option<TestData> = storage.get(Category::Agent, "agent1").await.unwrap();
        assert_eq!(retrieved, Some(data.clone()));

        assert!(storage.exists(Category::Agent, "agent1").await.unwrap());
        assert!(!storage.exists(Category::Agent, "agent2").await.unwrap());

        let keys = storage.list_keys(Category::Agent, None, 100).await.unwrap();
        assert_eq!(keys, vec!["agent1"]);

        assert_eq!(storage.count(Category::Agent).await.unwrap(), 1);

        assert!(storage.delete(Category::Agent, "agent1").await.unwrap());
        assert!(!storage.exists(Category::Agent, "agent1").await.unwrap());
    }

    #[test]
    fn test_storage_rules_validation() {
        let rules = TestStorageRules {
            challenge_id: "test".to_string(),
            min_stake: 1000,
            max_size: 1024,
        };

        let context = ValidationContext {
            block: 100,
            epoch: 1,
            total_validators: 3,
            our_validator: "validator1".to_string(),
            writes_this_epoch: 0,
            category_entry_count: 0,
        };

        // Valid request
        let valid_request = WriteRequest {
            category: Category::Agent,
            key: "agent1".to_string(),
            value: vec![1, 2, 3],
            size: 3,
            creator: "validator1".to_string(),
            creator_stake: 2000,
            block: 100,
            epoch: 1,
            is_update: false,
            previous_hash: None,
            signature: vec![],
        };

        assert!(rules.validate_write(&valid_request, &context).is_accepted());

        // Insufficient stake
        let low_stake_request = WriteRequest {
            creator_stake: 500,
            ..valid_request.clone()
        };

        assert!(!rules
            .validate_write(&low_stake_request, &context)
            .is_accepted());

        // Too large
        let large_request = WriteRequest {
            size: 2000,
            ..valid_request.clone()
        };

        assert!(!rules.validate_write(&large_request, &context).is_accepted());
    }
}
