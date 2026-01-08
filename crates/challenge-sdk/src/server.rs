//! Challenge Server Mode
//!
//! Provides HTTP server functionality for challenges to expose their evaluation
//! endpoints to platform-server.
//!
//! # Usage
//!
//! ```rust,ignore
//! use platform_challenge_sdk::server::{ChallengeServer, ServerConfig};
//!
//! let server = ChallengeServer::new(my_challenge)
//!     .config(ServerConfig::default())
//!     .build();
//!
//! server.run().await?;
//! ```
//!
//! # Endpoints
//!
//! The server exposes:
//! - `POST /evaluate` - Receive evaluation requests from platform
//! - `GET /health` - Health check
//! - `GET /config` - Challenge configuration schema
//! - `POST /validate` - Quick validation without full evaluation

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::error::ChallengeError;

#[cfg(feature = "http-server")]
use axum::extract::State;

/// Server configuration
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Host to bind to
    pub host: String,
    /// Port to listen on
    pub port: u16,
    /// Maximum concurrent evaluations
    pub max_concurrent: usize,
    /// Request timeout in seconds
    pub timeout_secs: u64,
    /// Enable CORS
    pub cors_enabled: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
            max_concurrent: 4,
            timeout_secs: 600,
            cors_enabled: true,
        }
    }
}

impl ServerConfig {
    /// Load from environment variables
    pub fn from_env() -> Self {
        Self {
            host: std::env::var("CHALLENGE_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: std::env::var("CHALLENGE_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(8080),
            max_concurrent: std::env::var("MAX_CONCURRENT")
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(4),
            timeout_secs: std::env::var("TIMEOUT_SECS")
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(600),
            cors_enabled: true,
        }
    }
}

// ============================================================================
// GENERIC REQUEST/RESPONSE TYPES
// ============================================================================

/// Generic evaluation request from platform
///
/// Platform sends this, challenge interprets the `data` field based on its needs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationRequest {
    /// Unique request ID
    pub request_id: String,
    /// Submission identifier
    pub submission_id: String,
    /// Participant identifier (could be miner hotkey, user id, etc.)
    pub participant_id: String,
    /// Submission data (challenge-specific, could be source code, config, etc.)
    pub data: serde_json::Value,
    /// Optional metadata
    pub metadata: Option<serde_json::Value>,
    /// Current epoch/round
    pub epoch: u64,
    /// Deadline timestamp (unix seconds)
    pub deadline: Option<i64>,
}

/// Generic evaluation response to platform
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResponse {
    /// Request ID this responds to
    pub request_id: String,
    /// Whether evaluation succeeded
    pub success: bool,
    /// Error message if failed
    pub error: Option<String>,
    /// Evaluation score (0.0 - 1.0)
    pub score: f64,
    /// Detailed results (challenge-specific)
    pub results: serde_json::Value,
    /// Execution time in milliseconds
    pub execution_time_ms: i64,
    /// Cost incurred (if applicable)
    pub cost: Option<f64>,
}

impl EvaluationResponse {
    /// Create successful response
    pub fn success(request_id: &str, score: f64, results: serde_json::Value) -> Self {
        Self {
            request_id: request_id.to_string(),
            success: true,
            error: None,
            score,
            results,
            execution_time_ms: 0,
            cost: None,
        }
    }

    /// Create error response
    pub fn error(request_id: &str, error: impl Into<String>) -> Self {
        Self {
            request_id: request_id.to_string(),
            success: false,
            error: Some(error.into()),
            score: 0.0,
            results: serde_json::Value::Null,
            execution_time_ms: 0,
            cost: None,
        }
    }

    /// Set execution time
    pub fn with_time(mut self, ms: i64) -> Self {
        self.execution_time_ms = ms;
        self
    }

    /// Set cost
    pub fn with_cost(mut self, cost: f64) -> Self {
        self.cost = Some(cost);
        self
    }
}

/// Validation request (quick check without full evaluation)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationRequest {
    /// Data to validate
    pub data: serde_json::Value,
}

/// Validation response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResponse {
    /// Whether data is valid
    pub valid: bool,
    /// Validation errors
    pub errors: Vec<String>,
    /// Warnings (valid but not recommended)
    pub warnings: Vec<String>,
}

