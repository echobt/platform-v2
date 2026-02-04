//! WASM Challenge Interface and Adapter
//!
//! This module provides a standard interface for WASM-based challenges.
//! Challenge authors implement the `WasmChallenge` trait, and this module
//! handles serialization/deserialization between Rust and WASM memory.
//!
//! # WASM ABI
//!
//! WASM challenges must export these functions:
//! - `evaluate(ptr: i32, len: i32) -> i64` - Evaluate a submission, returns fixed-point score
//! - `validate(ptr: i32, len: i32) -> i32` - Validate submission format, returns 0/1
//! - `alloc(size: i32) -> i32` - Allocate memory for input data
//! - `dealloc(ptr: i32, len: i32)` - Deallocate memory
//!
//! # Score Format
//!
//! Scores are returned as fixed-point integers:
//! - Range: 0 to 1,000,000
//! - Maps to: 0.0 to 1.0
//! - Example: 750,000 = 0.75
//!
//! # Example
//!
//! ```rust,ignore
//! use platform_challenge_sdk::wasm_challenge::{
//!     WasmChallenge, WasmEvaluationInput, WasmEvaluationOutput, WasmValidationResult
//! };
//!
//! struct MyChallenge;
//!
//! impl WasmChallenge for MyChallenge {
//!     fn name() -> &'static str { "my-challenge" }
//!     fn version() -> &'static str { "1.0.0" }
//!
//!     fn evaluate(input: WasmEvaluationInput) -> WasmEvaluationOutput {
//!         let score = compute_score(&input.data);
//!         WasmEvaluationOutput::success(score)
//!     }
//!
//!     fn validate(input: WasmEvaluationInput) -> WasmValidationResult {
//!         if is_valid_format(&input.data) {
//!             WasmValidationResult::valid()
//!         } else {
//!             WasmValidationResult::invalid("Invalid submission format")
//!         }
//!     }
//! }
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ============================================================================
// CONSTANTS
// ============================================================================

/// Scale factor for fixed-point score representation.
/// Score range: 0 to SCORE_SCALE maps to 0.0 to 1.0
pub const SCORE_SCALE: i64 = 1_000_000;

/// Validation success return code
pub const VALIDATION_SUCCESS: i32 = 1;

/// Validation failure return code
pub const VALIDATION_FAILURE: i32 = 0;

/// Error return code for evaluation (negative indicates error)
pub const EVALUATION_ERROR: i64 = -1;

// ============================================================================
// SCORE CONVERSION
// ============================================================================

/// Convert a floating-point score (0.0-1.0) to fixed-point integer (0-1,000,000).
///
/// # Arguments
///
/// * `score` - Score value between 0.0 and 1.0
///
/// # Returns
///
/// Fixed-point integer representation, clamped to valid range
///
/// # Examples
///
/// ```
/// use platform_challenge_sdk::wasm_challenge::score_to_fixed;
///
/// assert_eq!(score_to_fixed(0.0), 0);
/// assert_eq!(score_to_fixed(0.5), 500_000);
/// assert_eq!(score_to_fixed(1.0), 1_000_000);
/// assert_eq!(score_to_fixed(1.5), 1_000_000); // Clamped
/// assert_eq!(score_to_fixed(-0.5), 0);         // Clamped
/// ```
#[inline]
pub fn score_to_fixed(score: f64) -> i64 {
    (score.clamp(0.0, 1.0) * SCORE_SCALE as f64) as i64
}

/// Convert a fixed-point integer (0-1,000,000) to floating-point score (0.0-1.0).
///
/// # Arguments
///
/// * `fixed` - Fixed-point integer score
///
/// # Returns
///
/// Floating-point score, clamped to 0.0-1.0 range
///
/// # Examples
///
/// ```
/// use platform_challenge_sdk::wasm_challenge::fixed_to_score;
///
/// assert_eq!(fixed_to_score(0), 0.0);
/// assert_eq!(fixed_to_score(500_000), 0.5);
/// assert_eq!(fixed_to_score(1_000_000), 1.0);
/// assert_eq!(fixed_to_score(2_000_000), 1.0); // Clamped
/// assert_eq!(fixed_to_score(-500_000), 0.0);  // Clamped
/// ```
#[inline]
pub fn fixed_to_score(fixed: i64) -> f64 {
    (fixed as f64 / SCORE_SCALE as f64).clamp(0.0, 1.0)
}

// ============================================================================
// ERROR TYPES
// ============================================================================

/// Errors that can occur during WASM serialization/deserialization.
#[derive(Debug, Error)]
pub enum WasmSerializeError {
    /// Serialization of input data failed
    #[error("Serialization failed: {0}")]
    SerializationFailed(String),

    /// Deserialization of output data failed
    #[error("Deserialization failed: {0}")]
    DeserializationFailed(String),

    /// Data format is invalid or corrupted
    #[error("Invalid data format: {0}")]
    InvalidFormat(String),

    /// Memory operation failed
    #[error("Memory error: {0}")]
    MemoryError(String),
}

