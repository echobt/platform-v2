//! WASM Runtime for Challenge Execution
//!
//! This module provides a sandboxed WASM execution environment for running
//! challenge evaluations. It uses wasmtime for secure, memory-isolated execution
//! with configurable resource limits.
//!
//! # Architecture
//!
//! The WASM runtime expects challenge modules to export two key functions:
//! - `evaluate(input_ptr: i32, input_len: i32) -> i64` - Returns score as fixed-point
//! - `validate(input_ptr: i32, input_len: i32) -> i32` - Returns 1 if valid, 0 otherwise
//!
//! Memory is managed through a shared linear memory that both the host and guest
//! can access. Input data is written to guest memory, and results are returned
//! through the function return value.

use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use wasmtime::{
    Config, Engine, Extern, Instance, Linker, Memory, MemoryType, Module, Store, StoreLimits,
    StoreLimitsBuilder,
};

/// Errors that can occur during WASM execution
#[derive(Error, Debug)]
pub enum WasmError {
    /// Failed to create the WASM engine
    #[error("Failed to create WASM engine: {0}")]
    EngineCreation(String),

    /// Failed to compile the WASM module
    #[error("Failed to compile WASM module: {0}")]
    ModuleCompilation(String),

    /// Failed to instantiate the WASM module
    #[error("Failed to instantiate WASM module: {0}")]
    ModuleInstantiation(String),

    /// Required function not found in WASM module
    #[error("Required function not exported: {0}")]
    MissingFunction(String),

    /// Memory allocation failed
    #[error("Memory allocation failed: {0}")]
    MemoryAllocation(String),

    /// Execution timed out
    #[error("WASM execution timed out after {0} seconds")]
    Timeout(u64),

    /// Execution failed
    #[error("WASM execution failed: {0}")]
    ExecutionFailed(String),

    /// Memory access violation
    #[error("Memory access violation: {0}")]
    MemoryAccessViolation(String),

    /// Invalid WASM bytecode
    #[error("Invalid WASM bytecode: {0}")]
    InvalidBytecode(String),

    /// Fuel exhausted (CPU limit reached)
    #[error("Fuel exhausted - CPU limit exceeded")]
    FuelExhausted,

    /// Resource limit exceeded
    #[error("Resource limit exceeded: {0}")]
    ResourceLimitExceeded(String),
}

/// Configuration for the WASM runtime
#[derive(Clone, Debug)]
pub struct WasmRuntimeConfig {
    /// Maximum memory allocation in megabytes
    pub max_memory_mb: u64,
    /// Maximum CPU time in seconds
    pub max_cpu_secs: u64,
    /// Fuel limit for deterministic execution (None = unlimited)
    /// Each WASM instruction consumes fuel, providing a way to limit CPU usage
    pub fuel_limit: Option<u64>,
}

impl Default for WasmRuntimeConfig {
    fn default() -> Self {
        Self {
            max_memory_mb: 512,
            max_cpu_secs: 60,
            fuel_limit: Some(100_000_000), // Default fuel limit
        }
    }
}

impl WasmRuntimeConfig {
    /// Create a minimal config for testing
    pub fn minimal() -> Self {
        Self {
            max_memory_mb: 64,
            max_cpu_secs: 10,
            fuel_limit: Some(10_000_000),
        }
    }

    /// Create a config with no fuel limit (use with caution)
    pub fn unlimited_fuel(mut self) -> Self {
        self.fuel_limit = None;
        self
    }

    /// Set the maximum memory in megabytes
    pub fn with_max_memory_mb(mut self, mb: u64) -> Self {
        self.max_memory_mb = mb;
        self
    }

    /// Set the maximum CPU time in seconds
    pub fn with_max_cpu_secs(mut self, secs: u64) -> Self {
        self.max_cpu_secs = secs;
        self
    }

    /// Set the fuel limit
    pub fn with_fuel_limit(mut self, fuel: u64) -> Self {
        self.fuel_limit = Some(fuel);
        self
    }
}

