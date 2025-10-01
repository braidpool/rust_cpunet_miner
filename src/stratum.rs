use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bitcoin::pow::Target;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::timeout;
use url::Url;

use crate::cli::Config;
use crate::mining::{
    apply_fudge_to_target, parse_job_template, share_target_from_difficulty, MiningCoordinator,
    ShareSubmission, Subscription,
};

const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct StratumClient {
    coordinator: MiningCoordinator,
    outbound: UnboundedSender<String>,
    inbound: UnboundedReceiver<Value>,
    share_rx: UnboundedReceiver<ShareSubmission>,
    next_id: u64,
    username: String,
    password: String,
    pending: HashMap<u64, PendingRequest>,
    debug: bool,
    fudge: f64,
}

enum PendingRequest {
    SubmitShare(ShareSubmissionMeta),
}

#[derive(Clone, Debug)]
struct ShareSubmissionMeta {
    job_id: String,
    nonce: String,
    extranonce2: String,
    is_block: bool,
    hash: [u8; 32],
}

impl StratumClient {
    pub async fn connect(config: Config, coordinator: MiningCoordinator) -> Result<Self> {
        let pool_url = config
            .pool_url
            .clone()
            .ok_or_else(|| anyhow!("pool URL must be provided"))?;

        let share_rx = coordinator.take_share_receiver()?;
        let debug = config.debug;
        let fudge = config.fudge;

        let parsed = Url::parse(&pool_url).context("invalid pool URL")?;
        if parsed.scheme() != "stratum+tcp" {
            bail!("unsupported scheme {}", parsed.scheme());
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow!("pool URL missing host"))?;
        let port = parsed.port().unwrap_or(3333);

        let addr = format!("{}:{}", host, port);
        let stream = TcpStream::connect(addr.clone())
            .await
            .with_context(|| format!("failed to connect to {addr}"))?;
        stream.set_nodelay(true)?;

        let (reader, writer) = stream.into_split();
        let mut writer = BufWriter::new(writer);
        let (outbound_tx, mut outbound_rx) = unbounded_channel::<String>();
        let writer_debug = debug;
        tokio::spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                let write_res = async {
                    if writer_debug {
                        println!("[debug] -> {msg}");
                    }
                    writer.write_all(msg.as_bytes()).await?;
                    writer.write_all(b"\n").await?;
                    writer.flush().await
                };
                if let Err(e) = write_res.await {
                    eprintln!("[stratum] write error: {e}");
                    break;
                }
            }
        });

        let (inbound_tx, inbound_rx) = unbounded_channel::<Value>();
        let reader_debug = debug;
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        let _ = inbound_tx.send(json!({ "type": "closed" }));
                        break;
                    }
                    Ok(_) => {
                        let text = line.trim_end();
                        if reader_debug {
                            println!("[debug] <- {text}");
                        }
                        match serde_json::from_str::<Value>(text) {
                            Ok(value) => {
                                let _ = inbound_tx.send(value);
                            }
                            Err(err) => {
                                eprintln!("[stratum] failed to parse message: {err}: {text}");
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("[stratum] read error: {err}");
                        break;
                    }
                }
            }
        });

        let username = config
            .username
            .clone()
            .ok_or_else(|| anyhow!("username must be provided"))?;
        let password = config.password.clone().unwrap_or_default();

        let mut client = StratumClient {
            coordinator,
            outbound: outbound_tx,
            inbound: inbound_rx,
            share_rx,
            next_id: 1,
            username,
            password,
            pending: HashMap::new(),
            debug,
            fudge,
        };

        client.perform_handshake().await?;
        Ok(client)
    }

    pub async fn run(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                Some(msg) = self.inbound.recv() => {
                    self.process_message(msg).await?;
                }
                Some(share) = self.share_rx.recv() => {
                    self.submit_share(share).await?;
                }
                else => {
                    bail!("stratum connection closed");
                }
            }
        }
    }

    async fn perform_handshake(&mut self) -> Result<()> {
        println!("Connecting to pool as {}", self.username);
        let subscribe_id = self.next_request_id();
        let subscribe_params = vec![Value::String("cpunet_miner/0.1".to_string())];
        self.send_request(subscribe_id, "mining.subscribe", subscribe_params)?;
        let subscribe_response = self.wait_for_response(subscribe_id).await?;
        let (extranonce1, extranonce2_size) = parse_subscribe_result(&subscribe_response)?;
        self.coordinator.update_subscription(Subscription {
            extranonce1,
            extranonce2_size,
        });

        let authorize_id = self.next_request_id();
        let auth_params = vec![
            Value::String(self.username.clone()),
            Value::String(self.password.clone()),
        ];
        self.send_request(authorize_id, "mining.authorize", auth_params)?;
        let authorize_response = self.wait_for_response(authorize_id).await?;
        ensure_authorized(&authorize_response)?;
        println!("Authorized successfully");
        Ok(())
    }

    async fn wait_for_response(&mut self, request_id: u64) -> Result<Value> {
        loop {
            let message = timeout(SUBSCRIBE_TIMEOUT, self.inbound.recv())
                .await
                .map_err(|_| anyhow!("timeout waiting for stratum response"))?
                .ok_or_else(|| anyhow!("stratum connection closed during handshake"))?;
            if message.get("type") == Some(&Value::String("closed".to_string())) {
                bail!("stratum connection closed");
            }
            if let Some(id) = message.get("id") {
                if id.as_u64() == Some(request_id) {
                    return Ok(message);
                }
            }
            self.handle_notification(&message).await?;
        }
    }

    fn send_request(&self, id: u64, method: &str, params: Vec<Value>) -> Result<()> {
        let payload = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        let text = serde_json::to_string(&payload)?;
        self.outbound
            .send(text)
            .map_err(|_| anyhow!("failed to queue stratum request"))
    }

    async fn process_message(&mut self, message: Value) -> Result<()> {
        if let Some(id) = message.get("id").and_then(|v| v.as_u64()) {
            if let Some(pending) = self.pending.remove(&id) {
                self.handle_response(id, pending, message).await?;
            } else {
                // handshake responses should not reach here
                self.handle_notification(&message).await?;
            }
        } else if message.get("method").is_some() {
            self.handle_notification(&message).await?;
        } else if message.get("type") == Some(&Value::String("closed".to_string())) {
            bail!("stratum connection closed by remote");
        }
        Ok(())
    }

    async fn handle_response(
        &mut self,
        _id: u64,
        pending: PendingRequest,
        message: Value,
    ) -> Result<()> {
        match pending {
            PendingRequest::SubmitShare(meta) => {
                let accepted = message
                    .get("result")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let hash_hex = hex::encode(meta.hash);
                if accepted {
                    if meta.is_block {
                        println!(
                            "Block candidate accepted! job={} nonce={} extranonce2={} hash={}",
                            meta.job_id, meta.nonce, meta.extranonce2, hash_hex
                        );
                    } else {
                        println!(
                            "Share accepted: job={} nonce={} extranonce2={} hash={}",
                            meta.job_id, meta.nonce, meta.extranonce2, hash_hex
                        );
                    }
                } else {
                    let error_msg = message
                        .get("error")
                        .and_then(|e| e[1].as_str())
                        .unwrap_or("unknown error");
                    println!(
                        "Share rejected: job={} nonce={} extranonce2={} hash={} ({})",
                        meta.job_id, meta.nonce, meta.extranonce2, hash_hex, error_msg
                    );
                }
            }
        }
        Ok(())
    }

    async fn handle_notification(&mut self, message: &Value) -> Result<()> {
        let method = match message.get("method").and_then(|v| v.as_str()) {
            Some(m) => m,
            None => return Ok(()),
        };
        let params = message
            .get("params")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        match method {
            "mining.set_difficulty" => {
                if let Some(diff_val) = params.first() {
                    let difficulty = diff_val
                        .as_f64()
                        .unwrap_or_else(|| diff_val.as_u64().unwrap_or(1) as f64);
                    let base_target = share_target_from_difficulty(difficulty);
                    let target = apply_fudge_to_target(base_target, self.fudge);
                    let effective = difficulty / self.fudge;
                    if self.debug {
                        println!(
                            "[debug] difficulty update reported={:.4} effective={:.4}",
                            difficulty, effective
                        );
                    }
                    self.coordinator.update_share_target(target);
                }
            }
            "mining.set_extranonce" => {
                if params.len() >= 2 {
                    let extranonce1_hex = params[0]
                        .as_str()
                        .ok_or_else(|| anyhow!("invalid extranonce1"))?;
                    let extranonce1 =
                        hex::decode(extranonce1_hex).with_context(|| "invalid extranonce1 hex")?;
                    let size = params[1]
                        .as_u64()
                        .ok_or_else(|| anyhow!("invalid extranonce2 size"))?;
                    self.coordinator.update_subscription(Subscription {
                        extranonce1,
                        extranonce2_size: size as usize,
                    });
                }
            }
            "mining.notify" => {
                if params.len() < 9 {
                    bail!("invalid mining.notify params");
                }
                let job_id = params[0].as_str().unwrap_or_default().to_string();
                let prevhash = params[1].as_str().unwrap_or_default();
                let coinbase1 = params[2].as_str().unwrap_or_default();
                let coinbase2 = params[3].as_str().unwrap_or_default();
                let merkle_branch_values = params[4]
                    .as_array()
                    .ok_or_else(|| anyhow!("invalid merkle branch"))?;
                let merkle_branch = merkle_branch_values
                    .iter()
                    .map(|v| v.as_str().unwrap_or_default().to_string())
                    .collect::<Vec<_>>();
                let version = params[5].as_str().unwrap_or_default();
                let nbits = params[6].as_str().unwrap_or_default();
                let ntime = params[7].as_str().unwrap_or_default();
                let clean_jobs = params[8].as_bool().unwrap_or(false);

                let template = parse_job_template(
                    job_id.clone(),
                    prevhash,
                    coinbase1,
                    coinbase2,
                    &merkle_branch,
                    version,
                    nbits,
                    ntime,
                    clean_jobs,
                )?;
                println!("New job {} (clean={})", job_id, clean_jobs);
                self.coordinator.install_job(template);
            }
            "mining.set_target" => {
                if let Some(target_hex) = params.first().and_then(|v| v.as_str()) {
                    let target_bytes =
                        hex::decode(target_hex).with_context(|| "invalid target hex")?;
                    if target_bytes.len() == 32 {
                        let mut buf = [0u8; 32];
                        buf.copy_from_slice(&target_bytes);
                        let base_target = Target::from_be_bytes(buf);
                        let target = apply_fudge_to_target(base_target, self.fudge);
                        if self.debug {
                            println!("[debug] share target override received");
                        }
                        self.coordinator.update_share_target(target);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn submit_share(&mut self, share: ShareSubmission) -> Result<()> {
        let id = self.next_request_id();
        let params = vec![
            Value::String(self.username.clone()),
            Value::String(share.job_id.clone()),
            Value::String(share.extranonce2.clone()),
            Value::String(share.ntime.clone()),
            Value::String(share.nonce.clone()),
        ];
        self.pending.insert(
            id,
            PendingRequest::SubmitShare(ShareSubmissionMeta {
                job_id: share.job_id.clone(),
                nonce: share.nonce.clone(),
                extranonce2: share.extranonce2.clone(),
                is_block: share.is_block_candidate,
                hash: share.hash,
            }),
        );
        self.send_request(id, "mining.submit", params)?;
        Ok(())
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

fn parse_subscribe_result(response: &Value) -> Result<(Vec<u8>, usize)> {
    let result = response
        .get("result")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("invalid subscribe response"))?;
    if result.len() < 3 {
        bail!("subscribe response missing extranonce values");
    }
    let extranonce1_hex = result[1]
        .as_str()
        .ok_or_else(|| anyhow!("invalid extranonce1"))?;
    let extranonce1 = hex::decode(extranonce1_hex).with_context(|| "invalid extranonce1 hex")?;
    let extranonce2_size = result[2]
        .as_u64()
        .ok_or_else(|| anyhow!("invalid extranonce2 size"))?;
    Ok((extranonce1, extranonce2_size as usize))
}

fn ensure_authorized(response: &Value) -> Result<()> {
    match response.get("result") {
        Some(Value::Bool(true)) => Ok(()),
        Some(Value::Bool(false)) => bail!("authorization rejected by pool"),
        Some(other) => bail!("unexpected authorize response: {other}"),
        None => bail!("authorize response missing result"),
    }
}