impl From<bincode::Error> for WasmSerializeError {
    fn from(err: bincode::Error) -> Self {
        WasmSerializeError::SerializationFailed(err.to_string())
    }
}

impl From<serde_json::Error> for WasmSerializeError {
    fn from(err: serde_json::Error) -> Self {
        WasmSerializeError::SerializationFailed(err.to_string())
    }
}

// ============================================================================
// INPUT/OUTPUT TYPES
// ============================================================================

/// Input data for WASM evaluation.
///
/// This structure is serialized and passed to the WASM module's `evaluate` and
/// `validate` functions. The `data` field contains the raw submission bytes,
/// while `metadata` provides additional context.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WasmEvaluationInput {
    /// Unique identifier for this submission
    pub submission_id: String,

    /// Identifier for the participant (miner/agent)
    pub participant_id: String,

    /// Raw submission data bytes
    pub data: Vec<u8>,

    /// Additional metadata as JSON (task params, config, etc.)
    pub metadata: serde_json::Value,
}

impl WasmEvaluationInput {
    /// Create a new evaluation input.
    ///
    /// # Arguments
    ///
    /// * `submission_id` - Unique submission identifier
    /// * `participant_id` - Participant identifier
    /// * `data` - Raw submission data
    pub fn new(submission_id: impl Into<String>, participant_id: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            submission_id: submission_id.into(),
            participant_id: participant_id.into(),
            data,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// Create input with metadata.
    ///
    /// # Arguments
    ///
    /// * `submission_id` - Unique submission identifier
    /// * `participant_id` - Participant identifier
    /// * `data` - Raw submission data
    /// * `metadata` - Additional metadata as JSON
    pub fn with_metadata(
        submission_id: impl Into<String>,
        participant_id: impl Into<String>,
        data: Vec<u8>,
        metadata: serde_json::Value,
    ) -> Self {
        Self {
            submission_id: submission_id.into(),
            participant_id: participant_id.into(),
            data,
            metadata,
        }
    }
}

/// Output data from WASM evaluation.
///
/// Contains the evaluation results including score, validity, detailed results,
/// and any error information.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WasmEvaluationOutput {
    /// Evaluation score (0.0 to 1.0)
    pub score: f64,

    /// Whether the submission is valid
    pub is_valid: bool,

    /// Detailed evaluation results as JSON
    pub results: serde_json::Value,

    /// Error message if evaluation failed
    pub error: Option<String>,
}

impl WasmEvaluationOutput {
    /// Create a successful evaluation output.
    ///
    /// # Arguments
    ///
    /// * `score` - Evaluation score (will be clamped to 0.0-1.0)
    pub fn success(score: f64) -> Self {
        Self {
            score: score.clamp(0.0, 1.0),
            is_valid: true,
            results: serde_json::Value::Object(serde_json::Map::new()),
            error: None,
        }
    }

    /// Create a successful evaluation output with detailed results.
    ///
    /// # Arguments
    ///
    /// * `score` - Evaluation score (will be clamped to 0.0-1.0)
    /// * `results` - Detailed results as JSON
    pub fn success_with_results(score: f64, results: serde_json::Value) -> Self {
        Self {
            score: score.clamp(0.0, 1.0),
            is_valid: true,
            results,
            error: None,
        }
    }

    /// Create a failed evaluation output.
    ///
    /// # Arguments
    ///
    /// * `error` - Error message describing the failure
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            score: 0.0,
            is_valid: false,
            results: serde_json::Value::Object(serde_json::Map::new()),
            error: Some(error.into()),
        }
    }

    /// Create an invalid submission output (validation failed).
    ///
    /// # Arguments
    ///
    /// * `error` - Error message describing why the submission is invalid
    pub fn invalid(error: impl Into<String>) -> Self {
        Self {
            score: 0.0,
            is_valid: false,
            results: serde_json::Value::Object(serde_json::Map::new()),
            error: Some(error.into()),
        }
    }

    /// Convert score to fixed-point representation for WASM return value.
    #[inline]
    pub fn score_as_fixed(&self) -> i64 {
        if self.is_valid {
            score_to_fixed(self.score)
        } else {
            EVALUATION_ERROR
        }
    }
}

impl Default for WasmEvaluationOutput {
    fn default() -> Self {
        Self::success(0.0)
    }
}

/// Validation result from WASM.
///
/// Used by the `validate` function to indicate whether a submission
/// has a valid format before full evaluation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WasmValidationResult {
    /// Whether the submission format is valid
    pub is_valid: bool,

    /// Error message if validation failed
    pub error: Option<String>,
}

impl WasmValidationResult {
    /// Create a successful validation result.
    pub fn valid() -> Self {
        Self {
            is_valid: true,
            error: None,
        }
    }

    /// Create a failed validation result.
    ///
    /// # Arguments
    ///
    /// * `error` - Error message describing the validation failure
    pub fn invalid(error: impl Into<String>) -> Self {
        Self {
            is_valid: false,
            error: Some(error.into()),
        }
    }

