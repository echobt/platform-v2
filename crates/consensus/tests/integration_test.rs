//! Integration tests for platform

use parking_lot::RwLock;
use platform_consensus::{ConsensusConfig, ConsensusState, PBFTEngine};
use platform_core::{
    ChainState, Challenge, ChallengeConfig, Hotkey, Keypair, NetworkConfig, NetworkMessage,
    Proposal, ProposalAction, SignedNetworkMessage, Stake, SudoAction, ValidatorInfo, Vote,
};
use platform_storage::Storage;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_consensus_with_8_validators() {
    // Create 8 validators
    let validators: Vec<Keypair> = (0..8).map(|_| Keypair::generate()).collect();
    let sudo = Keypair::generate();

    // Create chain state
    let mut state = ChainState::new(sudo.hotkey(), NetworkConfig::default());

    // Add all validators
    for (i, v) in validators.iter().enumerate() {
        let stake = Stake::new(10_000_000_000 + (i as u64 * 1_000_000_000));
        let info = ValidatorInfo::new(v.hotkey(), stake);
        state.add_validator(info).unwrap();
    }

    assert_eq!(state.validators.len(), 8);

    // Consensus threshold for 8 validators at 50% = 4
    assert_eq!(state.consensus_threshold(), 4);

    // Create consensus state
    let consensus_state = ConsensusState::new(ConsensusConfig::default());
    consensus_state.set_validator_count(8);

    // Create a proposal
    let proposal = Proposal::new(
        ProposalAction::NewBlock {
            state_hash: [1u8; 32],
        },
        sudo.hotkey(),
        0,
    );

    // Start a round
    let proposal_id = consensus_state.start_round(proposal.clone());

    // Add 3 approve votes (not enough for 50% threshold = 4)
    for i in 0..3 {
        let vote = Vote::approve(proposal_id, validators[i].hotkey());
        let result = consensus_state.add_vote(vote);
        assert!(
            result.is_none(),
            "Should not reach consensus with only {} votes",
            i + 1
        );
    }

    // Add 4th vote (should reach consensus at 50%)
    let vote = Vote::approve(proposal_id, validators[3].hotkey());
    let result = consensus_state.add_vote(vote);

    assert!(matches!(
        result,
        Some(platform_consensus::ConsensusResult::Approved(_))
    ));
}

#[tokio::test]
async fn test_sudo_action() {
    let sudo = Keypair::generate();
    let chain_state = Arc::new(RwLock::new(ChainState::new(
        sudo.hotkey(),
        NetworkConfig::default(),
    )));

    let (tx, _rx) = mpsc::channel(100);
    let engine = PBFTEngine::new(sudo.clone(), chain_state.clone(), tx);

    // Add some validators
    {
        let mut state = chain_state.write();
        for _ in 0..4 {
            let kp = Keypair::generate();
            let info = ValidatorInfo::new(kp.hotkey(), Stake::new(10_000_000_000));
            state.add_validator(info).unwrap();
        }
    }
    engine.sync_validators();

    // Verify sudo access
    assert!(engine.is_sudo());
}

#[tokio::test]
async fn test_challenge_lifecycle() {
    let owner = Keypair::generate();

    // Create a simple WASM challenge
    let wasm = vec![0u8; 100]; // Dummy WASM
    let challenge = Challenge::new(
        "Test Challenge".into(),
        "A test challenge".into(),
        wasm,
        owner.hotkey(),
        ChallengeConfig::default(),
    );

    // Verify hash
    assert!(challenge.verify_code());

    // Store and load
    let dir = tempdir().unwrap();
    let storage = Storage::open(dir.path()).unwrap();

    storage.save_challenge(&challenge).unwrap();
    let loaded = storage.load_challenge(&challenge.id).unwrap().unwrap();

    assert_eq!(loaded.name, challenge.name);
    assert_eq!(loaded.wasm_code, challenge.wasm_code);
}

#[tokio::test]
async fn test_signed_messages() {
    let kp = Keypair::generate();

    // Create and sign a message
    let msg = NetworkMessage::Heartbeat(platform_core::HeartbeatMessage::new(
        kp.hotkey(),
        100,
        [0u8; 32],
    ));

    let signed = SignedNetworkMessage::new(msg, &kp).unwrap();

    // Verify signature
    assert!(signed.verify().unwrap());
    assert_eq!(signed.signer(), &kp.hotkey());
}

#[tokio::test]
async fn test_state_persistence() {
    let sudo = Keypair::generate();
    let mut state = ChainState::new(sudo.hotkey(), NetworkConfig::default());

    // Add validators
    for _ in 0..4 {
        let kp = Keypair::generate();
        let info = ValidatorInfo::new(kp.hotkey(), Stake::new(10_000_000_000));
        state.add_validator(info).unwrap();
    }

    // Add a challenge
    let challenge = Challenge::new(
        "Test".into(),
        "Test".into(),
        vec![0u8; 50],
        sudo.hotkey(),
        ChallengeConfig::default(),
    );
    state.add_challenge(challenge.clone());

    // Increment block
    state.increment_block();
    state.increment_block();

    // Save and load
    let dir = tempdir().unwrap();
    let storage = Storage::open(dir.path()).unwrap();

    storage.save_state(&state).unwrap();
    let loaded = storage.load_state().unwrap().unwrap();

    assert_eq!(loaded.block_height, 2);
    assert_eq!(loaded.validators.len(), 4);
    assert_eq!(loaded.challenges.len(), 1);
}
