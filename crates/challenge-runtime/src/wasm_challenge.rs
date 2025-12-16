//! WASM Challenge Implementation
//!
//! Implements the Challenge trait for term-challenge.
//! This code runs inside the WASM sandbox and uses host functions
//! to interact with the system.

use crate::epoch_sync::{EpochPhase, EpochSync, EpochSyncConfig};
use crate::host_functions::*;
use crate::miner_verifier::{AgentMinerRegistry, MinerVerifier, MinerVerifierConfig};
use crate::weight_calculator::{MinerScore, WeightCalculator, WeightCalculatorConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Challenge state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeState {
    /// Current epoch
    pub epoch: u64,
    /// Current phase
    pub phase: String,
    /// Evaluation results this epoch
    pub results: HashMap<String, AgentEvaluationResult>,
    /// Consensus results
    pub consensus: HashMap<String, ConsensusScore>,
    /// Current weights (pending submission)
    pub pending_weights: Option<Vec<(u16, u16)>>,
    /// Weight commit state
    pub commit_hash: Option<[u8; 32]>,
    /// Whether weights revealed this epoch
    pub weights_revealed: bool,
}

/// Agent evaluation result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvaluationResult {
    /// Agent hash
    pub agent_hash: String,
    /// Miner hotkey
    pub miner_hotkey: String,
    /// Miner UID
    pub miner_uid: u16,
    /// Final score (0.0 - 1.0)
    pub score: f64,
    /// Task scores
    pub task_scores: Vec<TaskScore>,
    /// Total cost in USD
    pub total_cost_usd: f64,
    /// Evaluation timestamp
    pub timestamp: u64,
    /// Result hash for verification
    pub result_hash: String,
}

/// Task score
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskScore {
    pub task_id: String,
    pub score: f64,
    pub passed: bool,
    pub cost_usd: f64,
    pub execution_time_ms: u64,
}

/// Consensus score (after 2/3 validators agree)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusScore {
    pub agent_hash: String,
    pub score: f64,
    pub validator_count: u32,
    pub result_hash: String,
}

/// Data keys for this challenge
pub mod data_keys {
    pub const EVALUATION_RESULT: &str = "term:eval_result";
    pub const CONSENSUS_SCORE: &str = "term:consensus";
    pub const LEADERBOARD: &str = "term:leaderboard";
    pub const WEIGHTS: &str = "term:weights";
}

/// Terminal Bench Challenge - WASM Implementation
pub struct TerminalBenchWasm<H: HostFunctions> {
    /// Host functions
    host: Arc<H>,
    /// Challenge state
    state: Arc<parking_lot::RwLock<ChallengeState>>,
    /// Weight calculator
    weight_calc: WeightCalculator,
    /// Epoch sync
    epoch_sync: EpochSync,
    /// Miner verifier
    miner_verifier: MinerVerifier,
    /// Agent-miner registry
    agent_registry: AgentMinerRegistry,
    /// Mechanism ID
    mechanism_id: u8,
    /// Validators required for consensus (2/3)
    consensus_threshold: f64,
}

impl<H: HostFunctions> TerminalBenchWasm<H> {
    pub fn new(host: Arc<H>, mechanism_id: u8) -> Self {
        let weight_config = WeightCalculatorConfig {
            mechanism_id,
            ..Default::default()
        };

        Self {
            host,
            state: Arc::new(parking_lot::RwLock::new(ChallengeState {
                epoch: 0,
                phase: "WAITING".to_string(),
                results: HashMap::new(),
                consensus: HashMap::new(),
                pending_weights: None,
                commit_hash: None,
                weights_revealed: false,
            })),
            weight_calc: WeightCalculator::new(weight_config),
            epoch_sync: EpochSync::new(EpochSyncConfig::default()),
            miner_verifier: MinerVerifier::new(MinerVerifierConfig::default()),
            agent_registry: AgentMinerRegistry::new(),
            mechanism_id,
            consensus_threshold: 0.67, // 2/3
        }
    }

