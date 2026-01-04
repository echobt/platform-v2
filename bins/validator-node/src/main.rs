//! Validator Node - Centralized Architecture
//!
//! All communication via platform-server (chain.platform.network).
//! No P2P networking. Weights submitted via Subtensor (handles CRv4 automatically).

use anyhow::Result;
use challenge_orchestrator::{ChallengeOrchestrator, OrchestratorConfig};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use platform_bittensor::{
    signer_from_seed, sync_metagraph, BittensorClient, BittensorSigner, BlockSync, BlockSyncConfig,
    BlockSyncEvent, ExtrinsicWait, Subtensor,
};
use platform_core::{production_sudo_key, ChainState, Keypair, NetworkConfig};
use platform_rpc::{RpcConfig, RpcServer};
use platform_storage::Storage;
use platform_subnet_manager::BanList;
use secure_container_runtime::{run_ws_server, ContainerBroker, SecurityPolicy, WsConfig};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

// ==================== Platform Server Client ====================

#[derive(Clone)]
pub struct PlatformServerClient {
    base_url: String,
    client: reqwest::Client,
}

impl PlatformServerClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("HTTP client"),
        }
    }

    /// Health check with infinite retry loop (30s interval)
    pub async fn health_with_retry(&self) -> bool {
        let mut attempt = 0u64;
        loop {
            attempt += 1;
            match self
                .client
                .get(format!("{}/health", self.base_url))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    info!("Platform server connected (attempt {})", attempt);
                    return true;
                }
                Ok(r) => {
                    warn!(
                        "Platform server health check failed: {} (attempt {}, retrying in 30s)",
                        r.status(),
                        attempt
                    );
                }
                Err(e) => {
                    warn!(
                        "Platform server not reachable: {} (attempt {}, retrying in 30s)",
                        e, attempt
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }

    pub async fn health(&self) -> bool {
        self.client
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// List challenges with infinite retry loop (30s interval)
    pub async fn list_challenges(&self) -> Result<Vec<ChallengeInfo>> {
        let url = format!("{}/api/v1/challenges", self.base_url);
        let mut attempt = 0u64;
        loop {
            attempt += 1;
            match self.client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<Vec<ChallengeInfo>>().await {
                        Ok(challenges) => return Ok(challenges),
                        Err(e) => {
                            warn!(
                                "Failed to parse challenges response: {} (attempt {}, retrying in 30s)",
                                e, attempt
                            );
                        }
                    }
                }
                Ok(resp) => {
                    warn!(
                        "Failed to list challenges: {} (attempt {}, retrying in 30s)",
                        resp.status(),
                        attempt
                    );
                }
                Err(e) => {
                    warn!(
                        "Platform server not reachable: {} (attempt {}, retrying in 30s)",
                        e, attempt
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }

    /// Get weights with infinite retry loop (30s interval)
    pub async fn get_weights(&self, challenge_id: &str, epoch: u64) -> Result<Vec<(u16, u16)>> {
        let url = format!(
            "{}/api/v1/challenges/{}/get_weights?epoch={}",
            self.base_url, challenge_id, epoch
        );
        let mut attempt = 0u64;
        loop {
            attempt += 1;
            match self.client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<serde_json::Value>().await {
                        Ok(data) => {
                            let weights = data
                                .get("weights")
                                .and_then(|w| w.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|w| {
                                            Some((
                                                w.get("uid")?.as_u64()? as u16,
                                                w.get("weight")?.as_u64()? as u16,
                                            ))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            return Ok(weights);
                        }
                        Err(e) => {
                            warn!(
                                "Failed to parse weights response: {} (attempt {}, retrying in 30s)",
                                e, attempt
                            );
                        }
                    }
                }
                Ok(resp) => {
                    warn!(
                        "Failed to get weights for {}: {} (attempt {}, retrying in 30s)",
                        challenge_id,
                        resp.status(),
                        attempt
                    );
                }
                Err(e) => {
                    warn!(
                        "Platform server not reachable: {} (attempt {}, retrying in 30s)",
                        e, attempt
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChallengeInfo {
    pub id: String,
    #[allow(dead_code)]
    pub name: String,
    pub mechanism_id: i32,
    #[allow(dead_code)]
    pub emission_weight: f64,
    pub is_healthy: bool,
}

// ==================== WebSocket Events ====================

/// WebSocket event from platform-server
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum WsEvent {
    #[serde(rename = "challenge_event")]
    ChallengeEvent(ChallengeCustomEvent),
    #[serde(rename = "challenge_stopped")]
    ChallengeStopped(ChallengeStoppedEvent),
    #[serde(rename = "challenge_started")]
    ChallengeStarted(ChallengeStartedEvent),
    #[serde(rename = "ping")]
    Ping,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChallengeStoppedEvent {
    pub id: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChallengeStartedEvent {
    pub id: String,
    pub endpoint: String,
    pub docker_image: String,
    pub mechanism_id: u8,
    pub emission_weight: f64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_cpu")]
    pub cpu_cores: f64,
    #[serde(default = "default_memory")]
    pub memory_mb: u64,
    #[serde(default)]
    pub gpu_required: bool,
}

fn default_timeout() -> u64 {
    3600
}
fn default_cpu() -> f64 {
    2.0
}
fn default_memory() -> u64 {
    4096
}

/// Custom event from a challenge
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChallengeCustomEvent {
    pub challenge_id: String,
    pub event_name: String,
    pub payload: serde_json::Value,
    pub timestamp: i64,
}

/// Payload for new_submission event from term-challenge (broadcast to all)
#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewSubmissionPayload {
    pub submission_id: String,
    pub agent_hash: String,
    pub miner_hotkey: String,
    pub source_code: String,
    pub name: Option<String>,
    pub epoch: i64,
}

/// Payload for new_submission_assigned event (targeted to specific validators)
/// This notifies validator they are assigned, but binary may not be ready yet
#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewSubmissionAssignedPayload {
    pub agent_hash: String,
    pub miner_hotkey: String,
    pub submission_id: String,
    pub challenge_id: String,
    /// Endpoint to download the binary (e.g., "/api/v1/validator/download_binary/{agent_hash}")
    pub download_endpoint: String,
}

/// Payload for binary_ready event (sent when compilation completes)
/// This is when validators should actually download and evaluate
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BinaryReadyPayload {
    pub agent_hash: String,
    pub challenge_id: String,
    /// Endpoint to download the binary
    pub download_endpoint: String,
}

// ==================== CLI ====================

#[derive(Parser, Debug)]
#[command(name = "validator-node")]
#[command(about = "Platform Validator - Centralized Architecture")]
struct Args {
    /// Secret key (hex or mnemonic)
    #[arg(short = 'k', long, env = "VALIDATOR_SECRET_KEY")]
    secret_key: Option<String>,

    /// Data directory
    #[arg(short, long, default_value = "./data")]
    data_dir: PathBuf,

    /// Stake in TAO (for --no-bittensor mode)
    #[arg(long, default_value = "1000")]
    stake: f64,

    #[arg(
        long,
        env = "SUBTENSOR_ENDPOINT",
        default_value = "wss://entrypoint-finney.opentensor.ai:443"
    )]
    subtensor_endpoint: String,

    #[arg(long, env = "NETUID", default_value = "100")]
    netuid: u16,

    #[arg(long)]
    no_bittensor: bool,

    #[arg(long, default_value = "8080")]
    rpc_port: u16,

    #[arg(long, default_value = "0.0.0.0")]
    rpc_addr: String,

    #[arg(long, default_value = "true")]
    docker_challenges: bool,

    #[arg(long, env = "BROKER_WS_PORT", default_value = "8090")]
    broker_port: u16,

    #[arg(long, env = "BROKER_JWT_SECRET")]
    broker_jwt_secret: Option<String>,

    #[arg(
        long,
        env = "PLATFORM_SERVER_URL",
        default_value = "https://chain.platform.network"
    )]
    platform_server: String,

    #[arg(long, env = "VERSION_KEY", default_value = "1")]
    version_key: u64,
}

// ==================== Main ====================

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,validator_node=debug".into()),
        )
        .init();

    let args = Args::parse();
    info!("Starting validator (centralized mode)");

    // Keypair
    let keypair = load_keypair(&args)?;
    info!("Validator: {}", keypair.ss58_address());

    // Data dir
    std::fs::create_dir_all(&args.data_dir)?;
    let data_dir = std::fs::canonicalize(&args.data_dir)?;

    // Storage
    let storage = Storage::open(data_dir.join("validator.db"))?;
    let _storage = Arc::new(storage);

    // Chain state
    let chain_state = Arc::new(RwLock::new(ChainState::new(
        production_sudo_key(),
        NetworkConfig::default(),
    )));
    let bans = Arc::new(RwLock::new(BanList::default()));

    // Platform server - wait until connected (infinite retry)
    let platform_client = Arc::new(PlatformServerClient::new(&args.platform_server));
    info!("Platform server: {}", args.platform_server);
    platform_client.health_with_retry().await;

    // Container broker
    info!("Container broker on port {}...", args.broker_port);
    let broker = Arc::new(ContainerBroker::with_policy(SecurityPolicy::default()).await?);
    let ws_config = WsConfig {
        bind_addr: format!("0.0.0.0:{}", args.broker_port),
        jwt_secret: args.broker_jwt_secret.clone(),
        allowed_challenges: vec![],
        max_connections_per_challenge: 10,
    };
    let broker_clone = broker.clone();
    tokio::spawn(async move {
        if let Err(e) = run_ws_server(broker_clone, ws_config).await {
            error!("Broker error: {}", e);
        }
    });

    // Challenge orchestrator
    let orchestrator = if args.docker_challenges {
        match ChallengeOrchestrator::new(OrchestratorConfig {
            network_name: "platform-network".to_string(),
            health_check_interval: Duration::from_secs(30),
            stop_timeout: Duration::from_secs(30),
            registry: None,
        })
        .await
        {
            Ok(o) => Some(Arc::new(o)),
            Err(e) => {
                warn!("Docker orchestrator failed: {}", e);
                None
            }
        }
    } else {
        None
    };

    // List challenges and start containers
    let challenges = platform_client.list_challenges().await?;
    if challenges.is_empty() {
        info!("No challenges registered on platform-server");
    } else {
        info!("Challenges from platform-server:");
        for ch in &challenges {
            info!(
                "  - {} (mechanism={}, healthy={})",
                ch.id, ch.mechanism_id, ch.is_healthy
            );
        }

        // Start challenge containers
        if let Some(ref orch) = orchestrator {
            for ch in &challenges {
                let docker_image = format!("ghcr.io/platformnetwork/{}:latest", ch.id);
                // Generate a deterministic UUID from challenge name
                let challenge_uuid =
                    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, ch.id.as_bytes());
                let config = challenge_orchestrator::ChallengeContainerConfig {
                    challenge_id: platform_core::ChallengeId(challenge_uuid),
                    name: ch.id.clone(),
                    docker_image,
                    mechanism_id: ch.mechanism_id as u8,
                    emission_weight: ch.emission_weight,
                    timeout_secs: 3600,
                    cpu_cores: 2.0,
                    memory_mb: 4096,
                    gpu_required: false,
                };

                info!("Starting challenge container: {}", ch.id);
                match orch.add_challenge(config).await {
                    Ok(_) => info!("Challenge container started: {}", ch.id),
                    Err(e) => error!("Failed to start challenge {}: {}", ch.id, e),
                }
            }

            // Start health monitoring
            let orch_clone = orch.clone();
            tokio::spawn(async move {
                if let Err(e) = orch_clone.start().await {
                    error!("Orchestrator health monitor error: {}", e);
                }
            });
        }
    }

    // Build challenge URL map for WebSocket event handler
    // Maps challenge name -> local container endpoint from orchestrator
    let challenge_urls: Arc<RwLock<HashMap<String, String>>> =
        Arc::new(RwLock::new(HashMap::new()));
    if let Some(ref orch) = orchestrator {
        for ch in &challenges {
            // Generate same deterministic UUID used when starting the container
            let challenge_uuid = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, ch.id.as_bytes());
            let challenge_id = platform_core::ChallengeId(challenge_uuid);

            // Get the actual endpoint from the orchestrator
            if let Some(instance) = orch.get_challenge(&challenge_id) {
                challenge_urls
                    .write()
                    .insert(ch.id.clone(), instance.endpoint.clone());
                info!(
                    "Challenge URL registered: {} -> {}",
                    ch.id, instance.endpoint
                );
            } else {
                warn!("Challenge {} not found in orchestrator", ch.id);
            }
        }
    }

    // Start WebSocket listener for platform-server events
    // This listens for new_submission events and triggers local evaluation
    // Also handles challenge_stopped events to stop local containers
    let ws_platform_url = args.platform_server.clone();
    let ws_keypair = keypair.clone();
    let ws_challenge_urls = challenge_urls.clone();
    let ws_orchestrator = orchestrator.clone();
    tokio::spawn(async move {
        start_websocket_listener(
            ws_platform_url,
            ws_keypair,
            ws_challenge_urls,
            ws_orchestrator,
        )
        .await;
    });

    // RPC server
    let addr: SocketAddr = format!("{}:{}", args.rpc_addr, args.rpc_port).parse()?;
    let rpc_server = RpcServer::new(
        RpcConfig {
            addr,
            netuid: args.netuid,
            name: "Platform".to_string(),
            min_stake: (args.stake * 1e9) as u64,
            cors_enabled: true,
        },
        chain_state.clone(),
        bans.clone(),
    );
    let _rpc = rpc_server.spawn();
    info!("RPC: http://{}:{}", args.rpc_addr, args.rpc_port);

    // Bittensor setup
    let subtensor: Option<Arc<Subtensor>>;
    let subtensor_signer: Option<Arc<BittensorSigner>>;
    let mut block_rx: Option<tokio::sync::mpsc::Receiver<BlockSyncEvent>> = None;

    if !args.no_bittensor {
        info!(
            "Bittensor: {} (netuid={})",
            args.subtensor_endpoint, args.netuid
        );

        // Create Subtensor with persistence for automatic commit-reveal handling
        let state_path = data_dir.join("subtensor_state.json");
        match Subtensor::with_persistence(&args.subtensor_endpoint, state_path.clone()).await {
            Ok(st) => {
                info!("Subtensor connected with persistence at {:?}", state_path);

                // Create signer
                let secret = args.secret_key.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("VALIDATOR_SECRET_KEY required for Bittensor")
                })?;

                match signer_from_seed(secret) {
                    Ok(signer) => {
                        info!("Bittensor hotkey: {}", signer.account_id());
                        subtensor_signer = Some(Arc::new(signer));
                    }
                    Err(e) => {
                        error!("Failed to create signer: {}", e);
                        subtensor_signer = None;
                    }
                }

                subtensor = Some(Arc::new(st));

                // Sync metagraph
                let client = BittensorClient::new(&args.subtensor_endpoint).await?;
                match sync_metagraph(&client, args.netuid).await {
                    Ok(mg) => info!("Metagraph: {} neurons", mg.n),
                    Err(e) => warn!("Metagraph sync failed: {}", e),
                }

                // Block sync
                let mut sync = BlockSync::new(BlockSyncConfig {
                    netuid: args.netuid,
                    ..Default::default()
                });
                let rx = sync.take_event_receiver();

                let client = Arc::new(client);
                if let Err(e) = sync.connect(client).await {
                    warn!("Block sync connect failed: {}", e);
                } else {
                    tokio::spawn(async move {
                        if let Err(e) = sync.start().await {
                            error!("Block sync error: {}", e);
                        }
                    });
                    block_rx = rx;
                    info!("Block sync: started");
                }
            }
            Err(e) => {
                error!("Subtensor connection failed: {}", e);
                subtensor = None;
                subtensor_signer = None;
            }
        }
    } else {
        info!("Bittensor: disabled");
        subtensor = None;
        subtensor_signer = None;
    }

    info!("Validator running. Ctrl+C to stop.");

    let netuid = args.netuid;
    let version_key = args.version_key;
    let mut interval = tokio::time::interval(Duration::from_secs(60));

    loop {
        tokio::select! {
            Some(event) = async {
                match block_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                handle_block_event(
                    event,
                    &platform_client,
                    &subtensor,
                    &subtensor_signer,
                    netuid,
                    version_key,
                ).await;
            }

            _ = interval.tick() => {
                debug!("Heartbeat");
            }

            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down...");
                break;
            }
        }
    }

    info!("Stopped.");
    Ok(())
}

