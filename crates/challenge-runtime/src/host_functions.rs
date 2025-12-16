//! Host Functions for WASM Challenges
//!
//! These are the APIs that WASM challenge code can call to interact with
//! the host runtime. The challenge runs in a sandboxed WASM environment
//! and communicates with the host via these functions.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      WASM Sandbox                                │
//! │  ┌─────────────────────────────────────────────────────────┐   │
//! │  │                   Challenge Code                         │   │
//! │  │  - Task definitions                                      │   │
//! │  │  - Scoring logic                                         │   │
//! │  │  - Verification rules                                    │   │
//! │  │  - Weight calculation                                    │   │
//! │  └──────────────────────┬──────────────────────────────────┘   │
//! │                         │ Host Function Calls                   │
//! └─────────────────────────┼───────────────────────────────────────┘
//!                           ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                    Host Runtime (Rust)                          │
//! │  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────────────┐   │
//! │  │  Docker  │ │ LLM Proxy│ │ Storage  │ │ Bittensor Client │   │
//! │  │  Runner  │ │          │ │          │ │                  │   │
//! │  └──────────┘ └──────────┘ └──────────┘ └──────────────────┘   │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Available Host Functions
//!
//! ## Execution
//! - `host_execute_agent` - Run agent in Docker container
//! - `host_execute_command` - Run a shell command
//! - `host_get_execution_result` - Get result of execution
//!
//! ## LLM Proxy
//! - `host_llm_call` - Make an LLM API call (tracked)
//! - `host_llm_get_usage` - Get current LLM usage/cost
//!
//! ## Storage
//! - `host_storage_get` - Read from challenge storage
//! - `host_storage_set` - Write to challenge storage
//! - `host_storage_delete` - Delete from storage
//!
//! ## Network
//! - `host_broadcast_result` - Broadcast result to P2P network
//! - `host_get_validator_results` - Get results from other validators
//!
//! ## Chain
//! - `host_get_epoch` - Get current epoch
//! - `host_get_block` - Get current block height
//! - `host_get_validators` - Get active validators
//! - `host_get_miner_stake` - Get miner's stake
//! - `host_submit_weights` - Submit weights to Bittensor

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Result of a host function call
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostResult<T> {
    pub success: bool,
    pub data: Option<T>,
    pub error: Option<String>,
}

impl<T> HostResult<T> {
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(error: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(error.into()),
        }
    }
}

// ==================== Execution Types ====================

/// Request to execute an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteAgentRequest {
    /// Agent hash/identifier
    pub agent_hash: String,
    /// Task ID to execute
    pub task_id: String,
    /// Task instruction/prompt
    pub instruction: String,
    /// Working directory contents (files to mount)
    pub files: HashMap<String, Vec<u8>>,
    /// Environment variables
    pub env_vars: HashMap<String, String>,
    /// Timeout in seconds
    pub timeout_secs: u64,
    /// Memory limit in MB
    pub memory_limit_mb: u64,
    /// CPU limit (cores)
    pub cpu_limit: f32,
    /// Network access allowed
    pub allow_network: bool,
    /// Allowed network hosts (if network enabled)
    pub allowed_hosts: Vec<String>,
}

/// Result of agent execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteAgentResult {
    /// Execution ID
    pub execution_id: String,
    /// Exit code
    pub exit_code: i32,
    /// Standard output
    pub stdout: String,
    /// Standard error
    pub stderr: String,
    /// Execution time in milliseconds
    pub execution_time_ms: u64,
    /// Memory used in MB
    pub memory_used_mb: u64,
    /// Files produced (path -> content)
    pub output_files: HashMap<String, Vec<u8>>,
    /// LLM calls made during execution
    pub llm_calls: Vec<LLMCallRecord>,
    /// Total cost in USD
    pub total_cost_usd: f64,
    /// Was killed due to timeout
    pub timed_out: bool,
    /// Was killed due to resource limit
    pub resource_limited: bool,
}

/// Record of an LLM API call
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMCallRecord {
    /// Timestamp (unix ms)
    pub timestamp: u64,
    /// Model used
    pub model: String,
    /// Provider (openai, anthropic, etc)
    pub provider: String,
    /// Input tokens
    pub input_tokens: u64,
    /// Output tokens
    pub output_tokens: u64,
    /// Cost in USD
    pub cost_usd: f64,
    /// Latency in ms
    pub latency_ms: u64,
    /// Was the call allowed (model in whitelist)
    pub allowed: bool,
    /// Was the call blocked (cost limit, etc)
    pub blocked: bool,
    /// Block reason if blocked
    pub block_reason: Option<String>,
}

// ==================== LLM Proxy Types ====================

/// LLM call request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMCallRequest {
    /// Provider (openai, anthropic)
    pub provider: String,
    /// Model name
    pub model: String,
    /// Messages for chat completion
    pub messages: Vec<LLMMessage>,
    /// Max tokens to generate
    pub max_tokens: Option<u64>,
    /// Temperature
    pub temperature: Option<f64>,
    /// Stop sequences
    pub stop: Option<Vec<String>>,
}

/// LLM message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMMessage {
    pub role: String,
    pub content: String,
}

