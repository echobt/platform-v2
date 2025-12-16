//! State Synchronization Protocol
//!
//! Handles full state sync for new validators joining the network.
//!
//! # Sync Process
//!
//! 1. New validator connects to bootstrap peers
//! 2. Requests full state snapshot
//! 3. Receives chain state, validators, challenges
//! 4. Syncs challenge-specific data
//! 5. Catches up on recent blocks
//!
//! # Challenge Data Sync
//!
//! Challenge data is synced separately:
//! 1. Get list of challenges from chain state
//! 2. For each challenge, request challenge data from peers
//! 3. Verify data against challenge rules
//! 4. Store in local database

use parking_lot::RwLock;
use platform_core::ChainState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

/// Sync status for tracking progress
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    /// Not started
    Idle,
    /// Requesting state from peers
    RequestingState,
    /// Syncing chain state
    SyncingChainState,
    /// Syncing challenge data
    SyncingChallengeData { current: usize, total: usize },
    /// Catching up on recent blocks
    CatchingUp { from_block: u64, to_block: u64 },
    /// Fully synced
    Synced,
    /// Sync failed
    Failed(String),
}

/// State sync request types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StateSyncRequest {
    /// Request full chain state snapshot
    FullState,
    /// Request specific block range
    BlockRange { from: u64, to: u64 },
    /// Request challenge data
    ChallengeData { challenge_id: String },
    /// Request validator data for a challenge
    ValidatorChallengeData {
        challenge_id: String,
        validator: String,
    },
    /// Request state hash for verification
    StateHash,
    /// Request list of challenges
    ChallengeList,
    /// Ping to check if peer is synced
    Ping,
}

/// State sync response types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StateSyncResponse {
    /// Full state snapshot
    FullState {
        block_height: u64,
        epoch: u64,
        state_hash: Vec<u8>,
        chain_state: Vec<u8>, // Serialized ChainState
        validators_count: usize,
        challenges_count: usize,
    },
    /// Block range data
    BlockRange {
        from: u64,
        to: u64,
        blocks: Vec<BlockData>,
    },
    /// Challenge data
    ChallengeData {
        challenge_id: String,
        data: Vec<ChallengeDataEntry>,
    },
    /// Validator challenge data
    ValidatorChallengeData {
        challenge_id: String,
        validator: String,
        data: Vec<ValidatorDataEntry>,
    },
    /// State hash for verification
    StateHash { block_height: u64, hash: Vec<u8> },
    /// List of challenges
    ChallengeList { challenges: Vec<ChallengeSummary> },
    /// Pong response
    Pong { block_height: u64, is_synced: bool },
    /// Error response
    Error { message: String },
}

/// Simplified block data for sync
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockData {
    pub number: u64,
    pub hash: Vec<u8>,
    pub parent_hash: Vec<u8>,
    pub state_root: Vec<u8>,
    pub timestamp: u64,
}

/// Challenge data entry for sync
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeDataEntry {
    pub key: String,
    pub value: Vec<u8>,
    pub version: u64,
    pub updated_at: u64,
}

/// Validator data entry for sync
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorDataEntry {
    pub key: String,
    pub value: Vec<u8>,
    pub block_height: u64,
    pub signature: Vec<u8>,
}

/// Challenge summary for listing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeSummary {
    pub id: String,
    pub name: String,
    pub mechanism_id: u8,
    pub is_active: bool,
    pub data_entries: usize,
}

/// Configuration for state sync
#[derive(Debug, Clone)]
pub struct StateSyncConfig {
    /// Timeout for sync requests
    pub request_timeout: Duration,
    /// Maximum retries per request
    pub max_retries: u32,
    /// Batch size for challenge data sync
    pub challenge_data_batch_size: usize,
    /// Minimum peers required for sync
    pub min_peers_for_sync: usize,
    /// Verify state hash after sync
    pub verify_state_hash: bool,
}

impl Default for StateSyncConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            max_retries: 3,
            challenge_data_batch_size: 100,
            min_peers_for_sync: 1,
            verify_state_hash: true,
        }
    }
}