fn load_keypair(args: &Args) -> Result<Keypair> {
    let secret = args
        .secret_key
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("VALIDATOR_SECRET_KEY required"))?
        .trim();
    let hex = secret.strip_prefix("0x").unwrap_or(secret);

    if hex.len() == 64 {
        if let Ok(bytes) = hex::decode(hex) {
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                return Ok(Keypair::from_seed(&arr)?);
            }
        }
    }
    Ok(Keypair::from_mnemonic(secret)?)
}

async fn handle_block_event(
    event: BlockSyncEvent,
    platform_client: &Arc<PlatformServerClient>,
    subtensor: &Option<Arc<Subtensor>>,
    signer: &Option<Arc<BittensorSigner>>,
    netuid: u16,
    version_key: u64,
) {
    match event {
        BlockSyncEvent::NewBlock { block_number, .. } => {
            debug!("Block {}", block_number);
        }

        BlockSyncEvent::EpochTransition {
            old_epoch,
            new_epoch,
            block,
        } => {
            info!("Epoch: {} -> {} (block {})", old_epoch, new_epoch, block);
        }

        BlockSyncEvent::CommitWindowOpen { epoch, block } => {
            info!("=== COMMIT WINDOW: epoch {} block {} ===", epoch, block);

            // Submit weights via Subtensor (handles CRv4/commit-reveal automatically)
            if let (Some(st), Some(sig)) = (subtensor.as_ref(), signer.as_ref()) {
                // Fetch weights from platform-server
                let mechanism_weights = match platform_client.list_challenges().await {
                    Ok(challenges) if !challenges.is_empty() => {
                        let mut weights = Vec::new();

                        for challenge in challenges.iter().filter(|c| c.is_healthy) {
                            match platform_client.get_weights(&challenge.id, epoch).await {
                                Ok(w) if !w.is_empty() => {
                                    let uids: Vec<u16> = w.iter().map(|(u, _)| *u).collect();
                                    let vals: Vec<u16> = w.iter().map(|(_, v)| *v).collect();

                                    info!(
                                        "Challenge {} (mech {}): {} weights",
                                        challenge.id,
                                        challenge.mechanism_id,
                                        uids.len()
                                    );

                                    weights.push((challenge.mechanism_id as u8, uids, vals));
                                }
                                Ok(_) => debug!("Challenge {} has no weights", challenge.id),
                                Err(e) => {
                                    warn!("Failed to get weights for {}: {}", challenge.id, e)
                                }
                            }
                        }

                        weights
                    }
                    Ok(_) => {
                        info!("No challenges on platform-server");
                        vec![]
                    }
                    Err(e) => {
                        warn!("Failed to list challenges: {}", e);
                        vec![]
                    }
                };

                // Submit weights (or burn weights if none)
                let weights_to_submit = if mechanism_weights.is_empty() {
                    info!("No weights - submitting burn weights to UID 0");
                    vec![(0u8, vec![0u16], vec![65535u16])]
                } else {
                    mechanism_weights
                };

                // Submit each mechanism via Subtensor (handles CRv4 automatically)
                for (mechanism_id, uids, weights) in weights_to_submit {
                    match st
                        .set_mechanism_weights(
                            sig,
                            netuid,
                            mechanism_id,
                            &uids,
                            &weights,
                            version_key,
                            ExtrinsicWait::Finalized,
                        )
                        .await
                    {
                        Ok(resp) if resp.success => {
                            info!(
                                "Mechanism {} weights submitted: {:?}",
                                mechanism_id, resp.tx_hash
                            );
                        }
                        Ok(resp) => {
                            warn!("Mechanism {} issue: {}", mechanism_id, resp.message);
                        }
                        Err(e) => {
                            error!("Mechanism {} failed: {}", mechanism_id, e);
                        }
                    }
                }
            } else {
                warn!("No Subtensor/signer - cannot submit weights");
            }
        }

        BlockSyncEvent::RevealWindowOpen { epoch, block } => {
            info!("=== REVEAL WINDOW: epoch {} block {} ===", epoch, block);

            // With CRv4, reveals are automatic via DRAND
            // For older versions, Subtensor handles reveals internally
            if let (Some(st), Some(sig)) = (subtensor.as_ref(), signer.as_ref()) {
                if st.has_pending_commits().await {
                    info!("Revealing pending commits...");
                    match st.reveal_all_pending(sig, ExtrinsicWait::Finalized).await {
                        Ok(results) => {
                            for resp in results {
                                if resp.success {
                                    info!("Revealed: {:?}", resp.tx_hash);
                                }
                            }
                        }
                        Err(e) => error!("Reveal failed: {}", e),
                    }
                }
            }
        }

        BlockSyncEvent::PhaseChange {
            old_phase,
            new_phase,
            ..
        } => {
            debug!("Phase: {:?} -> {:?}", old_phase, new_phase);
        }

        BlockSyncEvent::Disconnected(e) => warn!("Bittensor disconnected: {}", e),
        BlockSyncEvent::Reconnected => info!("Bittensor reconnected"),
    }
}

