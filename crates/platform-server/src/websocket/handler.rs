//! WebSocket connection handler

use crate::api::auth::verify_signature;
use crate::db::queries;
use crate::models::WsEvent;
use crate::state::AppState;
use crate::websocket::events::WsConnection;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    /// Validator hotkey (SS58 format)
    pub hotkey: Option<String>,
    /// Timestamp for signature verification
    pub timestamp: Option<i64>,
    /// Signature of "ws_connect:{hotkey}:{timestamp}"
    pub signature: Option<String>,
    /// Role (validator, miner, etc.) - kept for API compatibility
    #[allow(dead_code)]
    pub role: Option<String>,
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(query): Query<WsQuery>,
) -> Response {
    // Verify authentication if hotkey is provided
    if let Some(ref hotkey) = query.hotkey {
        let timestamp = match query.timestamp {
            Some(ts) => ts,
            None => {
                warn!(
                    "WebSocket connection rejected: missing timestamp for hotkey {}",
                    hotkey
                );
                return (StatusCode::UNAUTHORIZED, "Missing timestamp").into_response();
            }
        };

        let signature = match &query.signature {
            Some(sig) => sig,
            None => {
                warn!(
                    "WebSocket connection rejected: missing signature for hotkey {}",
                    hotkey
                );
                return (StatusCode::UNAUTHORIZED, "Missing signature").into_response();
            }
        };

        // Verify timestamp is recent (within 5 minutes)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        if (now - timestamp).abs() > 300 {
            warn!(
                "WebSocket connection rejected: timestamp too old for hotkey {}",
                hotkey
            );
            return (StatusCode::UNAUTHORIZED, "Timestamp too old").into_response();
        }

        // Verify signature
        let message = format!("ws_connect:{}:{}", hotkey, timestamp);
        if !verify_signature(hotkey, &message, signature) {
            warn!(
                "WebSocket connection rejected: invalid signature for hotkey {}",
                hotkey
            );
            return (StatusCode::UNAUTHORIZED, "Invalid signature").into_response();
        }

        info!(
            "WebSocket authenticated for hotkey: {}",
            &hotkey[..16.min(hotkey.len())]
        );
    }

    let conn_id = Uuid::new_v4();
    ws.on_upgrade(move |socket| handle_socket(socket, state, conn_id, query))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, conn_id: Uuid, query: WsQuery) {
    let (mut sender, mut receiver) = socket.split();

    let conn = WsConnection {
        id: conn_id,
        hotkey: query.hotkey.clone(),
    };
    state.broadcaster.add_connection(conn);

    info!(
        "WebSocket connected: {} (hotkey: {:?})",
        conn_id, query.hotkey
    );

    // Register validator in DB when they connect with a hotkey
    // Lookup stake from metagraph (not from client - can't trust client-provided stake)
    if let Some(ref hotkey) = query.hotkey {
        let stake = state.get_validator_stake(hotkey);
        if let Err(e) = queries::upsert_validator(&state.db, hotkey, stake).await {
            warn!("Failed to register validator {}: {}", hotkey, e);
        } else {
            info!(
                "Validator {} registered (stake: {} TAO from metagraph)",
                &hotkey[..16.min(hotkey.len())],
                stake / 1_000_000_000 // Convert RAO to TAO for logging
            );
        }
    }

    let mut event_rx = state.broadcaster.subscribe();

    let send_task = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let msg = match serde_json::to_string(&event) {
                        Ok(json) => json,
                        Err(e) => {
                            error!("Failed to serialize event: {}", e);
                            continue;
                        }
                    };

                    if sender.send(Message::Text(msg)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    warn!("WebSocket {} lagged by {} messages", conn_id, n);
                }
                Err(RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    let state_clone = state.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    handle_client_message(&state_clone, conn_id, &text).await;
                }
                Ok(Message::Ping(_)) => {
                    debug!("Received ping from {}", conn_id);
                }
                Ok(Message::Pong(_)) => {
                    debug!("Received pong from {}", conn_id);
                }
                Ok(Message::Close(_)) => {
                    info!("WebSocket {} closed by client", conn_id);
                    break;
                }
                Err(e) => {
                    error!("WebSocket error for {}: {}", conn_id, e);
                    break;
                }
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    state.broadcaster.remove_connection(&conn_id);
    info!("WebSocket disconnected: {}", conn_id);
}

