//! Container lifecycle management

use crate::{ChallengeContainerConfig, ChallengeInstance, ContainerStatus, DockerClient};
use parking_lot::RwLock;
use platform_core::ChallengeId;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info};

/// Manages the lifecycle of challenge containers
pub struct LifecycleManager {
    docker: DockerClient,
    challenges: Arc<RwLock<HashMap<ChallengeId, ChallengeInstance>>>,
    configs: Arc<RwLock<HashMap<ChallengeId, ChallengeContainerConfig>>>,
}

impl LifecycleManager {
    pub fn new(
        docker: DockerClient,
        challenges: Arc<RwLock<HashMap<ChallengeId, ChallengeInstance>>>,
    ) -> Self {
        Self {
            docker,
            challenges,
            configs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a challenge configuration (will start container)
    pub async fn add(&mut self, config: ChallengeContainerConfig) -> anyhow::Result<()> {
        let challenge_id = config.challenge_id;

        // Pull image first
        self.docker.pull_image(&config.docker_image).await?;

        // Start container
        let instance = self.docker.start_challenge(&config).await?;

        // Store config and instance
        self.configs.write().insert(challenge_id, config);
        self.challenges.write().insert(challenge_id, instance);

        info!(challenge_id = %challenge_id, "Challenge added and started");
        Ok(())
    }

    /// Update a challenge (new image version)
    pub async fn update(&mut self, config: ChallengeContainerConfig) -> anyhow::Result<()> {
        let challenge_id = config.challenge_id;

        // Stop existing container - get container_id first, then release lock before await
        let container_id = self
            .challenges
            .read()
            .get(&challenge_id)
            .map(|i| i.container_id.clone());
        if let Some(container_id) = container_id {
            self.docker.stop_container(&container_id).await?;
            self.docker.remove_container(&container_id).await?;
        }

        // Pull new image
        self.docker.pull_image(&config.docker_image).await?;

        // Start new container
        let instance = self.docker.start_challenge(&config).await?;

        // Update config and instance
        self.configs.write().insert(challenge_id, config);
        self.challenges.write().insert(challenge_id, instance);

        info!(challenge_id = %challenge_id, "Challenge updated");
        Ok(())
    }

    /// Remove a challenge
    pub async fn remove(&mut self, challenge_id: ChallengeId) -> anyhow::Result<()> {
        // Remove instance and get container_id before await
        let instance = self.challenges.write().remove(&challenge_id);
        if let Some(instance) = instance {
            self.docker.stop_container(&instance.container_id).await?;
            self.docker.remove_container(&instance.container_id).await?;
        }

        self.configs.write().remove(&challenge_id);

        info!(challenge_id = %challenge_id, "Challenge removed");
        Ok(())
    }

    /// Restart a challenge (same config)
    pub async fn restart(&mut self, challenge_id: ChallengeId) -> anyhow::Result<()> {
        let config = self
            .configs
            .read()
            .get(&challenge_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Challenge config not found"))?;

        self.update(config).await
    }

    /// Restart unhealthy challenges
    pub async fn restart_unhealthy(&mut self) -> Vec<(ChallengeId, anyhow::Result<()>)> {
        let unhealthy: Vec<_> = self
            .challenges
            .read()
            .iter()
            .filter(|(_, instance)| instance.status == ContainerStatus::Unhealthy)
            .map(|(id, _)| *id)
            .collect();

        let mut results = Vec::new();

        for challenge_id in unhealthy {
            let result = self.restart(challenge_id).await;
            if let Err(ref e) = result {
                error!(challenge_id = %challenge_id, error = %e, "Failed to restart unhealthy challenge");
            }
            results.push((challenge_id, result));
        }

        results
    }

    /// Sync with target state (add missing, remove extra, update changed)
    pub async fn sync(
        &mut self,
        target_configs: Vec<ChallengeContainerConfig>,
    ) -> anyhow::Result<SyncResult> {
        let mut result = SyncResult::default();

        let target_ids: std::collections::HashSet<_> =
            target_configs.iter().map(|c| c.challenge_id).collect();

        let current_ids: std::collections::HashSet<_> =
            self.configs.read().keys().cloned().collect();

        // Remove challenges not in target
        for id in current_ids.difference(&target_ids) {
            match self.remove(*id).await {
                Ok(_) => result.removed.push(*id),
                Err(e) => result.errors.push((*id, e.to_string())),
            }
        }

        // Add or update challenges
        for config in target_configs {
            let needs_update = self
                .configs
                .read()
                .get(&config.challenge_id)
                .map(|existing| existing.docker_image != config.docker_image)
                .unwrap_or(true);

            if needs_update {
                let is_new = !current_ids.contains(&config.challenge_id);

                match self.update(config.clone()).await {
                    Ok(_) => {
                        if is_new {
                            result.added.push(config.challenge_id);
                        } else {
                            result.updated.push(config.challenge_id);
                        }
                    }
                    Err(e) => {
                        result.errors.push((config.challenge_id, e.to_string()));
                    }
                }
            } else {
                result.unchanged.push(config.challenge_id);
            }
        }

        info!(
            added = result.added.len(),
            updated = result.updated.len(),
            removed = result.removed.len(),
            unchanged = result.unchanged.len(),
            errors = result.errors.len(),
            "Sync completed"
        );

        Ok(result)
    }

    /// Stop all challenges (for shutdown)
    pub async fn stop_all(&mut self) -> Vec<(ChallengeId, anyhow::Result<()>)> {
        let ids: Vec<_> = self.challenges.read().keys().cloned().collect();
        let mut results = Vec::new();

        for id in ids {
            let result = self.remove(id).await;
            results.push((id, result));
        }

        results
    }
}

/// Result of a sync operation
#[derive(Default, Debug)]
pub struct SyncResult {
    pub added: Vec<ChallengeId>,
    pub updated: Vec<ChallengeId>,
    pub removed: Vec<ChallengeId>,
    pub unchanged: Vec<ChallengeId>,
    pub errors: Vec<(ChallengeId, String)>,
}

impl SyncResult {
    pub fn is_success(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn total_changes(&self) -> usize {
        self.added.len() + self.updated.len() + self.removed.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_result_default() {
        let result = SyncResult::default();
        assert!(result.is_success());
        assert_eq!(result.total_changes(), 0);
    }

    #[test]
    fn test_sync_result_with_changes() {
        let mut result = SyncResult::default();
        result.added.push(ChallengeId::new());
        result.updated.push(ChallengeId::new());

        assert!(result.is_success());
        assert_eq!(result.total_changes(), 2);
    }

    #[test]
    fn test_sync_result_with_errors() {
        let mut result = SyncResult::default();
        result
            .errors
            .push((ChallengeId::new(), "test error".to_string()));

        assert!(!result.is_success());
    }
}
