//! WASM-based challenge backend
//!
//! This module provides a WASM-based alternative to the Docker container backend
//! for challenge execution. It provides challenge lifecycle management with support
//! for hot-reloading and concurrent evaluations.
//!
//! ## Features
//! - Load and unload WASM challenge modules dynamically
//! - Hot-reload WASM modules without service interruption
//! - Concurrent evaluation support with proper synchronization
//! - Integrates with the existing wasm_runtime module for execution

use crate::wasm_runtime::{WasmError, WasmRuntime, WasmRuntimeConfig};
use parking_lot::RwLock;
use platform_core::ChallengeId;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, info};

/// Errors that can occur during WASM challenge backend operations
#[derive(Debug, Error)]
pub enum WasmBackendError {
    /// Challenge not found in loaded modules
    #[error("Challenge not loaded: {0}")]
    ChallengeNotLoaded(ChallengeId),

    /// Challenge already loaded
    #[error("Challenge already loaded: {0}")]
    ChallengeAlreadyLoaded(ChallengeId),

    /// WASM runtime error
    #[error("WASM runtime error: {0}")]
    RuntimeError(#[from] WasmError),

    /// Invalid WASM module
    #[error("Invalid WASM module: {0}")]
    InvalidModule(String),
}

/// A loaded WASM challenge instance
#[derive(Clone)]
pub struct WasmChallengeInstance {
    /// The challenge ID this instance belongs to
    pub challenge_id: ChallengeId,
    /// Original WASM bytecode
    pub wasm_code: Vec<u8>,
    /// Hash of the WASM code (for change detection)
    pub code_hash: String,
    /// When this module was loaded
    pub loaded_at: chrono::DateTime<chrono::Utc>,
}

impl WasmChallengeInstance {
    /// Create a new WASM challenge instance
    fn new(challenge_id: ChallengeId, wasm_code: Vec<u8>) -> Self {
        let code_hash = Self::compute_hash(&wasm_code);
        Self {
            challenge_id,
            wasm_code,
            code_hash,
            loaded_at: chrono::Utc::now(),
        }
    }

    /// Compute hash of the WASM code for change detection
    fn compute_hash(data: &[u8]) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

impl std::fmt::Debug for WasmChallengeInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmChallengeInstance")
            .field("challenge_id", &self.challenge_id)
            .field("code_hash", &self.code_hash)
            .field("wasm_code_len", &self.wasm_code.len())
            .field("loaded_at", &self.loaded_at)
            .finish()
    }
}

/// WASM-based challenge backend
///
/// This backend manages WASM challenge modules, providing lifecycle management
/// (load, unload, hot-reload) and evaluation capabilities. It uses the WasmRuntime
/// from the wasm_runtime module for actual WASM execution.
pub struct WasmChallengeBackend {
    runtime: WasmRuntime,
    challenges: Arc<RwLock<HashMap<ChallengeId, WasmChallengeInstance>>>,
}

