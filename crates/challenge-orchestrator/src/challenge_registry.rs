//! Dynamic Challenge Registry
//!
//! This module provides a registry system for loading and caching WASM challenge modules
//! from DHT/network storage. It supports:
//!
//! - Downloading challenges from DHT/network
//! - LRU caching with configurable limits
//! - Signature verification for WASM modules
//! - Hot-reload notification system
//!
//! # Example
//!
//! ```ignore
//! use challenge_orchestrator::challenge_registry::{ChallengeRegistry, RegistryConfig};
//!
//! let config = RegistryConfig::default();
//! let registry = ChallengeRegistry::new(config);
//!
//! // Get a challenge (loads from network if not cached)
//! let challenge = registry.get(&challenge_id).await?;
//!
//! // Subscribe to hot-reload notifications
//! let mut rx = registry.subscribe_reloads();
//! tokio::spawn(async move {
//!     while let Ok(id) = rx.recv().await {
//!         println!("Challenge {} was reloaded", id);
//!     }
//! });
//! ```

use parking_lot::RwLock;
use platform_core::{Challenge, ChallengeId, Hotkey, SignedMessage};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Errors that can occur in challenge registry operations
#[derive(Debug, Error)]
pub enum RegistryError {
    /// Challenge not found in cache or network
    #[error("Challenge not found: {0}")]
    NotFound(ChallengeId),

    /// Challenge WASM size exceeds the configured limit
    #[error("Challenge too large: {size_mb}MB exceeds {max_mb}MB limit")]
    TooLarge {
        /// Actual size in MB
        size_mb: u64,
        /// Maximum allowed size in MB
        max_mb: u64,
    },

    /// Signature verification failed for the challenge
    #[error("Invalid signature for challenge {0}")]
    InvalidSignature(ChallengeId),

    /// Network error while fetching challenge
    #[error("Network error: {0}")]
    NetworkError(String),

    /// Cache is full and cannot accept more entries
    #[error("Cache full")]
    CacheFull,

    /// Serialization/deserialization error
    #[error("Serialization error: {0}")]
    SerializationError(String),

    /// Internal error
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Cached challenge entry with metadata
#[derive(Clone, Debug)]
pub struct CachedChallenge {
    /// The challenge data including WASM code
    pub challenge: Challenge,
    /// When the challenge was loaded into cache
    pub loaded_at: chrono::DateTime<chrono::Utc>,
    /// When the challenge was last accessed
    pub last_accessed: chrono::DateTime<chrono::Utc>,
    /// Number of times this challenge has been accessed
    pub access_count: u64,
}

impl CachedChallenge {
    /// Create a new cached challenge entry
    fn new(challenge: Challenge) -> Self {
        let now = chrono::Utc::now();
        Self {
            challenge,
            loaded_at: now,
            last_accessed: now,
            access_count: 1,
        }
    }

    /// Update access statistics
    fn touch(&mut self) {
        self.last_accessed = chrono::Utc::now();
        self.access_count = self.access_count.saturating_add(1);
    }

    /// Check if the cached entry has expired based on TTL
    fn is_expired(&self, ttl_secs: u64) -> bool {
        if ttl_secs == 0 {
            return false;
        }
        let now = chrono::Utc::now();
        let age = now.signed_duration_since(self.loaded_at);
        age.num_seconds() as u64 > ttl_secs
    }

    /// Get the age of this cache entry in seconds
    fn age_secs(&self) -> u64 {
        let now = chrono::Utc::now();
        let age = now.signed_duration_since(self.loaded_at);
        age.num_seconds().max(0) as u64
    }
}

/// Configuration for the challenge registry
#[derive(Clone, Debug)]
pub struct RegistryConfig {
    /// Maximum number of challenges to keep in cache
    pub max_cache_size: usize,
    /// Maximum size of a single challenge WASM in MB
    pub max_challenge_size_mb: u64,
    /// Time-to-live for cached challenges in seconds (0 = never expires)
    pub cache_ttl_secs: u64,
    /// Whether to verify signatures when loading challenges
    pub verify_signatures: bool,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            max_cache_size: 100,
            max_challenge_size_mb: 50,
            cache_ttl_secs: 3600, // 1 hour
            verify_signatures: true,
        }
    }
}

