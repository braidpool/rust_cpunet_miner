use std::num::NonZeroUsize;
use std::thread;

use anyhow::{anyhow, Result};
use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Stratum pool URL, e.g. stratum+tcp://localhost:3333
    #[arg(short = 'o', long = "url", value_name = "URL")]
    pub pool_url: Option<String>,

    /// Username and password in USER:PASS format
    #[arg(short = 'O', long = "userpass", value_name = "USER:PASS")]
    pub credentials: Option<String>,

    /// Number of mining threads to run
    #[arg(short = 't', long = "threads", value_name = "N")]
    pub threads: Option<usize>,

    /// Run a standalone hashing benchmark instead of connecting to a pool
    #[arg(long)]
    pub benchmark: bool,

    /// Enable verbose debug output
    #[arg(short = 'D', long = "debug")]
    pub debug: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub pool_url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub threads: NonZeroUsize,
    pub benchmark: bool,
    pub debug: bool,
}

impl Cli {
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }

    pub fn into_config(self) -> Result<Config> {
        let threads = self
            .threads
            .map(NonZeroUsize::new)
            .flatten()
            .or_else(|| thread::available_parallelism().ok())
            .unwrap_or_else(|| NonZeroUsize::new(1).unwrap());

        let (username, password) = match self.credentials {
            Some(creds) => {
                let mut parts = creds.splitn(2, ':');
                let user = parts
                    .next()
                    .map(|s| s.to_string())
                    .ok_or_else(|| anyhow!("missing username in credentials"))?;
                let pass = parts.next().map(|s| s.to_string()).unwrap_or_default();
                (Some(user), Some(pass))
            }
            None => (None, None),
        };

        if !self.benchmark {
            if self.pool_url.is_none() {
                return Err(anyhow!(
                    "pool URL (-o/--url) is required unless --benchmark is set"
                ));
            }
            if username.is_none() {
                return Err(anyhow!(
                    "credentials (-O/--userpass) are required unless --benchmark is set"
                ));
            }
        }

        Ok(Config {
            pool_url: self.pool_url,
            username,
            password,
            threads,
            benchmark: self.benchmark,
            debug: self.debug,
        })
    }
}