    /// Convert to WASM return code.
    #[inline]
    pub fn as_return_code(&self) -> i32 {
        if self.is_valid {
            VALIDATION_SUCCESS
        } else {
            VALIDATION_FAILURE
        }
    }
}

impl Default for WasmValidationResult {
    fn default() -> Self {
        Self::valid()
    }
}

// ============================================================================
// HOST FUNCTIONS TRAIT
// ============================================================================

/// Host functions that WASM challenges can call.
///
/// These functions are provided by the runtime and allow challenges
/// to access controlled external functionality.
pub trait WasmHostFunctions {
    /// Log a message at the specified level.
    ///
    /// # Arguments
    ///
    /// * `level` - Log level (0=trace, 1=debug, 2=info, 3=warn, 4=error)
    /// * `message` - Message to log
    fn log(&self, level: u32, message: &str);

    /// Get a random u64 value.
    ///
    /// Note: For deterministic evaluation, this should return
    /// the same sequence of values across all validators.
    fn get_random(&self) -> u64;

    /// Get the current timestamp in milliseconds since Unix epoch.
    fn get_timestamp(&self) -> i64;
}

/// Log levels for host logging function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl From<u32> for LogLevel {
    fn from(value: u32) -> Self {
        match value {
            0 => LogLevel::Trace,
            1 => LogLevel::Debug,
            2 => LogLevel::Info,
            3 => LogLevel::Warn,
            _ => LogLevel::Error,
        }
    }
}

// ============================================================================
// WASM CHALLENGE TRAIT
// ============================================================================

/// Challenge interface that WASM modules must implement.
///
/// This trait defines the core functionality that every WASM challenge
/// must provide. Implementing this trait and using the provided macros
/// will generate the appropriate WASM exports.
///
/// # Example
///
/// ```rust,ignore
/// use platform_challenge_sdk::wasm_challenge::*;
///
/// struct MathChallenge;
///
/// impl WasmChallenge for MathChallenge {
///     fn name() -> &'static str { "math-challenge" }
///     fn version() -> &'static str { "1.0.0" }
///
///     fn evaluate(input: WasmEvaluationInput) -> WasmEvaluationOutput {
///         // Parse expected answer from metadata
///         let expected = input.metadata["expected"].as_i64().unwrap_or(0);
///         
///         // Parse submitted answer from data
///         let submitted: i64 = match String::from_utf8_lossy(&input.data).parse() {
///             Ok(n) => n,
///             Err(_) => return WasmEvaluationOutput::invalid("Invalid number format"),
///         };
///         
///         // Score based on correctness
///         let score = if submitted == expected { 1.0 } else { 0.0 };
///         WasmEvaluationOutput::success(score)
///     }
///
///     fn validate(input: WasmEvaluationInput) -> WasmValidationResult {
///         // Check that data is valid UTF-8 and a number
///         match String::from_utf8(input.data.clone()) {
///             Ok(s) if s.parse::<i64>().is_ok() => WasmValidationResult::valid(),
///             _ => WasmValidationResult::invalid("Submission must be a valid integer"),
///         }
///     }
/// }
/// ```
pub trait WasmChallenge {
    /// Get the challenge name.
    fn name() -> &'static str;

    /// Get the challenge version.
    fn version() -> &'static str;

    /// Evaluate a submission and return score with results.
    ///
    /// # Arguments
    ///
    /// * `input` - Evaluation input containing submission data
    ///
    /// # Returns
    ///
    /// Evaluation output with score, validity, and detailed results
    fn evaluate(input: WasmEvaluationInput) -> WasmEvaluationOutput;

    /// Validate a submission format before full evaluation.
    ///
    /// This is a lightweight check to reject obviously invalid submissions
    /// before spending resources on full evaluation.
    ///
    /// # Arguments
    ///
    /// * `input` - Evaluation input to validate
    ///
    /// # Returns
    ///
    /// Validation result indicating if the submission format is valid
    fn validate(input: WasmEvaluationInput) -> WasmValidationResult;
}

// ============================================================================
// SERIALIZATION HELPERS
// ============================================================================

/// Serialization format for WASM data exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SerializationFormat {
    /// JSON format (human-readable, larger size)
    #[default]
    Json,
    /// Bincode format (binary, smaller size, faster)
    Bincode,
}

// Internal types for bincode serialization (avoids serde_json::Value issues)
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BincodeEvaluationInput {
    submission_id: String,
    participant_id: String,
    data: Vec<u8>,
    metadata_json: String, // Stored as JSON string for bincode compatibility
}

impl From<&WasmEvaluationInput> for BincodeEvaluationInput {
    fn from(input: &WasmEvaluationInput) -> Self {
        Self {
            submission_id: input.submission_id.clone(),
            participant_id: input.participant_id.clone(),
            data: input.data.clone(),
            metadata_json: serde_json::to_string(&input.metadata).unwrap_or_else(|_| "{}".to_string()),
        }
    }
}

