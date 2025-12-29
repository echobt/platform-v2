//! Submissions API handlers

use crate::db::queries;
use crate::models::*;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use sp_core::crypto::Ss58Codec;
use std::sync::Arc;

/// Validate that a string is a valid SS58 hotkey address
fn is_valid_ss58_hotkey(hotkey: &str) -> bool {
    sp_core::crypto::AccountId32::from_ss58check(hotkey).is_ok()
}

#[derive(Debug, Deserialize)]
pub struct ListSubmissionsQuery {
    pub limit: Option<usize>,
    pub status: Option<String>,
}

pub async fn list_submissions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListSubmissionsQuery>,
) -> Result<Json<Vec<Submission>>, StatusCode> {
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

pub async fn get_submission_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let submission = queries::get_submission(&state.db, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({
        "agent_hash": submission.agent_hash,
        "source_code": submission.source_code,
    })))
}

pub async fn submit_agent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SubmitAgentRequest>,
) -> Result<Json<SubmitAgentResponse>, (StatusCode, Json<SubmitAgentResponse>)> {
    // Validate miner_hotkey is a valid SS58 address
    if !is_valid_ss58_hotkey(&req.miner_hotkey) {
        tracing::warn!(
            "Invalid miner_hotkey format: {} (expected SS58 address starting with '5')",
            &req.miner_hotkey[..32.min(req.miner_hotkey.len())]
        );
        return Err((
            StatusCode::BAD_REQUEST,
            Json(SubmitAgentResponse {
                success: false,
                submission_id: None,
                agent_hash: None,
                error: Some(format!(
                    "Invalid miner_hotkey: must be a valid SS58 address (e.g., '5GrwvaEF...'). Received: {}",
                    &req.miner_hotkey[..32.min(req.miner_hotkey.len())]
                )),
            }),
        ));
    }

    let epoch = queries::get_current_epoch(&state.db).await.unwrap_or(0);

    // Rate limiting: 0.33 submissions per epoch (1 every 3 epochs)
    let can_submit = match queries::can_miner_submit(&state.db, &req.miner_hotkey, epoch).await {
        Ok(can) => can,
        Err(e) => {
            tracing::error!("Rate limit check failed for {}: {}", req.miner_hotkey, e);
            true // Allow submission if rate limit check fails
        }
    };

    tracing::debug!(
        "Submission check: miner={}, epoch={}, can_submit={}",
        &req.miner_hotkey[..16.min(req.miner_hotkey.len())],
        epoch,
        can_submit
    );

    if !can_submit {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(SubmitAgentResponse {
                success: false,
                submission_id: None,
                agent_hash: None,
                error: Some("Rate limit: 1 submission per 3 epochs".to_string()),
            }),
        ));
    }

    // Create submission with API key for centralized LLM inference
    let submission = queries::create_submission(
        &state.db,
        &req.miner_hotkey,
        &req.source_code,
        req.name.as_deref(),
        req.api_key.as_deref(),
        req.api_provider.as_deref(),
        req.api_keys_encrypted.as_deref(),
        epoch,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(SubmitAgentResponse {
                success: false,
                submission_id: None,
                agent_hash: None,
                error: Some(e.to_string()),
            }),
        )
    })?;

    tracing::info!(
        "Agent submitted: {} (hash: {}) from {} with provider: {:?}",
        submission.name.as_deref().unwrap_or("unnamed"),
        &submission.agent_hash[..16],
        &req.miner_hotkey,
        req.api_provider
    );

    // Create evaluation job for each active challenge
    let challenges = queries::get_challenges(&state.db).await.unwrap_or_default();

    for challenge in challenges.iter().filter(|c| c.status == "active") {
        match queries::create_job(&state.db, &submission.id, &challenge.id).await {
            Ok(job) => {
                tracing::info!(
                    "Created job {} for submission {} on challenge {}",
                    job.id,
                    &submission.id[..8],
                    challenge.id
                );
            }
            Err(e) => {
                tracing::warn!("Failed to create job for challenge {}: {}", challenge.id, e);
            }
        }
    }

    // Broadcast submission event
    state
        .broadcast_event(WsEvent::SubmissionReceived(SubmissionEvent {
            submission_id: submission.id.clone(),
            agent_hash: submission.agent_hash.clone(),
            miner_hotkey: submission.miner_hotkey.clone(),
            name: submission.name.clone(),
            epoch,
        }))
        .await;

    Ok(Json(SubmitAgentResponse {
        success: true,
        submission_id: Some(submission.id),
        agent_hash: Some(submission.agent_hash),
        error: None,
    }))
}
