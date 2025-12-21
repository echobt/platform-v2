//! Container backend abstraction
//!
//! Provides a unified interface for container management that can use:
//! - Direct Docker (for local development/testing)
//! - SecureContainerClient via broker (for production validators)
//!
//! The backend is selected based on the CONTAINER_BROKER_SOCKET environment variable.
//! If set, uses the secure broker. Otherwise, uses direct Docker.

use crate::{ChallengeContainerConfig, ChallengeInstance, ContainerStatus};
use async_trait::async_trait;
use secure_container_runtime::{
    ContainerConfigBuilder, ContainerState, NetworkMode, SecureContainerClient,
};
use tracing::{info, warn};

/// Container backend trait for managing challenge containers
#[async_trait]
pub trait ContainerBackend: Send + Sync {
    /// Start a challenge container
    async fn start_challenge(
        &self,
        config: &ChallengeContainerConfig,
    ) -> anyhow::Result<ChallengeInstance>;

    /// Stop a container
    async fn stop_container(&self, container_id: &str) -> anyhow::Result<()>;

    /// Remove a container
    async fn remove_container(&self, container_id: &str) -> anyhow::Result<()>;

    /// Check if a container is running
    async fn is_container_running(&self, container_id: &str) -> anyhow::Result<bool>;

    /// Pull an image
    async fn pull_image(&self, image: &str) -> anyhow::Result<()>;

    /// Get container logs
    async fn get_logs(&self, container_id: &str, tail: usize) -> anyhow::Result<String>;

    /// Cleanup all containers for a challenge
    async fn cleanup_challenge(&self, challenge_id: &str) -> anyhow::Result<usize>;

    /// List containers for a challenge
    async fn list_challenge_containers(&self, challenge_id: &str) -> anyhow::Result<Vec<String>>;
}

/// Secure container backend using the broker
pub struct SecureBackend {
    client: SecureContainerClient,
    validator_id: String,
}

impl SecureBackend {
    /// Create a new secure backend
    pub fn new(socket_path: &str, validator_id: &str) -> Self {
        Self {
            client: SecureContainerClient::new(socket_path),
            validator_id: validator_id.to_string(),
        }
    }

    /// Create from environment
    pub fn from_env() -> Option<Self> {
        let socket = std::env::var("CONTAINER_BROKER_SOCKET").ok()?;
        let validator_id =
            std::env::var("VALIDATOR_HOTKEY").unwrap_or_else(|_| "unknown".to_string());
        Some(Self::new(&socket, &validator_id))
    }
}