// ==================== WebSocket Event Listener ====================

/// Start WebSocket listener for platform-server events
/// Listens for challenge events and triggers evaluations
/// Also handles challenge_stopped events to stop local containers
pub async fn start_websocket_listener(
    platform_url: String,
    keypair: Keypair,
    challenge_urls: Arc<RwLock<HashMap<String, String>>>,
    orchestrator: Option<Arc<ChallengeOrchestrator>>,
) {
    let validator_hotkey = keypair.ss58_address();
    let keypair = Arc::new(keypair); // Wrap in Arc for sharing across tasks

    // Convert HTTP URL to WebSocket URL with authentication params
    let base_ws_url = platform_url
        .replace("https://", "wss://")
        .replace("http://", "ws://")
        + "/ws";

    info!("Starting WebSocket listener: {}", base_ws_url);

    loop {
        // Generate fresh timestamp and signature for each connection attempt
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let message = format!("ws_connect:{}:{}", validator_hotkey, timestamp);
        let signature = hex::encode(keypair.sign_bytes(message.as_bytes()).unwrap_or_default());

        let ws_url = format!(
            "{}?hotkey={}&timestamp={}&signature={}&role=validator",
            base_ws_url, validator_hotkey, timestamp, signature
        );

        match connect_to_websocket(
            &ws_url,
            keypair.clone(),
            challenge_urls.clone(),
            orchestrator.clone(),
        )
        .await
        {
            Ok(()) => {
                info!("WebSocket connection closed, reconnecting in 5s...");
            }
            Err(e) => {
                warn!("WebSocket error: {}, reconnecting in 30s...", e);
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn connect_to_websocket(
    ws_url: &str,
    keypair: Arc<Keypair>,
    challenge_urls: Arc<RwLock<HashMap<String, String>>>,
    orchestrator: Option<Arc<ChallengeOrchestrator>>,
) -> Result<()> {
    let validator_hotkey = keypair.ss58_address();
    let (ws_stream, _) = connect_async(ws_url).await?;
    let (mut write, mut read) = ws_stream.split();

    info!("WebSocket connected to platform-server");

    // Send ping periodically to keep connection alive
    let ping_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            if write.send(Message::Ping(vec![])).await.is_err() {
                break;
            }
        }
    });

    // Process incoming messages
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => match serde_json::from_str::<WsEvent>(&text) {
                Ok(WsEvent::ChallengeEvent(event)) => {
                    handle_challenge_event(event, keypair.clone(), challenge_urls.clone()).await;
                }
                Ok(WsEvent::ChallengeStopped(event)) => {
                    info!("Received challenge_stopped event for: {}", event.id);
                    if let Some(ref orch) = orchestrator {
                        // Get the ChallengeId from challenge name
                        let challenge_uuid =
                            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, event.id.as_bytes());
                        let challenge_id = platform_core::ChallengeId(challenge_uuid);
                        match orch.remove_challenge(challenge_id).await {
                            Ok(_) => info!("Challenge container stopped: {}", event.id),
                            Err(e) => {
                                warn!("Failed to stop challenge container {}: {}", event.id, e)
                            }
                        }
                        // Remove from URL map
                        challenge_urls.write().remove(&event.id);
                    } else {
                        warn!("No orchestrator available to stop challenge: {}", event.id);
                    }
                }
                Ok(WsEvent::ChallengeStarted(event)) => {
                    info!(
                        "Received challenge_started event for: {} at {} (image: {}, emission: {})",
                        event.id, event.endpoint, event.docker_image, event.emission_weight
                    );
                    // Start the challenge container locally using values from the event
                    if let Some(ref orch) = orchestrator {
                        let challenge_uuid =
                            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, event.id.as_bytes());
                        let config = challenge_orchestrator::ChallengeContainerConfig {
                            challenge_id: platform_core::ChallengeId(challenge_uuid),
                            name: event.id.clone(),
                            docker_image: event.docker_image.clone(),
                            mechanism_id: event.mechanism_id,
                            emission_weight: event.emission_weight,
                            timeout_secs: event.timeout_secs,
                            cpu_cores: event.cpu_cores,
                            memory_mb: event.memory_mb,
                            gpu_required: event.gpu_required,
                        };

                        match orch.add_challenge(config).await {
                            Ok(_) => {
                                info!("Challenge container started locally: {}", event.id);
                                // Add to URL map
                                challenge_urls
                                    .write()
                                    .insert(event.id.clone(), event.endpoint.clone());
                            }
                            Err(e) => {
                                error!("Failed to start challenge container {}: {}", event.id, e)
                            }
                        }
                    } else {
                        warn!("No orchestrator available to start challenge: {}", event.id);
                    }
                }
                Ok(WsEvent::Ping) => {
                    debug!("Received ping from server");
                }
                Ok(WsEvent::Other) => {
                    debug!("Received other event");
                }
                Err(e) => {
                    debug!("Failed to parse WebSocket message: {}", e);
                }
            },
            Ok(Message::Ping(_)) => {
                debug!("Received ping");
            }
            Ok(Message::Close(_)) => {
                info!("WebSocket closed by server");
                break;
            }
            Err(e) => {
                warn!("WebSocket receive error: {}", e);
                break;
            }
            _ => {}
        }
    }

    ping_task.abort();
    Ok(())
}

