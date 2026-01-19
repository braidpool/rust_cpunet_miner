mod api;
mod cli;
mod hashing;
mod mining;
mod stats;
mod stratum;

use anyhow::Result;
use cli::Cli;
use mining::MiningCoordinator;
use stratum::StratumClient;
use api::ApiServer;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = cli.into_config()?;

    if config.benchmark {
        mining::benchmark(&config).await?;
        return Ok(());
    }

    let api_port = config.api_port;
    let api_bind = config.api_bind.clone();
    let coordinator = MiningCoordinator::new(config.clone())?;
    
    // API server in background
    let api_coordinator = coordinator.clone();
    let api_handle = tokio::spawn(async move {
        let api_server = ApiServer::new(api_coordinator, api_port, api_bind);
        if let Err(e) = api_server.serve().await {
            eprintln!("API server error: {}", e);
        }
    });

    match StratumClient::connect(config, coordinator).await {
        Ok(mut stratum) => {
            stratum.run().await
        }
        Err(e) => {
            // Connection failed, but keep API server running
            eprintln!("Warning: Failed to connect to pool: {}", e);
            eprintln!("API server is still running on http://{}:{}", config.api_bind, api_port);
            api_handle.await.map_err(|e| anyhow::anyhow!("API server task failed: {}", e))?;
            Ok(())
        }
    }
}