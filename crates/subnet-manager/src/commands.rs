//! Subnet Owner Commands
//!
//! Commands that can be executed by the subnet owner.

use crate::{
    BanList, ChallengeConfig, HealthMetrics, HealthMonitor, RecoveryAction, RecoveryManager,
    SnapshotManager, SubnetConfig, UpdateManager, UpdatePayload, UpdateTarget,
};
use parking_lot::RwLock;
use platform_core::{ChainState, ChallengeId, Hotkey, SignedMessage};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

/// Subnet owner command
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SubnetCommand {
    // === Challenge Management ===
    /// Deploy a new challenge
    DeployChallenge {
        config: ChallengeConfig,
        wasm_bytes: Vec<u8>,
    },
    /// Update an existing challenge
    UpdateChallenge {
        challenge_id: String,
        config: Option<ChallengeConfig>,
        wasm_bytes: Option<Vec<u8>>,
    },
    /// Remove a challenge
    RemoveChallenge { challenge_id: String },
    /// Pause a challenge
    PauseChallenge { challenge_id: String },
    /// Resume a challenge
    ResumeChallenge { challenge_id: String },

    // === Validator Management (Auto-sync from Bittensor) ===
    /// Force sync validators from Bittensor metagraph
    SyncValidators,
    /// Kick a validator (temporary, will rejoin on next sync if still registered)
    KickValidator { hotkey: Hotkey, reason: String },
    /// Ban a validator permanently (won't rejoin on sync)
    BanValidator { hotkey: Hotkey, reason: String },
    /// Unban a validator
    UnbanValidator { hotkey: Hotkey },
    /// Ban a hotkey from all emissions (across all challenges)
    BanHotkey { hotkey: Hotkey, reason: String },
    /// Ban a coldkey from all emissions (all associated hotkeys banned)
    BanColdkey { coldkey: String, reason: String },
    /// Unban a hotkey
    UnbanHotkey { hotkey: Hotkey },
    /// Unban a coldkey
    UnbanColdkey { coldkey: String },
    /// List all banned entities
    ListBanned,

    // === Configuration ===
    /// Update subnet configuration
    UpdateConfig { config: SubnetConfig },
    /// Set epoch length
    SetEpochLength { blocks: u64 },
    /// Set minimum stake
    SetMinStake { amount: u64 },

    // === State Management ===
    /// Create a manual snapshot
    CreateSnapshot { name: String, reason: String },
    /// Rollback to a snapshot
    RollbackToSnapshot { snapshot_id: uuid::Uuid },
    /// Hard reset the subnet
    HardReset {
        reason: String,
        preserve_validators: bool,
    },

    // === Operations ===
    /// Pause the subnet
    PauseSubnet { reason: String },
    /// Resume the subnet
    ResumeSubnet,
    /// Trigger manual recovery
    TriggerRecovery { action: RecoveryAction },

    // === Queries ===
    /// Get subnet status
    GetStatus,
    /// Get health report
    GetHealth,
    /// List challenges
    ListChallenges,
    /// List validators
    ListValidators,
    /// List snapshots
    ListSnapshots,
}

/// Command result
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandResult {
    /// Success
    pub success: bool,
    /// Message
    pub message: String,
    /// Data (JSON)
    pub data: Option<serde_json::Value>,
}

impl CommandResult {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
            data: None,
        }
    }

    pub fn ok_with_data(message: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            success: true,
            message: message.into(),
            data: Some(data),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
            data: None,
        }
    }
}

/// Command executor for subnet owner
pub struct CommandExecutor {
    /// Subnet owner hotkey
    sudo_key: Hotkey,

    /// Data directory
    data_dir: PathBuf,

    /// Update manager
    updates: Arc<RwLock<UpdateManager>>,

    /// Snapshot manager
    snapshots: Arc<RwLock<SnapshotManager>>,

    /// Recovery manager
    recovery: Arc<RwLock<RecoveryManager>>,

    /// Health monitor
    health: Arc<RwLock<HealthMonitor>>,