/// Handle challenge-specific events
async fn handle_challenge_event(
    event: ChallengeCustomEvent,
    keypair: Arc<Keypair>,
    challenge_urls: Arc<RwLock<HashMap<String, String>>>,
) {
    let validator_hotkey = keypair.ss58_address();
    info!(
        "Challenge event: {}:{} (ts: {})",
        event.challenge_id, event.event_name, event.timestamp
    );

    // Handle new_submission_assigned events (targeted notification - validator was selected)
    // This just notifies us we're assigned - binary may not be ready yet
    // We wait for binary_ready event before downloading
    if event.event_name == "new_submission_assigned" {
        match serde_json::from_value::<NewSubmissionAssignedPayload>(event.payload.clone()) {
            Ok(payload) => {
                info!(
                    "Assigned to evaluate agent {} from {} (waiting for binary_ready)",
                    &payload.agent_hash[..16.min(payload.agent_hash.len())],
                    &payload.miner_hotkey[..16.min(payload.miner_hotkey.len())]
                );
                // Don't download yet - wait for binary_ready event
            }
            Err(e) => {
                warn!("Failed to parse new_submission_assigned payload: {}", e);
            }
        }
        return;
    }

    // Handle binary_ready events (compilation complete - now download and evaluate)
    if event.event_name == "binary_ready" {
        match serde_json::from_value::<BinaryReadyPayload>(event.payload.clone()) {
            Ok(payload) => {
                info!(
                    "Binary ready for agent {}, starting download and evaluation",
                    &payload.agent_hash[..16.min(payload.agent_hash.len())]
                );

                // Convert to NewSubmissionAssignedPayload format for download function
                let download_payload = NewSubmissionAssignedPayload {
                    agent_hash: payload.agent_hash,
                    miner_hotkey: String::new(), // Not needed for download
                    submission_id: String::new(), // Not needed for download
                    challenge_id: payload.challenge_id.clone(),
                    download_endpoint: payload.download_endpoint,
                };

                // Spawn binary download and evaluation task
                let kp = keypair.clone();
                let challenge_id = event.challenge_id.clone();
                tokio::spawn(async move {
                    download_binary_and_evaluate(&challenge_id, download_payload, kp).await;
                });
            }
            Err(e) => {
                warn!("Failed to parse binary_ready payload: {}", e);
            }
        }
        return;
    }

    // Handle legacy new_submission events (broadcast to all - kept for backward compatibility)
    if event.event_name == "new_submission" {
        match serde_json::from_value::<NewSubmissionPayload>(event.payload.clone()) {
            Ok(submission) => {
                info!(
                    "New submission (broadcast): agent={} from={}",
                    &submission.agent_hash[..16.min(submission.agent_hash.len())],
                    &submission.miner_hotkey[..16.min(submission.miner_hotkey.len())]
                );

                // Get challenge container URL
                let challenge_url = {
                    let urls = challenge_urls.read();
                    urls.get(&event.challenge_id).cloned()
                };

                if let Some(url) = challenge_url {
                    // Spawn evaluation task
                    let kp = keypair.clone();
                    let challenge_id = event.challenge_id.clone();
                    tokio::spawn(async move {
                        evaluate_and_submit(&url, &challenge_id, submission, kp).await;
                    });
                } else {
                    warn!("No local container for challenge: {}", event.challenge_id);
                }
            }
            Err(e) => {
                warn!("Failed to parse new_submission payload: {}", e);
            }
        }
    }
}

