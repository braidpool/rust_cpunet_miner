use axum::{
    extract::Extension,
    routing::get,
    Router,
    Json,
};
use std::{net::SocketAddr, sync::Arc};

use crate::mining::MiningCoordinator;

async fn stats(
    Extension(miner): Extension<Arc<MiningCoordinator>>,
) -> Json<serde_json::Value> {
    Json(miner.info())
}

pub async fn run_api(
    miner: Arc<MiningCoordinator>,
    port: u16,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/stats", get(stats))
        .layer(Extension(miner));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("HTTP API running on http://{}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
