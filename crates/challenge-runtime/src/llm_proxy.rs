//! LLM API Proxy
//!
//! Intercepts all LLM API calls from agent containers to:
//! - Enforce model whitelist
//! - Track token usage and costs
//! - Enforce cost limits
//! - Log all interactions
//!
//! The proxy exposes endpoints compatible with OpenAI and Anthropic APIs.

use crate::host_functions::{LLMCallRecord, LLMUsage};
use axum::{
    body::Body,
    extract::{Json, State},
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use parking_lot::RwLock;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tracing::{error, info};

/// LLM Proxy configuration
#[derive(Debug, Clone)]
pub struct LLMProxyConfig {
    /// Port to listen on
    pub port: u16,
    /// Allowed models (empty = all allowed)
    pub allowed_models: HashSet<String>,
    /// Cost per 1K input tokens by model
    pub input_costs: HashMap<String, f64>,
    /// Cost per 1K output tokens by model
    pub output_costs: HashMap<String, f64>,
    /// Default cost limit per task (USD)
    pub default_cost_limit: f64,
    /// OpenAI API key
    pub openai_api_key: Option<String>,
    /// Anthropic API key
    pub anthropic_api_key: Option<String>,
}

impl Default for LLMProxyConfig {
    fn default() -> Self {
        let mut allowed_models = HashSet::new();
        let mut input_costs = HashMap::new();
        let mut output_costs = HashMap::new();

        // OpenAI models
        for model in &[
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4-turbo",
            "gpt-3.5-turbo",
            "o1",
            "o1-mini",
        ] {
            allowed_models.insert(model.to_string());
        }

        // Anthropic models
        for model in &[
            "claude-3-5-sonnet-20241022",
            "claude-3-5-haiku-20241022",
            "claude-3-opus-20240229",
        ] {
            allowed_models.insert(model.to_string());
        }

        // OpenAI pricing (per 1K tokens)
        input_costs.insert("gpt-4o".to_string(), 0.0025);
        output_costs.insert("gpt-4o".to_string(), 0.01);
        input_costs.insert("gpt-4o-mini".to_string(), 0.00015);
        output_costs.insert("gpt-4o-mini".to_string(), 0.0006);
        input_costs.insert("gpt-4-turbo".to_string(), 0.01);
        output_costs.insert("gpt-4-turbo".to_string(), 0.03);
        input_costs.insert("gpt-3.5-turbo".to_string(), 0.0005);
        output_costs.insert("gpt-3.5-turbo".to_string(), 0.0015);
        input_costs.insert("o1".to_string(), 0.015);
        output_costs.insert("o1".to_string(), 0.06);
        input_costs.insert("o1-mini".to_string(), 0.003);
        output_costs.insert("o1-mini".to_string(), 0.012);

        // Anthropic pricing
        input_costs.insert("claude-3-5-sonnet-20241022".to_string(), 0.003);
        output_costs.insert("claude-3-5-sonnet-20241022".to_string(), 0.015);
        input_costs.insert("claude-3-5-haiku-20241022".to_string(), 0.00025);
        output_costs.insert("claude-3-5-haiku-20241022".to_string(), 0.00125);
        input_costs.insert("claude-3-opus-20240229".to_string(), 0.015);
        output_costs.insert("claude-3-opus-20240229".to_string(), 0.075);

        Self {
            port: 8081,
            allowed_models,
            input_costs,
            output_costs,
            default_cost_limit: 0.50,
            openai_api_key: None,
            anthropic_api_key: None,
        }
    }
}

/// Shared proxy state
pub struct ProxyState {
    pub config: LLMProxyConfig,
    pub http_client: Client,
    /// Current usage by execution ID
    pub usage: RwLock<HashMap<String, UsageTracker>>,
    /// Recorded calls
    pub records: RwLock<Vec<LLMCallRecord>>,
}

/// Usage tracker for an execution
#[derive(Debug, Clone, Default)]
pub struct UsageTracker {
    pub execution_id: String,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub call_count: u64,
    pub cost_limit_usd: f64,
}

impl ProxyState {
    pub fn new(config: LLMProxyConfig) -> Self {
        Self {
            config,
            http_client: Client::new(),
            usage: RwLock::new(HashMap::new()),
            records: RwLock::new(Vec::new()),
        }
    }

    /// Set cost limit for an execution
    pub fn set_limit(&self, execution_id: &str, limit_usd: f64) {
        let mut usage = self.usage.write();
        let tracker = usage.entry(execution_id.to_string()).or_default();
        tracker.execution_id = execution_id.to_string();
        tracker.cost_limit_usd = limit_usd;
    }

    /// Get usage for an execution
    pub fn get_usage(&self, execution_id: &str) -> Option<UsageTracker> {
        self.usage.read().get(execution_id).cloned()
    }

    /// Clear usage for an execution
    pub fn clear_usage(&self, execution_id: &str) {
        self.usage.write().remove(execution_id);
    }

    /// Get all records
    pub fn get_records(&self) -> Vec<LLMCallRecord> {
        self.records.read().clone()
    }

    /// Clear all records
    pub fn clear_records(&self) {
        self.records.write().clear();
    }

    /// Check if model is allowed
    pub fn is_model_allowed(&self, model: &str) -> bool {
        self.config.allowed_models.is_empty() || self.config.allowed_models.contains(model)
    }

    /// Calculate cost for usage
    pub fn calculate_cost(&self, model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
        let input_cost = self.config.input_costs.get(model).copied().unwrap_or(0.01);
        let output_cost = self.config.output_costs.get(model).copied().unwrap_or(0.03);

        (input_tokens as f64 / 1000.0) * input_cost + (output_tokens as f64 / 1000.0) * output_cost
    }

    /// Record a call
    pub fn record_call(&self, record: LLMCallRecord) {
        self.records.write().push(record);
    }

    /// Update usage
    pub fn update_usage(
        &self,
        execution_id: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> (f64, bool) {
        let cost = self.calculate_cost(model, input_tokens, output_tokens);

        let mut usage = self.usage.write();
        let tracker = usage.entry(execution_id.to_string()).or_default();

        tracker.total_input_tokens += input_tokens;
        tracker.total_output_tokens += output_tokens;
        tracker.total_cost_usd += cost;
        tracker.call_count += 1;

        let limit_exceeded =
            tracker.cost_limit_usd > 0.0 && tracker.total_cost_usd > tracker.cost_limit_usd;

        (cost, limit_exceeded)
    }
}

/// LLM Proxy server
pub struct LLMProxy {
    state: Arc<ProxyState>,
}

impl LLMProxy {
    pub fn new(config: LLMProxyConfig) -> Self {
        Self {
            state: Arc::new(ProxyState::new(config)),
        }
    }

    /// Get the shared state
    pub fn state(&self) -> Arc<ProxyState> {
        self.state.clone()
    }

    /// Create the router
    pub fn router(&self) -> Router {
        Router::new()
            // OpenAI compatible endpoints
            .route("/openai/v1/chat/completions", post(openai_chat_completions))
            .route("/v1/chat/completions", post(openai_chat_completions))
            // Anthropic compatible endpoints
            .route("/anthropic/v1/messages", post(anthropic_messages))
            .route("/v1/messages", post(anthropic_messages))
            // Usage endpoint
            .route("/usage/:execution_id", axum::routing::get(get_usage))
            .with_state(self.state.clone())
    }

    /// Start the proxy server
    pub async fn start(&self) -> Result<(), String> {
        let addr = format!("0.0.0.0:{}", self.state.config.port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("Failed to bind: {}", e))?;

        info!("LLM Proxy listening on {}", addr);

        axum::serve(listener, self.router())
            .await
            .map_err(|e| format!("Server error: {}", e))?;

        Ok(())
    }
}

// ==================== OpenAI Handler ====================

#[derive(Debug, Deserialize)]
struct OpenAIChatRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    temperature: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OpenAIMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct OpenAIChatResponse {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<OpenAIChoice>,
    usage: OpenAIUsage,
}

#[derive(Debug, Serialize)]
struct OpenAIChoice {
    index: u32,
    message: OpenAIMessage,
    finish_reason: String,
}

#[derive(Debug, Serialize)]
struct OpenAIUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    message: String,
    r#type: String,
    code: String,
}

async fn openai_chat_completions(
    State(state): State<Arc<ProxyState>>,
    request: Request<Body>,
) -> impl IntoResponse {
    let start = Instant::now();

    // Get execution ID from header
    let execution_id = request
        .headers()
        .get("X-Execution-ID")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    // Parse body
    let body_bytes = match axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!("Invalid request body: {}", e),
                        r#type: "invalid_request_error".to_string(),
                        code: "invalid_body".to_string(),
                    },
                }),
            )
                .into_response();
        }
    };

    let req: OpenAIChatRequest = match serde_json::from_slice(&body_bytes) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!("Invalid JSON: {}", e),
                        r#type: "invalid_request_error".to_string(),
                        code: "invalid_json".to_string(),
                    },
                }),
            )
                .into_response();
        }
    };

    // Check model whitelist
    if !state.is_model_allowed(&req.model) {
        let record = LLMCallRecord {
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
            model: req.model.clone(),
            provider: "openai".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            latency_ms: start.elapsed().as_millis() as u64,
            allowed: false,
            blocked: true,
            block_reason: Some(format!("Model {} not in whitelist", req.model)),
        };
        state.record_call(record);

        return (
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: ErrorDetail {
                    message: format!("Model {} is not allowed", req.model),
                    r#type: "permission_error".to_string(),
                    code: "model_not_allowed".to_string(),
                },
            }),
        )
            .into_response();
    }

    // Check cost limit
    if let Some(usage) = state.get_usage(&execution_id) {
        if usage.cost_limit_usd > 0.0 && usage.total_cost_usd >= usage.cost_limit_usd {
            let record = LLMCallRecord {
                timestamp: chrono::Utc::now().timestamp_millis() as u64,
                model: req.model.clone(),
                provider: "openai".to_string(),
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0,
                latency_ms: start.elapsed().as_millis() as u64,
                allowed: true,
                blocked: true,
                block_reason: Some("Cost limit exceeded".to_string()),
            };
            state.record_call(record);

            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: "Cost limit exceeded".to_string(),
                        r#type: "cost_error".to_string(),
                        code: "cost_limit_exceeded".to_string(),
                    },
                }),
            )
                .into_response();
        }
    }

    // Forward to OpenAI
    let api_key = match &state.config.openai_api_key {
        Some(k) => k,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: "OpenAI API key not configured".to_string(),
                        r#type: "configuration_error".to_string(),
                        code: "no_api_key".to_string(),
                    },
                }),
            )
                .into_response();
        }
    };

    let response = state
        .http_client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .body(body_bytes.to_vec())
        .send()
        .await;

    let latency = start.elapsed().as_millis() as u64;

    match response {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.bytes().await.unwrap_or_default();

            if status.is_success() {
                // Parse response to get usage
                if let Ok(openai_resp) = serde_json::from_slice::<serde_json::Value>(&body) {
                    let input_tokens = openai_resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
                    let output_tokens = openai_resp["usage"]["completion_tokens"]
                        .as_u64()
                        .unwrap_or(0);

                    let (cost, _) =
                        state.update_usage(&execution_id, &req.model, input_tokens, output_tokens);

                    let record = LLMCallRecord {
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        model: req.model,
                        provider: "openai".to_string(),
                        input_tokens,
                        output_tokens,
                        cost_usd: cost,
                        latency_ms: latency,
                        allowed: true,
                        blocked: false,
                        block_reason: None,
                    };
                    state.record_call(record);
                }

                (StatusCode::OK, body.to_vec()).into_response()
            } else {
                (status, body.to_vec()).into_response()
            }
        }
        Err(e) => {
            error!("OpenAI request failed: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!("Upstream error: {}", e),
                        r#type: "upstream_error".to_string(),
                        code: "upstream_failed".to_string(),
                    },
                }),
            )
                .into_response()
        }
    }
}