impl TryFrom<BincodeEvaluationInput> for WasmEvaluationInput {
    type Error = WasmSerializeError;

    fn try_from(input: BincodeEvaluationInput) -> Result<Self, Self::Error> {
        let metadata = serde_json::from_str(&input.metadata_json)
            .map_err(|e| WasmSerializeError::DeserializationFailed(e.to_string()))?;
        Ok(Self {
            submission_id: input.submission_id,
            participant_id: input.participant_id,
            data: input.data,
            metadata,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BincodeEvaluationOutput {
    score: f64,
    is_valid: bool,
    results_json: String,
    error: Option<String>,
}

impl From<&WasmEvaluationOutput> for BincodeEvaluationOutput {
    fn from(output: &WasmEvaluationOutput) -> Self {
        Self {
            score: output.score,
            is_valid: output.is_valid,
            results_json: serde_json::to_string(&output.results).unwrap_or_else(|_| "{}".to_string()),
            error: output.error.clone(),
        }
    }
}

impl TryFrom<BincodeEvaluationOutput> for WasmEvaluationOutput {
    type Error = WasmSerializeError;

    fn try_from(output: BincodeEvaluationOutput) -> Result<Self, Self::Error> {
        let results = serde_json::from_str(&output.results_json)
            .map_err(|e| WasmSerializeError::DeserializationFailed(e.to_string()))?;
        Ok(Self {
            score: output.score,
            is_valid: output.is_valid,
            results,
            error: output.error,
        })
    }
}

/// Serialize evaluation input to bytes.
///
/// # Arguments
///
/// * `input` - The input to serialize
/// * `format` - Serialization format to use
///
/// # Returns
///
/// Serialized bytes or error
pub fn serialize_input(
    input: &WasmEvaluationInput,
    format: SerializationFormat,
) -> Result<Vec<u8>, WasmSerializeError> {
    match format {
        SerializationFormat::Json => {
            serde_json::to_vec(input).map_err(|e| WasmSerializeError::SerializationFailed(e.to_string()))
        }
        SerializationFormat::Bincode => {
            let bincode_input = BincodeEvaluationInput::from(input);
            bincode::serialize(&bincode_input).map_err(|e| WasmSerializeError::SerializationFailed(e.to_string()))
        }
    }
}

/// Deserialize evaluation input from bytes.
///
/// # Arguments
///
/// * `data` - Serialized input bytes
/// * `format` - Serialization format used
///
/// # Returns
///
/// Deserialized input or error
pub fn deserialize_input(
    data: &[u8],
    format: SerializationFormat,
) -> Result<WasmEvaluationInput, WasmSerializeError> {
    match format {
        SerializationFormat::Json => {
            serde_json::from_slice(data).map_err(|e| WasmSerializeError::DeserializationFailed(e.to_string()))
        }
        SerializationFormat::Bincode => {
            let bincode_input: BincodeEvaluationInput = bincode::deserialize(data)
                .map_err(|e| WasmSerializeError::DeserializationFailed(e.to_string()))?;
            bincode_input.try_into()
        }
    }
}

/// Serialize evaluation output to bytes.
///
/// # Arguments
///
/// * `output` - The output to serialize
/// * `format` - Serialization format to use
///
/// # Returns
///
/// Serialized bytes or error
pub fn serialize_output(
    output: &WasmEvaluationOutput,
    format: SerializationFormat,
) -> Result<Vec<u8>, WasmSerializeError> {
    match format {
        SerializationFormat::Json => {
            serde_json::to_vec(output).map_err(|e| WasmSerializeError::SerializationFailed(e.to_string()))
        }
        SerializationFormat::Bincode => {
            let bincode_output = BincodeEvaluationOutput::from(output);
            bincode::serialize(&bincode_output).map_err(|e| WasmSerializeError::SerializationFailed(e.to_string()))
        }
    }
}

/// Deserialize evaluation output from bytes.
///
/// # Arguments
///
/// * `data` - Serialized output bytes
/// * `format` - Serialization format used
///
/// # Returns
///
/// Deserialized output or error
pub fn deserialize_output(
    data: &[u8],
    format: SerializationFormat,
) -> Result<WasmEvaluationOutput, WasmSerializeError> {
    match format {
        SerializationFormat::Json => {
            serde_json::from_slice(data).map_err(|e| WasmSerializeError::DeserializationFailed(e.to_string()))
        }
        SerializationFormat::Bincode => {
            let bincode_output: BincodeEvaluationOutput = bincode::deserialize(data)
                .map_err(|e| WasmSerializeError::DeserializationFailed(e.to_string()))?;
            bincode_output.try_into()
        }
    }
}

/// Serialize validation result to bytes.
///
/// # Arguments
///
/// * `result` - The result to serialize
/// * `format` - Serialization format to use
///
/// # Returns
///
/// Serialized bytes or error
pub fn serialize_validation_result(
    result: &WasmValidationResult,
    format: SerializationFormat,
) -> Result<Vec<u8>, WasmSerializeError> {
    match format {
        SerializationFormat::Json => {
            serde_json::to_vec(result).map_err(|e| WasmSerializeError::SerializationFailed(e.to_string()))
        }
        SerializationFormat::Bincode => {
            bincode::serialize(result).map_err(|e| WasmSerializeError::SerializationFailed(e.to_string()))
        }
    }
}

/// Deserialize validation result from bytes.
///
/// # Arguments
///
/// * `data` - Serialized result bytes
/// * `format` - Serialization format used
///
/// # Returns
///
/// Deserialized result or error
pub fn deserialize_validation_result(
    data: &[u8],
    format: SerializationFormat,
) -> Result<WasmValidationResult, WasmSerializeError> {
    match format {
        SerializationFormat::Json => {
            serde_json::from_slice(data).map_err(|e| WasmSerializeError::DeserializationFailed(e.to_string()))
        }
        SerializationFormat::Bincode => {
            bincode::deserialize(data).map_err(|e| WasmSerializeError::DeserializationFailed(e.to_string()))
        }
    }
}

// ============================================================================
// WASM MEMORY HELPERS
// ============================================================================

/// Result of encoding data for WASM memory.
#[derive(Clone, Debug)]
pub struct WasmMemoryBuffer {
    /// Serialized data bytes
    pub data: Vec<u8>,
    /// Length of the data
    pub len: usize,
}

impl WasmMemoryBuffer {
    /// Create a new memory buffer from serialized data.
    pub fn new(data: Vec<u8>) -> Self {
        let len = data.len();
        Self { data, len }
    }

    /// Get the length as i32 for WASM.
    #[inline]
    pub fn len_i32(&self) -> i32 {
        self.len as i32
    }

    /// Check if the buffer is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Encode evaluation input for passing to WASM memory.
///
/// # Arguments
///
/// * `input` - Input to encode
/// * `format` - Serialization format
///
/// # Returns
///
/// Memory buffer ready for WASM or error
pub fn encode_for_wasm(
    input: &WasmEvaluationInput,
    format: SerializationFormat,
) -> Result<WasmMemoryBuffer, WasmSerializeError> {
    let data = serialize_input(input, format)?;
    Ok(WasmMemoryBuffer::new(data))
}

/// Decode evaluation output from WASM memory.
///
/// # Arguments
///
/// * `data` - Raw bytes from WASM memory
/// * `format` - Serialization format used
///
/// # Returns
///
/// Decoded output or error
pub fn decode_from_wasm(
    data: &[u8],
    format: SerializationFormat,
) -> Result<WasmEvaluationOutput, WasmSerializeError> {
    deserialize_output(data, format)
}

// ============================================================================
// CHALLENGE METADATA
// ============================================================================

/// Metadata about a WASM challenge module.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WasmChallengeMetadata {
    /// Challenge name
    pub name: String,
    /// Challenge version
    pub version: String,
    /// Description of the challenge
    pub description: Option<String>,
    /// Author information
    pub author: Option<String>,
    /// Serialization format used
    pub serialization_format: String,
    /// Required memory pages (64KB each)
    pub memory_pages: u32,
    /// Maximum evaluation time in milliseconds
    pub max_eval_time_ms: u64,
}

impl WasmChallengeMetadata {
    /// Create new challenge metadata with required fields.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            description: None,
            author: None,
            serialization_format: "json".to_string(),
            memory_pages: 16, // 1MB default
            max_eval_time_ms: 30_000, // 30 seconds default
        }
    }

    /// Set the description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the author.
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    /// Set the serialization format.
    pub fn with_serialization_format(mut self, format: SerializationFormat) -> Self {
        self.serialization_format = match format {
            SerializationFormat::Json => "json".to_string(),
            SerializationFormat::Bincode => "bincode".to_string(),
        };
        self
    }

    /// Set the required memory pages.
    pub fn with_memory_pages(mut self, pages: u32) -> Self {
        self.memory_pages = pages;
        self
    }

    /// Set the maximum evaluation time.
    pub fn with_max_eval_time(mut self, ms: u64) -> Self {
        self.max_eval_time_ms = ms;
        self
    }

    /// Get the serialization format enum.
    pub fn get_format(&self) -> SerializationFormat {
        match self.serialization_format.as_str() {
            "bincode" => SerializationFormat::Bincode,
            _ => SerializationFormat::Json,
        }
    }
}

impl Default for WasmChallengeMetadata {
    fn default() -> Self {
        Self::new("unknown", "0.0.0")
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Score conversion tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_score_to_fixed_normal_values() {
        assert_eq!(score_to_fixed(0.0), 0);
        assert_eq!(score_to_fixed(0.5), 500_000);
        assert_eq!(score_to_fixed(1.0), 1_000_000);
        assert_eq!(score_to_fixed(0.75), 750_000);
        assert_eq!(score_to_fixed(0.123456), 123_456);
    }

    #[test]
    fn test_score_to_fixed_clamping() {
        assert_eq!(score_to_fixed(-1.0), 0);
        assert_eq!(score_to_fixed(-0.5), 0);
        assert_eq!(score_to_fixed(1.5), 1_000_000);
        assert_eq!(score_to_fixed(100.0), 1_000_000);
    }

    #[test]
    fn test_fixed_to_score_normal_values() {
        assert!((fixed_to_score(0) - 0.0).abs() < f64::EPSILON);
        assert!((fixed_to_score(500_000) - 0.5).abs() < f64::EPSILON);
        assert!((fixed_to_score(1_000_000) - 1.0).abs() < f64::EPSILON);
        assert!((fixed_to_score(750_000) - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_fixed_to_score_clamping() {
        assert!((fixed_to_score(-500_000) - 0.0).abs() < f64::EPSILON);
        assert!((fixed_to_score(2_000_000) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_roundtrip() {
        let original = 0.654321;
        let fixed = score_to_fixed(original);
        let recovered = fixed_to_score(fixed);
        // Should be within 1/SCORE_SCALE precision
        assert!((original - recovered).abs() < 1.0 / SCORE_SCALE as f64);
    }

    // -------------------------------------------------------------------------
    // WasmEvaluationInput tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_wasm_evaluation_input_new() {
        let input = WasmEvaluationInput::new("sub-1", "part-1", vec![1, 2, 3]);
        assert_eq!(input.submission_id, "sub-1");
        assert_eq!(input.participant_id, "part-1");
        assert_eq!(input.data, vec![1, 2, 3]);
        assert!(input.metadata.is_object());
    }

    #[test]
    fn test_wasm_evaluation_input_with_metadata() {
        let metadata = serde_json::json!({"task": "test", "difficulty": 5});
        let input = WasmEvaluationInput::with_metadata("sub-2", "part-2", vec![4, 5, 6], metadata.clone());
        assert_eq!(input.submission_id, "sub-2");
        assert_eq!(input.metadata, metadata);
    }

    // -------------------------------------------------------------------------
    // WasmEvaluationOutput tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_wasm_evaluation_output_success() {
        let output = WasmEvaluationOutput::success(0.85);
        assert!((output.score - 0.85).abs() < f64::EPSILON);
        assert!(output.is_valid);
        assert!(output.error.is_none());
    }

    #[test]
    fn test_wasm_evaluation_output_success_clamping() {
        let output = WasmEvaluationOutput::success(1.5);
        assert!((output.score - 1.0).abs() < f64::EPSILON);

        let output = WasmEvaluationOutput::success(-0.5);
        assert!((output.score - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_wasm_evaluation_output_success_with_results() {
        let results = serde_json::json!({"passed": 5, "failed": 1});
        let output = WasmEvaluationOutput::success_with_results(0.9, results.clone());
        assert!((output.score - 0.9).abs() < f64::EPSILON);
        assert_eq!(output.results, results);
    }

    #[test]
    fn test_wasm_evaluation_output_failure() {
        let output = WasmEvaluationOutput::failure("evaluation crashed");
        assert!((output.score - 0.0).abs() < f64::EPSILON);
        assert!(!output.is_valid);
        assert_eq!(output.error, Some("evaluation crashed".to_string()));
    }

    #[test]
    fn test_wasm_evaluation_output_invalid() {
        let output = WasmEvaluationOutput::invalid("bad format");
        assert!(!output.is_valid);
        assert_eq!(output.error, Some("bad format".to_string()));
    }

    #[test]
    fn test_wasm_evaluation_output_score_as_fixed() {
        let success = WasmEvaluationOutput::success(0.75);
        assert_eq!(success.score_as_fixed(), 750_000);

        let failure = WasmEvaluationOutput::failure("error");
        assert_eq!(failure.score_as_fixed(), EVALUATION_ERROR);
    }

    #[test]
    fn test_wasm_evaluation_output_default() {
        let output = WasmEvaluationOutput::default();
        assert!((output.score - 0.0).abs() < f64::EPSILON);
        assert!(output.is_valid);
    }

    // -------------------------------------------------------------------------
    // WasmValidationResult tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_wasm_validation_result_valid() {
        let result = WasmValidationResult::valid();
        assert!(result.is_valid);
        assert!(result.error.is_none());
        assert_eq!(result.as_return_code(), VALIDATION_SUCCESS);
    }

    #[test]
    fn test_wasm_validation_result_invalid() {
        let result = WasmValidationResult::invalid("missing field");
        assert!(!result.is_valid);
        assert_eq!(result.error, Some("missing field".to_string()));
        assert_eq!(result.as_return_code(), VALIDATION_FAILURE);
    }

    #[test]
    fn test_wasm_validation_result_default() {
        let result = WasmValidationResult::default();
        assert!(result.is_valid);
    }

    // -------------------------------------------------------------------------
    // Serialization tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_serialize_deserialize_input_json() {
        let input = WasmEvaluationInput::new("sub", "part", vec![1, 2, 3]);
        let serialized = serialize_input(&input, SerializationFormat::Json).expect("serialize failed");
        let deserialized = deserialize_input(&serialized, SerializationFormat::Json).expect("deserialize failed");
        assert_eq!(input, deserialized);
    }

    #[test]
    fn test_serialize_deserialize_input_bincode() {
        let input = WasmEvaluationInput::with_metadata(
            "sub",
            "part",
            vec![1, 2, 3],
            serde_json::json!({"key": "value"}),
        );
        let serialized = serialize_input(&input, SerializationFormat::Bincode).expect("serialize failed");
        let deserialized = deserialize_input(&serialized, SerializationFormat::Bincode).expect("deserialize failed");
        assert_eq!(input, deserialized);
    }

    #[test]
    fn test_serialize_deserialize_output_json() {
        let output = WasmEvaluationOutput::success_with_results(0.85, serde_json::json!({"test": true}));
        let serialized = serialize_output(&output, SerializationFormat::Json).expect("serialize failed");
        let deserialized = deserialize_output(&serialized, SerializationFormat::Json).expect("deserialize failed");
        assert_eq!(output, deserialized);
    }

    #[test]
    fn test_serialize_deserialize_output_bincode() {
        let output = WasmEvaluationOutput::failure("test error");
        let serialized = serialize_output(&output, SerializationFormat::Bincode).expect("serialize failed");
        let deserialized = deserialize_output(&serialized, SerializationFormat::Bincode).expect("deserialize failed");
        assert_eq!(output, deserialized);
    }

    #[test]
    fn test_serialize_deserialize_validation_result() {
        let result = WasmValidationResult::invalid("test");
        let serialized = serialize_validation_result(&result, SerializationFormat::Json).expect("serialize failed");
        let deserialized =
            deserialize_validation_result(&serialized, SerializationFormat::Json).expect("deserialize failed");
        assert_eq!(result, deserialized);
    }

    #[test]
    fn test_bincode_is_smaller_than_json() {
        let input = WasmEvaluationInput::with_metadata(
            "submission-12345",
            "participant-67890",
            vec![0; 100],
            serde_json::json!({"key": "value", "nested": {"a": 1, "b": 2}}),
        );
        let json_size = serialize_input(&input, SerializationFormat::Json).unwrap().len();
        let bincode_size = serialize_input(&input, SerializationFormat::Bincode).unwrap().len();
        assert!(bincode_size < json_size, "bincode should be smaller than json");
    }

    #[test]
    fn test_deserialize_invalid_data() {
        let invalid_data = b"not valid json or bincode";
        let json_result = deserialize_input(invalid_data, SerializationFormat::Json);
        assert!(json_result.is_err());
        
        let bincode_result = deserialize_input(invalid_data, SerializationFormat::Bincode);
        assert!(bincode_result.is_err());
    }

    // -------------------------------------------------------------------------
    // WasmMemoryBuffer tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_wasm_memory_buffer() {
        let buffer = WasmMemoryBuffer::new(vec![1, 2, 3, 4, 5]);
        assert_eq!(buffer.len, 5);
        assert_eq!(buffer.len_i32(), 5);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_wasm_memory_buffer_empty() {
        let buffer = WasmMemoryBuffer::new(vec![]);
        assert!(buffer.is_empty());
        assert_eq!(buffer.len_i32(), 0);
    }

    #[test]
    fn test_encode_for_wasm() {
        let input = WasmEvaluationInput::new("s", "p", vec![1, 2, 3]);
        let buffer = encode_for_wasm(&input, SerializationFormat::Json).expect("encode failed");
        assert!(!buffer.is_empty());
        assert!(buffer.len > 0);
    }

    #[test]
    fn test_decode_from_wasm() {
        let output = WasmEvaluationOutput::success(0.5);
        let serialized = serialize_output(&output, SerializationFormat::Json).unwrap();
        let decoded = decode_from_wasm(&serialized, SerializationFormat::Json).expect("decode failed");
        assert_eq!(output, decoded);
    }

    // -------------------------------------------------------------------------
    // WasmChallengeMetadata tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_wasm_challenge_metadata_new() {
        let meta = WasmChallengeMetadata::new("test-challenge", "1.0.0");
        assert_eq!(meta.name, "test-challenge");
        assert_eq!(meta.version, "1.0.0");
        assert!(meta.description.is_none());
        assert!(meta.author.is_none());
    }

    #[test]
    fn test_wasm_challenge_metadata_builder() {
        let meta = WasmChallengeMetadata::new("my-challenge", "2.0.0")
            .with_description("A test challenge")
            .with_author("Test Author")
            .with_serialization_format(SerializationFormat::Bincode)
            .with_memory_pages(32)
            .with_max_eval_time(60_000);

        assert_eq!(meta.name, "my-challenge");
        assert_eq!(meta.version, "2.0.0");
        assert_eq!(meta.description, Some("A test challenge".to_string()));
        assert_eq!(meta.author, Some("Test Author".to_string()));
        assert_eq!(meta.serialization_format, "bincode");
        assert_eq!(meta.memory_pages, 32);
        assert_eq!(meta.max_eval_time_ms, 60_000);
    }

    #[test]
    fn test_wasm_challenge_metadata_get_format() {
        let json_meta = WasmChallengeMetadata::new("t", "1").with_serialization_format(SerializationFormat::Json);
        assert_eq!(json_meta.get_format(), SerializationFormat::Json);

        let bincode_meta = WasmChallengeMetadata::new("t", "1").with_serialization_format(SerializationFormat::Bincode);
        assert_eq!(bincode_meta.get_format(), SerializationFormat::Bincode);
    }

    #[test]
    fn test_wasm_challenge_metadata_default() {
        let meta = WasmChallengeMetadata::default();
        assert_eq!(meta.name, "unknown");
        assert_eq!(meta.version, "0.0.0");
    }

    // -------------------------------------------------------------------------
    // LogLevel tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_log_level_from_u32() {
        assert_eq!(LogLevel::from(0), LogLevel::Trace);
        assert_eq!(LogLevel::from(1), LogLevel::Debug);
        assert_eq!(LogLevel::from(2), LogLevel::Info);
        assert_eq!(LogLevel::from(3), LogLevel::Warn);
        assert_eq!(LogLevel::from(4), LogLevel::Error);
        assert_eq!(LogLevel::from(99), LogLevel::Error); // Default to error for unknown
    }

    // -------------------------------------------------------------------------
    // Error type tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_wasm_serialize_error_display() {
        let err = WasmSerializeError::SerializationFailed("test error".to_string());
        assert_eq!(err.to_string(), "Serialization failed: test error");

        let err = WasmSerializeError::DeserializationFailed("parse error".to_string());
        assert_eq!(err.to_string(), "Deserialization failed: parse error");

        let err = WasmSerializeError::InvalidFormat("bad data".to_string());
        assert_eq!(err.to_string(), "Invalid data format: bad data");

        let err = WasmSerializeError::MemoryError("out of memory".to_string());
        assert_eq!(err.to_string(), "Memory error: out of memory");
    }

    // -------------------------------------------------------------------------
    // Integration test: full evaluation cycle
    // -------------------------------------------------------------------------

    #[test]
    fn test_full_evaluation_cycle() {
        // Simulate a complete evaluation cycle
        
        // 1. Create input
        let input = WasmEvaluationInput::with_metadata(
            "submission-123",
            "miner-456",
            b"42".to_vec(),
            serde_json::json!({"expected": 42}),
        );

        // 2. Serialize for WASM
        let buffer = encode_for_wasm(&input, SerializationFormat::Json).expect("encode failed");
        assert!(!buffer.is_empty());

        // 3. Deserialize in "WASM" (simulated)
        let decoded_input = deserialize_input(&buffer.data, SerializationFormat::Json).expect("deserialize failed");
        assert_eq!(decoded_input.submission_id, "submission-123");

        // 4. Process and create output
        let output = WasmEvaluationOutput::success_with_results(
            0.95,
            serde_json::json!({"tests_passed": 19, "tests_total": 20}),
        );

        // 5. Serialize output
        let output_bytes = serialize_output(&output, SerializationFormat::Json).expect("serialize failed");

        // 6. Deserialize in host
        let final_output = decode_from_wasm(&output_bytes, SerializationFormat::Json).expect("decode failed");
        assert!((final_output.score - 0.95).abs() < f64::EPSILON);
        assert!(final_output.is_valid);
    }

    // -------------------------------------------------------------------------
    // WasmChallenge trait test (compile-time check)
    // -------------------------------------------------------------------------

    struct TestChallenge;

    impl WasmChallenge for TestChallenge {
        fn name() -> &'static str {
            "test-challenge"
        }

        fn version() -> &'static str {
            "1.0.0"
        }

        fn evaluate(input: WasmEvaluationInput) -> WasmEvaluationOutput {
            // Simple echo test: score based on data length
            let score = (input.data.len() as f64 / 100.0).min(1.0);
            WasmEvaluationOutput::success(score)
        }

        fn validate(input: WasmEvaluationInput) -> WasmValidationResult {
            if input.data.is_empty() {
                WasmValidationResult::invalid("Data cannot be empty")
            } else {
                WasmValidationResult::valid()
            }
        }
    }

    #[test]
    fn test_wasm_challenge_implementation() {
        assert_eq!(TestChallenge::name(), "test-challenge");
        assert_eq!(TestChallenge::version(), "1.0.0");

        let input = WasmEvaluationInput::new("s", "p", vec![0; 50]);
        let output = TestChallenge::evaluate(input);
        assert!((output.score - 0.5).abs() < f64::EPSILON);

        let empty_input = WasmEvaluationInput::new("s", "p", vec![]);
        let validation = TestChallenge::validate(empty_input);
        assert!(!validation.is_valid);

        let valid_input = WasmEvaluationInput::new("s", "p", vec![1]);
        let validation = TestChallenge::validate(valid_input);
        assert!(validation.is_valid);
    }
}
