# CPUNet Miner

`cpunet_miner` is a Rust-based CPU miner for the CPUNet Bitcoin test network. It speaks the Stratum v1
protocol, applies the CPUNet-specific proof-of-work tweak (a `cpunet\0` suffix in the double SHA-256
preimage), and uses the midstate optimisation to accelerate hashing on the CPU.

## Features
- Stratum v1 client supporting subscribe, authorize, difficulty, and job notifications.
- Multi-threaded CPU mining loop with nonce/extranonce coordination.
- CPUNet hashing (double SHA-256 of `header || "cpunet\0"`) implemented with midstate reuse.
- Share submission tracking and basic logging for accepted/rejected shares.
- Benchmark mode to measure CPUNet hash performance.

## Usage
```bash
cargo run --release -- \
  -o stratum+tcp://localhost:3333 \
  -O user:password \
  -t 4
```

### Options
- `-o, --url <URL>`: Stratum endpoint (e.g. `stratum+tcp://localhost:3333`).
- `-O, --userpass <USER:PASS>`: Worker credentials in `user:pass` form.
- `-t, --threads <N>`: Number of mining threads (defaults to available CPUs).
- `--benchmark`: Run the hashing benchmark and exit.
- `-D, --debug`: Print detailed debug output (including hash preimages when shares are found).
- `-f, --fudge <FACTOR>`: Scale the pool difficulty/target before submitting shares (useful for debugging).

## Development Notes
- The miner relies on the [`braidpool/rust-bitcoin`](https://github.com/braidpool/rust-bitcoin) fork, which
  exposes the CPUNet block header hashing changes.
- Unit tests cover the midstate hashing path and difficulty-to-target conversion.