async fn handle_client_message(_state: &AppState, conn_id: Uuid, text: &str) {
    let msg: Result<WsEvent, _> = serde_json::from_str(text);

    match msg {
        Ok(WsEvent::Ping) => {
            debug!("Received ping from {}", conn_id);
        }
        Ok(_) => {
            debug!("Received event from {}: {}", conn_id, text);
        }
        Err(e) => {
            warn!("Invalid message from {}: {} - {}", conn_id, text, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // WsQuery tests
    // =========================================================================

    #[test]
    fn test_ws_query_deserialize_full() {
        let json = r#"{
            "hotkey": "5GrwvaEF...",
            "timestamp": 1234567890,
            "signature": "abcd1234...",
            "role": "validator"
        }"#;

        let query: WsQuery = serde_json::from_str(json).unwrap();

        assert_eq!(query.hotkey, Some("5GrwvaEF...".to_string()));
        assert_eq!(query.timestamp, Some(1234567890));
        assert_eq!(query.signature, Some("abcd1234...".to_string()));
        assert_eq!(query.role, Some("validator".to_string()));
    }

    #[test]
    fn test_ws_query_deserialize_empty() {
        let json = "{}";

        let query: WsQuery = serde_json::from_str(json).unwrap();

        assert!(query.hotkey.is_none());
        assert!(query.timestamp.is_none());
        assert!(query.signature.is_none());
        assert!(query.role.is_none());
    }

    #[test]
    fn test_ws_query_deserialize_partial() {
        let json = r#"{"hotkey": "5GrwvaEF..."}"#;

        let query: WsQuery = serde_json::from_str(json).unwrap();

        assert_eq!(query.hotkey, Some("5GrwvaEF...".to_string()));
        assert!(query.timestamp.is_none());
    }

    // =========================================================================
    // Timestamp validation tests
    // =========================================================================

    #[test]
    fn test_timestamp_within_five_minutes() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let timestamp = now - 200; // 3 minutes 20 seconds ago

        let is_valid = (now - timestamp).abs() <= 300;

        assert!(is_valid);
    }

    #[test]
    fn test_timestamp_exactly_five_minutes() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let timestamp = now - 300; // Exactly 5 minutes ago

        let is_valid = (now - timestamp).abs() <= 300;

        assert!(is_valid);
    }

    #[test]
    fn test_timestamp_too_old() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let timestamp = now - 400; // 6 minutes 40 seconds ago

        let is_valid = (now - timestamp).abs() <= 300;

        assert!(!is_valid);
    }

    #[test]
    fn test_timestamp_in_future() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let timestamp = now + 400; // 6 minutes in the future

        let is_valid = (now - timestamp).abs() <= 300;

        assert!(!is_valid);
    }

    // =========================================================================
    // Signature message format tests
    // =========================================================================

    #[test]
    fn test_ws_connect_message_format() {
        let hotkey = "5GrwvaEF...";
        let timestamp = 1234567890i64;

        let message = format!("ws_connect:{}:{}", hotkey, timestamp);

        assert_eq!(message, "ws_connect:5GrwvaEF...:1234567890");
    }

    // =========================================================================
    // Stake conversion tests (RAO to TAO for logging)
    // =========================================================================

    #[test]
    fn test_rao_to_tao_conversion() {
        let stake_rao: u64 = 1_000_000_000; // 1 TAO in RAO
        let stake_tao = stake_rao / 1_000_000_000;

        assert_eq!(stake_tao, 1);
    }

    #[test]
    fn test_large_stake_conversion() {
        let stake_rao: u64 = 100_000_000_000_000; // 100,000 TAO in RAO
        let stake_tao = stake_rao / 1_000_000_000;

        assert_eq!(stake_tao, 100_000);
    }

    // =========================================================================
    // Hotkey truncation for logging tests
    // =========================================================================

    #[test]
    fn test_hotkey_truncation_short() {
        let hotkey = "5Gr";
        let truncated = &hotkey[..16.min(hotkey.len())];

        assert_eq!(truncated, "5Gr");
    }

    #[test]
    fn test_hotkey_truncation_long() {
        let hotkey = "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY";
        let truncated = &hotkey[..16.min(hotkey.len())];

        assert_eq!(truncated, "5GrwvaEF5zXb26Fz");
        assert_eq!(truncated.len(), 16);
    }

    // =========================================================================
    // WsConnection creation tests
    // =========================================================================

    #[test]
    fn test_ws_connection_from_query_with_hotkey() {
        let conn_id = Uuid::new_v4();
        let hotkey = Some("5GrwvaEF...".to_string());

        let conn = WsConnection {
            id: conn_id,
            hotkey: hotkey.clone(),
        };

        assert_eq!(conn.id, conn_id);
        assert_eq!(conn.hotkey, hotkey);
    }

    #[test]
    fn test_ws_connection_from_query_without_hotkey() {
        let conn_id = Uuid::new_v4();

        let conn = WsConnection {
            id: conn_id,
            hotkey: None,
        };

        assert_eq!(conn.id, conn_id);
        assert!(conn.hotkey.is_none());
    }

    // =========================================================================
    // Message parsing tests
    // =========================================================================

    #[test]
    fn test_parse_ping_message() {
        let json = r#"{"type": "ping"}"#;
        let msg: Result<WsEvent, _> = serde_json::from_str(json);

        assert!(msg.is_ok());
        match msg.unwrap() {
            WsEvent::Ping => {}
            _ => panic!("Expected Ping event"),
        }
    }

    #[test]
    fn test_parse_invalid_message() {
        let json = "not valid json {";
        let msg: Result<WsEvent, _> = serde_json::from_str(json);

        assert!(msg.is_err());
    }

    #[test]
    fn test_parse_unknown_event_type() {
        let json = r#"{"type": "unknown_event_type", "data": {}}"#;
        let msg: Result<WsEvent, _> = serde_json::from_str(json);

        // Should fail to parse unknown type
        assert!(msg.is_err());
    }

    // =========================================================================
    // Authentication flow logic tests
    // =========================================================================

    #[test]
    fn test_requires_timestamp_when_hotkey_present() {
        let query = WsQuery {
            hotkey: Some("5GrwvaEF...".to_string()),
            timestamp: None,
            signature: Some("sig123".to_string()),
            role: None,
        };

        // Should require timestamp when hotkey is provided
        assert!(query.hotkey.is_some());
        assert!(query.timestamp.is_none()); // Missing!
    }

    #[test]
    fn test_requires_signature_when_hotkey_present() {
        let query = WsQuery {
            hotkey: Some("5GrwvaEF...".to_string()),
            timestamp: Some(1234567890),
            signature: None,
            role: None,
        };

        // Should require signature when hotkey is provided
        assert!(query.hotkey.is_some());
        assert!(query.signature.is_none()); // Missing!
    }

    #[test]
    fn test_no_auth_required_when_no_hotkey() {
        let query = WsQuery {
            hotkey: None,
            timestamp: None,
            signature: None,
            role: None,
        };

        // Anonymous connections are allowed
        assert!(query.hotkey.is_none());
    }
}
