mod cli;
mod hashing;
mod mining;
mod stratum;

use anyhow::Result;
use cli::Cli;
use mining::MiningCoordinator;
use stratum::StratumClient;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = cli.into_config()?;

    if config.benchmark {
        mining::benchmark(&config).await?;
        return Ok(());
    }

    let coordinator = MiningCoordinator::new(config.clone())?;
    let mut stratum = StratumClient::connect(config, coordinator).await?;
    stratum.run().await
}
