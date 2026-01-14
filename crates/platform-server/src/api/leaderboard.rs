//! Leaderboard API handlers

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

#[derive(Debug, Deserialize)]
pub struct LeaderboardQuery {
    pub limit: Option<usize>,
}

pub async fn get_leaderboard(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LeaderboardQuery>,
) -> Result<Json<Vec<LeaderboardEntry>>, StatusCode> {
    let limit = query.limit.unwrap_or(100);
    let entries = queries::get_leaderboard(&state.db, limit)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(entries))
}

pub async fn get_agent_rank(
    State(state): State<Arc<AppState>>,
    Path(agent_hash): Path<String>,
) -> Result<Json<LeaderboardEntry>, StatusCode> {
    let entry = queries::get_leaderboard_entry(&state.db, &agent_hash)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // LeaderboardQuery tests
    // =========================================================================

    #[test]
    fn test_leaderboard_query_deserialize_empty() {
        let json = "{}";
        let query: LeaderboardQuery = serde_json::from_str(json).unwrap();
        assert!(query.limit.is_none());
    }

    #[test]
    fn test_leaderboard_query_deserialize_with_limit() {
        let json = r#"{"limit": 50}"#;
        let query: LeaderboardQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.limit, Some(50));
    }

    #[test]
    fn test_leaderboard_query_deserialize_with_zero_limit() {
        let json = r#"{"limit": 0}"#;
        let query: LeaderboardQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.limit, Some(0));
    }

    #[test]
    fn test_leaderboard_query_deserialize_with_large_limit() {
        let json = r#"{"limit": 10000}"#;
        let query: LeaderboardQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.limit, Some(10000));
    }

    // =========================================================================
    // Default limit tests
    // =========================================================================

    #[test]
    fn test_default_limit_is_100() {
        let query = LeaderboardQuery { limit: None };
        let limit = query.limit.unwrap_or(100);
        assert_eq!(limit, 100);
    }

    #[test]
    fn test_custom_limit_preserved() {
        let query = LeaderboardQuery { limit: Some(25) };
        let limit = query.limit.unwrap_or(100);
        assert_eq!(limit, 25);
    }

    // =========================================================================
    // LeaderboardEntry serialization tests
    // =========================================================================

    #[test]
    fn test_leaderboard_entry_serialization() {
        let entry = LeaderboardEntry {
            agent_hash: "hash123".to_string(),
            miner_hotkey: "miner1".to_string(),
            name: Some("Test Agent".to_string()),
            consensus_score: 0.95,
            evaluation_count: 10,
            rank: 1,
            first_epoch: 100,
            last_epoch: 150,
            best_rank: Some(1),
            total_rewards: 100.5,
            updated_at: 1234567890,
        };

        let json = serde_json::to_string(&entry).unwrap();

        assert!(json.contains("hash123"));
        assert!(json.contains("0.95"));
        assert!(json.contains("Test Agent"));
    }

    #[test]
    fn test_leaderboard_entry_deserialization() {
        let json = r#"{
            "agent_hash": "hash123",
            "miner_hotkey": "miner1",
            "name": "Test Agent",
            "consensus_score": 0.95,
            "evaluation_count": 10,
            "rank": 1,
            "first_epoch": 100,
            "last_epoch": 150,
            "best_rank": 1,
            "total_rewards": 100.5,
            "updated_at": 1234567890
        }"#;

        let entry: LeaderboardEntry = serde_json::from_str(json).unwrap();

        assert_eq!(entry.agent_hash, "hash123");
        assert_eq!(entry.consensus_score, 0.95);
        assert_eq!(entry.rank, 1);
    }

    #[test]
    fn test_leaderboard_entry_with_null_name() {
        let json = r#"{
            "agent_hash": "hash123",
            "miner_hotkey": "miner1",
            "name": null,
            "consensus_score": 0.5,
            "evaluation_count": 5,
            "rank": 10,
            "first_epoch": 100,
            "last_epoch": 100,
            "best_rank": null,
            "total_rewards": 0.0,
            "updated_at": 1234567890
        }"#;

        let entry: LeaderboardEntry = serde_json::from_str(json).unwrap();

        assert!(entry.name.is_none());
        assert!(entry.best_rank.is_none());
    }
}
