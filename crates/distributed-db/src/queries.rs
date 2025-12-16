//! High-level query API for the distributed database
//!
//! Provides convenient methods for common operations

use crate::{DistributedDB, Filter, Query, QueryResult};
use platform_core::{ChallengeContainerConfig, Hotkey};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Challenge data stored in DB
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredChallenge {
    pub id: String,
    pub name: String,
    pub docker_image: String,
    pub mechanism_id: u8,
    pub emission_weight: f64,
    pub created_at: u64,
    pub created_by: String,
    pub status: ChallengeStatus,
}

/// Challenge status
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChallengeStatus {
    Active,
    Paused,
    Deprecated,
}

/// Agent submission data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAgent {
    pub hash: String,
    pub challenge_id: String,
    pub submitter: String,
    pub submitted_at: u64,
    pub code_hash: String,
    pub status: AgentStatus,
}

/// Agent status
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentStatus {
    Pending,
    Evaluating,
    Evaluated,
    Failed,
}

/// Evaluation result data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvaluation {
    pub id: String,
    pub agent_hash: String,
    pub challenge_id: String,
    pub validator: String,
    pub score: f64,
    pub metrics: serde_json::Value,
    pub evaluated_at: u64,
    pub block_number: u64,
}

/// Weight submission data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredWeight {
    pub id: String,
    pub challenge_id: String,
    pub validator: String,
    pub weights: Vec<(u16, u16)>, // (uid, weight)
    pub block_number: u64,
    pub submitted_at: u64,
}

impl DistributedDB {
    // ==================== Challenge Operations ====================

    /// Store a challenge configuration
    pub fn store_challenge(&self, config: &ChallengeContainerConfig) -> anyhow::Result<()> {
        let challenge = StoredChallenge {
            id: config.challenge_id.to_string(),
            name: config.name.clone(),
            docker_image: config.docker_image.clone(),
            mechanism_id: config.mechanism_id,
            emission_weight: config.emission_weight,
            created_at: chrono::Utc::now().timestamp_millis() as u64,
            created_by: "sudo".to_string(),
            status: ChallengeStatus::Active,
        };

        let key = config.challenge_id.to_string();
        let value = serde_json::to_vec(&challenge)?;
        self.put("challenges", key.as_bytes(), &value)
    }

    /// Get a challenge by ID
    pub fn get_challenge(&self, challenge_id: &str) -> anyhow::Result<Option<StoredChallenge>> {
        self.get_typed("challenges", challenge_id.as_bytes())
    }

    /// List all active challenges
    pub fn list_challenges(&self) -> anyhow::Result<Vec<StoredChallenge>> {
        let query = Query::new("challenges");
        let result = self.query(query)?;
        self.parse_results(result)
    }

    /// List challenges by mechanism
    pub fn list_challenges_by_mechanism(
        &self,
        mechanism_id: u8,
    ) -> anyhow::Result<Vec<StoredChallenge>> {
        let query =
            Query::new("challenges").filter(Filter::eq("mechanism_id", mechanism_id.to_string()));
        let result = self.query(query)?;
        self.parse_results(result)
    }

    // ==================== Agent Operations ====================

    /// Store an agent submission
    pub fn store_agent(&self, agent: &StoredAgent) -> anyhow::Result<()> {
        let key = agent.hash.as_bytes();
        let value = serde_json::to_vec(agent)?;
        self.put("agents", key, &value)
    }

    /// Get an agent by hash
    pub fn get_agent(&self, hash: &str) -> anyhow::Result<Option<StoredAgent>> {
        self.get_typed("agents", hash.as_bytes())
    }

    /// List agents for a challenge
    pub fn list_agents_for_challenge(
        &self,
        challenge_id: &str,
    ) -> anyhow::Result<Vec<StoredAgent>> {
        let query = Query::new("agents").filter(Filter::eq("challenge_id", challenge_id));
        let result = self.query(query)?;
        self.parse_results(result)
    }

    /// List agents by submitter
    pub fn list_agents_by_submitter(&self, submitter: &Hotkey) -> anyhow::Result<Vec<StoredAgent>> {
        let query =
            Query::new("agents").filter(Filter::eq("submitter", hex::encode(submitter.as_bytes())));
        let result = self.query(query)?;
        self.parse_results(result)
    }

    /// Update agent status
    pub fn update_agent_status(&self, hash: &str, status: AgentStatus) -> anyhow::Result<()> {
        if let Some(mut agent) = self.get_agent(hash)? {
            agent.status = status;
            self.store_agent(&agent)?;
        }
        Ok(())
    }

    // ==================== Evaluation Operations ====================

    /// Store an evaluation result
    pub fn store_evaluation(&self, eval: &StoredEvaluation) -> anyhow::Result<()> {
        let key = eval.id.as_bytes();
        let value = serde_json::to_vec(eval)?;
        self.put("evaluations", key, &value)
    }

    /// Get evaluations for an agent
    pub fn get_evaluations_for_agent(
        &self,
        agent_hash: &str,
    ) -> anyhow::Result<Vec<StoredEvaluation>> {
        let query = Query::new("evaluations").filter(Filter::eq("agent_hash", agent_hash));
        let result = self.query(query)?;
        self.parse_results(result)
    }

    /// Get evaluations by validator
    pub fn get_evaluations_by_validator(
        &self,
        validator: &Hotkey,
    ) -> anyhow::Result<Vec<StoredEvaluation>> {
        let query = Query::new("evaluations")
            .filter(Filter::eq("validator", hex::encode(validator.as_bytes())));
        let result = self.query(query)?;
        self.parse_results(result)
    }

