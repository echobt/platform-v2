//! Challenge Weight Collector
//!
//! Concurrently fetches weights from all challenge endpoints on epoch transition
//! and batch submits them to Bittensor.

use crate::SubtensorClient;
use anyhow::Result;
use bittensor_rs::chain::ExtrinsicWait;
use bittensor_rs::validator::utility::batch_set_mechanism_weights;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

/// Default timeout for fetching weights from a challenge endpoint
pub const DEFAULT_CHALLENGE_TIMEOUT_SECS: u64 = 60;

/// Challenge endpoint configuration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChallengeEndpoint {
    /// Challenge name for logging
    pub name: String,
    /// Mechanism ID on Bittensor (0-15)
    pub mechanism_id: u8,
    /// HTTP endpoint to fetch weights from
    pub endpoint: String,
    /// Timeout in seconds (default: 60)
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Is this challenge active
    #[serde(default = "default_true")]
    pub active: bool,
}

fn default_timeout() -> u64 {
    DEFAULT_CHALLENGE_TIMEOUT_SECS
}

fn default_true() -> bool {
    true
}

/// Weights response from challenge endpoint
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChallengeWeightsResponse {
    /// Epoch these weights are for
    pub epoch: u64,
    /// UIDs to set weights for
    pub uids: Vec<u16>,
    /// Corresponding weights (0-65535 normalized)
    pub weights: Vec<u16>,
    /// Optional: challenge name
    #[serde(default)]
    pub challenge_name: Option<String>,
    /// Optional: mechanism ID (for verification)
    #[serde(default)]
    pub mechanism_id: Option<u8>,
}

/// Result of fetching weights from a single challenge
#[derive(Clone, Debug)]
pub struct ChallengeWeightResult {
    pub mechanism_id: u8,
    pub challenge_name: String,
    pub uids: Vec<u16>,
    pub weights: Vec<u16>,
    pub success: bool,
    pub error: Option<String>,
    pub duration_ms: u64,
}

/// Collector status
#[derive(Clone, Debug, Default)]
pub struct CollectorStatus {
    pub last_epoch: u64,
    pub last_collection_time: Option<chrono::DateTime<chrono::Utc>>,
    pub successful_challenges: usize,
    pub failed_challenges: usize,
    pub last_tx_hash: Option<String>,
}

/// Challenge Weight Collector
///
/// Fetches weights from all challenge endpoints concurrently and
/// batch submits to Bittensor.
pub struct ChallengeWeightCollector {
    /// Subtensor client for weight submission
    client: SubtensorClient,
    /// Challenge endpoints
    endpoints: Arc<RwLock<Vec<ChallengeEndpoint>>>,
    /// HTTP client for fetching weights
    http_client: reqwest::Client,
    /// Status
    status: Arc<RwLock<CollectorStatus>>,
}

