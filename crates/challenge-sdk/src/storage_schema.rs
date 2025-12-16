//! Challenge Storage Schema System
//!
//! Each challenge defines its own data classes with:
//! - Schema definition (fields, types)
//! - Validation rules per class (signature, rate limit, etc.)
//! - Custom validators for business logic
//!
//! # Validation Flow
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        WRITE REQUEST FLOW                               │
//! │                                                                         │
//! │  1. Miner signs (content_hash + hotkey + epoch) → signature            │
//! │  2. Miner sends request to Validator RPC                                │
//! │  3. Validator receives request:                                         │
//! │     a. verify_signature(request) → Is this really from the miner?      │
//! │     b. verify_rate_limit(miner) → Can they submit this epoch?          │
//! │     c. verify_content(data) → Is the data valid?                       │
//! │     d. custom_validation() → Challenge-specific rules                   │
//! │  4. If all pass → Broadcast to P2P for consensus                       │
//! │  5. 50% validators agree → Store permanently                            │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

// ============================================================================
// CHALLENGE SCHEMA
// ============================================================================

/// Complete schema definition for a challenge
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeSchema {
    /// Challenge identifier
    pub challenge_id: String,
    /// Schema version
    pub version: String,
    /// Description
    pub description: String,
    /// Data classes
    pub classes: Vec<DataClass>,
    /// Global rules
    pub global_rules: GlobalRules,
}

impl ChallengeSchema {
    pub fn builder(challenge_id: &str) -> SchemaBuilder {
        SchemaBuilder::new(challenge_id)
    }

    pub fn get_class(&self, name: &str) -> Option<&DataClass> {
        self.classes.iter().find(|c| c.name == name)
    }

    pub fn validate(&self) -> Result<(), SchemaError> {
        if self.challenge_id.is_empty() {
            return Err(SchemaError::InvalidSchema("challenge_id required".into()));
        }
        if self.classes.is_empty() {
            return Err(SchemaError::InvalidSchema(
                "at least one class required".into(),
            ));
        }
        for class in &self.classes {
            class.validate()?;
        }
        Ok(())
    }
}

// ============================================================================
// DATA CLASS
// ============================================================================

/// A data class with validation rules
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataClass {
    /// Class name
    pub name: String,
    /// Description
    pub description: String,
    /// Fields
    pub fields: Vec<Field>,
    /// Validation rules
    pub validation: ClassValidation,
    /// Indexes
    pub indexes: Vec<String>,
    /// Key pattern
    pub key_pattern: String,
}

impl DataClass {
    pub fn builder(name: &str) -> ClassBuilder {
        ClassBuilder::new(name)
    }

    pub fn validate(&self) -> Result<(), SchemaError> {
        if self.name.is_empty() {
            return Err(SchemaError::InvalidSchema("class name required".into()));
        }
        if self.key_pattern.is_empty() {
            return Err(SchemaError::InvalidSchema("key_pattern required".into()));
        }
        Ok(())
    }

    pub fn generate_key(&self, data: &serde_json::Value) -> Result<String, SchemaError> {
        let mut key = self.key_pattern.clone();
        for field_ref in extract_field_refs(&self.key_pattern) {
            let value = data
                .get(&field_ref)
                .ok_or_else(|| SchemaError::MissingField(field_ref.clone()))?;
            let str_value = match value {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => value.to_string(),
            };
            key = key.replace(&format!("{{{}}}", field_ref), &str_value);
        }
        Ok(key)
    }
}

fn extract_field_refs(pattern: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut in_brace = false;
    let mut current = String::new();

    for c in pattern.chars() {
        match c {
            '{' => {
                in_brace = true;
                current.clear();
            }
            '}' => {
                if in_brace && !current.is_empty() {
                    refs.push(current.clone());
                }
                in_brace = false;
            }
            _ if in_brace => current.push(c),
            _ => {}
        }
    }
    refs
}

