use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::mining::{JobTemplate, ShareSubmission, Subscription};

#[derive(Clone)]
pub struct MiningStats {
    inner: Arc<StatsInner>,
}

struct StatsInner {
    start_time: Instant,
    hashes_computed: AtomicU64,
    shares_submitted: AtomicU64,
    shares_accepted: AtomicU64,
    shares_rejected: AtomicU64,
    blocks_found: AtomicU64,
    state: Mutex<StatsState>,
    share_history_size: usize,
}

struct StatsState {
    current_job: Option<JobTemplate>,
    subscription: Option<Subscription>,
    recent_shares: Vec<ShareRecord>,
    connection_status: ConnectionStatus,
    pool_url: String,
    username: String,
    threads: usize,
    difficulty: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareRecord {
    pub timestamp: String,
    pub job_id: String,
    pub nonce: String,
    pub extranonce2: String,
    pub hash: String,
    pub is_block_candidate: bool,
    pub accepted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MiningStatsSnapshot {
    pub uptime_seconds: u64,
    pub hashrate: HashrateInfo,
    pub shares: ShareStats,
    pub connection: ConnectionInfo,
    pub current_job: Option<JobInfo>,
    pub worker_threads: Vec<WorkerInfo>,
    pub recent_shares: Vec<ShareRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HashrateInfo {
    pub current_khash_s: f64,
    pub total_hashes: u64,
    pub threads: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareStats {
    pub submitted: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub acceptance_rate: f64,
    pub blocks_found: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionInfo {
    pub status: ConnectionStatus,
    pub pool_url: String,
    pub username: String,
    pub uptime_seconds: u64,
    pub difficulty: f64,
    pub subscription: Option<SubscriptionInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionInfo {
    pub extranonce1: String,
    pub extranonce2_size: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInfo {
    pub job_id: String,
    pub clean_jobs: bool,
    pub prev_block_hash: String,
    pub merkle_branches: usize,
    pub network_target: String,
    pub compact_target: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerInfo {
    pub id: usize,
    pub status: String,
    pub hashes_computed: u64,
}

impl MiningStats {
    pub fn new(pool_url: String, username: String, threads: usize, share_history_size: usize) -> Self {
        MiningStats {
            inner: Arc::new(StatsInner {
                start_time: Instant::now(),
                hashes_computed: AtomicU64::new(0),
                shares_submitted: AtomicU64::new(0),
                shares_accepted: AtomicU64::new(0),
                shares_rejected: AtomicU64::new(0),
                blocks_found: AtomicU64::new(0),
                share_history_size,
                state: Mutex::new(StatsState {
                    current_job: None,
                    subscription: None,
                    recent_shares: Vec::new(),
                    connection_status: ConnectionStatus::Disconnected,
                    pool_url,
                    username,
                    threads,
                    difficulty: 1.0,
                }),
            }),
        }
    }

    #[allow(dead_code)]
    pub fn record_hashes(&self, count: u64) {
        self.inner.hashes_computed.fetch_add(count, Ordering::Relaxed);
    }

    pub fn record_share_submitted(&self, share: &ShareSubmission) {
        self.inner.shares_submitted.fetch_add(1, Ordering::Relaxed);
        if share.is_block_candidate {
            self.inner.blocks_found.fetch_add(1, Ordering::Relaxed);
        }

        let mut state = self.inner.state.lock().unwrap();
        let timestamp = chrono::Utc::now().to_rfc3339();
        state.recent_shares.push(ShareRecord {
            timestamp,
            job_id: share.job_id.clone(),
            nonce: share.nonce.clone(),
            extranonce2: share.extranonce2.clone(),
            hash: hex::encode(share.hash),
            is_block_candidate: share.is_block_candidate,
            accepted: false, // Will be updated when response received
        });

        // Keep only last N shares based on config
        if state.recent_shares.len() > self.inner.share_history_size {
            state.recent_shares.remove(0);
        }
    }

    pub fn record_share_accepted(&self) {
        self.inner.shares_accepted.fetch_add(1, Ordering::Relaxed);
        let mut state = self.inner.state.lock().unwrap();
        if let Some(last_share) = state.recent_shares.last_mut() {
            last_share.accepted = true;
        }
    }

    pub fn record_share_rejected(&self) {
        self.inner.shares_rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn update_job(&self, job: JobTemplate) {
        let mut state = self.inner.state.lock().unwrap();
        state.current_job = Some(job);
    }

    pub fn update_subscription(&self, subscription: Subscription) {
        let mut state = self.inner.state.lock().unwrap();
        state.subscription = Some(subscription);
    }

    #[allow(dead_code)]
    pub fn update_connection_status(&self, status: ConnectionStatus) {
        let mut state = self.inner.state.lock().unwrap();
        state.connection_status = status;
    }

    #[allow(dead_code)]
    pub fn update_difficulty(&self, difficulty: f64) {
        let mut state = self.inner.state.lock().unwrap();
        state.difficulty = difficulty;
    }

    pub fn get_snapshot(&self) -> MiningStatsSnapshot {
        let uptime = self.inner.start_time.elapsed();
        let uptime_seconds = uptime.as_secs();
        let hashes = self.inner.hashes_computed.load(Ordering::Relaxed);
        let submitted = self.inner.shares_submitted.load(Ordering::Relaxed);
        let accepted = self.inner.shares_accepted.load(Ordering::Relaxed);
        let rejected = self.inner.shares_rejected.load(Ordering::Relaxed);
        let blocks = self.inner.blocks_found.load(Ordering::Relaxed);

        let state = self.inner.state.lock().unwrap();

        let hashrate = if uptime_seconds > 0 {
            (hashes as f64) / (uptime.as_secs_f64() * 1000.0)
        } else {
            0.0
        };

        let acceptance_rate = if submitted > 0 {
            (accepted as f64 / submitted as f64) * 100.0
        } else {
            0.0
        };

        let current_job = state.current_job.as_ref().map(|job| JobInfo {
            job_id: job.job_id.clone(),
            clean_jobs: job.clean_jobs,
            prev_block_hash: hex::encode(job.prevhash.as_byte_array()),
            merkle_branches: job.merkle_branch.len(),
            network_target: hex::encode(job.network_target.to_be_bytes()),
            compact_target: format!("{:08x}", job.compact_target.to_consensus()),
        });

        let subscription = state.subscription.as_ref().map(|sub| SubscriptionInfo {
            extranonce1: hex::encode(&sub.extranonce1),
            extranonce2_size: sub.extranonce2_size,
        });

        let worker_threads = (0..state.threads)
            .map(|id| WorkerInfo {
                id,
                status: "mining".to_string(),
                hashes_computed: hashes / state.threads as u64,
            })
            .collect();

        MiningStatsSnapshot {
            uptime_seconds,
            hashrate: HashrateInfo {
                current_khash_s: hashrate,
                total_hashes: hashes,
                threads: state.threads,
            },
            shares: ShareStats {
                submitted,
                accepted,
                rejected,
                acceptance_rate,
                blocks_found: blocks,
            },
            connection: ConnectionInfo {
                status: state.connection_status.clone(),
                pool_url: state.pool_url.clone(),
                username: state.username.clone(),
                uptime_seconds,
                difficulty: state.difficulty,
                subscription,
            },
            current_job,
            worker_threads,
            recent_shares: state.recent_shares.clone(),
        }
    }

    pub fn get_json(&self) -> Result<String, serde_json::Error> {
        let snapshot = self.get_snapshot();
        serde_json::to_string_pretty(&snapshot)
    }
}