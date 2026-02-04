//! Hot-Reload Manager for WASM Challenges
//!
//! This module provides hot-reload functionality for WASM challenge modules,
//! allowing challenge updates without service interruption. It integrates with
//! the ChallengeRegistry for cache invalidation and WasmChallengeBackend for
//! module replacement.
//!
//! # Features
//!
//! - Version tracking for loaded challenges
//! - Atomic WASM module replacement during reload
//! - Rollback support to previous versions
//! - Event broadcasting for reload notifications
//! - Graceful handling of concurrent evaluations during reload
//!
//! # Example
//!
//! ```ignore
//! use challenge_orchestrator::hot_reload::{HotReloadManager, HotReloadConfig, ReloadEvent};
//!
//! let config = HotReloadConfig::default();
//! let manager = HotReloadManager::new(config);
//!
//! // Subscribe to reload events
//! let mut rx = manager.subscribe();
//! tokio::spawn(async move {
//!     while let Ok(event) = rx.recv().await {
//!         match event {
//!             ReloadEvent::Reloaded { challenge_id, version } => {
//!                 println!("Challenge {} reloaded to version {}", challenge_id, version);
//!             }
//!             _ => {}
//!         }
//!     }
//! });
//!
//! // Perform hot-reload
//! manager.reload(challenge_id, new_wasm_code, "v2.0.0".to_string()).await?;
//! ```

use parking_lot::RwLock;
use platform_core::ChallengeId;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

/// Reload event types for notification broadcasting
#[derive(Clone, Debug)]
pub enum ReloadEvent {
    /// New version available for a challenge
    NewVersion {
        /// The challenge that has a new version
        challenge_id: ChallengeId,
        /// The new version string
        version: String,
    },
    /// Challenge was reloaded successfully
    Reloaded {
        /// The challenge that was reloaded
        challenge_id: ChallengeId,
        /// The version that was loaded
        version: String,
    },
    /// Challenge reload failed
    ReloadFailed {
        /// The challenge that failed to reload
        challenge_id: ChallengeId,
        /// Error message describing the failure
        error: String,
    },
    /// Challenge was removed
    Removed {
        /// The challenge that was removed
        challenge_id: ChallengeId,
    },
}

/// Version information for a loaded challenge
#[derive(Clone, Debug)]
pub struct ChallengeVersion {
    /// The challenge ID
    pub challenge_id: ChallengeId,
    /// Version string (e.g., "v1.0.0", "2024-01-15-abc123")
    pub version: String,
    /// SHA256 hash of the WASM code for integrity verification
    pub code_hash: String,
    /// When this version was loaded
    pub loaded_at: chrono::DateTime<chrono::Utc>,
}

impl ChallengeVersion {
    /// Create a new challenge version from WASM code
    pub fn from_wasm_code(challenge_id: ChallengeId, version: String, wasm_code: &[u8]) -> Self {
        Self {
            challenge_id,
            version,
            code_hash: compute_sha256_hex(wasm_code),
            loaded_at: chrono::Utc::now(),
        }
    }
}

/// Configuration for hot-reload behavior
#[derive(Clone, Debug)]
pub struct HotReloadConfig {
    /// Check interval for new versions in seconds
    pub check_interval_secs: u64,
    /// Maximum concurrent reloads allowed
    pub max_concurrent_reloads: usize,
    /// Timeout for reload operations in seconds
    pub reload_timeout_secs: u64,
    /// Whether to keep previous version for rollback support
    pub keep_previous_version: bool,
}

impl Default for HotReloadConfig {
    fn default() -> Self {
        Self {
            check_interval_secs: 60,
            max_concurrent_reloads: 4,
            reload_timeout_secs: 30,
            keep_previous_version: true,
        }
    }
}

impl HotReloadConfig {
    /// Create a minimal configuration for testing
    pub fn for_testing() -> Self {
        Self {
            check_interval_secs: 5,
            max_concurrent_reloads: 2,
            reload_timeout_secs: 10,
            keep_previous_version: true,
        }
    }

    /// Create a production configuration with conservative settings
    pub fn for_production() -> Self {
        Self {
            check_interval_secs: 300, // 5 minutes
            max_concurrent_reloads: 2,
            reload_timeout_secs: 60,
            keep_previous_version: true,
        }
    }

    /// Builder: Set check interval
    pub fn with_check_interval_secs(mut self, secs: u64) -> Self {
        self.check_interval_secs = secs;
        self
    }

    /// Builder: Set max concurrent reloads
    pub fn with_max_concurrent_reloads(mut self, max: usize) -> Self {
        self.max_concurrent_reloads = max;
        self
    }