// ============================================================================
// FIELD DEFINITION
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub field_type: FieldType,
    pub required: bool,
    pub description: String,
}

impl Field {
    pub fn new(name: &str, field_type: FieldType) -> Self {
        Self {
            name: name.to_string(),
            field_type,
            required: true,
            description: String::new(),
        }
    }

    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FieldType {
    String,
    Integer,
    Float,
    Boolean,
    Bytes,
    Hash,
    Signature,
    Timestamp,
    Hotkey,
    Array(Box<FieldType>),
    Object,
}

// ============================================================================
// CLASS VALIDATION RULES
// ============================================================================

/// Validation rules for a data class
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassValidation {
    /// Maximum entry size in bytes
    pub max_size: usize,

    /// Require signature verification from the submitter
    pub require_signature: bool,

    /// Rate limit configuration
    pub rate_limit: Option<RateLimitConfig>,

    /// Require consensus (50% validators)
    pub require_consensus: bool,

    /// Who can update existing entries
    pub updatable_by: UpdatePermission,

    /// Custom validator function name
    pub custom_validator: Option<String>,
}

impl Default for ClassValidation {
    fn default() -> Self {
        Self {
            max_size: 1024 * 1024, // 1 MB
            require_signature: true,
            rate_limit: None,
            require_consensus: true,
            updatable_by: UpdatePermission::CreatorOnly,
            custom_validator: None,
        }
    }
}

/// Rate limit configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Maximum submissions per window
    /// e.g., 1 submission per 4 epochs = max_per_window: 1, window_epochs: 4
    pub max_per_window: u32,
    /// Window size in epochs
    pub window_epochs: u64,
    /// Per-miner or per-validator
    pub limit_by: RateLimitBy,
}

