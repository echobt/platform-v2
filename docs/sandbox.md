# Multi-Validator Sandbox Environment

This document describes how to use the multi-validator sandbox environment for testing the Platform P2P consensus system.

## Overview

The sandbox environment allows you to run multiple validator nodes locally to test:

- P2P communication via libp2p gossipsub
- PBFT consensus reaching agreement
- Leader election and view changes
- State synchronization across validators
- Score aggregation and weight calculation

## Quick Start

### Running Integration Tests (In-Memory)

The fastest way to test multi-validator consensus is to run the in-memory integration tests:

```bash
# Run all sandbox tests
cargo test --package platform-e2e-tests sandbox

# Run a specific test
cargo test --package platform-e2e-tests test_pbft_consensus_single_proposal
```

These tests create mock validators in-memory and verify:
- Validator bootstrap and peer discovery
- Message propagation between validators
- PBFT consensus phases (propose, prepare, commit)
- Leader rotation across view changes
- Stake-weighted voting and score aggregation

### Running Docker Sandbox (Full Stack)

For full-stack testing with actual validator binaries running in containers:

```bash
# Start the sandbox (3 validators)
./scripts/sandbox-up.sh

# View logs
docker compose -f docker/docker-compose.sandbox.yml logs -f

# Run tests against the sandbox
./scripts/run-sandbox-tests.sh

# Stop the sandbox
./scripts/sandbox-down.sh

# Stop but keep data
./scripts/sandbox-down.sh --keep-data
```

## Architecture

### Docker Sandbox

The Docker sandbox (`docker/docker-compose.sandbox.yml`) creates:

| Service       | Port | Description |
|---------------|------|-------------|
| validator-1   | 9001 | Bootstrap node |
| validator-2   | 9002 | Peer validator |
| validator-3   | 9003 | Peer validator |

Each validator:
- Uses a deterministic seed for its keypair (`SEED_1`, `SEED_2`, `SEED_3`)
- Runs with `--no-bittensor` (no blockchain connection)
- Bootstraps to the other validators via DNS names
- Stores data in a separate Docker volume

### P2P Network

Validators discover each other using:
- DNS multiaddrs: `/dns4/validator-1/tcp/9001`
- libp2p gossipsub for message broadcast
- Kademlia DHT for peer discovery

### Consensus

The sandbox uses PBFT consensus with:
- n=3 validators, f=0 fault tolerance, quorum=2
- Round-robin leader election by stake ordering
- View changes on timeout

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SEED_1` | `validator_sandbox_seed_1` | Keypair seed for validator 1 |
| `SEED_2` | `validator_sandbox_seed_2` | Keypair seed for validator 2 |
| `SEED_3` | `validator_sandbox_seed_3` | Keypair seed for validator 3 |
| `RUST_LOG` | `debug,platform_p2p_consensus=trace` | Log level |
| `NETUID` | `100` | Subnet UID |

### Custom Configuration

To add more validators, copy an existing validator service in `docker-compose.sandbox.yml` and update:
- Container name and hostname
- Port number
- Secret key seed
- Bootstrap peers list

## Test Coverage

The sandbox tests verify:

1. **Multi-validator Bootstrap** (`test_multi_validator_bootstrap`)
   - Creates 4 validators with mock metagraph
   - Verifies unique hotkeys and peer knowledge
   - Validates quorum calculation (n=4, f=1, quorum=3)

2. **Message Propagation** (`test_gossipsub_message_propagation`)
   - Leader creates proposal
   - Verifies prepare messages from all non-leaders
   - Validates proposal hash consistency

3. **PBFT Consensus** (`test_pbft_consensus_single_proposal`)
   - Full consensus round: propose → prepare → commit
   - Verifies quorum reached and decision made
   - Validates commit signatures

4. **Leader Election** (`test_leader_election_rotation`)
   - Verifies leader rotation across views
   - Tests view change initiation

5. **State Synchronization** (`test_state_synchronization`)
   - Verifies initial state consistency
   - Tests state updates and sync
   - Validates state hash matching after sync

6. **Score Aggregation** (`test_score_aggregation_consensus`)
   - Submits evaluations from all validators
   - Verifies stake-weighted score calculation
   - Validates finalization workflow

## CI Integration

The sandbox tests run automatically in CI:

```yaml
sandbox-tests:
  name: Sandbox Integration Tests
  runs-on: platform-runner
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
    - run: cargo nextest run --package platform-e2e-tests -E 'test(/sandbox/)'
```

## Troubleshooting

### Tests Fail with "Connection Refused"

If Docker sandbox tests fail, ensure:
1. Docker is running: `docker ps`
2. Sandbox is up: `./scripts/sandbox-up.sh`
3. Check logs: `docker compose -f docker/docker-compose.sandbox.yml logs`

### Validators Not Discovering Peers

Check:
1. All containers are on the same network
2. Bootstrap peers are correctly configured
3. Ports are not blocked

### State Hash Mismatch

This indicates validators have divergent state. Check:
1. All validators started at the same time
2. No conflicting proposals were made
3. Consensus was reached before state comparison

## Mock Bittensor Integration

The sandbox uses mock Bittensor infrastructure from `platform-bittensor` crate:

```rust
use platform_bittensor::mock::{
    MockMetagraphBuilder,
    MockNeuronBuilder,
    create_validators,
    hotkey_from_seed,
};

// Create a mock metagraph with 4 validators
let validators = create_validators(4, 100.0, 500.0);
let metagraph = MockMetagraphBuilder::new(100)
    .add_neurons(validators)
    .build();
```

This allows testing validator logic without connecting to the actual Bittensor network.