    // ==================== Challenge Interface ====================

    /// Called at start of each epoch
    pub async fn on_epoch_start(&self, epoch: u64) {
        info!("Epoch {} started", epoch);

        let mut state = self.state.write();
        state.epoch = epoch;
        state.phase = "EVALUATION".to_string();
        state.results.clear();
        state.consensus.clear();
        state.pending_weights = None;
        state.commit_hash = None;
        state.weights_revealed = false;
    }

    /// Evaluate an agent
    pub async fn evaluate_agent(&self, agent_hash: &str) -> Result<AgentEvaluationResult, String> {
        info!("Evaluating agent {}", agent_hash);

        // Verify we're in evaluation phase
        if self.epoch_sync.get_phase() != EpochPhase::Evaluation {
            return Err("Not in evaluation phase".to_string());
        }

        // Get miner for this agent
        let miner = self
            .agent_registry
            .get_miner(agent_hash)
            .ok_or_else(|| format!("Agent {} not registered", agent_hash))?;

        // Verify miner is still valid
        let verification = self.miner_verifier.verify(&miner.miner_hotkey).await;
        if !verification.is_valid() {
            return Err(format!(
                "Miner {} verification failed: {:?}",
                miner.miner_hotkey, verification
            ));
        }

        // Get tasks to run
        let tasks = self.get_tasks_for_evaluation();

        let mut task_scores = Vec::new();
        let mut total_cost = 0.0;

        // Set LLM cost limit
        self.host.llm_set_limit(10.0); // $10 max per evaluation

        for task in tasks {
            // Execute agent on task
            let exec_result = self
                .host
                .execute_agent(ExecuteAgentRequest {
                    agent_hash: agent_hash.to_string(),
                    task_id: task.id.clone(),
                    instruction: task.instruction.clone(),
                    files: task.files.clone(),
                    env_vars: HashMap::new(),
                    timeout_secs: 300,
                    memory_limit_mb: 4096,
                    cpu_limit: 2.0,
                    allow_network: true,
                    allowed_hosts: vec![
                        "api.openai.com".to_string(),
                        "api.anthropic.com".to_string(),
                    ],
                })
                .await;

            let (task_score, exec_cost, exec_time) = match exec_result {
                HostResult {
                    success: true,
                    data: Some(result),
                    ..
                } => {
                    // Verify task completion
                    let verify_result = self
                        .host
                        .verify_task(VerifyTaskRequest {
                            task_id: task.id.clone(),
                            test_script: task.test_script.clone(),
                            working_dir: result.output_files.clone(),
                            timeout_secs: 60,
                        })
                        .await;

                    let (score, passed) = match verify_result {
                        HostResult {
                            success: true,
                            data: Some(v),
                            ..
                        } => (v.score, v.passed),
                        _ => (0.0, false),
                    };

                    (score, result.total_cost_usd, result.execution_time_ms)
                }
                _ => {
                    warn!("Agent execution failed for task {}", task.id);
                    (0.0, 0.0, 0)
                }
            };

            total_cost += exec_cost;

            task_scores.push(TaskScore {
                task_id: task.id,
                score: task_score,
                passed: task_score >= 0.5,
                cost_usd: exec_cost,
                execution_time_ms: exec_time,
            });
        }

        // Calculate final score
        let final_score = if task_scores.is_empty() {
            0.0
        } else {
            task_scores.iter().map(|t| t.score).sum::<f64>() / task_scores.len() as f64
        };

        // Create result
        let result = AgentEvaluationResult {
            agent_hash: agent_hash.to_string(),
            miner_hotkey: miner.miner_hotkey.clone(),
            miner_uid: miner.miner_uid,
            score: final_score,
            task_scores,
            total_cost_usd: total_cost,
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
            result_hash: self.compute_result_hash(&agent_hash, final_score),
        };

        // Store result
        self.state
            .write()
            .results
            .insert(agent_hash.to_string(), result.clone());

        // Broadcast result to other validators
        let result_bytes = serde_json::to_vec(&result)
            .map_err(|e| format!("Failed to serialize result: {}", e))?;

        let _ = self
            .host
            .broadcast_result(BroadcastResultRequest {
                agent_hash: agent_hash.to_string(),
                result: result_bytes,
                signature: vec![], // Validator signature pending
            })
            .await;

        info!(
            "Evaluation complete for {}: score={:.4}",
            agent_hash, final_score
        );

        Ok(result)
    }

