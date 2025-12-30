//! Bridge API - Proxy submissions to term-challenge
//!
//! This module provides bridge endpoints that proxy requests to the
//! term-challenge container for the centralized submission flow.

use crate::state::AppState;
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Default challenge ID for term-challenge
const TERM_CHALLENGE_ID: &str = "term-challenge";

/// Get the term-challenge endpoint URL from environment or challenge manager
fn get_term_challenge_url(state: &AppState) -> Option<String> {
    // First check environment variable
    if let Ok(url) = std::env::var("TERM_CHALLENGE_URL") {
        return Some(url);
    }

    // Then check challenge manager
    if let Some(ref manager) = state.challenge_manager {
        // Try multiple possible IDs
        for id in &[
            TERM_CHALLENGE_ID,
            "term-challenge-server",
            "42b0dd5c-894f-3281-136a-ce34a0971d9f",
        ] {
            if let Some(endpoint) = manager.get_endpoint(id) {
                return Some(endpoint);
            }
        }
    }

    None
}

/// Proxy a request to term-challenge
async fn proxy_to_term_challenge(state: &AppState, path: &str, request: Request<Body>) -> Response {
    let base_url = match get_term_challenge_url(state) {
        Some(url) => url,
        None => {
            warn!("Term-challenge not available - no endpoint configured");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Term-challenge service not available. Configure TERM_CHALLENGE_URL or ensure challenge is running.",
            )
                .into_response();
        }
    };

    let url = format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    debug!("Proxying to term-challenge: {}", url);

    let method = request.method().clone();
    let headers = request.headers().clone();

    let body_bytes = match axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return (StatusCode::BAD_REQUEST, "Failed to read request body").into_response();
        }
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap();

    let mut req_builder = client.request(method, &url);
    for (key, value) in headers.iter() {
        if key != "host" && key != "content-length" {
            req_builder = req_builder.header(key, value);
        }
    }

    if !body_bytes.is_empty() {
        req_builder = req_builder.body(body_bytes.to_vec());
    }

    match req_builder.send().await {
        Ok(resp) => {
            let status = resp.status();
            let headers = resp.headers().clone();

            match resp.bytes().await {
                Ok(body) => {
                    let mut response = Response::builder().status(status);
                    for (key, value) in headers.iter() {
                        response = response.header(key, value);
                    }
                    response
                        .body(Body::from(body))
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
                }
                Err(e) => {
                    error!("Failed to read response: {}", e);
                    StatusCode::BAD_GATEWAY.into_response()
                }
            }
        }
        Err(e) => {
            if e.is_timeout() {
                (StatusCode::GATEWAY_TIMEOUT, "Term-challenge timeout").into_response()
            } else if e.is_connect() {
                warn!("Cannot connect to term-challenge at {}: {}", base_url, e);
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Term-challenge not reachable",
                )
                    .into_response()
            } else {
                error!("Proxy error: {}", e);
                (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response()
            }
        }
    }
}

/// POST /api/v1/submit - Bridge to term-challenge submit endpoint
pub async fn bridge_submit(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    info!("Bridging submit request to term-challenge");
    proxy_to_term_challenge(&state, "/api/v1/submit", request).await
}

/// GET /api/v1/challenge/leaderboard - Bridge to term-challenge leaderboard
pub async fn bridge_leaderboard(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response {
    proxy_to_term_challenge(&state, "/api/v1/leaderboard", request).await
}

/// GET /api/v1/challenge/status - Bridge to term-challenge status
pub async fn bridge_status(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    proxy_to_term_challenge(&state, "/api/v1/status", request).await
}

/// POST /api/v1/my/agents - Bridge to term-challenge my agents
pub async fn bridge_my_agents(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response {
    proxy_to_term_challenge(&state, "/api/v1/my/agents", request).await
}

/// POST /api/v1/my/agents/:hash/source - Bridge to term-challenge source
pub async fn bridge_my_agent_source(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(hash): axum::extract::Path<String>,
    request: Request<Body>,
) -> Response {
    let path = format!("/api/v1/my/agents/{}/source", hash);
    proxy_to_term_challenge(&state, &path, request).await
}

/// POST /api/v1/validator/claim_job - Bridge to term-challenge validator claim
pub async fn bridge_validator_claim(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response {
    proxy_to_term_challenge(&state, "/api/v1/validator/claim_job", request).await
}

/// POST /api/v1/validator/complete_job - Bridge to term-challenge validator complete
pub async fn bridge_validator_complete(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response {
    proxy_to_term_challenge(&state, "/api/v1/validator/complete_job", request).await
}