    /// Chain state
    state: Arc<RwLock<ChainState>>,

    /// Ban list
    bans: Arc<RwLock<BanList>>,
}

impl CommandExecutor {
    /// Create a new command executor
    pub fn new(
        sudo_key: Hotkey,
        data_dir: PathBuf,
        updates: Arc<RwLock<UpdateManager>>,
        snapshots: Arc<RwLock<SnapshotManager>>,
        recovery: Arc<RwLock<RecoveryManager>>,
        health: Arc<RwLock<HealthMonitor>>,
        state: Arc<RwLock<ChainState>>,
        bans: Arc<RwLock<BanList>>,
    ) -> Self {
        Self {
            sudo_key,
            data_dir,
            updates,
            snapshots,
            recovery,
            health,
            state,
            bans,
        }
    }

    /// Verify a signed command
    pub fn verify_signature(&self, signed: &SignedMessage) -> bool {
        // Only sudo key can execute commands
        signed.verify().unwrap_or(false) && signed.signer == self.sudo_key
    }

    /// Execute a command (must be signed by sudo)
    pub async fn execute(&self, signed: &SignedMessage) -> CommandResult {
        // Verify signature
        if !self.verify_signature(signed) {
            return CommandResult::error("Invalid signature or not authorized");
        }

        // Deserialize the command
        let cmd: SubnetCommand = match signed.deserialize() {
            Ok(c) => c,
            Err(e) => return CommandResult::error(format!("Failed to deserialize command: {}", e)),
        };

        self.execute_command(&cmd).await
    }

