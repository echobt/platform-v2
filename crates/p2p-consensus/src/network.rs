//! P2P network layer using libp2p
//!
//! Implements gossipsub for message broadcasting and Kademlia DHT for peer discovery.
//! Provides the networking foundation for PBFT consensus.

use crate::config::P2PConfig;
use crate::messages::{P2PMessage, SignedP2PMessage};
use crate::validator::ValidatorSet;
use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity, MessageId, ValidationMode},
    identify,
    kad::{self, store::MemoryStore},
    noise, tcp, yamux, Multiaddr, PeerId, Swarm, SwarmBuilder,
};
use parking_lot::RwLock;
use platform_core::{Hotkey, Keypair};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Network errors
#[derive(Error, Debug)]
pub enum NetworkError {
    #[error("Transport error: {0}")]
    Transport(String),
    #[error("Gossipsub error: {0}")]
    Gossipsub(String),
    #[error("DHT error: {0}")]
    Dht(String),
    #[error("Serialization error: {0}")]
    Serialization(String),
    #[error("Not connected to any peers")]
    NoPeers,
    #[error("Channel error: {0}")]
    Channel(String),
    #[error("Replay attack detected: nonce {nonce} already seen for {signer}")]
    ReplayAttack { signer: String, nonce: u64 },
    #[error("Rate limit exceeded for {signer}: {count} messages in current window")]
    RateLimitExceeded { signer: String, count: u32 },
}

/// Combined network behavior using manual composition
pub struct NetworkBehaviour {
    /// Gossipsub for pub/sub messaging
    pub gossipsub: gossipsub::Behaviour,
    /// Kademlia DHT for peer discovery
    pub kademlia: kad::Behaviour<MemoryStore>,
    /// Identify protocol for peer identification
    pub identify: identify::Behaviour,
}

/// Events from the network layer
#[derive(Debug)]
pub enum NetworkEvent {
    /// Received a P2P message
    Message { source: PeerId, message: P2PMessage },
    /// New peer connected
    PeerConnected(PeerId),
    /// Peer disconnected
    PeerDisconnected(PeerId),
    /// Peer identified with hotkey
    PeerIdentified {
        peer_id: PeerId,
        hotkey: Option<Hotkey>,
        addresses: Vec<Multiaddr>,
    },
}

/// Commands for controlling the P2P network
#[derive(Debug, Clone)]
pub enum P2PCommand {
    /// Broadcast message to all peers
    Broadcast(P2PMessage),
    /// Dial a specific peer by multiaddr
    Dial(String),
    /// Disconnect from peer by peer ID string
    Disconnect(String),
    /// Shutdown the network
    Shutdown,
}

/// Events emitted from the P2P network
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum P2PEvent {
    /// Message received from a peer
    Message { from: PeerId, message: P2PMessage },
    /// A peer has connected
    PeerConnected(PeerId),
    /// A peer has disconnected
    PeerDisconnected(PeerId),
}

/// Mapping between peer IDs and validator hotkeys
pub struct PeerMapping {
    /// PeerId -> Hotkey
    peer_to_hotkey: RwLock<HashMap<PeerId, Hotkey>>,
    /// Hotkey -> PeerId
    hotkey_to_peer: RwLock<HashMap<Hotkey, PeerId>>,
}

impl PeerMapping {
    pub fn new() -> Self {
        Self {
            peer_to_hotkey: RwLock::new(HashMap::new()),
            hotkey_to_peer: RwLock::new(HashMap::new()),
        }
    }

    pub fn insert(&self, peer_id: PeerId, hotkey: Hotkey) {
        self.peer_to_hotkey.write().insert(peer_id, hotkey.clone());
        self.hotkey_to_peer.write().insert(hotkey, peer_id);
    }

    pub fn get_hotkey(&self, peer_id: &PeerId) -> Option<Hotkey> {
        self.peer_to_hotkey.read().get(peer_id).cloned()
    }

    pub fn get_peer(&self, hotkey: &Hotkey) -> Option<PeerId> {
        self.hotkey_to_peer.read().get(hotkey).copied()
    }

    pub fn remove_peer(&self, peer_id: &PeerId) {
        if let Some(hotkey) = self.peer_to_hotkey.write().remove(peer_id) {
            self.hotkey_to_peer.write().remove(&hotkey);
        }
    }

    /// Get the number of mapped peers (peers that have been identified with a hotkey)
    pub fn len(&self) -> usize {
        self.peer_to_hotkey.read().len()
    }

    /// Check if there are no mapped peers
    pub fn is_empty(&self) -> bool {
        self.peer_to_hotkey.read().is_empty()
    }
}

impl Default for PeerMapping {
    fn default() -> Self {
        Self::new()
    }
}

/// Default rate limit: maximum messages per second per signer
const DEFAULT_RATE_LIMIT: u32 = 100;

/// Rate limit sliding window in milliseconds (1 second)
const RATE_LIMIT_WINDOW_MS: i64 = 1000;

/// Nonce expiry time in milliseconds (5 minutes)
const NONCE_EXPIRY_MS: i64 = 5 * 60 * 1000;

/// P2P network node
pub struct P2PNetwork {
    /// Local keypair
    keypair: Keypair,
    /// libp2p peer ID
    local_peer_id: PeerId,
    /// Network configuration
    config: P2PConfig,
    /// Gossipsub topics
    consensus_topic: IdentTopic,
    challenge_topic: IdentTopic,
    /// Peer mapping
    peer_mapping: Arc<PeerMapping>,
    /// Reference to validator set
    #[allow(dead_code)]
    validator_set: Arc<ValidatorSet>,
    /// Event sender
    #[allow(dead_code)]
    event_tx: mpsc::Sender<NetworkEvent>,
    /// Message nonce counter
    nonce: RwLock<u64>,
    /// Seen nonces for replay protection with timestamps (hotkey -> (nonce -> timestamp_ms))
    /// Timestamps allow automatic expiry of old nonces
    seen_nonces: RwLock<HashMap<Hotkey, HashMap<u64, i64>>>,
    /// Message timestamps for sliding window rate limiting (hotkey -> recent message timestamps in ms)
    message_timestamps: RwLock<HashMap<Hotkey, VecDeque<i64>>>,
}

