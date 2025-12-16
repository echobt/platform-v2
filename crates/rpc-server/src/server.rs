//! RPC Server implementation

use crate::handlers::*;
use crate::jsonrpc::{JsonRpcRequest, JsonRpcResponse, RpcHandler, PARSE_ERROR};
use crate::RpcState;
use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use parking_lot::RwLock;
use platform_challenge_sdk::RouteRequest;
use platform_core::ChainState;
use platform_subnet_manager::BanList;
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{debug, info, trace};

/// RPC server configuration
#[derive(Clone, Debug)]
pub struct RpcConfig {
    /// Listen address
    pub addr: SocketAddr,
    /// Subnet UID
    pub netuid: u16,
    /// Subnet name
    pub name: String,
    /// Minimum stake for validators
    pub min_stake: u64,
    /// Enable CORS
    pub cors_enabled: bool,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:8080".parse().unwrap(),
            netuid: 1,
            name: "Mini-Chain".to_string(),
            min_stake: 1_000_000_000_000, // 1000 TAO
            cors_enabled: true,
        }
    }
}

/// RPC Server
pub struct RpcServer {
    config: RpcConfig,
    state: Arc<RpcState>,
    rpc_handler: Arc<RpcHandler>,
}

impl RpcServer {
    /// Create a new RPC server
    pub fn new(
        config: RpcConfig,
        chain_state: Arc<RwLock<ChainState>>,
        bans: Arc<RwLock<BanList>>,
    ) -> Self {
        let state = Arc::new(RpcState::new(
            chain_state.clone(),
            bans,
            config.netuid,
            config.name.clone(),
            config.min_stake,
        ));

        let rpc_handler = Arc::new(RpcHandler::new(chain_state, config.netuid));

        Self {
            config,
            state,
            rpc_handler,
        }
    }

    /// Get the RPC handler (to update peers, etc.)
    pub fn rpc_handler(&self) -> Arc<RpcHandler> {
        self.rpc_handler.clone()
    }

    /// Build the router
    pub fn router(&self) -> Router {
        let rpc_handler = self.rpc_handler.clone();

        let mut router = Router::new()
            // JSON-RPC 2.0 endpoint (Substrate-style)
            .route(
                "/",
                post({
                    let handler = rpc_handler.clone();
                    move |body: Json<Value>| {
                        let handler = handler.clone();
                        async move { jsonrpc_handler(body, handler).await }
                    }
                }),
            )
            // Also support /rpc for explicit path
            .route(
                "/rpc",
                post({
                    let handler = rpc_handler.clone();
                    move |body: Json<Value>| {
                        let handler = handler.clone();
                        async move { jsonrpc_handler(body, handler).await }
                    }
                }),
            )
            // Keep simple health endpoint for load balancers
            .route("/health", get(health_handler))
            // Challenge custom routes: /challenge/{id}/*path
            .route("/challenge/:challenge_id/*path", {
                let handler = rpc_handler.clone();
                axum::routing::any(
                    move |axum::extract::Path((challenge_id, path)): axum::extract::Path<(
                        String,
                        String,
                    )>,
                          method: axum::http::Method,
                          axum::extract::Query(query): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >,
                          headers: axum::http::HeaderMap,
                          body: Option<Json<Value>>| {
                        let handler = handler.clone();
                        async move {
                            challenge_route_handler(
                                handler,
                                challenge_id,
                                path,
                                method.as_str().to_string(),
                                query,
                                headers,
                                body.map(|b| b.0).unwrap_or(Value::Null),
                            )
                            .await
                        }
                    },
                )
            })
            // Challenge route without subpath: /challenge/{id}
            .route("/challenge/:challenge_id", {
                let handler = rpc_handler.clone();
                axum::routing::any(
                    move |axum::extract::Path(challenge_id): axum::extract::Path<String>,
                          method: axum::http::Method,
                          axum::extract::Query(query): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >,
                          headers: axum::http::HeaderMap,
                          body: Option<Json<Value>>| {
                        let handler = handler.clone();
                        async move {
                            challenge_route_handler(
                                handler,
                                challenge_id,
                                "".to_string(),
                                method.as_str().to_string(),
                                query,
                                headers,
                                body.map(|b| b.0).unwrap_or(Value::Null),
                            )
                            .await
                        }
                    },
                )
            })
            .with_state(self.state.clone())
            .layer(TraceLayer::new_for_http());

        if self.config.cors_enabled {
            router = router.layer(
                CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods(Any)
                    .allow_headers(Any),
            );
        }

        router
    }

    /// Start the server
    pub async fn run(self) -> anyhow::Result<()> {
        let addr = self.config.addr;
        let router = self.router();

        info!("RPC server starting on {}", addr);

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, router).await?;

        Ok(())
    }

    /// Start the server in a background task
    pub fn spawn(self) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        tokio::spawn(async move { self.run().await })
    }

    /// Get the listen address
    pub fn addr(&self) -> SocketAddr {
        self.config.addr
    }
}