/// Store data for WASM execution context
struct StoreData {
    limits: StoreLimits,
}

/// WASM Runtime for executing challenge evaluation code
///
/// The runtime provides:
/// - Memory isolation with configurable limits
/// - CPU time limits via fuel consumption
/// - Timeout handling for long-running evaluations
/// - Safe memory management for input/output
pub struct WasmRuntime {
    engine: Arc<Engine>,
    config: WasmRuntimeConfig,
}

impl WasmRuntime {
    /// Create a new WASM runtime with the given configuration
    pub fn new(config: WasmRuntimeConfig) -> Result<Self, WasmError> {
        let mut engine_config = Config::new();

        // Enable async support for timeout handling
        engine_config.async_support(true);

        // Enable fuel consumption for CPU limiting if configured
        if config.fuel_limit.is_some() {
            engine_config.consume_fuel(true);
        }

        // Enable optimizations
        engine_config.cranelift_opt_level(wasmtime::OptLevel::Speed);

        // Create the engine
        let engine =
            Engine::new(&engine_config).map_err(|e| WasmError::EngineCreation(e.to_string()))?;

        Ok(Self {
            engine: Arc::new(engine),
            config,
        })
    }

    /// Evaluate an agent submission using the challenge WASM code
    ///
    /// # Arguments
    /// * `wasm_code` - The challenge WASM bytecode
    /// * `input` - The agent submission data to evaluate
    ///
    /// # Returns
    /// A score between 0.0 and 1.0 on success
    pub async fn evaluate(&self, wasm_code: &[u8], input: &[u8]) -> Result<f64, WasmError> {
        let timeout = Duration::from_secs(self.config.max_cpu_secs);
        let result = tokio::time::timeout(timeout, self.execute_evaluate(wasm_code, input)).await;

        match result {
            Ok(inner_result) => inner_result,
            Err(_) => Err(WasmError::Timeout(self.config.max_cpu_secs)),
        }
    }

    /// Validate an agent submission using the challenge WASM code
    ///
    /// # Arguments
    /// * `wasm_code` - The challenge WASM bytecode
    /// * `input` - The agent submission data to validate
    ///
    /// # Returns
    /// `true` if the submission is valid, `false` otherwise
    pub async fn validate(&self, wasm_code: &[u8], input: &[u8]) -> Result<bool, WasmError> {
        let timeout = Duration::from_secs(self.config.max_cpu_secs);
        let result = tokio::time::timeout(timeout, self.execute_validate(wasm_code, input)).await;

        match result {
            Ok(inner_result) => inner_result,
            Err(_) => Err(WasmError::Timeout(self.config.max_cpu_secs)),
        }
    }

    /// Internal execution of the evaluate function
    async fn execute_evaluate(&self, wasm_code: &[u8], input: &[u8]) -> Result<f64, WasmError> {
        // Compile module
        let module = Module::new(&self.engine, wasm_code)
            .map_err(|e| WasmError::ModuleCompilation(e.to_string()))?;

        // Create store with limits
        let mut store = self.create_store()?;

        // Instantiate with memory
        let (instance, memory) = self.instantiate_with_memory(&mut store, &module).await?;

        // Write input to memory
        let input_ptr = self.write_to_memory(&mut store, &memory, input)?;

        // Get the evaluate function
        let evaluate_func = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "evaluate")
            .map_err(|e| WasmError::MissingFunction(format!("evaluate: {}", e)))?;

        // Call the function
        let input_len = input.len() as i32;
        let result = evaluate_func
            .call_async(&mut store, (input_ptr, input_len))
            .await
            .map_err(|e| {
                if e.to_string().contains("fuel") {
                    WasmError::FuelExhausted
                } else {
                    WasmError::ExecutionFailed(e.to_string())
                }
            })?;

        // Convert fixed-point result to f64 (assumes 6 decimal places: 1_000_000 = 1.0)
        let score = (result as f64) / 1_000_000.0;