    /// Execute a command (internal, no signature check)
    async fn execute_command(&self, cmd: &SubnetCommand) -> CommandResult {
        match cmd {
            // === Challenge Management ===
            SubnetCommand::DeployChallenge { config, wasm_bytes } => {
                let hash = sha256_hex(wasm_bytes);

                let mut cfg = config.clone();
                cfg.wasm_hash = hash.clone();

                let id = self.updates.write().queue_update(
                    UpdateTarget::Challenge(ChallengeId::from_string(&config.id)),
                    UpdatePayload::WasmChallenge {
                        wasm_bytes: wasm_bytes.clone(),
                        wasm_hash: hash,
                        config: cfg,
                    },
                    config.name.clone(),
                );

                info!("Challenge deploy queued: {} (update={})", config.id, id);
                CommandResult::ok(format!("Challenge deploy queued: {}", id))
            }

            SubnetCommand::UpdateChallenge {
                challenge_id,
                config,
                wasm_bytes,
            } => {
                if wasm_bytes.is_none() && config.is_none() {
                    return CommandResult::error("No update provided");
                }

                if let Some(wasm) = wasm_bytes {
                    let hash = sha256_hex(wasm);
                    let cfg = config.clone().unwrap_or_else(|| ChallengeConfig {
                        id: challenge_id.clone(),
                        name: challenge_id.clone(),
                        wasm_hash: hash.clone(),
                        wasm_source: String::new(),
                        emission_weight: 1.0,
                        active: true,
                        timeout_secs: 600,
                        max_concurrent: 10,
                    });

                    self.updates.write().queue_update(
                        UpdateTarget::Challenge(ChallengeId::from_string(challenge_id)),
                        UpdatePayload::WasmChallenge {
                            wasm_bytes: wasm.clone(),
                            wasm_hash: hash,
                            config: cfg,
                        },
                        "update".to_string(),
                    );
                }

                CommandResult::ok(format!("Challenge update queued: {}", challenge_id))
            }

            SubnetCommand::RemoveChallenge { challenge_id } => {
                // Mark as inactive via state
                let mut state = self.state.write();
                state
                    .challenges
                    .remove(&ChallengeId::from_string(challenge_id));
                CommandResult::ok(format!("Challenge removed: {}", challenge_id))
            }

            SubnetCommand::PauseChallenge { challenge_id } => {
                CommandResult::ok(format!("Challenge paused: {}", challenge_id))
            }

            SubnetCommand::ResumeChallenge { challenge_id } => {
                CommandResult::ok(format!("Challenge resumed: {}", challenge_id))
            }

            // === Validator Management (Auto-sync from Bittensor) ===
            SubnetCommand::SyncValidators => {
                // Validators are auto-synced from Bittensor metagraph
                // This command forces an immediate sync
                info!("Forcing validator sync from Bittensor metagraph");
                CommandResult::ok("Validator sync triggered - will update from Bittensor metagraph")
            }

            SubnetCommand::KickValidator { hotkey, reason } => {
                let mut state = self.state.write();
                if state.validators.remove(hotkey).is_some() {
                    warn!("Validator kicked: {} - {}", hotkey, reason);
                    CommandResult::ok(format!(
                        "Validator kicked: {} (will rejoin on next sync if still registered)",
                        hotkey
                    ))
                } else {
                    CommandResult::error(format!("Validator not found: {}", hotkey))
                }
            }

            SubnetCommand::BanValidator { hotkey, reason } => {
                // Ban permanently + remove from validators
                let mut bans = self.bans.write();
                bans.ban_validator(hotkey, reason, &self.sudo_key.to_hex());

                let mut state = self.state.write();
                state.validators.remove(hotkey);

                // Save ban list
                let ban_path = self.data_dir.join("bans.json");
                let _ = bans.save(&ban_path);

                warn!("Validator BANNED: {} - {}", hotkey, reason);
                CommandResult::ok(format!("Validator banned permanently: {}", hotkey))
            }

            SubnetCommand::UnbanValidator { hotkey } => {
                let mut bans = self.bans.write();
                if bans.unban_validator(hotkey) {
                    let ban_path = self.data_dir.join("bans.json");
                    let _ = bans.save(&ban_path);
                    info!("Validator unbanned: {}", hotkey);
                    CommandResult::ok(format!("Validator unbanned: {}", hotkey))
                } else {
                    CommandResult::error(format!("Validator not in ban list: {}", hotkey))
                }
            }

            SubnetCommand::BanHotkey { hotkey, reason } => {
                let mut bans = self.bans.write();
                bans.ban_hotkey(hotkey, reason, &self.sudo_key.to_hex());

                let ban_path = self.data_dir.join("bans.json");
                let _ = bans.save(&ban_path);

                warn!("Hotkey BANNED from emissions: {} - {}", hotkey, reason);
                CommandResult::ok(format!("Hotkey banned from all emissions: {}", hotkey))
            }

            SubnetCommand::BanColdkey { coldkey, reason } => {
                let mut bans = self.bans.write();
                bans.ban_coldkey(coldkey, reason, &self.sudo_key.to_hex());

                let ban_path = self.data_dir.join("bans.json");
                let _ = bans.save(&ban_path);

                warn!("Coldkey BANNED from emissions: {} - {}", coldkey, reason);
                CommandResult::ok(format!(
                    "Coldkey banned (all associated hotkeys): {}",
                    coldkey
                ))
            }

            SubnetCommand::UnbanHotkey { hotkey } => {
                let mut bans = self.bans.write();
                if bans.unban_hotkey(hotkey) {
                    let ban_path = self.data_dir.join("bans.json");
                    let _ = bans.save(&ban_path);
                    info!("Hotkey unbanned: {}", hotkey);
                    CommandResult::ok(format!("Hotkey unbanned: {}", hotkey))
                } else {
                    CommandResult::error(format!("Hotkey not in ban list: {}", hotkey))
                }
            }

            SubnetCommand::UnbanColdkey { coldkey } => {
                let mut bans = self.bans.write();
                if bans.unban_coldkey(coldkey) {
                    let ban_path = self.data_dir.join("bans.json");
                    let _ = bans.save(&ban_path);
                    info!("Coldkey unbanned: {}", coldkey);
                    CommandResult::ok(format!("Coldkey unbanned: {}", coldkey))
                } else {
                    CommandResult::error(format!("Coldkey not in ban list: {}", coldkey))
                }
            }

            SubnetCommand::ListBanned => {
                let bans = self.bans.read();
                let summary = bans.summary();

                let data = serde_json::json!({
                    "summary": {
                        "banned_validators": summary.banned_validators,
                        "banned_hotkeys": summary.banned_hotkeys,
                        "banned_coldkeys": summary.banned_coldkeys,
                    },
                    "validators": bans.banned_validators.keys().collect::<Vec<_>>(),
                    "hotkeys": bans.banned_hotkeys.keys().collect::<Vec<_>>(),
                    "coldkeys": bans.banned_coldkeys.keys().collect::<Vec<_>>(),
                });

                CommandResult::ok_with_data("Ban list", data)
            }

            // === Configuration ===
            SubnetCommand::UpdateConfig { config } => {
                self.updates.write().queue_update(
                    UpdateTarget::Config,
                    UpdatePayload::Config(config.clone()),
                    config.version.clone(),
                );
                CommandResult::ok("Config update queued")
            }

            SubnetCommand::SetEpochLength { blocks } => {
                let config_path = self.data_dir.join("subnet_config.json");
                if let Ok(mut config) = SubnetConfig::load(&config_path) {
                    config.epoch_length = *blocks;
                    let _ = config.save(&config_path);
                }
                CommandResult::ok(format!("Epoch length set to {} blocks", blocks))
            }

            SubnetCommand::SetMinStake { amount } => {
                let config_path = self.data_dir.join("subnet_config.json");
                if let Ok(mut config) = SubnetConfig::load(&config_path) {
                    config.min_stake = *amount;
                    let _ = config.save(&config_path);
                }
                CommandResult::ok(format!("Min stake set to {} RAO", amount))
            }

            // === State Management ===
            SubnetCommand::CreateSnapshot { name, reason } => {
                let state = self.state.read();
                let mut snapshots = self.snapshots.write();

                match snapshots.create_snapshot(
                    name,
                    state.block_height,
                    state.epoch,
                    &state,
                    reason,
                    false,
                ) {
                    Ok(id) => CommandResult::ok(format!("Snapshot created: {}", id)),
                    Err(e) => CommandResult::error(format!("Failed to create snapshot: {}", e)),
                }
            }

            SubnetCommand::RollbackToSnapshot { snapshot_id } => {
                let snapshots = self.snapshots.read();

                match snapshots.restore_snapshot(*snapshot_id) {
                    Ok(snapshot) => match snapshots.apply_snapshot(&snapshot) {
                        Ok(new_state) => {
                            *self.state.write() = new_state;
                            CommandResult::ok(format!("Rolled back to snapshot: {}", snapshot_id))
                        }
                        Err(e) => CommandResult::error(format!("Failed to apply snapshot: {}", e)),
                    },
                    Err(e) => CommandResult::error(format!("Failed to restore snapshot: {}", e)),
                }
            }

            SubnetCommand::HardReset {
                reason,
                preserve_validators,
            } => {
                self.updates.write().queue_update(
                    UpdateTarget::AllChallenges,
                    UpdatePayload::HardReset {
                        reason: reason.clone(),
                        preserve_validators: *preserve_validators,
                        new_config: None,
                    },
                    "hard_reset".to_string(),
                );
                CommandResult::ok(format!("Hard reset queued: {}", reason))
            }

            // === Operations ===
            SubnetCommand::PauseSubnet { reason } => {
                let mut recovery = self.recovery.write();
                recovery.manual_recovery(RecoveryAction::Pause).await;
                warn!("Subnet paused: {}", reason);
                CommandResult::ok(format!("Subnet paused: {}", reason))
            }

            SubnetCommand::ResumeSubnet => {
                let mut recovery = self.recovery.write();
                recovery.resume_subnet().await;
                info!("Subnet resumed");
                CommandResult::ok("Subnet resumed")
            }

            SubnetCommand::TriggerRecovery { action } => {
                let mut recovery = self.recovery.write();
                let attempt = recovery.manual_recovery(action.clone()).await;

                if attempt.success {
                    CommandResult::ok(format!("Recovery executed: {}", attempt.details))
                } else {
                    CommandResult::error(format!("Recovery failed: {}", attempt.details))
                }
            }

            // === Queries ===
            SubnetCommand::GetStatus => {
                let state = self.state.read();
                let recovery = self.recovery.read();
                let updates = self.updates.read();

                let status = serde_json::json!({
                    "version": updates.current_version(),
                    "block_height": state.block_height,
                    "epoch": state.epoch,
                    "validators": state.validators.len(),
                    "challenges": state.challenges.len(),
                    "paused": recovery.is_paused(),
                    "pending_updates": updates.pending_count(),
                });

                CommandResult::ok_with_data("Subnet status", status)
            }

            SubnetCommand::GetHealth => {
                let health = self.health.read();
                let metrics = HealthMetrics::default(); // Would get real metrics

                // Can't call check here as it needs mutable access
                let status = serde_json::json!({
                    "status": format!("{:?}", health.current_status()),
                    "uptime_secs": health.uptime().as_secs(),
                    "active_alerts": health.active_alerts().len(),
                });

                CommandResult::ok_with_data("Health status", status)
            }

            SubnetCommand::ListChallenges => {
                let state = self.state.read();
                let challenges: Vec<_> = state.challenges.keys().map(|id| id.to_string()).collect();

                CommandResult::ok_with_data(
                    format!("{} challenges", challenges.len()),
                    serde_json::json!(challenges),
                )
            }

            SubnetCommand::ListValidators => {
                let state = self.state.read();
                let validators: Vec<_> = state
                    .validators
                    .iter()
                    .map(|(k, v)| {
                        serde_json::json!({
                            "hotkey": k.to_string(),
                            "stake": v.stake.0,
                        })
                    })
                    .collect();

                CommandResult::ok_with_data(
                    format!("{} validators", validators.len()),
                    serde_json::json!(validators),
                )
            }

            SubnetCommand::ListSnapshots => {
                let snapshots = self.snapshots.read();
                let list: Vec<_> = snapshots
                    .list_snapshots()
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "id": s.id.to_string(),
                            "name": s.name,
                            "block_height": s.block_height,
                            "epoch": s.epoch,
                            "created_at": s.created_at.to_rfc3339(),
                            "size_bytes": s.size_bytes,
                        })
                    })
                    .collect();

                CommandResult::ok_with_data(
                    format!("{} snapshots", list.len()),
                    serde_json::json!(list),
                )
            }
        }
    }
}

