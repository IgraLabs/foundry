# IGRA Foundry Fork: Dev Guide

This repo is a Foundry fork with **IGRA mode**: an EVM-compatible execution layer (EL) where the **write path** (`eth_sendRawTransaction`) is routed through **Kaspa L1 payload transactions**.

This guide is aimed at engineers working on the fork or using it for testnet/devnet work.

## What’s Different vs Upstream Foundry

When IGRA mode is enabled:

- **Writes go through Kaspa**:
  - Foundry intercepts `eth_sendRawTransaction`.
  - It wraps the raw signed EVM tx bytes into a Kaspa tx `payload` as `[header][L2Data][4-byte nonce]`.
  - It **mines** a Kaspa txid prefix (network-specific) by iterating the payload nonce.
  - It signs + submits the Kaspa tx via gRPC, then waits for EL receipt as configured.
- **`eth_sendTransaction*` is not supported** in IGRA mode (unlocked node / browser wallet send flows are intentionally rejected).
- The user experience is kept familiar:
  - `cast send`, `forge create --broadcast`, `forge script --broadcast/--resume` still work, but the submission path is IGRA-aware.

Reference docs:
- Design and config contract: `docs/dev/igra-kaspa-integration-plan.md`, `docs/dev/igra-kaspa-design-spec-v2.md`
- Deterministic merge gate: `docs/dev/igra-deterministic-harness.md`

## Quick Start (Build)

```bash
cd /Users/user/Source/igra/foundry

# Recommended to avoid inheriting a global/unwritable CARGO_TARGET_DIR.
export CARGO_TARGET_DIR="$PWD/target"
export RUSTC_WRAPPER=

make build
```

Useful binaries:
- `target/release/cast`
- `target/release/forge`
- `target/release/anvil`
- `target/release/igra-loadgen` (high-throughput load generator)
- `target/release/kaspa_utxos` (debug helper)
- `target/release/kaspa_fund` (fan-out funder)

## IGRA Mode: How to Enable

IGRA mode is enabled when any is set:

1. CLI: `--igra`
2. Env: `FOUNDRY_IGRA_ENABLED=true`
3. Config: `igra.enabled = true` in `foundry.toml`

When enabled, Foundry validates up-front (RPC reachability, chain IDs, kaspa network, prefix hex, key material, etc.) and fails fast on mismatch.

## IGRA Configuration (`foundry.toml`)

Minimal working example for galleon testnet:

```toml
[profile.default]
eth_rpc_url = "https://galleon-testnet.igralabs.com:8545"

[igra]
enabled = true
el_rpc_url = "https://galleon-testnet.igralabs.com:8545"
kaspa_rpc_url = "grpc://stage-roman.igralabs.com:16210"
expected_el_chain_id = 38836
kaspa_network = "testnet-10"
tx_id_prefix = "97b4"
payload_compression = "none"
mining_timeout_secs = 120
el_receipt_timeout_secs = 300
sender_lock_timeout_secs = 60

[igra.kaspa_wallet]
# One of:
# - private_key = "0x..."
# - mnemonic = "word1 ... word12/24"
# - keystore = "/path/to/keystore.json"
mnemonic = "..."
mnemonic_passphrase = "..."
mnemonic_index = 0
```

Network-specific prefix:
- testnet-10 (galleon): `97b4`
- mainnet: `97b1`

## Key Material (Kaspa)

Kaspa signer material can be provided via:
- `cast send --private-key-kaspa ...`
- `cast send --mnemonic-kaspa ... --mnemonic-passphrase-kaspa ... --mnemonic-index-kaspa ...`
- `cast send --mnemonic-kaspa ... --mnemonic-passphrase-kaspa-as-mnemonic` (non-standard; opt-in)
- `cast send --mnemonic-kaspa ... --mnemonic-passphrase-kaspa-empty` (explicit empty passphrase)
- `cast send --keystore-kaspa ... --password-kaspa ...`

Environment variables used by the CLI:
- `KASPA_PRIVATE_KEY`
- `KASPA_MNEMONIC`
- `KASPA_MNEMONIC_PASSPHRASE`
- `KASPA_MNEMONIC_PASSPHRASE_AS_MNEMONIC`
- `KASPA_MNEMONIC_PASSPHRASE_EMPTY`
- `KASPA_MNEMONIC_DERIVATION_PATH`
- `KASPA_MNEMONIC_INDEX`
- `KASPA_KEYSTORE`
- `KASPA_KEYSTORE_ACCOUNT`
- `KASPA_PASSWORD`

Testnet script behavior:
- Scripts default to **empty** BIP39 passphrase (standard behavior).
- If you need the non-standard setup `passphrase == mnemonic`, set `IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC=1`.

## How to Test

### 1. Deterministic Harness (merge gate)

This is the first thing to run after code changes affecting IGRA transport/config/persistence:

```bash
./scripts/igra/deterministic-harness.sh
```

What it covers (high level):
- IGRA config validation + precedence
- Provider wiring
- Sender-lock / nonce-gap handling
- Lifecycle persistence transitions
- Payload format (header + raw tx + 4-byte nonce)
- Prefix mining behavior / timeout classification
- Guardrails for unsupported flows

Details: `docs/dev/igra-deterministic-harness.md`

