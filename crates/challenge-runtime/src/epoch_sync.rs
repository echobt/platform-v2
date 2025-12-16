//! Epoch Synchronization with Bittensor
//!
//! Synchronizes challenge execution with Bittensor's tempo.
//! Manages epoch phases: EVAL -> COMMIT -> REVEAL

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Epoch phase
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EpochPhase {
    /// Evaluation phase - run agent evaluations
    Evaluation,
    /// Commit phase - submit weight commits
    Commit,
    /// Reveal phase - reveal weights
    Reveal,
    /// Waiting for next epoch
    Waiting,
}

impl std::fmt::Display for EpochPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EpochPhase::Evaluation => write!(f, "EVALUATION"),
            EpochPhase::Commit => write!(f, "COMMIT"),
            EpochPhase::Reveal => write!(f, "REVEAL"),
            EpochPhase::Waiting => write!(f, "WAITING"),
        }
    }
}

/// Epoch sync configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpochSyncConfig {
    /// Subnet UID
    pub subnet_uid: u16,
    /// Tempo (blocks per epoch) - default 360 for Bittensor
    pub tempo: u64,
    /// Evaluation phase percentage (0.0 - 1.0)
    pub eval_phase_pct: f64,
    /// Commit phase percentage
    pub commit_phase_pct: f64,
    /// Reveal phase percentage
    pub reveal_phase_pct: f64,
    /// Subtensor endpoint
    pub subtensor_endpoint: String,
    /// Block time in seconds (12 for Bittensor)
    pub block_time_secs: u64,
}

impl Default for EpochSyncConfig {
    fn default() -> Self {
        Self {
            subnet_uid: 100,
            tempo: 360,             // ~72 minutes at 12s/block
            eval_phase_pct: 0.75,   // 75% for evaluation
            commit_phase_pct: 0.15, // 15% for commit
            reveal_phase_pct: 0.10, // 10% for reveal
            subtensor_endpoint: "wss://entrypoint-finney.opentensor.ai:443".to_string(),
            block_time_secs: 12,
        }
    }
}

/// Epoch info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpochInfo {
    /// Current epoch number
    pub epoch: u64,
    /// Current block
    pub block: u64,
    /// Block at epoch start
    pub epoch_start_block: u64,
    /// Block at epoch end
    pub epoch_end_block: u64,
    /// Current phase
    pub phase: EpochPhase,
    /// Blocks remaining in current phase
    pub blocks_remaining_in_phase: u64,
    /// Estimated time remaining in phase (seconds)
    pub time_remaining_secs: u64,
    /// Progress in current epoch (0.0 - 1.0)
    pub epoch_progress: f64,
}

/// Epoch event
#[derive(Debug, Clone)]
pub enum EpochEvent {
    /// New epoch started
    EpochStarted { epoch: u64 },
    /// Phase changed
    PhaseChanged { epoch: u64, phase: EpochPhase },
    /// Block update
    BlockUpdate { block: u64, epoch: u64 },
    /// Epoch ending soon (last N blocks)
    EpochEndingSoon { epoch: u64, blocks_left: u64 },
}

/// Epoch synchronizer
pub struct EpochSync {
    config: EpochSyncConfig,
    /// Current state
    state: Arc<RwLock<EpochState>>,
    /// Event broadcaster
    event_tx: broadcast::Sender<EpochEvent>,
    /// Shutdown signal
    shutdown: Arc<RwLock<bool>>,
}

#[derive(Debug, Clone)]
struct EpochState {
    current_block: u64,
    current_epoch: u64,
    current_phase: EpochPhase,
    last_update: u64,
}

impl EpochSync {
    pub fn new(config: EpochSyncConfig) -> Self {
        let (event_tx, _) = broadcast::channel(100);

        Self {
            config,
            state: Arc::new(RwLock::new(EpochState {
                current_block: 0,
                current_epoch: 0,
                current_phase: EpochPhase::Waiting,
                last_update: 0,
            })),
            event_tx,
            shutdown: Arc::new(RwLock::new(false)),
        }
    }

    /// Get event receiver
    pub fn subscribe(&self) -> broadcast::Receiver<EpochEvent> {
        self.event_tx.subscribe()
    }

    /// Start epoch sync (background task)
    pub async fn start(&self) -> Result<(), String> {
        info!("Starting epoch sync for subnet {}", self.config.subnet_uid);

        let config = self.config.clone();
        let state = self.state.clone();
        let event_tx = self.event_tx.clone();
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(config.block_time_secs));

