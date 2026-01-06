use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bitcoin::block::Header as BlockHeader;
use bitcoin::block::Version;
use bitcoin::hash_types::{BlockHash, TxMerkleNode};
use bitcoin::hashes::{sha256, sha256d};
use bitcoin::pow::{CompactTarget, Target};
use bitcoin::BlockTime;
use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::Zero;
use serde::Serialize;
use serde_json::json;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::cli::Config;
use crate::hashing::{hash_from_midstate, midstate_from_prefix, CPUNET_SUFFIX};

#[derive(Clone)]
pub struct MiningCoordinator {
    inner: Arc<CoordinatorInner>,
}

struct CoordinatorInner {
    config: Config,
    shared: Arc<SharedState>,
    share_tx: UnboundedSender<ShareSubmission>,
    share_rx: Mutex<Option<UnboundedReceiver<ShareSubmission>>>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
}

#[derive(Clone, Debug)]
pub struct JobTemplate {
    pub job_id: String,
    pub version: Version,
    pub prevhash: BlockHash,
    pub coinbase1: Vec<u8>,
    pub coinbase2: Vec<u8>,
    pub merkle_branch: Vec<Vec<u8>>,
    pub compact_target: CompactTarget,
    pub ntime: BlockTime,
    pub clean_jobs: bool,
    pub network_target: Target,
}

#[derive(Clone, Debug)]
pub struct Subscription {
    pub extranonce1: Vec<u8>,
    pub extranonce2_size: usize,
}

#[derive(Clone, Debug)]
pub struct ShareSubmission {
    pub job_id: String,
    pub extranonce2: String,
    pub ntime: String,
    pub nonce: String,
    pub hash: [u8; 32],
    pub is_block_candidate: bool,
}

struct SharedState {
    inner: Mutex<StateInner>,
    notify: Condvar,
    version: AtomicU64,
    // total hashes counted since last sample (worker threads increment)
    hash_count: AtomicU64,
    // last computed hashes-per-second
    hashrate: AtomicU64,
}

// add counters for hashrate reporting
impl SharedState {
    fn new(inner: StateInner) -> Self {
        SharedState {
            inner: Mutex::new(inner),
            notify: Condvar::new(),
            version: AtomicU64::new(0),
            hash_count: AtomicU64::new(0),
            hashrate: AtomicU64::new(0),
        }
    }
}

struct StateInner {
    subscription: Option<Subscription>,
    share_target: Target,
    active_job: Option<Arc<JobContext>>,
    pending_template: Option<JobTemplate>,
    shutting_down: bool,
}

#[derive(Serialize)]
pub struct JobInfo {
    pub job_id: String,
    pub ntime: u32,
    pub clean_jobs: bool,
}

struct JobContext {
    template: JobTemplate,
    subscription: Subscription,
    share_target: Target,
    extranonce_counter: AtomicU64,
    debug: bool,
}

struct WorkUnit {
    midstate: sha256::Midstate,
    tail: [u8; 16],
    extranonce2: Vec<u8>,
    prefix: [u8; 64],
}

impl MiningCoordinator {
    pub fn new(config: Config) -> Result<Self> {
        let (share_tx, share_rx) = unbounded_channel();
        let shared = Arc::new(SharedState {
            inner: Mutex::new(StateInner {
                subscription: None,
                share_target: apply_fudge_to_target(
                    share_target_from_difficulty(1.0),
                    config.fudge,
                ),
                active_job: None,
                pending_template: None,
                shutting_down: false,
            }),
            notify: Condvar::new(),
            version: AtomicU64::new(0),
            hash_count: AtomicU64::new(0),
            hashrate: AtomicU64::new(0),
        });

        let coordinator = MiningCoordinator {
            inner: Arc::new(CoordinatorInner {
                config: config.clone(),
                shared: shared.clone(),
                share_tx,
                share_rx: Mutex::new(Some(share_rx)),
                workers: Mutex::new(Vec::new()),
            }),
        };

        coordinator.spawn_workers()?;
        Ok(coordinator)
    }