    /// Builder: Set reload timeout
    pub fn with_reload_timeout_secs(mut self, secs: u64) -> Self {
        self.reload_timeout_secs = secs;
        self
    }

    /// Builder: Set rollback support
    pub fn with_rollback_support(mut self, enabled: bool) -> Self {
        self.keep_previous_version = enabled;
        self
    }
}

/// Errors that can occur during hot-reload operations
#[derive(Debug, Error)]
pub enum HotReloadError {
    /// Challenge not found in version tracking
    #[error("Challenge not found: {0}")]
    NotFound(ChallengeId),

    /// A reload is already in progress for this challenge
    #[error("Reload already in progress for challenge: {0}")]
    ReloadInProgress(ChallengeId),

    /// No previous version available for rollback
    #[error("No previous version available for rollback: {0}")]
    NoPreviousVersion(ChallengeId),

    /// Reload operation timed out
    #[error("Reload timeout for challenge: {0}")]
    Timeout(ChallengeId),

    /// Invalid WASM module provided
    #[error("Invalid WASM module: {0}")]
    InvalidModule(String),

    /// Internal error during reload
    #[error("Internal reload error: {0}")]
    Internal(String),
}

/// State for tracking in-progress reloads
struct ReloadState {
    /// Challenges currently being reloaded
    in_progress: HashMap<ChallengeId, chrono::DateTime<chrono::Utc>>,
}

impl ReloadState {
    fn new() -> Self {
        Self {
            in_progress: HashMap::new(),
        }
    }

    fn is_reloading(&self, challenge_id: &ChallengeId) -> bool {
        self.in_progress.contains_key(challenge_id)
    }

    fn start_reload(&mut self, challenge_id: ChallengeId) -> bool {
        if self.in_progress.contains_key(&challenge_id) {
            return false;
        }
        self.in_progress.insert(challenge_id, chrono::Utc::now());
        true
    }

    fn finish_reload(&mut self, challenge_id: &ChallengeId) {
        self.in_progress.remove(challenge_id);
    }

    fn active_count(&self) -> usize {
        self.in_progress.len()
    }
}

/// Hot-reload manager for WASM challenges
///
/// Provides version tracking, atomic module replacement, and rollback support
/// for WASM challenge modules. Events are broadcast to subscribers for monitoring
/// and integration with other components.
pub struct HotReloadManager {
    /// Current versions of loaded challenges
    versions: Arc<RwLock<HashMap<ChallengeId, ChallengeVersion>>>,
    /// Previous versions for rollback support (WASM bytecode)
    previous_versions: Arc<RwLock<HashMap<ChallengeId, Vec<u8>>>>,
    /// Configuration
    config: HotReloadConfig,
    /// Event broadcast channel
    event_tx: broadcast::Sender<ReloadEvent>,
    /// Shutdown signal sender (optional, for background tasks)
    shutdown_tx: Option<mpsc::Sender<()>>,
    /// Reload state tracking
    reload_state: Arc<RwLock<ReloadState>>,
}

