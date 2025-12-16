#![allow(dead_code, unused_variables, unused_imports)]
//! P2P Network Layer using libp2p
//!
//! Provides gossip-based message propagation and direct peer communication.
//! Includes DDoS protection, stake-based access control, and state sync.
//!
//! # Modules
//!
//! - `behaviour` - libp2p network behaviour
//! - `node` - Network node implementation
//! - `protocol` - Protocol definitions
//! - `protection` - DDoS protection and rate limiting
//! - `state_sync` - State synchronization for new validators
//! - `data_gossip` - Challenge data gossip and consensus

pub mod behaviour;
pub mod data_gossip;
pub mod node;
pub mod protection;
pub mod protocol;
pub mod state_sync;

pub use behaviour::*;
pub use data_gossip::*;
pub use node::*;
pub use protection::*;
pub use protocol::*;
pub use state_sync::*;