    /// Receive evaluation result from another validator
    pub fn receive_result(&self, agent_hash: &str, validator: &str, score: f64, result_hash: &str) {
        debug!(
            "Received result from {} for agent {}: score={:.4}",
            validator, agent_hash, score
        );

        // Check for consensus
        let validator_results = self.host.get_validator_results(agent_hash);

        if let HostResult {
            success: true,
            data: Some(results),
            ..
        } = validator_results
        {
            let validators = match self.host.get_validators() {
                HostResult {
                    success: true,
                    data: Some(v),
                    ..
                } => v,
                _ => return,
            };

            let total_validators = validators.len();
            let required = (total_validators as f64 * self.consensus_threshold).ceil() as usize;

            // Check if we have consensus (scores within 10% tolerance)
            let matching_results: Vec<_> = results
                .iter()
                .filter(|r| (r.score - score).abs() / score.max(0.01) < 0.10)
                .collect();

            if matching_results.len() >= required {
                let avg_score = matching_results.iter().map(|r| r.score).sum::<f64>()
                    / matching_results.len() as f64;

                let consensus = ConsensusScore {
                    agent_hash: agent_hash.to_string(),
                    score: avg_score,
                    validator_count: matching_results.len() as u32,
                    result_hash: result_hash.to_string(),
                };

                self.state
                    .write()
                    .consensus
                    .insert(agent_hash.to_string(), consensus);
                info!(
                    "Consensus reached for agent {}: score={:.4}",
                    agent_hash, avg_score
                );
            }
        }
    }

    /// Calculate weights from consensus scores
    pub fn calculate_weights(&self) -> Vec<(u16, u16)> {
        let state = self.state.read();

        if state.consensus.is_empty() {
            warn!("No consensus results to calculate weights from");
            return vec![];
        }

        let scores: Vec<MinerScore> = state
            .consensus
            .values()
            .filter_map(|c| {
                state.results.get(&c.agent_hash).map(|r| MinerScore {
                    uid: r.miner_uid,
                    hotkey: r.miner_hotkey.clone(),
                    score: c.score,
                    agent_hash: c.agent_hash.clone(),
                    tasks_completed: r.task_scores.len() as u32,
                    cost_usd: r.total_cost_usd,
                })
            })
            .collect();

        let result = self.weight_calc.calculate(&scores, state.epoch);
        let weights = self.weight_calc.to_bittensor_format(&result);

        info!(
            "Calculated weights for {} miners (epoch {})",
            weights.len(),
            state.epoch
        );

        weights
    }

    /// Commit weights (during commit phase)
    pub async fn commit_weights(&self) -> Result<(), String> {
        if self.epoch_sync.get_phase() != EpochPhase::Commit {
            return Err("Not in commit phase".to_string());
        }

        let weights = self.calculate_weights();
        if weights.is_empty() {
            return Err("No weights to commit".to_string());
        }

        // Create commit
        let mut salt = [0u8; 32];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut salt);

        let commit_hash = self.weight_calc.create_commit(&weights, &salt);

        // Chain commit handled by epoch manager
        // self.host.submit_weight_commit(commit_hash).await?;

        let mut state = self.state.write();
        state.pending_weights = Some(weights);
        state.commit_hash = Some(commit_hash);

        info!("Weights committed: hash={:?}", hex::encode(commit_hash));