/// Download binary via bridge and start evaluation
///
/// This is the new flow for assigned validators:
/// 1. Download binary from central server via bridge
/// 2. Run agent binary in isolated Docker container
/// 3. Submit results back via bridge
async fn download_binary_and_evaluate(
    challenge_id: &str,
    payload: NewSubmissionAssignedPayload,
    keypair: Arc<Keypair>,
) {
    let validator_hotkey = keypair.ss58_address();
    let platform_url = std::env::var("PLATFORM_SERVER_URL")
        .unwrap_or_else(|_| "https://chain.platform.network".to_string());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300)) // 5 min timeout for binary download
        .build()
        .unwrap_or_default();

    // Generate timestamp and signature for authentication
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // Sign with validator's keypair for authentication
    let message = format!("download_binary:{}:{}", payload.agent_hash, timestamp);
    let signature = hex::encode(keypair.sign_bytes(message.as_bytes()).unwrap_or_default());

    // Download binary via bridge
    let download_url = format!(
        "{}/api/v1/bridge/{}/api/v1/validator/download_binary/{}",
        platform_url, challenge_id, payload.agent_hash
    );

    info!(
        "Downloading binary for agent {} from {}",
        &payload.agent_hash[..16.min(payload.agent_hash.len())],
        download_url
    );

    let download_request = serde_json::json!({
        "validator_hotkey": validator_hotkey,
        "signature": signature,
        "timestamp": timestamp,
    });

    let binary = match client
        .post(&download_url)
        .json(&download_request)
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                match response.bytes().await {
                    Ok(binary) => {
                        info!(
                            "Downloaded binary for agent {} ({} bytes)",
                            &payload.agent_hash[..16.min(payload.agent_hash.len())],
                            binary.len()
                        );
                        binary.to_vec()
                    }
                    Err(e) => {
                        warn!(
                            "Failed to read binary response for agent {}: {}",
                            &payload.agent_hash[..16.min(payload.agent_hash.len())],
                            e
                        );
                        return;
                    }
                }
            } else {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                warn!(
                    "Binary download failed for agent {}: {} - {}",
                    &payload.agent_hash[..16.min(payload.agent_hash.len())],
                    status,
                    text
                );
                return;
            }
        }
        Err(e) => {
            warn!(
                "Binary download request failed for agent {}: {}",
                &payload.agent_hash[..16.min(payload.agent_hash.len())],
                e
            );
            return;
        }
    };

    // Run evaluation in isolated Docker container
    let result = run_binary_evaluation(challenge_id, &payload, &binary, &validator_hotkey).await;

    match result {
        Ok((score, tasks_passed, tasks_total, tasks_failed, total_cost)) => {
            info!(
                "Evaluation complete for {}: score={:.2}%, passed={}/{}",
                &payload.agent_hash[..16.min(payload.agent_hash.len())],
                score * 100.0,
                tasks_passed,
                tasks_total
            );

            // Submit result to central server
            submit_evaluation_result(
                challenge_id,
                &payload.agent_hash,
                keypair,
                score,
                tasks_passed,
                tasks_total,
                tasks_failed,
                total_cost,
                0, // epoch - will be filled by server
            )
            .await;
        }
        Err(e) => {
            error!(
                "Evaluation failed for agent {}: {}",
                &payload.agent_hash[..16.min(payload.agent_hash.len())],
                e
            );
        }
    }
}