/// Health check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Server is healthy
    pub healthy: bool,
    /// Current load (0.0 - 1.0)
    pub load: f64,
    /// Pending evaluations
    pub pending: u32,
    /// Uptime in seconds
    pub uptime_secs: u64,
    /// Version
    pub version: String,
    /// Challenge ID
    pub challenge_id: String,
}

/// Configuration schema response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigResponse {
    /// Challenge ID
    pub challenge_id: String,
    /// Challenge name
    pub name: String,
    /// Version
    pub version: String,
    /// Configuration schema (JSON Schema)
    pub config_schema: Option<serde_json::Value>,
    /// Supported features
    pub features: Vec<String>,
    /// Limits
    pub limits: ConfigLimits,
}

/// Configuration limits
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigLimits {
    /// Maximum submission size in bytes
    pub max_submission_size: Option<u64>,
    /// Maximum evaluation time in seconds
    pub max_evaluation_time: Option<u64>,
    /// Maximum cost per evaluation
    pub max_cost: Option<f64>,
}

// ============================================================================
// SERVER TRAIT
// ============================================================================

/// Trait that challenges must implement for server mode
#[async_trait::async_trait]
pub trait ServerChallenge: Send + Sync {
    /// Get challenge ID
    fn challenge_id(&self) -> &str;

    /// Get challenge name
    fn name(&self) -> &str;

    /// Get version
    fn version(&self) -> &str;

    /// Evaluate a submission
    async fn evaluate(
        &self,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, ChallengeError>;

    /// Validate submission data (quick check)
    async fn validate(
        &self,
        request: ValidationRequest,
    ) -> Result<ValidationResponse, ChallengeError> {
        // Default: accept everything
        Ok(ValidationResponse {
            valid: true,
            errors: vec![],
            warnings: vec![],
        })
    }

    /// Get configuration schema
    fn config(&self) -> ConfigResponse {
        ConfigResponse {
            challenge_id: self.challenge_id().to_string(),
            name: self.name().to_string(),
            version: self.version().to_string(),
            config_schema: None,
            features: vec![],
            limits: ConfigLimits::default(),
        }
    }
}

// ============================================================================
// SERVER STATE
// ============================================================================

/// Server state
pub struct ServerState<C: ServerChallenge> {
    pub challenge: Arc<C>,
    pub config: ServerConfig,
    pub started_at: Instant,
    pub pending_count: Arc<RwLock<u32>>,
}

// ============================================================================
// SERVER BUILDER
// ============================================================================

/// Builder for challenge server
pub struct ChallengeServerBuilder<C: ServerChallenge> {
    challenge: C,
    config: ServerConfig,
}

impl<C: ServerChallenge + 'static> ChallengeServerBuilder<C> {
    /// Create new builder
    pub fn new(challenge: C) -> Self {
        Self {
            challenge,
            config: ServerConfig::default(),
        }
    }

    /// Set configuration
    pub fn config(mut self, config: ServerConfig) -> Self {
        self.config = config;
        self
    }

    /// Set host
    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.config.host = host.into();
        self
    }

    /// Set port
    pub fn port(mut self, port: u16) -> Self {
        self.config.port = port;
        self
    }

    /// Load config from environment
    pub fn from_env(mut self) -> Self {
        self.config = ServerConfig::from_env();
        self
    }

    /// Build and return server state
    pub fn build(self) -> ChallengeServer<C> {
        ChallengeServer {
            state: Arc::new(ServerState {
                challenge: Arc::new(self.challenge),
                config: self.config,
                started_at: Instant::now(),
                pending_count: Arc::new(RwLock::new(0)),
            }),
        }
    }
}

/// Challenge HTTP server
pub struct ChallengeServer<C: ServerChallenge> {
    state: Arc<ServerState<C>>,
}