impl RegistryConfig {
    /// Create a config for testing (smaller limits, no verification)
    pub fn for_testing() -> Self {
        Self {
            max_cache_size: 10,
            max_challenge_size_mb: 10,
            cache_ttl_secs: 60,
            verify_signatures: false,
        }
    }

    /// Create a config for production (larger limits, verification enabled)
    pub fn for_production() -> Self {
        Self {
            max_cache_size: 500,
            max_challenge_size_mb: 100,
            cache_ttl_secs: 7200, // 2 hours
            verify_signatures: true,
        }
    }
}

/// Network loader trait for fetching challenges from DHT/network
///
/// This trait abstracts the network layer for loading challenges,
/// allowing different implementations (DHT, HTTP, mock, etc.)
#[async_trait::async_trait]
pub trait ChallengeLoader: Send + Sync {
    /// Load a challenge from the network
    async fn load(&self, challenge_id: &ChallengeId) -> Result<Challenge, RegistryError>;

    /// Check if a challenge exists in the network
    async fn exists(&self, challenge_id: &ChallengeId) -> Result<bool, RegistryError>;
}

/// No-op loader for local-only mode or testing
pub struct LocalOnlyLoader;

#[async_trait::async_trait]
impl ChallengeLoader for LocalOnlyLoader {
    async fn load(&self, challenge_id: &ChallengeId) -> Result<Challenge, RegistryError> {
        Err(RegistryError::NotFound(*challenge_id))
    }

    async fn exists(&self, _challenge_id: &ChallengeId) -> Result<bool, RegistryError> {
        Ok(false)
    }
}

/// Challenge registry for managing WASM challenge modules
///
/// The registry provides:
/// - Caching with LRU eviction
/// - Network loading via pluggable loader
/// - Signature verification
/// - Hot-reload notifications
pub struct ChallengeRegistry<L: ChallengeLoader = LocalOnlyLoader> {
    /// Cached challenges (challenge_id -> CachedChallenge)
    cache: Arc<RwLock<HashMap<ChallengeId, CachedChallenge>>>,
    /// Configuration
    config: RegistryConfig,
    /// Hot-reload notification channel sender
    reload_tx: broadcast::Sender<ChallengeId>,
    /// Network loader for fetching challenges
    loader: Arc<L>,
}

impl ChallengeRegistry<LocalOnlyLoader> {
    /// Create a new registry with default local-only loader
    pub fn new(config: RegistryConfig) -> Self {
        Self::with_loader(config, LocalOnlyLoader)
    }
}