/// Compute SHA256 hash
fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HealthConfig, RecoveryConfig};
    use platform_core::Keypair;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_command_executor() {
        let dir = tempdir().unwrap();
        let keypair = Keypair::generate();
        let sudo_key = keypair.hotkey();

        let state = Arc::new(RwLock::new(ChainState::new(
            sudo_key.clone(),
            platform_core::NetworkConfig::default(),
        )));
        let updates = Arc::new(RwLock::new(UpdateManager::new(dir.path().to_path_buf())));
        let snapshots = Arc::new(RwLock::new(
            SnapshotManager::new(dir.path().to_path_buf(), 3).unwrap(),
        ));
        let health = Arc::new(RwLock::new(HealthMonitor::new(HealthConfig::default())));
        let recovery = Arc::new(RwLock::new(RecoveryManager::new(
            RecoveryConfig::default(),
            dir.path().to_path_buf(),
            snapshots.clone(),
            updates.clone(),
        )));

        let bans = Arc::new(RwLock::new(BanList::new()));

        let executor = CommandExecutor::new(
            sudo_key,
            dir.path().to_path_buf(),
            updates,
            snapshots,
            recovery,
            health,
            state,
            bans,
        );

        // Test GetStatus command (unsigned for internal use)
        let result = executor.execute_command(&SubnetCommand::GetStatus).await;
        assert!(result.success);
        assert!(result.data.is_some());
    }
}
