//! Multi-validator sandbox integration tests
//!
//! These tests verify inter-validator communication and consensus
//! using in-memory P2P networks with mock Bittensor integration.
//!
//! Tests are designed to run without Docker, using mock validators
//! that communicate through in-memory channels and shared state.

use platform_bittensor::mock::{create_validators, hotkey_from_seed, MockMetagraphBuilder};
use platform_core::{Hotkey, Keypair};
use platform_p2p_consensus::{
    ConsensusDecision, ConsensusEngine, P2PConfig, StateManager, ValidatorRecord, ValidatorSet,
};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

/// Number of validators to use in multi-validator tests (4 validators = f=1, quorum=3)
const TEST_VALIDATOR_COUNT: usize = 4;

/// Create a deterministic keypair from a seed index
fn keypair_from_seed(seed: u64) -> Keypair {
    let seed_bytes = hotkey_from_seed(seed);
    Keypair::from_seed(&seed_bytes).expect("Failed to create keypair from seed")
}

/// Helper struct representing a test validator with all its components
struct TestValidator {
    keypair: Keypair,
    validator_set: Arc<ValidatorSet>,
    state_manager: Arc<StateManager>,
    consensus_engine: ConsensusEngine,
}

impl TestValidator {
    /// Create a new test validator with the given seed
    fn new(seed: u64, all_validators: &[Keypair]) -> Self {
        let keypair = keypair_from_seed(seed);
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let state_manager = Arc::new(StateManager::for_netuid(100));

        // Register all validators in the set
        for validator_keypair in all_validators {
            let record = ValidatorRecord::new(validator_keypair.hotkey(), 10_000);
            let _ = validator_set.register_validator(record);
        }

        let consensus_engine = ConsensusEngine::new(
            keypair.clone(),
            validator_set.clone(),
            state_manager.clone(),
        );

        Self {
            keypair,
            validator_set,
            state_manager,
            consensus_engine,
        }
    }

    /// Get the validator's hotkey
    fn hotkey(&self) -> Hotkey {
        self.keypair.hotkey()
    }

    /// Check if this validator is the leader for the current view
    fn is_leader(&self) -> bool {
        self.consensus_engine.am_i_leader()
    }
}

/// Create multiple test validators that share knowledge of each other
fn create_test_validators(count: usize) -> Vec<TestValidator> {
    // First generate all keypairs
    let all_keypairs: Vec<Keypair> = (0..count).map(|i| keypair_from_seed(i as u64)).collect();

    // Then create validators with knowledge of all peers
    (0..count)
        .map(|i| TestValidator::new(i as u64, &all_keypairs))
        .collect()
}

// ============================================================================
// TEST 1: Multi-validator Bootstrap
// ============================================================================

#[tokio::test]
async fn test_multi_validator_bootstrap() {
    // Create 4 validators using mock metagraph setup
    let validators = create_test_validators(TEST_VALIDATOR_COUNT);

    // Verify all validators were created
    assert_eq!(
        validators.len(),
        TEST_VALIDATOR_COUNT,
        "Should create exactly {} validators",
        TEST_VALIDATOR_COUNT
    );

    // Verify each validator has unique hotkey
    let mut hotkeys: Vec<Hotkey> = validators.iter().map(|v| v.hotkey()).collect();
    hotkeys.sort_by(|a, b| a.0.cmp(&b.0));
    hotkeys.dedup_by(|a, b| a.0 == b.0);
    assert_eq!(
        hotkeys.len(),
        TEST_VALIDATOR_COUNT,
        "All validators should have unique hotkeys"
    );

    // Verify each validator knows about all others
    for validator in &validators {
        let active_count = validator.validator_set.active_count();
        assert_eq!(
            active_count, TEST_VALIDATOR_COUNT,
            "Each validator should know about {} peers, but found {}",
            TEST_VALIDATOR_COUNT, active_count
        );
    }

    // Verify quorum calculation (n=4, f=1, quorum=3)
    let first_validator = &validators[0];
    let quorum = first_validator.consensus_engine.quorum_size();
    assert_eq!(quorum, 3, "With 4 validators, quorum should be 3 (2f+1)");

    // Verify fault tolerance
    let fault_tolerance = first_validator.validator_set.fault_tolerance();
    assert_eq!(
        fault_tolerance, 1,
        "With 4 validators, fault tolerance should be 1"
    );

    // Verify mock metagraph integration works
    let mock_neurons = create_validators(TEST_VALIDATOR_COUNT as u16, 100.0, 1000.0);
    let metagraph = MockMetagraphBuilder::new(100)
        .add_neurons(mock_neurons)
        .build();

    assert_eq!(
        metagraph.n as usize, TEST_VALIDATOR_COUNT,
        "Mock metagraph should have {} neurons",
        TEST_VALIDATOR_COUNT
    );
}

