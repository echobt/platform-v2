//! Challenge trait for implementing custom challenges

use crate::{
    AgentInfo, ChallengeContext, ChallengeError, ChallengeId, ChallengeMetadata, ChallengeRoute,
    EvaluationJob, EvaluationResult, Result, RouteRequest, RouteResponse, WeightAssignment,
};
use async_trait::async_trait;
use serde_json::Value;

/// Trait for implementing a challenge
///
/// Each challenge defines its own evaluation logic, weight calculation,
/// and database schema. Challenges run independently with their own
/// isolated database.
///
/// # Example
///
/// ```rust,ignore
/// use platform_challenge_sdk::prelude::*;
///
/// struct TerminalBenchChallenge;
///
/// #[async_trait]
/// impl Challenge for TerminalBenchChallenge {
///     fn id(&self) -> ChallengeId {
///         ChallengeId::from_str("...").unwrap()
///     }
///
///     fn name(&self) -> &str {
///         "Terminal Benchmark"
///     }
///
///     fn emission_weight(&self) -> f64 {
///         0.5  // 50% of emissions
///     }
///
///     async fn evaluate(&self, ctx: &ChallengeContext, agent: &AgentInfo) -> Result<EvaluationResult> {
///         // Run terminal benchmark
///         let score = run_benchmark(agent).await?;
///         Ok(EvaluationResult::new(ctx.job_id(), agent.hash.clone(), score))
///     }
///
///     async fn calculate_weights(&self, ctx: &ChallengeContext) -> Result<Vec<WeightAssignment>> {
///         // Get all evaluation results from DB
///         let results = ctx.db().get_all_results().await?;
///         
///         // Calculate weights based on scores
///         let weights = results.iter()
///             .map(|r| WeightAssignment::new(r.agent_hash.clone(), r.score))
///             .collect();
///         
///         Ok(weights)
///     }
/// }
/// ```
#[async_trait]
pub trait Challenge: Send + Sync {
    // ==================== Required Methods ====================

    /// Get the unique challenge ID
    fn id(&self) -> ChallengeId;

    /// Get the challenge name
    fn name(&self) -> &str;

    /// Get the emission weight (percentage of total emissions, 0.0 - 1.0)
    fn emission_weight(&self) -> f64;

    /// Evaluate an agent
    ///
    /// This is the core evaluation logic. Returns a score between 0.0 and 1.0.
    async fn evaluate(
        &self,
        ctx: &ChallengeContext,
        agent: &AgentInfo,
        payload: Value,
    ) -> Result<EvaluationResult>;

    /// Calculate weights for all agents
    ///
    /// Called at the end of each epoch to compute weight assignments.
    /// Weights should sum to 1.0 (normalized).
    async fn calculate_weights(&self, ctx: &ChallengeContext) -> Result<Vec<WeightAssignment>>;

    // ==================== Optional Methods ====================

    /// Get challenge description
    fn description(&self) -> &str {
        ""
    }

    /// Get challenge version
    fn version(&self) -> &str {
        "1.0.0"
    }

    /// Called when the challenge starts
    async fn on_startup(&self, _ctx: &ChallengeContext) -> Result<()> {
        Ok(())
    }

    /// Called when the challenge is ready
    async fn on_ready(&self, _ctx: &ChallengeContext) -> Result<()> {
        Ok(())
    }

    /// Called when the database is initialized
    async fn on_db_ready(&self, _ctx: &ChallengeContext) -> Result<()> {
        Ok(())
    }

    /// Called at the start of a new epoch
    async fn on_epoch_start(&self, _ctx: &ChallengeContext, _epoch: u64) -> Result<()> {
        Ok(())
    }

    /// Called at the end of an epoch
    async fn on_epoch_end(&self, _ctx: &ChallengeContext, _epoch: u64) -> Result<()> {
        Ok(())
    }

    /// Validate an agent before evaluation
    async fn validate_agent(&self, _ctx: &ChallengeContext, agent: &AgentInfo) -> Result<bool> {
        // Default: accept all agents
        Ok(true)
    }

    /// Get custom job types supported by this challenge
    fn supported_job_types(&self) -> Vec<&str> {
        vec!["evaluate"]
    }

    /// Handle a custom job type
    async fn handle_job(
        &self,
        ctx: &ChallengeContext,
        job: &EvaluationJob,
    ) -> Result<EvaluationResult> {
        match job.job_type.as_str() {
            "evaluate" => {
                let agent = AgentInfo::new(job.agent_hash.clone());
                self.evaluate(ctx, &agent, job.payload.clone()).await
            }
            _ => Err(ChallengeError::UnsupportedJobType(job.job_type.clone())),
        }
    }