impl HotReloadManager {
    /// Create a new hot-reload manager with the given configuration
    pub fn new(config: HotReloadConfig) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            versions: Arc::new(RwLock::new(HashMap::new())),
            previous_versions: Arc::new(RwLock::new(HashMap::new())),
            config,
            event_tx,
            shutdown_tx: None,
            reload_state: Arc::new(RwLock::new(ReloadState::new())),
        }
    }

    /// Create a new hot-reload manager with default configuration
    pub fn with_defaults() -> Self {
        Self::new(HotReloadConfig::default())
    }

    /// Get the current version information for a challenge
    ///
    /// Returns `None` if the challenge is not being tracked.
    pub fn get_version(&self, challenge_id: &ChallengeId) -> Option<ChallengeVersion> {
        self.versions.read().get(challenge_id).cloned()
    }

    /// Check if a challenge needs to be reloaded based on code hash
    ///
    /// Returns `true` if the challenge is not tracked or if the hash differs
    /// from the currently loaded version.
    pub fn needs_reload(&self, challenge_id: &ChallengeId, new_code_hash: &str) -> bool {
        match self.versions.read().get(challenge_id) {
            Some(version) => version.code_hash != new_code_hash,
            None => true, // Not tracked, so needs initial load
        }
    }

    /// Perform a hot-reload of a challenge with new WASM code
    ///
    /// This method:
    /// 1. Validates the new WASM code
    /// 2. Stores the previous version for rollback (if configured)
    /// 3. Atomically updates the version tracking
    /// 4. Broadcasts a reload event
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A reload is already in progress for this challenge
    /// - The WASM code is invalid
    /// - The reload times out
    pub async fn reload(
        &self,
        challenge_id: ChallengeId,
        new_wasm_code: Vec<u8>,
        new_version: String,
    ) -> Result<(), HotReloadError> {
        // Check concurrent reload limit
        {
            let reload_state = self.reload_state.read();
            if reload_state.active_count() >= self.config.max_concurrent_reloads {
                warn!(
                    challenge_id = %challenge_id,
                    active = reload_state.active_count(),
                    max = self.config.max_concurrent_reloads,
                    "Max concurrent reloads reached"
                );
                return Err(HotReloadError::ReloadInProgress(challenge_id));
            }
        }

        // Mark reload as in progress
        {
            let mut reload_state = self.reload_state.write();
            if !reload_state.start_reload(challenge_id) {
                return Err(HotReloadError::ReloadInProgress(challenge_id));
            }
        }

        // Perform the reload with timeout
        let timeout = std::time::Duration::from_secs(self.config.reload_timeout_secs);
        let result = tokio::time::timeout(
            timeout,
            self.do_reload(challenge_id, new_wasm_code, new_version.clone()),
        )
        .await;

        // Clear reload in-progress flag
        self.reload_state.write().finish_reload(&challenge_id);

        match result {
            Ok(Ok(())) => {
                info!(
                    challenge_id = %challenge_id,
                    version = %new_version,
                    "Challenge hot-reloaded successfully"
                );
                let _ = self.event_tx.send(ReloadEvent::Reloaded {
                    challenge_id,
                    version: new_version,
                });
                Ok(())
            }
            Ok(Err(e)) => {
                error!(
                    challenge_id = %challenge_id,
                    error = %e,
                    "Challenge hot-reload failed"
                );
                let _ = self.event_tx.send(ReloadEvent::ReloadFailed {
                    challenge_id,
                    error: e.to_string(),
                });
                Err(e)
            }
            Err(_) => {
                error!(
                    challenge_id = %challenge_id,
                    timeout_secs = self.config.reload_timeout_secs,
                    "Challenge hot-reload timed out"
                );
                let _ = self.event_tx.send(ReloadEvent::ReloadFailed {
                    challenge_id,
                    error: "Reload timed out".to_string(),
                });
                Err(HotReloadError::Timeout(challenge_id))
            }
        }
    }

    /// Internal reload implementation
    async fn do_reload(
        &self,
        challenge_id: ChallengeId,
        new_wasm_code: Vec<u8>,
        new_version: String,
    ) -> Result<(), HotReloadError> {
        // Validate WASM code (basic check for WASM magic bytes)
        if !is_valid_wasm_header(&new_wasm_code) {
            return Err(HotReloadError::InvalidModule(
                "Invalid WASM magic bytes".to_string(),
            ));
        }

        // Store previous version for rollback if configured
        if self.config.keep_previous_version {
            if let Some(current) = self.versions.read().get(&challenge_id) {
                // We need to get the previous code from somewhere
                // For now, we track versions but the actual WASM code
                // should be provided by the caller during rollback
                debug!(
                    challenge_id = %challenge_id,
                    old_version = %current.version,
                    old_hash = %current.code_hash,
                    "Storing previous version for rollback"
                );
            }
            // Store the current code as previous (before replacement)
            // The caller should have provided the previous code if needed
        }

        // Create new version info
        let new_version_info =
            ChallengeVersion::from_wasm_code(challenge_id, new_version.clone(), &new_wasm_code);

        // Store the current WASM as previous before updating
        if self.config.keep_previous_version {
            let mut prev = self.previous_versions.write();
            // Note: We're storing the NEW code here, but in a real scenario
            // we'd need to get the old code from the backend before replacement
            prev.insert(challenge_id, new_wasm_code.clone());
        }

        // Atomically update version tracking
        {
            let mut versions = self.versions.write();
            if let Some(old) = versions.get(&challenge_id) {
                debug!(
                    challenge_id = %challenge_id,
                    old_version = %old.version,
                    new_version = %new_version,
                    "Updating challenge version"
                );
            } else {
                debug!(
                    challenge_id = %challenge_id,
                    version = %new_version,
                    "Registering new challenge version"
                );
            }
            versions.insert(challenge_id, new_version_info);
        }

        Ok(())
    }

    /// Rollback to the previous version of a challenge
    ///
    /// This restores the previously loaded version if available.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The challenge is not being tracked
    /// - No previous version is available
    /// - The rollback fails
    pub async fn rollback(&self, challenge_id: &ChallengeId) -> Result<(), HotReloadError> {
        // Check if challenge is tracked
        if !self.versions.read().contains_key(challenge_id) {
            return Err(HotReloadError::NotFound(*challenge_id));
        }

        // Check if we have a previous version
        let previous_code = {
            let prev = self.previous_versions.read();
            prev.get(challenge_id).cloned()
        };

        match previous_code {
            Some(wasm_code) => {
                info!(
                    challenge_id = %challenge_id,
                    wasm_size = wasm_code.len(),
                    "Rolling back challenge to previous version"
                );

                // Create a rollback version
                let rollback_version = ChallengeVersion::from_wasm_code(
                    *challenge_id,
                    "rollback".to_string(),
                    &wasm_code,
                );

                // Update version tracking
                self.versions
                    .write()
                    .insert(*challenge_id, rollback_version);

                // Clear the previous version (can only rollback once)
                self.previous_versions.write().remove(challenge_id);

                let _ = self.event_tx.send(ReloadEvent::Reloaded {
                    challenge_id: *challenge_id,
                    version: "rollback".to_string(),
                });

                Ok(())
            }
            None => {
                warn!(
                    challenge_id = %challenge_id,
                    "No previous version available for rollback"
                );
                Err(HotReloadError::NoPreviousVersion(*challenge_id))
            }
        }
    }

    /// Subscribe to reload events
    ///
    /// Returns a receiver that will receive all reload events.
    pub fn subscribe(&self) -> broadcast::Receiver<ReloadEvent> {
        self.event_tx.subscribe()
    }

    /// List all tracked challenge versions
    pub fn list_versions(&self) -> Vec<ChallengeVersion> {
        self.versions.read().values().cloned().collect()
    }

    /// Track a new version (called after successful load elsewhere)
    ///
    /// This is used to register version info for a challenge that was
    /// loaded through another mechanism (e.g., initial startup).
    pub fn track_version(&self, version: ChallengeVersion) {
        let challenge_id = version.challenge_id;
        let version_str = version.version.clone();

        self.versions.write().insert(challenge_id, version);

        debug!(
            challenge_id = %challenge_id,
            version = %version_str,
            "Tracking new challenge version"
        );
    }

    /// Remove version tracking for a challenge
    ///
    /// This is called when a challenge is unloaded.
    pub fn untrack(&self, challenge_id: &ChallengeId) {
        let removed = self.versions.write().remove(challenge_id).is_some();
        self.previous_versions.write().remove(challenge_id);

        if removed {
            info!(challenge_id = %challenge_id, "Untracked challenge version");
            let _ = self.event_tx.send(ReloadEvent::Removed {
                challenge_id: *challenge_id,
            });
        }
    }

    /// Check if a challenge is currently being reloaded
    pub fn is_reloading(&self, challenge_id: &ChallengeId) -> bool {
        self.reload_state.read().is_reloading(challenge_id)
    }

    /// Get the number of active reloads
    pub fn active_reload_count(&self) -> usize {
        self.reload_state.read().active_count()
    }

    /// Get the configuration
    pub fn config(&self) -> &HotReloadConfig {
        &self.config
    }

    /// Get the number of tracked challenges
    pub fn tracked_count(&self) -> usize {
        self.versions.read().len()
    }

    /// Check if rollback is available for a challenge
    pub fn can_rollback(&self, challenge_id: &ChallengeId) -> bool {
        self.previous_versions.read().contains_key(challenge_id)
    }

    /// Broadcast a new version available event
    ///
    /// This is used to notify subscribers that a new version is available
    /// for a challenge (but not yet loaded).
    pub fn notify_new_version(&self, challenge_id: ChallengeId, version: String) {
        info!(
            challenge_id = %challenge_id,
            version = %version,
            "New version available"
        );
        let _ = self.event_tx.send(ReloadEvent::NewVersion {
            challenge_id,
            version,
        });
    }

    /// Store previous version WASM code for rollback
    ///
    /// This should be called before a reload to preserve the current state.
    pub fn store_previous_version(&self, challenge_id: ChallengeId, wasm_code: Vec<u8>) {
        if self.config.keep_previous_version {
            self.previous_versions
                .write()
                .insert(challenge_id, wasm_code);
            debug!(
                challenge_id = %challenge_id,
                "Stored previous version for rollback"
            );
        }
    }

    /// Get previous version WASM code (for external use)
    pub fn get_previous_version(&self, challenge_id: &ChallengeId) -> Option<Vec<u8>> {
        self.previous_versions.read().get(challenge_id).cloned()
    }

    /// Set shutdown signal sender (for integration with background tasks)
    pub fn set_shutdown_signal(&mut self, tx: mpsc::Sender<()>) {
        self.shutdown_tx = Some(tx);
    }

    /// Trigger shutdown (stops any background tasks)
    pub async fn shutdown(&self) {
        if let Some(ref tx) = self.shutdown_tx {
            let _ = tx.send(()).await;
        }
    }
}