    fn spawn_workers(&self) -> Result<()> {
        let mut workers = self.inner.workers.lock().unwrap();
        if !workers.is_empty() {
            return Ok(());
        }
        let thread_count = self.inner.config.threads.get();
        for id in 0..thread_count {
            let shared = self.inner.shared.clone();
            let share_tx = self.inner.share_tx.clone();
            let handle = thread::Builder::new()
                .name(format!("miner-{id}"))
                .spawn(move || worker_loop(id, shared, share_tx))
                .map_err(|e| anyhow!("failed to spawn worker thread: {e}"))?;
            workers.push(handle);
        }
        {
            let shared = self.inner.shared.clone();
            let handle = thread::Builder::new()
                .name("hashrate-sampler".to_string())
                .spawn(move || loop {
                    let delta = shared.hash_count.swap(0, Ordering::Relaxed);
                    shared.hashrate.store(delta, Ordering::Relaxed);
                    thread::sleep(Duration::from_secs(1));
                })
                .map_err(|e| anyhow!("failed to spawn sampler thread: {e}"))?;
            workers.push(handle);
        }
        Ok(())
    }

    pub fn take_share_receiver(&self) -> Result<UnboundedReceiver<ShareSubmission>> {
        self.inner
            .share_rx
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow!("share receiver already taken"))
    }

    pub fn update_subscription(&self, subscription: Subscription) {
        let debug = self.inner.config.debug;
        let mut guard = self.inner.shared.inner.lock().unwrap();
        guard.subscription = Some(subscription.clone());
        if let Some(job) = guard.active_job.take() {
            let new_job = JobContext::new(
                job.template.clone(),
                subscription,
                guard.share_target,
                debug,
            );
            guard.active_job = Some(Arc::new(new_job));
        } else if let Some(template) = guard.pending_template.take() {
            let new_job = JobContext::new(template, subscription, guard.share_target, debug);
            guard.active_job = Some(Arc::new(new_job));
        }
        drop(guard);
        self.bump_version();
    }

    pub fn update_share_target(&self, target: Target) {
        let debug = self.inner.config.debug;
        let fudge = self.inner.config.fudge;
        let mut guard = self.inner.shared.inner.lock().unwrap();
        let adjusted = apply_fudge_to_target(target, fudge);
        guard.share_target = adjusted;
        if let Some(job) = guard.active_job.take() {
            if let Some(subscription) = guard.subscription.clone() {
                let new_job = JobContext::new(job.template.clone(), subscription, adjusted, debug);
                guard.active_job = Some(Arc::new(new_job));
            }
        }
        drop(guard);
        self.bump_version();
    }

    pub fn install_job(&self, template: JobTemplate) {
        let debug = self.inner.config.debug;
        let mut guard = self.inner.shared.inner.lock().unwrap();
        if let Some(subscription) = guard.subscription.clone() {
            let job = JobContext::new(template, subscription, guard.share_target, debug);
            guard.active_job = Some(Arc::new(job));
        } else {
            guard.pending_template = Some(template);
        }
        drop(guard);
        self.bump_version();
    }

    fn bump_version(&self) {
        self.inner.shared.version.fetch_add(1, Ordering::SeqCst);
        self.inner.shared.notify.notify_all();
    }

    pub fn status(&self) -> &'static str {
        let guard: std::sync::MutexGuard<'_, StateInner> = self.inner.shared.inner.lock().unwrap();
        if guard.shutting_down {
            "shutting_down"
        } else if guard.pending_template.is_some() {
            "mining"
        } else {
            "idle"
        }
    }

    /// Hashrate in kilo-hashes/sec
    pub fn hashrate_khs(&self) -> f64 {
        let raw = self.inner.shared.hashrate.load(Ordering::Relaxed);
        raw as f64 / 1_000.0
    }

    ///  job count
    pub fn jobs(&self) -> usize {
        let guard = self.inner.shared.inner.lock().unwrap();
        guard.pending_template.iter().count()
    }

   
    pub fn info(&self) -> serde_json::Value {
        let status = self.status();
        let hashrate_khs = self.hashrate_khs();
        let jobs = self.jobs();

        let version = self.inner.shared.version.load(Ordering::Relaxed);
        let hash_count = self.inner.shared.hash_count.load(Ordering::Relaxed);
        let raw_hashrate = self.inner.shared.hashrate.load(Ordering::Relaxed);

        let config = &self.inner.config;
        let threads = config.threads.get();

        let workers_len = {
            let workers = self.inner.workers.lock().unwrap();
            workers.len()
        };

        let (share_target_hex, subscription_json, pending_template, shutting_down) = {
            let guard = self.inner.shared.inner.lock().unwrap();

            let share_target_hex = hex::encode(guard.share_target.to_be_bytes());

            let subscription_json = guard.subscription.as_ref().map(|s| {
                json!({
                    "extranonce1": hex::encode(&s.extranonce1),
                    "extranonce2_size": s.extranonce2_size,
                })
            });

            (
                share_target_hex,
                subscription_json,
                guard.pending_template.is_some(),
                guard.shutting_down,
            )
        };

        json!({
            "status": status,
            "hashrate_khs": hashrate_khs,
            "jobs": jobs,

            "config": {
                "threads": threads,
                "debug": config.debug,
                "fudge": config.fudge,
                "benchmark": config.benchmark,
                "pool_url": config.pool_url,
                "http_port": config.http_port,
            },

            "workers": workers_len,

            "protocol": {
                "share_target": share_target_hex,
                "subscription": subscription_json,
                "pending_template": pending_template,
                "shutting_down": shutting_down,
            },

            "metrics": {
                "version": version,
                "hash_count": hash_count,
                "raw_hashrate": raw_hashrate,
            }
        })
    }
}