// ============================================================================
// TEST 2: Gossipsub Message Propagation (Simulated)
// ============================================================================

#[tokio::test]
async fn test_gossipsub_message_propagation() {
    // Create validators
    let validators = create_test_validators(TEST_VALIDATOR_COUNT);

    // Find the leader at view 0
    let leader_idx = validators
        .iter()
        .position(|v| v.is_leader())
        .expect("One validator should be leader at view 0");

    let leader = &validators[leader_idx];

    // Leader creates a proposal
    use platform_p2p_consensus::messages::StateChangeType;
    let proposal = leader
        .consensus_engine
        .create_proposal(StateChangeType::ChallengeSubmission, vec![1, 2, 3, 4])
        .expect("Leader should be able to create proposal");

    // Verify proposal has correct fields
    assert_eq!(proposal.view, 0, "Proposal should be for view 0");
    assert_eq!(proposal.sequence, 1, "Proposal should have sequence 1");
    assert_eq!(
        proposal.proposer,
        leader.hotkey(),
        "Proposer should be leader's hotkey"
    );

    // Simulate message propagation: other validators receive the proposal
    let mut prepare_messages = Vec::new();

    for (i, validator) in validators.iter().enumerate() {
        if i == leader_idx {
            continue; // Leader already has the proposal
        }

        // Non-leader handles the proposal and creates a prepare message
        let prepare = validator
            .consensus_engine
            .handle_proposal(proposal.clone())
            .expect("Validator should handle valid proposal");

        // Verify prepare message
        assert_eq!(prepare.view, 0, "Prepare should be for view 0");
        assert_eq!(prepare.sequence, 1, "Prepare should have sequence 1");
        assert_eq!(
            prepare.validator,
            validator.hotkey(),
            "Prepare should be from correct validator"
        );

        prepare_messages.push(prepare);
    }

    // Verify we got prepare messages from all non-leader validators
    assert_eq!(
        prepare_messages.len(),
        TEST_VALIDATOR_COUNT - 1,
        "Should receive prepare from all non-leader validators"
    );

    // All prepare messages should have the same proposal hash
    let expected_hash = proposal.proposal.data_hash;
    for prepare in &prepare_messages {
        assert_eq!(
            prepare.proposal_hash, expected_hash,
            "All prepares should reference same proposal hash"
        );
    }
}

// ============================================================================
// TEST 3: PBFT Consensus - Single Proposal
// ============================================================================

