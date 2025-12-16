//! Storage types for dynamic storage system
//!
//! Provides typed storage keys and values for the blockchain.

use platform_core::{ChallengeId, Hotkey};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

/// Storage key with namespace
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StorageKey {
    /// Namespace (usually challenge_id or "system")
    pub namespace: String,
    /// Optional validator scope
    pub validator: Option<Hotkey>,
    /// Key name
    pub key: String,
}

impl StorageKey {
    /// Create a system-level key
    pub fn system(key: impl Into<String>) -> Self {
        Self {
            namespace: "system".to_string(),
            validator: None,
            key: key.into(),
        }
    }

    /// Create a challenge-level key
    pub fn challenge(challenge_id: &ChallengeId, key: impl Into<String>) -> Self {
        Self {
            namespace: challenge_id.0.to_string(),
            validator: None,
            key: key.into(),
        }
    }

    /// Create a validator-scoped key within a challenge
    pub fn validator(
        challenge_id: &ChallengeId,
        validator: &Hotkey,
        key: impl Into<String>,
    ) -> Self {
        Self {
            namespace: challenge_id.0.to_string(),
            validator: Some(validator.clone()),
            key: key.into(),
        }
    }

    /// Create a global validator key (not challenge-specific)
    pub fn global_validator(validator: &Hotkey, key: impl Into<String>) -> Self {
        Self {
            namespace: "validators".to_string(),
            validator: Some(validator.clone()),
            key: key.into(),
        }
    }

    /// Convert to bytes for storage
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(self.namespace.as_bytes());
        bytes.push(0x00); // Separator
        if let Some(ref v) = self.validator {
            bytes.extend(v.as_bytes());
        }
        bytes.push(0x00); // Separator
        bytes.extend(self.key.as_bytes());
        bytes
    }

    /// Get the prefix for scanning keys in a namespace
    pub fn namespace_prefix(namespace: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(namespace.as_bytes());
        bytes.push(0x00);
        bytes
    }
}

/// Typed storage value
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum StorageValue {
    /// Boolean value
    Bool(bool),
    /// Unsigned 64-bit integer
    U64(u64),
    /// Signed 64-bit integer
    I64(i64),
    /// Unsigned 128-bit integer (for large numbers like TAO amounts)
    U128(u128),
    /// Floating point
    F64(f64),
    /// UTF-8 string
    String(String),
    /// Raw bytes
    Bytes(Vec<u8>),
    /// JSON value (for complex structures)
    Json(serde_json::Value),
    /// List of values
    List(Vec<StorageValue>),
    /// Map of string keys to values
    Map(HashMap<String, StorageValue>),
    /// Null/None
    Null,
}

