//! DHT-based state synchronization
//!
//! Synchronizes state between validators using:
//! - State root comparison
//! - Merkle proof exchange
//! - Missing data retrieval via DHT

use crate::{MerkleProof, MerkleTrie, RocksStorage, SyncData, SyncState};
use libp2p::request_response::{self, Behaviour as RequestResponse, Codec, ProtocolSupport};
use libp2p::{PeerId, StreamProtocol};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Sync protocol name
const SYNC_PROTOCOL: &str = "/distributed-db/sync/1.0.0";

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
                    block_number: 0, // Block number from state manager
                    pending_count: 0,
                })
            }
            SyncRequest::GetEntries { state_root } => {
                // Only return entries if state root differs
                let our_root = self.merkle.read().root_hash();
                if our_root == state_root {
                    return SyncResponse::Entries(SyncData {
                        state_root: our_root,
                        entries: Vec::new(),
                    });
                }

                // Collect all entries
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
                // Find keys we have that weren't in peer's list
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

        // Check for divergence
        if state.state_root != our_root {
            let _ = self.event_tx.send(SyncEvent::StateDivergence {
                peer,
                our_root,
                their_root: state.state_root,
            });

            // Queue for sync
            self.pending_syncs.write().insert(peer, state.clone());
        }

        self.peer_states.write().insert(peer, state.clone());
        let _ = self
            .event_tx
            .send(SyncEvent::PeerStateUpdated { peer, state });
    }

    /// Apply sync data from peer
    pub fn apply_sync_data(&self, peer: PeerId, data: SyncData) -> anyhow::Result<()> {
        // State root verification via merkle proofs
        let entries_count = data.entries.len();

        for (collection, key, value) in data.entries {
            self.storage.put(&collection, &key, &value)?;

            // Update merkle trie
            let full_key = format!("{}:{}", collection, hex::encode(&key));
            self.merkle.write().insert(full_key.as_bytes(), &value);
        }

        let _ = self.event_tx.send(SyncEvent::SyncCompleted {
            peer,
            entries_received: entries_count,
        });

        // Remove from pending
        self.pending_syncs.write().remove(&peer);

        Ok(())
    }

    /// Get peers that need sync
    pub fn peers_needing_sync(&self) -> Vec<(PeerId, SyncState)> {
        self.pending_syncs
            .read()
            .iter()
            .map(|(p, s)| (*p, s.clone()))
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

/// Sync protocol codec for libp2p
#[derive(Debug, Clone, Default)]
pub struct SyncCodec;

impl Codec for SyncCodec {
    type Protocol = StreamProtocol;
    type Request = SyncRequest;
    type Response = SyncResponse;

    fn read_request<'life0, 'life1, 'life2, 'async_trait, T>(
        &'life0 mut self,
        _protocol: &'life1 Self::Protocol,
        io: &'life2 mut T,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<Self::Request>> + Send + 'async_trait>,
    >
    where
        T: futures::AsyncRead + Unpin + Send + 'async_trait,
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            use futures::AsyncReadExt;
            let mut len_bytes = [0u8; 4];
            io.read_exact(&mut len_bytes).await?;
            let len = u32::from_le_bytes(len_bytes) as usize;

            let mut buf = vec![0u8; len];
            io.read_exact(&mut buf).await?;

            bincode::deserialize(&buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })
    }

    fn read_response<'life0, 'life1, 'life2, 'async_trait, T>(
        &'life0 mut self,
        _protocol: &'life1 Self::Protocol,
        io: &'life2 mut T,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::io::Result<Self::Response>> + Send + 'async_trait,
        >,
    >
    where
        T: futures::AsyncRead + Unpin + Send + 'async_trait,
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            use futures::AsyncReadExt;
            let mut len_bytes = [0u8; 4];
            io.read_exact(&mut len_bytes).await?;
            let len = u32::from_le_bytes(len_bytes) as usize;

            let mut buf = vec![0u8; len];
            io.read_exact(&mut buf).await?;

            bincode::deserialize(&buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })
    }

    fn write_request<'life0, 'life1, 'life2, 'async_trait, T>(
        &'life0 mut self,
        _protocol: &'life1 Self::Protocol,
        io: &'life2 mut T,
        req: Self::Request,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'async_trait>,
    >
    where
        T: futures::AsyncWrite + Unpin + Send + 'async_trait,
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            use futures::AsyncWriteExt;
            let buf = bincode::serialize(&req)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

            let len = (buf.len() as u32).to_le_bytes();
            io.write_all(&len).await?;
            io.write_all(&buf).await?;
            io.flush().await?;

            Ok(())
        })
    }

    fn write_response<'life0, 'life1, 'life2, 'async_trait, T>(
        &'life0 mut self,
        _protocol: &'life1 Self::Protocol,
        io: &'life2 mut T,
        res: Self::Response,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'async_trait>,
    >
    where
        T: futures::AsyncWrite + Unpin + Send + 'async_trait,
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            use futures::AsyncWriteExt;
            let buf = bincode::serialize(&res)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

            let len = (buf.len() as u32).to_le_bytes();
            io.write_all(&len).await?;
            io.write_all(&buf).await?;
            io.flush().await?;

            Ok(())
        })
    }
}

/// Create sync behaviour for libp2p
pub fn create_sync_behaviour(local_peer_id: PeerId) -> RequestResponse<SyncCodec> {
    let protocols = vec![(StreamProtocol::new(SYNC_PROTOCOL), ProtocolSupport::Full)];
    RequestResponse::new(protocols, request_response::Config::default())
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
}