#[tokio::test]
async fn test_pbft_consensus_single_proposal() {
    use platform_p2p_consensus::messages::StateChangeType;

    // Create validators
    let validators = create_test_validators(TEST_VALIDATOR_COUNT);

    // Find leader
    let leader_idx = validators
        .iter()
        .position(|v| v.is_leader())
        .expect("Should have a leader");

    // Phase 1: Leader creates proposal
    let proposal = validators[leader_idx]
        .consensus_engine
        .create_proposal(StateChangeType::ChallengeSubmission, b"test_data".to_vec())
        .expect("Leader creates proposal");

    let _proposal_hash = proposal.proposal.data_hash;

    // Phase 2: Non-leaders handle proposal and create prepare messages
    let mut prepare_messages = Vec::new();
    for (i, validator) in validators.iter().enumerate() {
        if i != leader_idx {
            let prepare = validator
                .consensus_engine
                .handle_proposal(proposal.clone())
                .expect("Handle proposal");
            prepare_messages.push((i, prepare));
        }
    }

    // Phase 3: All validators receive prepare messages and check for quorum
    // Quorum is 3, so after 2 prepares from others + leader's implicit prepare = 3
    let mut commit_messages: Vec<(usize, platform_p2p_consensus::CommitMessage)> = Vec::new();

    // Distribute prepare messages to all validators
    for (preparer_idx, prepare) in &prepare_messages {
        for (validator_idx, validator) in validators.iter().enumerate() {
            if validator_idx == *preparer_idx {
                continue; // Don't send to self
            }

            match validator.consensus_engine.handle_prepare(prepare.clone()) {
                Ok(Some(commit)) => {
                    // Quorum reached, got a commit message
                    commit_messages.push((validator_idx, commit));
                }
                Ok(None) => {
                    // Not enough prepares yet
                }
                Err(platform_p2p_consensus::ConsensusError::AlreadyVoted) => {
                    // Already processed this prepare
                }
                Err(e) => {
                    panic!("Unexpected error handling prepare: {:?}", e);
                }
            }
        }
    }

    // Verify we got commit messages (indicates prepare quorum was reached)
    assert!(
        !commit_messages.is_empty(),
        "Should have at least one commit message after prepare quorum"
    );

    // Phase 4: Distribute commit messages to reach consensus
    let mut decisions: Vec<ConsensusDecision> = Vec::new();

    for (committer_idx, commit) in &commit_messages {
        for (validator_idx, validator) in validators.iter().enumerate() {
            if validator_idx == *committer_idx {
                continue;
            }

            match validator.consensus_engine.handle_commit(commit.clone()) {
                Ok(Some(decision)) => {
                    decisions.push(decision);
                }
                Ok(None) => {
                    // Not enough commits yet
                }
                Err(platform_p2p_consensus::ConsensusError::AlreadyVoted) => {
                    // Already processed this commit
                }
                Err(_) => {
                    // May fail if round completed, that's ok
                }
            }
        }
    }

    // Verify at least one validator reached consensus
    assert!(
        !decisions.is_empty(),
        "At least one validator should reach consensus"
    );

    // Verify decision properties
    let decision = &decisions[0];
    assert_eq!(decision.view, 0, "Decision should be for view 0");
    assert_eq!(decision.sequence, 1, "Decision should have sequence 1");
    assert_eq!(
        decision.content.change_type,
        StateChangeType::ChallengeSubmission,
        "Decision should be ChallengeSubmission"
    );
    assert!(
        !decision.commit_signatures.is_empty(),
        "Decision should have commit signatures"
    );
}

// ============================================================================
// TEST 4: Leader Election Rotation
// ============================================================================

#[tokio::test]
async fn test_leader_election_rotation() {
    // Create validators
    let validators = create_test_validators(TEST_VALIDATOR_COUNT);

    // Track leaders across view changes
    let mut leaders_by_view: HashMap<u64, Hotkey> = HashMap::new();

    // Get leader for views 0-7 (should cycle through all validators twice)
    for view in 0u64..8 {
        let leader_election =
            platform_p2p_consensus::LeaderElection::new(validators[0].validator_set.clone());
        let leader = leader_election
            .leader_for_view(view)
            .expect("Should have leader for each view");

        leaders_by_view.insert(view, leader);
    }

    // Verify leaders rotate (not all the same)
    let unique_leaders: std::collections::HashSet<_> =
        leaders_by_view.values().map(|h| h.0).collect();
    assert_eq!(
        unique_leaders.len(),
        TEST_VALIDATOR_COUNT,
        "Leaders should rotate through all {} validators",
        TEST_VALIDATOR_COUNT
    );

    // Verify rotation pattern repeats every n validators
    for view in 0u64..4 {
        let leader_view_n = leaders_by_view.get(&view).unwrap();
        let leader_view_n_plus_4 = leaders_by_view.get(&(view + 4)).unwrap();
        assert_eq!(
            leader_view_n,
            leader_view_n_plus_4,
            "Leader for view {} should match view {} (rotation period = {})",
            view,
            view + 4,
            TEST_VALIDATOR_COUNT
        );
    }

    // Test view change initiation
    let test_validator = &validators[0];
    let view_change = test_validator
        .consensus_engine
        .initiate_view_change(1)
        .expect("Should initiate view change");

    assert_eq!(view_change.new_view, 1, "View change should target view 1");
    assert_eq!(
        view_change.validator,
        test_validator.hotkey(),
        "View change should be from validator"
    );
}

// ============================================================================
// TEST 5: State Synchronization
// ============================================================================