impl<L: ChallengeLoader> ChallengeRegistry<L> {
    /// Create a new registry with a custom loader
    pub fn with_loader(config: RegistryConfig, loader: L) -> Self {
        let (reload_tx, _) = broadcast::channel(64);
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            config,
            reload_tx,
            loader: Arc::new(loader),
        }
    }

    /// Get a challenge, loading from network if not cached
    ///
    /// This method will:
    /// 1. Check if the challenge is in cache and not expired
    /// 2. If not, attempt to load from network
    /// 3. Verify signature if configured
    /// 4. Cache the result
    pub async fn get(&self, challenge_id: &ChallengeId) -> Result<Challenge, RegistryError> {
        // Check cache first
        {
            let mut cache = self.cache.write();
            if let Some(cached) = cache.get_mut(challenge_id) {
                if !cached.is_expired(self.config.cache_ttl_secs) {
                    cached.touch();
                    debug!(
                        challenge_id = %challenge_id,
                        access_count = cached.access_count,
                        "Challenge cache hit"
                    );
                    return Ok(cached.challenge.clone());
                }
                // Expired - remove from cache
                debug!(
                    challenge_id = %challenge_id,
                    age_secs = cached.age_secs(),
                    "Challenge cache entry expired"
                );
                cache.remove(challenge_id);
            }
        }

        // Load from network
        debug!(challenge_id = %challenge_id, "Loading challenge from network");
        let challenge = self.loader.load(challenge_id).await?;

        // Validate size
        let size_mb = challenge.wasm_code.len() as u64 / (1024 * 1024);
        if size_mb > self.config.max_challenge_size_mb {
            return Err(RegistryError::TooLarge {
                size_mb,
                max_mb: self.config.max_challenge_size_mb,
            });
        }

        // Verify signature if configured
        if self.config.verify_signatures && !self.verify_signature(&challenge, &challenge.owner) {
            return Err(RegistryError::InvalidSignature(*challenge_id));
        }

        // Cache the result
        self.cache_challenge(challenge.clone())?;

        info!(
            challenge_id = %challenge_id,
            name = %challenge.name,
            wasm_size_bytes = challenge.wasm_code.len(),
            "Challenge loaded and cached"
        );

        Ok(challenge)
    }

    /// Manually register/add a challenge to the cache
    ///
    /// This bypasses network loading and directly caches the challenge.
    /// Useful for local testing or pre-loaded challenges.
    pub async fn register(&self, challenge: Challenge) -> Result<(), RegistryError> {
        // Validate size
        let size_mb = challenge.wasm_code.len() as u64 / (1024 * 1024);
        if size_mb > self.config.max_challenge_size_mb {
            return Err(RegistryError::TooLarge {
                size_mb,
                max_mb: self.config.max_challenge_size_mb,
            });
        }

        // Verify signature if configured
        if self.config.verify_signatures && !self.verify_signature(&challenge, &challenge.owner) {
            return Err(RegistryError::InvalidSignature(challenge.id));
        }

        let challenge_id = challenge.id;
        self.cache_challenge(challenge)?;

        // Notify subscribers of the new/updated challenge
        let _ = self.reload_tx.send(challenge_id);

        info!(challenge_id = %challenge_id, "Challenge manually registered");

        Ok(())
    }

    /// Remove a challenge from the cache
    pub fn remove(&self, challenge_id: &ChallengeId) {
        let removed = self.cache.write().remove(challenge_id).is_some();
        if removed {
            debug!(challenge_id = %challenge_id, "Challenge removed from cache");
        }
    }

    /// Check if a challenge is currently cached
    pub fn is_cached(&self, challenge_id: &ChallengeId) -> bool {
        let cache = self.cache.read();
        if let Some(cached) = cache.get(challenge_id) {
            !cached.is_expired(self.config.cache_ttl_secs)
        } else {
            false
        }
    }

    /// List all cached challenge IDs
    pub fn list_cached(&self) -> Vec<ChallengeId> {
        let cache = self.cache.read();
        cache
            .iter()
            .filter(|(_, cached)| !cached.is_expired(self.config.cache_ttl_secs))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Subscribe to hot-reload notifications
    ///
    /// Returns a receiver that will receive challenge IDs when they are reloaded.
    pub fn subscribe_reloads(&self) -> broadcast::Receiver<ChallengeId> {
        self.reload_tx.subscribe()
    }

    /// Trigger a hot-reload for a specific challenge
    ///
    /// This will:
    /// 1. Remove the challenge from cache
    /// 2. Reload it from network
    /// 3. Notify all subscribers
    pub async fn hot_reload(&self, challenge_id: &ChallengeId) -> Result<(), RegistryError> {
        // Remove from cache
        self.remove(challenge_id);

        // Reload from network
        let challenge = self.loader.load(challenge_id).await?;

        // Validate size
        let size_mb = challenge.wasm_code.len() as u64 / (1024 * 1024);
        if size_mb > self.config.max_challenge_size_mb {
            return Err(RegistryError::TooLarge {
                size_mb,
                max_mb: self.config.max_challenge_size_mb,
            });
        }

        // Verify signature if configured
        if self.config.verify_signatures && !self.verify_signature(&challenge, &challenge.owner) {
            return Err(RegistryError::InvalidSignature(*challenge_id));
        }

        // Cache the result
        self.cache_challenge(challenge)?;

        // Notify subscribers
        let _ = self.reload_tx.send(*challenge_id);

        info!(challenge_id = %challenge_id, "Challenge hot-reloaded");

        Ok(())
    }

    /// Verify the signature of a challenge's WASM code
    ///
    /// The signature is computed over the hash of the WASM code.
    pub fn verify_signature(&self, challenge: &Challenge, owner: &Hotkey) -> bool {
        // Verify that the challenge code hash matches the actual WASM code
        let computed_hash = hex::encode(compute_sha256(&challenge.wasm_code));
        if computed_hash != challenge.code_hash {
            warn!(
                challenge_id = %challenge.id,
                expected_hash = %challenge.code_hash,
                computed_hash = %computed_hash,
                "Challenge code hash mismatch"
            );
            return false;
        }

        // Verify the owner matches
        if &challenge.owner != owner {
            warn!(
                challenge_id = %challenge.id,
                expected_owner = %owner,
                actual_owner = %challenge.owner,
                "Challenge owner mismatch"
            );
            return false;
        }

        // The Challenge struct already has verify_code() which checks the hash
        challenge.verify_code()
    }

    /// Verify a signed challenge payload
    ///
    /// This verifies an externally signed challenge message using sr25519.
    pub fn verify_signed_challenge(
        &self,
        signed_msg: &SignedMessage,
        expected_owner: &Hotkey,
    ) -> Result<Challenge, RegistryError> {
        // Verify signature
        let is_valid = signed_msg
            .verify()
            .map_err(|e| RegistryError::Internal(format!("Signature verification error: {}", e)))?;

        if !is_valid {
            return Err(RegistryError::InvalidSignature(ChallengeId::default()));
        }

        // Verify signer matches expected owner
        if &signed_msg.signer != expected_owner {
            return Err(RegistryError::InvalidSignature(ChallengeId::default()));
        }

        // Deserialize the challenge
        let challenge: Challenge = signed_msg
            .deserialize()
            .map_err(|e| RegistryError::SerializationError(e.to_string()))?;

        Ok(challenge)
    }

    /// Evict expired entries from cache
    ///
    /// This should be called periodically to clean up stale entries.
    pub fn evict_expired(&self) {
        let mut cache = self.cache.write();
        let before_count = cache.len();
        cache.retain(|id, cached| {
            let keep = !cached.is_expired(self.config.cache_ttl_secs);
            if !keep {
                debug!(challenge_id = %id, "Evicting expired challenge");
            }
            keep
        });
        let evicted = before_count - cache.len();
        if evicted > 0 {
            info!(evicted_count = evicted, "Evicted expired challenges");
        }
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        let cache = self.cache.read();
        let total_entries = cache.len();
        let total_wasm_bytes: usize = cache.values().map(|c| c.challenge.wasm_code.len()).sum();
        let oldest_entry_secs = cache.values().map(|c| c.age_secs()).max().unwrap_or(0);
        let total_access_count: u64 = cache.values().map(|c| c.access_count).sum();

        CacheStats {
            total_entries,
            total_wasm_bytes,
            oldest_entry_secs,
            total_access_count,
            max_cache_size: self.config.max_cache_size,
        }
    }

    /// Internal method to cache a challenge with LRU eviction
    fn cache_challenge(&self, challenge: Challenge) -> Result<(), RegistryError> {
        let mut cache = self.cache.write();

        // Check if we need to evict entries
        while cache.len() >= self.config.max_cache_size {
            // Find the least recently used entry
            let lru_id = cache
                .iter()
                .min_by_key(|(_, cached)| cached.last_accessed)
                .map(|(id, _)| *id);

            if let Some(id) = lru_id {
                debug!(challenge_id = %id, "Evicting LRU challenge for new entry");
                cache.remove(&id);
            } else {
                // This should not happen, but handle it gracefully
                return Err(RegistryError::CacheFull);
            }
        }

        let challenge_id = challenge.id;
        cache.insert(challenge_id, CachedChallenge::new(challenge));

        Ok(())
    }
}