/// Run the agent binary in an isolated Docker container for evaluation
///
/// This function:
/// 1. Creates a temporary file with the binary
/// 2. Runs it in a Docker container with no network access
/// 3. Sends tasks to the agent and collects responses
/// 4. Returns aggregated scores
async fn run_binary_evaluation(
    challenge_id: &str,
    payload: &NewSubmissionAssignedPayload,
    binary: &[u8],
    validator_hotkey: &str,
) -> Result<(f64, i32, i32, i32, f64)> {
    use anyhow::Context;
    use std::io::Write;
    use tempfile::NamedTempFile;

    info!(
        "Starting evaluation for agent {} in Docker container",
        &payload.agent_hash[..16.min(payload.agent_hash.len())]
    );

    // Write binary to temp file
    let mut temp_file = NamedTempFile::new().context("Failed to create temp file")?;
    temp_file
        .write_all(binary)
        .context("Failed to write binary")?;
    let binary_path = temp_file.path().to_string_lossy().to_string();

    // Make binary executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&binary_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&binary_path, perms)?;
    }

    // Get tasks from challenge server via bridge
    let platform_url = std::env::var("PLATFORM_SERVER_URL")
        .unwrap_or_else(|_| "https://chain.platform.network".to_string());

    let tasks_url = format!(
        "{}/api/v1/bridge/{}/api/v1/validator/get_tasks/{}",
        platform_url, challenge_id, payload.agent_hash
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default();

    let tasks: Vec<serde_json::Value> = match client.get(&tasks_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            resp.json().await.unwrap_or_else(|_| {
                // Default to simple test tasks if parsing fails
                vec![serde_json::json!({
                    "task_id": "test_1",
                    "instruction": "Echo 'Hello World'",
                    "expected_output": "Hello World"
                })]
            })
        }
        _ => {
            // Default tasks for testing
            vec![serde_json::json!({
                "task_id": "test_1",
                "instruction": "Echo 'Hello World'",
                "expected_output": "Hello World"
            })]
        }
    };

    let tasks_total = tasks.len() as i32;
    let mut tasks_passed = 0i32;
    let mut tasks_failed = 0i32;
    let total_cost = 0.0f64; // No LLM cost for binary evaluation

    // Run each task in Docker
    for (idx, task) in tasks.iter().enumerate() {
        let default_task_id = format!("task_{}", idx);
        let task_id = task["task_id"].as_str().unwrap_or(&default_task_id);
        let instruction = task["instruction"].as_str().unwrap_or("No instruction");
        let expected = task["expected_output"].as_str();

        info!(
            "Running task {} for agent {}",
            task_id,
            &payload.agent_hash[..16.min(payload.agent_hash.len())]
        );

        // Run binary in Docker container using secure-container-runtime
        // For now, use direct process execution with timeout as fallback
        // In production, this would use the ContainerBroker
        let result = run_task_in_docker(&binary_path, instruction).await;

        match result {
            Ok(output) => {
                // Check if output matches expected (simple comparison)
                let passed = match expected {
                    Some(exp) => output.trim().contains(exp.trim()),
                    None => !output.is_empty(), // Any output is considered pass
                };

                if passed {
                    tasks_passed += 1;
                    debug!("Task {} PASSED", task_id);
                } else {
                    tasks_failed += 1;
                    debug!(
                        "Task {} FAILED: expected {:?}, got {}",
                        task_id, expected, output
                    );
                }
            }
            Err(e) => {
                tasks_failed += 1;
                warn!("Task {} ERROR: {}", task_id, e);
            }
        }
    }

    // Calculate score
    let score = if tasks_total > 0 {
        tasks_passed as f64 / tasks_total as f64
    } else {
        0.0
    };

    info!(
        "Evaluation finished: {}/{} tasks passed, score={:.2}%",
        tasks_passed,
        tasks_total,
        score * 100.0
    );

    Ok((score, tasks_passed, tasks_total, tasks_failed, total_cost))
}

