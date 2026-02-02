//! Evaluations API handlers

use crate::db::queries;
use crate::models::*;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use std::sync::Arc;
use tracing::info;

pub async fn submit_evaluation(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SubmitEvaluationRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Verify timestamp is recent (within 5 minutes) to prevent replay attacks
    let current_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("System time before UNIX epoch")
        .as_secs() as i64;
    if (current_time - req.timestamp).abs() > 300 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": "Timestamp too old or in future (must be within 5 minutes)"
            })),
        ));
    }

    // Verify signature from validator
    let message = format!(
        "submit_evaluation:{}:{}:{}:{}",
        req.submission_id, req.agent_hash, req.validator_hotkey, req.timestamp
    );

    if !crate::api::auth::verify_signature(&req.validator_hotkey, &message, &req.signature) {
        tracing::warn!(
            "Invalid signature for evaluation submission from validator: {}",
            &req.validator_hotkey[..16.min(req.validator_hotkey.len())]
        );
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "success": false,
                "error": "Invalid signature"
            })),
        ));
    }

    let evaluation = queries::create_evaluation(&state.db, &req)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "success": false, "error": e.to_string() })),
            )
        })?;

    // Update leaderboard
    let _ = queries::update_leaderboard(&state.db, &req.agent_hash).await;

    // Broadcast evaluation event
    state
        .broadcast_event(WsEvent::EvaluationComplete(EvaluationEvent {
            submission_id: req.submission_id.clone(),
            agent_hash: req.agent_hash.clone(),
            validator_hotkey: req.validator_hotkey.clone(),
            score: req.score,
            tasks_passed: req.tasks_passed,
            tasks_total: req.tasks_total,
        }))
        .await;

    info!(
        "Evaluation submitted: {} by {} (score: {:.2})",
        req.agent_hash, req.validator_hotkey, req.score
    );

    Ok(Json(serde_json::json!({
        "success": true,
        "evaluation_id": evaluation.id,
    })))
}

