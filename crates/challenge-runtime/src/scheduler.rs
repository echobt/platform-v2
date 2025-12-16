//! Job Scheduler
//!
//! Manages the queue of evaluation jobs.

use parking_lot::RwLock;
use platform_challenge_sdk::{ChallengeId, EvaluationJob, JobStatus};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;
use tracing::debug;

/// Job scheduler for evaluation jobs
pub struct JobScheduler {
    /// Pending jobs queue (per challenge)
    pending: RwLock<HashMap<ChallengeId, VecDeque<EvaluationJob>>>,

    /// Currently running jobs
    running: RwLock<HashMap<uuid::Uuid, EvaluationJob>>,

    /// Completed jobs (recent)
    completed: RwLock<VecDeque<CompletedJob>>,

    /// Maximum concurrent evaluations
    max_concurrent: usize,

    /// Semaphore for concurrency control
    semaphore: Semaphore,

    /// Counters
    pending_count: AtomicUsize,
    running_count: AtomicUsize,
    completed_count: AtomicUsize,
}

impl JobScheduler {
    /// Create a new scheduler
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
            running: RwLock::new(HashMap::new()),
            completed: RwLock::new(VecDeque::with_capacity(1000)),
            max_concurrent,
            semaphore: Semaphore::new(max_concurrent),
            pending_count: AtomicUsize::new(0),
            running_count: AtomicUsize::new(0),
            completed_count: AtomicUsize::new(0),
        }
    }

    /// Submit a job
    pub async fn submit(&self, job: EvaluationJob) -> Result<(), SchedulerError> {
        let challenge_id = job.challenge_id;

        self.pending
            .write()
            .entry(challenge_id)
            .or_insert_with(VecDeque::new)
            .push_back(job);

        self.pending_count.fetch_add(1, Ordering::SeqCst);

        debug!("Job submitted for challenge {:?}", challenge_id);
        Ok(())
    }

    /// Get the next job to execute
    pub async fn next_job(&self) -> Option<EvaluationJob> {
        // Try to acquire semaphore
        let permit = self.semaphore.try_acquire().ok()?;

        // Find a pending job
        let mut pending = self.pending.write();

        for queue in pending.values_mut() {
            if let Some(mut job) = queue.pop_front() {
                job.status = JobStatus::Running;
                job.started_at = Some(chrono::Utc::now());

                let job_id = job.id;

                // Move to running
                self.running.write().insert(job_id, job.clone());

                self.pending_count.fetch_sub(1, Ordering::SeqCst);
                self.running_count.fetch_add(1, Ordering::SeqCst);

                // Forget permit (will be released when job completes)
                std::mem::forget(permit);

                return Some(job);
            }
        }

        // No jobs available, release permit
        drop(permit);
        None
    }

    /// Mark a job as completed
    pub fn complete_job(&self, job_id: uuid::Uuid, status: JobStatus, result: Option<f64>) {
        if let Some(mut job) = self.running.write().remove(&job_id) {
            job.status = status;
            job.completed_at = Some(chrono::Utc::now());

            let completed = CompletedJob {
                job_id: job.id,
                challenge_id: job.challenge_id,
                agent_hash: job.agent_hash.clone(),
                status,
                score: result,
                started_at: job.started_at,
                completed_at: job.completed_at,
            };

            // Add to completed queue
            let mut completed_queue = self.completed.write();
            completed_queue.push_back(completed);

            // Keep only recent completed jobs
            while completed_queue.len() > 1000 {
                completed_queue.pop_front();
            }

            self.running_count.fetch_sub(1, Ordering::SeqCst);
            self.completed_count.fetch_add(1, Ordering::SeqCst);

            // Release semaphore
            self.semaphore.add_permits(1);

            debug!("Job {} completed with status {:?}", job_id, status);
        }
    }

    /// Get pending job count
    pub fn pending_count(&self) -> usize {
        self.pending_count.load(Ordering::SeqCst)
    }

    /// Get running job count
    pub fn running_count(&self) -> usize {
        self.running_count.load(Ordering::SeqCst)
    }

    /// Get completed job count
    pub fn completed_count(&self) -> usize {
        self.completed_count.load(Ordering::SeqCst)
    }

    /// Get pending jobs for a challenge
    pub fn pending_for_challenge(&self, challenge_id: &ChallengeId) -> Vec<EvaluationJob> {
        self.pending
            .read()
            .get(challenge_id)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get running jobs
    pub fn running_jobs(&self) -> Vec<EvaluationJob> {
        self.running.read().values().cloned().collect()
    }

    /// Get recent completed jobs
    pub fn recent_completed(&self, limit: usize) -> Vec<CompletedJob> {
        self.completed
            .read()
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }

    /// Cancel a pending job
    pub fn cancel_job(&self, job_id: uuid::Uuid) -> bool {
        let mut pending = self.pending.write();

        for queue in pending.values_mut() {
            if let Some(pos) = queue.iter().position(|j| j.id == job_id) {
                queue.remove(pos);
                self.pending_count.fetch_sub(1, Ordering::SeqCst);
                debug!("Job {} cancelled", job_id);
                return true;
            }
        }

        false
    }

    /// Clear all pending jobs for a challenge
    pub fn clear_pending(&self, challenge_id: &ChallengeId) -> usize {
        let mut pending = self.pending.write();

        if let Some(queue) = pending.get_mut(challenge_id) {
            let count = queue.len();
            queue.clear();
            self.pending_count.fetch_sub(count, Ordering::SeqCst);
            return count;
        }

        0
    }

    /// Get scheduler stats
    pub fn stats(&self) -> SchedulerStats {
        SchedulerStats {
            pending: self.pending_count(),
            running: self.running_count(),
            completed: self.completed_count(),
            max_concurrent: self.max_concurrent,
        }
    }
}