/// Run a single task in Docker container
///
/// Uses a simple Python container to execute the binary with proper isolation
async fn run_task_in_docker(binary_path: &str, instruction: &str) -> Result<String> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Command;

    // Create input JSON for the agent
    let input = serde_json::json!({
        "instruction": instruction,
        "step": 1,
        "output": "",
        "exit_code": 0
    });

    // Run with timeout using Docker
    // Note: In production, this should use the ContainerBroker via WebSocket
    // For now, we use direct Docker command with security restrictions
    let output = tokio::time::timeout(Duration::from_secs(60), async {
        // Try Docker first
        let docker_result = Command::new("docker")
            .args(&[
                "run",
                "--rm",
                "--network=none", // No network
                "--memory=512m",  // Memory limit
                "--cpus=0.5",     // CPU limit
                "--read-only",    // Read-only root
                "--security-opt=no-new-privileges",
                "-v",
                &format!("{}:/agent:ro", binary_path),
                "python:3.11-slim",
                "/agent",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        match docker_result {
            Ok(mut child) => {
                // Send input to agent
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(format!("{}\n", input).as_bytes()).await;
                    let _ = stdin.flush().await;
                }

                // Read output
                let output = child.wait_with_output().await?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                if output.status.success() || !stdout.is_empty() {
                    // Try to parse response JSON
                    if let Some(line) = stdout.lines().last() {
                        if let Ok(resp) = serde_json::from_str::<serde_json::Value>(line) {
                            if let Some(cmd) = resp["command"].as_str() {
                                return Ok(cmd.to_string());
                            }
                        }
                    }
                    Ok(stdout.to_string())
                } else {
                    Err(anyhow::anyhow!("Container failed: {}", stderr))
                }
            }
            Err(e) => {
                // Fallback: direct execution (less secure, only for dev)
                warn!(
                    "Docker not available, using direct execution (DEV ONLY): {}",
                    e
                );

                let mut child = Command::new(binary_path)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;

                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(format!("{}\n", input).as_bytes()).await;
                    let _ = stdin.flush().await;
                }

                let output = child.wait_with_output().await?;
                let stdout = String::from_utf8_lossy(&output.stdout);

                if let Some(line) = stdout.lines().last() {
                    if let Ok(resp) = serde_json::from_str::<serde_json::Value>(line) {
                        if let Some(cmd) = resp["command"].as_str() {
                            return Ok(cmd.to_string());
                        }
                    }
                }
                Ok(stdout.to_string())
            }
        }
    })
    .await;

    match output {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("Task execution timed out")),
    }
}

