#![allow(dead_code, unused_variables, unused_imports)]
//! Challenge Runtime for Mini-Chain Validators
//!
//! Executes challenges, manages job queues, and coordinates with the epoch system.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      WASM Sandbox                                │
//! │  ┌─────────────────────────────────────────────────────────┐   │
//! │  │                   Challenge Code                         │   │
//! │  │  - allowed_data_keys()                                   │   │
//! │  │  - verify_data()                                         │   │
//! │  │  - calculate_weights()                                   │   │
//! │  │  - evaluate_agent()                                      │   │
//! │  └──────────────────────┬──────────────────────────────────┘   │
//! │                         │ Host Function Calls                   │
//! └─────────────────────────┼───────────────────────────────────────┘
//!                           ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                    Host Runtime (Rust)                          │
//! │  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────────────┐   │
//! │  │  Docker  │ │ LLM Proxy│ │  Epoch   │ │ Bittensor Client │   │
//! │  │  Runner  │ │          │ │   Sync   │ │                  │   │
//! │  └──────────┘ └──────────┘ └──────────┘ └──────────────────┘   │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

pub mod executor;
pub mod manager;
pub mod runtime;
pub mod scheduler;

// Host-side components for WASM challenges
pub mod anti_cheat_weights;
pub mod docker_runner;
pub mod epoch_sync;
pub mod host_functions;
pub mod llm_proxy;
pub mod miner_verifier;
pub mod p2p_broadcast;
pub mod wasm_challenge;
pub mod weight_calculator;

#[cfg(test)]
mod integration_test;

pub use executor::*;
pub use manager::*;
pub use runtime::*;
pub use scheduler::*;

// Host functions and components
pub use anti_cheat_weights::{
    AgentRanking, AntiCheatConfig, AntiCheatWeightCalculator, CalculationStats, FlagReason,
    FlaggedValidator, ValidatorScore, WeightCalculationResult as AntiCheatResult,
};
pub use docker_runner::{DockerRunner, DockerRunnerConfig};
pub use epoch_sync::{EpochEvent, EpochInfo, EpochPhase, EpochSync, EpochSyncConfig};
pub use host_functions::{
    ExecuteAgentRequest, ExecuteAgentResult, HostFunctions, HostResult, LLMCallRecord,
    LLMCallRequest, LLMCallResponse, LLMUsage, MinerInfo, TestResult, ValidatorInfo,
    VerifyTaskRequest, VerifyTaskResult, WeightSubmission,
};
pub use llm_proxy::{LLMProxy, LLMProxyConfig, ProxyState};
pub use miner_verifier::{
    AgentMinerAssociation, AgentMinerRegistry, MinerVerifier, MinerVerifierConfig,
    VerificationResult,
};
pub use p2p_broadcast::{
    ChallengeMessage, ConsensusInfo, P2PBroadcast, P2PBroadcastConfig, ReceivedResult, WireMessage,
};
pub use wasm_challenge::{
    AgentEvaluationResult, ChallengeState, ConsensusScore, DataKeySpec, TaskScore,
    TerminalBenchWasm,
};
pub use weight_calculator::{
    CalculatedWeight, CommitRevealState, MinerScore, WeightCalculationResult, WeightCalculator,
    WeightCalculatorConfig, MAX_WEIGHT,
};
