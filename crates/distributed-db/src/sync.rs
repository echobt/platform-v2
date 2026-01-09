//! State synchronization
//!
//! Synchronizes state between validators using:
//! - State root comparison
//! - Merkle proof exchange
//! - Missing data retrieval

use crate::{MerkleProof, MerkleTrie, RocksStorage, SyncData, SyncState};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Sync request message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncRequest {
    /// Get current state
    GetState,
    /// Get entries with state root
    GetEntries { state_root: [u8; 32] },
    /// Get specific key
    GetKey { collection: String, key: Vec<u8> },
    /// Get Merkle proof for key
    GetProof { key: Vec<u8> },
    /// Get missing keys (keys we have that peer doesn't)
    GetMissingKeys { our_keys: Vec<Vec<u8>> },
}

/// Sync response message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncResponse {
    /// Current state
    State(SyncState),
    /// Entries data
    Entries(SyncData),
    /// Single key value
    KeyValue(Option<Vec<u8>>),
    /// Merkle proof
    Proof(Option<MerkleProof>),
    /// Missing keys (keys we have that peer requested)
    MissingKeys(Vec<(String, Vec<u8>, Vec<u8>)>),
    /// Error
    Error(String),
}

/// Peer identifier (simplified without libp2p)
pub type PeerId = String;

/// State synchronizer
pub struct StateSynchronizer {
    /// Local storage
    storage: Arc<RocksStorage>,
    /// Merkle trie
    merkle: Arc<RwLock<MerkleTrie>>,
    /// Known peers and their state roots
    peer_states: Arc<RwLock<HashMap<PeerId, SyncState>>>,
    /// Pending sync requests
    pending_syncs: Arc<RwLock<HashMap<PeerId, SyncState>>>,
    /// Event sender
    event_tx: mpsc::UnboundedSender<SyncEvent>,
}

/// Sync event
#[derive(Debug, Clone)]
pub enum SyncEvent {
    /// Peer state updated
    PeerStateUpdated { peer: PeerId, state: SyncState },
    /// Sync completed with peer
    SyncCompleted {
        peer: PeerId,
        entries_received: usize,
    },
    /// Sync failed
    SyncFailed { peer: PeerId, error: String },
    /// State divergence detected
    StateDivergence {
        peer: PeerId,
        our_root: [u8; 32],
        their_root: [u8; 32],
    },
}

impl StateSynchronizer {
    /// Create a new synchronizer
    pub fn new(
        storage: Arc<RocksStorage>,
        merkle: Arc<RwLock<MerkleTrie>>,
    ) -> (Self, mpsc::UnboundedReceiver<SyncEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        (
            Self {
                storage,
                merkle,
                peer_states: Arc::new(RwLock::new(HashMap::new())),
                pending_syncs: Arc::new(RwLock::new(HashMap::new())),
                event_tx,
            },
            event_rx,
        )
    }

    /// Handle incoming sync request
    pub fn handle_request(&self, request: SyncRequest) -> SyncResponse {
        match request {
            SyncRequest::GetState => {
                let root = self.merkle.read().root_hash();
                SyncResponse::State(SyncState {
                    state_root: root,
                    block_number: 0,
                    pending_count: 0,
                })
            }
            SyncRequest::GetEntries { state_root } => {
                let our_root = self.merkle.read().root_hash();
                if our_root == state_root {
                    return SyncResponse::Entries(SyncData {
                        state_root: our_root,
                        entries: Vec::new(),
                    });
                }

                let mut entries = Vec::new();
                if let Ok(collections) = self.storage.list_collections() {
                    for collection in collections {
                        if let Ok(items) = self.storage.iter_collection(&collection) {
                            for (key, value) in items {
                                entries.push((collection.clone(), key, value));
                            }
                        }
                    }
                }

                SyncResponse::Entries(SyncData {
                    state_root: our_root,
                    entries,
                })
            }
            SyncRequest::GetKey { collection, key } => match self.storage.get(&collection, &key) {
                Ok(value) => SyncResponse::KeyValue(value),
                Err(e) => SyncResponse::Error(e.to_string()),
            },
            SyncRequest::GetProof { key } => {
                let proof = self.merkle.read().generate_proof(&key);
                SyncResponse::Proof(proof)
            }
            SyncRequest::GetMissingKeys { our_keys } => {
                let mut missing = Vec::new();
                if let Ok(collections) = self.storage.list_collections() {
                    for collection in collections {
                        if let Ok(items) = self.storage.iter_collection(&collection) {
                            for (key, value) in items {
                                let full_key = format!("{}:{}", collection, hex::encode(&key));
                                if !our_keys.iter().any(|k| k == full_key.as_bytes()) {
                                    missing.push((collection.clone(), key, value));
                                }
                            }
                        }
                    }
                }

                SyncResponse::MissingKeys(missing)
            }
        }
    }