    /// Get top scores for a challenge
    pub fn get_top_scores(
        &self,
        challenge_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<StoredEvaluation>> {
        let query = Query::new("evaluations")
            .filter(Filter::eq("challenge_id", challenge_id))
            .order_by("score", true) // descending
            .limit(limit);
        let result = self.query(query)?;
        self.parse_results(result)
    }

    // ==================== Weight Operations ====================

    /// Store weight submission
    pub fn store_weights(&self, weights: &StoredWeight) -> anyhow::Result<()> {
        let key = weights.id.as_bytes();
        let value = serde_json::to_vec(weights)?;
        self.put("weights", key, &value)
    }

    /// Get weights for a block
    pub fn get_weights_at_block(&self, block: u64) -> anyhow::Result<Vec<StoredWeight>> {
        let query = Query::new("weights").filter(Filter::eq("block_number", block.to_string()));
        let result = self.query(query)?;
        self.parse_results(result)
    }

    /// Get latest weights for challenge
    pub fn get_latest_weights(&self, challenge_id: &str) -> anyhow::Result<Vec<StoredWeight>> {
        let query = Query::new("weights")
            .filter(Filter::eq("challenge_id", challenge_id))
            .order_by("block_number", true) // descending
            .limit(100);
        let result = self.query(query)?;
        self.parse_results(result)
    }

    // ==================== Helper Methods ====================

    /// Get typed value from collection
    fn get_typed<T: DeserializeOwned>(
        &self,
        collection: &str,
        key: &[u8],
    ) -> anyhow::Result<Option<T>> {
        match self.get(collection, key)? {
            Some(value) => {
                let parsed = serde_json::from_slice(&value)?;
                Ok(Some(parsed))
            }
            None => Ok(None),
        }
    }

    /// Parse query results into typed vec
    fn parse_results<T: DeserializeOwned>(&self, result: QueryResult) -> anyhow::Result<Vec<T>> {
        let mut items = Vec::new();
        for entry in result.entries {
            if let Ok(item) = serde_json::from_slice::<T>(&entry.value) {
                items.push(item);
            }
        }
        Ok(items)
    }

    // ==================== Aggregation Operations ====================

    /// Get challenge statistics
    pub fn get_challenge_stats(&self, challenge_id: &str) -> anyhow::Result<ChallengeStats> {
        let agents = self.list_agents_for_challenge(challenge_id)?;
        let evaluations: Vec<StoredEvaluation> = {
            let query = Query::new("evaluations").filter(Filter::eq("challenge_id", challenge_id));
            let result = self.query(query)?;
            self.parse_results(result)?
        };

        let total_agents = agents.len();
        let evaluated_agents = agents
            .iter()
            .filter(|a| a.status == AgentStatus::Evaluated)
            .count();
        let avg_score = if !evaluations.is_empty() {
            evaluations.iter().map(|e| e.score).sum::<f64>() / evaluations.len() as f64
        } else {
            0.0
        };
        let max_score = evaluations.iter().map(|e| e.score).fold(0.0f64, f64::max);

        Ok(ChallengeStats {
            challenge_id: challenge_id.to_string(),
            total_agents,
            evaluated_agents,
            total_evaluations: evaluations.len(),
            avg_score,
            max_score,
        })
    }

    /// Get global stats
    pub fn get_global_stats(&self) -> anyhow::Result<GlobalStats> {
        let challenges = self.list_challenges()?;
        let agents: Vec<StoredAgent> = {
            let query = Query::new("agents");
            let result = self.query(query)?;
            self.parse_results(result)?
        };
        let evaluations: Vec<StoredEvaluation> = {
            let query = Query::new("evaluations");
            let result = self.query(query)?;
            self.parse_results(result)?
        };

        Ok(GlobalStats {
            total_challenges: challenges.len(),
            active_challenges: challenges
                .iter()
                .filter(|c| c.status == ChallengeStatus::Active)
                .count(),
            total_agents: agents.len(),
            total_evaluations: evaluations.len(),
            state_root: hex::encode(self.state_root()),
        })
    }
}

/// Challenge statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeStats {
    pub challenge_id: String,
    pub total_agents: usize,
    pub evaluated_agents: usize,
    pub total_evaluations: usize,
    pub avg_score: f64,
    pub max_score: f64,
}

/// Global statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalStats {
    pub total_challenges: usize,
    pub active_challenges: usize,
    pub total_agents: usize,
    pub total_evaluations: usize,
    pub state_root: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_challenge_operations() {
        let dir = tempdir().unwrap();
        let validator = Hotkey::from_bytes(&[1u8; 32]).unwrap();
        let db = DistributedDB::open(dir.path(), validator).unwrap();

        // Store challenge
        let config = ChallengeContainerConfig::new("Test Challenge", "test:latest", 0, 1.0);
        db.store_challenge(&config).unwrap();

        // Get challenge
        let challenge = db.get_challenge(&config.challenge_id.to_string()).unwrap();
        assert!(challenge.is_some());
        assert_eq!(challenge.unwrap().name, "Test Challenge");

        // List challenges
        let challenges = db.list_challenges().unwrap();
        assert_eq!(challenges.len(), 1);
    }

    #[test]
    fn test_agent_operations() {
        let dir = tempdir().unwrap();
        let validator = Hotkey::from_bytes(&[1u8; 32]).unwrap();
        let db = DistributedDB::open(dir.path(), validator).unwrap();

        let agent = StoredAgent {
            hash: "abc123".to_string(),
            challenge_id: "test-challenge".to_string(),
            submitter: "submitter123".to_string(),
            submitted_at: 1000,
            code_hash: "code123".to_string(),
            status: AgentStatus::Pending,
        };

        db.store_agent(&agent).unwrap();

        let retrieved = db.get_agent("abc123").unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().challenge_id, "test-challenge");
    }
}