/// State synchronization manager
pub struct StateSync {
    config: StateSyncConfig,
    status: Arc<RwLock<SyncStatus>>,
    /// Peer sync status (peer_id -> (block_height, is_synced))
    peer_status: Arc<RwLock<HashMap<String, (u64, bool)>>>,
    /// Last sync time
    last_sync: Arc<RwLock<Option<Instant>>>,
    /// Local block height
    local_height: Arc<RwLock<u64>>,
}

impl StateSync {
    /// Create a new state sync manager
    pub fn new(config: StateSyncConfig) -> Self {
        Self {
            config,
            status: Arc::new(RwLock::new(SyncStatus::Idle)),
            peer_status: Arc::new(RwLock::new(HashMap::new())),
            last_sync: Arc::new(RwLock::new(None)),
            local_height: Arc::new(RwLock::new(0)),
        }
    }

    /// Get current sync status
    pub fn status(&self) -> SyncStatus {
        self.status.read().clone()
    }

    /// Check if fully synced
    pub fn is_synced(&self) -> bool {
        matches!(*self.status.read(), SyncStatus::Synced)
    }

    /// Set local block height
    pub fn set_local_height(&self, height: u64) {
        *self.local_height.write() = height;
    }

    /// Get local block height
    pub fn local_height(&self) -> u64 {
        *self.local_height.read()
    }

    /// Update peer status from pong
    pub fn update_peer_status(&self, peer_id: &str, block_height: u64, is_synced: bool) {
        self.peer_status
            .write()
            .insert(peer_id.to_string(), (block_height, is_synced));
    }

    /// Get best peer to sync from
    pub fn best_sync_peer(&self) -> Option<(String, u64)> {
        let peers = self.peer_status.read();
        let local = self.local_height();

        peers
            .iter()
            .filter(|(_, (height, synced))| *synced && *height > local)
            .max_by_key(|(_, (height, _))| *height)
            .map(|(peer, (height, _))| (peer.clone(), *height))
    }

    /// Check if we need to sync
    pub fn needs_sync(&self) -> bool {
        let peers = self.peer_status.read();
        let local = self.local_height();

        // Check if any peer is ahead
        peers
            .values()
            .any(|(height, synced)| *synced && *height > local + 1)
    }

    /// Start sync process
    pub fn start_sync(&self) {
        let mut status = self.status.write();
        if matches!(
            *status,
            SyncStatus::Idle | SyncStatus::Synced | SyncStatus::Failed(_)
        ) {
            *status = SyncStatus::RequestingState;
            info!("Starting state sync");
        }
    }

    /// Update sync status
    pub fn set_status(&self, new_status: SyncStatus) {
        let mut status = self.status.write();
        info!("Sync status: {:?} -> {:?}", *status, new_status);
        *status = new_status;

        if matches!(*status, SyncStatus::Synced) {
            *self.last_sync.write() = Some(Instant::now());
        }
    }

    /// Create a full state request
    pub fn create_full_state_request() -> StateSyncRequest {
        StateSyncRequest::FullState
    }

    /// Create a challenge data request
    pub fn create_challenge_data_request(challenge_id: &str) -> StateSyncRequest {
        StateSyncRequest::ChallengeData {
            challenge_id: challenge_id.to_string(),
        }
    }

    /// Create a ping request
    pub fn create_ping_request() -> StateSyncRequest {
        StateSyncRequest::Ping
    }

