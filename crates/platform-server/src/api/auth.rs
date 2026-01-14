//! Authentication API handlers

use crate::models::*;
use crate::state::AppState;
use axum::{extract::State, http::StatusCode, Json};
use sp_core::{crypto::Pair as _, crypto::Ss58Codec, sr25519};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use uuid::Uuid;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

pub async fn authenticate(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AuthRequest>,
) -> Result<Json<AuthResponse>, (StatusCode, Json<AuthResponse>)> {
    let current_time = now();
    if (current_time - req.timestamp).abs() > 300 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(AuthResponse {
                success: false,
                token: None,
                expires_at: None,
                error: Some("Timestamp too old or in future".to_string()),
            }),
        ));
    }

    let message = format!("auth:{}:{}:{:?}", req.hotkey, req.timestamp, req.role);

    if !verify_signature(&req.hotkey, &message, &req.signature) {
        warn!("Invalid signature for auth request from {}", req.hotkey);
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(AuthResponse {
                success: false,
                token: None,
                expires_at: None,
                error: Some("Invalid signature".to_string()),
            }),
        ));
    }

    if req.role == AuthRole::Owner && !state.is_owner(&req.hotkey) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(AuthResponse {
                success: false,
                token: None,
                expires_at: None,
                error: Some("Not authorized as owner".to_string()),
            }),
        ));
    }

    let token = Uuid::new_v4().to_string();
    let expires_at = current_time + 3600;

    let session = AuthSession {
        hotkey: req.hotkey.clone(),
        role: req.role.clone(),
        expires_at,
    };
    state.sessions.insert(token.clone(), session);

    if req.role == AuthRole::Validator {
        let _ = crate::db::queries::upsert_validator(&state.db, &req.hotkey, 0).await;
    }

    info!("Authenticated {} as {:?}", req.hotkey, req.role);

    Ok(Json(AuthResponse {
        success: true,
        token: Some(token),
        expires_at: Some(expires_at),
        error: None,
    }))
}

