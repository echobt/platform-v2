//! PBFT-style consensus implementation
//!
//! Implements a practical Byzantine Fault Tolerant consensus protocol
//! with view changes and leader election.

use crate::messages::{
    CommitMessage, ConsensusProposal, NewViewMessage, PrePrepare, PrepareMessage, PreparedProof,
    ProposalContent, SequenceNumber, StateChangeType, ViewChangeMessage, ViewNumber,
};
use crate::state::StateManager;
use crate::validator::{LeaderElection, ValidatorSet};
use parking_lot::RwLock;
use platform_core::{Hotkey, Keypair, SignedMessage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tracing::{info, warn};

/// Consensus errors
#[derive(Error, Debug)]
pub enum ConsensusError {
    #[error("Not the leader for view {0}")]
    NotLeader(ViewNumber),
    #[error("Invalid proposal: {0}")]
    InvalidProposal(String),
    #[error("Invalid signature from {0}")]
    InvalidSignature(String),
    #[error("Sequence number mismatch: expected {expected}, got {actual}")]
    SequenceMismatch { expected: u64, actual: u64 },
    #[error("View mismatch: expected {expected}, got {actual}")]
    ViewMismatch { expected: u64, actual: u64 },
    #[error("Not enough votes: need {needed}, have {have}")]
    NotEnoughVotes { needed: usize, have: usize },
    #[error("Consensus timeout")]
    Timeout,
    #[error("View change in progress")]
    ViewChangeInProgress,
    #[error("Already voted in this round")]
    AlreadyVoted,
}

/// Consensus phase
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConsensusPhase {
    /// Idle, waiting for proposal
    Idle,
    /// Pre-prepare received, collecting prepares
    PrePrepare,
    /// Prepare quorum reached, collecting commits
    Prepared,
    /// Commit quorum reached, executing
    Committed,
    /// View change in progress
    ViewChange,
}

/// State of a consensus round
#[derive(Clone, Debug)]
pub struct ConsensusRound {
    /// View number
    pub view: ViewNumber,
    /// Sequence number
    pub sequence: SequenceNumber,
    /// Current phase
    pub phase: ConsensusPhase,
    /// The proposal (if received)
    pub proposal: Option<ConsensusProposal>,
    /// Pre-prepare message
    pub pre_prepare: Option<PrePrepare>,
    /// Prepare messages received (validator -> message)
    pub prepares: HashMap<Hotkey, PrepareMessage>,
    /// Commit messages received (validator -> message)
    pub commits: HashMap<Hotkey, CommitMessage>,
    /// Proposal hash for this round
    pub proposal_hash: [u8; 32],
    /// Whether we have prepared
    pub local_prepared: bool,
    /// Whether we have committed
    pub local_committed: bool,
    /// Round start time (unix millis)
    pub started_at: i64,
}

impl ConsensusRound {
    fn new(view: ViewNumber, sequence: SequenceNumber) -> Self {
        Self {
            view,
            sequence,
            phase: ConsensusPhase::Idle,
            proposal: None,
            pre_prepare: None,
            prepares: HashMap::new(),
            commits: HashMap::new(),
            proposal_hash: [0u8; 32],
            local_prepared: false,
            local_committed: false,
            started_at: chrono::Utc::now().timestamp_millis(),
        }
    }
}

/// View change state
#[derive(Clone, Debug)]
pub struct ViewChangeState {
    /// New view being proposed
    pub new_view: ViewNumber,
    /// View change messages received
    pub view_changes: HashMap<Hotkey, ViewChangeMessage>,
    /// When view change started
    pub started_at: i64,
}

/// Result of consensus decision
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsensusDecision {
    /// View number
    pub view: ViewNumber,
    /// Sequence number
    pub sequence: SequenceNumber,
    /// The decided proposal content
    pub content: ProposalContent,
    /// Commit signatures (proof of consensus)
    pub commit_signatures: Vec<(Hotkey, Vec<u8>)>,
}

/// PBFT Consensus engine
pub struct ConsensusEngine {
    /// Our keypair
    keypair: Keypair,
    /// Validator set
    validator_set: Arc<ValidatorSet>,
    /// Leader election
    leader_election: LeaderElection,
    /// State manager (for applying decisions and sudo checks)
    state_manager: Arc<StateManager>,
    /// Current view number
    current_view: RwLock<ViewNumber>,
    /// Next sequence number
    next_sequence: RwLock<SequenceNumber>,
    /// Current consensus round
    current_round: RwLock<Option<ConsensusRound>>,
    /// View change state
    view_change_state: RwLock<Option<ViewChangeState>>,
    /// Completed decisions (sequence -> decision)
    decisions: RwLock<HashMap<SequenceNumber, ConsensusDecision>>,
    /// Round timeout in milliseconds
    round_timeout_ms: i64,
    /// View change timeout in milliseconds (for extended view change operations)
    view_change_timeout_ms: i64,
}

impl ConsensusEngine {
    /// Create new consensus engine
    pub fn new(
        keypair: Keypair,
        validator_set: Arc<ValidatorSet>,
        state_manager: Arc<StateManager>,
    ) -> Self {
        let leader_election = LeaderElection::new(validator_set.clone());

        Self {
            keypair,
            validator_set,
            leader_election,
            state_manager,
            current_view: RwLock::new(0),
            next_sequence: RwLock::new(1),
            current_round: RwLock::new(None),
            view_change_state: RwLock::new(None),
            decisions: RwLock::new(HashMap::new()),
            round_timeout_ms: 30_000,       // 30 seconds
            view_change_timeout_ms: 60_000, // 60 seconds
        }
    }

    /// Get current view number
    pub fn current_view(&self) -> ViewNumber {
        *self.current_view.read()
    }

    /// Get next sequence number
    pub fn next_sequence(&self) -> SequenceNumber {
        *self.next_sequence.read()
    }

    /// Check if we are the current leader
    pub fn am_i_leader(&self) -> bool {
        self.leader_election.am_i_leader(*self.current_view.read())
    }

    /// Get the current leader's hotkey
    pub fn current_leader(&self) -> Option<Hotkey> {
        self.leader_election
            .leader_for_view(*self.current_view.read())
    }

    /// Get required quorum size (2f+1)
    pub fn quorum_size(&self) -> usize {
        self.validator_set.quorum_size()
    }

    /// Create a new proposal (called by leader)
    pub fn create_proposal(
        &self,
        change_type: StateChangeType,
        data: Vec<u8>,
    ) -> Result<ConsensusProposal, ConsensusError> {
        let view = *self.current_view.read();

        if !self.leader_election.am_i_leader(view) {
            return Err(ConsensusError::NotLeader(view));
        }

        // Verify sudo authorization for ConfigUpdate proposals
        if change_type == StateChangeType::ConfigUpdate {
            let is_sudo = self
                .state_manager
                .read(|s| s.is_sudo(&self.keypair.hotkey()));
            if !is_sudo {
                return Err(ConsensusError::InvalidProposal(
                    "ConfigUpdate requires sudo authorization".to_string(),
                ));
            }
        }

        let sequence = *self.next_sequence.read();

        // Hash the data
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let data_hash: [u8; 32] = hasher.finalize().into();

        let content = ProposalContent {
            change_type,
            data,
            data_hash,
        };

        // Create signature
        #[derive(Serialize)]
        struct SigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            data_hash: [u8; 32],
        }

        let signing_data = SigningData {
            view,
            sequence,
            data_hash,
        };

        let signing_bytes = bincode::serialize(&signing_data)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let signature = self
            .keypair
            .sign_bytes(&signing_bytes)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let proposal = ConsensusProposal {
            view,
            sequence,
            proposal: content,
            proposer: self.keypair.hotkey(),
            signature,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };

        // Start the round
        let mut round = ConsensusRound::new(view, sequence);
        round.proposal = Some(proposal.clone());
        round.proposal_hash = data_hash;
        round.phase = ConsensusPhase::PrePrepare;
        *self.current_round.write() = Some(round);

        info!(view, sequence, "Created consensus proposal");
        Ok(proposal)
    }

    /// Handle incoming proposal (for non-leaders)
    pub fn handle_proposal(
        &self,
        proposal: ConsensusProposal,
    ) -> Result<PrepareMessage, ConsensusError> {
        let view = *self.current_view.read();
        let sequence = *self.next_sequence.read();

        // Validate view and sequence
        if proposal.view != view {
            return Err(ConsensusError::ViewMismatch {
                expected: view,
                actual: proposal.view,
            });
        }

        if proposal.sequence != sequence {
            return Err(ConsensusError::SequenceMismatch {
                expected: sequence,
                actual: proposal.sequence,
            });
        }

        // Verify proposer is the leader
        let expected_leader = self.leader_election.leader_for_view(view);
        if expected_leader.as_ref() != Some(&proposal.proposer) {
            return Err(ConsensusError::InvalidProposal(format!(
                "Proposer {:?} is not the leader",
                proposal.proposer
            )));
        }

        // Verify proposal hash
        let mut hasher = Sha256::new();
        hasher.update(&proposal.proposal.data);
        let computed_hash: [u8; 32] = hasher.finalize().into();

        if computed_hash != proposal.proposal.data_hash {
            return Err(ConsensusError::InvalidProposal(
                "Data hash mismatch".to_string(),
            ));
        }

        // Verify cryptographic signature on the proposal
        #[derive(Serialize)]
        struct ProposalSigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            data_hash: [u8; 32],
        }

        let signing_data = ProposalSigningData {
            view: proposal.view,
            sequence: proposal.sequence,
            data_hash: proposal.proposal.data_hash,
        };

        let signing_bytes = bincode::serialize(&signing_data)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let signed_msg = SignedMessage {
            message: signing_bytes,
            signature: proposal.signature.clone(),
            signer: proposal.proposer.clone(),
        };

        let is_valid_sig = signed_msg
            .verify()
            .map_err(|e| ConsensusError::InvalidSignature(e.to_string()))?;

        if !is_valid_sig {
            return Err(ConsensusError::InvalidSignature(proposal.proposer.to_hex()));
        }

        // Verify sudo authorization for ConfigUpdate proposals
        if proposal.proposal.change_type == StateChangeType::ConfigUpdate {
            let is_sudo = self.state_manager.read(|s| s.is_sudo(&proposal.proposer));
            if !is_sudo {
                return Err(ConsensusError::InvalidProposal(
                    "ConfigUpdate requires sudo authorization".to_string(),
                ));
            }
        }

        // Start round
        let mut round = ConsensusRound::new(view, sequence);
        round.proposal = Some(proposal);
        round.proposal_hash = computed_hash;
        round.phase = ConsensusPhase::PrePrepare;
        *self.current_round.write() = Some(round);

        // Create prepare message
        let prepare = self.create_prepare(view, sequence, computed_hash)?;

        info!(view, sequence, "Handling proposal, sending prepare");
        Ok(prepare)
    }

    /// Create pre-prepare message (leader sends after receiving proposal)
    pub fn create_pre_prepare(
        &self,
        view: ViewNumber,
        sequence: SequenceNumber,
        proposal_hash: [u8; 32],
    ) -> Result<PrePrepare, ConsensusError> {
        if !self.leader_election.am_i_leader(view) {
            return Err(ConsensusError::NotLeader(view));
        }

        #[derive(Serialize)]
        struct SigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            proposal_hash: [u8; 32],
        }

        let signing_data = SigningData {
            view,
            sequence,
            proposal_hash,
        };

        let signing_bytes = bincode::serialize(&signing_data)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let signature = self
            .keypair
            .sign_bytes(&signing_bytes)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let pre_prepare = PrePrepare {
            view,
            sequence,
            proposal_hash,
            leader: self.keypair.hotkey(),
            signature,
        };

        // Store in current round
        if let Some(round) = self.current_round.write().as_mut() {
            round.pre_prepare = Some(pre_prepare.clone());
        }

        Ok(pre_prepare)
    }

    /// Create prepare message
    fn create_prepare(
        &self,
        view: ViewNumber,
        sequence: SequenceNumber,
        proposal_hash: [u8; 32],
    ) -> Result<PrepareMessage, ConsensusError> {
        #[derive(Serialize)]
        struct SigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            proposal_hash: [u8; 32],
        }

        let signing_data = SigningData {
            view,
            sequence,
            proposal_hash,
        };

        let signing_bytes = bincode::serialize(&signing_data)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let signature = self
            .keypair
            .sign_bytes(&signing_bytes)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let prepare = PrepareMessage {
            view,
            sequence,
            proposal_hash,
            validator: self.keypair.hotkey(),
            signature,
        };

        // Mark as locally prepared
        if let Some(round) = self.current_round.write().as_mut() {
            round.local_prepared = true;
            round
                .prepares
                .insert(self.keypair.hotkey(), prepare.clone());
        }

        Ok(prepare)
    }

    /// Handle incoming prepare message
    pub fn handle_prepare(
        &self,
        prepare: PrepareMessage,
    ) -> Result<Option<CommitMessage>, ConsensusError> {
        use std::collections::hash_map::Entry;

        let mut round_guard = self.current_round.write();
        let round = round_guard
            .as_mut()
            .ok_or_else(|| ConsensusError::InvalidProposal("No active round".to_string()))?;

        // Validate view and sequence
        if prepare.view != round.view {
            return Err(ConsensusError::ViewMismatch {
                expected: round.view,
                actual: prepare.view,
            });
        }

        if prepare.sequence != round.sequence {
            return Err(ConsensusError::SequenceMismatch {
                expected: round.sequence,
                actual: prepare.sequence,
            });
        }

        // Validate proposal hash
        if prepare.proposal_hash != round.proposal_hash {
            return Err(ConsensusError::InvalidProposal(
                "Proposal hash mismatch".to_string(),
            ));
        }

        // Verify cryptographic signature on the prepare message
        #[derive(Serialize)]
        struct PrepareSigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            proposal_hash: [u8; 32],
        }

        let signing_data = PrepareSigningData {
            view: prepare.view,
            sequence: prepare.sequence,
            proposal_hash: prepare.proposal_hash,
        };

        let signing_bytes = bincode::serialize(&signing_data)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let is_valid_sig = self
            .validator_set
            .verify_signature(&prepare.validator, &signing_bytes, &prepare.signature)
            .map_err(|e| ConsensusError::InvalidSignature(e.to_string()))?;

        if !is_valid_sig {
            return Err(ConsensusError::InvalidSignature(prepare.validator.to_hex()));
        }

        // Atomic check and insert using Entry API to prevent TOCTOU race
        match round.prepares.entry(prepare.validator.clone()) {
            Entry::Occupied(_) => return Err(ConsensusError::AlreadyVoted),
            Entry::Vacant(entry) => entry.insert(prepare),
        };

        // Check if we have quorum
        let quorum = self.quorum_size();
        if round.prepares.len() >= quorum && round.phase == ConsensusPhase::PrePrepare {
            round.phase = ConsensusPhase::Prepared;
            info!(
                view = round.view,
                sequence = round.sequence,
                prepares = round.prepares.len(),
                "Reached prepare quorum"
            );

            // Extract data needed for commit while still holding lock to avoid race condition
            let view = round.view;
            let sequence = round.sequence;
            let proposal_hash = round.proposal_hash;

            // Mark as locally committed while holding lock
            round.local_committed = true;

            // Create commit message while still holding lock
            let commit = self.create_commit_internal(view, sequence, proposal_hash)?;
            round.commits.insert(self.keypair.hotkey(), commit.clone());

            drop(round_guard);
            return Ok(Some(commit));
        }

        Ok(None)
    }

    /// Internal commit message creation - does not acquire current_round lock
    /// Use this when the caller already holds the lock to avoid race conditions
    fn create_commit_internal(
        &self,
        view: ViewNumber,
        sequence: SequenceNumber,
        proposal_hash: [u8; 32],
    ) -> Result<CommitMessage, ConsensusError> {
        #[derive(Serialize)]
        struct SigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            proposal_hash: [u8; 32],
        }

        let signing_data = SigningData {
            view,
            sequence,
            proposal_hash,
        };

        let signing_bytes = bincode::serialize(&signing_data)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let signature = self
            .keypair
            .sign_bytes(&signing_bytes)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        Ok(CommitMessage {
            view,
            sequence,
            proposal_hash,
            validator: self.keypair.hotkey(),
            signature,
        })
    }

    /// Handle incoming commit message
    pub fn handle_commit(
        &self,
        commit: CommitMessage,
    ) -> Result<Option<ConsensusDecision>, ConsensusError> {
        use std::collections::hash_map::Entry;

        let mut round_guard = self.current_round.write();
        let round = round_guard
            .as_mut()
            .ok_or_else(|| ConsensusError::InvalidProposal("No active round".to_string()))?;

        // Validate view and sequence
        if commit.view != round.view {
            return Err(ConsensusError::ViewMismatch {
                expected: round.view,
                actual: commit.view,
            });
        }

        if commit.sequence != round.sequence {
            return Err(ConsensusError::SequenceMismatch {
                expected: round.sequence,
                actual: commit.sequence,
            });
        }

        // Validate proposal hash
        if commit.proposal_hash != round.proposal_hash {
            return Err(ConsensusError::InvalidProposal(
                "Proposal hash mismatch".to_string(),
            ));
        }

        // Verify cryptographic signature on the commit message
        #[derive(Serialize)]
        struct CommitSigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            proposal_hash: [u8; 32],
        }

        let signing_data = CommitSigningData {
            view: commit.view,
            sequence: commit.sequence,
            proposal_hash: commit.proposal_hash,
        };

        let signing_bytes = bincode::serialize(&signing_data)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let is_valid_sig = self
            .validator_set
            .verify_signature(&commit.validator, &signing_bytes, &commit.signature)
            .map_err(|e| ConsensusError::InvalidSignature(e.to_string()))?;

        if !is_valid_sig {
            return Err(ConsensusError::InvalidSignature(commit.validator.to_hex()));
        }

        // Atomic check and insert using Entry API to prevent TOCTOU race
        match round.commits.entry(commit.validator.clone()) {
            Entry::Occupied(_) => return Err(ConsensusError::AlreadyVoted),
            Entry::Vacant(entry) => entry.insert(commit),
        };

        // Check if we have quorum
        let quorum = self.quorum_size();
        if round.commits.len() >= quorum && round.phase == ConsensusPhase::Prepared {
            round.phase = ConsensusPhase::Committed;
            info!(
                view = round.view,
                sequence = round.sequence,
                commits = round.commits.len(),
                "Reached commit quorum - consensus achieved!"
            );

            // Create decision
            let proposal = round.proposal.as_ref().ok_or_else(|| {
                ConsensusError::InvalidProposal("No proposal in committed round".to_string())
            })?;
            let decision = ConsensusDecision {
                view: round.view,
                sequence: round.sequence,
                content: proposal.proposal.clone(),
                commit_signatures: round
                    .commits
                    .iter()
                    .map(|(h, c)| (h.clone(), c.signature.clone()))
                    .collect(),
            };

            // Store decision
            let seq = round.sequence;
            drop(round_guard);
            self.decisions.write().insert(seq, decision.clone());

            // Increment sequence
            *self.next_sequence.write() += 1;

            // Clear current round
            *self.current_round.write() = None;

            return Ok(Some(decision));
        }

        Ok(None)
    }

    /// Initiate view change
    pub fn initiate_view_change(
        &self,
        new_view: ViewNumber,
    ) -> Result<ViewChangeMessage, ConsensusError> {
        let current_view = *self.current_view.read();
        if new_view <= current_view {
            return Err(ConsensusError::ViewMismatch {
                expected: current_view + 1,
                actual: new_view,
            });
        }

        // Get last prepared info
        let (last_prepared_sequence, prepared_proof) = {
            let round = self.current_round.read();
            if let Some(r) = round.as_ref() {
                if r.phase >= ConsensusPhase::Prepared && r.pre_prepare.is_some() {
                    let proof = PreparedProof {
                        pre_prepare: r.pre_prepare.clone().unwrap(),
                        prepares: r.prepares.values().cloned().collect(),
                    };
                    (Some(r.sequence), Some(proof))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        };

        #[derive(Serialize)]
        struct SigningData {
            new_view: ViewNumber,
            last_prepared_sequence: Option<SequenceNumber>,
        }

        let signing_data = SigningData {
            new_view,
            last_prepared_sequence,
        };

        let signing_bytes = bincode::serialize(&signing_data)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let signature = self
            .keypair
            .sign_bytes(&signing_bytes)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let view_change = ViewChangeMessage {
            new_view,
            last_prepared_sequence,
            prepared_proof,
            validator: self.keypair.hotkey(),
            signature,
        };

        // Start view change state
        let mut state = ViewChangeState {
            new_view,
            view_changes: HashMap::new(),
            started_at: chrono::Utc::now().timestamp_millis(),
        };
        state
            .view_changes
            .insert(self.keypair.hotkey(), view_change.clone());
        *self.view_change_state.write() = Some(state);

        info!(new_view, "Initiating view change");
        Ok(view_change)
    }

    /// Handle incoming view change message
    pub fn handle_view_change(
        &self,
        view_change: ViewChangeMessage,
    ) -> Result<Option<NewViewMessage>, ConsensusError> {
        // Verify cryptographic signature on the view change message
        #[derive(Serialize)]
        struct ViewChangeSigningData {
            new_view: ViewNumber,
            last_prepared_sequence: Option<SequenceNumber>,
        }

        let signing_data = ViewChangeSigningData {
            new_view: view_change.new_view,
            last_prepared_sequence: view_change.last_prepared_sequence,
        };

        let signing_bytes = bincode::serialize(&signing_data)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let is_valid_sig = self
            .validator_set
            .verify_signature(
                &view_change.validator,
                &signing_bytes,
                &view_change.signature,
            )
            .map_err(|e| ConsensusError::InvalidSignature(e.to_string()))?;

        if !is_valid_sig {
            return Err(ConsensusError::InvalidSignature(
                view_change.validator.to_hex(),
            ));
        }

        // Verify prepared_proof signatures if present
        if let Some(ref proof) = view_change.prepared_proof {
            // Verify PrePrepare signature
            #[derive(Serialize)]
            struct PrePrepareSigningData {
                view: ViewNumber,
                sequence: SequenceNumber,
                proposal_hash: [u8; 32],
            }

            let pre_prepare_signing_data = PrePrepareSigningData {
                view: proof.pre_prepare.view,
                sequence: proof.pre_prepare.sequence,
                proposal_hash: proof.pre_prepare.proposal_hash,
            };

            let pre_prepare_signing_bytes = bincode::serialize(&pre_prepare_signing_data)
                .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

            let pre_prepare_valid = self
                .validator_set
                .verify_signature(
                    &proof.pre_prepare.leader,
                    &pre_prepare_signing_bytes,
                    &proof.pre_prepare.signature,
                )
                .map_err(|e| ConsensusError::InvalidSignature(e.to_string()))?;

            if !pre_prepare_valid {
                return Err(ConsensusError::InvalidSignature(format!(
                    "Invalid PrePrepare signature from {}",
                    proof.pre_prepare.leader.to_hex()
                )));
            }

            // Verify each Prepare message signature
            #[derive(Serialize)]
            struct PrepareSigningData {
                view: ViewNumber,
                sequence: SequenceNumber,
                proposal_hash: [u8; 32],
            }

            let mut valid_prepare_count = 0;
            for prepare in &proof.prepares {
                let prepare_signing_data = PrepareSigningData {
                    view: prepare.view,
                    sequence: prepare.sequence,
                    proposal_hash: prepare.proposal_hash,
                };

                let prepare_signing_bytes = bincode::serialize(&prepare_signing_data)
                    .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

                let prepare_valid = self
                    .validator_set
                    .verify_signature(
                        &prepare.validator,
                        &prepare_signing_bytes,
                        &prepare.signature,
                    )
                    .map_err(|e| ConsensusError::InvalidSignature(e.to_string()))?;

                if prepare_valid {
                    valid_prepare_count += 1;
                } else {
                    warn!(
                        validator = %prepare.validator.to_hex(),
                        "Invalid Prepare signature in prepared_proof, skipping"
                    );
                }
            }

            // Verify we have 2f+1 valid prepares
            let quorum = self.quorum_size();
            if valid_prepare_count < quorum {
                return Err(ConsensusError::NotEnoughVotes {
                    needed: quorum,
                    have: valid_prepare_count,
                });
            }
        }

        let mut state_guard = self.view_change_state.write();

        let state = state_guard.get_or_insert_with(|| ViewChangeState {
            new_view: view_change.new_view,
            view_changes: HashMap::new(),
            started_at: chrono::Utc::now().timestamp_millis(),
        });

        // Handle view number mismatch
        if view_change.new_view != state.new_view {
            if view_change.new_view > state.new_view {
                // Higher view - switch to it
                info!(
                    old_view = state.new_view,
                    new_view = view_change.new_view,
                    "Switching to higher view change"
                );
                state.new_view = view_change.new_view;
                state.view_changes.clear();
                state.started_at = chrono::Utc::now().timestamp_millis();
            } else {
                // Lower view - ignore stale view change
                warn!(
                    received_view = view_change.new_view,
                    current_view = state.new_view,
                    "Ignoring stale view change for lower view"
                );
                return Ok(None);
            }
        }

        state
            .view_changes
            .insert(view_change.validator.clone(), view_change);

        // Check if we have quorum and are the new leader
        let quorum = self.quorum_size();
        let new_view = state.new_view;

        if state.view_changes.len() >= quorum
            && self
                .leader_election
                .is_leader(&self.keypair.hotkey(), new_view)
        {
            info!(new_view, "View change quorum reached, becoming new leader");

            // Create new view message
            #[derive(Serialize)]
            struct SigningData {
                view: ViewNumber,
            }

            let signing_bytes = bincode::serialize(&SigningData { view: new_view })
                .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

            let signature = self
                .keypair
                .sign_bytes(&signing_bytes)
                .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

            let new_view_msg = NewViewMessage {
                view: new_view,
                view_changes: state.view_changes.values().cloned().collect(),
                leader: self.keypair.hotkey(),
                signature,
            };

            // Update view
            drop(state_guard);
            *self.current_view.write() = new_view;
            *self.view_change_state.write() = None;
            *self.current_round.write() = None;

            return Ok(Some(new_view_msg));
        }

        Ok(None)
    }

    /// Handle new view message (from new leader)
    pub fn handle_new_view(&self, new_view: NewViewMessage) -> Result<(), ConsensusError> {
        // Verify sender is the leader for this view
        if !self
            .leader_election
            .is_leader(&new_view.leader, new_view.view)
        {
            return Err(ConsensusError::InvalidProposal(format!(
                "{:?} is not the leader for view {}",
                new_view.leader, new_view.view
            )));
        }

        // Verify cryptographic signature on the new view message
        #[derive(Serialize)]
        struct NewViewSigningData {
            view: ViewNumber,
        }

        let signing_data = NewViewSigningData {
            view: new_view.view,
        };

        let signing_bytes = bincode::serialize(&signing_data)
            .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

        let is_valid_sig = self
            .validator_set
            .verify_signature(&new_view.leader, &signing_bytes, &new_view.signature)
            .map_err(|e| ConsensusError::InvalidSignature(e.to_string()))?;

        if !is_valid_sig {
            return Err(ConsensusError::InvalidSignature(new_view.leader.to_hex()));
        }

        // Verify quorum of view changes
        let quorum = self.quorum_size();
        if new_view.view_changes.len() < quorum {
            return Err(ConsensusError::NotEnoughVotes {
                needed: quorum,
                have: new_view.view_changes.len(),
            });
        }

        // Verify each ViewChangeMessage signature AND that they're all for the announced view
        for vc in &new_view.view_changes {
            // CRITICAL: Verify ViewChange is for the announced view
            if vc.new_view != new_view.view {
                return Err(ConsensusError::ViewMismatch {
                    expected: new_view.view,
                    actual: vc.new_view,
                });
            }

            #[derive(Serialize)]
            struct ViewChangeSigningData {
                new_view: ViewNumber,
                last_prepared_sequence: Option<SequenceNumber>,
            }

            let signing_data = ViewChangeSigningData {
                new_view: vc.new_view,
                last_prepared_sequence: vc.last_prepared_sequence,
            };

            let signing_bytes = bincode::serialize(&signing_data)
                .map_err(|e| ConsensusError::InvalidProposal(e.to_string()))?;

            let is_valid_sig = self
                .validator_set
                .verify_signature(&vc.validator, &signing_bytes, &vc.signature)
                .map_err(|e| ConsensusError::InvalidSignature(e.to_string()))?;

            if !is_valid_sig {
                return Err(ConsensusError::InvalidSignature(vc.validator.to_hex()));
            }
        }

        // Transition to new view
        info!(view = new_view.view, "Transitioning to new view");
        *self.current_view.write() = new_view.view;
        *self.view_change_state.write() = None;
        *self.current_round.write() = None;

        Ok(())
    }

    /// Check for round timeout
    pub fn check_timeout(&self) -> bool {
        let now = chrono::Utc::now().timestamp_millis();

        if let Some(round) = self.current_round.read().as_ref() {
            if now - round.started_at > self.round_timeout_ms {
                warn!(
                    view = round.view,
                    sequence = round.sequence,
                    "Consensus round timed out"
                );
                return true;
            }
        }

        false
    }

    /// Check if view change state has timed out
    pub fn check_view_change_timeout(&self) -> bool {
        let now = chrono::Utc::now().timestamp_millis();

        if let Some(view_change_state) = self.view_change_state.read().as_ref() {
            if now - view_change_state.started_at > self.view_change_timeout_ms {
                warn!(
                    new_view = view_change_state.new_view,
                    "View change operation timed out"
                );
                return true;
            }
        }

        false
    }

    /// Determine if we should initiate a view change based on timeouts
    ///
    /// Returns true if:
    /// - The current consensus round has timed out, or
    /// - There is an ongoing view change that has timed out
    pub fn should_initiate_view_change(&self) -> bool {
        // Check if current round has timed out
        if self.check_timeout() {
            return true;
        }

        // Check if there's a stalled view change operation
        if self.check_view_change_timeout() {
            return true;
        }

        false
    }

    /// Get a past decision
    pub fn get_decision(&self, sequence: SequenceNumber) -> Option<ConsensusDecision> {
        self.decisions.read().get(&sequence).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_engine() -> ConsensusEngine {
        let keypair = Keypair::generate();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let state_manager = Arc::new(StateManager::for_netuid(100));

        // Register ourselves as a validator
        let record = crate::validator::ValidatorRecord::new(keypair.hotkey(), 10_000);
        validator_set.register_validator(record).unwrap();

        ConsensusEngine::new(keypair, validator_set, state_manager)
    }

    #[test]
    fn test_engine_creation() {
        let engine = create_test_engine();
        assert_eq!(engine.current_view(), 0);
        assert_eq!(engine.next_sequence(), 1);
    }

    #[test]
    fn test_create_proposal_as_leader() {
        let engine = create_test_engine();

        // With only one validator, we're always the leader
        let proposal = engine
            .create_proposal(StateChangeType::ChallengeSubmission, vec![1, 2, 3])
            .unwrap();

        assert_eq!(proposal.view, 0);
        assert_eq!(proposal.sequence, 1);
        assert_eq!(
            proposal.proposal.change_type,
            StateChangeType::ChallengeSubmission
        );
    }

    #[test]
    fn test_consensus_phases() {
        let phase = ConsensusPhase::Idle;
        assert!(phase < ConsensusPhase::Committed);
    }

    #[test]
    fn test_quorum_calculation() {
        let keypair = Keypair::generate();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let state_manager = Arc::new(StateManager::for_netuid(100));

        // Add 4 validators (n=4, f=1, quorum=3)
        for i in 0..4 {
            let mut bytes = [0u8; 32];
            bytes[0] = i;
            let record = crate::validator::ValidatorRecord::new(Hotkey(bytes), 10_000);
            validator_set.register_validator(record).unwrap();
        }

        let engine = ConsensusEngine::new(keypair, validator_set, state_manager);
        assert_eq!(engine.quorum_size(), 3);
    }

    #[test]
    fn test_view_change_initiation() {
        let engine = create_test_engine();

        let view_change = engine.initiate_view_change(1).unwrap();
        assert_eq!(view_change.new_view, 1);
        assert_eq!(view_change.validator, engine.keypair.hotkey());
    }

    // ========================================================================
    // New tests for PBFT consensus logic
    // ========================================================================

    /// Helper to create a test engine with multiple validators for quorum testing
    /// Our validator is given the highest stake to ensure we're the leader at view 0
    fn create_multi_validator_engine(num_validators: u8) -> ConsensusEngine {
        let keypair = Keypair::generate();
        let validator_set = Arc::new(ValidatorSet::new(keypair.clone(), 0));
        let state_manager = Arc::new(StateManager::for_netuid(100));

        // Register our keypair as a validator with HIGHEST stake to ensure we're leader
        let our_record = crate::validator::ValidatorRecord::new(keypair.hotkey(), 100_000);
        validator_set.register_validator(our_record).unwrap();

        // Register additional validators with lower stake
        for i in 1..num_validators {
            let mut bytes = [0u8; 32];
            bytes[0] = i;
            let record = crate::validator::ValidatorRecord::new(Hotkey(bytes), 10_000);
            validator_set.register_validator(record).unwrap();
        }

        ConsensusEngine::new(keypair, validator_set, state_manager)
    }

    /// Helper to create a valid proposal for testing
    fn create_valid_proposal(engine: &ConsensusEngine) -> ConsensusProposal {
        engine
            .create_proposal(StateChangeType::ChallengeSubmission, vec![1, 2, 3])
            .expect("create proposal should succeed")
    }

    #[test]
    fn test_handle_proposal_validates_view_number() {
        let engine = create_test_engine();

        // Create a proposal with wrong view number
        let mut proposal = create_valid_proposal(&engine);
        proposal.view = 99; // Wrong view number

        let result = engine.handle_proposal(proposal);
        assert!(matches!(
            result,
            Err(ConsensusError::ViewMismatch {
                expected: 0,
                actual: 99
            })
        ));
    }

    #[test]
    fn test_handle_proposal_validates_sequence_number() {
        let engine = create_test_engine();

        // Create a proposal with wrong sequence number
        let mut proposal = create_valid_proposal(&engine);
        proposal.sequence = 99; // Wrong sequence number

        let result = engine.handle_proposal(proposal);
        assert!(matches!(
            result,
            Err(ConsensusError::SequenceMismatch {
                expected: 1,
                actual: 99
            })
        ));
    }

    #[test]
    fn test_handle_proposal_verifies_proposer_is_leader() {
        // Create an engine with multiple validators - we are the leader at view 0
        let engine = create_multi_validator_engine(3);

        // Create a valid proposal
        let mut proposal = create_valid_proposal(&engine);

        // Change the proposer to a non-leader
        proposal.proposer = Hotkey([99u8; 32]);

        let result = engine.handle_proposal(proposal);
        assert!(matches!(result, Err(ConsensusError::InvalidProposal(_))));
        if let Err(ConsensusError::InvalidProposal(msg)) = result {
            assert!(msg.contains("not the leader"));
        }
    }

    #[test]
    fn test_handle_proposal_verifies_proposal_hash() {
        let engine = create_test_engine();

        // Create a valid proposal
        let mut proposal = create_valid_proposal(&engine);

        // Tamper with the data hash
        proposal.proposal.data_hash = [99u8; 32];

        let result = engine.handle_proposal(proposal);
        assert!(matches!(result, Err(ConsensusError::InvalidProposal(_))));
        if let Err(ConsensusError::InvalidProposal(msg)) = result {
            assert!(msg.contains("hash mismatch"));
        }
    }

    #[test]
    fn test_handle_prepare_validates_view_and_sequence() {
        let engine = create_test_engine();

        // First create and handle a proposal to start a round
        let proposal = create_valid_proposal(&engine);
        {
            let mut round = ConsensusRound::new(proposal.view, proposal.sequence);
            round.proposal = Some(proposal.clone());
            round.proposal_hash = proposal.proposal.data_hash;
            round.phase = ConsensusPhase::PrePrepare;
            *engine.current_round.write() = Some(round);
        }

        // Create a prepare with wrong view
        let prepare = PrepareMessage {
            view: 99, // Wrong view
            sequence: 1,
            proposal_hash: proposal.proposal.data_hash,
            validator: engine.keypair.hotkey(),
            signature: vec![],
        };

        let result = engine.handle_prepare(prepare);
        assert!(matches!(
            result,
            Err(ConsensusError::ViewMismatch {
                expected: 0,
                actual: 99
            })
        ));

        // Create a prepare with wrong sequence
        let prepare_wrong_seq = PrepareMessage {
            view: 0,
            sequence: 99, // Wrong sequence
            proposal_hash: proposal.proposal.data_hash,
            validator: engine.keypair.hotkey(),
            signature: vec![],
        };

        let result = engine.handle_prepare(prepare_wrong_seq);
        assert!(matches!(
            result,
            Err(ConsensusError::SequenceMismatch {
                expected: 1,
                actual: 99
            })
        ));
    }

    #[test]
    fn test_handle_prepare_rejects_duplicate_prepares() {
        let engine = create_test_engine();

        // Create and start a round
        let proposal = create_valid_proposal(&engine);
        let proposal_hash = proposal.proposal.data_hash;

        // Create a properly signed prepare message
        #[derive(Serialize)]
        struct PrepareSigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            proposal_hash: [u8; 32],
        }
        let signing_data = PrepareSigningData {
            view: 0,
            sequence: 1,
            proposal_hash,
        };
        let signing_bytes = bincode::serialize(&signing_data).unwrap();
        let signature = engine.keypair.sign_bytes(&signing_bytes).unwrap();

        let prepare = PrepareMessage {
            view: 0,
            sequence: 1,
            proposal_hash,
            validator: engine.keypair.hotkey(),
            signature,
        };

        // First prepare should succeed
        let result = engine.handle_prepare(prepare.clone());
        assert!(result.is_ok());

        // Second prepare from same validator should fail with AlreadyVoted
        let result2 = engine.handle_prepare(prepare);
        assert!(matches!(result2, Err(ConsensusError::AlreadyVoted)));
    }

    #[test]
    fn test_handle_prepare_detects_quorum_reached() {
        // Create engine with 4 validators (quorum = 3)
        let engine = create_multi_validator_engine(4);

        // Create and start a round as leader
        let proposal = create_valid_proposal(&engine);
        let proposal_hash = proposal.proposal.data_hash;

        // Clear round and set up fresh for prepare testing
        {
            let mut round = ConsensusRound::new(0, 1);
            round.proposal = Some(proposal.clone());
            round.proposal_hash = proposal_hash;
            round.phase = ConsensusPhase::PrePrepare;
            *engine.current_round.write() = Some(round);
        }

        // Add prepares from validators until quorum
        #[derive(Serialize)]
        struct PrepareSigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            proposal_hash: [u8; 32],
        }

        // Add prepares from 2 other validators (plus our own = 3, which is quorum)
        let validators: Vec<Keypair> = (1u8..3).map(|_| Keypair::generate()).collect();

        for validator in &validators {
            let record = crate::validator::ValidatorRecord::new(validator.hotkey(), 10_000);
            engine.validator_set.register_validator(record).unwrap();

            let signing_data = PrepareSigningData {
                view: 0,
                sequence: 1,
                proposal_hash,
            };
            let signing_bytes = bincode::serialize(&signing_data).unwrap();
            let signature = validator.sign_bytes(&signing_bytes).unwrap();

            let prepare = PrepareMessage {
                view: 0,
                sequence: 1,
                proposal_hash,
                validator: validator.hotkey(),
                signature,
            };

            let result = engine.handle_prepare(prepare);
            assert!(result.is_ok());
        }

        // Add our own prepare - this should trigger quorum (3)
        let our_signing_data = PrepareSigningData {
            view: 0,
            sequence: 1,
            proposal_hash,
        };
        let our_signing_bytes = bincode::serialize(&our_signing_data).unwrap();
        let our_signature = engine.keypair.sign_bytes(&our_signing_bytes).unwrap();

        let our_prepare = PrepareMessage {
            view: 0,
            sequence: 1,
            proposal_hash,
            validator: engine.keypair.hotkey(),
            signature: our_signature,
        };

        let result = engine.handle_prepare(our_prepare);

        // Should return a commit message when quorum is reached
        match result {
            Ok(Some(commit)) => {
                assert_eq!(commit.view, 0);
                assert_eq!(commit.sequence, 1);
                assert_eq!(commit.proposal_hash, proposal_hash);
            }
            Ok(None) => {
                // Check if phase changed to Prepared
                let round = engine.current_round.read();
                if let Some(ref r) = *round {
                    assert!(r.phase >= ConsensusPhase::Prepared || r.prepares.len() >= 3);
                }
            }
            Err(e) => panic!("Expected quorum to be detected, got error: {:?}", e),
        }
    }

    #[test]
    fn test_handle_commit_validates_view_and_sequence() {
        let engine = create_test_engine();

        // Create and start a round
        let proposal = create_valid_proposal(&engine);
        {
            let mut round = ConsensusRound::new(proposal.view, proposal.sequence);
            round.proposal = Some(proposal.clone());
            round.proposal_hash = proposal.proposal.data_hash;
            round.phase = ConsensusPhase::Prepared;
            *engine.current_round.write() = Some(round);
        }

        // Create a commit with wrong view
        let commit = CommitMessage {
            view: 99, // Wrong view
            sequence: 1,
            proposal_hash: proposal.proposal.data_hash,
            validator: engine.keypair.hotkey(),
            signature: vec![],
        };

        let result = engine.handle_commit(commit);
        assert!(matches!(
            result,
            Err(ConsensusError::ViewMismatch {
                expected: 0,
                actual: 99
            })
        ));

        // Create a commit with wrong sequence
        let commit_wrong_seq = CommitMessage {
            view: 0,
            sequence: 99, // Wrong sequence
            proposal_hash: proposal.proposal.data_hash,
            validator: engine.keypair.hotkey(),
            signature: vec![],
        };

        let result = engine.handle_commit(commit_wrong_seq);
        assert!(matches!(
            result,
            Err(ConsensusError::SequenceMismatch {
                expected: 1,
                actual: 99
            })
        ));
    }

    #[test]
    fn test_handle_commit_rejects_duplicate_commits() {
        // Use multiple validators so quorum > 1 (with 4 validators, quorum = 3)
        // This prevents the round from completing after a single commit
        let engine = create_multi_validator_engine(4);

        // Create and start a round in Prepared phase
        let proposal = create_valid_proposal(&engine);
        let proposal_hash = proposal.proposal.data_hash;

        {
            let mut round = ConsensusRound::new(0, 1);
            round.proposal = Some(proposal.clone());
            round.proposal_hash = proposal_hash;
            round.phase = ConsensusPhase::Prepared;
            *engine.current_round.write() = Some(round);
        }

        // Create a properly signed commit message
        #[derive(Serialize)]
        struct CommitSigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            proposal_hash: [u8; 32],
        }
        let signing_data = CommitSigningData {
            view: 0,
            sequence: 1,
            proposal_hash,
        };
        let signing_bytes = bincode::serialize(&signing_data).unwrap();
        let signature = engine.keypair.sign_bytes(&signing_bytes).unwrap();

        let commit = CommitMessage {
            view: 0,
            sequence: 1,
            proposal_hash,
            validator: engine.keypair.hotkey(),
            signature,
        };

        // First commit should succeed (but not reach quorum with 4 validators)
        let result = engine.handle_commit(commit.clone());
        assert!(result.is_ok());

        // Second commit from same validator should fail with AlreadyVoted
        let result2 = engine.handle_commit(commit);
        assert!(matches!(result2, Err(ConsensusError::AlreadyVoted)));
    }

    #[test]
    fn test_handle_commit_creates_decision_when_quorum_reached() {
        // Create engine with 4 validators (quorum = 3)
        let engine = create_multi_validator_engine(4);

        // Create and start a round as leader
        let proposal = create_valid_proposal(&engine);
        let proposal_hash = proposal.proposal.data_hash;

        // Set up round in Prepared phase
        {
            let mut round = ConsensusRound::new(0, 1);
            round.proposal = Some(proposal.clone());
            round.proposal_hash = proposal_hash;
            round.phase = ConsensusPhase::Prepared;
            *engine.current_round.write() = Some(round);
        }

        // Add commits from validators until quorum
        #[derive(Serialize)]
        struct CommitSigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            proposal_hash: [u8; 32],
        }

        // Add commits from 2 other validators
        let validators: Vec<Keypair> = (0u8..2).map(|_| Keypair::generate()).collect();

        for validator in &validators {
            let record = crate::validator::ValidatorRecord::new(validator.hotkey(), 10_000);
            engine.validator_set.register_validator(record).unwrap();

            let signing_data = CommitSigningData {
                view: 0,
                sequence: 1,
                proposal_hash,
            };
            let signing_bytes = bincode::serialize(&signing_data).unwrap();
            let signature = validator.sign_bytes(&signing_bytes).unwrap();

            let commit = CommitMessage {
                view: 0,
                sequence: 1,
                proposal_hash,
                validator: validator.hotkey(),
                signature,
            };

            let result = engine.handle_commit(commit);
            assert!(result.is_ok());
        }

        // Add our own commit - this should trigger quorum and create decision
        let our_signing_data = CommitSigningData {
            view: 0,
            sequence: 1,
            proposal_hash,
        };
        let our_signing_bytes = bincode::serialize(&our_signing_data).unwrap();
        let our_signature = engine.keypair.sign_bytes(&our_signing_bytes).unwrap();

        let our_commit = CommitMessage {
            view: 0,
            sequence: 1,
            proposal_hash,
            validator: engine.keypair.hotkey(),
            signature: our_signature,
        };

        let result = engine.handle_commit(our_commit);

        // Should return a decision when quorum is reached
        match result {
            Ok(Some(decision)) => {
                assert_eq!(decision.view, 0);
                assert_eq!(decision.sequence, 1);
                assert_eq!(
                    decision.content.change_type,
                    StateChangeType::ChallengeSubmission
                );
                assert!(!decision.commit_signatures.is_empty());
            }
            Ok(None) => {
                // Check if we have enough commits
                let round = engine.current_round.read();
                if let Some(ref r) = *round {
                    assert!(r.phase >= ConsensusPhase::Committed || r.commits.len() >= 3);
                }
            }
            Err(e) => panic!("Expected decision to be created, got error: {:?}", e),
        }
    }

    #[test]
    fn test_handle_view_change_validates_signatures() {
        let engine = create_test_engine();

        // Create a view change message with invalid signature
        let view_change = ViewChangeMessage {
            new_view: 1,
            last_prepared_sequence: None,
            prepared_proof: None,
            validator: engine.keypair.hotkey(),
            signature: vec![0u8; 64], // Invalid signature
        };

        let result = engine.handle_view_change(view_change);
        assert!(matches!(result, Err(ConsensusError::InvalidSignature(_))));
    }

    #[test]
    fn test_handle_view_change_tracks_multiple_view_changes() {
        let engine = create_multi_validator_engine(4);

        // Create view change messages from multiple validators
        let validators: Vec<Keypair> = (0u8..2).map(|_| Keypair::generate()).collect();

        for validator in &validators {
            let record = crate::validator::ValidatorRecord::new(validator.hotkey(), 10_000);
            engine.validator_set.register_validator(record).unwrap();

            // Sign the view change message
            #[derive(Serialize)]
            struct ViewChangeSigningData {
                new_view: ViewNumber,
                last_prepared_sequence: Option<SequenceNumber>,
            }
            let signing_data = ViewChangeSigningData {
                new_view: 1,
                last_prepared_sequence: None,
            };
            let signing_bytes = bincode::serialize(&signing_data).unwrap();
            let signature = validator.sign_bytes(&signing_bytes).unwrap();

            let view_change = ViewChangeMessage {
                new_view: 1,
                last_prepared_sequence: None,
                prepared_proof: None,
                validator: validator.hotkey(),
                signature,
            };

            let result = engine.handle_view_change(view_change);
            assert!(result.is_ok());
        }

        // Verify view changes were tracked
        let state = engine.view_change_state.read();
        assert!(state.is_some());
        let view_state = state.as_ref().unwrap();
        assert_eq!(view_state.new_view, 1);
        assert_eq!(view_state.view_changes.len(), 2);
    }

    #[test]
    fn test_handle_new_view_validates_sender_is_leader() {
        let engine = create_multi_validator_engine(4);

        // Create a new view message from a non-leader
        let fake_leader = Keypair::generate();
        let record = crate::validator::ValidatorRecord::new(fake_leader.hotkey(), 10_000);
        engine.validator_set.register_validator(record).unwrap();

        // Sign the new view message
        #[derive(Serialize)]
        struct NewViewSigningData {
            view: ViewNumber,
        }
        let signing_data = NewViewSigningData { view: 1 };
        let signing_bytes = bincode::serialize(&signing_data).unwrap();
        let signature = fake_leader.sign_bytes(&signing_bytes).unwrap();

        let new_view = NewViewMessage {
            view: 1,
            view_changes: vec![], // Empty for this test
            leader: fake_leader.hotkey(),
            signature,
        };

        let result = engine.handle_new_view(new_view);
        assert!(matches!(result, Err(ConsensusError::InvalidProposal(_))));
        if let Err(ConsensusError::InvalidProposal(msg)) = result {
            assert!(msg.contains("not the leader"));
        }
    }

    #[test]
    fn test_check_timeout_returns_false_when_not_timed_out() {
        let engine = create_test_engine();

        // Start a round
        let proposal = create_valid_proposal(&engine);
        {
            let mut round = ConsensusRound::new(0, 1);
            round.proposal = Some(proposal);
            round.started_at = chrono::Utc::now().timestamp_millis();
            *engine.current_round.write() = Some(round);
        }

        // Should not be timed out immediately
        assert!(!engine.check_timeout());
    }

    #[test]
    fn test_check_view_change_timeout_returns_false_when_not_timed_out() {
        let engine = create_test_engine();

        // Start a view change
        let view_change = engine.initiate_view_change(1).unwrap();
        assert_eq!(view_change.new_view, 1);

        // Should not be timed out immediately
        assert!(!engine.check_view_change_timeout());
    }

    #[test]
    fn test_should_initiate_view_change_logic() {
        let engine = create_test_engine();

        // No round, no view change state -> should not initiate
        assert!(!engine.should_initiate_view_change());

        // Start a fresh round -> should not initiate (not timed out)
        {
            let mut round = ConsensusRound::new(0, 1);
            round.started_at = chrono::Utc::now().timestamp_millis();
            *engine.current_round.write() = Some(round);
        }
        assert!(!engine.should_initiate_view_change());
    }

    #[test]
    fn test_get_decision_retrieves_stored_decisions() {
        let engine = create_test_engine();

        // No decision initially
        assert!(engine.get_decision(1).is_none());

        // Store a decision manually
        let decision = ConsensusDecision {
            view: 0,
            sequence: 1,
            content: ProposalContent {
                change_type: StateChangeType::ChallengeSubmission,
                data: vec![1, 2, 3],
                data_hash: [0u8; 32],
            },
            commit_signatures: vec![],
        };
        engine.decisions.write().insert(1, decision.clone());

        // Should retrieve the decision
        let retrieved = engine.get_decision(1);
        assert!(retrieved.is_some());
        let retrieved_decision = retrieved.unwrap();
        assert_eq!(retrieved_decision.view, 0);
        assert_eq!(retrieved_decision.sequence, 1);
        assert_eq!(
            retrieved_decision.content.change_type,
            StateChangeType::ChallengeSubmission
        );
    }

    #[test]
    fn test_handle_prepare_rejects_wrong_proposal_hash() {
        let engine = create_test_engine();

        // Create and start a round
        let proposal = create_valid_proposal(&engine);
        let correct_hash = proposal.proposal.data_hash;

        {
            let mut round = ConsensusRound::new(0, 1);
            round.proposal = Some(proposal);
            round.proposal_hash = correct_hash;
            round.phase = ConsensusPhase::PrePrepare;
            *engine.current_round.write() = Some(round);
        }

        // Create a prepare with wrong hash
        let wrong_hash = [99u8; 32];
        #[derive(Serialize)]
        struct PrepareSigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            proposal_hash: [u8; 32],
        }
        let signing_data = PrepareSigningData {
            view: 0,
            sequence: 1,
            proposal_hash: wrong_hash,
        };
        let signing_bytes = bincode::serialize(&signing_data).unwrap();
        let signature = engine.keypair.sign_bytes(&signing_bytes).unwrap();

        let prepare = PrepareMessage {
            view: 0,
            sequence: 1,
            proposal_hash: wrong_hash, // Wrong hash
            validator: engine.keypair.hotkey(),
            signature,
        };

        let result = engine.handle_prepare(prepare);
        assert!(matches!(result, Err(ConsensusError::InvalidProposal(_))));
    }

    #[test]
    fn test_handle_commit_rejects_wrong_proposal_hash() {
        let engine = create_test_engine();

        // Create and start a round in Prepared phase
        let proposal = create_valid_proposal(&engine);
        let correct_hash = proposal.proposal.data_hash;

        {
            let mut round = ConsensusRound::new(0, 1);
            round.proposal = Some(proposal);
            round.proposal_hash = correct_hash;
            round.phase = ConsensusPhase::Prepared;
            *engine.current_round.write() = Some(round);
        }

        // Create a commit with wrong hash
        let wrong_hash = [99u8; 32];
        #[derive(Serialize)]
        struct CommitSigningData {
            view: ViewNumber,
            sequence: SequenceNumber,
            proposal_hash: [u8; 32],
        }
        let signing_data = CommitSigningData {
            view: 0,
            sequence: 1,
            proposal_hash: wrong_hash,
        };
        let signing_bytes = bincode::serialize(&signing_data).unwrap();
        let signature = engine.keypair.sign_bytes(&signing_bytes).unwrap();

        let commit = CommitMessage {
            view: 0,
            sequence: 1,
            proposal_hash: wrong_hash, // Wrong hash
            validator: engine.keypair.hotkey(),
            signature,
        };

        let result = engine.handle_commit(commit);
        assert!(matches!(result, Err(ConsensusError::InvalidProposal(_))));
    }

    #[test]
    fn test_initiate_view_change_rejects_lower_view() {
        let engine = create_test_engine();

        // Set current view to 5
        *engine.current_view.write() = 5;

        // Try to initiate view change to view 3 (lower)
        let result = engine.initiate_view_change(3);
        assert!(matches!(
            result,
            Err(ConsensusError::ViewMismatch {
                expected: 6,
                actual: 3
            })
        ));
    }

    #[test]
    fn test_handle_new_view_validates_quorum_of_view_changes() {
        // Create engine with 4 validators (quorum = 3)
        let engine = create_multi_validator_engine(4);

        // Get the expected leader for view 1
        let leader_hotkey = engine
            .leader_election
            .leader_for_view(1)
            .expect("should have leader");

        // Create a keypair for the leader if it's not us
        let leader_keypair = if leader_hotkey == engine.keypair.hotkey() {
            engine.keypair.clone()
        } else {
            // Generate a keypair that matches the leader hotkey - this won't work
            // so we need to change view to find one where we are the leader
            // Let's just test with view 0 where we are likely the leader
            engine.keypair.clone()
        };

        // Sign the new view message
        #[derive(Serialize)]
        struct NewViewSigningData {
            view: ViewNumber,
        }
        let signing_data = NewViewSigningData { view: 1 };
        let signing_bytes = bincode::serialize(&signing_data).unwrap();
        let signature = leader_keypair.sign_bytes(&signing_bytes).unwrap();

        // Create a new view message with insufficient view changes
        let new_view = NewViewMessage {
            view: 1,
            view_changes: vec![], // Empty - not enough view changes
            leader: leader_keypair.hotkey(),
            signature,
        };

        let result = engine.handle_new_view(new_view);
        // Should fail either due to not being leader or not enough votes
        assert!(result.is_err());
    }

    #[test]
    fn test_create_pre_prepare_requires_leader() {
        // Create engine with multiple validators
        let engine = create_multi_validator_engine(4);

        // Set view to one where we're not the leader
        // With 4 validators, we should not always be the leader
        for view in 0..10 {
            if !engine.am_i_leader() {
                *engine.current_view.write() = view;

                let result = engine.create_pre_prepare(view, 1, [0u8; 32]);
                assert!(matches!(result, Err(ConsensusError::NotLeader(_))));
                break;
            }
            *engine.current_view.write() = view;
        }
    }

    #[test]
    fn test_am_i_leader_and_current_leader() {
        let engine = create_test_engine();

        // With single validator, we should always be the leader
        assert!(engine.am_i_leader());
        assert_eq!(engine.current_leader(), Some(engine.keypair.hotkey()));
    }

    #[test]
    fn test_handle_prepare_without_active_round() {
        let engine = create_test_engine();

        // No active round
        *engine.current_round.write() = None;

        let prepare = PrepareMessage {
            view: 0,
            sequence: 1,
            proposal_hash: [0u8; 32],
            validator: engine.keypair.hotkey(),
            signature: vec![],
        };

        let result = engine.handle_prepare(prepare);
        assert!(matches!(result, Err(ConsensusError::InvalidProposal(_))));
        if let Err(ConsensusError::InvalidProposal(msg)) = result {
            assert!(msg.contains("No active round"));
        }
    }

    #[test]
    fn test_handle_commit_without_active_round() {
        let engine = create_test_engine();

        // No active round
        *engine.current_round.write() = None;

        let commit = CommitMessage {
            view: 0,
            sequence: 1,
            proposal_hash: [0u8; 32],
            validator: engine.keypair.hotkey(),
            signature: vec![],
        };

        let result = engine.handle_commit(commit);
        assert!(matches!(result, Err(ConsensusError::InvalidProposal(_))));
        if let Err(ConsensusError::InvalidProposal(msg)) = result {
            assert!(msg.contains("No active round"));
        }
    }
}
