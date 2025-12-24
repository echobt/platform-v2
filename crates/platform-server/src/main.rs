//! Platform Server - Challenge Orchestrator for Subnet Owners
//!
//! This server is the SINGLE SOURCE OF TRUTH for the Platform subnet.
//! Run ONLY by the subnet owner at chain.platform.network
//!
//! Architecture:
//! ```
//! Platform Server (this)
//!  ├── Control Plane API (REST)
//!  ├── Data API + Rule Engine
//!  ├── WebSocket for validators
//!  └── PostgreSQL databases
//!
//! Validators connect via WebSocket to receive events and query Data API.
//! Challenges run as always-on containers, accessing DB only via Data API.
//! ```

mod api;
mod challenge_proxy;
mod data_api;
mod db;
mod models;
mod observability;
mod rule_engine;
mod state;
mod websocket;

use crate::challenge_proxy::ChallengeProxy;
use crate::observability::init_sentry;
use crate::state::AppState;
use crate::websocket::handler::ws_handler;
use axum::{
    routing::{any, delete, get, post, put},
    Router,
};
use clap::Parser;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "platform-server")]
#[command(about = "Platform Network - Central API Server (Subnet Owner Only)")]
struct Args {
    /// Server port
    #[arg(short, long, default_value = "8080", env = "PORT")]
    port: u16,

    /// Server host
    #[arg(long, default_value = "0.0.0.0", env = "HOST")]
    host: String,

    /// Challenge ID
    #[arg(short, long, env = "CHALLENGE_ID")]
    challenge_id: String,

    /// Challenge Docker image (e.g., term-challenge:latest)
    #[arg(long, env = "CHALLENGE_IMAGE")]
    challenge_image: Option<String>,

    /// Challenge container URL (if already running)
    #[arg(long, env = "CHALLENGE_URL")]
    challenge_url: Option<String>,

    /// Owner hotkey (required - only owner can run this server)
    #[arg(long, env = "OWNER_HOTKEY")]
    owner_hotkey: String,

    /// PostgreSQL base URL
    #[arg(
        long,
        env = "DATABASE_URL",
        default_value = "postgres://postgres:postgres@localhost:5432"
    )]
    database_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("platform_server=debug".parse().unwrap())
                .add_directive("info".parse().unwrap()),
        )
        .init();

    // Initialize Sentry for error tracking (optional - set SENTRY_DSN env var)
    let _sentry_guard = init_sentry();
    if _sentry_guard.is_some() {
        info!("Sentry error tracking enabled");
    }

    let args = Args::parse();

    info!("╔══════════════════════════════════════════════════════════════╗");
    info!("║       Platform Server - Single Source of Truth               ║");
    info!("║              (Subnet Owner Only)                             ║");
    info!("╚══════════════════════════════════════════════════════════════╝");
    info!("");
    info!("  Challenge ID: {}", args.challenge_id);
    info!(
        "  Owner hotkey: {}...",
        &args.owner_hotkey[..16.min(args.owner_hotkey.len())]
    );
    info!("  Listening on: {}:{}", args.host, args.port);

    // Initialize database
    let db = db::init_challenge_db(&args.database_url, &args.challenge_id).await?;
    info!(
        "  Database: platform_{}",
        args.challenge_id.replace('-', "_")
    );

    // Initialize challenge proxy (connects to challenge Docker container)
    let challenge_url = args
        .challenge_url
        .or_else(|| {
            args.challenge_image
                .as_ref()
                .map(|_| "http://localhost:8081".to_string())
        })
        .unwrap_or_else(|| "http://localhost:8081".to_string());

    let challenge_proxy = Arc::new(ChallengeProxy::new(&args.challenge_id, &challenge_url));
    info!("  Challenge URL: {}", challenge_url);

    // Create application state
    let state = Arc::new(AppState::new(
        db,
        args.challenge_id.clone(),
        Some(args.owner_hotkey.clone()),
        challenge_proxy.clone(),
    ));

    // Build router
    let app = Router::new()
        // Health check
        .route("/health", get(health_check))
        // WebSocket for validators
        .route("/ws", get(ws_handler))
        // === CONTROL PLANE API ===
        .route("/api/v1/auth", post(api::auth::authenticate))
        .route("/api/v1/validators", get(api::validators::list_validators))
        .route(
            "/api/v1/validators",
            post(api::validators::register_validator),
        )
        .route(
            "/api/v1/validators/:hotkey",
            get(api::validators::get_validator),
        )
        .route(
            "/api/v1/validators/heartbeat",
            post(api::validators::heartbeat),
        )
        .route(
            "/api/v1/network/state",
            get(api::challenges::get_network_state),
        )
        .route("/api/v1/config", get(api::challenges::get_config_current))
        .route(
            "/api/v1/config",
            post(api::challenges::update_config_current),
        )
        // === DATA API (with Claim/Lease) ===
        .route("/api/v1/data/tasks", get(data_api::list_tasks))
        .route("/api/v1/data/tasks/claim", post(data_api::claim_task))
        .route(
            "/api/v1/data/tasks/:task_id/renew",
            post(data_api::renew_task),
        )
        .route("/api/v1/data/tasks/:task_id/ack", post(data_api::ack_task))
        .route(
            "/api/v1/data/tasks/:task_id/fail",
            post(data_api::fail_task),
        )
        .route("/api/v1/data/results", post(data_api::write_result))
        .route(
            "/api/v1/data/results/:agent_hash",
            get(data_api::get_results),
        )
        .route("/api/v1/data/snapshot", get(data_api::get_snapshot))
        // === SUBMISSIONS & EVALUATIONS ===
        .route(
            "/api/v1/submissions",
            get(api::submissions::list_submissions),
        )
        .route("/api/v1/submissions", post(api::submissions::submit_agent))
        .route(
            "/api/v1/submissions/:id",
            get(api::submissions::get_submission),
        )
        .route(
            "/api/v1/submissions/:id/source",
            get(api::submissions::get_submission_source),
        )
        .route(
            "/api/v1/evaluations",
            post(api::evaluations::submit_evaluation),
        )
        .route(
            "/api/v1/evaluations/:agent_hash",
            get(api::evaluations::get_evaluations),
        )
        .route(
            "/api/v1/leaderboard",
            get(api::leaderboard::get_leaderboard),
        )
        .route(
            "/api/v1/leaderboard/:agent_hash",
            get(api::leaderboard::get_agent_rank),
        )
        // === CHALLENGE PROXY (routes to challenge container) ===
        .route(
            "/api/v1/challenge/*path",
            any(challenge_proxy::proxy_handler),
        )
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state);

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!("");
    info!("╔══════════════════════════════════════════════════════════════╗");
    info!(
        "║  Server ready at http://{}                          ║",
        addr
    );
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  Control Plane: /api/v1/{{auth,validators,config}}           ║");
    info!("║  Data API:      /api/v1/data/{{tasks,results,snapshot}}      ║");
    info!(
        "║  WebSocket:     ws://{}/ws                          ║",
        addr
    );
    info!("╚══════════════════════════════════════════════════════════════╝");

    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_check() -> &'static str {
    "OK"
}