/// Verify an sr25519 signature from a hotkey
///
/// # Arguments
/// * `hotkey_ss58` - SS58 encoded hotkey (e.g., "5GrwvaEF...")
/// * `message` - The message that was signed
/// * `signature_hex` - Hex-encoded 64-byte sr25519 signature
///
/// # Returns
/// true if signature is valid, false otherwise
pub fn verify_signature(hotkey_ss58: &str, message: &str, signature_hex: &str) -> bool {
    // Parse hotkey from SS58
    let public = match sr25519::Public::from_ss58check(hotkey_ss58) {
        Ok(p) => p,
        Err(e) => {
            warn!("Invalid hotkey SS58 format: {} - {:?}", hotkey_ss58, e);
            return false;
        }
    };

    // Parse signature from hex
    let signature_bytes = match hex::decode(signature_hex) {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!("Invalid signature hex format: {:?}", e);
            return false;
        }
    };

    if signature_bytes.len() != 64 {
        warn!(
            "Invalid signature length: {} (expected 64)",
            signature_bytes.len()
        );
        return false;
    }

    // Convert to sr25519 signature
    let mut sig_bytes = [0u8; 64];
    sig_bytes.copy_from_slice(&signature_bytes);
    let signature = sr25519::Signature::from_raw(sig_bytes);

    // Verify signature
    let is_valid = sr25519::Pair::verify(&signature, message.as_bytes(), &public);

    if !is_valid {
        warn!(
            "Signature verification failed for hotkey: {}",
            &hotkey_ss58[..16.min(hotkey_ss58.len())]
        );
    }

    is_valid
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::Pair;

    /// Helper to generate a valid sr25519 keypair for testing
    fn generate_test_keypair() -> sr25519::Pair {
        sr25519::Pair::generate().0
    }

    /// Helper to get SS58 address from keypair
    fn get_ss58_address(pair: &sr25519::Pair) -> String {
        use sp_core::crypto::Ss58Codec;
        pair.public().to_ss58check()
    }

    /// Helper to sign a message and return hex-encoded signature
    fn sign_message(pair: &sr25519::Pair, message: &str) -> String {
        let signature = pair.sign(message.as_bytes());
        hex::encode(signature.0)
    }

    // =========================================================================
    // verify_signature tests
    // =========================================================================

    #[test]
    fn test_verify_signature_valid() {
        let pair = generate_test_keypair();
        let hotkey = get_ss58_address(&pair);
        let message = "test message for signing";
        let signature = sign_message(&pair, message);

        assert!(
            verify_signature(&hotkey, message, &signature),
            "Valid signature should be verified successfully"
        );
    }

    #[test]
    fn test_verify_signature_invalid_signature() {
        let pair = generate_test_keypair();
        let hotkey = get_ss58_address(&pair);
        let message = "test message";

        // Create a different keypair and sign with it
        let other_pair = generate_test_keypair();
        let wrong_signature = sign_message(&other_pair, message);

        assert!(
            !verify_signature(&hotkey, message, &wrong_signature),
            "Signature from different key should be rejected"
        );
    }

    #[test]
    fn test_verify_signature_wrong_message() {
        let pair = generate_test_keypair();
        let hotkey = get_ss58_address(&pair);
        let original_message = "original message";
        let signature = sign_message(&pair, original_message);

        // Try to verify with a different message
        let tampered_message = "tampered message";
        assert!(
            !verify_signature(&hotkey, tampered_message, &signature),
            "Signature for different message should be rejected"
        );
    }

    #[test]
    fn test_verify_signature_invalid_hotkey_format() {
        let pair = generate_test_keypair();
        let message = "test message";
        let signature = sign_message(&pair, message);

        // Invalid SS58 format
        assert!(
            !verify_signature("invalid_hotkey_format", message, &signature),
            "Invalid hotkey format should be rejected"
        );
    }

    #[test]
    fn test_verify_signature_invalid_hex_signature() {
        let pair = generate_test_keypair();
        let hotkey = get_ss58_address(&pair);
        let message = "test message";

        // Not valid hex
        assert!(
            !verify_signature(&hotkey, message, "not_valid_hex_zzz"),
            "Invalid hex signature should be rejected"
        );
    }

    #[test]
    fn test_verify_signature_wrong_length() {
        let pair = generate_test_keypair();
        let hotkey = get_ss58_address(&pair);
        let message = "test message";

        // Valid hex but wrong length (should be 64 bytes = 128 hex chars)
        let short_signature = "abcd1234";
        assert!(
            !verify_signature(&hotkey, message, short_signature),
            "Signature with wrong length should be rejected"
        );

        // Too long signature
        let long_signature = "a".repeat(256);
        assert!(
            !verify_signature(&hotkey, message, &long_signature),
            "Signature that's too long should be rejected"
        );
    }

    #[test]
    fn test_verify_signature_empty_message() {
        let pair = generate_test_keypair();
        let hotkey = get_ss58_address(&pair);
        let message = "";
        let signature = sign_message(&pair, message);

        assert!(
            verify_signature(&hotkey, message, &signature),
            "Empty message should still be signable and verifiable"
        );
    }

    #[test]
    fn test_verify_signature_unicode_message() {
        let pair = generate_test_keypair();
        let hotkey = get_ss58_address(&pair);
        let message = "Hello 世界 🌍 مرحبا";
        let signature = sign_message(&pair, message);

        assert!(
            verify_signature(&hotkey, message, &signature),
            "Unicode messages should be handled correctly"
        );
    }

    #[test]
    fn test_verify_signature_auth_message_format() {
        // Test the exact message format used in authenticate()
        let pair = generate_test_keypair();
        let hotkey = get_ss58_address(&pair);
        let timestamp = 1234567890i64;
        let role = AuthRole::Validator;

        let message = format!("auth:{}:{}:{:?}", hotkey, timestamp, role);
        let signature = sign_message(&pair, &message);

        assert!(
            verify_signature(&hotkey, &message, &signature),
            "Auth message format should be verifiable"
        );
    }

    #[test]
    fn test_verify_signature_ws_connect_format() {
        // Test the message format used in WebSocket handler
        let pair = generate_test_keypair();
        let hotkey = get_ss58_address(&pair);
        let timestamp = 1234567890i64;

        let message = format!("ws_connect:{}:{}", hotkey, timestamp);
        let signature = sign_message(&pair, &message);

        assert!(
            verify_signature(&hotkey, &message, &signature),
            "WS connect message format should be verifiable"
        );
    }

    #[test]
    fn test_verify_signature_case_sensitive_hotkey() {
        let pair = generate_test_keypair();
        let hotkey = get_ss58_address(&pair);
        let message = "test message";
        let signature = sign_message(&pair, message);

        // Modify case of hotkey (SS58 is case-sensitive)
        let modified_hotkey = if hotkey.chars().next().unwrap().is_uppercase() {
            hotkey.to_lowercase()
        } else {
            hotkey.to_uppercase()
        };

        assert!(
            !verify_signature(&modified_hotkey, message, &signature),
            "Modified case hotkey should be rejected"
        );
    }

    #[test]
    fn test_verify_signature_all_zeros() {
        let pair = generate_test_keypair();
        let hotkey = get_ss58_address(&pair);
        let message = "test message";

        // 64 bytes of zeros
        let zero_signature = "0".repeat(128);
        assert!(
            !verify_signature(&hotkey, message, &zero_signature),
            "All-zero signature should be rejected"
        );
    }

    // =========================================================================
    // now() helper function tests
    // =========================================================================

    #[test]
    fn test_now_returns_reasonable_timestamp() {
        let timestamp = now();

        // Should be after year 2020 (1577836800)
        assert!(timestamp > 1577836800, "Timestamp should be after 2020");

        // Should be before year 2100 (4102444800)
        assert!(timestamp < 4102444800, "Timestamp should be before 2100");
    }

    #[test]
    fn test_now_is_monotonic() {
        let t1 = now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let t2 = now();

        assert!(t2 >= t1, "Time should not go backwards");
    }
}