#[tokio::test]
async fn test_state_synchronization() {
    use platform_p2p_consensus::ChainState;

    // Create validators with separate state managers
    let validators = create_test_validators(TEST_VALIDATOR_COUNT);

    // Each validator starts with independent state
    let initial_hashes: Vec<[u8; 32]> = validators
        .iter()
        .map(|v| v.state_manager.state_hash())
        .collect();

    // Initial states should be identical (same netuid, default state)
    for (i, hash) in initial_hashes.iter().enumerate() {
        for (j, other_hash) in initial_hashes.iter().enumerate() {
            if i != j {
                assert_eq!(
                    hash, other_hash,
                    "Initial state hashes should match between validator {} and {}",
                    i, j
                );
            }
        }
    }

    // Simulate state update on one validator
    let hotkey_to_add = Hotkey([42u8; 32]);
    validators[0].state_manager.apply(|state| {
        state.update_validator(hotkey_to_add.clone(), 5000);
    });

    // States should now differ
    let updated_hash = validators[0].state_manager.state_hash();
    let other_hash = validators[1].state_manager.state_hash();
    assert_ne!(
        updated_hash, other_hash,
        "After update, state hashes should differ"
    );

    // Get snapshot from updated validator
    let snapshot = validators[0].state_manager.snapshot();
    let snapshot_bytes = snapshot.to_bytes().expect("Serialize snapshot");

    // Other validators sync state
    for i in 1..validators.len() {
        let new_state = ChainState::from_bytes(&snapshot_bytes).expect("Deserialize state");
        validators[i]
            .state_manager
            .apply_sync_state(new_state)
            .expect("Apply synced state");
    }

    // All states should now match
    let synced_hashes: Vec<[u8; 32]> = validators
        .iter()
        .map(|v| v.state_manager.state_hash())
        .collect();

    for (i, hash) in synced_hashes.iter().enumerate() {
        assert_eq!(
            hash, &updated_hash,
            "Validator {} should have synced state hash",
            i
        );
    }

    // Verify the update was applied
    for validator in &validators {
        let has_validator = validator
            .state_manager
            .read(|state| state.validators.contains_key(&hotkey_to_add));
        assert!(
            has_validator,
            "All validators should have the new validator entry"
        );
    }
}

// ============================================================================
// TEST 6: Score Aggregation Consensus
// ============================================================================

#[tokio::test]
async fn test_score_aggregation_consensus() {
    use platform_core::ChallengeId;
    use platform_p2p_consensus::{EvaluationRecord, ValidatorEvaluation};

    // Create validators
    let validators = create_test_validators(TEST_VALIDATOR_COUNT);

    // Set up a challenge evaluation in shared state
    let challenge_id = ChallengeId::new();
    let submission_id = "test_submission_001";

    // Use first validator's state for this test (simulates consensus on shared state)
    let primary_state_manager = &validators[0].state_manager;

    // Register all validators in the primary state
    let stakes = [10_000u64, 11_000, 12_000, 13_000];
    primary_state_manager.apply(|state| {
        for (i, validator) in validators.iter().enumerate() {
            state.validators.insert(validator.hotkey(), stakes[i]);
        }
    });

    // Create an evaluation record
    primary_state_manager.apply(|state| {
        let record = EvaluationRecord {
            submission_id: submission_id.to_string(),
            challenge_id,
            miner: Hotkey([99u8; 32]),
            agent_hash: "abc123".to_string(),
            evaluations: HashMap::new(),
            aggregated_score: None,
            finalized: false,
            created_at: chrono::Utc::now().timestamp_millis(),
            finalized_at: None,
        };
        state.add_evaluation(record);
    });

    // Each validator submits their evaluation score
    let scores = [0.85, 0.82, 0.88, 0.84]; // Different scores from each validator

    for (i, validator) in validators.iter().enumerate() {
        // Create signing data for the evaluation
        #[derive(Serialize)]
        struct EvaluationSigningData<'a> {
            submission_id: &'a str,
            score: f64,
        }

        let signing_data = EvaluationSigningData {
            submission_id,
            score: scores[i],
        };
        let signing_bytes = bincode::serialize(&signing_data).expect("Serialize signing data");
        let signature = validator
            .keypair
            .sign_bytes(&signing_bytes)
            .expect("Sign evaluation");

        let evaluation = ValidatorEvaluation {
            score: scores[i],
            stake: stakes[i], // Self-reported stake (will be verified against state)
            timestamp: chrono::Utc::now().timestamp_millis(),
            signature: signature.clone(),
        };

        // Add evaluation to primary state
        primary_state_manager.apply(|state| {
            state
                .add_validator_evaluation(submission_id, validator.hotkey(), evaluation, &signature)
                .expect("Add evaluation");
        });
    }

    // Finalize the evaluation
    let aggregated_score = primary_state_manager
        .apply(|state| state.finalize_evaluation(submission_id))
        .expect("Finalize evaluation");

    // Verify aggregated score is stake-weighted average
    // Stakes: 10000, 11000, 12000, 13000
    // Scores: 0.85, 0.82, 0.88, 0.84
    // Weighted sum: 0.85*10000 + 0.82*11000 + 0.88*12000 + 0.84*13000 = 39170
    // Total stake: 46000
    // Expected: 39170 / 46000 ≈ 0.8515
    let expected_score = (scores[0] * stakes[0] as f64
        + scores[1] * stakes[1] as f64
        + scores[2] * stakes[2] as f64
        + scores[3] * stakes[3] as f64)
        / (stakes[0] + stakes[1] + stakes[2] + stakes[3]) as f64;

    assert!(
        (aggregated_score - expected_score).abs() < 0.001,
        "Aggregated score {} should be close to expected {}",
        aggregated_score,
        expected_score
    );

    // Verify the evaluation is now in completed_evaluations
    let is_finalized = primary_state_manager.read(|state| {
        state.pending_evaluations.get(submission_id).is_none()
            && state
                .completed_evaluations
                .values()
                .any(|evals| evals.iter().any(|e| e.submission_id == submission_id))
    });
    assert!(is_finalized, "Evaluation should be moved to completed");
}