/// Handler for challenge custom routes
async fn challenge_route_handler(
    handler: Arc<RpcHandler>,
    challenge_id: String,
    path: String,
    method: String,
    query: HashMap<String, String>,
    headers: axum::http::HeaderMap,
    body: Value,
) -> impl IntoResponse {
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", path)
    };

    trace!("Challenge route: {} {} {}", challenge_id, method, path);

    // Check if challenge has registered routes
    // Clone the routes while holding the lock, then drop it
    let challenge_routes = {
        let routes = handler.challenge_routes.read();
        let result = routes.get(&challenge_id).cloned();

        if result.is_none() {
            // Try to find by name
            let chain = handler.chain_state.read();
            let actual_id = chain
                .challenges
                .values()
                .find(|c| c.name == challenge_id)
                .map(|c| c.id.to_string());
            drop(chain);

            actual_id.and_then(|id| routes.get(&id).cloned())
        } else {
            result
        }
    };

    let challenge_routes = match challenge_routes {
        Some(r) => r,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "challenge_not_found",
                    "message": format!("Challenge '{}' not found or has no routes", challenge_id)
                })),
            );
        }
    };

    // Find matching route
    let mut matched_route = None;
    let mut params = HashMap::new();

    for route in &challenge_routes {
        if let Some(p) = route.matches(&method, &path) {
            matched_route = Some(route.clone());
            params = p;
            break;
        }
    }

    let route = match matched_route {
        Some(r) => r,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "route_not_found",
                    "message": format!("No route matches {} {}", method, path),
                    "availableRoutes": challenge_routes.iter()
                        .map(|r| format!("{} {}", r.method.as_str(), r.path))
                        .collect::<Vec<_>>()
                })),
            );
        }
    };

    // Build request
    let mut headers_map = HashMap::new();
    for (key, value) in headers.iter() {
        if let Ok(v) = value.to_str() {
            headers_map.insert(key.as_str().to_string(), v.to_string());
        }
    }

    let request = RouteRequest {
        method,
        path,
        params,
        query,
        headers: headers_map,
        body,
        auth_hotkey: None,
    };

    // Call the route handler if registered
    let maybe_handler = handler.route_handler.read().clone();
    match maybe_handler {
        Some(handle) => {
            let response = handle(challenge_id.clone(), request).await;
            (
                StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(response.body),
            )
        }
        None => {
            // No handler registered - return info about the route
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "route": {
                        "method": route.method.as_str(),
                        "path": route.path,
                        "description": route.description,
                    },
                    "message": "Route handler not registered. Use RPC to interact with challenges.",
                    "hint": "Use JSON-RPC method 'challenge_callRoute' to invoke this route"
                })),
            )
        }
    }
}

/// JSON-RPC 2.0 request handler
async fn jsonrpc_handler(
    Json(body): Json<Value>,
    handler: Arc<RpcHandler>,
) -> (StatusCode, Json<JsonRpcResponse>) {
    // Handle batch requests
    if let Some(arr) = body.as_array() {
        // For batch, we'd return an array - for now just handle first
        if let Some(first) = arr.first() {
            return handle_single_request(first.clone(), &handler);
        }
        return (
            StatusCode::BAD_REQUEST,
            Json(JsonRpcResponse::error(
                Value::Null,
                PARSE_ERROR,
                "Empty batch",
            )),
        );
    }

    handle_single_request(body, &handler)
}

fn handle_single_request(body: Value, handler: &RpcHandler) -> (StatusCode, Json<JsonRpcResponse>) {
    // Parse the request
    let req: JsonRpcRequest = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(JsonRpcResponse::error(
                    Value::Null,
                    PARSE_ERROR,
                    format!("Parse error: {}", e),
                )),
            );
        }
    };

    // Validate jsonrpc version
    if req.jsonrpc != "2.0" {
        return (
            StatusCode::BAD_REQUEST,
            Json(JsonRpcResponse::error(
                req.id,
                PARSE_ERROR,
                "Invalid JSON-RPC version",
            )),
        );
    }

    // Handle the request
    let response = handler.handle(req);

    let status = if response.error.is_some() {
        StatusCode::OK // JSON-RPC errors still return 200
    } else {
        StatusCode::OK
    };

    (status, Json(response))
}

/// Quick helper to create and start a server
pub async fn start_rpc_server(
    addr: &str,
    chain_state: Arc<RwLock<ChainState>>,
    bans: Arc<RwLock<BanList>>,
    netuid: u16,
    name: &str,
) -> anyhow::Result<()> {
    let config = RpcConfig {
        addr: addr.parse()?,
        netuid,
        name: name.to_string(),
        ..Default::default()
    };

    let server = RpcServer::new(config, chain_state, bans);
    server.run().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_core::{Keypair, NetworkConfig};

    #[test]
    fn test_rpc_config_default() {
        let config = RpcConfig::default();
        assert_eq!(config.netuid, 1);
        assert!(config.cors_enabled);
    }

    #[tokio::test]
    async fn test_rpc_server_creation() {
        let kp = Keypair::generate();
        let state = Arc::new(RwLock::new(ChainState::new(
            kp.hotkey(),
            NetworkConfig::default(),
        )));
        let bans = Arc::new(RwLock::new(BanList::new()));

        let config = RpcConfig::default();
        let server = RpcServer::new(config, state, bans);

        let router = server.router();
        // Router created successfully
    }
}
