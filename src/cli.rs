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

    /// Divide the reported pool difficulty by this factor before submitting shares
    #[arg(
        short = 'f',
        long = "fudge",
        value_name = "FACTOR",
        default_value_t = 1.0
    )]
    pub fudge: f64,

    /// Port for the HTTP API server 
    #[arg(long = "api-port", value_name = "PORT", default_value_t = 8080)]
    pub api_port: u16,
    #[arg(long = "api-bind", value_name = "ADDRESS", default_value = "0.0.0.0")]
    pub api_bind: String,
    #[arg(long = "share-history", value_name = "COUNT", default_value_t = 100)]
    pub share_history_size: usize,
    #[arg(long = "bench-duration", value_name = "SECONDS", default_value_t = 5)]
    pub benchmark_duration: u64,
    #[arg(long = "pool-timeout", value_name = "SECONDS", default_value_t = 10)]
    pub pool_timeout: u64,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub pool_url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub threads: NonZeroUsize,
    pub benchmark: bool,
    pub debug: bool,
    pub fudge: f64,
    pub api_port: u16,
    pub api_bind: String,
    pub share_history_size: usize,
    pub benchmark_duration: u64,
    pub pool_timeout: u64,
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

        if self.fudge <= 0.0 || !self.fudge.is_finite() {
            return Err(anyhow!("fudge factor (-f/--fudge) must be positive"));
        }

        Ok(Config {
            pool_url: self.pool_url,
            username,
            password,
            threads,
            benchmark: self.benchmark,
            debug: self.debug,
            fudge: self.fudge,
            api_port: self.api_port,
            api_bind: self.api_bind,
            share_history_size: self.share_history_size,
            benchmark_duration: self.benchmark_duration,
            pool_timeout: self.pool_timeout,
        })
    }
}