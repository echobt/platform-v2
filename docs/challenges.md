# Challenge Crates

This document explains how to create, integrate, and manage challenge crates in the Platform network.

## Overview

Challenge crates are modular Rust crates that implement evaluation logic for the Platform network. They support:

- **Hot-reload**: Update challenge logic without service interruption
- **State persistence**: Checkpoint and restore evaluation state across updates
- **Seamless integration**: Work alongside existing Docker-based challenges

## Directory Structure

```
platform/
├── challenges/                    # Challenge crates directory
│   ├── challenge-template/        # Template for new challenges
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   └── your-challenge/            # Your custom challenge
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs
├── crates/
│   ├── challenge-sdk/             # SDK with traits and types
│   ├── challenge-orchestrator/    # Container orchestration
│   └── subnet-manager/            # Hot-reload management
└── Cargo.toml                     # Workspace configuration
```

## Creating a New Challenge Crate

### Step 1: Copy the Template

```bash
cp -r challenges/challenge-template challenges/my-challenge
```

### Step 2: Update Cargo.toml

Edit `challenges/my-challenge/Cargo.toml`:

```toml
[package]
name = "my-challenge"
version = "0.1.0"
edition = "2021"
description = "My custom challenge for Platform"
license = "Apache-2.0"

[dependencies]
platform-challenge-sdk = { path = "../../crates/challenge-sdk" }
async-trait = "0.1"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
tokio = { version = "1.40", features = ["full"] }
tracing = "0.1"
thiserror = "2.0"
uuid = { version = "1.10", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
```

### Step 3: Add to Workspace

Edit the root `Cargo.toml`:

```toml
[workspace]
members = [
    # ... existing members ...
    "challenges/my-challenge",
]
```

### Step 4: Implement Required Traits

Your challenge must implement `ServerChallenge` for evaluation and optionally `HotReloadableChallenge` for hot-reload support.

## Required Traits

### ServerChallenge (from challenge-sdk)

```rust
use platform_challenge_sdk::prelude::*;

#[async_trait]
impl ServerChallenge for MyChallenge {
    fn challenge_id(&self) -> &str { "my-challenge" }
    fn name(&self) -> &str { "My Challenge" }
    fn version(&self) -> &str { "0.1.0" }
    
    async fn evaluate(&self, req: EvaluationRequest) -> Result<EvaluationResponse, ChallengeError> {
        // Your evaluation logic
        let score = self.compute_score(&req.data)?;
        Ok(EvaluationResponse::success(&req.request_id, score, json!({})))
    }
}
```

### HotReloadableChallenge (for hot-reload support)

```rust
use platform_challenge_sdk::crate_interface::*;

#[async_trait]
impl HotReloadableChallenge for MyChallenge {
    fn challenge_id(&self) -> &str { "my-challenge" }
    fn version(&self) -> &str { "0.1.0" }
    fn min_compatible_version(&self) -> &str { "0.1.0" }
    
    async fn create_checkpoint(&self) -> Result<EvaluationCheckpoint, ChallengeCrateError> {
        // Serialize your state
        let state_data = self.serialize_state()?;
        let pending = self.get_pending_evaluations();
        
        Ok(EvaluationCheckpoint {
            id: Uuid::new_v4(),
            challenge_id: self.challenge_id().to_string(),
            state_data,
            pending_evaluations: pending,
            created_at: Utc::now(),
            crate_version: self.version().to_string(),
        })
    }
    
    async fn restore_from_checkpoint(&mut self, checkpoint: EvaluationCheckpoint) 
        -> Result<RestoreResult, ChallengeCrateError> 
    {
        // Check version compatibility
        if !self.is_version_compatible(&checkpoint.crate_version) {
            return Err(ChallengeCrateError::IncompatibleVersion {
                expected: self.min_compatible_version().to_string(),
                actual: checkpoint.crate_version,
            });
        }
        
        // Restore state
        self.deserialize_state(&checkpoint.state_data)?;
        
        // Resume evaluations
        let resumed = self.resume_evaluations(&checkpoint.pending_evaluations);
        
        Ok(RestoreResult {
            checkpoint_id: checkpoint.id,
            resumed_count: resumed.len(),
            dropped_count: checkpoint.pending_evaluations.len() - resumed.len(),
        })
    }
    
    fn pending_evaluations_count(&self) -> usize {
        self.state.pending.len()
    }
    
    fn is_safe_to_update(&self) -> bool {
        // Check if there are no critical operations
        self.state.pending.is_empty() || self.state.pending.iter().all(|e| e.progress < 10)
    }
    
    async fn prepare_for_update(&mut self) -> Result<(), ChallengeCrateError> {
        self.accepting_new_requests = false;
        Ok(())
    }
    
    async fn resume_after_update(&mut self) -> Result<(), ChallengeCrateError> {
        self.accepting_new_requests = true;
        Ok(())
    }
}
```

## Hot-Reload Lifecycle

The hot-reload system follows a three-phase process:

### Phase 1: Prepare for Update

```
┌─────────────────┐
│   Running       │
│   Challenge     │
└────────┬────────┘
         │ prepare_for_update()
         ▼
┌─────────────────┐
│ Create          │
│ Checkpoint      │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Awaiting        │
│ Update          │
└─────────────────┘
```