### 2. Testnet Smoke (single tx end-to-end)

Runs a mint tx through IGRA mode and checks resulting state:

```bash
KEEP_TMP=1 ./scripts/igra/testnet-smoke.sh
```

Inputs are configured via env vars (see the script header), notably:
- `IGRA_EL_RPC_URL`
- `IGRA_KASPA_RPC_URL`
- `IGRA_KASPA_NETWORK`
- `IGRA_TX_ID_PREFIX`
- `IGRA_MNEMONIC_KASPA` / `IGRA_PRIVATE_KEY_KASPA` etc.

Repo default testnet wallets (used for our smoke/loadgen repro on galleon testnet):
- EVM mnemonic (IKAS): `test test test test test test test test test test test junk`
- Prefunded EVM sender (index 0): `0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266`
- Kaspa mnemonic (KAS): same phrase as above
- Kaspa passphrase: `passphrase == mnemonic` (non-standard; opt-in)
- Prefunded Kaspa address (index 0): `kaspatest:qzf364tlnl7ja0w65ydu0m5l70pur2hcm3l3ahkmhs660zcyf7cvuf6uznufr`

To use that setup with the smoke script:
```bash
MN="test test test test test test test test test test test junk"
IGRA_MNEMONIC_KASPA="$MN" \
IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC=1 \
IGRA_MNEMONIC_INDEX_KASPA=0 \
./scripts/igra/testnet-smoke.sh
```

Note:
- `scripts/igra/testnet-smoke.sh` sandboxes `HOME` per run, so the IGRA tx-map SQLite store does
  not leak across runs and cause nonce-gap blocking.

### 3. Testnet Load (10 accounts)

High-throughput tool (no per-tx `cast` process spawning):

- Repro + funding guide: `docs/dev/igra-loadgen-10-accounts.md`
- Binary: `crates/igra-loadgen`

Tip:
- `igra-loadgen` has `--no-proxy` to disable automatic proxy detection for HTTP(S) RPC in sandboxed environments.

## What to Expect (Perf and Bottlenecks)

### Prefix Mining Dominates

On testnet-10 we use a **2-byte txid prefix** (`97b4`).

- Expected attempts per tx is ~65,536 on average.
- That loop is CPU-bound and scales:
  - ~linearly with cores/CPU speed
  - ~exponentially with prefix length (adding 1 byte multiplies expected work by 256)

Empirical observation (10 funded accounts, 2-byte prefix, on a typical dev box):
- ~8–9 TPS per worker
- ~80–90 TPS aggregate for 10 workers

If you need 500 TPS sustained:
- you likely need to shorten the required prefix or parallelize mining significantly beyond a single dev machine.

### UTXO Management (avoid “UTXO explosion”)

`IgraTransport` supports two modes:

- `kaspa_utxo_mode=rpc`:
  - loads UTXOs via `get_utxos_by_addresses` and selects inputs.
  - does not scale if an address has a huge UTXO set.
- `kaspa_utxo_mode=chain` (recommended for load):
  - seeds once from RPC, then maintains a single-UTXO “tip” locally and spends it each tx.
  - this keeps per-worker UTXO count roughly constant (~1) and avoids double-spend contention.

Operationally:
- For stress tests, fund **one Kaspa address per worker** and run `kaspa_utxo_mode=chain`.
- Do not reuse an address that already has millions of UTXOs.

### EL Gas Funding

Your load budget is dominated by EL gas cost per L2 tx.
Always check `eth_gasPrice` / EIP-1559 rules on the target EL before assuming “cheap enough”.

## Debugging Tips

- Check chain id:
  - `cast chain-id --rpc-url "$EL_RPC_URL"`
- Check EL gas price:
  - `cast rpc --rpc-url "$EL_RPC_URL" eth_gasPrice`
- Inspect local IGRA lifecycle cache:
  - `cast igra-status` (uses local tx-map cache)
- Kaspa UTXO visibility:
  - `kaspa_utxos <grpc_url> <addr1> [addr2...]`

## Common Failures (What They Mean)

- **“insufficient Kaspa UTXOs for fee payment”**
  - the configured Kaspa address has no spendable UTXOs, or you derived the wrong address (passphrase/index mismatch).
- **Prefix mining timeouts**
  - CPU too slow for the configured prefix length and `mining_timeout_secs`.
- **Kaspa “not standard” errors after funding**
  - you attempted to create too-small outputs; fund with >= 1 KAS per worker (rule-of-thumb) to avoid storage-mass issues.
- **High-rate multi-worker failures with a shared Kaspa key**
  - UTXO contention / double-spend; use one Kaspa key per worker.

## Source Pointers (Code)

- IGRA transport wrapper + submission path:
  - `crates/common/src/provider/igra_transport.rs`
- Provider wiring / builder:
  - `crates/common/src/provider/mod.rs`
- Testnet smoke runner:
  - `scripts/igra/testnet-smoke.sh`
- (Low TPS) stress runner using `cast send` per tx:
  - `scripts/igra/testnet-stress.sh`
- High-throughput loadgen:
  - `crates/igra-loadgen/src/main.rs`
- Kaspa helpers:
  - `crates/common/src/bin/kaspa_utxos.rs`
  - `crates/common/src/bin/kaspa_fund.rs`