/// LLM call response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMCallResponse {
    /// Response content
    pub content: String,
    /// Model used
    pub model: String,
    /// Input tokens
    pub input_tokens: u64,
    /// Output tokens
    pub output_tokens: u64,
    /// Cost in USD
    pub cost_usd: f64,
    /// Finish reason
    pub finish_reason: String,
}

/// Current LLM usage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMUsage {
    /// Total input tokens
    pub total_input_tokens: u64,
    /// Total output tokens
    pub total_output_tokens: u64,
    /// Total cost in USD
    pub total_cost_usd: f64,
    /// Number of calls
    pub call_count: u64,
    /// Cost limit
    pub cost_limit_usd: f64,
    /// Remaining budget
    pub remaining_usd: f64,
}

// ==================== Storage Types ====================

/// Storage entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageEntry {
    pub key: String,
    pub value: Vec<u8>,
    pub created_at: u64,
    pub updated_at: u64,
}

// ==================== Network Types ====================

/// Broadcast result request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BroadcastResultRequest {
    /// Agent hash
    pub agent_hash: String,
    /// Evaluation result (serialized)
    pub result: Vec<u8>,
    /// Signature
    pub signature: Vec<u8>,
}

/// Validator result from network
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorResult {
    /// Validator hotkey
    pub validator: String,
    /// Agent hash
    pub agent_hash: String,
    /// Score
    pub score: f64,
    /// Result hash
    pub result_hash: String,
    /// Epoch
    pub epoch: u64,
    /// Signature
    pub signature: Vec<u8>,
}

// ==================== Chain Types ====================

/// Validator info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorInfo {
    pub hotkey: String,
    pub coldkey: String,
    pub stake: u64,
    pub is_active: bool,
}

/// Miner info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinerInfo {
    pub hotkey: String,
    pub coldkey: String,
    pub stake: u64,
    pub is_registered: bool,
    pub uid: Option<u16>,
}

/// Weight submission
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightSubmission {
    /// UID -> weight pairs
    pub weights: Vec<(u16, u16)>,
    /// Mechanism ID
    pub mechanism_id: u8,
}

// ==================== Task Verification Types ====================

/// Task verification request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyTaskRequest {
    /// Task ID
    pub task_id: String,
    /// Test script content
    pub test_script: String,
    /// Working directory with agent output
    pub working_dir: HashMap<String, Vec<u8>>,
    /// Timeout for verification
    pub timeout_secs: u64,
}

/// Task verification result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyTaskResult {
    /// Did the task pass
    pub passed: bool,
    /// Score (0.0 - 1.0)
    pub score: f64,
    /// Test output
    pub output: String,
    /// Number of tests passed
    pub tests_passed: u32,
    /// Number of tests failed
    pub tests_failed: u32,
    /// Detailed test results
    pub test_details: Vec<TestResult>,
}

/// Individual test result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub message: Option<String>,
    pub expected: Option<String>,
    pub actual: Option<String>,
}

// ==================== Host Function Trait ====================

/// Trait that host implementations must provide
/// This is implemented by the validator runtime
#[allow(async_fn_in_trait)]
pub trait HostFunctions: Send + Sync {
    // Execution
    async fn execute_agent(&self, request: ExecuteAgentRequest) -> HostResult<ExecuteAgentResult>;
    async fn verify_task(&self, request: VerifyTaskRequest) -> HostResult<VerifyTaskResult>;

    // LLM
    async fn llm_call(&self, request: LLMCallRequest) -> HostResult<LLMCallResponse>;
    fn llm_get_usage(&self) -> HostResult<LLMUsage>;
    fn llm_set_limit(&self, limit_usd: f64);

    // Storage
    fn storage_get(&self, key: &str) -> HostResult<Option<StorageEntry>>;
    fn storage_set(&self, key: &str, value: &[u8]) -> HostResult<()>;
    fn storage_delete(&self, key: &str) -> HostResult<()>;
    fn storage_list(&self, prefix: &str) -> HostResult<Vec<String>>;

    // Network
    async fn broadcast_result(&self, request: BroadcastResultRequest) -> HostResult<()>;
    fn get_validator_results(&self, agent_hash: &str) -> HostResult<Vec<ValidatorResult>>;

    // Chain
    fn get_epoch(&self) -> u64;
    fn get_block(&self) -> u64;
    fn get_validators(&self) -> HostResult<Vec<ValidatorInfo>>;
    fn get_miner_info(&self, hotkey: &str) -> HostResult<Option<MinerInfo>>;
    fn is_miner_registered(&self, hotkey: &str) -> bool;
    fn get_miner_stake(&self, hotkey: &str) -> u64;
    async fn submit_weights(&self, submission: WeightSubmission) -> HostResult<()>;

    // Logging
    fn log(&self, level: &str, message: &str);
}

/// Log levels for host logging
pub mod log_level {
    pub const TRACE: &str = "trace";
    pub const DEBUG: &str = "debug";
    pub const INFO: &str = "info";
    pub const WARN: &str = "warn";
    pub const ERROR: &str = "error";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_result() {
        let ok: HostResult<i32> = HostResult::ok(42);
        assert!(ok.success);
        assert_eq!(ok.data, Some(42));

        let err: HostResult<i32> = HostResult::err("failed");
        assert!(!err.success);
        assert_eq!(err.error, Some("failed".to_string()));
    }
}