impl ChallengeWeightCollector {
    /// Create a new collector
    pub fn new(client: SubtensorClient) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_CHALLENGE_TIMEOUT_SECS + 5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            endpoints: Arc::new(RwLock::new(Vec::new())),
            http_client,
            status: Arc::new(RwLock::new(CollectorStatus::default())),
        }
    }

    /// Register a challenge endpoint
    pub async fn register_endpoint(&self, endpoint: ChallengeEndpoint) {
        let mut endpoints = self.endpoints.write().await;

        // Check if already registered
        if endpoints
            .iter()
            .any(|e| e.mechanism_id == endpoint.mechanism_id)
        {
            warn!(
                "Endpoint for mechanism {} already registered, updating",
                endpoint.mechanism_id
            );
            endpoints.retain(|e| e.mechanism_id != endpoint.mechanism_id);
        }

        info!(
            "Registered challenge endpoint: {} (mechanism {}) at {}",
            endpoint.name, endpoint.mechanism_id, endpoint.endpoint
        );
        endpoints.push(endpoint);
    }

    /// Register multiple endpoints
    pub async fn register_endpoints(&self, endpoints: Vec<ChallengeEndpoint>) {
        for endpoint in endpoints {
            self.register_endpoint(endpoint).await;
        }
    }

    /// Get registered endpoints
    pub async fn get_endpoints(&self) -> Vec<ChallengeEndpoint> {
        self.endpoints.read().await.clone()
    }

    /// Get collector status
    pub async fn status(&self) -> CollectorStatus {
        self.status.read().await.clone()
    }

    /// Fetch weights from a single challenge endpoint
    async fn fetch_challenge_weights(
        &self,
        endpoint: &ChallengeEndpoint,
        epoch: u64,
    ) -> ChallengeWeightResult {
        let start = std::time::Instant::now();
        let timeout_duration = Duration::from_secs(endpoint.timeout_secs);

        // Build URL with epoch parameter
        let url = if endpoint.endpoint.contains('?') {
            format!("{}&epoch={}", endpoint.endpoint, epoch)
        } else {
            format!("{}?epoch={}", endpoint.endpoint, epoch)
        };

        debug!(
            "Fetching weights from {} (mechanism {}) with {}s timeout",
            endpoint.name, endpoint.mechanism_id, endpoint.timeout_secs
        );

        let result = timeout(timeout_duration, async {
            let response = self.http_client.get(&url).send().await?;

            if !response.status().is_success() {
                return Err(anyhow::anyhow!(
                    "HTTP error: {} - {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                ));
            }

            let weights: ChallengeWeightsResponse = response.json().await?;
            Ok(weights)
        })
        .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok(weights)) => {
                // Verify weights
                if weights.uids.len() != weights.weights.len() {
                    return ChallengeWeightResult {
                        mechanism_id: endpoint.mechanism_id,
                        challenge_name: endpoint.name.clone(),
                        uids: vec![],
                        weights: vec![],
                        success: false,
                        error: Some("UIDs and weights length mismatch".to_string()),
                        duration_ms,
                    };
                }

                info!(
                    "Fetched {} weights from {} in {}ms",
                    weights.uids.len(),
                    endpoint.name,
                    duration_ms
                );

                ChallengeWeightResult {
                    mechanism_id: endpoint.mechanism_id,
                    challenge_name: endpoint.name.clone(),
                    uids: weights.uids,
                    weights: weights.weights,
                    success: true,
                    error: None,
                    duration_ms,
                }
            }
            Ok(Err(e)) => {
                error!("Failed to fetch weights from {}: {}", endpoint.name, e);
                ChallengeWeightResult {
                    mechanism_id: endpoint.mechanism_id,
                    challenge_name: endpoint.name.clone(),
                    uids: vec![],
                    weights: vec![],
                    success: false,
                    error: Some(e.to_string()),
                    duration_ms,
                }
            }
            Err(_) => {
                error!(
                    "Timeout fetching weights from {} after {}s",
                    endpoint.name, endpoint.timeout_secs
                );
                ChallengeWeightResult {
                    mechanism_id: endpoint.mechanism_id,
                    challenge_name: endpoint.name.clone(),
                    uids: vec![],
                    weights: vec![],
                    success: false,
                    error: Some(format!("Timeout after {}s", endpoint.timeout_secs)),
                    duration_ms,
                }
            }
        }
    }

    /// Collect weights from all challenges concurrently
    pub async fn collect_all_weights(&self, epoch: u64) -> Vec<ChallengeWeightResult> {
        let endpoints = self.endpoints.read().await.clone();
        let active_endpoints: Vec<_> = endpoints.into_iter().filter(|e| e.active).collect();

        if active_endpoints.is_empty() {
            warn!("No active challenge endpoints registered");
            return vec![];
        }

        info!(
            "Collecting weights from {} challenges for epoch {}",
            active_endpoints.len(),
            epoch
        );

        // Fetch all concurrently
        let futures: Vec<_> = active_endpoints
            .iter()
            .map(|endpoint| self.fetch_challenge_weights(endpoint, epoch))
            .collect();

        let results = join_all(futures).await;

        // Update status
        let successful = results.iter().filter(|r| r.success).count();
        let failed = results.len() - successful;

        let mut status = self.status.write().await;
        status.last_epoch = epoch;
        status.last_collection_time = Some(chrono::Utc::now());
        status.successful_challenges = successful;
        status.failed_challenges = failed;

        info!(
            "Weight collection complete: {}/{} successful",
            successful,
            results.len()
        );

        results
    }

    /// Collect weights and batch submit to Bittensor
    ///
    /// Returns the transaction hash on success.
    /// If a challenge fails, its weights default to 100% for UID 0 (burn).
    pub async fn collect_and_submit(&self, epoch: u64) -> Result<String> {
        let results = self.collect_all_weights(epoch).await;

        if results.is_empty() {
            return Err(anyhow::anyhow!("No challenge endpoints registered"));
        }

        // Build weights for all mechanisms - use default (100% to UID 0) for failed challenges
        let all_weights: Vec<(u8, Vec<u16>, Vec<u16>)> = results
            .iter()
            .map(|r| {
                if r.success && !r.uids.is_empty() {
                    // Success - use actual weights
                    (r.mechanism_id, r.uids.clone(), r.weights.clone())
                } else {
                    // Failed - default to 100% weight on UID 0 (burn)
                    warn!(
                        "Challenge {} (mechanism {}) failed, defaulting to 100% UID 0: {:?}",
                        r.challenge_name, r.mechanism_id, r.error
                    );
                    (r.mechanism_id, vec![0], vec![65535]) // 100% to UID 0
                }
            })
            .collect();

        info!(
            "Batch submitting weights for {} mechanisms",
            all_weights.len()
        );

        // Log which challenges are being submitted
        for (mec_id, uids, _) in &all_weights {
            if uids.len() == 1 && uids[0] == 0 {
                debug!("  Mechanism {}: DEFAULT (100% to UID 0)", mec_id);
            } else {
                debug!("  Mechanism {}: {} weights", mec_id, uids.len());
            }
        }

        // Submit batch
        let tx_hash = batch_set_mechanism_weights(
            self.client.client()?,
            self.client.signer()?,
            self.client.netuid(),
            all_weights,
            self.client.version_key(),
            ExtrinsicWait::Finalized,
        )
        .await?;

        // Update status
        let mut status = self.status.write().await;
        status.last_tx_hash = Some(tx_hash.clone());

        info!("Batch weight submission successful: {}", tx_hash);
        Ok(tx_hash)
    }

    /// Handle new epoch event
    ///
    /// Called when a new epoch starts. Collects weights and submits.
    pub async fn on_new_epoch(&self, epoch: u64) -> Result<String> {
        info!("New epoch {} - starting weight collection", epoch);
        self.collect_and_submit(epoch).await
    }

    /// Get mutable access to client
    pub fn client_mut(&mut self) -> &mut SubtensorClient {
        &mut self.client
    }

    /// Get client reference
    pub fn client(&self) -> &SubtensorClient {
        &self.client
    }
}