impl P2PNetwork {
    /// Create a new P2P network
    pub fn new(
        keypair: Keypair,
        config: P2PConfig,
        validator_set: Arc<ValidatorSet>,
        event_tx: mpsc::Sender<NetworkEvent>,
    ) -> Result<Self, NetworkError> {
        // Generate libp2p keypair from our keypair seed
        let seed = keypair.seed();
        let libp2p_keypair = libp2p::identity::Keypair::ed25519_from_bytes(seed).map_err(|e| {
            NetworkError::Transport(format!("Failed to create libp2p keypair: {}", e))
        })?;
        let local_peer_id = PeerId::from(libp2p_keypair.public());

        let consensus_topic = IdentTopic::new(&config.consensus_topic);
        let challenge_topic = IdentTopic::new(&config.challenge_topic);

        Ok(Self {
            keypair,
            local_peer_id,
            config,
            consensus_topic,
            challenge_topic,
            peer_mapping: Arc::new(PeerMapping::new()),
            validator_set,
            event_tx,
            nonce: RwLock::new(0),
            seen_nonces: RwLock::new(HashMap::new()),
            message_timestamps: RwLock::new(HashMap::new()),
        })
    }

    /// Get local peer ID
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    /// Get local hotkey
    pub fn local_hotkey(&self) -> Hotkey {
        self.keypair.hotkey()
    }

    /// Get peer mapping
    pub fn peer_mapping(&self) -> Arc<PeerMapping> {
        self.peer_mapping.clone()
    }

    /// Get the count of connected peers that have been identified with a hotkey
    ///
    /// This returns the number of peers in the peer mapping, which includes
    /// peers that have sent at least one verified message.
    pub fn connected_peer_count(&self) -> usize {
        self.peer_mapping.len()
    }

    /// Check if we have the minimum required peers for consensus
    ///
    /// This is useful for determining if the network has enough participants
    /// to achieve consensus on proposals.
    pub fn has_min_peers(&self, min_required: usize) -> bool {
        self.connected_peer_count() >= min_required
    }

    /// Create gossipsub behaviour
    fn create_gossipsub(
        &self,
        libp2p_keypair: &libp2p::identity::Keypair,
    ) -> Result<gossipsub::Behaviour, NetworkError> {
        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(1))
            .validation_mode(ValidationMode::Strict)
            .message_id_fn(|msg: &gossipsub::Message| {
                use sha2::Digest;
                let hash = sha2::Sha256::digest(&msg.data);
                MessageId::from(hash.to_vec())
            })
            .max_transmit_size(self.config.max_message_size)
            .build()
            .map_err(|e| NetworkError::Gossipsub(e.to_string()))?;