fn worker_loop(id: usize, shared: Arc<SharedState>, share_tx: UnboundedSender<ShareSubmission>) {
    let mut seen_version = shared.version.load(Ordering::SeqCst);
    loop {
        let (job, job_version) = {
            let mut guard = shared.inner.lock().unwrap();
            loop {
                if guard.shutting_down {
                    return;
                }
                if let Some(job) = guard.active_job.clone() {
                    let current_version = shared.version.load(Ordering::SeqCst);
                    if current_version != seen_version || job.template.clean_jobs {
                        seen_version = current_version;
                        break (job, current_version);
                    }
                }
                guard = shared.notify.wait(guard).unwrap();
            }
        };

        if let Err(err) = mine_job(id, job, job_version, &shared, &share_tx) {
            eprintln!("[worker {id}] mining error: {err:?}");
            thread::sleep(Duration::from_secs(1));
        }
    }
}

fn mine_job(
    _id: usize,
    job: Arc<JobContext>,
    job_version: u64,
    shared: &Arc<SharedState>,
    share_tx: &UnboundedSender<ShareSubmission>,
) -> Result<()> {
    loop {
        if shared.version.load(Ordering::SeqCst) != job_version {
            return Ok(());
        }

        let extranonce2 = match job.next_extranonce2() {
            Some(bytes) => bytes,
            None => {
                thread::sleep(Duration::from_millis(25));
                continue;
            }
        };

        let work =
            prepare_work(&job, &extranonce2).with_context(|| "failed to prepare work unit")?;

        let WorkUnit {
            midstate,
            mut tail,
            extranonce2,
            prefix,
        } = work;
        let share_target = job.share_target;
        let network_target = job.template.network_target;
        let extranonce2_hex = hex::encode(&extranonce2);
        let ntime = job.template.ntime.clone();
        let job_id = job.template.job_id.clone();

        let mut nonce: u32 = 0;
        loop {
            if shared.version.load(Ordering::SeqCst) != job_version {
                return Ok(());
            }
            set_nonce(&mut tail, nonce);
            let hash = hash_from_midstate(&midstate, &tail, CPUNET_SUFFIX);
            shared.hash_count.fetch_add(1, Ordering::Relaxed);
            if hash_meets_target(&hash, share_target) {
                let is_block = hash_meets_target(&hash, network_target);
                if job.debug {
                    let mut header = [0u8; 80];
                    header[..64].copy_from_slice(&prefix);
                    header[64..80].copy_from_slice(&tail);
                    let mut preimage = Vec::with_capacity(80 + CPUNET_SUFFIX.len());
                    preimage.extend_from_slice(&header);
                    preimage.extend_from_slice(CPUNET_SUFFIX);
                    println!(
                        "[debug] Found share hash: {} preimage: {} block_candidate: {}",
                        hex::encode(hash),
                        hex::encode(&preimage),
                        is_block
                    );
                }
                let submission = ShareSubmission {
                    job_id: job_id.clone(),
                    extranonce2: extranonce2_hex.clone(),
                    ntime: format!("{:08x}", ntime.to_u32()),
                    nonce: format!("{:08x}", nonce),
                    hash,
                    is_block_candidate: is_block,
                };
                let _ = share_tx.send(submission);
            }
            nonce = nonce.wrapping_add(1);
            if nonce == 0 {
                break;
            }
        }
    }
}

