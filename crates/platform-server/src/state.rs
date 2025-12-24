//! Application state

use crate::challenge_proxy::ChallengeProxy;
use crate::db::DbPool;
use crate::models::{AuthSession, TaskLease, WsEvent};
use crate::websocket::events::EventBroadcaster;
use dashmap::DashMap;
use std::sync::Arc;

pub struct AppState {
    pub db: DbPool,
    pub challenge_id: String,
    pub sessions: DashMap<String, AuthSession>,
    pub broadcaster: Arc<EventBroadcaster>,
    pub owner_hotkey: Option<String>,
    pub challenge_proxy: Arc<ChallengeProxy>,
    /// Active task leases (task_id -> lease info)
    pub task_leases: DashMap<String, TaskLease>,
}

impl AppState {
    pub fn new(
        db: DbPool,
        challenge_id: String,
        owner_hotkey: Option<String>,
        challenge_proxy: Arc<ChallengeProxy>,
    ) -> Self {
        Self {
            db,
            challenge_id,
            sessions: DashMap::new(),
            broadcaster: Arc::new(EventBroadcaster::new(1000)),
            owner_hotkey,
            challenge_proxy,
            task_leases: DashMap::new(),
        }
    }

    pub async fn broadcast_event(&self, event: WsEvent) {
        self.broadcaster.broadcast(event);
    }

    pub fn is_owner(&self, hotkey: &str) -> bool {
        self.owner_hotkey
            .as_ref()
            .map(|o| o == hotkey)
            .unwrap_or(false)
    }
}