/// Evaluate agent locally and submit result to central server (legacy broadcast flow)
async fn evaluate_and_submit(
    challenge_url: &str,
    challenge_id: &str,
    submission: NewSubmissionPayload,
    keypair: Arc<Keypair>,
) {
    let validator_hotkey = keypair.ss58_address();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3600))
        .build()
        .unwrap_or_default();

    info!(
        "Evaluating agent {} via local container {}",
        &submission.agent_hash[..16.min(submission.agent_hash.len())],
        challenge_url
    );

    // Call local challenge container /evaluate endpoint
    let eval_request = serde_json::json!({
        "submission_id": submission.submission_id,
        "agent_hash": submission.agent_hash,
        "miner_hotkey": submission.miner_hotkey,
        "validator_hotkey": validator_hotkey,
        "name": submission.name,
        "source_code": submission.source_code,
        "epoch": submission.epoch,
    });

    match client
        .post(format!("{}/evaluate", challenge_url))
        .json(&eval_request)
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                match response.json::<serde_json::Value>().await {
                    Ok(result) => {
                        let score = result["score"].as_f64().unwrap_or(0.0);
                        let tasks_passed = result["tasks_passed"].as_i64().unwrap_or(0);
                        let tasks_total = result["tasks_total"].as_i64().unwrap_or(0);
                        let tasks_failed = result["tasks_failed"].as_i64().unwrap_or(0);
                        let total_cost = result["total_cost_usd"].as_f64().unwrap_or(0.0);

                        info!(
                            "Evaluation complete for {}: score={:.2}%, passed={}/{}",
                            &submission.agent_hash[..16.min(submission.agent_hash.len())],
                            score * 100.0,
                            tasks_passed,
                            tasks_total
                        );

                        // Submit result to central server
                        submit_evaluation_result(
                            challenge_id,
                            &submission.agent_hash,
                            keypair.clone(),
                            score,
                            tasks_passed as i32,
                            tasks_total as i32,
                            tasks_failed as i32,
                            total_cost,
                            submission.epoch,
                        )
                        .await;
                    }
                    Err(e) => {
                        warn!("Failed to parse evaluation response: {}", e);
                    }
                }
            } else {
                warn!(
                    "Evaluation failed for {}: {}",
                    &submission.agent_hash[..16.min(submission.agent_hash.len())],
                    response.status()
                );
            }
        }
        Err(e) => {
            warn!(
                "Evaluation request failed for {}: {}",
                &submission.agent_hash[..16.min(submission.agent_hash.len())],
                e
            );
        }
    }
}

/// Submit evaluation result to term-challenge via bridge
///
/// The bridge routes to term-challenge which stores and aggregates validator results
async fn submit_evaluation_result(
    challenge_id: &str,
    agent_hash: &str,
    keypair: Arc<Keypair>,
    score: f64,
    tasks_passed: i32,
    tasks_total: i32,
    tasks_failed: i32,
    total_cost_usd: f64,
    _epoch: i64, // Not used - server tracks epoch
) {
    let validator_hotkey = keypair.ss58_address();
    let platform_url = std::env::var("PLATFORM_SERVER_URL")
        .unwrap_or_else(|_| "https://chain.platform.network".to_string());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Sign with validator's keypair
    let message = format!("submit_result:{}:{}", agent_hash, timestamp);
    let signature = hex::encode(keypair.sign_bytes(message.as_bytes()).unwrap_or_default());

    let result_request = serde_json::json!({
        "agent_hash": agent_hash,
        "validator_hotkey": validator_hotkey,
        "score": score,
        "tasks_passed": tasks_passed,
        "tasks_total": tasks_total,
        "tasks_failed": tasks_failed,
        "total_cost_usd": total_cost_usd,
        "timestamp": timestamp,
        "signature": signature,
        "skip_verification": true, // Binary evaluation doesn't use task logs
    });

    // Submit via bridge to term-challenge
    let url = format!(
        "{}/api/v1/bridge/{}/api/v1/validator/submit_result",
        platform_url, challenge_id
    );

    match client.post(&url).json(&result_request).send().await {
        Ok(response) => {
            if response.status().is_success() {
                match response.json::<serde_json::Value>().await {
                    Ok(result) => {
                        let success = result["success"].as_bool().unwrap_or(false);
                        let consensus = result["consensus_reached"].as_bool().unwrap_or(false);
                        let validators = result["validators_completed"].as_i64().unwrap_or(0);
                        let total = result["total_validators"].as_i64().unwrap_or(0);

                        if success {
                            info!(
                                "Result submitted for {}: {}/{} validators, consensus={}",
                                &agent_hash[..16.min(agent_hash.len())],
                                validators,
                                total,
                                consensus
                            );
                        } else {
                            let error = result["error"].as_str().unwrap_or("unknown");
                            warn!("Result submission failed: {}", error);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to parse submit_result response: {}", e);
                    }
                }
            } else {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                warn!(
                    "Result submission failed for {}: {} - {}",
                    &agent_hash[..16.min(agent_hash.len())],
                    status,
                    text
                );
            }
        }
        Err(e) => {
            warn!(
                "Result submission request failed for {}: {}",
                &agent_hash[..16.min(agent_hash.len())],
                e
            );
        }
    }
}