/// Completed job info
#[derive(Clone, Debug)]
pub struct CompletedJob {
    pub job_id: uuid::Uuid,
    pub challenge_id: ChallengeId,
    pub agent_hash: String,
    pub status: JobStatus,
    pub score: Option<f64>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Scheduler statistics
#[derive(Clone, Debug)]
pub struct SchedulerStats {
    pub pending: usize,
    pub running: usize,
    pub completed: usize,
    pub max_concurrent: usize,
}

/// Scheduler errors
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("Queue full")]
    QueueFull,

    #[error("Job not found: {0}")]
    NotFound(uuid::Uuid),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_job(challenge_id: ChallengeId) -> EvaluationJob {
        EvaluationJob::new(
            challenge_id,
            "agent_hash".to_string(),
            "evaluate".to_string(),
            serde_json::Value::Null,
        )
    }

    #[tokio::test]
    async fn test_submit_and_get_job() {
        let scheduler = JobScheduler::new(4);
        let challenge_id = ChallengeId::new();

        let job = create_test_job(challenge_id);
        let job_id = job.id;

        scheduler.submit(job).await.unwrap();

        assert_eq!(scheduler.pending_count(), 1);

        let next = scheduler.next_job().await;
        assert!(next.is_some());
        assert_eq!(next.unwrap().id, job_id);

        assert_eq!(scheduler.pending_count(), 0);
        assert_eq!(scheduler.running_count(), 1);
    }

    #[tokio::test]
    async fn test_complete_job() {
        let scheduler = JobScheduler::new(4);
        let challenge_id = ChallengeId::new();

        let job = create_test_job(challenge_id);
        let job_id = job.id;

        scheduler.submit(job).await.unwrap();
        scheduler.next_job().await;

        scheduler.complete_job(job_id, JobStatus::Completed, Some(0.85));

        assert_eq!(scheduler.running_count(), 0);
        assert_eq!(scheduler.completed_count(), 1);
    }

    #[tokio::test]
    async fn test_concurrency_limit() {
        let scheduler = JobScheduler::new(2);
        let challenge_id = ChallengeId::new();

        // Submit 3 jobs
        for _ in 0..3 {
            scheduler
                .submit(create_test_job(challenge_id))
                .await
                .unwrap();
        }

        // Get 2 jobs (at limit)
        scheduler.next_job().await.unwrap();
        scheduler.next_job().await.unwrap();

        // Third should be None (at limit)
        assert!(scheduler.next_job().await.is_none());
        assert_eq!(scheduler.running_count(), 2);
        assert_eq!(scheduler.pending_count(), 1);
    }
}
