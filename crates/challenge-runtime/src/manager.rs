//! Challenge Manager
//!
//! Manages loaded challenges and their databases.

use parking_lot::RwLock;
use platform_challenge_sdk::{
    BoxedChallenge, Challenge, ChallengeContext, ChallengeDatabase, ChallengeId,
};
use platform_core::Hotkey;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};

/// Manages challenges and their resources
pub struct ChallengeManager {
    /// Base path for databases
    data_dir: PathBuf,

    /// Loaded challenges
    challenges: RwLock<HashMap<ChallengeId, Arc<BoxedChallenge>>>,

    /// Challenge databases
    databases: RwLock<HashMap<ChallengeId, Arc<ChallengeDatabase>>>,

    /// Current validator (set when runtime starts)
    validator: RwLock<Option<Hotkey>>,

    /// Current epoch (updated by runtime)
    epoch: RwLock<u64>,
}

impl ChallengeManager {
    /// Create a new manager
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            challenges: RwLock::new(HashMap::new()),
            databases: RwLock::new(HashMap::new()),
            validator: RwLock::new(None),
            epoch: RwLock::new(0),
        }
    }

    /// Set the validator hotkey
    pub fn set_validator(&self, validator: Hotkey) {
        *self.validator.write() = Some(validator);
    }

    /// Set the current epoch
    pub fn set_epoch(&self, epoch: u64) {
        *self.epoch.write() = epoch;
    }

    /// Register a challenge
    pub async fn register<C: Challenge + 'static>(&self, challenge: C) -> Result<(), ManagerError> {
        let id = challenge.id();
        let name = challenge.name().to_string();

        // Check if already registered
        if self.challenges.read().contains_key(&id) {
            return Err(ManagerError::AlreadyRegistered(id));
        }

        // Open database for this challenge
        let db = ChallengeDatabase::open(&self.data_dir, id)
            .map_err(|e| ManagerError::Database(e.to_string()))?;
        let db = Arc::new(db);

        // Run migrations if any
        for migration in challenge.db_migrations() {
            let current_version = db.get_version().unwrap_or(0);
            if migration.version > current_version {
                info!("Running migration {} for challenge {}", migration.name, id);
                for op in &migration.operations {
                    match op {
                        platform_challenge_sdk::DbOperation::CreateTree(name) => {
                            db.open_tree(name)?;
                        }
                        platform_challenge_sdk::DbOperation::DropTree(_name) => {
                            // sled doesn't support dropping trees easily
                        }
                        platform_challenge_sdk::DbOperation::Insert { tree, key, value } => {
                            let tree = db.open_tree(tree)?;
                            tree.insert(key.as_bytes(), value.as_slice())
                                .map_err(|e| ManagerError::Database(e.to_string()))?;
                        }
                    }
                }
                db.set_version(migration.version)?;
            }
        }

        // Create context for initialization
        let validator = self.validator.read().clone().unwrap_or(Hotkey([0u8; 32]));
        let epoch = *self.epoch.read();
        let ctx = ChallengeContext::new(id, validator, epoch, db.clone());

        // Call startup hooks
        let boxed: BoxedChallenge = Box::new(challenge);

        boxed
            .on_startup(&ctx)
            .await
            .map_err(|e| ManagerError::Initialization(e.to_string()))?;

        boxed
            .on_db_ready(&ctx)
            .await
            .map_err(|e| ManagerError::Initialization(e.to_string()))?;

        boxed
            .on_ready(&ctx)
            .await
            .map_err(|e| ManagerError::Initialization(e.to_string()))?;

        // Store challenge and database
        self.challenges.write().insert(id, Arc::new(boxed));
        self.databases.write().insert(id, db);

        info!("Challenge registered: {} ({})", name, id);
        Ok(())
    }

    /// Unregister a challenge
    pub async fn unregister(&self, id: &ChallengeId) -> Result<(), ManagerError> {
        self.challenges.write().remove(id);

        // Flush and close database
        if let Some(db) = self.databases.write().remove(id) {
            db.flush().ok();
        }

        info!("Challenge unregistered: {}", id);
        Ok(())
    }

    /// Get a challenge by ID
    pub fn get_challenge(&self, id: &ChallengeId) -> Option<Arc<BoxedChallenge>> {
        self.challenges.read().get(id).cloned()
    }

    /// Get database for a challenge
    pub fn get_database(&self, id: &ChallengeId) -> Option<Arc<ChallengeDatabase>> {
        self.databases.read().get(id).cloned()
    }

    /// Get context for a challenge
    pub fn get_context(&self, id: &ChallengeId) -> Option<ChallengeContext> {
        let db = self.get_database(id)?;
        let validator = self.validator.read().clone()?;
        let epoch = *self.epoch.read();

        Some(ChallengeContext::new(*id, validator, epoch, db))
    }

    /// List all registered challenges
    pub fn list_challenges(&self) -> Vec<ChallengeId> {
        self.challenges.read().keys().cloned().collect()
    }

    /// Get challenge count
    pub fn count(&self) -> usize {
        self.challenges.read().len()
    }

    /// Get all challenge metadata
    pub fn get_all_metadata(&self) -> Vec<platform_challenge_sdk::ChallengeMetadata> {
        self.challenges
            .read()
            .values()
            .map(|c| c.metadata())
            .collect()
    }

    /// Check if a challenge is registered
    pub fn is_registered(&self, id: &ChallengeId) -> bool {
        self.challenges.read().contains_key(id)
    }

    /// Flush all databases
    pub fn flush_all(&self) {
        for db in self.databases.read().values() {
            db.flush().ok();
        }
    }
}

/// Manager errors
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("Challenge already registered: {0:?}")]
    AlreadyRegistered(ChallengeId),

    #[error("Challenge not found: {0:?}")]
    NotFound(ChallengeId),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Initialization error: {0}")]
    Initialization(String),
}

impl From<platform_challenge_sdk::ChallengeError> for ManagerError {
    fn from(err: platform_challenge_sdk::ChallengeError) -> Self {
        ManagerError::Database(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use platform_challenge_sdk::{AgentInfo, EvaluationResult, WeightAssignment};
    use tempfile::tempdir;

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
            _payload: serde_json::Value,
        ) -> platform_challenge_sdk::Result<EvaluationResult> {
            Ok(EvaluationResult::new(
                ctx.job_id(),
                agent.hash.clone(),
                0.75,
            ))
        }

        async fn calculate_weights(
            &self,
            _ctx: &ChallengeContext,
        ) -> platform_challenge_sdk::Result<Vec<WeightAssignment>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn test_register_challenge() {
        let dir = tempdir().unwrap();
        let manager = ChallengeManager::new(dir.path().to_path_buf());

        let challenge = TestChallenge {
            id: ChallengeId::new(),
        };
        let id = challenge.id();

        manager.register(challenge).await.unwrap();

        assert!(manager.is_registered(&id));
        assert_eq!(manager.count(), 1);
    }

    #[tokio::test]
    async fn test_unregister_challenge() {
        let dir = tempdir().unwrap();
        let manager = ChallengeManager::new(dir.path().to_path_buf());

        let challenge = TestChallenge {
            id: ChallengeId::new(),
        };
        let id = challenge.id();

        manager.register(challenge).await.unwrap();
        manager.unregister(&id).await.unwrap();

        assert!(!manager.is_registered(&id));
    }
}
