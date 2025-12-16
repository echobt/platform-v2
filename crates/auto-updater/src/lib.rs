//! Auto-Updater for Mini-Chain Validators
//!
//! Monitors for version updates from the network and automatically
//! pulls new Docker images and restarts the validator.

pub mod updater;
pub mod version;
pub mod watcher;

pub use updater::*;
pub use version::*;
pub use watcher::*;