impl RateLimitConfig {
    /// 1 submission per N epochs
    pub fn one_per_epochs(epochs: u64) -> Self {
        Self {
            max_per_window: 1,
            window_epochs: epochs,
            limit_by: RateLimitBy::Submitter,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RateLimitBy {
    /// Rate limit by submitter (miner hotkey)
    Submitter,
    /// Rate limit by validator
    Validator,
    /// Global rate limit
    Global,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UpdatePermission {
    /// Only the original creator
    CreatorOnly,
    /// Any validator
    AnyValidator,
    /// No updates (immutable)
    Immutable,
}

// ============================================================================
// GLOBAL RULES
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalRules {
    /// Maximum entry size
    pub max_entry_size: usize,
    /// Banned hotkeys
    pub banned_hotkeys: Vec<String>,
    /// Banned coldkeys
    pub banned_coldkeys: Vec<String>,
}

impl Default for GlobalRules {
    fn default() -> Self {
        Self {
            max_entry_size: 10 * 1024 * 1024,
            banned_hotkeys: Vec::new(),
            banned_coldkeys: Vec::new(),
        }
    }
}

// ============================================================================
// WRITE REQUEST (what gets validated)
// ============================================================================

/// A write request to be validated
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteRequest {
    /// Class being written to
    pub class_name: String,
    /// Key (generated from key_pattern)
    pub key: String,
    /// Serialized data
    pub data: Vec<u8>,
    /// Submitter hotkey (miner or validator)
    pub submitter_hotkey: String,
    /// Submitter coldkey
    pub submitter_coldkey: Option<String>,
    /// Current epoch
    pub epoch: u64,
    /// Current block
    pub block: u64,
    /// Content hash (SHA256 of data)
    pub content_hash: [u8; 32],
    /// Signature of (content_hash + submitter_hotkey + epoch)
    pub signature: Vec<u8>,
    /// Is this an update?
    pub is_update: bool,
}

impl WriteRequest {
    /// Compute what should be signed: SHA256(content_hash || hotkey || epoch)
    pub fn compute_sign_payload(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(&self.content_hash);
        hasher.update(self.submitter_hotkey.as_bytes());
        hasher.update(self.epoch.to_le_bytes());
        hasher.finalize().into()
    }

    /// Verify content hash matches data
    pub fn verify_content_hash(&self) -> bool {
        let computed: [u8; 32] = Sha256::digest(&self.data).into();
        computed == self.content_hash
    }

    /// Deserialize data
    pub fn deserialize<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.data)
    }
}

// ============================================================================
// VALIDATION CONTEXT
// ============================================================================

/// Context for validation
#[derive(Debug, Clone)]
pub struct ValidationContext {
    /// Current epoch
    pub epoch: u64,
    /// Current block
    pub block: u64,
    /// Total validators
    pub total_validators: usize,
    /// Our validator hotkey
    pub our_validator: String,
    /// Submissions by this submitter in recent epochs (epoch -> count)
    pub submitter_history: HashMap<u64, u32>,
    /// Existing entry creator (if update)
    pub existing_creator: Option<String>,
}

impl ValidationContext {
    /// Count submissions in window
    pub fn count_in_window(&self, window_epochs: u64) -> u32 {
        let start_epoch = self.epoch.saturating_sub(window_epochs - 1);
        (start_epoch..=self.epoch)
            .filter_map(|e| self.submitter_history.get(&e))
            .sum()
    }
}

// ============================================================================
// VALIDATION RESULT
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ValidationResult {
    Valid,
    Invalid(ValidationError),
}

impl ValidationResult {
    pub fn valid() -> Self {
        ValidationResult::Valid
    }
    pub fn invalid(err: ValidationError) -> Self {
        ValidationResult::Invalid(err)
    }
    pub fn is_valid(&self) -> bool {
        matches!(self, ValidationResult::Valid)
    }
    pub fn error(&self) -> Option<&ValidationError> {
        match self {
            ValidationResult::Invalid(e) => Some(e),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    pub code: String,
    pub message: String,
    pub field: Option<String>,
}

impl ValidationError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_string(),
            message: message.to_string(),
            field: None,
        }
    }

    pub fn with_field(mut self, field: &str) -> Self {
        self.field = Some(field.to_string());
        self
    }

    pub fn signature_invalid() -> Self {
        Self::new("SIGNATURE_INVALID", "Signature verification failed")
    }

    pub fn content_hash_mismatch() -> Self {
        Self::new("CONTENT_HASH_MISMATCH", "Content hash does not match data")
    }

    pub fn rate_limited(max: u32, window: u64) -> Self {
        Self::new(
            "RATE_LIMITED",
            &format!("Max {} per {} epochs", max, window),
        )
    }

    pub fn too_large(max: usize, actual: usize) -> Self {
        Self::new("TOO_LARGE", &format!("Max {} bytes, got {}", max, actual))
    }

    pub fn banned() -> Self {
        Self::new("BANNED", "Submitter is banned")
    }

    pub fn unauthorized(reason: &str) -> Self {
        Self::new("UNAUTHORIZED", reason)
    }

    pub fn missing_field(field: &str) -> Self {
        Self::new("MISSING_FIELD", &format!("Required: {}", field)).with_field(field)
    }

    pub fn custom(message: &str) -> Self {
        Self::new("CUSTOM", message)
    }
}

// ============================================================================
// SCHEMA ERROR
// ============================================================================

#[derive(Debug, Clone)]
pub enum SchemaError {
    InvalidSchema(String),
    ClassNotFound(String),
    MissingField(String),
    ValidationFailed(ValidationError),
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaError::InvalidSchema(e) => write!(f, "Invalid schema: {}", e),
            SchemaError::ClassNotFound(c) => write!(f, "Class not found: {}", c),
            SchemaError::MissingField(n) => write!(f, "Missing field: {}", n),
            SchemaError::ValidationFailed(e) => write!(f, "Validation: {}", e.message),
        }
    }
}

impl std::error::Error for SchemaError {}

// ============================================================================
// SCHEMA VALIDATOR TRAIT
// ============================================================================