/// Builder for ChallengeWeightCollector
pub struct ChallengeWeightCollectorBuilder {
    client: SubtensorClient,
    endpoints: Vec<ChallengeEndpoint>,
}

impl ChallengeWeightCollectorBuilder {
    pub fn new(client: SubtensorClient) -> Self {
        Self {
            client,
            endpoints: Vec::new(),
        }
    }

    pub fn add_endpoint(mut self, endpoint: ChallengeEndpoint) -> Self {
        self.endpoints.push(endpoint);
        self
    }

    pub fn add_endpoints(mut self, endpoints: Vec<ChallengeEndpoint>) -> Self {
        self.endpoints.extend(endpoints);
        self
    }

    pub async fn build(self) -> ChallengeWeightCollector {
        let collector = ChallengeWeightCollector::new(self.client);
        collector.register_endpoints(self.endpoints).await;
        collector
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_challenge_endpoint_serde() {
        let endpoint = ChallengeEndpoint {
            name: "Test Challenge".to_string(),
            mechanism_id: 1,
            endpoint: "http://localhost:8080/weights".to_string(),
            timeout_secs: 60,
            active: true,
        };

        let json = serde_json::to_string(&endpoint).unwrap();
        let parsed: ChallengeEndpoint = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.mechanism_id, 1);
        assert_eq!(parsed.timeout_secs, 60);
    }

    #[test]
    fn test_weights_response_serde() {
        let response = ChallengeWeightsResponse {
            epoch: 100,
            uids: vec![1, 2, 3],
            weights: vec![20000, 30000, 15535],
            challenge_name: Some("Test".to_string()),
            mechanism_id: Some(1),
        };

        let json = serde_json::to_string(&response).unwrap();
        let parsed: ChallengeWeightsResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.epoch, 100);
        assert_eq!(parsed.uids.len(), 3);
    }
}
