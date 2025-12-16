//! Protocol definitions for P2P communication

use libp2p::StreamProtocol;

/// Protocol version
pub const PROTOCOL_VERSION: &str = "/platform/1.0.0";

/// Gossipsub topic for broadcast messages
pub const GOSSIP_TOPIC: &str = "platform-gossip";

/// Get the stream protocol
pub fn stream_protocol() -> StreamProtocol {
    StreamProtocol::new(PROTOCOL_VERSION)
}

/// Message types for request-response
#[derive(Debug, Clone)]
pub enum RequestType {
    StateSync,
    GetChallenge,
    GetValidators,
}