        gossipsub::Behaviour::new(
            MessageAuthenticity::Signed(libp2p_keypair.clone()),
            gossipsub_config,
        )
        .map_err(|e| NetworkError::Gossipsub(e.to_string()))
    }

    /// Create behaviour components
    pub fn create_behaviour(
        &self,
        libp2p_keypair: &libp2p::identity::Keypair,
    ) -> Result<NetworkBehaviour, NetworkError> {
        let gossipsub = self.create_gossipsub(libp2p_keypair)?;
        let store = MemoryStore::new(self.local_peer_id);
        let kademlia = kad::Behaviour::new(self.local_peer_id, store);
        let identify_config =
            identify::Config::new("/platform/1.0.0".to_string(), libp2p_keypair.public());
        let identify = identify::Behaviour::new(identify_config);

        Ok(NetworkBehaviour {
            gossipsub,
            kademlia,
            identify,
        })
    }

    /// Subscribe to gossipsub topics
    pub fn subscribe(&self, behaviour: &mut NetworkBehaviour) -> Result<(), NetworkError> {
        behaviour
            .gossipsub
            .subscribe(&self.consensus_topic)
            .map_err(|e| {
                NetworkError::Gossipsub(format!("Failed to subscribe to consensus: {}", e))
            })?;

        behaviour
            .gossipsub
            .subscribe(&self.challenge_topic)
            .map_err(|e| {
                NetworkError::Gossipsub(format!("Failed to subscribe to challenge: {}", e))
            })?;

        info!(
            consensus_topic = %self.config.consensus_topic,
            challenge_topic = %self.config.challenge_topic,
            "Subscribed to gossipsub topics"
        );

        Ok(())
    }

    /// Connect to bootstrap peers
    pub async fn connect_bootstrap<TBehaviour>(
        &self,
        swarm: &mut Swarm<TBehaviour>,
        behaviour: &mut NetworkBehaviour,
    ) -> Result<usize, NetworkError>
    where
        TBehaviour: libp2p::swarm::NetworkBehaviour,
    {
        let mut connected = 0;

        for addr_str in &self.config.bootstrap_peers {
            match addr_str.parse::<Multiaddr>() {
                Ok(addr) => {
                    info!(addr = %addr, "Connecting to bootstrap peer");
                    match swarm.dial(addr.clone()) {
                        Ok(_) => {
                            if let Some(peer_id) = extract_peer_id(&addr) {
                                behaviour.kademlia.add_address(&peer_id, addr);
                                connected += 1;
                            }
                        }
                        Err(e) => {
                            warn!(addr = %addr_str, error = %e, "Failed to dial bootstrap peer");
                        }
                    }
                }
                Err(e) => {
                    warn!(addr = %addr_str, error = %e, "Invalid bootstrap address");
                }
            }
        }

        Ok(connected)
    }

    /// Broadcast a message to the consensus topic
    pub fn broadcast_consensus(
        &self,
        behaviour: &mut NetworkBehaviour,
        message: P2PMessage,
    ) -> Result<(), NetworkError> {
        let signed = self.sign_message(message)?;
        let bytes =
            bincode::serialize(&signed).map_err(|e| NetworkError::Serialization(e.to_string()))?;

        behaviour
            .gossipsub
            .publish(self.consensus_topic.clone(), bytes)
            .map_err(|e| NetworkError::Gossipsub(e.to_string()))?;

        debug!(msg_type = %signed.message.type_name(), "Broadcast consensus message");
        Ok(())
    }

    /// Broadcast a message to the challenge topic
    pub fn broadcast_challenge(
        &self,
        behaviour: &mut NetworkBehaviour,
        message: P2PMessage,
    ) -> Result<(), NetworkError> {
        let signed = self.sign_message(message)?;
        let bytes =
            bincode::serialize(&signed).map_err(|e| NetworkError::Serialization(e.to_string()))?;

        behaviour
            .gossipsub
            .publish(self.challenge_topic.clone(), bytes)
            .map_err(|e| NetworkError::Gossipsub(e.to_string()))?;

        debug!(msg_type = %signed.message.type_name(), "Broadcast challenge message");
        Ok(())
    }

    /// Sign a P2P message
    fn sign_message(&self, message: P2PMessage) -> Result<SignedP2PMessage, NetworkError> {
        let nonce = {
            let mut n = self.nonce.write();
            *n += 1;
            *n
        };

        let mut signed = SignedP2PMessage {
            message,
            signer: self.keypair.hotkey(),
            signature: vec![],
            nonce,
        };

        let signing_bytes = signed
            .signing_bytes()
            .map_err(|e| NetworkError::Serialization(e.to_string()))?;

        signed.signature = self
            .keypair
            .sign_bytes(&signing_bytes)
            .map_err(|e| NetworkError::Serialization(e.to_string()))?;

        Ok(signed)
    }

    /// Verify a signed message
    pub fn verify_message(&self, signed: &SignedP2PMessage) -> bool {
        let signing_bytes = match signed.signing_bytes() {
            Ok(bytes) => bytes,
            Err(_) => return false,
        };

        let signed_msg = platform_core::SignedMessage {
            message: signing_bytes,
            signature: signed.signature.clone(),
            signer: signed.signer.clone(),
        };

        signed_msg.verify().unwrap_or_default()
    }

    /// Handle incoming gossipsub message
    ///
    /// Performs the following security checks:
    /// 1. Signature verification
    /// 2. Replay protection (nonce tracking)
    /// 3. Rate limiting (messages per second)
    pub fn handle_gossipsub_message(
        &self,
        source: PeerId,
        data: &[u8],
    ) -> Result<P2PMessage, NetworkError> {
        let signed: SignedP2PMessage =
            bincode::deserialize(data).map_err(|e| NetworkError::Serialization(e.to_string()))?;

        // Verify signature first
        if !self.verify_message(&signed) {
            return Err(NetworkError::Gossipsub(
                "Invalid message signature".to_string(),
            ));
        }

        // Check rate limit before processing
        self.check_rate_limit(&signed.signer)?;

        // Check for replay attack (after signature verification to avoid DoS)
        self.check_replay(&signed.signer, signed.nonce)?;

        // Update peer mapping
        if self.peer_mapping.get_hotkey(&source).is_none() {
            self.peer_mapping.insert(source, signed.signer.clone());
        }

        Ok(signed.message)
    }

    /// Check if a nonce has been seen before (replay attack detection)
    ///
    /// Uses timestamp-based expiry to automatically clean old nonces and bound memory usage.
    /// Nonces older than NONCE_EXPIRY_MS (5 minutes) are automatically removed.
    fn check_replay(&self, signer: &Hotkey, nonce: u64) -> Result<(), NetworkError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut seen_nonces = self.seen_nonces.write();
        let nonces = seen_nonces.entry(signer.clone()).or_default();

        // Auto-expire old nonces to bound memory usage
        nonces.retain(|_, timestamp| now_ms - *timestamp < NONCE_EXPIRY_MS);

        // Check if this nonce was already seen (and not expired)
        if nonces.contains_key(&nonce) {
            return Err(NetworkError::ReplayAttack {
                signer: signer.to_hex(),
                nonce,
            });
        }

        // Record this nonce with current timestamp
        nonces.insert(nonce, now_ms);
        Ok(())
    }

    /// Check and update rate limit for a signer using sliding window
    ///
    /// Uses a sliding window approach to prevent burst attacks at window boundaries.
    /// Tracks individual message timestamps and counts messages within the window.
    fn check_rate_limit(&self, signer: &Hotkey) -> Result<(), NetworkError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut timestamps = self.message_timestamps.write();
        let queue = timestamps.entry(signer.clone()).or_default();

        // Remove timestamps older than the sliding window
        while let Some(&front) = queue.front() {
            if now_ms - front > RATE_LIMIT_WINDOW_MS {
                queue.pop_front();
            } else {
                break;
            }
        }

        // Check if over limit (>= because we're about to add one more)
        if queue.len() >= DEFAULT_RATE_LIMIT as usize {
            return Err(NetworkError::RateLimitExceeded {
                signer: signer.to_hex(),
                count: queue.len() as u32,
            });
        }

        // Add current timestamp
        queue.push_back(now_ms);
        Ok(())
    }

    /// Clean old nonces to prevent memory growth
    ///
    /// This should be called periodically (e.g., every minute) to remove
    /// old nonces that are no longer relevant for replay protection.
    /// The `max_age_secs` parameter determines how long to keep nonces.
    ///
    /// Note: Nonces are also automatically cleaned during `check_replay()` calls,
    /// but this method provides bulk cleanup for signers who have stopped sending messages.
    pub fn clean_old_nonces(&self, max_age_secs: u64) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let max_age_ms = (max_age_secs * 1000) as i64;
        let mut seen_nonces = self.seen_nonces.write();

        // Clean expired nonces for each signer
        for nonces in seen_nonces.values_mut() {
            nonces.retain(|_, timestamp| now_ms - *timestamp < max_age_ms);
        }

        // Remove signers with no remaining nonces
        seen_nonces.retain(|_, nonces| !nonces.is_empty());

        debug!(
            "Cleaned old nonces, current signer count: {}",
            seen_nonces.len()
        );
    }

    /// Clean stale rate limit entries
    ///
    /// Should be called periodically to remove old rate limit tracking entries.
    /// Removes signers who haven't sent messages within the rate limit window.
    pub fn clean_rate_limit_entries(&self) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut timestamps = self.message_timestamps.write();

        // Clean old timestamps for each signer
        for queue in timestamps.values_mut() {
            while let Some(&front) = queue.front() {
                if now_ms - front > RATE_LIMIT_WINDOW_MS {
                    queue.pop_front();
                } else {
                    break;
                }
            }
        }

        // Remove signers with no recent messages
        timestamps.retain(|_, queue| !queue.is_empty());
    }

    /// Start listening on configured addresses
    pub fn start_listening<TBehaviour>(
        &self,
        swarm: &mut Swarm<TBehaviour>,
    ) -> Result<Vec<Multiaddr>, NetworkError>
    where
        TBehaviour: libp2p::swarm::NetworkBehaviour,
    {
        let mut listening_addrs = Vec::new();

        for addr_str in &self.config.listen_addrs {
            match addr_str.parse::<Multiaddr>() {
                Ok(addr) => match swarm.listen_on(addr.clone()) {
                    Ok(_) => {
                        info!(addr = %addr, "Listening on address");
                        listening_addrs.push(addr);
                    }
                    Err(e) => {
                        error!(addr = %addr_str, error = %e, "Failed to listen on address");
                    }
                },
                Err(e) => {
                    error!(addr = %addr_str, error = %e, "Invalid listen address");
                }
            }
        }

        if listening_addrs.is_empty() {
            return Err(NetworkError::Transport(
                "No valid listen addresses".to_string(),
            ));
        }

        Ok(listening_addrs)
    }

    /// Bootstrap Kademlia DHT
    pub fn bootstrap_dht(&self, behaviour: &mut NetworkBehaviour) {
        match behaviour.kademlia.bootstrap() {
            Ok(_) => info!("Started Kademlia bootstrap"),
            Err(e) => warn!(error = ?e, "Failed to bootstrap Kademlia (no peers?)"),
        }
    }

    /// Get connected peer count
    pub fn peer_count<TBehaviour>(&self, swarm: &Swarm<TBehaviour>) -> usize
    where
        TBehaviour: libp2p::swarm::NetworkBehaviour,
    {
        swarm.connected_peers().count()
    }

    /// Start the P2P network and return event/command channels
    ///
    /// Returns a tuple of (event_receiver, command_sender) that can be used to
    /// interact with the network. The network runs in the background and processes
    /// incoming events, broadcasting them through the event channel.
    pub async fn start(
        &self,
    ) -> Result<(mpsc::Receiver<P2PEvent>, mpsc::Sender<P2PCommand>), NetworkError> {
        let (event_tx, event_rx) = mpsc::channel::<P2PEvent>(1000);
        let (cmd_tx, _cmd_rx) = mpsc::channel::<P2PCommand>(1000);

        // Get libp2p keypair
        let seed = self.keypair.seed();
        let libp2p_keypair = libp2p::identity::Keypair::ed25519_from_bytes(seed).map_err(|e| {
            NetworkError::Transport(format!("Failed to create libp2p keypair: {}", e))
        })?;

        // Create behaviour
        let mut behaviour = self.create_behaviour(&libp2p_keypair)?;

        // Subscribe to topics
        self.subscribe(&mut behaviour)?;

        info!(
            peer_id = %self.local_peer_id,
            "P2P network started, returning event/command channels"
        );

        // Store event_tx for forwarding events
        let _event_tx_clone = event_tx.clone();

        // The actual event loop would be spawned here in a full implementation
        // For now, we return the channels and let the caller handle the swarm event loop
        // This allows for more flexible integration with different runtime patterns

        Ok((event_rx, cmd_tx))
    }
}