        // Clamp to valid range
        Ok(score.clamp(0.0, 1.0))
    }

    /// Internal execution of the validate function
    async fn execute_validate(&self, wasm_code: &[u8], input: &[u8]) -> Result<bool, WasmError> {
        // Compile module
        let module = Module::new(&self.engine, wasm_code)
            .map_err(|e| WasmError::ModuleCompilation(e.to_string()))?;

        // Create store with limits
        let mut store = self.create_store()?;

        // Instantiate with memory
        let (instance, memory) = self.instantiate_with_memory(&mut store, &module).await?;

        // Write input to memory
        let input_ptr = self.write_to_memory(&mut store, &memory, input)?;

        // Get the validate function
        let validate_func = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "validate")
            .map_err(|e| WasmError::MissingFunction(format!("validate: {}", e)))?;

        // Call the function
        let input_len = input.len() as i32;
        let result = validate_func
            .call_async(&mut store, (input_ptr, input_len))
            .await
            .map_err(|e| {
                if e.to_string().contains("fuel") {
                    WasmError::FuelExhausted
                } else {
                    WasmError::ExecutionFailed(e.to_string())
                }
            })?;

        // Convention: 1 = valid, 0 = invalid
        Ok(result == 1)
    }

    /// Create a store with configured limits
    fn create_store(&self) -> Result<Store<StoreData>, WasmError> {
        // Calculate memory limits in bytes
        let max_memory_bytes = self.config.max_memory_mb * 1024 * 1024;

        let limits = StoreLimitsBuilder::new()
            .memory_size(max_memory_bytes as usize)
            .memories(1) // Only allow 1 memory
            .tables(10) // Reasonable table limit
            .instances(1) // Only allow 1 instance
            .build();

        let mut store = Store::new(&self.engine, StoreData { limits });
        store.limiter(|data| &mut data.limits);

        // Set fuel if configured
        if let Some(fuel) = self.config.fuel_limit {
            store
                .set_fuel(fuel)
                .map_err(|e| WasmError::EngineCreation(format!("Failed to set fuel: {}", e)))?;
        }

        Ok(store)
    }

    /// Instantiate a WASM module with memory
    async fn instantiate_with_memory(
        &self,
        store: &mut Store<StoreData>,
        module: &Module,
    ) -> Result<(Instance, Memory), WasmError> {
        // Check if module exports memory or needs it imported
        let has_memory_import = module
            .imports()
            .any(|import| import.ty().memory().is_some());

        let mut linker = Linker::new(&self.engine);

        let memory = if has_memory_import {
            // Module expects memory to be imported - create and provide it
            let memory_type = MemoryType::new(1, Some(self.config.max_memory_mb as u32 * 16)); // 16 pages per MB
            let memory = Memory::new(&mut *store, memory_type)
                .map_err(|e| WasmError::MemoryAllocation(e.to_string()))?;

            // Find the memory import name
            for import in module.imports() {
                if let Some(_mem_type) = import.ty().memory() {
                    linker
                        .define(&mut *store, import.module(), import.name(), memory)
                        .map_err(|e| {
                            WasmError::ModuleInstantiation(format!(
                                "Failed to define memory: {}",
                                e
                            ))
                        })?;
                    break;
                }
            }

            memory
        } else {
            // Module exports its own memory - we'll get it after instantiation
            Memory::new(&mut *store, MemoryType::new(0, None))
                .map_err(|e| WasmError::MemoryAllocation(e.to_string()))?
        };

        // Instantiate the module (must use async when async support is enabled)
        let instance = linker
            .instantiate_async(&mut *store, module)
            .await
            .map_err(|e| WasmError::ModuleInstantiation(e.to_string()))?;

        // Get the actual memory from the instance if it exports one
        let actual_memory =
            if let Some(Extern::Memory(mem)) = instance.get_export(&mut *store, "memory") {
                mem
            } else {
                memory
            };

        Ok((instance, actual_memory))
    }

    /// Write data to WASM linear memory
    fn write_to_memory(
        &self,
        store: &mut Store<StoreData>,
        memory: &Memory,
        data: &[u8],
    ) -> Result<i32, WasmError> {
        // Start writing at offset 0 (simple allocation strategy)
        // In production, you'd want a proper allocator
        let offset: usize = 0;

        // Ensure memory is large enough
        let required_pages = data.len().div_ceil(65536) as u64;
        let current_pages = memory.size(&*store);

        if current_pages < required_pages {
            let delta = required_pages - current_pages;
            memory.grow(&mut *store, delta).map_err(|e| {
                WasmError::MemoryAllocation(format!("Failed to grow memory: {}", e))
            })?;
        }

        // Write data to memory
        memory.write(&mut *store, offset, data).map_err(|e| {
            WasmError::MemoryAccessViolation(format!("Failed to write to memory: {}", e))
        })?;

        Ok(offset as i32)
    }

    /// Get runtime configuration
    pub fn config(&self) -> &WasmRuntimeConfig {
        &self.config
    }

    /// Create a runtime with default configuration
    pub fn with_defaults() -> Result<Self, WasmError> {
        Self::new(WasmRuntimeConfig::default())
    }
}