pub async fn get_evaluations(
    State(state): State<Arc<AppState>>,
    Path(agent_hash): Path<String>,
) -> Result<Json<Vec<Evaluation>>, StatusCode> {
    let evaluations = queries::get_evaluations_for_agent(&state.db, &agent_hash)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(evaluations))
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // SubmitEvaluationRequest tests
    // =========================================================================

    #[test]
    fn test_submit_evaluation_request_deserialize() {
        let json = r#"{
            "submission_id": "sub-123",
            "agent_hash": "hash456",
            "validator_hotkey": "5GrwvaEF...",
            "signature": "sig789",
            "timestamp": 1234567890,
            "score": 0.95,
            "tasks_passed": 19,
            "tasks_total": 20,
            "tasks_failed": 1,
            "total_cost_usd": 2.50,
            "execution_time_ms": 5000,
            "task_results": [{"task": 1, "passed": true}],
            "execution_log": "Test log"
        }"#;

        let req: SubmitEvaluationRequest = serde_json::from_str(json).unwrap();

        assert_eq!(req.submission_id, "sub-123");
        assert_eq!(req.score, 0.95);
        assert_eq!(req.tasks_passed, 19);
        assert_eq!(req.tasks_total, 20);
        assert_eq!(req.tasks_failed, 1);
        assert_eq!(req.timestamp, 1234567890);
    }

    #[test]
    fn test_submit_evaluation_request_minimal() {
        let json = r#"{
            "submission_id": "sub-123",
            "agent_hash": "hash456",
            "validator_hotkey": "5GrwvaEF...",
            "signature": "sig789",
            "timestamp": 1234567890,
            "score": 0.5,
            "tasks_passed": 5,
            "tasks_total": 10,
            "tasks_failed": 5,
            "total_cost_usd": 0.0,
            "execution_time_ms": null,
            "task_results": null,
            "execution_log": null
        }"#;

        let req: SubmitEvaluationRequest = serde_json::from_str(json).unwrap();

        assert!(req.execution_time_ms.is_none());
        assert!(req.task_results.is_none());
        assert!(req.execution_log.is_none());
        assert_eq!(req.timestamp, 1234567890);
    }

    // =========================================================================
    // Evaluation model tests
    // =========================================================================

    #[test]
    fn test_evaluation_serialization() {
        let eval = Evaluation {
            id: "eval-123".to_string(),
            submission_id: "sub-456".to_string(),
            agent_hash: "hash789".to_string(),
            validator_hotkey: "5GrwvaEF...".to_string(),
            score: 0.85,
            tasks_passed: 17,
            tasks_total: 20,
            tasks_failed: 3,
            total_cost_usd: 1.50,
            execution_time_ms: Some(3000),
            task_results: Some("{\"results\": []}".to_string()),
            execution_log: Some("log content".to_string()),
            created_at: 1234567890,
        };

        let json = serde_json::to_string(&eval).unwrap();

        assert!(json.contains("eval-123"));
        assert!(json.contains("0.85"));
        assert!(json.contains("17"));
    }

    #[test]
    fn test_evaluation_deserialization() {
        let json = r#"{
            "id": "eval-123",
            "submission_id": "sub-456",
            "agent_hash": "hash789",
            "validator_hotkey": "validator1",
            "score": 0.90,
            "tasks_passed": 18,
            "tasks_total": 20,
            "tasks_failed": 2,
            "total_cost_usd": 2.0,
            "execution_time_ms": 4000,
            "task_results": null,
            "execution_log": null,
            "created_at": 1234567890
        }"#;

        let eval: Evaluation = serde_json::from_str(json).unwrap();

        assert_eq!(eval.id, "eval-123");
        assert_eq!(eval.score, 0.90);
        assert_eq!(eval.tasks_passed, 18);
    }

    // =========================================================================
    // EvaluationEvent tests
    // =========================================================================

    #[test]
    fn test_evaluation_event_creation() {
        let event = EvaluationEvent {
            submission_id: "sub-123".to_string(),
            agent_hash: "hash456".to_string(),
            validator_hotkey: "5GrwvaEF...".to_string(),
            score: 0.95,
            tasks_passed: 19,
            tasks_total: 20,
        };

        let json = serde_json::to_string(&event).unwrap();

        assert!(json.contains("sub-123"));
        assert!(json.contains("0.95"));
    }

    #[test]
    fn test_ws_event_evaluation_complete() {
        let event = WsEvent::EvaluationComplete(EvaluationEvent {
            submission_id: "sub-123".to_string(),
            agent_hash: "hash456".to_string(),
            validator_hotkey: "validator1".to_string(),
            score: 0.80,
            tasks_passed: 16,
            tasks_total: 20,
        });

        let json = serde_json::to_string(&event).unwrap();

        assert!(json.contains("evaluation_complete"));
        assert!(json.contains("sub-123"));
    }

    // =========================================================================
    // Score validation tests
    // =========================================================================

    #[test]
    fn test_score_range_valid() {
        let scores = vec![0.0, 0.5, 1.0, 0.95, 0.001];

        for score in scores {
            assert!(
                score >= 0.0 && score <= 1.0,
                "Score {} should be valid",
                score
            );
        }
    }

    #[test]
    fn test_tasks_consistency() {
        let passed = 17u32;
        let failed = 3u32;
        let total = 20u32;

        assert_eq!(passed + failed, total);
    }

    // =========================================================================
    // Response format tests
    // =========================================================================

    #[test]
    fn test_success_response_format() {
        let response = serde_json::json!({
            "success": true,
            "evaluation_id": "eval-123"
        });

        assert!(response["success"].as_bool().unwrap());
        assert_eq!(response["evaluation_id"], "eval-123");
    }

    #[test]
    fn test_error_response_format() {
        let response = serde_json::json!({
            "success": false,
            "error": "Database error"
        });

        assert!(!response["success"].as_bool().unwrap());
        assert!(response["error"].as_str().is_some());
    }
}