    /// Update peer state
    pub fn update_peer_state(&self, peer: PeerId, state: SyncState) {
        let our_root = self.merkle.read().root_hash();

        if state.state_root != our_root {
            let _ = self.event_tx.send(SyncEvent::StateDivergence {
                peer: peer.clone(),
                our_root,
                their_root: state.state_root,
            });

            self.pending_syncs
                .write()
                .insert(peer.clone(), state.clone());
        }

        self.peer_states.write().insert(peer.clone(), state.clone());
        let _ = self
            .event_tx
            .send(SyncEvent::PeerStateUpdated { peer, state });
    }

    /// Apply sync data from peer
    pub fn apply_sync_data(&self, peer: PeerId, data: SyncData) -> anyhow::Result<()> {
        let entries_count = data.entries.len();

        for (collection, key, value) in data.entries {
            self.storage.put(&collection, &key, &value)?;

            let full_key = format!("{}:{}", collection, hex::encode(&key));
            self.merkle.write().insert(full_key.as_bytes(), &value);
        }

        let _ = self.event_tx.send(SyncEvent::SyncCompleted {
            peer: peer.clone(),
            entries_received: entries_count,
        });

        self.pending_syncs.write().remove(&peer);

        Ok(())
    }

    /// Get peers that need sync
    pub fn peers_needing_sync(&self) -> Vec<(PeerId, SyncState)> {
        self.pending_syncs
            .read()
            .iter()
            .map(|(p, s)| (p.clone(), s.clone()))
            .collect()
    }

    /// Get all known peer states
    pub fn peer_states(&self) -> HashMap<PeerId, SyncState> {
        self.peer_states.read().clone()
    }

    /// Check if we're in sync with majority
    pub fn is_in_sync(&self) -> bool {
        let our_root = self.merkle.read().root_hash();
        let states = self.peer_states.read();

        if states.is_empty() {
            return true;
        }

        let matching = states.values().filter(|s| s.state_root == our_root).count();
        matching > states.len() / 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_sync_request_response() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, _rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        // Add some data
        storage.put("challenges", b"test", b"data").unwrap();
        merkle.write().insert(b"challenges:test", b"data");

        // Test GetState
        let response = sync.handle_request(SyncRequest::GetState);
        if let SyncResponse::State(state) = response {
            assert_ne!(state.state_root, [0u8; 32]);
        } else {
            panic!("Expected State response");
        }

        // Test GetKey
        let response = sync.handle_request(SyncRequest::GetKey {
            collection: "challenges".to_string(),
            key: b"test".to_vec(),
        });
        if let SyncResponse::KeyValue(Some(value)) = response {
            assert_eq!(value, b"data");
        } else {
            panic!("Expected KeyValue response");
        }
    }

    #[test]
    fn test_get_entries_same_root() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, _rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        storage.put("challenges", b"k1", b"v1").unwrap();
        merkle.write().insert(b"challenges:k1", b"v1");

        let our_root = merkle.read().root_hash();

        // Request with same root should return empty entries
        let response = sync.handle_request(SyncRequest::GetEntries {
            state_root: our_root,
        });

