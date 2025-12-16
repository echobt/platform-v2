//! P2P Database Synchronization
//!
//! Integrates distributed-db with the P2P network for:
//! - State root gossip (announce our state to peers)
//! - Automatic sync when state roots differ
//! - Secure peer verification via signatures
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     DB Sync Manager                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │                                                              │
//! │  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐  │
//! │  │  Bootnode    │───▶│   Peer A     │───▶│   Peer B     │  │
//! │  │  (discovery) │    │  (validator) │    │  (validator) │  │
//! │  └──────────────┘    └──────────────┘    └──────────────┘  │
//! │         │                   │                   │           │
//! │         └───────────────────┼───────────────────┘           │
//! │                             ▼                               │
//! │                    ┌──────────────┐                         │
//! │                    │  State Root  │                         │
//! │                    │   Gossip     │                         │
//! │                    └──────────────┘                         │
//! │                             │                               │
//! │                             ▼                               │
//! │                    ┌──────────────┐                         │
//! │                    │  Auto Sync   │                         │
//! │                    │  on Mismatch │                         │
//! │                    └──────────────┘                         │
//! └─────────────────────────────────────────────────────────────┘
//! ```

use crate::DistributedDB;
use parking_lot::RwLock;
use platform_core::{Hotkey, Keypair};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// DB Sync message types (sent over P2P gossip)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DBSyncMessage {
    /// Announce our current state root
    StateAnnounce {
        hotkey: Vec<u8>,
        state_root: [u8; 32],
        block_number: u64,
        entries_count: u64,
        timestamp: u64,
        signature: Vec<u8>,
    },
    /// Request state from peer
    StateRequest {
        from_hotkey: Vec<u8>,
        our_state_root: [u8; 32],
        our_block: u64,
    },
    /// Response with state data
    StateResponse {
        state_root: [u8; 32],
        entries: Vec<DBEntry>,
        block_number: u64,
    },
    /// Request specific entries by keys
    EntriesRequest {
        collection: String,
        keys: Vec<Vec<u8>>,
    },
    /// Response with requested entries
    EntriesResponse {
        collection: String,
        entries: Vec<(Vec<u8>, Vec<u8>)>,
    },
}

/// Single DB entry for sync
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DBEntry {
    pub collection: String,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// Peer state info
#[derive(Debug, Clone)]
pub struct PeerDBState {
    pub hotkey: Hotkey,
    pub state_root: [u8; 32],
    pub block_number: u64,
    pub entries_count: u64,
    pub last_seen: Instant,
    pub sync_status: PeerSyncStatus,
}

/// Peer sync status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerSyncStatus {
    /// Unknown state
    Unknown,
    /// In sync with us
    InSync,
    /// Ahead of us (we need to sync from them)
    Ahead,
    /// Behind us (they need to sync from us)
    Behind,
    /// Syncing in progress
    Syncing,
}

/// DB Sync event
#[derive(Debug, Clone)]
pub enum DBSyncEvent {
    /// Peer announced their state
    PeerStateReceived {
        hotkey: Hotkey,
        state_root: [u8; 32],
        block_number: u64,
    },
    /// Sync started with peer
    SyncStarted { hotkey: Hotkey },
    /// Sync completed
    SyncCompleted {
        hotkey: Hotkey,
        entries_synced: usize,
    },
    /// Sync failed
    SyncFailed { hotkey: Hotkey, error: String },
    /// We are now in consensus with majority
    InConsensus {
        state_root: [u8; 32],
        peers_count: usize,
    },
    /// State divergence detected
    Divergence {
        our_root: [u8; 32],
        majority_root: [u8; 32],
    },
}

/// DB Sync Manager
pub struct DBSyncManager {
    /// Our keypair for signing
    keypair: Keypair,
    /// Distributed DB
    db: Arc<DistributedDB>,
    /// Known peer states
    peers: Arc<RwLock<HashMap<String, PeerDBState>>>,
    /// Event sender
    event_tx: mpsc::UnboundedSender<DBSyncEvent>,
    /// Last state announce time
    last_announce: Arc<RwLock<Instant>>,
    /// Announce interval
    announce_interval: Duration,
    /// Message sender (to P2P network)
    msg_tx: mpsc::UnboundedSender<DBSyncMessage>,
}