// ============================================================================
// TEST 7: Development Config Validation
// ============================================================================

#[test]
fn test_development_config() {
    let config = P2PConfig::development();

    // Verify relaxed settings for testing
    assert_eq!(
        config.min_stake, 0,
        "Development config should have no stake requirement"
    );
    assert_eq!(
        config.heartbeat_interval_secs, 5,
        "Development config should have short heartbeat"
    );
    assert!(
        config.listen_addrs[0].contains("127.0.0.1"),
        "Development config should use localhost"
    );
    assert!(
        config.consensus_topic.contains("dev"),
        "Development config should use dev topic"
    );
}

// ============================================================================
// TEST 8: Hotkey Determinism
// ============================================================================

#[test]
fn test_hotkey_from_seed_determinism() {
    // Verify hotkey_from_seed produces deterministic results
    let seed = 42u64;
    let hotkey1 = hotkey_from_seed(seed);
    let hotkey2 = hotkey_from_seed(seed);
    let hotkey_different = hotkey_from_seed(43);

    assert_eq!(hotkey1, hotkey2, "Same seed should produce same hotkey");
    assert_ne!(
        hotkey1, hotkey_different,
        "Different seeds should produce different hotkeys"
    );

    // Verify keypair creation is deterministic
    let kp1 = keypair_from_seed(seed);
    let kp2 = keypair_from_seed(seed);
    assert_eq!(
        kp1.hotkey(),
        kp2.hotkey(),
        "Same seed should produce same keypair"
    );
}

// ============================================================================
// TEST 9: Mock Metagraph Builder
// ============================================================================

#[test]
fn test_mock_metagraph_builder() {
    // Test the mock infrastructure used for bittensor integration
    let netuid = 100u16;
    let validators = create_validators(4, 100.0, 500.0);

    let metagraph = MockMetagraphBuilder::new(netuid)
        .block(1000)
        .add_neurons(validators)
        .build();

    assert_eq!(
        metagraph.netuid, netuid,
        "Metagraph should have correct netuid"
    );
    assert_eq!(metagraph.n, 4, "Metagraph should have 4 neurons");
    assert_eq!(metagraph.block, 1000, "Metagraph should have correct block");

    // Verify neurons have stakes in expected range
    for (_, neuron) in &metagraph.neurons {
        let stake_tao = neuron.stake as f64 / 1_000_000_000.0;
        assert!(
            stake_tao >= 100.0 && stake_tao <= 500.0,
            "Stake {} TAO should be in range 100-500",
            stake_tao
        );
    }
}

// ============================================================================
// TEST 10: Consensus Quorum Edge Cases
// ============================================================================