impl WasmChallengeBackend {
    /// Create a new WASM challenge backend with the given configuration
    pub fn new(config: WasmRuntimeConfig) -> Result<Self, anyhow::Error> {
        let runtime = WasmRuntime::new(config)
            .map_err(|e| anyhow::anyhow!("Failed to create WASM runtime: {}", e))?;

        Ok(Self {
            runtime,
            challenges: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Create a backend with a pre-existing runtime (useful for testing)
    pub fn with_runtime(runtime: WasmRuntime) -> Self {
        Self {
            runtime,
            challenges: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Load a WASM challenge module
    ///
    /// This stores the WASM bytecode for later evaluation.
    /// Returns an error if a challenge with the same ID is already loaded.
    pub async fn load_challenge(
        &self,
        challenge_id: ChallengeId,
        wasm_code: Vec<u8>,
    ) -> Result<(), anyhow::Error> {
        // Check if already loaded
        if self.challenges.read().contains_key(&challenge_id) {
            return Err(WasmBackendError::ChallengeAlreadyLoaded(challenge_id).into());
        }

        // Validate the WASM bytecode by attempting a test evaluation
        // This ensures the module is valid and has required exports
        if let Err(e) = self.runtime.evaluate(&wasm_code, &[]).await {
            // Only fail on compilation errors, not runtime errors
            if matches!(e, WasmError::ModuleCompilation(_) | WasmError::InvalidBytecode(_)) {
                return Err(WasmBackendError::InvalidModule(e.to_string()).into());
            }
        }

        info!(
            challenge_id = %challenge_id,
            wasm_size = wasm_code.len(),
            "Loading WASM challenge module"
        );

        // Create the instance
        let instance = WasmChallengeInstance::new(challenge_id, wasm_code);

        // Store the loaded challenge
        self.challenges.write().insert(challenge_id, instance);

        info!(
            challenge_id = %challenge_id,
            "WASM challenge module loaded successfully"
        );

        Ok(())
    }

    /// Unload a WASM challenge module
    ///
    /// Removes the challenge from memory. Any pending evaluations will fail.
    pub async fn unload_challenge(&self, challenge_id: &ChallengeId) -> Result<(), anyhow::Error> {
        let removed = self.challenges.write().remove(challenge_id);

        match removed {
            Some(instance) => {
                info!(
                    challenge_id = %challenge_id,
                    code_hash = %instance.code_hash,
                    "WASM challenge module unloaded"
                );
                Ok(())
            }
            None => Err(WasmBackendError::ChallengeNotLoaded(*challenge_id).into()),
        }
    }

    /// Evaluate a challenge with the given input
    ///
    /// The evaluation calls the `evaluate` export in the WASM module with the
    /// provided input bytes and returns the score as a float (0.0 - 1.0).
    pub async fn evaluate(
        &self,
        challenge_id: &ChallengeId,
        input: &[u8],
    ) -> Result<f64, anyhow::Error> {
        // Get the WASM code for this challenge
        let wasm_code = {
            let challenges = self.challenges.read();
            let instance = challenges
                .get(challenge_id)
                .ok_or_else(|| WasmBackendError::ChallengeNotLoaded(*challenge_id))?;
            instance.wasm_code.clone()
        };

        debug!(
            challenge_id = %challenge_id,
            input_len = input.len(),
            "Evaluating challenge"
        );

        // Use the runtime to evaluate
        let score = self
            .runtime
            .evaluate(&wasm_code, input)
            .await
            .map_err(|e| anyhow::anyhow!("Evaluation failed: {}", e))?;

        debug!(
            challenge_id = %challenge_id,
            score = score,
            "Evaluation completed"
        );

        Ok(score)
    }

    /// Hot-reload a WASM challenge module
    ///
    /// Replaces the existing module with a new version atomically.
    /// This is useful for updating challenge logic without service interruption.
    pub async fn hot_reload(
        &self,
        challenge_id: &ChallengeId,
        new_wasm_code: Vec<u8>,
    ) -> Result<(), anyhow::Error> {
        // Check if challenge is loaded
        let old_hash = {
            let challenges = self.challenges.read();
            if !challenges.contains_key(challenge_id) {
                return Err(WasmBackendError::ChallengeNotLoaded(*challenge_id).into());
            }
            challenges
                .get(challenge_id)
                .map(|i| i.code_hash.clone())
                .unwrap_or_default()
        };

        info!(
            challenge_id = %challenge_id,
            new_wasm_size = new_wasm_code.len(),
            "Hot-reloading WASM challenge module"
        );

        // Validate the new WASM bytecode
        if let Err(e) = self.runtime.evaluate(&new_wasm_code, &[]).await {
            if matches!(e, WasmError::ModuleCompilation(_) | WasmError::InvalidBytecode(_)) {
                return Err(WasmBackendError::InvalidModule(e.to_string()).into());
            }
        }

        // Create new instance and atomically replace
        let new_instance = WasmChallengeInstance::new(*challenge_id, new_wasm_code);
        self.challenges.write().insert(*challenge_id, new_instance);

        info!(
            challenge_id = %challenge_id,
            old_hash = %old_hash,
            "WASM challenge module hot-reloaded successfully"
        );

        Ok(())
    }

    /// List all loaded challenge IDs
    pub fn list_loaded(&self) -> Vec<ChallengeId> {
        self.challenges.read().keys().cloned().collect()
    }

    /// Check if a challenge is currently loaded
    pub fn is_loaded(&self, challenge_id: &ChallengeId) -> bool {
        self.challenges.read().contains_key(challenge_id)
    }

    /// Get information about a loaded challenge
    pub fn get_challenge_info(&self, challenge_id: &ChallengeId) -> Option<WasmChallengeInstance> {
        self.challenges.read().get(challenge_id).cloned()
    }

    /// Get the number of loaded challenges
    pub fn loaded_count(&self) -> usize {
        self.challenges.read().len()
    }

    /// Get the runtime configuration
    pub fn runtime_config(&self) -> &WasmRuntimeConfig {
        self.runtime.config()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_core::ChallengeId;

    /// Create minimal valid WASM module bytes for testing
    /// Uses the wat crate to parse WAT (WebAssembly Text Format) into binary
    fn create_test_wasm_module() -> Vec<u8> {
        // WAT module with:
        // - Memory export
        // - evaluate(i32, i32) -> i64 that returns 500000 (0.5 score)
        // - validate(i32, i32) -> i32 that returns 1 (valid)
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

    /// Create a WASM module that returns a specific score
    fn create_wasm_with_score(score_fixed: i64) -> Vec<u8> {
        let wat = format!(
            r#"
            (module
                (memory (export "memory") 1)
                (func (export "evaluate") (param i32 i32) (result i64)
                    i64.const {})
                (func (export "validate") (param i32 i32) (result i32)
                    i32.const 1))
        "#,
            score_fixed
        );
        wat::parse_str(&wat).expect("failed to parse WAT")
    }

    #[test]
    fn test_wasm_challenge_instance_hash() {
        let challenge_id = ChallengeId::new();
        let wasm_code = vec![1, 2, 3, 4, 5];

        let instance = WasmChallengeInstance::new(challenge_id, wasm_code.clone());

        assert_eq!(instance.challenge_id, challenge_id);
        assert_eq!(instance.wasm_code, wasm_code);
        assert!(!instance.code_hash.is_empty());

        // Same code should produce same hash
        let instance2 = WasmChallengeInstance::new(challenge_id, wasm_code);
        assert_eq!(instance.code_hash, instance2.code_hash);

        // Different code should produce different hash
        let instance3 = WasmChallengeInstance::new(challenge_id, vec![5, 4, 3, 2, 1]);
        assert_ne!(instance.code_hash, instance3.code_hash);
    }

    #[test]
    fn test_wasm_challenge_instance_debug() {
        let challenge_id = ChallengeId::new();
        let instance = WasmChallengeInstance::new(challenge_id, vec![1, 2, 3]);

        let debug_str = format!("{:?}", instance);
        assert!(debug_str.contains("WasmChallengeInstance"));
        assert!(debug_str.contains("code_hash"));
        assert!(debug_str.contains("wasm_code_len"));
    }

    #[test]
    fn test_wasm_backend_creation() {
        let config = WasmRuntimeConfig::default();
        let backend = WasmChallengeBackend::new(config);
        assert!(backend.is_ok());

        let backend = backend.expect("backend should be created");
        assert_eq!(backend.loaded_count(), 0);
        assert!(backend.list_loaded().is_empty());
    }

    #[tokio::test]
    async fn test_wasm_backend_load_challenge() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm_module();

        let result = backend.load_challenge(challenge_id, wasm_code.clone()).await;
        assert!(result.is_ok(), "load_challenge should succeed: {:?}", result);

        assert!(backend.is_loaded(&challenge_id));
        assert_eq!(backend.loaded_count(), 1);
        assert!(backend.list_loaded().contains(&challenge_id));

        let info = backend.get_challenge_info(&challenge_id);
        assert!(info.is_some());
        let info = info.expect("info should exist");
        assert_eq!(info.challenge_id, challenge_id);
        assert_eq!(info.wasm_code, wasm_code);
    }

    #[tokio::test]
    async fn test_wasm_backend_load_duplicate_fails() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm_module();

        backend
            .load_challenge(challenge_id, wasm_code.clone())
            .await
            .expect("first load should succeed");

        let result = backend.load_challenge(challenge_id, wasm_code).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("already loaded"));
    }

    #[tokio::test]
    async fn test_wasm_backend_load_invalid_wasm_fails() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        let challenge_id = ChallengeId::new();
        let invalid_wasm = vec![0, 1, 2, 3]; // Not valid WASM

        let result = backend.load_challenge(challenge_id, invalid_wasm).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("invalid") 
                || err.to_string().to_lowercase().contains("wasm")
                || err.to_string().to_lowercase().contains("module"),
            "Expected WASM error, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_wasm_backend_unload_challenge() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm_module();

        backend
            .load_challenge(challenge_id, wasm_code)
            .await
            .expect("load should succeed");
        assert!(backend.is_loaded(&challenge_id));

        let result = backend.unload_challenge(&challenge_id).await;
        assert!(result.is_ok());

        assert!(!backend.is_loaded(&challenge_id));
        assert_eq!(backend.loaded_count(), 0);
        assert!(backend.get_challenge_info(&challenge_id).is_none());
    }

    #[tokio::test]
    async fn test_wasm_backend_unload_not_loaded_fails() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        let challenge_id = ChallengeId::new();
        let result = backend.unload_challenge(&challenge_id).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not loaded"));
    }

    #[tokio::test]
    async fn test_wasm_backend_evaluate() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm_module(); // Returns 0.5

        backend
            .load_challenge(challenge_id, wasm_code)
            .await
            .expect("load should succeed");

        let input = b"test input data";
        let result = backend.evaluate(&challenge_id, input).await;

        assert!(result.is_ok(), "evaluate should succeed: {:?}", result);
        let score = result.expect("score should be returned");
        assert!((score - 0.5).abs() < 0.001, "Score should be ~0.5, got {}", score);
    }

    #[tokio::test]
    async fn test_wasm_backend_evaluate_not_loaded_fails() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        let challenge_id = ChallengeId::new();
        let result = backend.evaluate(&challenge_id, b"test").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not loaded"));
    }