impl Clone for WasmRuntime {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
            config: self.config.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a minimal WASM module that exports evaluate and validate functions
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
    fn create_score_wasm_module(score_fixed: i64) -> Vec<u8> {
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

    /// Create a WASM module that returns invalid (0)
    fn create_invalid_wasm_module() -> Vec<u8> {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "evaluate") (param i32 i32) (result i64)
                    i64.const 500000)
                (func (export "validate") (param i32 i32) (result i32)
                    i32.const 0))
        "#;
        wat::parse_str(wat).expect("failed to parse WAT")
    }

    #[tokio::test]
    async fn test_wasm_runtime_creation() {
        let config = WasmRuntimeConfig::default();
        let runtime = WasmRuntime::new(config);
        assert!(runtime.is_ok());
    }

    #[tokio::test]
    async fn test_wasm_runtime_with_defaults() {
        let runtime = WasmRuntime::with_defaults();
        assert!(runtime.is_ok());
        let rt = runtime.expect("runtime creation failed");
        assert_eq!(rt.config().max_memory_mb, 512);
        assert_eq!(rt.config().max_cpu_secs, 60);
    }

    #[tokio::test]
    async fn test_wasm_runtime_minimal_config() {
        let config = WasmRuntimeConfig::minimal();
        assert_eq!(config.max_memory_mb, 64);
        assert_eq!(config.max_cpu_secs, 10);
    }

    #[tokio::test]
    async fn test_config_builders() {
        let config = WasmRuntimeConfig::default()
            .with_max_memory_mb(256)
            .with_max_cpu_secs(30)
            .with_fuel_limit(50_000_000);

        assert_eq!(config.max_memory_mb, 256);
        assert_eq!(config.max_cpu_secs, 30);
        assert_eq!(config.fuel_limit, Some(50_000_000));
    }

    #[tokio::test]
    async fn test_config_unlimited_fuel() {
        let config = WasmRuntimeConfig::default().unlimited_fuel();
        assert_eq!(config.fuel_limit, None);
    }

    #[tokio::test]
    async fn test_evaluate_basic() {
        let runtime =
            WasmRuntime::new(WasmRuntimeConfig::minimal()).expect("runtime creation failed");
        let wasm_code = create_test_wasm_module();
        let input = b"test input data";

        let result = runtime.evaluate(&wasm_code, input).await;
        assert!(result.is_ok(), "evaluate failed: {:?}", result.err());

        let score = result.expect("score extraction failed");
        // Test module returns 500000, which is 0.5
        assert!((score - 0.5).abs() < 0.001, "expected ~0.5, got {}", score);
    }

    #[tokio::test]
    async fn test_validate_basic() {
        let runtime =
            WasmRuntime::new(WasmRuntimeConfig::minimal()).expect("runtime creation failed");
        let wasm_code = create_test_wasm_module();
        let input = b"test input data";

        let result = runtime.validate(&wasm_code, input).await;
        assert!(result.is_ok(), "validate failed: {:?}", result.err());

        let is_valid = result.expect("validation result extraction failed");
        assert!(is_valid, "expected valid");
    }

    #[tokio::test]
    async fn test_validate_returns_false() {
        let runtime =
            WasmRuntime::new(WasmRuntimeConfig::minimal()).expect("runtime creation failed");
        let wasm_code = create_invalid_wasm_module();
        let input = b"test input data";

        let result = runtime.validate(&wasm_code, input).await;
        assert!(result.is_ok(), "validate failed: {:?}", result.err());

        let is_valid = result.expect("validation result extraction failed");
        assert!(!is_valid, "expected invalid");
    }

    #[tokio::test]
    async fn test_score_clamping_high() {
        let runtime =
            WasmRuntime::new(WasmRuntimeConfig::minimal()).expect("runtime creation failed");
        // Create module that returns 2_000_000 (would be 2.0 unclamped)
        let wasm_code = create_score_wasm_module(2_000_000);
        let input = b"test";

        let result = runtime.evaluate(&wasm_code, input).await;
        assert!(result.is_ok());

        let score = result.expect("score extraction failed");
        // Should be clamped to 1.0
        assert!(
            (score - 1.0).abs() < 0.001,
            "expected 1.0 (clamped), got {}",
            score
        );
    }

    #[tokio::test]
    async fn test_score_clamping_low() {
        let runtime =
            WasmRuntime::new(WasmRuntimeConfig::minimal()).expect("runtime creation failed");
        // Create module that returns -100000 (would be -0.1 unclamped)
        let wasm_code = create_score_wasm_module(-100_000);
        let input = b"test";

        let result = runtime.evaluate(&wasm_code, input).await;
        assert!(result.is_ok());

        let score = result.expect("score extraction failed");
        // Should be clamped to 0.0
        assert!(
            (score - 0.0).abs() < 0.001,
            "expected 0.0 (clamped), got {}",
            score
        );
    }

    #[tokio::test]
    async fn test_invalid_wasm_bytecode() {
        let runtime =
            WasmRuntime::new(WasmRuntimeConfig::minimal()).expect("runtime creation failed");
        let invalid_wasm = b"not valid wasm bytecode";

        let result = runtime.evaluate(invalid_wasm, b"input").await;
        assert!(result.is_err());

        match result.err().expect("expected error") {
            WasmError::ModuleCompilation(_) => {} // Expected
            other => panic!("expected ModuleCompilation error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_runtime_clone() {
        let runtime = WasmRuntime::with_defaults().expect("runtime creation failed");
        let cloned = runtime.clone();

        // Both should work independently
        let wasm_code = create_test_wasm_module();
        let input = b"test";

        let result1 = runtime.evaluate(&wasm_code, input).await;
        let result2 = cloned.evaluate(&wasm_code, input).await;

        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert!((result1.expect("r1") - result2.expect("r2")).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_empty_input() {
        let runtime =
            WasmRuntime::new(WasmRuntimeConfig::minimal()).expect("runtime creation failed");
        let wasm_code = create_test_wasm_module();
        let empty_input: &[u8] = &[];

        // Should handle empty input gracefully
        let result = runtime.evaluate(&wasm_code, empty_input).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_large_input() {
        let runtime = WasmRuntime::new(WasmRuntimeConfig::minimal().with_max_memory_mb(128))
            .expect("runtime creation failed");
        let wasm_code = create_test_wasm_module();
        // Create 1MB of input data
        let large_input = vec![0u8; 1024 * 1024];

        let result = runtime.evaluate(&wasm_code, &large_input).await;
        assert!(result.is_ok(), "large input should be handled");
    }

    #[test]
    fn test_wasm_error_display() {
        let error = WasmError::Timeout(60);
        assert!(error.to_string().contains("60 seconds"));

        let error = WasmError::MissingFunction("evaluate".to_string());
        assert!(error.to_string().contains("evaluate"));

        let error = WasmError::FuelExhausted;
        assert!(error.to_string().contains("Fuel exhausted"));
    }
}