#[async_trait]
impl ContainerBackend for SecureBackend {
    async fn start_challenge(
        &self,
        config: &ChallengeContainerConfig,
    ) -> anyhow::Result<ChallengeInstance> {
        info!(
            challenge = %config.name,
            image = %config.docker_image,
            "Starting challenge via secure broker"
        );

        // Build container config
        let container_config = ContainerConfigBuilder::new(
            &config.docker_image,
            &config.challenge_id.to_string(),
            &self.validator_id,
        )
        .memory((config.memory_mb * 1024 * 1024) as i64)
        .cpu(config.cpu_cores)
        .network_mode(NetworkMode::Isolated)
        .expose(8080)
        .env("CHALLENGE_ID", &config.challenge_id.to_string())
        .env("MECHANISM_ID", &config.mechanism_id.to_string())
        .build();

        // Create and start container
        let (container_id, _container_name) = self
            .client
            .create_container(container_config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create container: {}", e))?;

        self.client
            .start_container(&container_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to start container: {}", e))?;

        // Get endpoint
        let endpoint = self
            .client
            .get_endpoint(&container_id, 8080)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get endpoint: {}", e))?;

        info!(
            container_id = %container_id,
            endpoint = %endpoint,
            "Challenge container started via broker"
        );

        Ok(ChallengeInstance {
            challenge_id: config.challenge_id,
            container_id,
            image: config.docker_image.clone(),
            endpoint,
            started_at: chrono::Utc::now(),
            status: ContainerStatus::Running,
        })
    }

    async fn stop_container(&self, container_id: &str) -> anyhow::Result<()> {
        self.client
            .stop_container(container_id, 30)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to stop container: {}", e))
    }

    async fn remove_container(&self, container_id: &str) -> anyhow::Result<()> {
        self.client
            .remove_container(container_id, true)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to remove container: {}", e))
    }

    async fn is_container_running(&self, container_id: &str) -> anyhow::Result<bool> {
        match self.client.inspect(container_id).await {
            Ok(info) => Ok(info.state == ContainerState::Running),
            Err(_) => Ok(false),
        }
    }

    async fn pull_image(&self, image: &str) -> anyhow::Result<()> {
        self.client
            .pull_image(image)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to pull image: {}", e))
    }

    async fn get_logs(&self, container_id: &str, tail: usize) -> anyhow::Result<String> {
        self.client
            .logs(container_id, tail)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get logs: {}", e))
    }

    async fn cleanup_challenge(&self, challenge_id: &str) -> anyhow::Result<usize> {
        let result = self
            .client
            .cleanup_challenge(challenge_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to cleanup: {}", e))?;

        if !result.success() {
            warn!(errors = ?result.errors, "Some cleanup errors occurred");
        }

        Ok(result.removed)
    }

    async fn list_challenge_containers(&self, challenge_id: &str) -> anyhow::Result<Vec<String>> {
        let containers = self
            .client
            .list_by_challenge(challenge_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list containers: {}", e))?;

        Ok(containers.into_iter().map(|c| c.id).collect())
    }
}

/// Direct Docker backend (for local development)
pub struct DirectDockerBackend {
    docker: crate::docker::DockerClient,
}

impl DirectDockerBackend {
    /// Create a new direct Docker backend
    pub async fn new() -> anyhow::Result<Self> {
        let docker = crate::docker::DockerClient::connect().await?;
        Ok(Self { docker })
    }
}

#[async_trait]
impl ContainerBackend for DirectDockerBackend {
    async fn start_challenge(
        &self,
        config: &ChallengeContainerConfig,
    ) -> anyhow::Result<ChallengeInstance> {
        self.docker.start_challenge(config).await
    }

    async fn stop_container(&self, container_id: &str) -> anyhow::Result<()> {
        self.docker.stop_container(container_id).await
    }

    async fn remove_container(&self, container_id: &str) -> anyhow::Result<()> {
        self.docker.remove_container(container_id).await
    }

    async fn is_container_running(&self, container_id: &str) -> anyhow::Result<bool> {
        self.docker.is_container_running(container_id).await
    }

    async fn pull_image(&self, image: &str) -> anyhow::Result<()> {
        self.docker.pull_image(image).await
    }

    async fn get_logs(&self, container_id: &str, tail: usize) -> anyhow::Result<String> {
        self.docker.get_logs(container_id, tail).await
    }

    async fn cleanup_challenge(&self, challenge_id: &str) -> anyhow::Result<usize> {
        let containers = self.docker.list_challenge_containers().await?;
        let mut removed = 0;

        for container_id in containers {
            if container_id.contains(&challenge_id.to_string()) {
                let _ = self.docker.stop_container(&container_id).await;
                if self.docker.remove_container(&container_id).await.is_ok() {
                    removed += 1;
                }
            }
        }

        Ok(removed)
    }

    async fn list_challenge_containers(&self, _challenge_id: &str) -> anyhow::Result<Vec<String>> {
        self.docker.list_challenge_containers().await
    }
}

/// Create the appropriate backend based on environment
pub async fn create_backend() -> anyhow::Result<Box<dyn ContainerBackend>> {
    // Check if broker socket is configured
    if let Some(secure) = SecureBackend::from_env() {
        info!("Using secure container broker");
        return Ok(Box::new(secure));
    }

    // Fall back to direct Docker
    info!("Using direct Docker (local development mode)");
    let direct = DirectDockerBackend::new().await?;
    Ok(Box::new(direct))
}

/// Check if running in secure mode (broker available)
pub fn is_secure_mode() -> bool {
    std::env::var("CONTAINER_BROKER_SOCKET").is_ok()
}