// ==================== Anthropic Handler ====================

#[derive(Debug, Deserialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: u64,
    #[serde(default)]
    system: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

async fn anthropic_messages(
    State(state): State<Arc<ProxyState>>,
    request: Request<Body>,
) -> impl IntoResponse {
    let start = Instant::now();

    let execution_id = request
        .headers()
        .get("X-Execution-ID")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    let body_bytes = match axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("Invalid body: {}", e)).into_response();
        }
    };

    let req: AnthropicRequest = match serde_json::from_slice(&body_bytes) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {}", e)).into_response();
        }
    };

    // Check model whitelist
    if !state.is_model_allowed(&req.model) {
        let record = LLMCallRecord {
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
            model: req.model.clone(),
            provider: "anthropic".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            latency_ms: start.elapsed().as_millis() as u64,
            allowed: false,
            blocked: true,
            block_reason: Some(format!("Model {} not allowed", req.model)),
        };
        state.record_call(record);

        return (
            StatusCode::FORBIDDEN,
            format!("Model {} not allowed", req.model),
        )
            .into_response();
    }

    // Check cost limit
    if let Some(usage) = state.get_usage(&execution_id) {
        if usage.cost_limit_usd > 0.0 && usage.total_cost_usd >= usage.cost_limit_usd {
            return (StatusCode::PAYMENT_REQUIRED, "Cost limit exceeded").into_response();
        }
    }

    // Forward to Anthropic
    let api_key = match &state.config.anthropic_api_key {
        Some(k) => k,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Anthropic API key not configured",
            )
                .into_response();
        }
    };

    let response = state
        .http_client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .body(body_bytes.to_vec())
        .send()
        .await;

    let latency = start.elapsed().as_millis() as u64;

    match response {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.bytes().await.unwrap_or_default();

            if status.is_success() {
                if let Ok(anthropic_resp) = serde_json::from_slice::<serde_json::Value>(&body) {
                    let input_tokens = anthropic_resp["usage"]["input_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    let output_tokens = anthropic_resp["usage"]["output_tokens"]
                        .as_u64()
                        .unwrap_or(0);

                    let (cost, _) =
                        state.update_usage(&execution_id, &req.model, input_tokens, output_tokens);

                    let record = LLMCallRecord {
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        model: req.model,
                        provider: "anthropic".to_string(),
                        input_tokens,
                        output_tokens,
                        cost_usd: cost,
                        latency_ms: latency,
                        allowed: true,
                        blocked: false,
                        block_reason: None,
                    };
                    state.record_call(record);
                }

                (StatusCode::OK, body.to_vec()).into_response()
            } else {
                (status, body.to_vec()).into_response()
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e)).into_response(),
    }
}