/// Trait for challenge-specific validation
pub trait SchemaValidator: Send + Sync {
    /// Get schema
    fn schema(&self) -> &ChallengeSchema;

    /// Verify signature (challenge provides crypto implementation)
    fn verify_signature(&self, request: &WriteRequest) -> bool;

    /// Validate a write request
    fn validate_write(&self, request: &WriteRequest, ctx: &ValidationContext) -> ValidationResult {
        let schema = self.schema();

        // Get class
        let class = match schema.get_class(&request.class_name) {
            Some(c) => c,
            None => {
                return ValidationResult::invalid(ValidationError::new(
                    "CLASS_NOT_FOUND",
                    &request.class_name,
                ))
            }
        };

        // 1. Check banned
        if schema
            .global_rules
            .banned_hotkeys
            .contains(&request.submitter_hotkey)
        {
            return ValidationResult::invalid(ValidationError::banned());
        }
        if let Some(coldkey) = &request.submitter_coldkey {
            if schema.global_rules.banned_coldkeys.contains(coldkey) {
                return ValidationResult::invalid(ValidationError::banned());
            }
        }

        // 2. Check size
        if request.data.len() > class.validation.max_size {
            return ValidationResult::invalid(ValidationError::too_large(
                class.validation.max_size,
                request.data.len(),
            ));
        }

        // 3. Verify content hash (anti-tampering)
        if !request.verify_content_hash() {
            return ValidationResult::invalid(ValidationError::content_hash_mismatch());
        }

        // 4. Verify signature (anti-relay attack)
        if class.validation.require_signature {
            if !self.verify_signature(request) {
                return ValidationResult::invalid(ValidationError::signature_invalid());
            }
        }

        // 5. Check rate limit
        if let Some(rate_limit) = &class.validation.rate_limit {
            let count = ctx.count_in_window(rate_limit.window_epochs);
            if count >= rate_limit.max_per_window {
                return ValidationResult::invalid(ValidationError::rate_limited(
                    rate_limit.max_per_window,
                    rate_limit.window_epochs,
                ));
            }
        }

        // 6. Check update permission
        if request.is_update {
            match &class.validation.updatable_by {
                UpdatePermission::Immutable => {
                    return ValidationResult::invalid(ValidationError::unauthorized(
                        "Class is immutable",
                    ));
                }
                UpdatePermission::CreatorOnly => {
                    if ctx.existing_creator.as_ref() != Some(&request.submitter_hotkey) {
                        return ValidationResult::invalid(ValidationError::unauthorized(
                            "Only creator can update",
                        ));
                    }
                }
                UpdatePermission::AnyValidator => {}
            }
        }

        // 7. Validate fields
        if let Err(e) = self.validate_fields(request, class) {
            return ValidationResult::invalid(e);
        }

        // 8. Custom validation
        if let Some(validator_name) = &class.validation.custom_validator {
            if let Err(e) = self.run_custom_validator(validator_name, request, ctx) {
                return ValidationResult::invalid(e);
            }
        }

        ValidationResult::Valid
    }

    /// Validate fields
    fn validate_fields(
        &self,
        request: &WriteRequest,
        class: &DataClass,
    ) -> Result<(), ValidationError> {
        let data: serde_json::Value = request
            .deserialize()
            .map_err(|e| ValidationError::new("INVALID_JSON", &e.to_string()))?;

        for field in &class.fields {
            if field.required && data.get(&field.name).is_none() {
                return Err(ValidationError::missing_field(&field.name));
            }
        }

        Ok(())
    }

    /// Run custom validator
    fn run_custom_validator(
        &self,
        _name: &str,
        _request: &WriteRequest,
        _ctx: &ValidationContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
}

// ============================================================================
// BUILDERS
// ============================================================================

pub struct SchemaBuilder {
    challenge_id: String,
    version: String,
    description: String,
    classes: Vec<DataClass>,
    global_rules: GlobalRules,
}

impl SchemaBuilder {
    pub fn new(challenge_id: &str) -> Self {
        Self {
            challenge_id: challenge_id.to_string(),
            version: "1.0.0".to_string(),
            description: String::new(),
            classes: Vec::new(),
            global_rules: GlobalRules::default(),
        }
    }

