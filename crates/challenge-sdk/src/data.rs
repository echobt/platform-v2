//! Challenge Data Submission System
//!
//! Enables challenges to define what data validators can store
//! and how it should be verified.
//!
//! # Example
//!
//! ```rust,ignore
//! use platform_challenge_sdk::data::*;
//!
//! impl Challenge for MyChallenge {
//!     // Define what data keys are allowed
//!     fn allowed_data_keys(&self) -> Vec<DataKeySpec> {
//!         vec![
//!             DataKeySpec::new("score")
//!                 .validator_scoped()
//!                 .with_schema(json!({"type": "number", "minimum": 0, "maximum": 100})),
//!             DataKeySpec::new("leaderboard")
//!                 .challenge_scoped()
//!                 .max_size(1024 * 1024)
//!                 .ttl_blocks(100),
//!         ]
//!     }
//!
//!     // Verify submitted data
//!     async fn verify_data(&self, ctx: &ChallengeContext, submission: &DataSubmission) -> DataVerification {
//!         match submission.key.as_str() {
//!             "score" => {
//!                 // Verify score is valid
//!                 if let Ok(score) = serde_json::from_slice::<f64>(&submission.value) {
//!                     if score >= 0.0 && score <= 100.0 {
//!                         return DataVerification::accept();
//!                     }
//!                 }
//!                 DataVerification::reject("Invalid score format")
//!             }
//!             _ => DataVerification::reject("Unknown key"),
//!         }
//!     }
//! }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Specification for a data key that validators can write to
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataKeySpec {
    /// Key name
    pub key: String,
    /// Scope of the data
    pub scope: DataScope,
    /// Maximum size in bytes (0 = unlimited)
    pub max_size: usize,
    /// TTL in blocks (0 = permanent)
    pub ttl_blocks: u64,
    /// JSON schema for validation (optional)
    pub schema: Option<Value>,
    /// Description
    pub description: String,
    /// Whether this key requires consensus
    pub requires_consensus: bool,
    /// Minimum validators needed for consensus
    pub min_consensus: usize,
}

impl DataKeySpec {
    /// Create a new data key spec
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            scope: DataScope::Validator,
            max_size: 0,
            ttl_blocks: 0,
            schema: None,
            description: String::new(),
            requires_consensus: true,
            min_consensus: 1,
        }
    }

    /// Set scope to validator (each validator has own value)
    pub fn validator_scoped(mut self) -> Self {
        self.scope = DataScope::Validator;
        self
    }

    /// Set scope to challenge (single value for entire challenge)
    pub fn challenge_scoped(mut self) -> Self {
        self.scope = DataScope::Challenge;
        self
    }

    /// Set scope to global (shared across challenges)
    pub fn global_scoped(mut self) -> Self {
        self.scope = DataScope::Global;
        self
    }

    /// Set maximum size in bytes
    pub fn max_size(mut self, size: usize) -> Self {
        self.max_size = size;
        self
    }

    /// Set TTL in blocks
    pub fn ttl_blocks(mut self, blocks: u64) -> Self {
        self.ttl_blocks = blocks;
        self
    }

    /// Set JSON schema for validation
    pub fn with_schema(mut self, schema: Value) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Set description
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Disable consensus requirement
    pub fn no_consensus(mut self) -> Self {
        self.requires_consensus = false;
        self
    }

    /// Set minimum consensus validators
    pub fn min_consensus(mut self, count: usize) -> Self {
        self.min_consensus = count;
        self
    }
}

/// Scope of data storage
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataScope {
    /// Each validator has their own value
    Validator,
    /// Single value for the entire challenge
    Challenge,
    /// Shared across all challenges
    Global,
}

/// Data submission from a validator
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataSubmission {
    /// Key being written to
    pub key: String,
    /// Value to store
    pub value: Vec<u8>,
    /// Submitting validator
    pub validator: String,
    /// Block height
    pub block_height: u64,
    /// Epoch
    pub epoch: u64,
    /// Additional metadata
    pub metadata: HashMap<String, Value>,
}

impl DataSubmission {
    /// Create a new submission
    pub fn new(key: impl Into<String>, value: Vec<u8>, validator: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value,
            validator: validator.into(),
            block_height: 0,
            epoch: 0,
            metadata: HashMap::new(),
        }
    }

    /// Set block height
    pub fn at_block(mut self, height: u64) -> Self {
        self.block_height = height;
        self
    }

    /// Set epoch
    pub fn at_epoch(mut self, epoch: u64) -> Self {
        self.epoch = epoch;
        self
    }

    /// Add metadata
    pub fn with_metadata(mut self, key: impl Into<String>, value: Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }

    /// Parse value as JSON
    pub fn value_json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.value)
    }

    /// Parse value as string
    pub fn value_string(&self) -> Result<String, std::string::FromUtf8Error> {
        String::from_utf8(self.value.clone())
    }
}