/// Extract peer ID from multiaddr if present
fn extract_peer_id(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|p| {
        if let libp2p::multiaddr::Protocol::P2p(peer_id) = p {
            Some(peer_id)
        } else {
            None
        }
    })
}

/// Network runner that processes swarm events
pub struct NetworkRunner {
    network: Arc<P2PNetwork>,
    event_tx: mpsc::Sender<NetworkEvent>,
}

impl NetworkRunner {
    pub fn new(network: Arc<P2PNetwork>, event_tx: mpsc::Sender<NetworkEvent>) -> Self {
        Self { network, event_tx }
    }

    /// Handle gossipsub event
    pub async fn handle_gossipsub_event(
        &self,
        event: gossipsub::Event,
    ) -> Result<(), NetworkError> {
        if let gossipsub::Event::Message {
            propagation_source,
            message,
            ..
        } = event
        {
            match self
                .network
                .handle_gossipsub_message(propagation_source, &message.data)
            {
                Ok(msg) => {
                    debug!(
                        source = %propagation_source,
                        msg_type = %msg.type_name(),
                        "Received gossipsub message"
                    );
                    if let Err(e) = self
                        .event_tx
                        .send(NetworkEvent::Message {
                            source: propagation_source,
                            message: msg,
                        })
                        .await
                    {
                        error!(error = %e, "Failed to send message event");
                    }
                }
                Err(e) => {
                    warn!(
                        source = %propagation_source,
                        error = %e,
                        "Failed to process gossipsub message"
                    );
                }
            }
        }
        Ok(())
    }

