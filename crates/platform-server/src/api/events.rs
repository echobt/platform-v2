//! Challenge Events API
//!
//! Allows challenges to broadcast custom events to validators via WebSocket.
//! Secured with shared secret to prevent unauthorized broadcasts.

use crate::models::{ChallengeCustomEvent, WsEvent};
use crate::state::AppState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info, warn};

/// Shared secret for challenge event broadcasts (set via BROADCAST_SECRET env var)
/// Challenges must include this in X-Broadcast-Secret header
fn get_broadcast_secret() -> Option<String> {
    std::env::var("BROADCAST_SECRET").ok()
}

#[derive(Debug, Deserialize)]
pub struct BroadcastEventRequest {
    /// Challenge ID (must match a registered challenge)
    pub challenge_id: String,
    /// Event name (e.g., "new_submission", "evaluation_needed")
    pub event_name: String,
    /// Event payload - challenge-specific JSON data
    pub payload: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct BroadcastEventResponse {
    pub success: bool,
    pub connections_notified: usize,
    pub error: Option<String>,
}

/// Constant-time comparison to prevent timing attacks
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// POST /api/v1/events/broadcast - Broadcast a custom challenge event
///
/// Called by challenge containers to notify validators of events.
/// Validators filter events by challenge_id to receive only relevant ones.
///
/// Requires X-Broadcast-Secret header matching BROADCAST_SECRET env var.
/// If BROADCAST_SECRET is not set, rejects requests unless DISABLE_BROADCAST_AUTH is set.
pub async fn broadcast_event(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<BroadcastEventRequest>,
) -> Result<Json<BroadcastEventResponse>, (StatusCode, String)> {
    // Get broadcast secret - REQUIRED unless explicitly disabled
    let expected_secret = match get_broadcast_secret() {
        Some(secret) if !secret.is_empty() => secret,
        _ => {
            // Check if auth is explicitly disabled (for local development only)
            if std::env::var("DISABLE_BROADCAST_AUTH").is_ok() {
                warn!("SECURITY: Broadcast authentication disabled - only use in development!");
                String::new() // Empty string means skip check
            } else {
                error!("BROADCAST_SECRET not set - rejecting broadcast request");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Server misconfigured: BROADCAST_SECRET not set".into(),
                ));
            }
        }
    };

    // Verify secret if required
    if !expected_secret.is_empty() {
        let provided_secret = headers
            .get("X-Broadcast-Secret")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        // Use constant-time comparison to prevent timing attacks
        if !constant_time_eq(provided_secret.as_bytes(), expected_secret.as_bytes()) {
            warn!(
                "Unauthorized broadcast attempt for challenge: {}",
                req.challenge_id
            );
            return Err((StatusCode::UNAUTHORIZED, "Invalid broadcast secret".into()));
        }
    }

    // Create the custom event
    let event = ChallengeCustomEvent {
        challenge_id: req.challenge_id.clone(),
        event_name: req.event_name.clone(),
        payload: req.payload,
        timestamp: chrono::Utc::now().timestamp(),
    };

    // Get connection count before broadcast
    let connections = state.broadcaster.connection_count();

    // Broadcast to all connected clients
    state.broadcaster.broadcast(WsEvent::ChallengeEvent(event));

    info!(
        "Broadcast challenge event: {}:{} to {} connections",
        req.challenge_id, req.event_name, connections
    );

    Ok(Json(BroadcastEventResponse {
        success: true,
        connections_notified: connections,
        error: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // get_broadcast_secret tests
    // =========================================================================

    #[test]
    fn test_get_broadcast_secret_not_set() {
        std::env::remove_var("BROADCAST_SECRET");
        let secret = get_broadcast_secret();
        assert!(secret.is_none());
    }

    #[test]
    fn test_get_broadcast_secret_set() {
        std::env::set_var("BROADCAST_SECRET", "my_test_secret");
        let secret = get_broadcast_secret();
        assert_eq!(secret, Some("my_test_secret".to_string()));
        std::env::remove_var("BROADCAST_SECRET");
    }

    // =========================================================================
    // BroadcastEventRequest tests
    // =========================================================================

    #[test]
    fn test_broadcast_event_request_deserialize() {
        let json = r#"{
            "challenge_id": "term-challenge",
            "event_name": "new_submission",
            "payload": {"task_id": "123", "miner": "5GrwvaEF..."}
        }"#;

        let req: BroadcastEventRequest = serde_json::from_str(json).unwrap();

        assert_eq!(req.challenge_id, "term-challenge");
        assert_eq!(req.event_name, "new_submission");
        assert!(req.payload.get("task_id").is_some());
    }

    #[test]
    fn test_broadcast_event_request_empty_payload() {
        let json = r#"{
            "challenge_id": "test",
            "event_name": "ping",
            "payload": {}
        }"#;

        let req: BroadcastEventRequest = serde_json::from_str(json).unwrap();

        assert!(req.payload.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_broadcast_event_request_complex_payload() {
        let json = r#"{
            "challenge_id": "term-challenge",
            "event_name": "evaluation_complete",
            "payload": {
                "results": [
                    {"task": 1, "passed": true},
                    {"task": 2, "passed": false}
                ],
                "total_score": 0.85,
                "metadata": {"version": "1.0"}
            }
        }"#;

        let req: BroadcastEventRequest = serde_json::from_str(json).unwrap();

        assert!(req.payload.get("results").unwrap().is_array());
        assert_eq!(req.payload.get("total_score").unwrap().as_f64(), Some(0.85));
    }

    // =========================================================================
    // BroadcastEventResponse tests
    // =========================================================================

    #[test]
    fn test_broadcast_event_response_success() {
        let response = BroadcastEventResponse {
            success: true,
            connections_notified: 10,
            error: None,
        };

        let json = serde_json::to_string(&response).unwrap();

        assert!(json.contains("true"));
        assert!(json.contains("10"));
        assert!(!json.contains("error\":\""));
    }

    #[test]
    fn test_broadcast_event_response_with_error() {
        let response = BroadcastEventResponse {
            success: false,
            connections_notified: 0,
            error: Some("Authentication failed".to_string()),
        };

        let json = serde_json::to_string(&response).unwrap();

        assert!(json.contains("false"));
        assert!(json.contains("Authentication failed"));
    }

    // =========================================================================
    // Header secret extraction tests
    // =========================================================================

    #[test]
    fn test_header_secret_extraction() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Broadcast-Secret", "my_secret".parse().unwrap());

        let secret = headers
            .get("X-Broadcast-Secret")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        assert_eq!(secret, "my_secret");
    }

    #[test]
    fn test_header_secret_missing() {
        let headers = HeaderMap::new();

        let secret = headers
            .get("X-Broadcast-Secret")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        assert_eq!(secret, "");
    }

    #[test]
    fn test_secret_comparison() {
        let expected = "secret123";
        let provided = "secret123";
        let wrong = "wrong_secret";

        assert_eq!(expected, provided);
        assert_ne!(expected, wrong);
    }

    // =========================================================================
    // ChallengeCustomEvent construction tests
    // =========================================================================

    #[test]
    fn test_challenge_custom_event_from_request() {
        let req = BroadcastEventRequest {
            challenge_id: "test-challenge".to_string(),
            event_name: "test_event".to_string(),
            payload: serde_json::json!({"key": "value"}),
        };

        let event = ChallengeCustomEvent {
            challenge_id: req.challenge_id.clone(),
            event_name: req.event_name.clone(),
            payload: req.payload.clone(),
            timestamp: chrono::Utc::now().timestamp(),
        };

        assert_eq!(event.challenge_id, "test-challenge");
        assert_eq!(event.event_name, "test_event");
        assert!(event.timestamp > 0);
    }

    #[test]
    fn test_ws_event_wrapping() {
        let custom_event = ChallengeCustomEvent {
            challenge_id: "test".to_string(),
            event_name: "ping".to_string(),
            payload: serde_json::json!({}),
            timestamp: 1234567890,
        };

        let ws_event = WsEvent::ChallengeEvent(custom_event);

        // Should serialize properly
        let json = serde_json::to_string(&ws_event).unwrap();
        assert!(json.contains("challenge_event"));
    }
}