        if let SyncResponse::Entries(data) = response {
            assert_eq!(data.state_root, our_root);
            assert_eq!(data.entries.len(), 0);
        } else {
            panic!("Expected Entries response");
        }
    }

    #[test]
    fn test_get_entries_different_root() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, _rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        storage.put("challenges", b"k1", b"v1").unwrap();
        storage.put("agents", b"agent1", b"data1").unwrap();
        merkle.write().insert(b"challenges:k1", b"v1");
        merkle.write().insert(b"agents:agent1", b"data1");

        let different_root = [99u8; 32];

        // Request with different root should return all entries
        let response = sync.handle_request(SyncRequest::GetEntries {
            state_root: different_root,
        });

        if let SyncResponse::Entries(data) = response {
            assert_ne!(data.state_root, different_root);
            assert!(data.entries.len() >= 2);
        } else {
            panic!("Expected Entries response");
        }
    }

    #[test]
    fn test_get_key_not_found() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, _rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        let response = sync.handle_request(SyncRequest::GetKey {
            collection: "challenges".to_string(),
            key: b"nonexistent".to_vec(),
        });

        if let SyncResponse::KeyValue(None) = response {
            // Expected
        } else {
            panic!("Expected KeyValue(None) response");
        }
    }

    #[test]
    fn test_get_proof() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, _rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        merkle.write().insert(b"test:key", b"value");

        let response = sync.handle_request(SyncRequest::GetProof {
            key: b"test:key".to_vec(),
        });

        if let SyncResponse::Proof(proof_opt) = response {
            assert!(proof_opt.is_some());
        } else {
            panic!("Expected Proof response");
        }
    }

    #[test]
    fn test_get_missing_keys() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, _rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        storage.put("challenges", b"k1", b"v1").unwrap();
        storage.put("challenges", b"k2", b"v2").unwrap();

        // Request with one of the keys
        let our_keys = vec![b"challenges:6b31".to_vec()]; // k1 hex

        let response = sync.handle_request(SyncRequest::GetMissingKeys { our_keys });

        if let SyncResponse::MissingKeys(missing) = response {
            // Should return k2 as missing
            assert!(missing.len() >= 1);
        } else {
            panic!("Expected MissingKeys response");
        }
    }

    #[test]
    fn test_update_peer_state_matching_root() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, mut rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        storage.put("challenges", b"test", b"data").unwrap();
        merkle.write().insert(b"challenges:test", b"data");

        let our_root = merkle.read().root_hash();

        // Update with matching root
        sync.update_peer_state(
            "peer1".to_string(),
            SyncState {
                state_root: our_root,
                block_number: 100,
                pending_count: 0,
            },
        );

        // Should get PeerStateUpdated event
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, SyncEvent::PeerStateUpdated { .. }));

        // Should not be in pending syncs
        assert_eq!(sync.peers_needing_sync().len(), 0);
    }

    #[test]
    fn test_update_peer_state_divergent_root() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, mut rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        storage.put("challenges", b"test", b"data").unwrap();
        merkle.write().insert(b"challenges:test", b"data");

        let different_root = [99u8; 32];

        // Update with different root
        sync.update_peer_state(
            "peer1".to_string(),
            SyncState {
                state_root: different_root,
                block_number: 100,
                pending_count: 0,
            },
        );

        // Should get StateDivergence event
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, SyncEvent::StateDivergence { .. }));

        // Should also get PeerStateUpdated event
        let event2 = rx.try_recv().unwrap();
        assert!(matches!(event2, SyncEvent::PeerStateUpdated { .. }));

        // Should be in pending syncs
        assert_eq!(sync.peers_needing_sync().len(), 1);
    }

    #[test]
    fn test_apply_sync_data() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, mut rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        let peer = "peer1".to_string();

        // Add peer to pending syncs
        sync.pending_syncs.write().insert(
            peer.clone(),
            SyncState {
                state_root: [1u8; 32],
                block_number: 100,
                pending_count: 0,
            },
        );

        let sync_data = SyncData {
            state_root: [1u8; 32],
            entries: vec![
                ("challenges".to_string(), b"k1".to_vec(), b"v1".to_vec()),
                ("agents".to_string(), b"a1".to_vec(), b"d1".to_vec()),
            ],
        };

        sync.apply_sync_data(peer.clone(), sync_data).unwrap();

        // Verify data was applied
        assert_eq!(
            storage.get("challenges", b"k1").unwrap(),
            Some(b"v1".to_vec())
        );
        assert_eq!(storage.get("agents", b"a1").unwrap(), Some(b"d1".to_vec()));

        // Should get SyncCompleted event
        let event = rx.try_recv().unwrap();
        if let SyncEvent::SyncCompleted {
            peer: event_peer,
            entries_received,
        } = event
        {
            assert_eq!(event_peer, peer);
            assert_eq!(entries_received, 2);
        } else {
            panic!("Expected SyncCompleted event");
        }

        // Should be removed from pending syncs
        assert_eq!(sync.peers_needing_sync().len(), 0);
    }

    #[test]
    fn test_peer_states() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, _rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        let state1 = SyncState {
            state_root: [1u8; 32],
            block_number: 100,
            pending_count: 0,
        };
        let state2 = SyncState {
            state_root: [2u8; 32],
            block_number: 101,
            pending_count: 1,
        };

        sync.update_peer_state("peer1".to_string(), state1.clone());
        sync.update_peer_state("peer2".to_string(), state2.clone());

        let states = sync.peer_states();
        assert_eq!(states.len(), 2);
        assert_eq!(states.get("peer1").unwrap().state_root, state1.state_root);
        assert_eq!(states.get("peer2").unwrap().state_root, state2.state_root);
    }

    #[test]
    fn test_is_in_sync_no_peers() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, _rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        // With no peers, should consider ourselves in sync
        assert!(sync.is_in_sync());
    }

    #[test]
    fn test_is_in_sync_majority() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, _rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        storage.put("challenges", b"test", b"data").unwrap();
        merkle.write().insert(b"challenges:test", b"data");
        let our_root = merkle.read().root_hash();

        // Add 3 peers: 2 with our root, 1 with different root
        sync.update_peer_state(
            "peer1".to_string(),
            SyncState {
                state_root: our_root,
                block_number: 100,
                pending_count: 0,
            },
        );
        sync.update_peer_state(
            "peer2".to_string(),
            SyncState {
                state_root: our_root,
                block_number: 100,
                pending_count: 0,
            },
        );
        sync.update_peer_state(
            "peer3".to_string(),
            SyncState {
                state_root: [99u8; 32],
                block_number: 100,
                pending_count: 0,
            },
        );

        // 2 out of 3 match, so we're in sync (majority)
        assert!(sync.is_in_sync());
    }

    #[test]
    fn test_is_in_sync_minority() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, _rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        storage.put("challenges", b"test", b"data").unwrap();
        merkle.write().insert(b"challenges:test", b"data");
        let our_root = merkle.read().root_hash();

        // Add 3 peers: 1 with our root, 2 with different root
        sync.update_peer_state(
            "peer1".to_string(),
            SyncState {
                state_root: our_root,
                block_number: 100,
                pending_count: 0,
            },
        );
        sync.update_peer_state(
            "peer2".to_string(),
            SyncState {
                state_root: [99u8; 32],
                block_number: 100,
                pending_count: 0,
            },
        );
        sync.update_peer_state(
            "peer3".to_string(),
            SyncState {
                state_root: [99u8; 32],
                block_number: 100,
                pending_count: 0,
            },
        );

        // Only 1 out of 3 match, so we're out of sync
        assert!(!sync.is_in_sync());
    }

    #[test]
    fn test_peers_needing_sync() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(RocksStorage::open(dir.path()).unwrap());
        let merkle = Arc::new(RwLock::new(MerkleTrie::new()));

        let (sync, _rx) = StateSynchronizer::new(storage.clone(), merkle.clone());

        storage.put("challenges", b"test", b"data").unwrap();
        merkle.write().insert(b"challenges:test", b"data");

        // Add peer with different root (should be in pending_syncs)
        sync.update_peer_state(
            "peer1".to_string(),
            SyncState {
                state_root: [99u8; 32],
                block_number: 100,
                pending_count: 0,
            },
        );

        let needing_sync = sync.peers_needing_sync();
        assert_eq!(needing_sync.len(), 1);
        assert_eq!(needing_sync[0].0, "peer1");
    }
}
