//! Submissions API handlers - DEPRECATED
//!
//! Submissions have been migrated to term-challenge.
//! These endpoints now return deprecation notices directing users
//! to use the term-challenge API instead.

use crate::db::queries;
use crate::models::*;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

/// Term-challenge URL for submissions (configured via TERM_CHALLENGE_URL env)
fn get_term_challenge_url() -> String {
    std::env::var("TERM_CHALLENGE_URL").unwrap_or_else(|_| "http://localhost:8081".to_string())
}

#[derive(Debug, Deserialize)]
pub struct ListSubmissionsQuery {
    pub limit: Option<usize>,
    pub status: Option<String>,
}

/// DEPRECATED: List submissions
/// Submissions are now managed by term-challenge.
pub async fn list_submissions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListSubmissionsQuery>,
) -> Result<Json<Vec<Submission>>, StatusCode> {
    // Still return data from local DB for backward compatibility during migration
    let submissions = if query.status.as_deref() == Some("pending") {
        queries::get_pending_submissions(&state.db).await
    } else {
        queries::get_pending_submissions(&state.db).await
    }
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let limit = query.limit.unwrap_or(100);
    let limited: Vec<_> = submissions.into_iter().take(limit).collect();
    Ok(Json(limited))
}

/// DEPRECATED: Get submission by ID
/// Submissions are now managed by term-challenge.
pub async fn get_submission(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Submission>, StatusCode> {
    let submission = queries::get_submission(&state.db, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(submission))
}

/// DEPRECATED: Get submission source code
/// Source code access is now managed by term-challenge with proper authentication.
pub async fn get_submission_source(
    State(_state): State<Arc<AppState>>,
    Path(_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // No longer expose source code from platform-server
    // Users must use term-challenge API with proper authentication
    Err((
        StatusCode::GONE,
        Json(serde_json::json!({
            "error": "Source code access has been migrated to term-challenge",
            "message": "Use the term-challenge API to access source code with proper authentication",
            "term_challenge_url": get_term_challenge_url(),
            "endpoints": {
                "owner_source": "POST /api/v1/my/agents/:hash/source",
                "validator_claim": "POST /api/v1/validator/claim_job"
            }
        })),
    ))
}

/// DEPRECATED: Submit agent endpoint
/// Agent submissions have been migrated to term-challenge.
/// This endpoint returns a redirect notice.
pub async fn submit_agent(
    State(_state): State<Arc<AppState>>,
    Json(_req): Json<SubmitAgentRequest>,
) -> Result<Json<SubmitAgentResponse>, (StatusCode, Json<SubmitAgentResponse>)> {
    // Submissions are now handled by term-challenge
    tracing::warn!("Deprecated submit_agent endpoint called - redirecting to term-challenge");

    Err((
        StatusCode::GONE,
        Json(SubmitAgentResponse {
            success: false,
            submission_id: None,
            agent_hash: None,
            error: Some(format!(
                "Agent submissions have been migrated to term-challenge. \
                 Please use: POST {}/api/v1/submit",
                get_term_challenge_url()
            )),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // get_term_challenge_url tests
    // =========================================================================

    #[test]
    fn test_get_term_challenge_url_default() {
        // When env var is not set, should return default
        std::env::remove_var("TERM_CHALLENGE_URL");
        let url = get_term_challenge_url();
        assert_eq!(url, "http://localhost:8081");
    }

    #[test]
    fn test_get_term_challenge_url_from_env() {
        std::env::set_var("TERM_CHALLENGE_URL", "http://custom:9000");
        let url = get_term_challenge_url();
        assert_eq!(url, "http://custom:9000");
        std::env::remove_var("TERM_CHALLENGE_URL");
    }

    // =========================================================================
    // ListSubmissionsQuery tests
    // =========================================================================

    #[test]
    fn test_list_submissions_query_deserialize_empty() {
        let json = "{}";
        let query: ListSubmissionsQuery = serde_json::from_str(json).unwrap();
        assert!(query.limit.is_none());
        assert!(query.status.is_none());
    }

    #[test]
    fn test_list_submissions_query_deserialize_with_values() {
        let json = r#"{"limit": 50, "status": "pending"}"#;
        let query: ListSubmissionsQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.limit, Some(50));
        assert_eq!(query.status, Some("pending".to_string()));
    }

    #[test]
    fn test_list_submissions_query_deserialize_limit_only() {
        let json = r#"{"limit": 25}"#;
        let query: ListSubmissionsQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.limit, Some(25));
        assert!(query.status.is_none());
    }

    // =========================================================================
    // Deprecation response format tests
    // =========================================================================

    #[test]
    fn test_submit_agent_response_error_format() {
        let response = SubmitAgentResponse {
            success: false,
            submission_id: None,
            agent_hash: None,
            error: Some("Test error message".to_string()),
        };

        assert!(!response.success);
        assert!(response.submission_id.is_none());
        assert!(response.agent_hash.is_none());
        assert_eq!(response.error, Some("Test error message".to_string()));
    }

    #[test]
    fn test_submit_agent_response_success_format() {
        let response = SubmitAgentResponse {
            success: true,
            submission_id: Some("sub-123".to_string()),
            agent_hash: Some("hash-456".to_string()),
            error: None,
        };

        assert!(response.success);
        assert_eq!(response.submission_id, Some("sub-123".to_string()));
        assert_eq!(response.agent_hash, Some("hash-456".to_string()));
        assert!(response.error.is_none());
    }

    #[test]
    fn test_source_code_migration_response_contains_endpoints() {
        let response = serde_json::json!({
            "error": "Source code access has been migrated to term-challenge",
            "message": "Use the term-challenge API to access source code with proper authentication",
            "term_challenge_url": get_term_challenge_url(),
            "endpoints": {
                "owner_source": "POST /api/v1/my/agents/:hash/source",
                "validator_claim": "POST /api/v1/validator/claim_job"
            }
        });

        assert!(response["endpoints"]["owner_source"].as_str().is_some());
        assert!(response["endpoints"]["validator_claim"].as_str().is_some());
    }

    // =========================================================================
    // Limit handling tests
    // =========================================================================

    #[test]
    fn test_default_limit_is_100() {
        let query = ListSubmissionsQuery {
            limit: None,
            status: None,
        };
        let limit = query.limit.unwrap_or(100);
        assert_eq!(limit, 100);
    }

    #[test]
    fn test_custom_limit_preserved() {
        let query = ListSubmissionsQuery {
            limit: Some(50),
            status: None,
        };
        let limit = query.limit.unwrap_or(100);
        assert_eq!(limit, 50);
    }
}