impl<C: ServerChallenge + 'static> ChallengeServer<C> {
    /// Create new server builder
    pub fn builder(challenge: C) -> ChallengeServerBuilder<C> {
        ChallengeServerBuilder::new(challenge)
    }

    /// Run the server (requires axum feature)
    #[cfg(feature = "http-server")]
    pub async fn run(&self) -> Result<(), ChallengeError> {
        use axum::{
            extract::{Json, State},
            http::StatusCode,
            routing::{get, post},
            Router,
        };

        let state = Arc::clone(&self.state);
        let addr: SocketAddr = format!("{}:{}", state.config.host, state.config.port)
            .parse()
            .map_err(|e| ChallengeError::Config(format!("Invalid address: {}", e)))?;

        let app = Router::new()
            .route("/health", get(health_handler::<C>))
            .route("/config", get(config_handler::<C>))
            .route("/evaluate", post(evaluate_handler::<C>))
            .route("/validate", post(validate_handler::<C>))
            .with_state(state);

        info!(
            "Starting challenge server {} on {}",
            self.state.challenge.challenge_id(),
            addr
        );

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| ChallengeError::Io(e.to_string()))?;

        axum::serve(listener, app)
            .await
            .map_err(|e| ChallengeError::Io(e.to_string()))?;

        Ok(())
    }

    /// Get server address
    pub fn address(&self) -> String {
        format!("{}:{}", self.state.config.host, self.state.config.port)
    }
}

// ============================================================================
// HTTP HANDLERS (when http-server feature enabled)
// ============================================================================

#[cfg(feature = "http-server")]
async fn health_handler<C: ServerChallenge + 'static>(
    State(state): State<Arc<ServerState<C>>>,
) -> axum::Json<HealthResponse> {
    let pending = *state.pending_count.read().await;
    let load = pending as f64 / state.config.max_concurrent as f64;

    axum::Json(HealthResponse {
        healthy: true,
        load: load.min(1.0),
        pending,
        uptime_secs: state.started_at.elapsed().as_secs(),
        version: state.challenge.version().to_string(),
        challenge_id: state.challenge.challenge_id().to_string(),
    })
}

#[cfg(feature = "http-server")]
async fn config_handler<C: ServerChallenge + 'static>(
    State(state): State<Arc<ServerState<C>>>,
) -> axum::Json<ConfigResponse> {
    axum::Json(state.challenge.config())
}

#[cfg(feature = "http-server")]
async fn evaluate_handler<C: ServerChallenge + 'static>(
    State(state): State<Arc<ServerState<C>>>,
    axum::Json(request): axum::Json<EvaluationRequest>,
) -> (axum::http::StatusCode, axum::Json<EvaluationResponse>) {
    let request_id = request.request_id.clone();
    let start = Instant::now();

    // Increment pending
    {
        let mut count = state.pending_count.write().await;
        *count += 1;
    }

    let result = state.challenge.evaluate(request).await;

    // Decrement pending
    {
        let mut count = state.pending_count.write().await;
        *count = count.saturating_sub(1);
    }

    match result {
        Ok(mut response) => {
            response.execution_time_ms = start.elapsed().as_millis() as i64;
            (axum::http::StatusCode::OK, axum::Json(response))
        }
        Err(e) => {
            let response = EvaluationResponse::error(&request_id, e.to_string())
                .with_time(start.elapsed().as_millis() as i64);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(response),
            )
        }
    }
}

#[cfg(feature = "http-server")]
async fn validate_handler<C: ServerChallenge + 'static>(
    State(state): State<Arc<ServerState<C>>>,
    axum::Json(request): axum::Json<ValidationRequest>,
) -> axum::Json<ValidationResponse> {
    match state.challenge.validate(request).await {
        Ok(response) => axum::Json(response),
        Err(e) => axum::Json(ValidationResponse {
            valid: false,
            errors: vec![e.to_string()],
            warnings: vec![],
        }),
    }
}

// ============================================================================
// MACROS FOR EASY IMPLEMENTATION
// ============================================================================