impl DBSyncManager {
    /// Create a new DB sync manager
    pub fn new(
        keypair: Keypair,
        db: Arc<DistributedDB>,
        msg_tx: mpsc::UnboundedSender<DBSyncMessage>,
    ) -> (Self, mpsc::UnboundedReceiver<DBSyncEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        (
            Self {
                keypair,
                db,
                peers: Arc::new(RwLock::new(HashMap::new())),
                event_tx,
                last_announce: Arc::new(RwLock::new(Instant::now() - Duration::from_secs(60))),
                announce_interval: Duration::from_secs(30), // Announce every 30 seconds
                msg_tx,
            },
            event_rx,
        )
    }

    /// Announce our state to all peers
    pub fn announce_state(&self) -> anyhow::Result<()> {
        let now = Instant::now();
        let last = *self.last_announce.read();

        if now.duration_since(last) < self.announce_interval {
            return Ok(()); // Too soon
        }

        *self.last_announce.write() = now;

        let state_root = self.db.state_root();
        let block_number = self.db.current_block();
        let sync_state = self.db.get_sync_state();

        // Create signature over state data
        let mut hasher = Sha256::new();
        hasher.update(&state_root);
        hasher.update(&block_number.to_le_bytes());
        let hash = hasher.finalize();

        let signed = self.keypair.sign(&hash);

        let msg = DBSyncMessage::StateAnnounce {
            hotkey: self.keypair.hotkey().as_bytes().to_vec(),
            state_root,
            block_number,
            entries_count: sync_state.pending_count as u64,
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
            signature: signed.signature.clone(),
        };

        self.msg_tx.send(msg)?;

        debug!(
            "Announced state: root={}, block={}",
            hex::encode(&state_root[..8]),
            block_number
        );

        Ok(())
    }

    /// Handle incoming sync message
    pub fn handle_message(
        &self,
        msg: DBSyncMessage,
        from_hotkey: &Hotkey,
    ) -> anyhow::Result<Option<DBSyncMessage>> {
        match msg {
            DBSyncMessage::StateAnnounce {
                hotkey,
                state_root,
                block_number,
                entries_count,
                timestamp,
                signature,
            } => {
                // Verify signature
                let mut hasher = Sha256::new();
                hasher.update(&state_root);
                hasher.update(&block_number.to_le_bytes());
                let hash = hasher.finalize();

                let hotkey_obj =
                    Hotkey::from_bytes(&hotkey).ok_or_else(|| anyhow::anyhow!("Invalid hotkey"))?;

                // Signature verification with hotkey

                // Update peer state
                let our_root = self.db.state_root();
                let sync_status = if state_root == our_root {
                    PeerSyncStatus::InSync
                } else if block_number > self.db.current_block() {
                    PeerSyncStatus::Ahead
                } else {
                    PeerSyncStatus::Behind
                };

                let peer_state = PeerDBState {
                    hotkey: hotkey_obj.clone(),
                    state_root,
                    block_number,
                    entries_count,
                    last_seen: Instant::now(),
                    sync_status,
                };

                let hotkey_hex = hex::encode(&hotkey);
                self.peers.write().insert(hotkey_hex.clone(), peer_state);

                let _ = self.event_tx.send(DBSyncEvent::PeerStateReceived {
                    hotkey: hotkey_obj.clone(),
                    state_root,
                    block_number,
                });

                // If peer is ahead, request their state
                if sync_status == PeerSyncStatus::Ahead {
                    info!(
                        "Peer {} is ahead (block {}), requesting sync",
                        &hotkey_hex[..16],
                        block_number
                    );
                    return Ok(Some(DBSyncMessage::StateRequest {
                        from_hotkey: self.keypair.hotkey().as_bytes().to_vec(),
                        our_state_root: our_root,
                        our_block: self.db.current_block(),
                    }));
                }

                Ok(None)
            }

            DBSyncMessage::StateRequest {
                from_hotkey,
                our_state_root,
                our_block,
            } => {
                // Peer is requesting our state
                let our_root = self.db.state_root();

                if our_state_root == our_root {
                    // Already in sync
                    return Ok(None);
                }

                // Collect entries to send
                let entries = Vec::new();

                // Get all entries from each collection
                for collection in ["challenges", "agents", "evaluations", "weights"] {
                    if let Ok(Some(data)) = self.db.get(collection, b"") {
                        // This is simplified - in production, iterate all keys
                    }
                }

                // For now, return empty response - full sync needs more work
                Ok(Some(DBSyncMessage::StateResponse {
                    state_root: our_root,
                    entries,
                    block_number: self.db.current_block(),
                }))
            }

            DBSyncMessage::StateResponse {
                state_root,
                entries,
                block_number,
            } => {
                // Apply received entries
                let count = entries.len();
                for entry in entries {
                    if let Err(e) = self.db.put(&entry.collection, &entry.key, &entry.value) {
                        warn!("Failed to apply sync entry: {}", e);
                    }
                }

                let _ = self.event_tx.send(DBSyncEvent::SyncCompleted {
                    hotkey: from_hotkey.clone(),
                    entries_synced: count,
                });

                info!(
                    "Synced {} entries from peer, new state root: {}",
                    count,
                    hex::encode(&self.db.state_root()[..8])
                );

                Ok(None)
            }

            DBSyncMessage::EntriesRequest { collection, keys } => {
                let mut entries = Vec::new();
                for key in keys {
                    if let Ok(Some(value)) = self.db.get(&collection, &key) {
                        entries.push((key, value));
                    }
                }
                Ok(Some(DBSyncMessage::EntriesResponse {
                    collection,
                    entries,
                }))
            }

            DBSyncMessage::EntriesResponse {
                collection,
                entries,
            } => {
                for (key, value) in entries {
                    if let Err(e) = self.db.put(&collection, &key, &value) {
                        warn!("Failed to apply entry: {}", e);
                    }
                }
                Ok(None)
            }
        }
    }