fn prepare_work(job: &JobContext, extranonce2: &[u8]) -> Result<WorkUnit> {
    let coinbase = build_coinbase(&job.template, &job.subscription.extranonce1, extranonce2);
    let merkle_root = compute_merkle_root(&coinbase, &job.template.merkle_branch)?;
    let header = BlockHeader {
        version: job.template.version,
        prev_blockhash: job.template.prevhash,
        merkle_root,
        time: job.template.ntime,
        bits: job.template.compact_target,
        nonce: 0,
    };
    let header_bytes = bitcoin::consensus::serialize(&header);
    let mut prefix = [0u8; 64];
    prefix.copy_from_slice(&header_bytes[..64]);
    let mut tail = [0u8; 16];
    tail.copy_from_slice(&header_bytes[64..80]);
    let midstate = midstate_from_prefix(&prefix);

    Ok(WorkUnit {
        midstate,
        tail,
        extranonce2: extranonce2.to_vec(),
        prefix,
    })
}

fn build_coinbase(template: &JobTemplate, extranonce1: &[u8], extranonce2: &[u8]) -> Vec<u8> {
    let mut coinbase = Vec::with_capacity(
        template.coinbase1.len() + extranonce1.len() + extranonce2.len() + template.coinbase2.len(),
    );
    coinbase.extend_from_slice(&template.coinbase1);
    coinbase.extend_from_slice(extranonce1);
    coinbase.extend_from_slice(extranonce2);
    coinbase.extend_from_slice(&template.coinbase2);
    coinbase
}

fn compute_merkle_root(coinbase: &[u8], merkle_branch: &[Vec<u8>]) -> Result<TxMerkleNode> {
    let mut hash = sha256d::Hash::hash(coinbase);
    for branch in merkle_branch {
        if branch.len() != 32 {
            bail!("invalid merkle branch element length: {}", branch.len());
        }
        let mut data = Vec::with_capacity(64);
        data.extend_from_slice(hash.as_byte_array());
        data.extend_from_slice(branch);
        hash = sha256d::Hash::hash(&data);
    }
    Ok(TxMerkleNode::from_byte_array(hash.to_byte_array()))
}

fn set_nonce(tail: &mut [u8; 16], nonce: u32) {
    tail[12..16].copy_from_slice(&nonce.to_le_bytes());
}

fn hash_meets_target(hash: &[u8; 32], target: Target) -> bool {
    if target == Target::ZERO {
        return false;
    }
    let block_hash = BlockHash::from_byte_array(*hash);
    target.is_met_by(block_hash)
}

impl JobContext {
    fn new(
        template: JobTemplate,
        subscription: Subscription,
        share_target: Target,
        debug: bool,
    ) -> Self {
        JobContext {
            template,
            subscription,
            share_target,
            extranonce_counter: AtomicU64::new(0),
            debug,
        }
    }

    fn next_extranonce2(&self) -> Option<Vec<u8>> {
        let size = self.subscription.extranonce2_size;
        if size == 0 {
            return Some(Vec::new());
        }
        let counter = self.extranonce_counter.fetch_add(1, Ordering::Relaxed);
        let limit = if size >= 8 {
            u64::MAX
        } else {
            (1u64 << (size * 8)) - 1
        };
        if size < 8 && counter > limit {
            return None;
        }
        let value = counter & limit;
        let mut bytes = vec![0u8; size];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = ((value >> (8 * i)) & 0xff) as u8;
        }
        Some(bytes)
    }
}