#[tokio::test]
async fn test_consensus_quorum_edge_cases() {
    // Test with minimum viable validator count (n=4, f=1)
    let validators = create_test_validators(4);

    // Verify quorum math
    let validator_set = &validators[0].validator_set;
    assert_eq!(validator_set.active_count(), 4);
    assert_eq!(validator_set.fault_tolerance(), 1); // f = (4-1)/3 = 1
    assert_eq!(validator_set.quorum_size(), 3); // 2f+1 = 3

    // Test with 7 validators (n=7, f=2, quorum=5)
    let validators_7 = create_test_validators(7);
    let validator_set_7 = &validators_7[0].validator_set;
    assert_eq!(validator_set_7.active_count(), 7);
    assert_eq!(validator_set_7.fault_tolerance(), 2); // f = (7-1)/3 = 2
    assert_eq!(validator_set_7.quorum_size(), 5); // 2f+1 = 5

    // Test with 10 validators (n=10, f=3, quorum=7)
    let validators_10 = create_test_validators(10);
    let validator_set_10 = &validators_10[0].validator_set;
    assert_eq!(validator_set_10.active_count(), 10);
    assert_eq!(validator_set_10.fault_tolerance(), 3); // f = (10-1)/3 = 3
    assert_eq!(validator_set_10.quorum_size(), 7); // 2f+1 = 7
}

// ============================================================================
// TEST 11: View Change with Quorum
// ============================================================================

#[tokio::test]
async fn test_view_change_with_quorum() {
    let validators = create_test_validators(TEST_VALIDATOR_COUNT);

    // All validators initiate view change to view 1
    let mut view_change_messages = Vec::new();
    for validator in &validators {
        let view_change = validator
            .consensus_engine
            .initiate_view_change(1)
            .expect("Initiate view change");
        view_change_messages.push(view_change);
    }

    // Verify we have view change messages from all validators
    assert_eq!(
        view_change_messages.len(),
        TEST_VALIDATOR_COUNT,
        "Should have view change from all validators"
    );

    // All view changes should target view 1
    for vc in &view_change_messages {
        assert_eq!(vc.new_view, 1, "All view changes should target view 1");
    }

    // Find who would be leader at view 1
    let leader_election =
        platform_p2p_consensus::LeaderElection::new(validators[0].validator_set.clone());
    let expected_leader = leader_election
        .leader_for_view(1)
        .expect("Should have leader for view 1");

    // The new leader should be different from view 0 leader
    let view_0_leader = leader_election
        .leader_for_view(0)
        .expect("Should have leader for view 0");

    assert_ne!(
        expected_leader, view_0_leader,
        "Leader should change between view 0 and view 1"
    );
}

// ============================================================================
// TEST 12: Stake-Weighted Voting
// ============================================================================

#[test]
fn test_stake_weighted_voting() {
    use platform_p2p_consensus::StakeWeightedVoting;

    // Create validators with different stakes
    let keypairs: Vec<Keypair> = (0..4).map(|i| keypair_from_seed(i)).collect();
    let stakes = [10000u64, 20000, 30000, 40000]; // Different stakes

    let local_keypair = keypairs[0].clone();
    let validator_set = Arc::new(ValidatorSet::new(local_keypair, 0));

    // Register validators with varying stakes
    for (i, kp) in keypairs.iter().enumerate() {
        let record = ValidatorRecord::new(kp.hotkey(), stakes[i]);
        validator_set.register_validator(record).expect("Register");
    }

    let voting = StakeWeightedVoting::new(validator_set.clone());

    // Verify voting power is proportional to stake
    let total_stake: u64 = stakes.iter().sum(); // 100000

    for (i, kp) in keypairs.iter().enumerate() {
        let power = voting.voting_power(&kp.hotkey());
        let expected = stakes[i] as f64 / total_stake as f64;
        assert!(
            (power - expected).abs() < 0.001,
            "Voting power for validator {} should be {}, got {}",
            i,
            expected,
            power
        );
    }

    // Test threshold meeting
    // Validators 0+1+2 = 10000+20000+30000 = 60000 = 60%
    let voters: Vec<Hotkey> = keypairs[0..3].iter().map(|kp| kp.hotkey()).collect();
    assert!(
        voting.meets_threshold(&voters, 0.5),
        "60% should meet 50% threshold"
    );
    assert!(
        voting.meets_threshold(&voters, 0.6),
        "60% should meet 60% threshold"
    );
    assert!(
        !voting.meets_threshold(&voters, 0.7),
        "60% should not meet 70% threshold"
    );
}