// ==================== Usage Handler ====================

async fn get_usage(
    State(state): State<Arc<ProxyState>>,
    axum::extract::Path(execution_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match state.get_usage(&execution_id) {
        Some(usage) => Json(LLMUsage {
            total_input_tokens: usage.total_input_tokens,
            total_output_tokens: usage.total_output_tokens,
            total_cost_usd: usage.total_cost_usd,
            call_count: usage.call_count,
            cost_limit_usd: usage.cost_limit_usd,
            remaining_usd: (usage.cost_limit_usd - usage.total_cost_usd).max(0.0),
        })
        .into_response(),
        None => (StatusCode::NOT_FOUND, "Execution not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_whitelist() {
        let config = LLMProxyConfig::default();
        let state = ProxyState::new(config);

        assert!(state.is_model_allowed("gpt-4o"));
        assert!(state.is_model_allowed("claude-3-5-sonnet-20241022"));
        assert!(!state.is_model_allowed("gpt-4-base")); // Not in whitelist
    }

    #[test]
    fn test_cost_calculation() {
        let config = LLMProxyConfig::default();
        let state = ProxyState::new(config);

        // gpt-4o: $0.0025/1K input, $0.01/1K output
        let cost = state.calculate_cost("gpt-4o", 1000, 500);
        assert!((cost - 0.0075).abs() < 0.0001); // 0.0025 + 0.005
    }

    #[test]
    fn test_usage_tracking() {
        let config = LLMProxyConfig::default();
        let state = ProxyState::new(config);

        state.set_limit("exec1", 1.0);

        let (cost1, exceeded1) = state.update_usage("exec1", "gpt-4o", 1000, 500);
        assert!(!exceeded1);

        let usage = state.get_usage("exec1").unwrap();
        assert_eq!(usage.total_input_tokens, 1000);
        assert_eq!(usage.total_output_tokens, 500);
        assert!((usage.total_cost_usd - cost1).abs() < 0.0001);
    }
}