/// Macro to implement ServerChallenge for an existing Challenge
#[macro_export]
macro_rules! impl_server_challenge {
    ($type:ty, evaluate: |$self:ident, $req:ident| $body:expr) => {
        #[async_trait::async_trait]
        impl $crate::server::ServerChallenge for $type {
            fn challenge_id(&self) -> &str {
                <Self as $crate::challenge::Challenge>::id(self).as_str()
            }

            fn name(&self) -> &str {
                <Self as $crate::challenge::Challenge>::name(self)
            }

            fn version(&self) -> &str {
                <Self as $crate::challenge::Challenge>::version(self)
            }

            async fn evaluate(
                &$self,
                $req: $crate::server::EvaluationRequest,
            ) -> Result<$crate::server::EvaluationResponse, $crate::error::ChallengeError> {
                $body
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 8080);
        assert_eq!(config.max_concurrent, 4);
        assert_eq!(config.timeout_secs, 600);
        assert!(config.cors_enabled);
    }

    #[test]
    fn test_server_config_from_env() {
        // Should use defaults when env vars not set
        let config = ServerConfig::from_env();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn test_evaluation_request() {
        let req = EvaluationRequest {
            request_id: "req-123".to_string(),
            submission_id: "sub-456".to_string(),
            participant_id: "participant-789".to_string(),
            data: json!({"code": "fn main() {}"}),
            metadata: Some(json!({"version": "1.0"})),
            epoch: 5,
            deadline: Some(1234567890),
        };

        assert_eq!(req.request_id, "req-123");
        assert_eq!(req.epoch, 5);
        assert!(req.metadata.is_some());
    }

    #[test]
    fn test_evaluation_response_success() {
        let resp = EvaluationResponse::success("req-123", 0.95, json!({"passed": 19, "total": 20}));

        assert!(resp.success);
        assert_eq!(resp.request_id, "req-123");
        assert_eq!(resp.score, 0.95);
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_evaluation_response_error() {
        let resp = EvaluationResponse::error("req-456", "Timeout occurred");

        assert!(!resp.success);
        assert_eq!(resp.request_id, "req-456");
        assert_eq!(resp.score, 0.0);
        assert_eq!(resp.error, Some("Timeout occurred".to_string()));
    }

    #[test]
    fn test_evaluation_response_with_time() {
        let resp = EvaluationResponse::success("req", 0.8, json!({})).with_time(1500);

        assert_eq!(resp.execution_time_ms, 1500);
    }

    #[test]
    fn test_evaluation_response_with_cost() {
        let resp = EvaluationResponse::success("req", 0.8, json!({})).with_cost(0.05);

        assert_eq!(resp.cost, Some(0.05));
    }

    #[test]
    fn test_validation_request() {
        let req = ValidationRequest {
            data: json!({"input": "test"}),
        };

        assert_eq!(req.data["input"], "test");
    }

    #[test]
    fn test_validation_response() {
        let resp = ValidationResponse {
            valid: true,
            errors: vec![],
            warnings: vec!["Consider updating format".to_string()],
        };

        assert!(resp.valid);
        assert!(resp.errors.is_empty());
        assert_eq!(resp.warnings.len(), 1);
    }

    #[test]
    fn test_health_response() {
        let health = HealthResponse {
            healthy: true,
            load: 0.5,
            pending: 2,
            uptime_secs: 3600,
            version: "1.0.0".to_string(),
            challenge_id: "test-challenge".to_string(),
        };

        assert!(health.healthy);
        assert_eq!(health.load, 0.5);
        assert_eq!(health.uptime_secs, 3600);
    }

    #[test]
    fn test_config_response() {
        let config = ConfigResponse {
            challenge_id: "test".to_string(),
            name: "Test Challenge".to_string(),
            version: "1.0.0".to_string(),
            config_schema: Some(json!({"type": "object"})),
            features: vec!["feature1".to_string(), "feature2".to_string()],
            limits: ConfigLimits {
                max_submission_size: Some(1024 * 1024),
                max_evaluation_time: Some(300),
                max_cost: Some(0.1),
            },
        };

        assert_eq!(config.challenge_id, "test");
        assert_eq!(config.features.len(), 2);
        assert_eq!(config.limits.max_submission_size, Some(1024 * 1024));
    }

    #[test]
    fn test_config_limits_default() {
        let limits = ConfigLimits::default();
        assert!(limits.max_submission_size.is_none());
        assert!(limits.max_evaluation_time.is_none());
        assert!(limits.max_cost.is_none());
    }

    // Test with a mock challenge
    struct MockChallenge;

    #[async_trait::async_trait]
    impl ServerChallenge for MockChallenge {
        fn challenge_id(&self) -> &str {
            "mock-challenge"
        }

        fn name(&self) -> &str {
            "Mock Challenge"
        }

        fn version(&self) -> &str {
            "1.0.0"
        }

        async fn evaluate(
            &self,
            request: EvaluationRequest,
        ) -> Result<EvaluationResponse, ChallengeError> {
            Ok(EvaluationResponse::success(
                &request.request_id,
                0.75,
                json!({"mock": true}),
            ))
        }
    }

    #[tokio::test]
    async fn test_mock_challenge_evaluate() {
        let challenge = MockChallenge;

        let req = EvaluationRequest {
            request_id: "test".to_string(),
            submission_id: "sub".to_string(),
            participant_id: "participant".to_string(),
            data: json!({}),
            metadata: None,
            epoch: 1,
            deadline: None,
        };

        let result = challenge.evaluate(req).await.unwrap();
        assert!(result.success);
        assert_eq!(result.score, 0.75);
    }

    #[tokio::test]
    async fn test_mock_challenge_validate_default() {
        let challenge = MockChallenge;

        let req = ValidationRequest { data: json!({}) };

        let result = challenge.validate(req).await.unwrap();
        assert!(result.valid); // Default implementation accepts everything
    }

    #[test]
    fn test_mock_challenge_config_default() {
        let challenge = MockChallenge;
        let config = challenge.config();

        assert_eq!(config.challenge_id, "mock-challenge");
        assert_eq!(config.name, "Mock Challenge");
        assert_eq!(config.version, "1.0.0");
        assert!(config.features.is_empty());
    }

    #[test]
    fn test_server_builder_new() {
        let challenge = MockChallenge;
        let builder = ChallengeServerBuilder::new(challenge);

        assert_eq!(builder.config.port, 8080); // default port
    }

    #[test]
    fn test_server_builder_config() {
        let challenge = MockChallenge;
        let custom_config = ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 9000,
            max_concurrent: 10,
            timeout_secs: 120,
            cors_enabled: false,
        };

        let builder = ChallengeServerBuilder::new(challenge).config(custom_config);

        assert_eq!(builder.config.host, "127.0.0.1");
        assert_eq!(builder.config.port, 9000);
    }

    #[test]
    fn test_server_builder_host() {
        let challenge = MockChallenge;
        let builder = ChallengeServerBuilder::new(challenge).host("192.168.1.1");

        assert_eq!(builder.config.host, "192.168.1.1");
    }

    #[test]
    fn test_server_builder_port() {
        let challenge = MockChallenge;
        let builder = ChallengeServerBuilder::new(challenge).port(3000);

        assert_eq!(builder.config.port, 3000);
    }

    #[test]
    fn test_server_builder_build() {
        let challenge = MockChallenge;
        let server = ChallengeServerBuilder::new(challenge)
            .host("localhost")
            .port(8888)
            .build();

        assert_eq!(server.address(), "localhost:8888");
    }

    #[test]
    fn test_challenge_server_builder() {
        let challenge = MockChallenge;
        let builder = ChallengeServer::builder(challenge);

        assert_eq!(builder.config.port, 8080);
    }

    #[test]
    fn test_server_address() {
        let challenge = MockChallenge;
        let server = ChallengeServer::builder(challenge)
            .host("0.0.0.0")
            .port(8080)
            .build();

        assert_eq!(server.address(), "0.0.0.0:8080");
    }

    #[test]
    fn test_server_builder_from_env() {
        // Test from_env method (will use defaults when env vars not set)
        let challenge = MockChallenge;
        let builder = ChallengeServerBuilder::new(challenge).from_env();

        // Should use default values from ServerConfig::from_env()
        assert_eq!(builder.config.host, "0.0.0.0");
        assert_eq!(builder.config.port, 8080);
    }

    #[test]
    fn test_server_config_from_env_with_env_vars() {
        // Note: In real usage, this would read from environment variables
        // For this test, we just verify the function exists and returns defaults
        let config = ServerConfig::from_env();

        // Should return valid config (with defaults when env vars not set)
        assert!(!config.host.is_empty());
        assert!(config.port > 0);
        assert!(config.max_concurrent > 0);
    }
}
