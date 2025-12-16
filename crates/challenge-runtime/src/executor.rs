//! Challenge Executor
//!
//! Handles the actual execution of challenge evaluations with timeout and error handling.

use platform_challenge_sdk::{
    AgentInfo, Challenge, ChallengeContext, EvaluationJob, EvaluationResult,
};
use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

/// Executor configuration
#[derive(Clone, Debug)]
pub struct ExecutorConfig {
    /// Default timeout for evaluations
    pub default_timeout: Duration,
    /// Maximum memory per evaluation (bytes)
    pub max_memory: Option<u64>,
    /// Enable detailed logging
    pub verbose: bool,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(300),
            max_memory: Some(512 * 1024 * 1024), // 512 MB
            verbose: false,
        }
    }
}

/// Challenge executor
pub struct ChallengeExecutor {
    config: ExecutorConfig,
}

impl ChallengeExecutor {
    /// Create a new executor
    pub fn new(config: ExecutorConfig) -> Self {
        Self { config }
    }

    /// Execute an evaluation job
    pub async fn execute(
        &self,
        challenge: &dyn Challenge,
        ctx: &ChallengeContext,
        job: &EvaluationJob,
    ) -> Result<EvaluationResult, ExecutorError> {
        let start = std::time::Instant::now();

        if self.config.verbose {
            info!(
                "Starting evaluation: challenge={}, job={}, agent={}",
                ctx.challenge_id(),
                job.id,
                job.agent_hash
            );
        }

        // Create agent info
        let agent = AgentInfo::new(job.agent_hash.clone());

        // Execute with timeout
        let evaluation_future = challenge.handle_job(ctx, job);

        let result = match timeout(self.config.default_timeout, evaluation_future).await {
            Ok(Ok(result)) => {
                let elapsed = start.elapsed();
                debug!(
                    "Evaluation completed: job={}, score={:.4}, time={}ms",
                    job.id,
                    result.score,
                    elapsed.as_millis()
                );

                Ok(EvaluationResult {
                    execution_time_ms: elapsed.as_millis() as u64,
                    ..result
                })
            }
            Ok(Err(e)) => {
                error!("Evaluation failed: job={}, error={}", job.id, e);
                Err(ExecutorError::EvaluationFailed(e.to_string()))
            }
            Err(_) => {
                warn!(
                    "Evaluation timeout: job={}, timeout={}s",
                    job.id,
                    self.config.default_timeout.as_secs()
                );
                Err(ExecutorError::Timeout)
            }
        };

        result
    }

    /// Execute a batch of jobs
    pub async fn execute_batch(
        &self,
        challenge: &dyn Challenge,
        ctx: &ChallengeContext,
        jobs: Vec<EvaluationJob>,
    ) -> Vec<(uuid::Uuid, Result<EvaluationResult, ExecutorError>)> {
        let mut results = Vec::with_capacity(jobs.len());

        for job in jobs {
            let job_ctx = ctx.clone().with_job_id(job.id);
            let result = self.execute(challenge, &job_ctx, &job).await;
            results.push((job.id, result));
        }

        results
    }

    /// Validate an agent before evaluation
    pub async fn validate_agent(
        &self,
        challenge: &dyn Challenge,
        ctx: &ChallengeContext,
        agent: &AgentInfo,
    ) -> Result<bool, ExecutorError> {
        challenge
            .validate_agent(ctx, agent)
            .await
            .map_err(|e| ExecutorError::ValidationFailed(e.to_string()))
    }
}

impl Default for ChallengeExecutor {
    fn default() -> Self {
        Self::new(ExecutorConfig::default())
    }
}

/// Executor errors
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("Evaluation failed: {0}")]
    EvaluationFailed(String),

    #[error("Validation failed: {0}")]
    ValidationFailed(String),

    #[error("Evaluation timeout")]
    Timeout,

    #[error("Memory limit exceeded")]
    MemoryLimitExceeded,

    #[error("Challenge not found")]
    ChallengeNotFound,
}

/// Execution metrics
#[derive(Clone, Debug, Default)]
pub struct ExecutionMetrics {
    pub total_executions: u64,
    pub successful_executions: u64,
    pub failed_executions: u64,
    pub timeout_executions: u64,
    pub total_execution_time_ms: u64,
    pub average_execution_time_ms: f64,
}

impl ExecutionMetrics {
    /// Update metrics with a result
    pub fn record(&mut self, success: bool, timeout: bool, execution_time_ms: u64) {
        self.total_executions += 1;
        self.total_execution_time_ms += execution_time_ms;

        if success {
            self.successful_executions += 1;
        } else if timeout {
            self.timeout_executions += 1;
        } else {
            self.failed_executions += 1;
        }

        self.average_execution_time_ms =
            self.total_execution_time_ms as f64 / self.total_executions as f64;
    }

    /// Get success rate
    pub fn success_rate(&self) -> f64 {
        if self.total_executions == 0 {
            0.0
        } else {
            self.successful_executions as f64 / self.total_executions as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use platform_challenge_sdk::{ChallengeDatabase, ChallengeId, WeightAssignment};

    struct FastChallenge;

    #[async_trait]
    impl Challenge for FastChallenge {
        fn id(&self) -> ChallengeId {
            ChallengeId::new()
        }

        fn name(&self) -> &str {
            "Fast Challenge"
        }

        fn emission_weight(&self) -> f64 {
            1.0
        }

        async fn evaluate(
            &self,
            ctx: &ChallengeContext,
            agent: &AgentInfo,
            _payload: serde_json::Value,
        ) -> platform_challenge_sdk::Result<EvaluationResult> {
            Ok(EvaluationResult::new(ctx.job_id(), agent.hash.clone(), 0.9))
        }

        async fn calculate_weights(
            &self,
            _ctx: &ChallengeContext,
        ) -> platform_challenge_sdk::Result<Vec<WeightAssignment>> {
            Ok(vec![])
        }
    }

    struct SlowChallenge;

    #[async_trait]
    impl Challenge for SlowChallenge {
        fn id(&self) -> ChallengeId {
            ChallengeId::new()
        }

        fn name(&self) -> &str {
            "Slow Challenge"
        }

        fn emission_weight(&self) -> f64 {
            1.0
        }

        async fn evaluate(
            &self,
            ctx: &ChallengeContext,
            agent: &AgentInfo,
            _payload: serde_json::Value,
        ) -> platform_challenge_sdk::Result<EvaluationResult> {
            // Sleep for too long
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok(EvaluationResult::new(ctx.job_id(), agent.hash.clone(), 0.5))
        }

        async fn calculate_weights(
            &self,
            _ctx: &ChallengeContext,
        ) -> platform_challenge_sdk::Result<Vec<WeightAssignment>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn test_execution_metrics() {
        let mut metrics = ExecutionMetrics::default();

        metrics.record(true, false, 100);
        metrics.record(true, false, 200);
        metrics.record(false, false, 50);

        assert_eq!(metrics.total_executions, 3);
        assert_eq!(metrics.successful_executions, 2);
        assert_eq!(metrics.failed_executions, 1);
        assert!((metrics.success_rate() - 0.666).abs() < 0.01);
    }
}