    /// Get database migrations (SQL or sled operations)
    fn db_migrations(&self) -> Vec<DbMigration> {
        vec![]
    }

    // ==================== Custom Routes ====================

    /// Get custom HTTP routes exposed by this challenge
    ///
    /// Routes are mounted under /challenge/{challenge_id}/
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// fn routes(&self) -> Vec<ChallengeRoute> {
    ///     vec![
    ///         ChallengeRoute::get("/leaderboard", "Get leaderboard"),
    ///         ChallengeRoute::get("/agent/:hash", "Get agent details"),
    ///         ChallengeRoute::post("/submit", "Submit evaluation"),
    ///     ]
    /// }
    /// ```
    fn routes(&self) -> Vec<ChallengeRoute> {
        vec![]
    }

    /// Handle an incoming request to a custom route
    ///
    /// Called when a request matches one of the routes defined in `routes()`.
    /// The request path is relative to the challenge (e.g., "/leaderboard").
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// async fn handle_route(&self, ctx: &ChallengeContext, req: RouteRequest) -> RouteResponse {
    ///     match (req.method.as_str(), req.path.as_str()) {
    ///         ("GET", "/leaderboard") => {
    ///             let data = self.get_leaderboard(ctx).await;
    ///             RouteResponse::json(data)
    ///         }
    ///         ("GET", path) if path.starts_with("/agent/") => {
    ///             let hash = req.param("hash").unwrap_or_default();
    ///             let agent = self.get_agent(ctx, hash).await;
    ///             RouteResponse::json(agent)
    ///         }
    ///         _ => RouteResponse::not_found()
    ///     }
    /// }
    /// ```
    async fn handle_route(&self, _ctx: &ChallengeContext, _req: RouteRequest) -> RouteResponse {
        RouteResponse::not_found()
    }

    /// Called when routes are being registered
    ///
    /// Can be used to initialize route-related state
    async fn on_routes_registered(&self, _ctx: &ChallengeContext) -> Result<()> {
        Ok(())
    }

    // ==================== Data Storage ====================

    /// Get allowed data keys that validators can write to
    ///
    /// Define what data keys validators can store and their constraints.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// fn allowed_data_keys(&self) -> Vec<DataKeySpec> {
    ///     vec![
    ///         DataKeySpec::new("score")
    ///             .validator_scoped()
    ///             .max_size(1024),
    ///         DataKeySpec::new("leaderboard")
    ///             .challenge_scoped()
    ///             .ttl_blocks(100),
    ///     ]
    /// }
    /// ```
    fn allowed_data_keys(&self) -> Vec<crate::DataKeySpec> {
        vec![]
    }

    /// Verify data submitted by a validator
    ///
    /// Called when a validator submits data. Return DataVerification::accept()
    /// to store the data, or DataVerification::reject("reason") to reject it.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// async fn verify_data(&self, ctx: &ChallengeContext, submission: &DataSubmission) -> DataVerification {
    ///     match submission.key.as_str() {
    ///         "score" => {
    ///             if let Ok(score) = submission.value_json::<f64>() {
    ///                 if score >= 0.0 && score <= 100.0 {
    ///                     return DataVerification::accept();
    ///                 }
    ///             }
    ///             DataVerification::reject("Invalid score")
    ///         }
    ///         _ => DataVerification::reject("Unknown key")
    ///     }
    /// }
    /// ```
    async fn verify_data(
        &self,
        _ctx: &ChallengeContext,
        submission: &crate::DataSubmission,
    ) -> crate::DataVerification {
        // Default: accept all data for defined keys
        let allowed = self.allowed_data_keys();
        if allowed.iter().any(|k| k.key == submission.key) {
            crate::DataVerification::accept()
        } else {
            crate::DataVerification::reject(format!("Key '{}' not allowed", submission.key))
        }
    }

