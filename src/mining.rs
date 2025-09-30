use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bitcoin::block::Version;
use bitcoin::hash_types::{BlockHash, TxMerkleNode};
use bitcoin::hashes::{sha256, sha256d};
use bitcoin::pow::{CompactTarget, Target};
use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::Zero;
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
    pub ntime: u32,
    pub ntime_str: String,
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
}

struct StateInner {
    subscription: Option<Subscription>,
    share_target: Target,
    active_job: Option<Arc<JobContext>>,
    pending_template: Option<JobTemplate>,
    shutting_down: bool,
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
                share_target: share_target_from_difficulty(1.0),
                active_job: None,
                pending_template: None,
                shutting_down: false,
            }),
            notify: Condvar::new(),
            version: AtomicU64::new(0),
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
        let mut guard = self.inner.shared.inner.lock().unwrap();
        guard.share_target = target;
        if let Some(job) = guard.active_job.take() {
            if let Some(subscription) = guard.subscription.clone() {
                let new_job = JobContext::new(job.template.clone(), subscription, target, debug);
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
        let ntime = job.template.ntime_str.clone();
        let job_id = job.template.job_id.clone();

        let mut nonce: u32 = 0;
        loop {
            if shared.version.load(Ordering::SeqCst) != job_version {
                return Ok(());
            }
            set_nonce(&mut tail, nonce);
            let hash = hash_from_midstate(&midstate, &tail, CPUNET_SUFFIX);
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
                        "[debug] Found share hash={} preimage={} block_candidate={}",
                        hex::encode(hash),
                        hex::encode(&preimage),
                        is_block
                    );
                }
                let submission = ShareSubmission {
                    job_id: job_id.clone(),
                    extranonce2: extranonce2_hex.clone(),
                    ntime: ntime.clone(),
                    nonce: format!("{nonce:08x}"),
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
    let header_bytes = encode_header_bytes(
        job.template.version,
        job.template.prevhash,
        merkle_root,
        job.template.ntime,
        job.template.compact_target,
        0,
    );
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

fn encode_header_bytes(
    version: Version,
    prevhash: BlockHash,
    merkle_root: TxMerkleNode,
    ntime: u32,
    compact: CompactTarget,
    nonce: u32,
) -> [u8; 80] {
    let mut bytes = [0u8; 80];
    bytes[..4].copy_from_slice(&version.to_consensus().to_le_bytes());
    bytes[4..36].copy_from_slice(prevhash.as_byte_array());
    bytes[36..68].copy_from_slice(merkle_root.as_byte_array());
    bytes[68..72].copy_from_slice(&ntime.to_le_bytes());
    bytes[72..76].copy_from_slice(&compact.to_consensus().to_le_bytes());
    bytes[76..80].copy_from_slice(&nonce.to_le_bytes());
    bytes
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
    let prevhash = BlockHash::from_byte_array(prevhash_array);

    let coinbase1 = hex::decode(coinbase1).context("invalid coinbase1 hex")?;
    let coinbase2 = hex::decode(coinbase2).context("invalid coinbase2 hex")?;

    let merkle_branch = merkle_branch
        .iter()
        .map(|s| hex::decode(s).context("invalid merkle branch hex"))
        .collect::<Result<Vec<_>>>()?;

    let version_u32 = u32::from_str_radix(version, 16).context("invalid version hex")?;
    let version = Version::from_consensus(i32::from_le_bytes(version_u32.to_le_bytes()));
    let nbits_u32 = u32::from_str_radix(nbits, 16).context("invalid nbits hex")?;
    let compact_target = CompactTarget::from_consensus(nbits_u32);
    let ntime_u32 = u32::from_str_radix(ntime, 16).context("invalid ntime hex")?;

    let network_target = Target::from_compact(compact_target);

    Ok(JobTemplate {
        job_id,
        version,
        prevhash,
        coinbase1,
        coinbase2,
        merkle_branch,
        compact_target,
        ntime: ntime_u32,
        ntime_str: ntime.to_string(),
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
    use bitcoin::BlockTime;

    #[test]
    fn midstate_hash_matches_block_header() {
        let version = Version::from_consensus(2);
        let prevhash = BlockHash::from_byte_array([0u8; 32]);
        let merkle_root = TxMerkleNode::from_byte_array([1u8; 32]);
        let ntime = 0x5b8d80b3;
        let compact = CompactTarget::from_consensus(0x1d00ffff);

        let header_bytes = encode_header_bytes(version, prevhash, merkle_root, ntime, compact, 0);
        let mut prefix = [0u8; 64];
        prefix.copy_from_slice(&header_bytes[..64]);
        let mut tail = [0u8; 16];
        tail.copy_from_slice(&header_bytes[64..80]);
        let midstate = midstate_from_prefix(&prefix);
        let hash_bytes = hash_from_midstate(&midstate, &tail, CPUNET_SUFFIX);

        let expected = BlockHeader {
            version,
            prev_blockhash: prevhash,
            merkle_root,
            time: BlockTime::from_u32(ntime),
            bits: compact,
            nonce: 0,
        }
        .block_hash();

        assert_eq!(BlockHash::from_byte_array(hash_bytes), expected);
    }

    #[test]
    fn difficulty_one_returns_max_target() {
        assert_eq!(share_target_from_difficulty(1.0), Target::MAX);
    }
}
