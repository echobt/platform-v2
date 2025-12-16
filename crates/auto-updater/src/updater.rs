//! Auto-updater - pulls new images and triggers restart

use crate::{UpdateRequirement, Version, VersionWatcher};
use bollard::image::CreateImageOptions;
use bollard::Docker;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

/// Auto-updater that pulls new Docker images and signals for restart
pub struct AutoUpdater {
    docker: Docker,
    watcher: Arc<VersionWatcher>,
    restart_sender: broadcast::Sender<RestartSignal>,
}

impl AutoUpdater {
    pub async fn new(watcher: Arc<VersionWatcher>) -> anyhow::Result<Self> {
        let docker = Docker::connect_with_local_defaults()?;
        docker.ping().await?;

        let (restart_sender, _) = broadcast::channel(16);

        Ok(Self {
            docker,
            watcher,
            restart_sender,
        })
    }

    /// Subscribe to restart signals
    pub fn subscribe_restart(&self) -> broadcast::Receiver<RestartSignal> {
        self.restart_sender.subscribe()
    }

    /// Start the auto-update loop
    pub async fn start(self: Arc<Self>) {
        let mut update_rx = self.watcher.subscribe();

        tokio::spawn(async move {
            loop {
                match update_rx.recv().await {
                    Ok(requirement) => {
                        if let Err(e) = self.handle_update(requirement).await {
                            error!(error = %e, "Failed to handle update");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "Update receiver lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("Update channel closed, stopping auto-updater");
                        break;
                    }
                }
            }
        });
    }

    /// Handle a version update
    async fn handle_update(&self, requirement: UpdateRequirement) -> anyhow::Result<()> {
        let current = Version::current();

        if !current.needs_update(&requirement.min_version) {
            debug!("Already on required version or newer");
            return Ok(());
        }

        info!(
            current = %current,
            required = %requirement.min_version,
            image = %requirement.docker_image,
            "Pulling new version"
        );

        // Pull the new image
        self.pull_image(&requirement.docker_image).await?;

        info!(image = %requirement.docker_image, "New image pulled successfully");

        // Signal for restart
        let signal = RestartSignal {
            new_version: requirement.recommended_version,
            image: requirement.docker_image,
            mandatory: requirement.mandatory,
        };

        let _ = self.restart_sender.send(signal);

        // If mandatory, exit to trigger restart
        if requirement.mandatory {
            info!("Mandatory update - initiating graceful shutdown");
            // Give time for cleanup
            tokio::time::sleep(Duration::from_secs(5)).await;
            // Exit with code 0 - systemd/Docker will restart with new image
            std::process::exit(0);
        }

        Ok(())
    }

    /// Pull a Docker image
    async fn pull_image(&self, image: &str) -> anyhow::Result<()> {
        let options = CreateImageOptions {
            from_image: image,
            ..Default::default()
        };

        let mut stream = self.docker.create_image(Some(options), None, None);

        while let Some(result) = stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(status) = info.status {
                        debug!(status = %status, "Pull progress");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Pull warning");
                }
            }
        }

        Ok(())
    }

    /// Manually trigger an update check and pull
    pub async fn check_and_update(&self) -> anyhow::Result<UpdateResult> {
        let Some(requirement) = self.watcher.get_requirement() else {
            return Ok(UpdateResult::NoUpdateAvailable);
        };

        let current = Version::current();
        if !current.needs_update(&requirement.min_version) {
            return Ok(UpdateResult::AlreadyUpToDate);
        }

        self.pull_image(&requirement.docker_image).await?;

        Ok(UpdateResult::Updated {
            from: current,
            to: requirement.recommended_version,
            image: requirement.docker_image,
        })
    }

    /// Get the image name for the current version
    pub fn current_image(&self) -> String {
        let version = Version::current();
        format!("cortexlm/platform-validator:{}", version)
    }
}

/// Signal to restart the validator
#[derive(Clone, Debug)]
pub struct RestartSignal {
    pub new_version: Version,
    pub image: String,
    pub mandatory: bool,
}

/// Result of an update check
#[derive(Debug)]
pub enum UpdateResult {
    NoUpdateAvailable,
    AlreadyUpToDate,
    Updated {
        from: Version,
        to: Version,
        image: String,
    },
}

/// Graceful shutdown handler for updates
pub struct GracefulShutdown {
    shutdown_sender: broadcast::Sender<()>,
}

impl GracefulShutdown {
    pub fn new() -> Self {
        let (shutdown_sender, _) = broadcast::channel(1);
        Self { shutdown_sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.shutdown_sender.subscribe()
    }

    pub fn trigger(&self) {
        let _ = self.shutdown_sender.send(());
    }
}

impl Default for GracefulShutdown {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_restart_signal() {
        let signal = RestartSignal {
            new_version: Version::new(0, 2, 0),
            image: "cortexlm/validator:0.2.0".to_string(),
            mandatory: true,
        };

        assert!(signal.mandatory);
        assert_eq!(signal.new_version.to_string(), "0.2.0");
    }

    #[test]
    fn test_graceful_shutdown() {
        let shutdown = GracefulShutdown::new();
        let mut rx = shutdown.subscribe();

        shutdown.trigger();

        assert!(rx.try_recv().is_ok());
    }
}