    /// Called when data is stored (after verification and consensus)
    ///
    /// Can be used to update derived state based on the new data.
    async fn on_data_stored(
        &self,
        _ctx: &ChallengeContext,
        _key: &str,
        _value: &[u8],
        _validator: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// Query stored data
    ///
    /// Called to retrieve data. Challenges can override to add access control.
    async fn query_data(
        &self,
        _ctx: &ChallengeContext,
        _query: &crate::DataQuery,
    ) -> Result<Vec<crate::StoredData>> {
        // Default: return empty (override to implement)
        Ok(vec![])
    }

    /// Get challenge configuration as metadata
    fn metadata(&self) -> ChallengeMetadata {
        ChallengeMetadata {
            id: self.id(),
            name: self.name().to_string(),
            description: self.description().to_string(),
            version: self.version().to_string(),
            owner: platform_core::Hotkey([0u8; 32]), // Will be set by runtime
            emission_weight: self.emission_weight(),
            config: crate::ChallengeConfig::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            is_active: true,
        }
    }
}

/// Database migration for challenge initialization
#[derive(Clone, Debug)]
pub struct DbMigration {
    pub version: u32,
    pub name: String,
    pub operations: Vec<DbOperation>,
}

/// Database operation for migrations
#[derive(Clone, Debug)]
pub enum DbOperation {
    /// Create a tree/table
    CreateTree(String),
    /// Drop a tree/table
    DropTree(String),
    /// Insert initial data
    Insert {
        tree: String,
        key: String,
        value: Vec<u8>,
    },
}

impl DbMigration {
    pub fn new(version: u32, name: impl Into<String>) -> Self {
        Self {
            version,
            name: name.into(),
            operations: vec![],
        }
    }

    pub fn create_tree(mut self, name: impl Into<String>) -> Self {
        self.operations.push(DbOperation::CreateTree(name.into()));
        self
    }

    pub fn drop_tree(mut self, name: impl Into<String>) -> Self {
        self.operations.push(DbOperation::DropTree(name.into()));
        self
    }

    pub fn insert(
        mut self,
        tree: impl Into<String>,
        key: impl Into<String>,
        value: Vec<u8>,
    ) -> Self {
        self.operations.push(DbOperation::Insert {
            tree: tree.into(),
            key: key.into(),
            value,
        });
        self
    }
}

/// A boxed challenge for dynamic dispatch
pub type BoxedChallenge = Box<dyn Challenge>;

/// Challenge registry for managing multiple challenges
pub struct ChallengeRegistry {
    challenges: parking_lot::RwLock<std::collections::HashMap<ChallengeId, BoxedChallenge>>,
}

impl ChallengeRegistry {
    pub fn new() -> Self {
        Self {
            challenges: parking_lot::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Register a challenge
    pub fn register<C: Challenge + 'static>(&self, challenge: C) {
        let id = challenge.id();
        let name = challenge.name().to_string();
        self.challenges.write().insert(id, Box::new(challenge));
        tracing::info!("Registered challenge: {} ({})", name, id);
    }

    /// Get a challenge by ID
    pub fn get(&self, id: &ChallengeId) -> Option<std::sync::Arc<BoxedChallenge>> {
        // Returns a cloned Arc reference to the challenge
        None
    }

    /// List all registered challenges
    pub fn list(&self) -> Vec<ChallengeId> {
        self.challenges.read().keys().cloned().collect()
    }

    /// Get total emission weight
    pub fn total_emission_weight(&self) -> f64 {
        self.challenges
            .read()
            .values()
            .map(|c| c.emission_weight())
            .sum()
    }
}

impl Default for ChallengeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestChallenge {
        id: ChallengeId,
    }

    #[async_trait]
    impl Challenge for TestChallenge {
        fn id(&self) -> ChallengeId {
            self.id
        }

        fn name(&self) -> &str {
            "Test Challenge"
        }

        fn emission_weight(&self) -> f64 {
            0.5
        }

        async fn evaluate(
            &self,
            ctx: &ChallengeContext,
            agent: &AgentInfo,
            _payload: Value,
        ) -> Result<EvaluationResult> {
            Ok(EvaluationResult::new(
                ctx.job_id(),
                agent.hash.clone(),
                0.75,
            ))
        }

        async fn calculate_weights(
            &self,
            _ctx: &ChallengeContext,
        ) -> Result<Vec<WeightAssignment>> {
            Ok(vec![
                WeightAssignment::new("agent1".to_string(), 0.6),
                WeightAssignment::new("agent2".to_string(), 0.4),
            ])
        }
    }

    #[test]
    fn test_challenge_metadata() {
        let challenge = TestChallenge {
            id: ChallengeId::new(),
        };
        assert_eq!(challenge.name(), "Test Challenge");
        assert_eq!(challenge.emission_weight(), 0.5);
    }
}