1. `ChallengeHotReloadManager.prepare_for_update()` is called
2. Challenge creates checkpoint of current state
3. Pending evaluations are captured
4. Challenge pauses accepting new work

### Phase 2: Apply Update

```
┌─────────────────┐
│ Awaiting        │
│ Update          │
└────────┬────────┘
         │ apply_update()
         ▼
┌─────────────────┐
│ Load New        │
│ Crate Version   │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Restoring       │
│ State           │
└─────────────────┘
```

1. New crate version is loaded
2. `apply_update()` marks transition to restoration phase

### Phase 3: Complete Update

```
┌─────────────────┐
│ Restoring       │
│ State           │
└────────┬────────┘
         │ restore_from_checkpoint()
         ▼
┌─────────────────┐
│ Resume          │
│ Evaluations     │
└────────┬────────┘
         │ complete_update()
         ▼
┌─────────────────┐
│   Running       │
│   (New Version) │
└─────────────────┘
```

1. `restore_from_checkpoint()` restores state and resumes evaluations
2. `complete_update()` finalizes the update
3. Challenge resumes normal operation

## State Persistence

### Checkpoints

Checkpoints capture:
- **state_data**: Serialized challenge-specific state
- **pending_evaluations**: List of in-progress evaluations
- **crate_version**: Version at checkpoint time
- **state_hash**: Integrity verification

### Version Compatibility

When restoring from a checkpoint:

```rust
fn is_version_compatible(&self, checkpoint_version: &str) -> bool {
    // Default: simple string comparison
    checkpoint_version >= self.min_compatible_version()
}
```

Override this method for semantic versioning:

```rust
fn is_version_compatible(&self, checkpoint_version: &str) -> bool {
    // Example: Allow restore from any 0.x version
    let current = semver::Version::parse(self.version()).ok();
    let checkpoint = semver::Version::parse(checkpoint_version).ok();
    match (current, checkpoint) {
        (Some(c), Some(p)) => c.major == p.major,
        _ => false,
    }
}
```

## Integration with Docker

Challenge crates work alongside existing Docker-based challenges:

- **Docker challenges**: Managed by `ChallengeOrchestrator`
- **Crate challenges**: Managed by `ChallengeHotReloadManager`

Both systems share:
- Snapshot infrastructure (`SnapshotManager`)
- Recovery mechanisms (`RecoveryManager`)
- Update pipeline (`UpdateManager`)

## Testing Your Challenge

### Run Tests

```bash
# Test your challenge crate
cargo test -p my-challenge

# Test with the SDK
cargo test -p platform-challenge-sdk

# Full workspace test
cargo test --workspace
```

### Local Evaluation

```rust
#[tokio::test]
async fn test_evaluation() {
    let challenge = MyChallenge::new();
    let req = EvaluationRequest {
        request_id: "test-1".to_string(),
        participant_id: "miner-abc".to_string(),
        data: json!({"task": "test_task"}),
        config: Default::default(),
    };
    
    let resp = challenge.evaluate(req).await.unwrap();
    assert!(resp.success);
}
```

### Test Hot-Reload

```rust
#[tokio::test]
async fn test_hot_reload_cycle() {
    let mut challenge = MyChallenge::new();
    
    // Start some evaluations
    challenge.start_evaluation("req-1", "miner-1").await;
    
    // Create checkpoint
    let checkpoint = challenge.create_checkpoint().await.unwrap();
    assert!(!checkpoint.pending_evaluations.is_empty());
    
    // Simulate new version
    let mut new_challenge = MyChallenge::new();
    
    // Restore from checkpoint
    let result = new_challenge.restore_from_checkpoint(checkpoint).await.unwrap();
    assert!(result.resumed_count > 0);
}
```

## Examples

See the template challenge at `challenges/challenge-template/` for a complete working example.

For a real-world challenge example, see [term-challenge](https://github.com/PlatformNetwork/term-challenge) (Docker-based, demonstrates the challenge pattern).

## Recovery Integration

The `RecoveryManager` provides automatic recovery for challenge updates:

```rust
// These actions are available for challenge recovery
RecoveryAction::RestoreChallengeFromCheckpoint {
    challenge_id: "my-challenge".to_string(),
    checkpoint_id,
}

RecoveryAction::RollbackChallengeUpdate {
    challenge_id: "my-challenge".to_string(),
    reason: "Restoration failed".to_string(),
}

RecoveryAction::ResumeChallengeAfterUpdate {
    challenge_id: "my-challenge".to_string(),
}
```

## Best Practices

1. **Version your state**: Include version info in serialized state for migration
2. **Handle partial evaluations**: Design for evaluations that may be interrupted
3. **Test checkpoint/restore**: Ensure round-trip serialization works
4. **Monitor hot-reloads**: Use events from `ChallengeHotReloadManager`
5. **Set appropriate min_compatible_version**: Only increase when state format changes
6. **Log important operations**: Use tracing for debugging

## Troubleshooting

### Checkpoint too old

Increase `max_checkpoint_age_hours` in `HotReloadConfig` or create a fresh checkpoint.

### Version incompatible

The new crate version has a `min_compatible_version` higher than the checkpoint version. You may need to:
1. Accept data loss and start fresh
2. Implement state migration logic

### Restoration failed

Check logs for specific errors. Common causes:
1. State format changed between versions
2. Dependencies missing in new version
3. Corrupted checkpoint data (hash mismatch)