/// Result of data verification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataVerification {
    /// Whether to accept the data
    pub accepted: bool,
    /// Reason for rejection (if rejected)
    pub reason: Option<String>,
    /// Transform the value before storing
    pub transformed_value: Option<Vec<u8>>,
    /// Override TTL (blocks)
    pub ttl_override: Option<u64>,
    /// Additional data to emit as events
    pub events: Vec<DataEvent>,
}

impl DataVerification {
    /// Accept the data
    pub fn accept() -> Self {
        Self {
            accepted: true,
            reason: None,
            transformed_value: None,
            ttl_override: None,
            events: vec![],
        }
    }

    /// Reject the data
    pub fn reject(reason: impl Into<String>) -> Self {
        Self {
            accepted: false,
            reason: Some(reason.into()),
            transformed_value: None,
            ttl_override: None,
            events: vec![],
        }
    }

    /// Accept with transformed value
    pub fn accept_with_transform(value: Vec<u8>) -> Self {
        Self {
            accepted: true,
            reason: None,
            transformed_value: Some(value),
            ttl_override: None,
            events: vec![],
        }
    }

    /// Set TTL override
    pub fn with_ttl(mut self, blocks: u64) -> Self {
        self.ttl_override = Some(blocks);
        self
    }

    /// Add an event to emit
    pub fn with_event(mut self, event: DataEvent) -> Self {
        self.events.push(event);
        self
    }
}

/// Event emitted during data verification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataEvent {
    /// Event type
    pub event_type: String,
    /// Event data
    pub data: Value,
}

impl DataEvent {
    /// Create a new event
    pub fn new(event_type: impl Into<String>, data: Value) -> Self {
        Self {
            event_type: event_type.into(),
            data,
        }
    }
}

/// Stored data entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredData {
    /// Key
    pub key: String,
    /// Value
    pub value: Vec<u8>,
    /// Scope
    pub scope: DataScope,
    /// Validator who submitted (for Validator scope)
    pub validator: Option<String>,
    /// Block when stored
    pub stored_at_block: u64,
    /// Block when expires (if any)
    pub expires_at_block: Option<u64>,
    /// Version (incremented on update)
    pub version: u64,
}

impl StoredData {
    /// Check if expired
    pub fn is_expired(&self, current_block: u64) -> bool {
        self.expires_at_block
            .map(|e| current_block >= e)
            .unwrap_or(false)
    }

    /// Parse value as JSON
    pub fn value_json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.value)
    }
}

/// Query for retrieving stored data
#[derive(Debug, Clone)]
pub struct DataQuery {
    /// Key pattern (supports * wildcard)
    pub key_pattern: Option<String>,
    /// Scope filter
    pub scope: Option<DataScope>,
    /// Validator filter (for Validator scope)
    pub validator: Option<String>,
    /// Include expired
    pub include_expired: bool,
    /// Limit results
    pub limit: Option<usize>,
    /// Offset for pagination
    pub offset: Option<usize>,
}

impl DataQuery {
    /// Create a new query
    pub fn new() -> Self {
        Self {
            key_pattern: None,
            scope: None,
            validator: None,
            include_expired: false,
            limit: None,
            offset: None,
        }
    }

    /// Filter by key pattern
    pub fn key(mut self, pattern: impl Into<String>) -> Self {
        self.key_pattern = Some(pattern.into());
        self
    }

    /// Filter by scope
    pub fn scope(mut self, scope: DataScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Filter by validator
    pub fn validator(mut self, validator: impl Into<String>) -> Self {
        self.validator = Some(validator.into());
        self
    }

    /// Include expired entries
    pub fn include_expired(mut self) -> Self {
        self.include_expired = true;
        self
    }

    /// Limit results
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Offset for pagination
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }
}

impl Default for DataQuery {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_key_spec() {
        let spec = DataKeySpec::new("score")
            .validator_scoped()
            .max_size(1024)
            .ttl_blocks(100)
            .with_description("Player score");

        assert_eq!(spec.key, "score");
        assert_eq!(spec.scope, DataScope::Validator);
        assert_eq!(spec.max_size, 1024);
        assert_eq!(spec.ttl_blocks, 100);
    }

    #[test]
    fn test_data_verification() {
        let accept = DataVerification::accept();
        assert!(accept.accepted);

        let reject = DataVerification::reject("Bad data");
        assert!(!reject.accepted);
        assert_eq!(reject.reason, Some("Bad data".to_string()));
    }

    #[test]
    fn test_data_submission() {
        let sub = DataSubmission::new("score", vec![1, 2, 3], "validator1")
            .at_block(100)
            .at_epoch(5);

        assert_eq!(sub.key, "score");
        assert_eq!(sub.block_height, 100);
        assert_eq!(sub.epoch, 5);
    }
}