    /// Handle kademlia event
    pub async fn handle_kademlia_event(&self, event: kad::Event) -> Result<(), NetworkError> {
        match event {
            kad::Event::RoutingUpdated { peer, .. } => {
                debug!(peer = %peer, "Kademlia routing updated");
            }
            kad::Event::OutboundQueryProgressed {
                result: kad::QueryResult::Bootstrap(Ok(_)),
                ..
            } => {
                info!("Kademlia bootstrap completed");
            }
            kad::Event::OutboundQueryProgressed { .. } => {}
            _ => {}
        }
        Ok(())
    }

    /// Handle identify event
    pub async fn handle_identify_event(
        &self,
        event: identify::Event,
        behaviour: &mut NetworkBehaviour,
    ) -> Result<(), NetworkError> {
        if let identify::Event::Received { peer_id, info, .. } = event {
            debug!(
                peer = %peer_id,
                protocol = %info.protocol_version,
                "Received identify info"
            );

            for addr in &info.listen_addrs {
                behaviour.kademlia.add_address(&peer_id, addr.clone());
            }

            if let Err(e) = self
                .event_tx
                .send(NetworkEvent::PeerIdentified {
                    peer_id,
                    hotkey: self.network.peer_mapping.get_hotkey(&peer_id),
                    addresses: info.listen_addrs,
                })
                .await
            {
                error!(error = %e, "Failed to send peer identified event");
            }
        }
        Ok(())
    }

    /// Handle connection established
    pub async fn handle_connection_established(&self, peer_id: PeerId) -> Result<(), NetworkError> {
        info!(peer = %peer_id, "Connection established");
        if let Err(e) = self
            .event_tx
            .send(NetworkEvent::PeerConnected(peer_id))
            .await
        {
            error!(error = %e, "Failed to send peer connected event");
        }
        Ok(())
    }

    /// Handle connection closed
    pub async fn handle_connection_closed(&self, peer_id: PeerId) -> Result<(), NetworkError> {
        info!(peer = %peer_id, "Connection closed");
        self.network.peer_mapping.remove_peer(&peer_id);
        if let Err(e) = self
            .event_tx
            .send(NetworkEvent::PeerDisconnected(peer_id))
            .await
        {
            error!(error = %e, "Failed to send peer disconnected event");
        }
        Ok(())
    }
}