pub async fn benchmark(config: &Config) -> Result<()> {
    let threads = config.threads.get();
    println!("Running benchmark on {threads} threads...");

    let stop_flag = Arc::new(AtomicBool::new(false));
    let hash_counter = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for _ in 0..threads {
        let stop = stop_flag.clone();
        let counter = hash_counter.clone();
        let handle = thread::spawn(move || {
            let prefix = [0u8; 64];
            let midstate = midstate_from_prefix(&prefix);
            let mut tail = [0u8; 16];
            let mut nonce = 0u32;
            while !stop.load(Ordering::Relaxed) {
                tail[12..16].copy_from_slice(&nonce.to_le_bytes());
                let _ = hash_from_midstate(&midstate, &tail, CPUNET_SUFFIX);
                counter.fetch_add(1, Ordering::Relaxed);
                nonce = nonce.wrapping_add(1);
            }
        });
        handles.push(handle);
    }

    let duration = Duration::from_secs(5);
    tokio::time::sleep(duration).await;
    stop_flag.store(true, Ordering::Relaxed);

    for handle in handles {
        let _ = handle.join();
    }

    let total = hash_counter.load(Ordering::Relaxed);
    let khash = total as f64 / (duration.as_secs_f64() * 1000.0);
    println!("Benchmark: {:.2} khash/s", khash);

    Ok(())
}

pub fn parse_job_template(
    job_id: String,
    prevhash: &str,
    coinbase1: &str,
    coinbase2: &str,
    merkle_branch: &[String],
    version: &str,
    nbits: &str,
    ntime: &str,
    clean_jobs: bool,
) -> Result<JobTemplate> {
    let prevhash_bytes = hex::decode(prevhash).context("invalid prevhash hex")?;
    if prevhash_bytes.len() != 32 {
        bail!("prevhash must be 32 bytes");
    }
    let mut prevhash_array = [0u8; 32];
    prevhash_array.copy_from_slice(&prevhash_bytes);
    for chunk in prevhash_array.chunks_exact_mut(4) {
        chunk.reverse();
    }
    let prevhash = BlockHash::from_byte_array(prevhash_array);

    let coinbase1 = hex::decode(coinbase1).context("invalid coinbase1 hex")?;
    let coinbase2 = hex::decode(coinbase2).context("invalid coinbase2 hex")?;

    let merkle_branch = merkle_branch
        .iter()
        .map(|s| hex::decode(s).context("invalid merkle branch hex"))
        .collect::<Result<Vec<_>>>()?;

    let version_i32 = i32::from_str_radix(version, 16).context("invalid version hex")?;
    let version = Version::from_consensus(version_i32);
    let nbits_u32 = u32::from_str_radix(nbits, 16).context("invalid nbits hex")?;
    let compact_target = CompactTarget::from_consensus(nbits_u32);
    let ntime_u32 = u32::from_str_radix(ntime, 16).context("invalid ntime hex")?;
    let ntime = BlockTime::from_u32(ntime_u32);

    let network_target = Target::from_compact(compact_target);

    Ok(JobTemplate {
        job_id,
        version,
        prevhash,
        coinbase1,
        coinbase2,
        merkle_branch,
        compact_target,
        ntime: ntime,
        clean_jobs,
        network_target,
    })
}

pub fn share_target_from_difficulty(difficulty: f64) -> Target {
    if difficulty <= 0.0 || !difficulty.is_finite() {
        return Target::MAX;
    }
    let numerator = BigUint::from_bytes_be(&Target::MAX.to_be_bytes());
    let rational = match BigRational::from_float(difficulty) {
        Some(r) if !r.is_zero() => r,
        _ => return Target::MAX,
    };

    let numerator_rat = BigRational::from_integer(BigInt::from(numerator));
    let target_rat = numerator_rat / rational;
    let target_int = target_rat.floor().to_integer();
    let target_uint = target_int.to_biguint().unwrap_or_else(BigUint::zero);
    target_from_biguint(&target_uint)
}

