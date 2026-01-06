mod cli;
mod hashing;
mod mining;
mod stratum;

use anyhow::Result;
use cli::Cli;
use mining::MiningCoordinator;
use stratum::StratumClient;
use std::sync::Arc;
mod http_api;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = cli.into_config()?;

    if config.benchmark {
        mining::benchmark(&config).await?;
        return Ok(());
    }

    let coordinator = MiningCoordinator::new(config.clone())?;

    // start HTTP API
    if let Some(port) = config.http_port {
        let coord_clone = Arc::new(coordinator.clone()); 
        tokio::spawn(async move {
            if let Err(e) = http_api::run_api(coord_clone, port).await {
                eprintln!("HTTP API error: {:#?}", e);
            }
        });
    }

    let mut stratum = StratumClient::connect(config, coordinator).await?; 
    stratum.run().await
}
