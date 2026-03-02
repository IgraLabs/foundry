# IGRA TPS Benchmark — End-to-End Setup Guide

## Prerequisites

- A funded Kaspa testnet wallet (private key in hex)
- Galleon testnet must be running (EVM RPC + Kaspa gRPC accessible)

## Dependencies

| Repository | Required for | Branch |
|---|---|---|
| [IgraLabs/foundry](https://github.com/IgraLabs/foundry) | Benchmark binary + full cast/forge | `emdin-benchmarks` |
| [IgraLabs/rusty-kaspa](https://github.com/IgraLabs/rusty-kaspa) | Kaspa crypto, gRPC client, consensus types (6 crates) | `master` |
| [IgraLabs/kaswallet](https://github.com/IgraLabs/kaswallet) | Only needed if building full `cast`/`forge` (not needed for benchmark alone) | `roman/utxo-perf-opt` |

The benchmark binary (`igra_tps_bench`) uses only `rusty-kaspa`. It reimplements UTXO selection, signing, and prefix mining directly without `kaswallet`.

**Directory layout** — the workspace `Cargo.toml` uses path dependencies to sibling directories:
```
~/igra/
├── igra-foundry/    ← Cargo.toml references ../rusty-kaspa
├── rusty-kaspa/     ← required
└── kaswallet/       ← only if building cast/forge
```

## Option A: Local Machine (macOS/Linux)

### 1. Clone repositories

```bash
mkdir -p ~/igra && cd ~/igra

# IGRA Foundry fork (contains the benchmark)
git clone https://github.com/IgraLabs/foundry.git igra-foundry
cd igra-foundry
git checkout emdin-benchmarks

# Rusty Kaspa (required — Kaspa crypto + gRPC crates)
cd ~/igra
git clone https://github.com/IgraLabs/rusty-kaspa.git rusty-kaspa

# Kaswallet (only if building full cast/forge, NOT needed for benchmark)
# git clone https://github.com/IgraLabs/kaswallet.git kaswallet
# cd kaswallet && git checkout roman/utxo-perf-opt
```

### 2. Install dependencies

**macOS:**
```bash
brew install protobuf pkg-config openssl
```

**Ubuntu/Debian:**
```bash
sudo apt-get install -y build-essential pkg-config clang libssl-dev protobuf-compiler
```

**Rust** (if not installed):
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
```

### 3. Build the benchmark

```bash
cd ~/igra/igra-foundry
cargo build -p igra-kaspa-derive --bin igra_tps_bench --release
```

Binary will be at `target/release/igra_tps_bench`.

### 4. Get testnet configuration

```bash
# Verify EVM RPC is up
cast chain-id --rpc-url https://galleon-testnet.igralabs.com:8545
# Should return: 38836

# Chain ID in hex = TX ID prefix
python3 -c "print(hex(38836))"
# 0x97b4 → prefix is "97b4"
```

### 5. Fund your master wallet

The benchmark needs a funded Kaspa testnet address. Generate one or use an existing key.

Funding requirements (rule of thumb):
- **Per sender per round**: ~0.0022 KAS (fees)
- **Per sender base**: 0.105 KAS (minimum UTXO)
- **Example**: 500 senders × 10 rounds ≈ 63.5 KAS

### 6. Run the benchmark

```bash
./target/release/igra_tps_bench \
  --kaspa-rpc-url "grpc://95.217.73.85:16210" \
  --network testnet-10 \
  --el-rpc-url "https://galleon-testnet.igralabs.com:8545" \
  --chain-id 38836 \
  --master-private-key "YOUR_HEX_PRIVATE_KEY" \
  --tx-id-prefix "97b4" \
  --num-senders 500 \
  --txs-per-sender 10 \
  --senders-file /tmp/igra_bench_senders.json \
  --min-gas-price 2000000000000 \
  --skip-evm \
  --parallel-funding
```

**Key flags:**
- `--skip-evm` — L1-only mode (skip EVM funding, faster)
- `--parallel-funding` — 20x faster sender funding
- `--senders-file` — persist keypairs for re-runs/sweeps
- `--sweep-only` — recover all funds back to master

---

## Option B: AWS (for higher throughput)

We used a **c5.18xlarge** (72 vCPU, 144 GB RAM) in **us-east-1**. This gives ~250 TPS vs ~190 TPS local due to more CPU cores for parallel prefix mining.

### 1. Launch EC2 instance

```bash
# Create key pair (one time)
aws ec2 create-key-pair \
  --key-name igra-bench \
  --query 'KeyMaterial' \
  --output text > ~/.ssh/igra-bench.pem
chmod 400 ~/.ssh/igra-bench.pem

# Launch instance
aws ec2 run-instances \
  --image-id ami-0c7217cdde317cfec \
  --instance-type c5.18xlarge \
  --key-name igra-bench \
  --security-groups default \
  --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":100,"VolumeType":"gp3"}}]' \
  --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=igra-bench}]'
```

Note the instance ID and wait for it to be running, then get the public IP:
```bash
aws ec2 describe-instances --instance-ids i-XXXX \
  --query 'Reservations[0].Instances[0].PublicIpAddress' --output text
```

### 2. Upload source code

```bash
# Create tarball (from parent of igra-foundry and kaswallet dirs)
cd ~/igra
tar czf /tmp/igra-bench-src.tar.gz \
  --exclude='target' --exclude='.git' \
  igra-foundry kaswallet

# Upload
scp -i ~/.ssh/igra-bench.pem /tmp/igra-bench-src.tar.gz ubuntu@<AWS_IP>:~/
```

### 3. SSH in and run setup

```bash
ssh -i ~/.ssh/igra-bench.pem ubuntu@<AWS_IP>

# On the instance:
mkdir -p ~/bench
tar xzf ~/igra-bench-src.tar.gz -C ~/bench

# Install deps + Rust + clone rusty-kaspa + build everything
bash ~/bench/igra-foundry/scripts/aws-bench-setup.sh
```

This script:
1. Installs system deps (build-essential, protobuf, clang, openssl)
2. Installs Rust
3. Clones `rusty-kaspa` from IgraLabs GitHub
4. Extracts the uploaded tarball
5. Builds `kaspad` and `igra_tps_bench` in release mode
6. Starts a local `kaspad` node (testnet-10) and waits for sync

Build takes ~10-15 min on c5.18xlarge. kaspad sync takes ~5-10 min.

### 4. Run the benchmark

```bash
# Using the helper script:
bash ~/bench/igra-foundry/scripts/aws-bench-run.sh YOUR_HEX_PRIVATE_KEY \
  --senders 1000 --rounds 500

# Or directly:
~/bench/igra-foundry/target/release/igra_tps_bench \
  --kaspa-rpc-url "grpc://127.0.0.1:16210" \
  --network testnet-10 \
  --el-rpc-url "https://galleon-testnet.igralabs.com:8545" \
  --chain-id 38836 \
  --master-private-key "YOUR_KEY" \
  --tx-id-prefix "97b4" \
  --num-senders 1000 \
  --txs-per-sender 500 \
  --senders-file ~/bench/senders.json \
  --min-gas-price 2000000000000 \
  --skip-evm \
  --parallel-funding
```

### 5. Stop the instance when done

```bash
# From local machine:
aws ec2 stop-instances --instance-ids i-XXXX

# Or terminate to avoid storage costs:
aws ec2 terminate-instances --instance-ids i-XXXX
```

**Cost**: c5.18xlarge is ~$3/hr. A 30-min benchmark run costs ~$1.50 compute. Don't forget to stop/terminate.

---

## Benchmark Results Reference

| Setup | Senders | TXs | TPS | Failures |
|-------|---------|-----|-----|----------|
| Local (M-series Mac) | 500 | 5,000 | 182-197 | 0 |
| Local 30-min | 1,000 | 330,000 | 186 | 0 |
| AWS c5.18xlarge | 1,000 | 5,000 | 245-266 | 0 |
| AWS 30-min | 1,000 | 480,000 | 246 | 0 |
| Local with EVM verify | 100 | 100 | 228 | 0 (100/100 L2 confirmed) |

---

## Common Operations

### Recover funds (sweep)
```bash
./target/release/igra_tps_bench \
  --kaspa-rpc-url "grpc://95.217.73.85:16210" \
  --network testnet-10 \
  --master-private-key "YOUR_KEY" \
  --tx-id-prefix "97b4" \
  --senders-file /tmp/igra_bench_senders.json \
  --sweep-only
```

### Resume interrupted run
```bash
# If interrupted at round 5, resume from there:
./target/release/igra_tps_bench \
  ... \
  --senders-file /tmp/igra_bench_senders.json \
  --start-round 5 \
  --skip-funding
```

### Check Kaspa balance
```bash
cargo build -p igra-kaspa-derive --bin kaspa_balance --release
./target/release/kaspa_balance --rpc "grpc://95.217.73.85:16210" "kaspatest:YOUR_ADDRESS"
```

---

## Troubleshooting

| Problem | Solution |
|---------|----------|
| `insufficient master balance` | Pre-flight check failed. Send more tKAS to the address shown in the error. |
| `Kaspa transaction mass exceeds limit` | Too many small UTXOs. Run with fewer senders or without `--parallel-funding`. |
| `IGRA_MINING_001: timed out` | Prefix mining took >120s. Unlikely with 2-byte prefix; check CPU load. |
| `ResourceExhausted` from gRPC | Too many connections. The benchmark reuses one; check for other processes. |
| TXs on L1 but not on L2 | Wrong `--tx-id-prefix`. Must equal chain ID in hex. |
| Build fails: `kaspa-*` not found | Check that `rusty-kaspa` is cloned as sibling dir to `igra-foundry`. |
| AWS: kaspad not syncing | Check `~/bench/kaspad.log`. May need to open port 16111 for P2P. |