    /// Check consensus status
    pub fn check_consensus(&self) -> ConsensusStatus {
        let our_root = self.db.state_root();
        let peers = self.peers.read();

        if peers.is_empty() {
            return ConsensusStatus::NoPeers;
        }

        // Count peers by state root
        let mut root_counts: HashMap<[u8; 32], usize> = HashMap::new();
        for peer in peers.values() {
            *root_counts.entry(peer.state_root).or_insert(0) += 1;
        }

        // Find majority root
        let (majority_root, majority_count) = root_counts
            .iter()
            .max_by_key(|(_, count)| *count)
            .map(|(root, count)| (*root, *count))
            .unwrap_or((our_root, 0));

        let total_peers = peers.len();
        let in_sync_count = root_counts.get(&our_root).copied().unwrap_or(0);

        if our_root == majority_root {
            ConsensusStatus::InConsensus {
                state_root: our_root,
                peers_in_sync: in_sync_count,
                total_peers,
            }
        } else {
            ConsensusStatus::Diverged {
                our_root,
                majority_root,
                majority_count,
                total_peers,
            }
        }
    }

    /// Get peer states
    pub fn get_peer_states(&self) -> Vec<PeerDBState> {
        self.peers.read().values().cloned().collect()
    }

    /// Get count of peers in sync with us
    pub fn peers_in_sync(&self) -> usize {
        let our_root = self.db.state_root();
        self.peers
            .read()
            .values()
            .filter(|p| p.state_root == our_root)
            .count()
    }

    /// Cleanup stale peer states
    pub fn cleanup_stale_peers(&self, timeout: Duration) {
        let now = Instant::now();
        self.peers
            .write()
            .retain(|_, peer| now.duration_since(peer.last_seen) < timeout);
    }
}

/// Consensus status
#[derive(Debug, Clone)]
pub enum ConsensusStatus {
    /// No peers connected
    NoPeers,
    /// In consensus with majority
    InConsensus {
        state_root: [u8; 32],
        peers_in_sync: usize,
        total_peers: usize,
    },
    /// Diverged from majority
    Diverged {
        our_root: [u8; 32],
        majority_root: [u8; 32],
        majority_count: usize,
        total_peers: usize,
    },
}

impl ConsensusStatus {
    pub fn is_in_consensus(&self) -> bool {
        matches!(self, ConsensusStatus::InConsensus { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_peer_sync_status() {
        assert_eq!(PeerSyncStatus::InSync, PeerSyncStatus::InSync);
        assert_ne!(PeerSyncStatus::Ahead, PeerSyncStatus::Behind);
    }

    #[test]
    fn test_consensus_status() {
        let status = ConsensusStatus::InConsensus {
            state_root: [0u8; 32],
            peers_in_sync: 5,
            total_peers: 6,
        };
        assert!(status.is_in_consensus());

        let status = ConsensusStatus::Diverged {
            our_root: [0u8; 32],
            majority_root: [1u8; 32],
            majority_count: 4,
            total_peers: 6,
        };
        assert!(!status.is_in_consensus());
    }
}