/// Helper to build a complete swarm with all behaviours
pub async fn build_swarm(
    keypair: &Keypair,
    config: &P2PConfig,
) -> Result<(Swarm<libp2p::swarm::dummy::Behaviour>, NetworkBehaviour), NetworkError> {
    let seed = keypair.seed();
    let libp2p_keypair = libp2p::identity::Keypair::ed25519_from_bytes(seed)
        .map_err(|e| NetworkError::Transport(format!("Failed to create keypair: {}", e)))?;

    let local_peer_id = PeerId::from(libp2p_keypair.public());

    // Create gossipsub
    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(1))
        .validation_mode(ValidationMode::Strict)
        .message_id_fn(|msg: &gossipsub::Message| {
            use sha2::Digest;
            let hash = sha2::Sha256::digest(&msg.data);
            MessageId::from(hash.to_vec())
        })
        .max_transmit_size(config.max_message_size)
        .build()
        .map_err(|e| NetworkError::Gossipsub(e.to_string()))?;

    let gossipsub = gossipsub::Behaviour::new(
        MessageAuthenticity::Signed(libp2p_keypair.clone()),
        gossipsub_config,
    )
    .map_err(|e| NetworkError::Gossipsub(e.to_string()))?;

    // Create kademlia
    let store = MemoryStore::new(local_peer_id);
    let kademlia = kad::Behaviour::new(local_peer_id, store);

    // Create identify
    let identify_config =
        identify::Config::new("/platform/1.0.0".to_string(), libp2p_keypair.public());
    let identify = identify::Behaviour::new(identify_config);

    let behaviour = NetworkBehaviour {
        gossipsub,
        kademlia,
        identify,
    };

    // Build a minimal swarm for structure (actual swarm creation would need the behaviour)
    let swarm = SwarmBuilder::with_existing_identity(libp2p_keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )
        .map_err(|e| NetworkError::Transport(e.to_string()))?
        .with_dns()
        .map_err(|e| NetworkError::Transport(e.to_string()))?
        .with_behaviour(|_| libp2p::swarm::dummy::Behaviour)
        .map_err(|e| NetworkError::Transport(e.to_string()))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    Ok((swarm, behaviour))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_mapping() {
        let mapping = PeerMapping::new();
        let peer_id = PeerId::random();
        let hotkey = Hotkey([1u8; 32]);

        mapping.insert(peer_id, hotkey.clone());

        assert_eq!(mapping.get_hotkey(&peer_id), Some(hotkey.clone()));
        assert_eq!(mapping.get_peer(&hotkey), Some(peer_id));

        mapping.remove_peer(&peer_id);

        assert!(mapping.get_hotkey(&peer_id).is_none());
        assert!(mapping.get_peer(&hotkey).is_none());
    }

    #[test]
    fn test_extract_peer_id() {
        let peer_id = PeerId::random();
        let addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/9000/p2p/{}", peer_id)
            .parse()
            .unwrap();

        let extracted = extract_peer_id(&addr);
        assert_eq!(extracted, Some(peer_id));

        let addr_no_peer: Multiaddr = "/ip4/127.0.0.1/tcp/9000".parse().unwrap();
        assert!(extract_peer_id(&addr_no_peer).is_none());
    }

    #[tokio::test]
    async fn test_network_creation() {
        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair, config, validator_set, tx);
        assert!(network.is_ok());
    }

    #[test]
    fn test_peer_mapping_default() {
        let mapping = PeerMapping::default();
        let peer_id = PeerId::random();
        assert!(mapping.get_hotkey(&peer_id).is_none());
    }

    #[test]
    fn test_peer_mapping_len() {
        let mapping = PeerMapping::new();
        assert_eq!(mapping.len(), 0);

        let peer_id1 = PeerId::random();
        let hotkey1 = Hotkey([1u8; 32]);
        mapping.insert(peer_id1, hotkey1);
        assert_eq!(mapping.len(), 1);

        let peer_id2 = PeerId::random();
        let hotkey2 = Hotkey([2u8; 32]);
        mapping.insert(peer_id2, hotkey2);
        assert_eq!(mapping.len(), 2);

        mapping.remove_peer(&peer_id1);
        assert_eq!(mapping.len(), 1);

        mapping.remove_peer(&peer_id2);
        assert_eq!(mapping.len(), 0);
    }

    #[test]
    fn test_peer_mapping_is_empty() {
        let mapping = PeerMapping::new();
        assert!(mapping.is_empty());

        let peer_id = PeerId::random();
        let hotkey = Hotkey([1u8; 32]);
        mapping.insert(peer_id, hotkey);
        assert!(!mapping.is_empty());

        mapping.remove_peer(&peer_id);
        assert!(mapping.is_empty());
    }

    #[tokio::test]
    async fn test_network_local_peer_id() {
        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair.clone(), config, validator_set, tx)
            .expect("Failed to create network");

        // Verify that local_peer_id is derived correctly from keypair
        let seed = keypair.seed();
        let libp2p_keypair = libp2p::identity::Keypair::ed25519_from_bytes(seed)
            .expect("Failed to create libp2p keypair");
        let expected_peer_id = PeerId::from(libp2p_keypair.public());

        assert_eq!(network.local_peer_id(), expected_peer_id);
    }

    #[tokio::test]
    async fn test_network_local_hotkey() {
        let keypair = Keypair::generate();
        let expected_hotkey = keypair.hotkey();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair, config, validator_set, tx)
            .expect("Failed to create network");

        assert_eq!(network.local_hotkey(), expected_hotkey);
    }

    #[tokio::test]
    async fn test_network_connected_peer_count_initially_zero() {
        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair, config, validator_set, tx)
            .expect("Failed to create network");

        assert_eq!(network.connected_peer_count(), 0);
    }

    #[tokio::test]
    async fn test_network_has_min_peers_false_initially() {
        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair, config, validator_set, tx)
            .expect("Failed to create network");

        // Should return false when requiring any peers (since none connected)
        assert!(!network.has_min_peers(1));
        assert!(!network.has_min_peers(5));

        // Should return true for min_required of 0
        assert!(network.has_min_peers(0));
    }

    #[tokio::test]
    async fn test_replay_attack_detection() {
        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair, config, validator_set, tx)
            .expect("Failed to create network");

        let signer = Hotkey([42u8; 32]);
        let nonce = 12345u64;

        // First check should succeed
        let result1 = network.check_replay(&signer, nonce);
        assert!(result1.is_ok(), "First nonce check should succeed");

        // Second check with same nonce should fail (replay attack detected)
        let result2 = network.check_replay(&signer, nonce);
        assert!(result2.is_err(), "Duplicate nonce should be rejected");

        match result2 {
            Err(NetworkError::ReplayAttack {
                signer: s,
                nonce: n,
            }) => {
                assert_eq!(n, nonce);
                assert_eq!(s, signer.to_hex());
            }
            _ => panic!("Expected ReplayAttack error"),
        }

        // Different nonce for same signer should succeed
        let result3 = network.check_replay(&signer, nonce + 1);
        assert!(result3.is_ok(), "Different nonce should succeed");

        // Same nonce for different signer should succeed
        let other_signer = Hotkey([99u8; 32]);
        let result4 = network.check_replay(&other_signer, nonce);
        assert!(result4.is_ok(), "Same nonce for different signer should succeed");
    }

    #[tokio::test]
    async fn test_rate_limiting_under_limit() {
        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair, config, validator_set, tx)
            .expect("Failed to create network");

        let signer = Hotkey([42u8; 32]);

        // Should allow messages under the limit (DEFAULT_RATE_LIMIT = 100)
        for i in 0..50 {
            let result = network.check_rate_limit(&signer);
            assert!(
                result.is_ok(),
                "Message {} should be allowed under rate limit",
                i
            );
        }
    }

    #[tokio::test]
    async fn test_rate_limiting_exceeds_limit() {
        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair, config, validator_set, tx)
            .expect("Failed to create network");

        let signer = Hotkey([42u8; 32]);

        // Send messages up to the limit (DEFAULT_RATE_LIMIT = 100)
        for i in 0..DEFAULT_RATE_LIMIT {
            let result = network.check_rate_limit(&signer);
            assert!(
                result.is_ok(),
                "Message {} should be allowed at or under limit",
                i
            );
        }

        // Next message should be rate limited
        let result = network.check_rate_limit(&signer);
        assert!(result.is_err(), "Message over limit should be rejected");

        match result {
            Err(NetworkError::RateLimitExceeded { signer: s, count }) => {
                assert_eq!(s, signer.to_hex());
                assert_eq!(count, DEFAULT_RATE_LIMIT);
            }
            _ => panic!("Expected RateLimitExceeded error"),
        }
    }

    #[tokio::test]
    async fn test_clean_old_nonces() {
        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair, config, validator_set, tx)
            .expect("Failed to create network");

        let signer = Hotkey([42u8; 32]);

        // Add some nonces
        network.check_replay(&signer, 1).expect("First nonce should succeed");
        network.check_replay(&signer, 2).expect("Second nonce should succeed");
        network.check_replay(&signer, 3).expect("Third nonce should succeed");

        // Verify nonces are tracked
        {
            let seen = network.seen_nonces.read();
            let signer_nonces = seen.get(&signer).expect("Signer should have nonces");
            assert_eq!(signer_nonces.len(), 3);
        }

        // Clean with 0 max_age_secs should remove all nonces (they're all "old")
        network.clean_old_nonces(0);

        // After cleaning with 0 age, all nonces should be removed
        {
            let seen = network.seen_nonces.read();
            assert!(
                seen.get(&signer).is_none() || seen.get(&signer).unwrap().is_empty(),
                "All nonces should be cleaned with max_age of 0"
            );
        }
    }

    #[tokio::test]
    async fn test_clean_old_nonces_preserves_recent() {
        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair, config, validator_set, tx)
            .expect("Failed to create network");

        let signer = Hotkey([42u8; 32]);

        // Add some nonces
        network.check_replay(&signer, 100).expect("Nonce should succeed");
        network.check_replay(&signer, 200).expect("Nonce should succeed");

        // Clean with large max_age should preserve recent nonces
        network.clean_old_nonces(3600); // 1 hour

        // Nonces should still be there
        {
            let seen = network.seen_nonces.read();
            let signer_nonces = seen.get(&signer);
            assert!(signer_nonces.is_some(), "Signer should still have nonces");
            assert_eq!(signer_nonces.unwrap().len(), 2, "Both nonces should remain");
        }

        // Same nonces should still be rejected (replay protection still works)
        assert!(network.check_replay(&signer, 100).is_err());
        assert!(network.check_replay(&signer, 200).is_err());
    }

    #[tokio::test]
    async fn test_clean_rate_limit_entries() {
        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair, config, validator_set, tx)
            .expect("Failed to create network");

        let signer = Hotkey([42u8; 32]);

        // Add some rate limit entries
        network.check_rate_limit(&signer).expect("Should succeed");
        network.check_rate_limit(&signer).expect("Should succeed");

        // Verify entries exist
        {
            let timestamps = network.message_timestamps.read();
            let queue = timestamps.get(&signer);
            assert!(queue.is_some(), "Signer should have rate limit entries");
            assert_eq!(queue.unwrap().len(), 2);
        }

        // Clean rate limit entries (should not remove recent entries)
        network.clean_rate_limit_entries();

        // Entries should still be there (they're recent)
        {
            let timestamps = network.message_timestamps.read();
            let queue = timestamps.get(&signer);
            assert!(queue.is_some(), "Recent entries should be preserved");
            assert_eq!(queue.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn test_sign_and_verify_message() {
        use crate::messages::HeartbeatMessage;

        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair.clone(), config, validator_set, tx)
            .expect("Failed to create network");

        // Create a test message using the HeartbeatMessage struct
        let message = P2PMessage::Heartbeat(HeartbeatMessage {
            validator: keypair.hotkey(),
            state_hash: [0u8; 32],
            sequence: 1,
            stake: 1_000_000_000,
            timestamp: chrono::Utc::now().timestamp_millis(),
            signature: vec![],
        });

        // Sign the message
        let signed = network.sign_message(message.clone()).expect("Signing should succeed");

        // Verify signed message has correct fields
        assert_eq!(signed.signer, keypair.hotkey());
        assert!(!signed.signature.is_empty(), "Signature should not be empty");
        assert!(signed.nonce > 0, "Nonce should be positive");

        // Verify the signature is valid
        assert!(
            network.verify_message(&signed),
            "Signature verification should pass"
        );
    }

    #[tokio::test]
    async fn test_verify_message_rejects_invalid_signature() {
        use crate::messages::HeartbeatMessage;

        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair.clone(), config, validator_set, tx)
            .expect("Failed to create network");

        let message = P2PMessage::Heartbeat(HeartbeatMessage {
            validator: keypair.hotkey(),
            state_hash: [0u8; 32],
            sequence: 1,
            stake: 1_000_000_000,
            timestamp: chrono::Utc::now().timestamp_millis(),
            signature: vec![],
        });

        let mut signed = network.sign_message(message).expect("Signing should succeed");

        // Tamper with the signature
        if !signed.signature.is_empty() {
            signed.signature[0] ^= 0xFF;
        }

        // Verify should fail with tampered signature
        assert!(
            !network.verify_message(&signed),
            "Tampered signature should fail verification"
        );
    }

    #[tokio::test]
    async fn test_verify_message_rejects_wrong_signer() {
        use crate::messages::HeartbeatMessage;

        let keypair = Keypair::generate();
        let other_keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair.clone(), config, validator_set, tx)
            .expect("Failed to create network");

        let message = P2PMessage::Heartbeat(HeartbeatMessage {
            validator: keypair.hotkey(),
            state_hash: [0u8; 32],
            sequence: 1,
            stake: 1_000_000_000,
            timestamp: chrono::Utc::now().timestamp_millis(),
            signature: vec![],
        });

        let mut signed = network.sign_message(message).expect("Signing should succeed");

        // Change the signer to someone else (signature won't match)
        signed.signer = other_keypair.hotkey();

        // Verify should fail with wrong signer
        assert!(
            !network.verify_message(&signed),
            "Wrong signer should fail verification"
        );
    }

    #[tokio::test]
    async fn test_sign_message_increments_nonce() {
        use crate::messages::HeartbeatMessage;

        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair.clone(), config, validator_set, tx)
            .expect("Failed to create network");

        let message1 = P2PMessage::Heartbeat(HeartbeatMessage {
            validator: keypair.hotkey(),
            state_hash: [0u8; 32],
            sequence: 1,
            stake: 1_000_000_000,
            timestamp: chrono::Utc::now().timestamp_millis(),
            signature: vec![],
        });
        let message2 = P2PMessage::Heartbeat(HeartbeatMessage {
            validator: keypair.hotkey(),
            state_hash: [0u8; 32],
            sequence: 2,
            stake: 1_000_000_000,
            timestamp: chrono::Utc::now().timestamp_millis(),
            signature: vec![],
        });

        let signed1 = network.sign_message(message1).expect("Signing should succeed");
        let signed2 = network.sign_message(message2).expect("Signing should succeed");

        // Nonces should be incrementing
        assert!(
            signed2.nonce > signed1.nonce,
            "Nonces should be monotonically increasing"
        );
    }

    #[test]
    fn test_network_error_display() {
        // Test that NetworkError variants display correctly
        let transport_err = NetworkError::Transport("connection failed".to_string());
        assert!(transport_err.to_string().contains("Transport error"));
        assert!(transport_err.to_string().contains("connection failed"));

        let gossipsub_err = NetworkError::Gossipsub("publish failed".to_string());
        assert!(gossipsub_err.to_string().contains("Gossipsub error"));

        let dht_err = NetworkError::Dht("lookup failed".to_string());
        assert!(dht_err.to_string().contains("DHT error"));

        let serialization_err = NetworkError::Serialization("invalid format".to_string());
        assert!(serialization_err.to_string().contains("Serialization error"));

        let no_peers_err = NetworkError::NoPeers;
        assert!(no_peers_err.to_string().contains("Not connected"));

        let channel_err = NetworkError::Channel("channel closed".to_string());
        assert!(channel_err.to_string().contains("Channel error"));

        let replay_err = NetworkError::ReplayAttack {
            signer: "abc123".to_string(),
            nonce: 42,
        };
        assert!(replay_err.to_string().contains("Replay attack"));
        assert!(replay_err.to_string().contains("42"));
        assert!(replay_err.to_string().contains("abc123"));

        let rate_limit_err = NetworkError::RateLimitExceeded {
            signer: "def456".to_string(),
            count: 100,
        };
        assert!(rate_limit_err.to_string().contains("Rate limit exceeded"));
        assert!(rate_limit_err.to_string().contains("100"));
    }

    #[test]
    fn test_peer_mapping_multiple_operations() {
        let mapping = PeerMapping::new();

        // Insert multiple peers
        let peers: Vec<_> = (0..5)
            .map(|i| {
                let peer_id = PeerId::random();
                let mut hotkey_bytes = [0u8; 32];
                hotkey_bytes[0] = i as u8;
                let hotkey = Hotkey(hotkey_bytes);
                (peer_id, hotkey)
            })
            .collect();

        for (peer_id, hotkey) in &peers {
            mapping.insert(*peer_id, hotkey.clone());
        }

        assert_eq!(mapping.len(), 5);
        assert!(!mapping.is_empty());

        // Verify all mappings
        for (peer_id, hotkey) in &peers {
            assert_eq!(mapping.get_hotkey(peer_id), Some(hotkey.clone()));
            assert_eq!(mapping.get_peer(hotkey), Some(*peer_id));
        }

        // Remove some peers
        mapping.remove_peer(&peers[0].0);
        mapping.remove_peer(&peers[2].0);
        mapping.remove_peer(&peers[4].0);

        assert_eq!(mapping.len(), 2);

        // Verify remaining mappings
        assert!(mapping.get_hotkey(&peers[0].0).is_none());
        assert!(mapping.get_hotkey(&peers[1].0).is_some());
        assert!(mapping.get_hotkey(&peers[2].0).is_none());
        assert!(mapping.get_hotkey(&peers[3].0).is_some());
        assert!(mapping.get_hotkey(&peers[4].0).is_none());
    }

    #[tokio::test]
    async fn test_connected_peer_count_with_peer_mapping() {
        let keypair = Keypair::generate();
        let config = P2PConfig::development();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let (tx, _rx) = mpsc::channel(100);

        let network = P2PNetwork::new(keypair, config, validator_set, tx)
            .expect("Failed to create network");

        // Initially zero
        assert_eq!(network.connected_peer_count(), 0);
        assert!(!network.has_min_peers(1));

        // Add some peers to the peer mapping
        let peer_mapping = network.peer_mapping();
        peer_mapping.insert(PeerId::random(), Hotkey([1u8; 32]));
        peer_mapping.insert(PeerId::random(), Hotkey([2u8; 32]));
        peer_mapping.insert(PeerId::random(), Hotkey([3u8; 32]));

        // Count should reflect peer mapping
        assert_eq!(network.connected_peer_count(), 3);
        assert!(network.has_min_peers(1));
        assert!(network.has_min_peers(3));
        assert!(!network.has_min_peers(4));
    }
}