    /// Handle a sync request (as a responder)
    pub fn handle_request(
        &self,
        request: StateSyncRequest,
        chain_state: &ChainState,
        get_challenge_data: impl Fn(&str) -> Vec<ChallengeDataEntry>,
    ) -> StateSyncResponse {
        match request {
            StateSyncRequest::FullState => match bincode::serialize(chain_state) {
                Ok(data) => StateSyncResponse::FullState {
                    block_height: chain_state.block_height,
                    epoch: chain_state.epoch,
                    state_hash: chain_state.state_hash.to_vec(),
                    chain_state: data,
                    validators_count: chain_state.validators.len(),
                    challenges_count: chain_state.challenges.len(),
                },
                Err(e) => StateSyncResponse::Error {
                    message: format!("Serialization error: {}", e),
                },
            },

            StateSyncRequest::StateHash => StateSyncResponse::StateHash {
                block_height: chain_state.block_height,
                hash: chain_state.state_hash.to_vec(),
            },

            StateSyncRequest::ChallengeList => {
                let challenges: Vec<ChallengeSummary> = chain_state
                    .challenges
                    .values()
                    .map(|c| ChallengeSummary {
                        id: c.id.to_string(),
                        name: c.name.clone(),
                        mechanism_id: c.config.mechanism_id,
                        is_active: c.is_active,
                        data_entries: 0, // Storage query pending
                    })
                    .collect();

                StateSyncResponse::ChallengeList { challenges }
            }

            StateSyncRequest::ChallengeData { challenge_id } => {
                let data = get_challenge_data(&challenge_id);
                StateSyncResponse::ChallengeData { challenge_id, data }
            }

            StateSyncRequest::Ping => StateSyncResponse::Pong {
                block_height: chain_state.block_height,
                is_synced: self.is_synced(),
            },

            _ => StateSyncResponse::Error {
                message: "Request type not implemented".to_string(),
            },
        }
    }

    /// Process a sync response
    pub fn process_response(
        &self,
        response: StateSyncResponse,
    ) -> Result<ProcessedSyncData, String> {
        match response {
            StateSyncResponse::FullState {
                block_height,
                epoch,
                state_hash,
                chain_state,
                validators_count,
                challenges_count,
            } => {
                info!(
                    "Received full state: block={}, epoch={}, validators={}, challenges={}",
                    block_height, epoch, validators_count, challenges_count
                );

                Ok(ProcessedSyncData::FullState {
                    block_height,
                    epoch,
                    state_hash,
                    chain_state_bytes: chain_state,
                })
            }

            StateSyncResponse::ChallengeData { challenge_id, data } => {
                info!(
                    "Received {} data entries for challenge {}",
                    data.len(),
                    challenge_id
                );
                Ok(ProcessedSyncData::ChallengeData {
                    challenge_id,
                    entries: data,
                })
            }

            StateSyncResponse::Pong {
                block_height,
                is_synced,
            } => Ok(ProcessedSyncData::PeerStatus {
                block_height,
                is_synced,
            }),

            StateSyncResponse::Error { message } => Err(message),

            _ => Ok(ProcessedSyncData::Other),
        }
    }
}

/// Processed sync data for application layer
#[derive(Debug)]
pub enum ProcessedSyncData {
    FullState {
        block_height: u64,
        epoch: u64,
        state_hash: Vec<u8>,
        chain_state_bytes: Vec<u8>,
    },
    ChallengeData {
        challenge_id: String,
        entries: Vec<ChallengeDataEntry>,
    },
    PeerStatus {
        block_height: u64,
        is_synced: bool,
    },
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_status() {
        let sync = StateSync::new(StateSyncConfig::default());
        assert_eq!(sync.status(), SyncStatus::Idle);

        sync.start_sync();
        assert_eq!(sync.status(), SyncStatus::RequestingState);

        sync.set_status(SyncStatus::Synced);
        assert!(sync.is_synced());
    }

    #[test]
    fn test_peer_status() {
        let sync = StateSync::new(StateSyncConfig::default());
        sync.set_local_height(100);

        sync.update_peer_status("peer1", 100, true);
        sync.update_peer_status("peer2", 150, true);
        sync.update_peer_status("peer3", 90, true);

        let best = sync.best_sync_peer();
        assert!(best.is_some());
        assert_eq!(best.unwrap().1, 150);
    }

    #[test]
    fn test_needs_sync() {
        let sync = StateSync::new(StateSyncConfig::default());
        sync.set_local_height(100);

        // No peers - no sync needed
        assert!(!sync.needs_sync());

        // Peer at same height - no sync
        sync.update_peer_status("peer1", 100, true);
        assert!(!sync.needs_sync());

        // Peer ahead - sync needed
        sync.update_peer_status("peer2", 110, true);
        assert!(sync.needs_sync());
    }
}
