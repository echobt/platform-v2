#![allow(dead_code, unused_variables, unused_imports)]
//! PBFT-style Consensus for Mini-Chain
//!
//! Simplified Practical Byzantine Fault Tolerance for ~8 validators.
//! Requires 2f+1 votes where f is the max faulty nodes (f=2 for 8 validators).

pub mod pbft;
pub mod state;
pub mod types;

pub use pbft::*;
pub use state::*;
pub use types::*;