        Ok(())
    }

    /// Reveal weights (during reveal phase)
    pub async fn reveal_weights(&self) -> Result<(), String> {
        if self.epoch_sync.get_phase() != EpochPhase::Reveal {
            return Err("Not in reveal phase".to_string());
        }

        let state = self.state.read();

        let weights = state
            .pending_weights
            .clone()
            .ok_or_else(|| "No weights to reveal".to_string())?;

        drop(state);

        // Submit weights to chain
        let submission = WeightSubmission {
            weights: weights.clone(),
            mechanism_id: self.mechanism_id,
        };

        let result = self.host.submit_weights(submission).await;

        match result {
            HostResult { success: true, .. } => {
                self.state.write().weights_revealed = true;
                info!("Weights revealed successfully");
                Ok(())
            }
            HostResult { error: Some(e), .. } => Err(format!("Failed to reveal weights: {}", e)),
            _ => Err("Failed to reveal weights".to_string()),
        }
    }

    /// Data keys allowed for this challenge
    pub fn allowed_data_keys(&self) -> Vec<DataKeySpec> {
        vec![
            DataKeySpec {
                key_pattern: data_keys::EVALUATION_RESULT.to_string(),
                max_size: 1024 * 1024, // 1MB
                ttl_secs: 3600 * 24,   // 1 day
                validators_only: true,
            },
            DataKeySpec {
                key_pattern: data_keys::CONSENSUS_SCORE.to_string(),
                max_size: 1024,          // 1KB
                ttl_secs: 3600 * 24 * 7, // 1 week
                validators_only: true,
            },
            DataKeySpec {
                key_pattern: data_keys::LEADERBOARD.to_string(),
                max_size: 1024 * 100, // 100KB
                ttl_secs: 3600 * 24,  // 1 day
                validators_only: true,
            },
            DataKeySpec {
                key_pattern: data_keys::WEIGHTS.to_string(),
                max_size: 1024 * 10, // 10KB
                ttl_secs: 3600 * 24, // 1 day
                validators_only: true,
            },
        ]
    }

    /// Verify data submission
    pub fn verify_data(&self, key: &str, value: &[u8], submitter: &str) -> bool {
        // Check key pattern
        let valid_key = key.starts_with("term:");

        // Check size limits
        let specs = self.allowed_data_keys();
        let size_ok = specs
            .iter()
            .find(|s| key.starts_with(&s.key_pattern))
            .map(|s| value.len() <= s.max_size as usize)
            .unwrap_or(false);

        // Verify submitter is a validator
        let is_validator = match self.host.get_validators() {
            HostResult {
                success: true,
                data: Some(validators),
                ..
            } => validators.iter().any(|v| v.hotkey == submitter),
            _ => false,
        };

        valid_key && size_ok && is_validator
    }

    // ==================== Helper Methods ====================

    /// Get tasks for evaluation
    fn get_tasks_for_evaluation(&self) -> Vec<EvaluationTask> {
        // Return predefined tasks from the task database
        vec![
            EvaluationTask {
                id: "task_001".to_string(),
                instruction: "Create a Python function that sorts a list of integers".to_string(),
                files: HashMap::from([
                    ("main.py".to_string(), b"# Write your solution here\n".to_vec()),
                ]),
                test_script: r#"
#!/bin/bash
python3 -c "from main import sort_list; assert sort_list([3,1,2]) == [1,2,3]" && echo "PASS" || echo "FAIL"
"#.to_string(),
            },
        ]
    }

    /// Compute hash of result for verification
    fn compute_result_hash(&self, agent_hash: &str, score: f64) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(agent_hash.as_bytes());
        hasher.update(score.to_le_bytes());
        hasher.update(self.state.read().epoch.to_le_bytes());
        hex::encode(hasher.finalize())
    }
}

/// Data key specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataKeySpec {
    pub key_pattern: String,
    pub max_size: u64,
    pub ttl_secs: u64,
    pub validators_only: bool,
}