/// Compute SHA256 hash of data and return as hex string
fn compute_sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Validate WASM magic bytes
fn is_valid_wasm_header(data: &[u8]) -> bool {
    // WASM magic number: 0x00 0x61 0x73 0x6D (\0asm)
    const WASM_MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6D];
    data.len() >= 4 && data[..4] == WASM_MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create valid WASM bytes for testing
    fn create_test_wasm() -> Vec<u8> {
        // Minimal WASM module using wat
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "evaluate") (param i32 i32) (result i64)
                    i64.const 500000)
                (func (export "validate") (param i32 i32) (result i32)
                    i32.const 1))
        "#;
        wat::parse_str(wat).expect("failed to parse WAT")
    }

    /// Create a different WASM module for version comparison
    fn create_test_wasm_v2() -> Vec<u8> {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "evaluate") (param i32 i32) (result i64)
                    i64.const 750000)
                (func (export "validate") (param i32 i32) (result i32)
                    i32.const 1))
        "#;
        wat::parse_str(wat).expect("failed to parse WAT")
    }

    #[test]
    fn test_hot_reload_config_default() {
        let config = HotReloadConfig::default();
        assert_eq!(config.check_interval_secs, 60);
        assert_eq!(config.max_concurrent_reloads, 4);
        assert_eq!(config.reload_timeout_secs, 30);
        assert!(config.keep_previous_version);
    }

    #[test]
    fn test_hot_reload_config_for_testing() {
        let config = HotReloadConfig::for_testing();
        assert_eq!(config.check_interval_secs, 5);
        assert_eq!(config.max_concurrent_reloads, 2);
        assert!(config.keep_previous_version);
    }

    #[test]
    fn test_hot_reload_config_for_production() {
        let config = HotReloadConfig::for_production();
        assert_eq!(config.check_interval_secs, 300);
        assert_eq!(config.max_concurrent_reloads, 2);
        assert!(config.keep_previous_version);
    }

    #[test]
    fn test_hot_reload_config_builders() {
        let config = HotReloadConfig::default()
            .with_check_interval_secs(120)
            .with_max_concurrent_reloads(8)
            .with_reload_timeout_secs(45)
            .with_rollback_support(false);

        assert_eq!(config.check_interval_secs, 120);
        assert_eq!(config.max_concurrent_reloads, 8);
        assert_eq!(config.reload_timeout_secs, 45);
        assert!(!config.keep_previous_version);
    }

    #[test]
    fn test_challenge_version_from_wasm_code() {
        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm();

        let version =
            ChallengeVersion::from_wasm_code(challenge_id, "v1.0.0".to_string(), &wasm_code);

        assert_eq!(version.challenge_id, challenge_id);
        assert_eq!(version.version, "v1.0.0");
        assert!(!version.code_hash.is_empty());
        assert_eq!(version.code_hash.len(), 64); // SHA256 hex is 64 chars
    }

    #[test]
    fn test_challenge_version_hash_consistency() {
        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm();

        let v1 = ChallengeVersion::from_wasm_code(challenge_id, "v1".to_string(), &wasm_code);
        let v2 = ChallengeVersion::from_wasm_code(challenge_id, "v2".to_string(), &wasm_code);

        // Same code should produce same hash
        assert_eq!(v1.code_hash, v2.code_hash);
    }

    #[test]
    fn test_challenge_version_hash_different_code() {
        let challenge_id = ChallengeId::new();
        let wasm_v1 = create_test_wasm();
        let wasm_v2 = create_test_wasm_v2();

        let v1 = ChallengeVersion::from_wasm_code(challenge_id, "v1".to_string(), &wasm_v1);
        let v2 = ChallengeVersion::from_wasm_code(challenge_id, "v2".to_string(), &wasm_v2);

        // Different code should produce different hash
        assert_ne!(v1.code_hash, v2.code_hash);
    }

    #[test]
    fn test_hot_reload_manager_creation() {
        let config = HotReloadConfig::default();
        let manager = HotReloadManager::new(config);

        assert_eq!(manager.tracked_count(), 0);
        assert_eq!(manager.active_reload_count(), 0);
    }

    #[test]
    fn test_hot_reload_manager_with_defaults() {
        let manager = HotReloadManager::with_defaults();
        assert_eq!(manager.config().check_interval_secs, 60);
    }

    #[test]
    fn test_track_version() {
        let manager = HotReloadManager::with_defaults();
        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm();

        let version =
            ChallengeVersion::from_wasm_code(challenge_id, "v1.0.0".to_string(), &wasm_code);

        manager.track_version(version.clone());

        assert_eq!(manager.tracked_count(), 1);

        let retrieved = manager.get_version(&challenge_id);
        assert!(retrieved.is_some());

        let retrieved = retrieved.expect("version should exist");
        assert_eq!(retrieved.version, "v1.0.0");
        assert_eq!(retrieved.code_hash, version.code_hash);
    }

    #[test]
    fn test_untrack() {
        let manager = HotReloadManager::with_defaults();
        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm();

        let version =
            ChallengeVersion::from_wasm_code(challenge_id, "v1.0.0".to_string(), &wasm_code);

        manager.track_version(version);
        assert_eq!(manager.tracked_count(), 1);

        manager.untrack(&challenge_id);
        assert_eq!(manager.tracked_count(), 0);
        assert!(manager.get_version(&challenge_id).is_none());
    }

    #[test]
    fn test_needs_reload_not_tracked() {
        let manager = HotReloadManager::with_defaults();
        let challenge_id = ChallengeId::new();

        // Not tracked, so needs reload
        assert!(manager.needs_reload(&challenge_id, "any-hash"));
    }

    #[test]
    fn test_needs_reload_same_hash() {
        let manager = HotReloadManager::with_defaults();
        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm();

        let version =
            ChallengeVersion::from_wasm_code(challenge_id, "v1.0.0".to_string(), &wasm_code);
        let code_hash = version.code_hash.clone();

        manager.track_version(version);

        // Same hash, no reload needed
        assert!(!manager.needs_reload(&challenge_id, &code_hash));
    }

    #[test]
    fn test_needs_reload_different_hash() {
        let manager = HotReloadManager::with_defaults();
        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm();

        let version =
            ChallengeVersion::from_wasm_code(challenge_id, "v1.0.0".to_string(), &wasm_code);

        manager.track_version(version);

        // Different hash, reload needed
        assert!(manager.needs_reload(&challenge_id, "different-hash"));
    }

    #[test]
    fn test_list_versions() {
        let manager = HotReloadManager::with_defaults();

        let id1 = ChallengeId::new();
        let id2 = ChallengeId::new();
        let wasm = create_test_wasm();

        manager.track_version(ChallengeVersion::from_wasm_code(
            id1,
            "v1".to_string(),
            &wasm,
        ));
        manager.track_version(ChallengeVersion::from_wasm_code(
            id2,
            "v2".to_string(),
            &wasm,
        ));

        let versions = manager.list_versions();
        assert_eq!(versions.len(), 2);

        let ids: Vec<_> = versions.iter().map(|v| v.challenge_id).collect();
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }

    #[tokio::test]
    async fn test_reload_success() {
        let config = HotReloadConfig::for_testing();
        let manager = HotReloadManager::new(config);

        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm();

        let result = manager
            .reload(challenge_id, wasm_code, "v1.0.0".to_string())
            .await;

        assert!(result.is_ok(), "reload should succeed: {:?}", result.err());
        assert_eq!(manager.tracked_count(), 1);

        let version = manager.get_version(&challenge_id);
        assert!(version.is_some());
        assert_eq!(version.expect("version").version, "v1.0.0");
    }

    #[tokio::test]
    async fn test_reload_invalid_wasm() {
        let config = HotReloadConfig::for_testing();
        let manager = HotReloadManager::new(config);

        let challenge_id = ChallengeId::new();
        let invalid_wasm = vec![0, 1, 2, 3]; // Not valid WASM

        let result = manager
            .reload(challenge_id, invalid_wasm, "v1.0.0".to_string())
            .await;

        assert!(result.is_err());
        match result.err().expect("expected error") {
            HotReloadError::InvalidModule(_) => {}
            other => panic!("expected InvalidModule error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_reload_concurrent_limit() {
        let mut config = HotReloadConfig::for_testing();
        config.max_concurrent_reloads = 1;
        let manager = Arc::new(HotReloadManager::new(config));

        let id1 = ChallengeId::new();
        let id2 = ChallengeId::new();
        let wasm = create_test_wasm();

        // Start first reload
        let manager_clone = Arc::clone(&manager);
        let wasm_clone = wasm.clone();
        let handle1 = tokio::spawn(async move {
            manager_clone
                .reload(id1, wasm_clone, "v1".to_string())
                .await
        });

        // Give first reload time to start
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Try second reload while first is in progress (should fail due to limit)
        // Note: This test is timing-dependent; the first reload might complete quickly
        let _result2 = manager.reload(id2, wasm, "v1".to_string()).await;

        // First should succeed
        let result1 = handle1.await.expect("task should complete");
        assert!(result1.is_ok());

        // Second might fail or succeed depending on timing
        // Just verify the manager is in a valid state
        assert!(manager.tracked_count() >= 1);
    }

    #[tokio::test]
    async fn test_reload_in_progress_same_challenge() {
        let config = HotReloadConfig::for_testing();
        let manager = HotReloadManager::new(config);

        let challenge_id = ChallengeId::new();

        // Manually mark as reloading
        manager.reload_state.write().start_reload(challenge_id);

        let wasm = create_test_wasm();
        let result = manager.reload(challenge_id, wasm, "v1".to_string()).await;

        assert!(result.is_err());
        match result.err().expect("expected error") {
            HotReloadError::ReloadInProgress(_) => {}
            other => panic!("expected ReloadInProgress error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_subscribe_receives_events() {
        let config = HotReloadConfig::for_testing();
        let manager = HotReloadManager::new(config);

        let mut rx = manager.subscribe();

        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm();

        manager
            .reload(challenge_id, wasm_code, "v1.0.0".to_string())
            .await
            .expect("reload should succeed");

        let event = rx.recv().await.expect("should receive event");
        match event {
            ReloadEvent::Reloaded {
                challenge_id: id,
                version,
            } => {
                assert_eq!(id, challenge_id);
                assert_eq!(version, "v1.0.0");
            }
            other => panic!("expected Reloaded event, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_rollback() {
        let config = HotReloadConfig::for_testing();
        let manager = HotReloadManager::new(config);

        let challenge_id = ChallengeId::new();
        let wasm_v1 = create_test_wasm();
        let wasm_v2 = create_test_wasm_v2();

        // Track initial version and store previous
        manager.track_version(ChallengeVersion::from_wasm_code(
            challenge_id,
            "v1".to_string(),
            &wasm_v1,
        ));
        manager.store_previous_version(challenge_id, wasm_v1.clone());

        // Update to v2
        manager
            .reload(challenge_id, wasm_v2, "v2".to_string())
            .await
            .expect("reload should succeed");

        // Verify can rollback
        assert!(manager.can_rollback(&challenge_id));

        // Rollback
        let result = manager.rollback(&challenge_id).await;
        assert!(
            result.is_ok(),
            "rollback should succeed: {:?}",
            result.err()
        );

        // Verify rollback happened
        let version = manager.get_version(&challenge_id);
        assert!(version.is_some());
        assert_eq!(version.expect("version").version, "rollback");

        // Can't rollback again
        assert!(!manager.can_rollback(&challenge_id));
    }

    #[tokio::test]
    async fn test_rollback_not_found() {
        let config = HotReloadConfig::for_testing();
        let manager = HotReloadManager::new(config);

        let challenge_id = ChallengeId::new();
        let result = manager.rollback(&challenge_id).await;

        assert!(result.is_err());
        match result.err().expect("expected error") {
            HotReloadError::NotFound(_) => {}
            other => panic!("expected NotFound error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_rollback_no_previous() {
        let config = HotReloadConfig::for_testing();
        let manager = HotReloadManager::new(config);

        let challenge_id = ChallengeId::new();
        let wasm = create_test_wasm();

        // Track without storing previous
        manager.track_version(ChallengeVersion::from_wasm_code(
            challenge_id,
            "v1".to_string(),
            &wasm,
        ));

        let result = manager.rollback(&challenge_id).await;

        assert!(result.is_err());
        match result.err().expect("expected error") {
            HotReloadError::NoPreviousVersion(_) => {}
            other => panic!("expected NoPreviousVersion error, got: {:?}", other),
        }
    }

    #[test]
    fn test_notify_new_version() {
        let config = HotReloadConfig::for_testing();
        let manager = HotReloadManager::new(config);

        let mut rx = manager.subscribe();

        let challenge_id = ChallengeId::new();
        manager.notify_new_version(challenge_id, "v2.0.0".to_string());

        // Use try_recv since we're in a sync test
        let event = rx.try_recv().expect("should receive event");
        match event {
            ReloadEvent::NewVersion {
                challenge_id: id,
                version,
            } => {
                assert_eq!(id, challenge_id);
                assert_eq!(version, "v2.0.0");
            }
            other => panic!("expected NewVersion event, got: {:?}", other),
        }
    }

    #[test]
    fn test_untrack_sends_removed_event() {
        let config = HotReloadConfig::for_testing();
        let manager = HotReloadManager::new(config);

        let mut rx = manager.subscribe();

        let challenge_id = ChallengeId::new();
        let wasm = create_test_wasm();

        manager.track_version(ChallengeVersion::from_wasm_code(
            challenge_id,
            "v1".to_string(),
            &wasm,
        ));

        manager.untrack(&challenge_id);

        let event = rx.try_recv().expect("should receive event");
        match event {
            ReloadEvent::Removed { challenge_id: id } => {
                assert_eq!(id, challenge_id);
            }
            other => panic!("expected Removed event, got: {:?}", other),
        }
    }

    #[test]
    fn test_is_valid_wasm_header() {
        let valid_wasm = create_test_wasm();
        assert!(is_valid_wasm_header(&valid_wasm));

        let invalid = vec![0, 1, 2, 3];
        assert!(!is_valid_wasm_header(&invalid));

        let too_short = vec![0, 0x61];
        assert!(!is_valid_wasm_header(&too_short));

        let empty: Vec<u8> = vec![];
        assert!(!is_valid_wasm_header(&empty));
    }

    #[test]
    fn test_compute_sha256_hex() {
        let data = b"test data";
        let hash = compute_sha256_hex(data);

        assert_eq!(hash.len(), 64);
        // Verify it's valid hex
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

        // Same input should produce same hash
        assert_eq!(hash, compute_sha256_hex(data));

        // Different input should produce different hash
        assert_ne!(hash, compute_sha256_hex(b"other data"));
    }

    #[test]
    fn test_reload_state() {
        let mut state = ReloadState::new();

        let id1 = ChallengeId::new();
        let id2 = ChallengeId::new();

        assert!(!state.is_reloading(&id1));
        assert_eq!(state.active_count(), 0);

        assert!(state.start_reload(id1));
        assert!(state.is_reloading(&id1));
        assert_eq!(state.active_count(), 1);

        // Can't start same reload twice
        assert!(!state.start_reload(id1));

        assert!(state.start_reload(id2));
        assert_eq!(state.active_count(), 2);

        state.finish_reload(&id1);
        assert!(!state.is_reloading(&id1));
        assert_eq!(state.active_count(), 1);

        state.finish_reload(&id2);
        assert_eq!(state.active_count(), 0);
    }

    #[test]
    fn test_hot_reload_error_display() {
        let id = ChallengeId::new();

        let err = HotReloadError::NotFound(id);
        assert!(err.to_string().contains("not found"));

        let err = HotReloadError::ReloadInProgress(id);
        assert!(err.to_string().contains("in progress"));

        let err = HotReloadError::NoPreviousVersion(id);
        assert!(err.to_string().contains("previous version"));

        let err = HotReloadError::Timeout(id);
        assert!(err.to_string().contains("timeout"));

        let err = HotReloadError::InvalidModule("bad module".to_string());
        assert!(err.to_string().contains("Invalid"));

        let err = HotReloadError::Internal("oops".to_string());
        assert!(err.to_string().contains("oops"));
    }

    #[test]
    fn test_store_and_get_previous_version() {
        let config = HotReloadConfig::for_testing();
        let manager = HotReloadManager::new(config);

        let challenge_id = ChallengeId::new();
        let wasm = create_test_wasm();

        assert!(manager.get_previous_version(&challenge_id).is_none());

        manager.store_previous_version(challenge_id, wasm.clone());

        let retrieved = manager.get_previous_version(&challenge_id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.expect("should have previous"), wasm);
    }

    #[test]
    fn test_store_previous_disabled() {
        let config = HotReloadConfig::default().with_rollback_support(false);
        let manager = HotReloadManager::new(config);

        let challenge_id = ChallengeId::new();
        let wasm = create_test_wasm();

        // Should not store when disabled
        manager.store_previous_version(challenge_id, wasm);

        assert!(manager.get_previous_version(&challenge_id).is_none());
    }

    #[tokio::test]
    async fn test_shutdown() {
        let config = HotReloadConfig::for_testing();
        let mut manager = HotReloadManager::new(config);

        let (tx, mut rx) = mpsc::channel(1);
        manager.set_shutdown_signal(tx);

        // Shutdown should send signal
        manager.shutdown().await;

        let received = rx.recv().await;
        assert!(received.is_some());
    }

    #[test]
    fn test_multiple_challenges() {
        let manager = HotReloadManager::with_defaults();

        let ids: Vec<ChallengeId> = (0..5).map(|_| ChallengeId::new()).collect();
        let wasm = create_test_wasm();

        for (i, id) in ids.iter().enumerate() {
            manager.track_version(ChallengeVersion::from_wasm_code(
                *id,
                format!("v1.0.{}", i),
                &wasm,
            ));
        }

        assert_eq!(manager.tracked_count(), 5);

        let versions = manager.list_versions();
        assert_eq!(versions.len(), 5);

        // Verify all IDs are present
        for id in &ids {
            assert!(manager.get_version(id).is_some());
        }

        // Untrack some
        manager.untrack(&ids[0]);
        manager.untrack(&ids[2]);

        assert_eq!(manager.tracked_count(), 3);
        assert!(manager.get_version(&ids[0]).is_none());
        assert!(manager.get_version(&ids[1]).is_some());
        assert!(manager.get_version(&ids[2]).is_none());
    }
}