pub fn apply_fudge_to_target(target: Target, fudge: f64) -> Target {
    if !fudge.is_finite() || fudge <= 0.0 {
        return target;
    }
    if (fudge - 1.0).abs() < f64::EPSILON {
        return target;
    }

    let base = BigUint::from_bytes_be(&target.to_be_bytes());
    let factor = match BigRational::from_float(fudge) {
        Some(f) => f,
        None => return target,
    };
    let scaled = BigRational::from_integer(BigInt::from(base)) * factor;
    let scaled_int = scaled.floor().to_integer();
    let scaled_uint = scaled_int.to_biguint().unwrap_or_else(BigUint::zero);
    target_from_biguint(&scaled_uint)
}

fn target_from_biguint(value: &BigUint) -> Target {
    let bytes = value.to_bytes_be();
    let mut buf = [0u8; 32];
    let slice = if bytes.len() > 32 {
        &bytes[bytes.len() - 32..]
    } else {
        &bytes[..]
    };
    buf[32 - slice.len()..].copy_from_slice(slice);
    let candidate = Target::from_be_bytes(buf);
    if candidate > Target::MAX {
        Target::MAX
    } else {
        candidate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::block::Header as BlockHeader;
    use bitcoin::consensus::encode::serialize;
    use bitcoin::BlockTime;
    use std::convert::TryInto;
    use std::str::FromStr;

    #[test]
    fn midstate_hash_matches_block_header() {
        let version = Version::from_consensus(0x20000000);
        let prevhash =
            BlockHash::from_str("00000000bbffa57733938dbc05f86239f303636de4599905ab84bccfa909f49b")
                .expect("valid previous block hash");
        let merkle_root = TxMerkleNode::from_str(
            "45568d636e5c851bf0acd895b394f2abc40f62b91173613ff263da72aeae5b11",
        )
        .expect("valid merkle root");
        let ntime = 1723652722;
        let compact = CompactTarget::from_consensus(0x1d00ffff);
        let nonce = 2718237372;

        let expected_header = BlockHeader {
            version,
            prev_blockhash: prevhash,
            merkle_root,
            time: BlockTime::from_u32(ntime),
            bits: compact,
            nonce,
        };
        let expected_cpunet_hash =
            BlockHash::from_str("0000000086006c60cb448978c2b6b8e2d3917b3ea01f53ee89eed0607fed24d1")
                .expect("valid cpunet block hash");
        let header_bytes: [u8; 80] = serialize(&expected_header)
            .try_into()
            .expect("serialized header must be 80 bytes");
        let mut prefix = [0u8; 64];
        prefix.copy_from_slice(&header_bytes[..64]);
        let mut tail = [0u8; 16];
        tail.copy_from_slice(&header_bytes[64..80]);
        let midstate = midstate_from_prefix(&prefix);
        let hash_bytes_no_suffix = hash_from_midstate(&midstate, &tail, &[]);
        assert_eq!(
            BlockHash::from_byte_array(hash_bytes_no_suffix),
            BlockHash::from_byte_array(sha256d::Hash::hash(&header_bytes).to_byte_array())
        );

        let hash_bytes_with_suffix = hash_from_midstate(&midstate, &tail, CPUNET_SUFFIX);
        let mut header_with_suffix = Vec::from(header_bytes);
        header_with_suffix.extend_from_slice(CPUNET_SUFFIX);
        let expected_with_suffix = sha256d::Hash::hash(&header_with_suffix);
        assert_eq!(
            BlockHash::from_byte_array(hash_bytes_with_suffix),
            expected_cpunet_hash
        );
        assert_eq!(
            expected_cpunet_hash,
            BlockHash::from_byte_array(expected_with_suffix.to_byte_array())
        );
        assert_eq!(expected_cpunet_hash, expected_header.block_hash());
        println!("expected header: {:?}", expected_header);
        println!("expected cpunet hash: {:?}", expected_cpunet_hash);
    }

    #[test]
    fn difficulty_one_returns_max_target() {
        assert_eq!(share_target_from_difficulty(1.0), Target::MAX);
    }
}
