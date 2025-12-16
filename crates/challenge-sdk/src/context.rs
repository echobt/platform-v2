//! Challenge execution context

use crate::{ChallengeDatabase, ChallengeId, EpochInfo};
use platform_core::Hotkey;
use std::sync::Arc;

/// Context provided during challenge execution
///
/// Each challenge gets its own isolated context with:
/// - Independent database
/// - Current epoch information
/// - Validator information
/// - Job metadata
#[derive(Clone)]
pub struct ChallengeContext {
    /// Challenge ID
    challenge_id: ChallengeId,

    /// Current job ID (if executing a job)
    job_id: uuid::Uuid,

    /// Validator hotkey
    validator: Hotkey,

    /// Current epoch
    epoch: u64,

    /// Challenge database (isolated per challenge)
    db: Arc<ChallengeDatabase>,

    /// Epoch information
    epoch_info: Option<EpochInfo>,
}

impl ChallengeContext {
    /// Create a new context
    pub fn new(
        challenge_id: ChallengeId,
        validator: Hotkey,
        epoch: u64,
        db: Arc<ChallengeDatabase>,
    ) -> Self {
        Self {
            challenge_id,
            job_id: uuid::Uuid::nil(),
            validator,
            epoch,
            db,
            epoch_info: None,
        }
    }

    /// Set the current job ID
    pub fn with_job_id(mut self, job_id: uuid::Uuid) -> Self {
        self.job_id = job_id;
        self
    }

    /// Set epoch info
    pub fn with_epoch_info(mut self, info: EpochInfo) -> Self {
        self.epoch_info = Some(info);
        self
    }

    /// Get challenge ID
    pub fn challenge_id(&self) -> ChallengeId {
        self.challenge_id
    }

    /// Get current job ID
    pub fn job_id(&self) -> uuid::Uuid {
        self.job_id
    }

    /// Get validator hotkey
    pub fn validator(&self) -> &Hotkey {
        &self.validator
    }

    /// Get current epoch
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Get database access
    pub fn db(&self) -> &ChallengeDatabase {
        &self.db
    }

    /// Get epoch info
    pub fn epoch_info(&self) -> Option<&EpochInfo> {
        self.epoch_info.as_ref()
    }

    /// Log a message (will be associated with the job)
    pub fn log(&self, level: LogLevel, message: &str) {
        let prefix = format!(
            "[Challenge:{} Job:{} Epoch:{}]",
            &self.challenge_id.to_string()[..8],
            &self.job_id.to_string()[..8],
            self.epoch
        );

        match level {
            LogLevel::Debug => tracing::debug!("{} {}", prefix, message),
            LogLevel::Info => tracing::info!("{} {}", prefix, message),
            LogLevel::Warn => tracing::warn!("{} {}", prefix, message),
            LogLevel::Error => tracing::error!("{} {}", prefix, message),
        }
    }

    /// Log info message
    pub fn info(&self, message: &str) {
        self.log(LogLevel::Info, message);
    }

    /// Log debug message
    pub fn debug(&self, message: &str) {
        self.log(LogLevel::Debug, message);
    }

    /// Log warning
    pub fn warn(&self, message: &str) {
        self.log(LogLevel::Warn, message);
    }

    /// Log error
    pub fn error(&self, message: &str) {
        self.log(LogLevel::Error, message);
    }
}

impl std::fmt::Debug for ChallengeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChallengeContext")
            .field("challenge_id", &self.challenge_id)
            .field("job_id", &self.job_id)
            .field("validator", &self.validator)
            .field("epoch", &self.epoch)
            .finish()
    }
}

/// Log levels
#[derive(Clone, Copy, Debug)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}