    #[tokio::test]
    async fn test_wasm_backend_evaluate_score_clamping() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        // Test score above 1.0 (1.5 as fixed-point = 1_500_000)
        let challenge_id = ChallengeId::new();
        let wasm_high = create_wasm_with_score(1_500_000);

        backend
            .load_challenge(challenge_id, wasm_high)
            .await
            .expect("load should succeed");

        let score = backend
            .evaluate(&challenge_id, b"test")
            .await
            .expect("evaluate should succeed");
        assert!(
            (score - 1.0).abs() < 0.001,
            "Score should be clamped to 1.0, got {}",
            score
        );
    }

    #[tokio::test]
    async fn test_wasm_backend_hot_reload() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        let challenge_id = ChallengeId::new();
        let wasm_v1 = create_test_wasm_module(); // Returns 0.5

        backend
            .load_challenge(challenge_id, wasm_v1.clone())
            .await
            .expect("load should succeed");

        let info_v1 = backend
            .get_challenge_info(&challenge_id)
            .expect("info should exist");
        let hash_v1 = info_v1.code_hash.clone();

        // Hot reload with different WASM (returns 0.75)
        let wasm_v2 = create_wasm_with_score(750_000);
        let result = backend.hot_reload(&challenge_id, wasm_v2.clone()).await;
        assert!(result.is_ok(), "hot_reload should succeed: {:?}", result);

        let info_v2 = backend
            .get_challenge_info(&challenge_id)
            .expect("info should exist after reload");

        // Hash should be different
        assert_ne!(hash_v1, info_v2.code_hash);
        assert_eq!(info_v2.wasm_code, wasm_v2);

        // Evaluate should use new module
        let score = backend
            .evaluate(&challenge_id, b"test")
            .await
            .expect("evaluate should succeed");
        assert!(
            (score - 0.75).abs() < 0.001,
            "Score should be ~0.75 after reload, got {}",
            score
        );
    }

    #[tokio::test]
    async fn test_wasm_backend_hot_reload_not_loaded_fails() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm_module();

        let result = backend.hot_reload(&challenge_id, wasm_code).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not loaded"));
    }

    #[tokio::test]
    async fn test_wasm_backend_hot_reload_invalid_wasm_fails() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm_module();

        backend
            .load_challenge(challenge_id, wasm_code.clone())
            .await
            .expect("load should succeed");

        let info_before = backend
            .get_challenge_info(&challenge_id)
            .expect("info should exist");

        // Try to hot reload with invalid WASM
        let invalid_wasm = vec![0, 1, 2, 3];
        let result = backend.hot_reload(&challenge_id, invalid_wasm).await;
        assert!(result.is_err());

        // Original module should still be loaded
        let info_after = backend
            .get_challenge_info(&challenge_id)
            .expect("info should still exist");
        assert_eq!(info_before.code_hash, info_after.code_hash);
    }

    #[tokio::test]
    async fn test_wasm_backend_multiple_challenges() {
        let config = WasmRuntimeConfig::minimal();
        let backend = WasmChallengeBackend::new(config).expect("backend should be created");

        let id1 = ChallengeId::new();
        let id2 = ChallengeId::new();
        let id3 = ChallengeId::new();

        let wasm1 = create_wasm_with_score(250_000); // 0.25
        let wasm2 = create_wasm_with_score(500_000); // 0.50
        let wasm3 = create_wasm_with_score(750_000); // 0.75

        backend
            .load_challenge(id1, wasm1)
            .await
            .expect("load 1 should succeed");
        backend
            .load_challenge(id2, wasm2)
            .await
            .expect("load 2 should succeed");
        backend
            .load_challenge(id3, wasm3)
            .await
            .expect("load 3 should succeed");

        assert_eq!(backend.loaded_count(), 3);

        let s1 = backend.evaluate(&id1, b"test").await.expect("eval 1");
        let s2 = backend.evaluate(&id2, b"test").await.expect("eval 2");
        let s3 = backend.evaluate(&id3, b"test").await.expect("eval 3");

        assert!((s1 - 0.25).abs() < 0.001);
        assert!((s2 - 0.50).abs() < 0.001);
        assert!((s3 - 0.75).abs() < 0.001);

        // Unload one
        backend.unload_challenge(&id2).await.expect("unload should succeed");
        assert_eq!(backend.loaded_count(), 2);
        assert!(backend.is_loaded(&id1));
        assert!(!backend.is_loaded(&id2));
        assert!(backend.is_loaded(&id3));
    }

    #[tokio::test]
    async fn test_wasm_backend_concurrent_evaluations() {
        let config = WasmRuntimeConfig::minimal();
        let backend = Arc::new(WasmChallengeBackend::new(config).expect("backend should be created"));

        let challenge_id = ChallengeId::new();
        let wasm_code = create_test_wasm_module();

        backend
            .load_challenge(challenge_id, wasm_code)
            .await
            .expect("load should succeed");

        // Spawn multiple concurrent evaluations
        let mut handles = Vec::new();
        for i in 0..10 {
            let backend_clone = Arc::clone(&backend);
            let handle = tokio::spawn(async move {
                let input = format!("test input {}", i);
                backend_clone.evaluate(&challenge_id, input.as_bytes()).await
            });
            handles.push(handle);
        }

        // All should succeed
        for handle in handles {
            let result = handle.await.expect("task should complete");
            assert!(result.is_ok(), "concurrent evaluation should succeed");
            let score = result.expect("score");
            assert!((score - 0.5).abs() < 0.001);
        }
    }

    #[test]
    fn test_wasm_backend_with_runtime() {
        let config = WasmRuntimeConfig::default();
        let runtime = WasmRuntime::new(config.clone()).expect("runtime should be created");

        let backend = WasmChallengeBackend::with_runtime(runtime);
        assert_eq!(backend.loaded_count(), 0);
    }

    #[test]
    fn test_wasm_backend_error_display() {
        let id = ChallengeId::new();

        let err = WasmBackendError::ChallengeNotLoaded(id);
        assert!(err.to_string().contains("not loaded"));

        let err = WasmBackendError::ChallengeAlreadyLoaded(id);
        assert!(err.to_string().contains("already loaded"));

        let err = WasmBackendError::InvalidModule("bad module".to_string());
        assert!(err.to_string().contains("Invalid"));
    }
}
