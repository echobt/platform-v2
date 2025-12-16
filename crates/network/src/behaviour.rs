//! Network behaviour combining gossipsub and request-response

use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity, ValidationMode},
    identify, mdns,
    request_response::{self, ProtocolSupport},
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour},
    StreamProtocol,
};
use std::time::Duration;

use crate::protocol::GOSSIP_TOPIC;

/// Combined network behaviour
#[derive(NetworkBehaviour)]
pub struct MiniChainBehaviour {
    /// Gossipsub for broadcast messages
    pub gossipsub: gossipsub::Behaviour,

    /// mDNS for local peer discovery (optional, disabled by default)
    pub mdns: Toggle<mdns::tokio::Behaviour>,

    /// Identify protocol
    pub identify: identify::Behaviour,

    /// Request-response for direct messages
    pub request_response: request_response::cbor::Behaviour<SyncRequest, SyncResponse>,
}

impl MiniChainBehaviour {
    pub fn new(local_key: &libp2p::identity::Keypair, enable_mdns: bool) -> anyhow::Result<Self> {
        // Gossipsub configuration
        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(1))
            .validation_mode(ValidationMode::Strict)
            .message_id_fn(|msg| {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&msg.data, &mut hasher);
                std::hash::Hash::hash(&msg.source, &mut hasher);
                gossipsub::MessageId::from(std::hash::Hasher::finish(&hasher).to_string())
            })
            .build()
            .map_err(|e| anyhow::anyhow!("Gossipsub config error: {}", e))?;

        let gossipsub = gossipsub::Behaviour::new(
            MessageAuthenticity::Signed(local_key.clone()),
            gossipsub_config,
        )
        .map_err(|e| anyhow::anyhow!("Gossipsub error: {}", e))?;

        // mDNS for local discovery (optional)
        let mdns = if enable_mdns {
            Toggle::from(Some(mdns::tokio::Behaviour::new(
                mdns::Config::default(),
                local_key.public().to_peer_id(),
            )?))
        } else {
            Toggle::from(None)
        };

        // Identify protocol
        let identify = identify::Behaviour::new(identify::Config::new(
            "/platform/id/1.0.0".into(),
            local_key.public(),
        ));

        // Request-response for sync
        let request_response = request_response::cbor::Behaviour::new(
            [(
                StreamProtocol::new("/platform/sync/1.0.0"),
                ProtocolSupport::Full,
            )],
            request_response::Config::default(),
        );

        Ok(Self {
            gossipsub,
            mdns,
            identify,
            request_response,
        })
    }

    /// Subscribe to the gossip topic
    pub fn subscribe(&mut self) -> anyhow::Result<()> {
        let topic = IdentTopic::new(GOSSIP_TOPIC);
        self.gossipsub
            .subscribe(&topic)
            .map_err(|e| anyhow::anyhow!("Subscribe error: {:?}", e))?;
        Ok(())
    }

    /// Publish a message to the gossip topic
    pub fn publish(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        let topic = IdentTopic::new(GOSSIP_TOPIC);
        self.gossipsub
            .publish(topic, data)
            .map_err(|e| anyhow::anyhow!("Publish error: {:?}", e))?;
        Ok(())
    }
}

/// Sync request message
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum SyncRequest {
    /// Request full state
    FullState,

    /// Request state snapshot
    Snapshot,

    /// Request specific challenge
    Challenge { id: String },

    /// Request validators list
    Validators,
}

/// Sync response message
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum SyncResponse {
    /// Full state data
    FullState { data: Vec<u8> },

    /// State snapshot
    Snapshot { data: Vec<u8> },

    /// Challenge data
    Challenge { data: Option<Vec<u8>> },

    /// Validators list
    Validators { data: Vec<u8> },

    /// Error response
    Error { message: String },
}