            loop {
                if *shutdown.read() {
                    info!("Epoch sync shutting down");
                    break;
                }

                interval.tick().await;

                // Fetch current block
                match Self::fetch_current_block(&config).await {
                    Ok(block) => {
                        let old_state = state.read().clone();
                        let new_epoch = block / config.tempo;
                        let block_in_epoch = block % config.tempo;
                        let new_phase = Self::calculate_phase(&config, block_in_epoch);

                        // Update state
                        {
                            let mut s = state.write();
                            s.current_block = block;
                            s.current_epoch = new_epoch;
                            s.current_phase = new_phase;
                            s.last_update = chrono::Utc::now().timestamp() as u64;
                        }

                        // Emit events
                        if new_epoch != old_state.current_epoch {
                            let _ = event_tx.send(EpochEvent::EpochStarted { epoch: new_epoch });
                        }

                        if new_phase != old_state.current_phase {
                            let _ = event_tx.send(EpochEvent::PhaseChanged {
                                epoch: new_epoch,
                                phase: new_phase,
                            });
                        }

                        let _ = event_tx.send(EpochEvent::BlockUpdate {
                            block,
                            epoch: new_epoch,
                        });

                        // Warn when epoch ending soon
                        let blocks_until_end = config.tempo - block_in_epoch;
                        if blocks_until_end <= 10 {
                            let _ = event_tx.send(EpochEvent::EpochEndingSoon {
                                epoch: new_epoch,
                                blocks_left: blocks_until_end,
                            });
                        }
                    }
                    Err(e) => {
                        warn!("Failed to fetch block: {}", e);
                    }
                }
            }
        });

        Ok(())
    }

    /// Stop epoch sync
    pub fn stop(&self) {
        *self.shutdown.write() = true;
    }

    /// Get current epoch info
    pub fn get_info(&self) -> EpochInfo {
        let state = self.state.read();
        let block_in_epoch = state.current_block % self.config.tempo;
        let epoch_start = state.current_epoch * self.config.tempo;
        let epoch_end = epoch_start + self.config.tempo;

        let (phase_start, phase_end) = self.get_phase_bounds(state.current_phase);
        let blocks_remaining = (phase_end - block_in_epoch).max(0);

        EpochInfo {
            epoch: state.current_epoch,
            block: state.current_block,
            epoch_start_block: epoch_start,
            epoch_end_block: epoch_end,
            phase: state.current_phase,
            blocks_remaining_in_phase: blocks_remaining,
            time_remaining_secs: blocks_remaining * self.config.block_time_secs,
            epoch_progress: block_in_epoch as f64 / self.config.tempo as f64,
        }
    }

    /// Get current phase
    pub fn get_phase(&self) -> EpochPhase {
        self.state.read().current_phase
    }

    /// Get current epoch
    pub fn get_epoch(&self) -> u64 {
        self.state.read().current_epoch
    }

    /// Get current block
    pub fn get_block(&self) -> u64 {
        self.state.read().current_block
    }

    /// Check if in evaluation phase
    pub fn is_eval_phase(&self) -> bool {
        self.state.read().current_phase == EpochPhase::Evaluation
    }

    /// Check if in commit phase
    pub fn is_commit_phase(&self) -> bool {
        self.state.read().current_phase == EpochPhase::Commit
    }

    /// Check if in reveal phase
    pub fn is_reveal_phase(&self) -> bool {
        self.state.read().current_phase == EpochPhase::Reveal
    }

    /// Calculate phase from block in epoch
    fn calculate_phase(config: &EpochSyncConfig, block_in_epoch: u64) -> EpochPhase {
        let eval_blocks = (config.tempo as f64 * config.eval_phase_pct) as u64;
        let commit_blocks = (config.tempo as f64 * config.commit_phase_pct) as u64;

        if block_in_epoch < eval_blocks {
            EpochPhase::Evaluation
        } else if block_in_epoch < eval_blocks + commit_blocks {
            EpochPhase::Commit
        } else {
            EpochPhase::Reveal
        }
    }

    /// Get block bounds for a phase
    fn get_phase_bounds(&self, phase: EpochPhase) -> (u64, u64) {
        let eval_blocks = (self.config.tempo as f64 * self.config.eval_phase_pct) as u64;
        let commit_blocks = (self.config.tempo as f64 * self.config.commit_phase_pct) as u64;

        match phase {
            EpochPhase::Evaluation => (0, eval_blocks),
            EpochPhase::Commit => (eval_blocks, eval_blocks + commit_blocks),
            EpochPhase::Reveal => (eval_blocks + commit_blocks, self.config.tempo),
            EpochPhase::Waiting => (0, 0),
        }
    }

    /// Fetch current block from chain
    async fn fetch_current_block(config: &EpochSyncConfig) -> Result<u64, String> {
        // Bittensor block query implementation
        // Currently returns simulated block; will be replaced with actual chain query
        //
        // 1. Connect to subtensor via websocket
        // 2. Subscribe to new blocks or query System.Number

        // For now, simulate based on time
        let now = chrono::Utc::now().timestamp() as u64;
        let genesis = 1700000000u64; // Approximate
        let block = (now - genesis) / config.block_time_secs;

        Ok(block)
    }

    /// Update block manually (for testing or when receiving block from other source)
    pub fn update_block(&self, block: u64) {
        let new_epoch = block / self.config.tempo;
        let block_in_epoch = block % self.config.tempo;
        let new_phase = Self::calculate_phase(&self.config, block_in_epoch);

        let mut state = self.state.write();
        let old_epoch = state.current_epoch;
        let old_phase = state.current_phase;

        state.current_block = block;
        state.current_epoch = new_epoch;
        state.current_phase = new_phase;
        state.last_update = chrono::Utc::now().timestamp() as u64;

        drop(state);

        if new_epoch != old_epoch {
            let _ = self
                .event_tx
                .send(EpochEvent::EpochStarted { epoch: new_epoch });
        }

        if new_phase != old_phase {
            let _ = self.event_tx.send(EpochEvent::PhaseChanged {
                epoch: new_epoch,
                phase: new_phase,
            });
        }
    }

    /// Wait for specific phase
    pub async fn wait_for_phase(&self, target_phase: EpochPhase) -> EpochInfo {
        let mut rx = self.subscribe();

        loop {
            let info = self.get_info();
            if info.phase == target_phase {
                return info;
            }

            // Wait for phase change event
            match rx.recv().await {
                Ok(EpochEvent::PhaseChanged { phase, .. }) if phase == target_phase => {
                    return self.get_info();
                }
                Ok(_) => continue,
                Err(_) => {
                    // Channel closed, return current info
                    return self.get_info();
                }
            }
        }
    }

    /// Get estimated time until phase
    pub fn time_until_phase(&self, target_phase: EpochPhase) -> u64 {
        let info = self.get_info();
        let current_block_in_epoch = info.block % self.config.tempo;
        let (target_start, _) = self.get_phase_bounds(target_phase);

        if current_block_in_epoch >= target_start {
            // Phase is current or past, calculate for next epoch
            let blocks_to_epoch_end = self.config.tempo - current_block_in_epoch;
            let blocks_total = blocks_to_epoch_end + target_start;
            blocks_total * self.config.block_time_secs
        } else {
            // Phase is ahead in current epoch
            (target_start - current_block_in_epoch) * self.config.block_time_secs
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase_calculation() {
        let config = EpochSyncConfig::default();

        // Block 0 = Evaluation (0-75%)
        assert_eq!(
            EpochSync::calculate_phase(&config, 0),
            EpochPhase::Evaluation
        );

        // Block 269 = Evaluation (75% of 360 = 270)
        assert_eq!(
            EpochSync::calculate_phase(&config, 269),
            EpochPhase::Evaluation
        );

        // Block 270 = Commit (75-90%)
        assert_eq!(EpochSync::calculate_phase(&config, 270), EpochPhase::Commit);

        // Block 324 = Reveal (90-100%)
        assert_eq!(EpochSync::calculate_phase(&config, 324), EpochPhase::Reveal);
    }

    #[test]
    fn test_epoch_info() {
        let config = EpochSyncConfig::default();
        let sync = EpochSync::new(config);

        // Set to block 100 (epoch 0, eval phase)
        sync.update_block(100);

        let info = sync.get_info();
        assert_eq!(info.epoch, 0);
        assert_eq!(info.block, 100);
        assert_eq!(info.phase, EpochPhase::Evaluation);

        // Set to block 400 (epoch 1, eval phase)
        sync.update_block(400);

        let info = sync.get_info();
        assert_eq!(info.epoch, 1);
        assert_eq!(info.phase, EpochPhase::Evaluation);
    }

    #[test]
    fn test_phase_bounds() {
        let config = EpochSyncConfig::default();
        let sync = EpochSync::new(config);

        let (start, end) = sync.get_phase_bounds(EpochPhase::Evaluation);
        assert_eq!(start, 0);
        assert_eq!(end, 270); // 75% of 360

        let (start, end) = sync.get_phase_bounds(EpochPhase::Commit);
        assert_eq!(start, 270);
        assert_eq!(end, 324); // 90% of 360

        let (start, end) = sync.get_phase_bounds(EpochPhase::Reveal);
        assert_eq!(start, 324);
        assert_eq!(end, 360);
    }
}