    pub fn version(mut self, v: &str) -> Self {
        self.version = v.to_string();
        self
    }
    pub fn description(mut self, d: &str) -> Self {
        self.description = d.to_string();
        self
    }
    pub fn class(mut self, c: DataClass) -> Self {
        self.classes.push(c);
        self
    }
    pub fn global_rules(mut self, r: GlobalRules) -> Self {
        self.global_rules = r;
        self
    }

    pub fn build(self) -> Result<ChallengeSchema, SchemaError> {
        let schema = ChallengeSchema {
            challenge_id: self.challenge_id,
            version: self.version,
            description: self.description,
            classes: self.classes,
            global_rules: self.global_rules,
        };
        schema.validate()?;
        Ok(schema)
    }
}

pub struct ClassBuilder {
    name: String,
    description: String,
    fields: Vec<Field>,
    validation: ClassValidation,
    indexes: Vec<String>,
    key_pattern: String,
}

impl ClassBuilder {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            description: String::new(),
            fields: Vec::new(),
            validation: ClassValidation::default(),
            indexes: Vec::new(),
            key_pattern: String::new(),
        }
    }

    pub fn description(mut self, d: &str) -> Self {
        self.description = d.to_string();
        self
    }
    pub fn field(mut self, f: Field) -> Self {
        self.fields.push(f);
        self
    }
    pub fn string_field(self, n: &str) -> Self {
        self.field(Field::new(n, FieldType::String))
    }
    pub fn int_field(self, n: &str) -> Self {
        self.field(Field::new(n, FieldType::Integer))
    }
    pub fn float_field(self, n: &str) -> Self {
        self.field(Field::new(n, FieldType::Float))
    }
    pub fn hash_field(self, n: &str) -> Self {
        self.field(Field::new(n, FieldType::Hash))
    }
    pub fn hotkey_field(self, n: &str) -> Self {
        self.field(Field::new(n, FieldType::Hotkey))
    }
    pub fn signature_field(self, n: &str) -> Self {
        self.field(Field::new(n, FieldType::Signature))
    }
    pub fn timestamp_field(self, n: &str) -> Self {
        self.field(Field::new(n, FieldType::Timestamp))
    }
    pub fn bytes_field(self, n: &str) -> Self {
        self.field(Field::new(n, FieldType::Bytes))
    }

    pub fn validation(mut self, v: ClassValidation) -> Self {
        self.validation = v;
        self
    }
    pub fn max_size(mut self, s: usize) -> Self {
        self.validation.max_size = s;
        self
    }
    pub fn require_signature(mut self) -> Self {
        self.validation.require_signature = true;
        self
    }
    pub fn no_signature(mut self) -> Self {
        self.validation.require_signature = false;
        self
    }

    pub fn rate_limit(mut self, max: u32, window_epochs: u64) -> Self {
        self.validation.rate_limit = Some(RateLimitConfig {
            max_per_window: max,
            window_epochs,
            limit_by: RateLimitBy::Submitter,
        });
        self
    }

    pub fn one_per_epochs(self, epochs: u64) -> Self {
        self.rate_limit(1, epochs)
    }

    pub fn require_consensus(mut self) -> Self {
        self.validation.require_consensus = true;
        self
    }
    pub fn no_consensus(mut self) -> Self {
        self.validation.require_consensus = false;
        self
    }
    pub fn immutable(mut self) -> Self {
        self.validation.updatable_by = UpdatePermission::Immutable;
        self
    }
    pub fn creator_only(mut self) -> Self {
        self.validation.updatable_by = UpdatePermission::CreatorOnly;
        self
    }
    pub fn any_validator(mut self) -> Self {
        self.validation.updatable_by = UpdatePermission::AnyValidator;
        self
    }
    pub fn custom_validator(mut self, n: &str) -> Self {
        self.validation.custom_validator = Some(n.to_string());
        self
    }

    pub fn index(mut self, f: &str) -> Self {
        self.indexes.push(f.to_string());
        self
    }
    pub fn key_pattern(mut self, p: &str) -> Self {
        self.key_pattern = p.to_string();
        self
    }

    pub fn build(self) -> DataClass {
        DataClass {
            name: self.name,
            description: self.description,
            fields: self.fields,
            validation: self.validation,
            indexes: self.indexes,
            key_pattern: self.key_pattern,
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_schema() -> ChallengeSchema {
        ChallengeSchema::builder("test-challenge")
            .version("1.0.0")
            .class(
                DataClass::builder("Agent")
                    .description("Agent submission")
                    .string_field("agent_hash")
                    .hotkey_field("miner_hotkey")
                    .bytes_field("source_code")
                    .signature_field("signature")
                    .key_pattern("{agent_hash}")
                    .index("miner_hotkey")
                    .require_signature()
                    .one_per_epochs(4) // 1 agent per 4 epochs
                    .require_consensus()
                    .immutable()
                    .custom_validator("validate_agent")
                    .build(),
            )
            .class(
                DataClass::builder("Evaluation")
                    .description("Validator evaluation")
                    .string_field("agent_hash")
                    .hotkey_field("validator_hotkey")
                    .float_field("score")
                    .key_pattern("{agent_hash}:{validator_hotkey}")
                    .no_signature() // Validator stores their own
                    .no_consensus() // Each validator stores independently
                    .creator_only()
                    .build(),
            )
            .build()
            .unwrap()
    }

    struct TestValidator {
        schema: ChallengeSchema,
    }

    impl SchemaValidator for TestValidator {
        fn schema(&self) -> &ChallengeSchema {
            &self.schema
        }

        fn verify_signature(&self, _request: &WriteRequest) -> bool {
            // In tests, always valid
            true
        }

        fn run_custom_validator(
            &self,
            name: &str,
            request: &WriteRequest,
            _ctx: &ValidationContext,
        ) -> Result<(), ValidationError> {
            if name == "validate_agent" {
                // Check source code not empty
                let data: serde_json::Value = request.deserialize().unwrap();
                if let Some(code) = data.get("source_code") {
                    if code.as_str().map(|s| s.is_empty()).unwrap_or(true) {
                        return Err(ValidationError::custom("source_code cannot be empty"));
                    }
                }
            }
            Ok(())
        }
    }

    #[test]
    fn test_schema_builder() {
        let schema = create_test_schema();
        assert_eq!(schema.challenge_id, "test-challenge");
        assert_eq!(schema.classes.len(), 2);

        let agent = schema.get_class("Agent").unwrap();
        assert!(agent.validation.require_signature);
        assert!(agent.validation.require_consensus);
        assert_eq!(agent.validation.updatable_by, UpdatePermission::Immutable);

        let rate_limit = agent.validation.rate_limit.as_ref().unwrap();
        assert_eq!(rate_limit.max_per_window, 1);
        assert_eq!(rate_limit.window_epochs, 4);
    }

    #[test]
    fn test_rate_limit_validation() {
        let schema = create_test_schema();
        let validator = TestValidator { schema };

        let data = serde_json::json!({
            "agent_hash": "abc123",
            "miner_hotkey": "miner1",
            "source_code": "print('hello')",
            "signature": [1, 2, 3]
        });
        let data_bytes = serde_json::to_vec(&data).unwrap();
        let content_hash: [u8; 32] = Sha256::digest(&data_bytes).into();

        let request = WriteRequest {
            class_name: "Agent".to_string(),
            key: "abc123".to_string(),
            data: data_bytes,
            submitter_hotkey: "miner1".to_string(),
            submitter_coldkey: None,
            epoch: 10,
            block: 1000,
            content_hash,
            signature: vec![1, 2, 3],
            is_update: false,
        };

        // First submission (no history) - should pass
        let ctx = ValidationContext {
            epoch: 10,
            block: 1000,
            total_validators: 3,
            our_validator: "validator1".to_string(),
            submitter_history: HashMap::new(),
            existing_creator: None,
        };

        let result = validator.validate_write(&request, &ctx);
        assert!(result.is_valid(), "First submission should pass");

        // Second submission in same window (history shows 1 in epoch 8) - should fail
        let mut history = HashMap::new();
        history.insert(8, 1); // Submitted in epoch 8, within 4-epoch window

        let ctx_with_history = ValidationContext {
            epoch: 10,
            submitter_history: history,
            ..ctx
        };

        let result = validator.validate_write(&request, &ctx_with_history);
        assert!(
            !result.is_valid(),
            "Second submission should fail rate limit"
        );
        assert_eq!(result.error().unwrap().code, "RATE_LIMITED");

        // Submission after window passes (epoch 13, last was epoch 8)
        let mut old_history = HashMap::new();
        old_history.insert(8, 1);

        let ctx_after_window = ValidationContext {
            epoch: 13, // 8 + 4 + 1 = 13, so epoch 8 is outside window [10, 13]
            submitter_history: old_history,
            ..ctx_with_history
        };

        let result = validator.validate_write(&request, &ctx_after_window);
        assert!(result.is_valid(), "Submission after window should pass");
    }

    #[test]
    fn test_content_hash_verification() {
        let schema = create_test_schema();
        let validator = TestValidator { schema };

        let data = serde_json::json!({
            "agent_hash": "abc123",
            "miner_hotkey": "miner1",
            "source_code": "print('hello')",
            "signature": [1, 2, 3]
        });
        let data_bytes = serde_json::to_vec(&data).unwrap();

        // Wrong content hash
        let wrong_hash = [0u8; 32];

        let request = WriteRequest {
            class_name: "Agent".to_string(),
            key: "abc123".to_string(),
            data: data_bytes,
            submitter_hotkey: "miner1".to_string(),
            submitter_coldkey: None,
            epoch: 10,
            block: 1000,
            content_hash: wrong_hash, // Wrong!
            signature: vec![],
            is_update: false,
        };

        let ctx = ValidationContext {
            epoch: 10,
            block: 1000,
            total_validators: 3,
            our_validator: "validator1".to_string(),
            submitter_history: HashMap::new(),
            existing_creator: None,
        };

        let result = validator.validate_write(&request, &ctx);
        assert!(!result.is_valid());
        assert_eq!(result.error().unwrap().code, "CONTENT_HASH_MISMATCH");
    }

    #[test]
    fn test_banned_check() {
        let mut schema = create_test_schema();
        schema
            .global_rules
            .banned_hotkeys
            .push("bad_miner".to_string());

        let validator = TestValidator { schema };

        let data = serde_json::json!({
            "agent_hash": "abc123",
            "miner_hotkey": "bad_miner",
            "source_code": "evil code",
            "signature": [1, 2, 3]
        });
        let data_bytes = serde_json::to_vec(&data).unwrap();
        let content_hash: [u8; 32] = Sha256::digest(&data_bytes).into();

        let request = WriteRequest {
            class_name: "Agent".to_string(),
            key: "abc123".to_string(),
            data: data_bytes,
            submitter_hotkey: "bad_miner".to_string(),
            submitter_coldkey: None,
            epoch: 10,
            block: 1000,
            content_hash,
            signature: vec![],
            is_update: false,
        };

        let ctx = ValidationContext {
            epoch: 10,
            block: 1000,
            total_validators: 3,
            our_validator: "validator1".to_string(),
            submitter_history: HashMap::new(),
            existing_creator: None,
        };

        let result = validator.validate_write(&request, &ctx);
        assert!(!result.is_valid());
        assert_eq!(result.error().unwrap().code, "BANNED");
    }
}
