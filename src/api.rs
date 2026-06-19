use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};

use crate::mining::MiningCoordinator;

pub struct ApiServer {
    coordinator: Arc<MiningCoordinator>,
    port: u16,
    bind_address: String,
    cors_origin: Option<String>,
}

impl ApiServer {
    pub fn new(coordinator: MiningCoordinator, port: u16, bind_address: String, cors_origin: Option<String>) -> Self {
        ApiServer {
            coordinator: Arc::new(coordinator),
            port,
            bind_address,
            cors_origin,
        }
    }

    pub async fn serve(self) -> Result<(), Box<dyn std::error::Error>> {
        let cors = match self.cors_origin.as_deref() {
            None => CorsLayer::new(),
            Some("*") => CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([axum::http::Method::GET]),
            Some(origin) => {
                let header_val = origin
                    .parse::<HeaderValue>()
                    .map_err(|e| format!("invalid --cors-origin value: {e}"))?;
                CorsLayer::new()
                    .allow_origin(header_val)
                    .allow_methods([axum::http::Method::GET])
            }
        };

        let app = Router::new()
            .route("/", get(root))
            .route("/api/v1/stats", get(get_stats))
            .route("/api/v1/health", get(health_check))
            .route("/api/stats", get(get_stats))
            .route("/api/health", get(health_check))
            .layer(cors)
            .with_state(self.coordinator);

        let addr = format!("{}:{}", self.bind_address, self.port);
        println!("API server listening on http://{}", addr);
        println!("  Stats:      http://{}/api/v1/stats", addr);
        println!("  Health:     http://{}/api/v1/health", addr);
        
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;

        Ok(())
    }
}

async fn root() -> impl IntoResponse {
    Json(json!({
        "name": "CPUNet Miner API",
        "version": env!("CARGO_PKG_VERSION"),
        "api_version": "v1",
        "endpoints": {
            "stats": "/api/v1/stats",
            "health": "/api/v1/health"
        },
        "deprecated": {
            "stats": "/api/stats (use /api/v1/stats)",
            "health": "/api/health (use /api/v1/health)"
        }
    }))
}

async fn get_stats(State(coordinator): State<Arc<MiningCoordinator>>) -> Response {
    match coordinator.get_stats_snapshot() {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": format!("Failed to get stats: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn health_check(State(coordinator): State<Arc<MiningCoordinator>>) -> impl IntoResponse {
    let status = if let Ok(snapshot) = coordinator.get_stats_snapshot() {
        match snapshot.connection.status {
            crate::stats::ConnectionStatus::Connected => "healthy",
            crate::stats::ConnectionStatus::Connecting => "degraded",
            _ => "unhealthy",
        }
    } else {
        "unhealthy"
    };

    Json(json!({
        "status": status,
        "timestamp": chrono::Utc::now().to_rfc3339()
    }))
}