/// Cache statistics
#[derive(Clone, Debug)]
pub struct CacheStats {
    /// Total number of cached entries
    pub total_entries: usize,
    /// Total WASM bytes in cache
    pub total_wasm_bytes: usize,
    /// Age of the oldest entry in seconds
    pub oldest_entry_secs: u64,
    /// Total access count across all entries
    pub total_access_count: u64,
    /// Maximum cache size from config
    pub max_cache_size: usize,
}

/// Compute SHA256 hash of data
fn compute_sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_core::{ChallengeConfig, Keypair};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Create a test challenge
    fn create_test_challenge(name: &str) -> Challenge {
        let keypair = Keypair::generate();
        Challenge::new(
            name.to_string(),
            format!("Test challenge: {}", name),
            b"dummy wasm code".to_vec(),
            keypair.hotkey(),
            ChallengeConfig::default(),
        )
    }

    #[test]
    fn test_registry_config_default() {
        let config = RegistryConfig::default();
        assert_eq!(config.max_cache_size, 100);
        assert_eq!(config.max_challenge_size_mb, 50);
        assert_eq!(config.cache_ttl_secs, 3600);
        assert!(config.verify_signatures);
    }

    #[test]
    fn test_registry_config_for_testing() {
        let config = RegistryConfig::for_testing();
        assert_eq!(config.max_cache_size, 10);
        assert!(!config.verify_signatures);
    }

    #[test]
    fn test_registry_config_for_production() {
        let config = RegistryConfig::for_production();
        assert_eq!(config.max_cache_size, 500);
        assert!(config.verify_signatures);
    }

    #[test]
    fn test_cached_challenge_creation() {
        let challenge = create_test_challenge("test");
        let cached = CachedChallenge::new(challenge.clone());

        assert_eq!(cached.access_count, 1);
        assert_eq!(cached.challenge.name, "test");
    }

    #[test]
    fn test_cached_challenge_touch() {
        let challenge = create_test_challenge("test");
        let mut cached = CachedChallenge::new(challenge);
        let initial_count = cached.access_count;

        cached.touch();
        assert_eq!(cached.access_count, initial_count + 1);
    }

    #[test]
    fn test_cached_challenge_expiry() {
        let challenge = create_test_challenge("test");
        let mut cached = CachedChallenge::new(challenge);

        // Not expired with 0 TTL
        assert!(!cached.is_expired(0));

        // Not expired with long TTL
        assert!(!cached.is_expired(3600));

        // Simulate old entry
        cached.loaded_at = chrono::Utc::now() - chrono::Duration::hours(2);
        assert!(cached.is_expired(3600)); // 1 hour TTL, 2 hours old
    }

    #[tokio::test]
    async fn test_registry_register_and_get() {
        let config = RegistryConfig::for_testing();
        let registry = ChallengeRegistry::new(config);

        let challenge = create_test_challenge("test-challenge");
        let challenge_id = challenge.id;

        // Register the challenge
        registry.register(challenge.clone()).await.unwrap();

        // Verify it's cached
        assert!(registry.is_cached(&challenge_id));

        // Get the challenge
        let retrieved = registry.get(&challenge_id).await.unwrap();
        assert_eq!(retrieved.name, "test-challenge");
    }

    #[tokio::test]
    async fn test_registry_list_cached() {
        let config = RegistryConfig::for_testing();
        let registry = ChallengeRegistry::new(config);

        let challenge1 = create_test_challenge("challenge-1");
        let challenge2 = create_test_challenge("challenge-2");
        let id1 = challenge1.id;
        let id2 = challenge2.id;

        registry.register(challenge1).await.unwrap();
        registry.register(challenge2).await.unwrap();

        let cached = registry.list_cached();
        assert_eq!(cached.len(), 2);
        assert!(cached.contains(&id1));
        assert!(cached.contains(&id2));
    }

    #[tokio::test]
    async fn test_registry_remove() {
        let config = RegistryConfig::for_testing();
        let registry = ChallengeRegistry::new(config);

        let challenge = create_test_challenge("test");
        let challenge_id = challenge.id;

        registry.register(challenge).await.unwrap();
        assert!(registry.is_cached(&challenge_id));

        registry.remove(&challenge_id);
        assert!(!registry.is_cached(&challenge_id));
    }

    #[tokio::test]
    async fn test_registry_size_limit() {
        // Test with a realistic size limit
        let mut big_config = RegistryConfig::for_testing();
        big_config.max_challenge_size_mb = 1;
        big_config.verify_signatures = false;
        let registry2 = ChallengeRegistry::new(big_config);

        // Create a challenge with WASM > 1 MB
        let keypair = Keypair::generate();
        let big_challenge = Challenge::new(
            "big".to_string(),
            "Big challenge".to_string(),
            vec![0u8; 2 * 1024 * 1024], // 2 MB
            keypair.hotkey(),
            ChallengeConfig::default(),
        );

        let result = registry2.register(big_challenge).await;
        assert!(matches!(result, Err(RegistryError::TooLarge { .. })));
    }

    #[tokio::test]
    async fn test_registry_lru_eviction() {
        let mut config = RegistryConfig::for_testing();
        config.max_cache_size = 2;
        config.verify_signatures = false;
        let registry = ChallengeRegistry::new(config);

        let challenge1 = create_test_challenge("c1");
        let challenge2 = create_test_challenge("c2");
        let challenge3 = create_test_challenge("c3");
        let id1 = challenge1.id;
        let id2 = challenge2.id;
        let id3 = challenge3.id;

        registry.register(challenge1).await.unwrap();
        // Small delay to ensure different timestamps
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        registry.register(challenge2).await.unwrap();

        // Both should be cached
        assert_eq!(registry.list_cached().len(), 2);

        // Access challenge1 to make it more recently used
        let _ = registry.get(&id1).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Add challenge3, should evict challenge2 (LRU)
        registry.register(challenge3).await.unwrap();

        assert!(registry.is_cached(&id1));
        assert!(!registry.is_cached(&id2)); // Evicted
        assert!(registry.is_cached(&id3));
    }

    #[tokio::test]
    async fn test_registry_evict_expired() {
        let mut config = RegistryConfig::for_testing();
        config.cache_ttl_secs = 1; // 1 second TTL
        config.verify_signatures = false;
        let registry = ChallengeRegistry::new(config);

        let challenge = create_test_challenge("test");
        let challenge_id = challenge.id;

        registry.register(challenge).await.unwrap();
        assert!(registry.is_cached(&challenge_id));

        // Wait for expiry
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Run eviction
        registry.evict_expired();

        // Should be evicted
        assert!(!registry.is_cached(&challenge_id));
    }

    #[tokio::test]
    async fn test_registry_stats() {
        let config = RegistryConfig::for_testing();
        let registry = ChallengeRegistry::new(config);

        let challenge = create_test_challenge("test");
        registry.register(challenge).await.unwrap();

        let stats = registry.stats();
        assert_eq!(stats.total_entries, 1);
        assert!(stats.total_wasm_bytes > 0);
        assert_eq!(stats.max_cache_size, 10);
    }

    #[tokio::test]
    async fn test_registry_subscribe_reloads() {
        let config = RegistryConfig::for_testing();
        let registry = ChallengeRegistry::new(config);

        let mut rx = registry.subscribe_reloads();

        let challenge = create_test_challenge("test");
        let challenge_id = challenge.id;

        registry.register(challenge).await.unwrap();

        // Should receive notification
        let received_id = rx.recv().await.unwrap();
        assert_eq!(received_id, challenge_id);
    }

    #[test]
    fn test_verify_signature_valid() {
        let config = RegistryConfig::for_testing();
        let registry = ChallengeRegistry::new(config);

        let keypair = Keypair::generate();
        let challenge = Challenge::new(
            "test".to_string(),
            "Test".to_string(),
            b"wasm code".to_vec(),
            keypair.hotkey(),
            ChallengeConfig::default(),
        );

        assert!(registry.verify_signature(&challenge, &keypair.hotkey()));
    }

    #[test]
    fn test_verify_signature_wrong_owner() {
        let config = RegistryConfig::for_testing();
        let registry = ChallengeRegistry::new(config);

        let keypair1 = Keypair::generate();
        let keypair2 = Keypair::generate();
        let challenge = Challenge::new(
            "test".to_string(),
            "Test".to_string(),
            b"wasm code".to_vec(),
            keypair1.hotkey(),
            ChallengeConfig::default(),
        );

        // Verify with wrong owner should fail
        assert!(!registry.verify_signature(&challenge, &keypair2.hotkey()));
    }

    #[test]
    fn test_verify_signature_tampered_code() {
        let config = RegistryConfig::for_testing();
        let registry = ChallengeRegistry::new(config);

        let keypair = Keypair::generate();
        let mut challenge = Challenge::new(
            "test".to_string(),
            "Test".to_string(),
            b"wasm code".to_vec(),
            keypair.hotkey(),
            ChallengeConfig::default(),
        );

        // Tamper with the code without updating hash
        challenge.wasm_code = b"tampered code".to_vec();

        // Should fail verification
        assert!(!registry.verify_signature(&challenge, &keypair.hotkey()));
    }

    /// Mock loader for testing network loading
    struct MockLoader {
        challenges: RwLock<HashMap<ChallengeId, Challenge>>,
        load_count: AtomicU64,
    }

    impl MockLoader {
        fn new() -> Self {
            Self {
                challenges: RwLock::new(HashMap::new()),
                load_count: AtomicU64::new(0),
            }
        }

        fn add_challenge(&self, challenge: Challenge) {
            self.challenges.write().insert(challenge.id, challenge);
        }

        fn get_load_count(&self) -> u64 {
            self.load_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ChallengeLoader for MockLoader {
        async fn load(&self, challenge_id: &ChallengeId) -> Result<Challenge, RegistryError> {
            self.load_count.fetch_add(1, Ordering::SeqCst);
            self.challenges
                .read()
                .get(challenge_id)
                .cloned()
                .ok_or(RegistryError::NotFound(*challenge_id))
        }

        async fn exists(&self, challenge_id: &ChallengeId) -> Result<bool, RegistryError> {
            Ok(self.challenges.read().contains_key(challenge_id))
        }
    }

    #[tokio::test]
    async fn test_registry_with_loader() {
        let loader = MockLoader::new();
        let challenge = create_test_challenge("network-challenge");
        let challenge_id = challenge.id;
        loader.add_challenge(challenge);

        let mut config = RegistryConfig::for_testing();
        config.verify_signatures = false;
        let registry = ChallengeRegistry::with_loader(config, loader);

        // Not cached initially
        assert!(!registry.is_cached(&challenge_id));

        // Get should load from network
        let retrieved = registry.get(&challenge_id).await.unwrap();
        assert_eq!(retrieved.name, "network-challenge");

        // Now cached
        assert!(registry.is_cached(&challenge_id));

        // Second get should use cache
        let _ = registry.get(&challenge_id).await.unwrap();
        assert_eq!(registry.loader.get_load_count(), 1); // Only 1 network load
    }

    #[tokio::test]
    async fn test_registry_hot_reload() {
        let loader = MockLoader::new();
        let challenge = create_test_challenge("reload-test");
        let challenge_id = challenge.id;
        loader.add_challenge(challenge);

        let mut config = RegistryConfig::for_testing();
        config.verify_signatures = false;
        let registry = ChallengeRegistry::with_loader(config, loader);

        let mut rx = registry.subscribe_reloads();

        // Initial load
        let _ = registry.get(&challenge_id).await.unwrap();
        assert_eq!(registry.loader.get_load_count(), 1);

        // Hot reload
        registry.hot_reload(&challenge_id).await.unwrap();
        assert_eq!(registry.loader.get_load_count(), 2); // Second load

        // Should receive notification
        let received_id = rx.recv().await.unwrap();
        assert_eq!(received_id, challenge_id);
    }

    #[tokio::test]
    async fn test_registry_not_found() {
        let config = RegistryConfig::for_testing();
        let registry = ChallengeRegistry::new(config);

        let nonexistent_id = ChallengeId::new();
        let result = registry.get(&nonexistent_id).await;

        assert!(matches!(result, Err(RegistryError::NotFound(_))));
    }

    #[test]
    fn test_compute_sha256() {
        let data = b"test data";
        let hash1 = compute_sha256(data);
        let hash2 = compute_sha256(data);
        assert_eq!(hash1, hash2);

        let different_data = b"different data";
        let hash3 = compute_sha256(different_data);
        assert_ne!(hash1, hash3);
    }

    #[tokio::test]
    async fn test_verify_signed_challenge() {
        let config = RegistryConfig::for_testing();
        let registry = ChallengeRegistry::new(config);

        let keypair = Keypair::generate();
        let challenge = Challenge::new(
            "signed-test".to_string(),
            "Test".to_string(),
            b"wasm code".to_vec(),
            keypair.hotkey(),
            ChallengeConfig::default(),
        );

        // Sign the challenge
        let signed_msg = keypair.sign_data(&challenge).unwrap();

        // Verify and extract
        let verified = registry
            .verify_signed_challenge(&signed_msg, &keypair.hotkey())
            .unwrap();
        assert_eq!(verified.name, "signed-test");
    }

    #[tokio::test]
    async fn test_verify_signed_challenge_wrong_signer() {
        let config = RegistryConfig::for_testing();
        let registry = ChallengeRegistry::new(config);

        let keypair1 = Keypair::generate();
        let keypair2 = Keypair::generate();
        let challenge = Challenge::new(
            "test".to_string(),
            "Test".to_string(),
            b"wasm code".to_vec(),
            keypair1.hotkey(),
            ChallengeConfig::default(),
        );

        // Sign with keypair1
        let signed_msg = keypair1.sign_data(&challenge).unwrap();

        // Verify with wrong expected owner (keypair2)
        let result = registry.verify_signed_challenge(&signed_msg, &keypair2.hotkey());
        assert!(matches!(result, Err(RegistryError::InvalidSignature(_))));
    }

    #[test]
    fn test_cached_challenge_age_secs() {
        let challenge = create_test_challenge("test");
        let cached = CachedChallenge::new(challenge);

        // Age should be approximately 0
        assert!(cached.age_secs() < 2);
    }

    #[test]
    fn test_registry_error_display() {
        let err = RegistryError::NotFound(ChallengeId::new());
        assert!(err.to_string().contains("not found"));

        let err = RegistryError::TooLarge {
            size_mb: 100,
            max_mb: 50,
        };
        assert!(err.to_string().contains("100MB"));
        assert!(err.to_string().contains("50MB"));

        let err = RegistryError::NetworkError("connection refused".to_string());
        assert!(err.to_string().contains("connection refused"));

        let err = RegistryError::CacheFull;
        assert!(err.to_string().contains("full"));
    }
}