/// Task for evaluation
#[derive(Debug, Clone)]
struct EvaluationTask {
    id: String,
    instruction: String,
    files: HashMap<String, Vec<u8>>,
    test_script: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test host functions
    struct TestHost;

    impl HostFunctions for TestHost {
        async fn execute_agent(
            &self,
            _request: ExecuteAgentRequest,
        ) -> HostResult<ExecuteAgentResult> {
            HostResult::ok(ExecuteAgentResult {
                execution_id: "test".to_string(),
                exit_code: 0,
                stdout: "Success".to_string(),
                stderr: String::new(),
                execution_time_ms: 100,
                memory_used_mb: 100,
                output_files: HashMap::new(),
                llm_calls: vec![],
                total_cost_usd: 0.01,
                timed_out: false,
                resource_limited: false,
            })
        }

        async fn verify_task(&self, _request: VerifyTaskRequest) -> HostResult<VerifyTaskResult> {
            HostResult::ok(VerifyTaskResult {
                passed: true,
                score: 0.8,
                output: "PASS".to_string(),
                tests_passed: 1,
                tests_failed: 0,
                test_details: vec![],
            })
        }

        async fn llm_call(&self, _request: LLMCallRequest) -> HostResult<LLMCallResponse> {
            HostResult::err("Not implemented in test")
        }

        fn llm_get_usage(&self) -> HostResult<LLMUsage> {
            HostResult::ok(LLMUsage {
                total_input_tokens: 0,
                total_output_tokens: 0,
                total_cost_usd: 0.0,
                call_count: 0,
                cost_limit_usd: 10.0,
                remaining_usd: 10.0,
            })
        }

        fn llm_set_limit(&self, _limit_usd: f64) {}

        fn storage_get(&self, _key: &str) -> HostResult<Option<StorageEntry>> {
            HostResult::ok(None)
        }

        fn storage_set(&self, _key: &str, _value: &[u8]) -> HostResult<()> {
            HostResult::ok(())
        }

        fn storage_delete(&self, _key: &str) -> HostResult<()> {
            HostResult::ok(())
        }

        fn storage_list(&self, _prefix: &str) -> HostResult<Vec<String>> {
            HostResult::ok(vec![])
        }

        async fn broadcast_result(&self, _request: BroadcastResultRequest) -> HostResult<()> {
            HostResult::ok(())
        }

        fn get_validator_results(&self, _agent_hash: &str) -> HostResult<Vec<ValidatorResult>> {
            HostResult::ok(vec![])
        }

        fn get_epoch(&self) -> u64 {
            1
        }
        fn get_block(&self) -> u64 {
            100
        }

        fn get_validators(&self) -> HostResult<Vec<ValidatorInfo>> {
            HostResult::ok(vec![])
        }

        fn get_miner_info(&self, _hotkey: &str) -> HostResult<Option<MinerInfo>> {
            HostResult::ok(Some(MinerInfo {
                hotkey: "test".to_string(),
                coldkey: "cold".to_string(),
                stake: 1000_000_000_000,
                is_registered: true,
                uid: Some(1),
            }))
        }

        fn is_miner_registered(&self, _hotkey: &str) -> bool {
            true
        }
        fn get_miner_stake(&self, _hotkey: &str) -> u64 {
            1000_000_000_000
        }

        async fn submit_weights(&self, _submission: WeightSubmission) -> HostResult<()> {
            HostResult::ok(())
        }

        fn log(&self, level: &str, message: &str) {
            println!("[{}] {}", level, message);
        }
    }

    #[test]
    fn test_data_keys() {
        let host = Arc::new(TestHost);
        let challenge = TerminalBenchWasm::new(host, 0);

        let keys = challenge.allowed_data_keys();
        assert!(!keys.is_empty());
        assert!(keys.iter().any(|k| k.key_pattern == "term:eval_result"));
    }
}