impl StorageValue {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            StorageValue::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            StorageValue::U64(v) => Some(*v),
            StorageValue::I64(v) if *v >= 0 => Some(*v as u64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            StorageValue::I64(v) => Some(*v),
            StorageValue::U64(v) if *v <= i64::MAX as u64 => Some(*v as i64),
            _ => None,
        }
    }

    pub fn as_u128(&self) -> Option<u128> {
        match self {
            StorageValue::U128(v) => Some(*v),
            StorageValue::U64(v) => Some(*v as u128),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            StorageValue::F64(v) => Some(*v),
            StorageValue::U64(v) => Some(*v as f64),
            StorageValue::I64(v) => Some(*v as f64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            StorageValue::String(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            StorageValue::Bytes(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_json(&self) -> Option<&serde_json::Value> {
        match self {
            StorageValue::Json(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&Vec<StorageValue>> {
        match self {
            StorageValue::List(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&HashMap<String, StorageValue>> {
        match self {
            StorageValue::Map(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, StorageValue::Null)
    }
}

impl From<bool> for StorageValue {
    fn from(v: bool) -> Self {
        StorageValue::Bool(v)
    }
}

impl From<u64> for StorageValue {
    fn from(v: u64) -> Self {
        StorageValue::U64(v)
    }
}

impl From<i64> for StorageValue {
    fn from(v: i64) -> Self {
        StorageValue::I64(v)
    }
}

impl From<u128> for StorageValue {
    fn from(v: u128) -> Self {
        StorageValue::U128(v)
    }
}

impl From<f64> for StorageValue {
    fn from(v: f64) -> Self {
        StorageValue::F64(v)
    }
}

impl From<String> for StorageValue {
    fn from(v: String) -> Self {
        StorageValue::String(v)
    }
}

impl From<&str> for StorageValue {
    fn from(v: &str) -> Self {
        StorageValue::String(v.to_string())
    }
}

impl From<Vec<u8>> for StorageValue {
    fn from(v: Vec<u8>) -> Self {
        StorageValue::Bytes(v)
    }
}

impl From<serde_json::Value> for StorageValue {
    fn from(v: serde_json::Value) -> Self {
        StorageValue::Json(v)
    }
}

/// Storage entry with metadata
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StorageEntry {
    /// The value
    pub value: StorageValue,
    /// When this entry was created
    pub created_at: SystemTime,
    /// When this entry was last modified
    pub modified_at: SystemTime,
    /// Time-to-live (None = persistent)
    pub ttl: Option<Duration>,
    /// Version for optimistic locking
    pub version: u64,
    /// Who wrote this entry (validator hotkey)
    pub writer: Option<Hotkey>,
}

impl StorageEntry {
    pub fn new(value: StorageValue, writer: Option<Hotkey>) -> Self {
        let now = SystemTime::now();
        Self {
            value,
            created_at: now,
            modified_at: now,
            ttl: None,
            version: 1,
            writer,
        }
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Check if this entry has expired
    pub fn is_expired(&self) -> bool {
        if let Some(ttl) = self.ttl {
            if let Ok(elapsed) = self.created_at.elapsed() {
                return elapsed > ttl;
            }
        }
        false
    }

    /// Update the value, incrementing version
    pub fn update(&mut self, value: StorageValue, writer: Option<Hotkey>) {
        self.value = value;
        self.modified_at = SystemTime::now();
        self.version += 1;
        self.writer = writer;
    }
}

/// Storage change event
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StorageChange {
    pub key: StorageKey,
    pub old_value: Option<StorageValue>,
    pub new_value: Option<StorageValue>,
    pub block_height: u64,
    pub timestamp: SystemTime,
}

/// Storage statistics
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StorageStats {
    pub total_keys: u64,
    pub total_size_bytes: u64,
    pub namespaces: HashMap<String, NamespaceStats>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NamespaceStats {
    pub key_count: u64,
    pub size_bytes: u64,
    pub validator_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_key_system() {
        let key = StorageKey::system("version");
        assert_eq!(key.namespace, "system");
        assert!(key.validator.is_none());
        assert_eq!(key.key, "version");
    }

    #[test]
    fn test_storage_key_challenge() {
        let cid = ChallengeId(uuid::Uuid::new_v4());
        let key = StorageKey::challenge(&cid, "leaderboard");
        assert_eq!(key.namespace, cid.0.to_string());
        assert!(key.validator.is_none());
    }

    #[test]
    fn test_storage_key_validator() {
        let cid = ChallengeId(uuid::Uuid::new_v4());
        let hotkey = Hotkey([1u8; 32]);
        let key = StorageKey::validator(&cid, &hotkey, "score");
        assert!(key.validator.is_some());
    }

    #[test]
    fn test_storage_value_conversions() {
        let v = StorageValue::from(42u64);
        assert_eq!(v.as_u64(), Some(42));

        let v = StorageValue::from("hello");
        assert_eq!(v.as_str(), Some("hello"));

        let v = StorageValue::from(true);
        assert_eq!(v.as_bool(), Some(true));
    }

    #[test]
    fn test_storage_entry_expiry() {
        let entry =
            StorageEntry::new(StorageValue::Bool(true), None).with_ttl(Duration::from_millis(1));

        std::thread::sleep(Duration::from_millis(10));
        assert!(entry.is_expired());
    }
}
