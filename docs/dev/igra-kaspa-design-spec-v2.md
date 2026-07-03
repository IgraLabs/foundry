# IGRA-Kaspa Foundry Fork Design Spec (v2)

## Status

This document defines the v2 architecture and implementation plan before code changes.

v2 introduces two mandatory changes:

1. Kaswallet integration is in-process (library/runtime), not an external daemon/process.
2. Kaspa key UX follows EVM-style CLI/ENV patterns, with explicit `--private-key-kaspa` support.

Additional mandatory behavior:

- If IGRA mode is enabled and the user does not provide explicit Kaspa key material, Foundry defaults to the same signer material as EVM signer inputs (same private key or same mnemonic/keystore source).

## Document Scope

This is a companion design document to `docs/dev/igra-kaspa-integration-plan.md`.

1. Integration plan remains authoritative for full operational detail:
   - Complete lifecycle semantics
   - Full error catalog and code mapping
   - Deterministic E2E matrix and CI gates
2. This v2 document is authoritative for the two architectural deltas:
   - In-process kaswallet runtime (no external process)
   - EVM-like Kaspa key UX and fallback behavior
3. For overlapping topics, this document references integration-plan sections explicitly to avoid divergence.

## Quick Navigation

Key v2 changes:

1. Section 3: in-process runtime (`kaswallet` as dependency, no daemon/process).
2. Section 4: Kaspa key UX (`--private-key-kaspa`, fallback to EVM signer material).

Cross-references:

1. Complete config semantics: integration plan Section 1.3.
2. Lifecycle state machine: integration plan Section 8.1.
3. Full error catalog and codes: integration plan Section 12.
4. Full deterministic E2E matrix: integration plan Section 17.2.
5. Full implementation phases and CI gates: integration plan Section 18.

## 1. Goals and Non-Goals

### 1.1 Goals

1. Keep Foundry UX familiar for `cast send`, `forge create --broadcast`, `forge script --broadcast`.
2. Route write-path transactions through Kaspa payload submission transparently in IGRA mode.
3. Remove runtime dependency on external `kaswallet` process management.
4. Provide deterministic key selection semantics that are easy to reason about and debug.

### 1.2 Non-Goals (v2)

1. No browser wallet auto-send support in IGRA mode.
2. No unlocked node `eth_sendTransaction` path in IGRA mode.
3. No custom cancellation flow beyond normal Ethereum nonce replacement semantics.

## 2. Mode Activation and Config Contract

### 2.1 IGRA Mode Activation

IGRA mode is active when any of the following is set:

1. CLI: `--igra`
2. ENV: `FOUNDRY_IGRA_ENABLED=true`
3. `foundry.toml`: `igra.enabled = true`

Per-key precedence:

1. CLI
2. ENV
3. `foundry.toml`
4. Built-in defaults

### 2.2 Config Reference for v2

This section includes the complete operational keys from the integration plan, plus v2 key-source changes.

```toml
[igra]
enabled = false
network_profile = "testnet-10" # mainnet | testnet-10 | devnet | simnet | custom

# Required when enabled (unless profile supplies defaults)
el_rpc_url = ""
kaspa_rpc_url = ""
expected_el_chain_id = 0
kaspa_network = "testnet-10"

# Submission and finality
tx_id_prefix = "97b4" # testnet-10 (galleon-testnet). Use "97b1" for mainnet.
kaspa_submit_timeout_secs = 30
mining_timeout_secs = 120
kaspa_acceptance_confirmations = 0
el_receipt_timeout_secs = 300
el_confirmations = 1

# Payload policy
max_l2_tx_bytes = 131072
max_kaspa_compute_mass = 80000
payload_compression = "none" # v1 only: UnzippedPayload (0x94). ZippedPayload is not implemented.

# Retry and safety
max_retries = 3
max_network_retries = 5
retry_backoff_ms = 250
retry_backoff_max_ms = 5000
max_reorg_depth = 64
sender_lock_timeout_secs = 60

[igra.kaspa_wallet]
# v2 explicit Kaspa key source options (mirrors EVM UX style)
private_key = ""
mnemonic = ""
mnemonic_passphrase = ""
mnemonic_derivation_path = ""
mnemonic_index = 0
keystore = ""
keystore_account = ""
password_env = "KASPA_PASSWORD"
password_cache_ttl_secs = 0

[igra.wallet]
# Advanced Kaspa derivation settings from integration plan
master_path = "m/44'/111111'/0'"
allow_custom_master_path = true
cosigner_index = 0
account_count = 1
address_gap_limit = 64

[igra.cache]
dir = "~/.foundry/cache/igra"
ttl_secs = 20
completed_retention_hours = 168
failed_retention_hours = 720
max_db_size_mb = 512

[igra.rpc]
kaspa_qps = 20
el_qps = 20
burst = 40
```

### 2.3 Network Profile Defaults

`testnet-10` defaults:

1. `el_rpc_url = https://galleon-testnet.igralabs.com:8545`
2. `kaspa_rpc_url = grpc://kaspa-testnet-rpc.example.com:16210`
3. `kaspa_network = testnet-10`
4. `tx_id_prefix = 97b4` (Viaduct Transaction ID Prefix)
4. `el_confirmations = 1`

`mainnet` defaults:

1. `kaspa_network = mainnet`
2. `tx_id_prefix = 97b1` (Viaduct Transaction ID Prefix)
2. `el_confirmations = 12`
3. Requires explicit `el_rpc_url` and `kaspa_rpc_url`

`devnet` and `simnet` defaults:

1. Short operational timeouts
2. Requires explicit `el_rpc_url` and `kaspa_rpc_url`

`custom` profile:

1. All required keys must be explicit

`expected_el_chain_id` behavior:

1. If `expected_el_chain_id > 0`, fail fast when EL reports a different `eth_chainId`.
2. If `expected_el_chain_id = 0`, read `eth_chainId` at startup and treat it as the session expected value.

Note: On galleon-testnet, `eth_chainId` currently returns `0x97b4` (38836).

### 2.4 Fail-Fast Validation

When IGRA mode is enabled, command startup fails before execution if:

1. `el_rpc_url` missing/unreachable.
2. `kaspa_rpc_url` missing/unreachable.
3. EL chain ID mismatches `expected_el_chain_id` (when configured).
4. Kaspa node network mismatches `kaspa_network`.
5. Mainnet/testnet cross-combination is detected.
6. Required key material cannot be resolved.
7. `tx_id_prefix` is invalid hex or empty.
8. Payload size, retry, or timeout settings are out of bounds.

### 2.5 RPC Fingerprints

Cache invalidation and environment checks use:

- `el_rpc_fingerprint = sha256(el_rpc_url || el_chain_id || network_profile)`
- `kaspa_rpc_fingerprint = sha256(kaspa_rpc_url || kaspa_network || network_profile)`

Fingerprint changes force cache invalidation for tx mapping and UTXO snapshots.

## 3. In-Process Kaswallet Architecture

### 3.1 Hard Requirement

No external process invocation for submission path.

Disallowed in IGRA write path:

1. `std::process::Command` to call `kaswallet` binary.
2. Sidecar daemon assumptions.

Required in IGRA write path:

1. `kaswallet` and `rusty-kaspa` linked as Rust dependencies.
2. Submission/mining/UTXO logic called via in-process APIs.

### 3.2 Dependency Source and Pinning Policy

Use branches only to select provenance, then pin to commit SHA immediately:

1. `IgraLabs/kaswallet`, branch `<kaswallet-utxo-perf-branch>`
2. `IgraLabs/rusty-kaspa`, branch `<rusty-kaspa-dev-branch>`

Rules:

1. Cargo manifests use pinned commit SHAs, not floating branches.
2. Branch names are documentation/provenance only.
3. Update procedure: bump SHA, run integration suite, merge only if green.
4. CI must fail if lockfile dependency commit drifts from pinned manifests.

### 3.3 Runtime Components

#### IgraTransport<T>

Provider-level transport wrapper that intercepts write methods.

1. Access pattern: obtains runtime from process-global `OnceLock<IgraSubmitRuntime>`.
2. Lazy init: runtime created on first write call.
3. Thread safety: multiple transport instances share the same runtime.
4. Error handling: IGRA errors mapped to RPC failures; non-write methods pass through unchanged.

#### KaspaClientPool

Connection pool for Kaspa gRPC endpoints.

1. Pool size: min 2, max 8 connections.
2. Idle timeout: 60 seconds.
3. Reconnection: exponential backoff with jitter.
4. Circuit breaker: 3 consecutive failures triggers 30-second cooldown.
5. Request distribution: round-robin across healthy connections.

#### PrefixMiner

CPU-bound worker pool for payload nonce mining.

1. Default workers: `max(1, num_cpus - 1)`.
2. v2 configurability: fixed default only (future `igra.mining_threads`).
3. Work partitioning: disjoint nonce ranges per worker.
4. Completion: first valid prefix wins; all workers cancel on success/timeout.
5. Progress logs: every 5 seconds in verbose mode.

#### UtxoStateCache

In-memory cache for Kaspa wallet state.

1. Cached data: spendable UTXO set, fee estimates, balance snapshot.
2. TTL invalidation: `igra.cache.ttl_secs`.
3. Event invalidation: insufficient funds, conflict/double-spend, RPC fingerprint change.
4. Coherency: process-local cache only; no cross-process shared memory.

#### IgraTxStore (SQLite)

Persistent mapping and lifecycle state store.

1. Source schema: integration plan Section 9.1.
2. Key tables: `tx_map`, `sender_nonce_state`, `meta`.
3. DB settings: WAL mode, `busy_timeout = 5000ms`, transactional updates.
4. Concurrency: sender-level lock semantics via transactional row updates.

### 3.4 Lifetime Model

1. One runtime instance per process.
2. Reused across sends in the same process.
3. Short-lived commands still benefit from persistent on-disk tx store and warm caches.

### 3.5 Mining Ownership

Prefix mining is executed inside Foundry process by `PrefixMiner` using in-process kaswallet/rusty-kaspa APIs.

## 4. Kaspa Key Management (EVM-Like UX)

### 4.1 Kaspa CLI/ENV Interface

Kaspa-specific options:

1. `--private-key-kaspa` / `KASPA_PRIVATE_KEY`
2. `--mnemonic-kaspa` / `KASPA_MNEMONIC`
3. `--mnemonic-passphrase-kaspa` / `KASPA_MNEMONIC_PASSPHRASE`
4. `--mnemonic-derivation-path-kaspa` / `KASPA_MNEMONIC_DERIVATION_PATH`
5. `--mnemonic-index-kaspa` / `KASPA_MNEMONIC_INDEX`
6. `--keystore-kaspa` / `KASPA_KEYSTORE`
7. `--keystore-account-kaspa` / `KASPA_KEYSTORE_ACCOUNT`
8. `--password-kaspa` / `KASPA_PASSWORD`

These options are required only for IGRA write-path commands when fallback is unavailable.

### 4.2 Signer Resolution Precedence

Kaspa signer resolution order in IGRA mode:

1. Explicit Kaspa CLI options.
2. Explicit Kaspa ENV variables.
3. `foundry.toml` `[igra.kaspa_wallet]`.
4. Fallback to resolved EVM signer material (default behavior).

### 4.3 Default Fallback to EVM Signer Material

If no explicit Kaspa signer is supplied:

1. EVM `--private-key` -> use same 32-byte key for Kaspa signing.
2. EVM mnemonic flow -> derive EVM key bytes, then reuse those bytes as Kaspa key.
3. EVM keystore flow -> decrypt key and reuse same key bytes for Kaspa signing.

This is the default behavior in IGRA mode and matches the required UX.

### 4.3.1 Security Considerations for Key Fallback

`WARNING`: default fallback uses one private key for both EVM and Kaspa.

Implications:

1. Compromise on either chain exposes assets on the other.
2. No cryptographic isolation between execution environments.
3. Single point of failure for both chains.
4. May violate enterprise or custody key-separation policies.

Risk posture:

1. Acceptable: local development, testing, low-risk testnet operation.
2. Needs review: production deployments with meaningful value.
3. Not recommended: enterprise/high-security custody environments.

Production best practice:

```bash
cast send <to> <sig> <args> \
  --igra \
  --private-key "$EVM_KEY" \
  --private-key-kaspa "$KASPA_KEY"
```

### 4.4 Fallback Limitations and Rejections

Fallback fails when EVM signer cannot expose key material (for example hardware-only signing).

Error:

- `IGRA key resolution error: cannot derive Kaspa key from current EVM signer; provide --private-key-kaspa or --mnemonic-kaspa`

### 4.5 Derivation Defaults and Fallback Semantics

Explicit Kaspa mnemonic (`--mnemonic-kaspa`) defaults:

1. Path: `m/44'/111111'/0'/0/0`
2. Index: `0`

### 4.5.1 BIP39 Passphrase Gotcha (Important)

Kaspa mnemonic wallets may be created/imported with a non-empty BIP39 passphrase
(sometimes called "recovery passphrase", "mnemonic passphrase", or "payment passphrase").

This passphrase is part of BIP39 seed derivation. Therefore:

1. Same 12/24 words + different passphrase => different seed => different private keys => different Kaspa deposit address.
2. Empty passphrase (press ENTER) is not equivalent to a non-empty passphrase.

If your `kaspa-cli` or wallet flow asked for a BIP39 passphrase, you must provide it to Foundry:

```bash
export KASPA_MNEMONIC="test test test test test test test test test test test junk"
export KASPA_MNEMONIC_PASSPHRASE="the exact passphrase you used (may be empty)"
```

For the specific case where the passphrase was set to the same 12-word string (as in the referenced testnet setup),
set `KASPA_MNEMONIC_PASSPHRASE="$KASPA_MNEMONIC"` to reproduce the funded deposit address.
3. Result: standard Kaspa BIP-44 behavior.

Fallback from EVM mnemonic:

1. Derive EVM key using configured EVM derivation path/index.
2. Extract 32-byte private key.
3. Reuse those key bytes for Kaspa signing.
4. Apply Kaspa network/address encoding.

Important:

1. Fallback does not re-derive Kaspa key using EVM path string.
2. Fallback preserves "same key material" semantics.
3. Standard Kaspa mnemonic wallets may not show this address because derivation source is EVM path.

Recovery implication:

1. If user only has mnemonic, they can re-derive same key bytes through Foundry EVM mnemonic flow.
2. If needed outside Foundry, import raw key bytes into Kaspa tooling.

### 4.6 Security and Password Handling

1. Keep decrypted secrets in memory only during command lifetime.
2. Zeroize key/password buffers where library types support it.
3. Never print private key or mnemonic in logs.
4. Redact Kaspa key-source fields from debug output.
5. Password prompt/decrypt behavior:
   - Prompt once per command invocation when needed
   - No cache across commands by default (`password_cache_ttl_secs = 0`)
   - `KASPA_PASSWORD` may be used for automation but is less secure

### 4.7 Security Best Practices

Development:

1. Fallback to same key material is acceptable for local/testnet workflows.
2. Use low-value test funds only.
3. Keep `.env` and secret files out of git.

Staging:

1. Prefer separate Kaspa and EVM keys if multiple operators share environments.
2. Rotate exposed test keys regularly.

Production:

1. Do not rely on fallback key reuse for high-value/custodial flows.
2. Provide explicit Kaspa key material (`--private-key-kaspa` or `--mnemonic-kaspa`).
3. Keep `password_cache_ttl_secs = 0` unless a reviewed exception is approved.
4. Alert on repeated `FAILED_PERMANENT` lifecycle outcomes.
5. Maintain key rotation runbook and validation send after rotation.

## 5. Transaction Build and Submission Flow

### 5.1 Write-Path Interception

`IgraTransport<T>` intercepts only `eth_sendRawTransaction`.

Flow:

1. Decode typed tx envelope.
2. Validate tx type:
   - Supported: Legacy (0), EIP-2930 (1), EIP-1559 (2)
   - Rejected: EIP-4844 (3), EIP-7702, unknown future types
3. Validate tx size, chain ID, and signer inputs.
4. Resolve Kaspa signer.
5. Build Kaspa payload transaction.
6. Run prefix mining for payload nonce.
7. Submit to Kaspa node.
8. Persist lifecycle state and hash mapping.
9. Return L2 tx hash.

### 5.2 Payload Layout

Payload bytes:

1. 1-byte header (`version|tx_type`)
2. L2Data bytes:
   - uncompressed raw signed L2 tx bytes (txTypeId=0x4), or
   - zlib-compressed raw signed L2 tx bytes (txTypeId=0x5)
3. 4-byte payload nonce (little-endian)

Payload nonce is mining-only and independent of L2 account nonce.

### 5.3 Prefix Mining Execution

1. Worker default: `max(1, num_cpus - 1)` (single-core systems use 1).
2. v2 leaves worker count non-configurable for simplicity.
3. Timeout from `igra.mining_timeout_secs`.
4. Progress logging every 5s in verbose mode:
   - attempts/sec
   - elapsed time
   - best prefix distance
5. Expected SLO for 2-byte prefix:
   - p50 < 5s
   - p95 < 30s
   - p99 < 120s

Retry scope:

1. Retries apply to mining timeout and transient network errors.
2. No retries for config errors, unsupported tx types, invalid signatures, or insufficient funds.
3. Retry controls: `max_retries`, `max_network_retries`, `retry_backoff_ms`, `retry_backoff_max_ms`.
4. Full retry classes and backoff policy: integration plan Section 15.

### 5.4 Fee and UTXO Handling

1. EL gas semantics remain unchanged for L2 correctness.
2. Kaspa fee is independent and paid by Kaspa signer UTXOs.
3. UTXO selection and fee policy are provided by in-process kaswallet APIs.
4. Insufficient funds fail before broadcast:
   - `IGRA submit error: insufficient Kaspa UTXOs for fee payment`

### 5.5 Concurrent Transaction Ordering and Nonce Gaps

Same-sender ordering is mandatory across concurrent processes:

1. Decode sender + L2 nonce before wrapping.
2. Acquire sender-level lock in `sender_nonce_state`.
3. If `tx.nonce == next_expected_nonce`: proceed.
4. If `tx.nonce > next_expected_nonce`: set `BLOCKED_NONCE_GAP` and wait.
5. If `tx.nonce < next_expected_nonce`: treat as duplicate/replacement path.

Locking rules:

1. Sender-scoped lock timeout: `sender_lock_timeout_secs` (default 60s).
2. On timeout, mark stale attempt orphaned and continue with lock-steal procedure.
3. Different senders proceed independently in parallel.

### 5.6 Performance Tuning

Mining latency factors:

1. Prefix difficulty (`tx_id_prefix` length/pattern).
2. CPU capability and current host load.
3. Retry budgets and timeout values.

Recommended tuning:

1. Keep 2-byte prefix targets for standard latency; longer prefixes increase latency sharply.
2. Increase `mining_timeout_secs` before increasing retry counts for slow hosts.
3. Keep one core free (default worker formula) to reduce RPC starvation.

Cache and storage tuning:

1. Increase `igra.cache.ttl_secs` for stable wallets to reduce repeated UTXO reads.
2. Reduce `ttl_secs` for highly volatile wallets to lower stale-cache windows.
3. Place `igra.cache.dir` on fast local disk for high throughput.

## 6. State, Caching, and Recovery

### 6.1 Persistent Mapping

Store in SQLite:

1. `l2_tx_hash`
2. `kaspa_tx_id`
3. Lifecycle state
4. Sender, L2 nonce, payload nonce
5. Correlation ID format: `{unix_millis}-{pid}-{thread_id}-{random_u32_hex}`
6. Timestamps and retry counters
7. RPC fingerprints (`el_rpc_fingerprint`, `kaspa_rpc_fingerprint`)

Lifecycle states (from integration plan Section 8.1):

1. `RECEIVED_RAW_L2`
2. `BLOCKED_NONCE_GAP`
3. `KASPA_UNSIGNED_CREATED`
4. `KASPA_PREFIX_MINED`
5. `KASPA_SIGNED`
6. `KASPA_BROADCASTED`
7. `KASPA_ACCEPTED`
8. `EL_INDEXED`
9. `EL_RECEIPT_INCLUDED`
10. `EL_CONFIRMED`
11. `FAILED_RECOVERABLE`
12. `FAILED_PERMANENT`

### 6.2 Cache Isolation and Schema Versioning

DB path includes network for isolation:

- `<cache.dir>/tx-map-{kaspa_network}.sqlite`

Examples:

1. `~/.foundry/cache/igra/tx-map-mainnet.sqlite`
2. `~/.foundry/cache/igra/tx-map-testnet-10.sqlite`

Schema versioning:

1. Internal schema version tracked with `PRAGMA user_version`.
2. Current schema version: `1`.
3. Future changes use migrations on open and increment `user_version`.
4. File name does not encode schema version.

### 6.3 Recovery Window and Timeout Behavior

When state is `KASPA_BROADCASTED`, poll EL receipts until `el_receipt_timeout_secs`.

Backoff schedule:

1. 1s
2. 2s
3. 4s
4. 8s
5. cap at 10s

Timeout handling:

1. Move to `FAILED_RECOVERABLE`.
2. Apply retry budget for eligible transient failures.
3. After retry exhaustion, transition to `FAILED_PERMANENT`.
4. Keep mapping for manual inspection and `--resume`.

### 6.4 Cleanup and Retention

1. Retention controls:
   - `completed_retention_hours`
   - `failed_retention_hours`
   - `max_db_size_mb`
2. Cleanup job runs on startup and periodically during long-running commands.
3. Cleanup never deletes rows in active non-terminal states.

## 7. Command UX

### 7.1 Supported IGRA Commands

1. `cast send`
2. `forge create --broadcast`
3. `forge script --broadcast`
4. `forge script --resume`

### 7.2 Example Usage

Explicit Kaspa key:

```bash
cast send <to> <sig> <args> \
  --rpc-url https://galleon-testnet.igralabs.com:8545 \
  --igra \
  --private-key <EVM_KEY> \
  --private-key-kaspa <KASPA_KEY>
```

Fallback to EVM key material:

```bash
cast send <to> <sig> <args> \
  --rpc-url https://galleon-testnet.igralabs.com:8545 \
  --igra \
  --private-key <EVM_KEY>
```

### 7.2.1 Development Workflow (Fallback)

```bash
export EVM_KEY=0x...
cast send <to> <sig> <args> \
  --rpc-url https://galleon-testnet.igralabs.com:8545 \
  --igra \
  --private-key "$EVM_KEY"
```

### 7.2.2 Production Workflow (Separate Keys)

```bash
export EVM_KEY=0x...
export KASPA_KEY=0x...
cast send <to> <sig> <args> \
  --rpc-url https://galleon-testnet.igralabs.com:8545 \
  --igra \
  --private-key "$EVM_KEY" \
  --private-key-kaspa "$KASPA_KEY"
```

### 7.2.3 Explicit Kaspa Mnemonic Workflow

```bash
export EVM_KEY=0x...
export KASPA_MNEMONIC="test test test test test test test test test test test junk"
cast send <to> <sig> <args> \
  --rpc-url https://galleon-testnet.igralabs.com:8545 \
  --igra \
  --private-key "$EVM_KEY" \
  --mnemonic-kaspa "$KASPA_MNEMONIC"
```

## 8. Error Catalog

This section lists v2-critical errors only. For stable error codes and full catalog, see `docs/dev/igra-kaspa-integration-plan.md` Section 12.

v2-critical messages:

1. `IGRA config error: el_rpc_url is required when igra.enabled=true`
2. `IGRA config error: kaspa_rpc_url is required when igra.enabled=true`
3. `IGRA unsupported transaction type: EIP-4844`
4. `IGRA key resolution error: cannot derive Kaspa key from current EVM signer; provide --private-key-kaspa or --mnemonic-kaspa`
5. `IGRA submit error: insufficient Kaspa UTXOs for fee payment`

Integration-plan code families referenced:

1. `IGRA_CFG_*`
2. `IGRA_SIG_*`
3. `IGRA_TX_*`
4. `IGRA_NONCE_*`
5. `IGRA_FEE_*`
6. `IGRA_MINING_*`
7. `IGRA_NET_*`
8. `IGRA_STATE_*`
9. `IGRA_RECOVERY_*`

## 9. Implementation Plan

This phase list focuses on v2 deltas. Integration plan Section 18 adds broader hardening/observability/CI phases.

### Phase 1: Config and CLI Surface

1. Add `KaspaWalletOpts` to shared CLI option structs.
2. Add env bindings for `KASPA_*`.
3. Add config parsing/validation for `[igra.kaspa_wallet]`.
4. Add precedence tests (CLI > ENV > TOML > fallback).

### Phase 2: Key Resolver

1. Implement `KaspaSignerResolver`.
2. Implement explicit-source resolution.
3. Implement fallback from EVM key bytes (private key, mnemonic-derived key, keystore).
4. Add unsupported-signer failure tests.

### Phase 3: In-Process Submit Runtime

1. Remove subprocess submission path.
2. Add `IgraSubmitRuntime` and `PrefixMiner` with kaswallet/rusty-kaspa APIs.
3. Add Kaspa client pool and UTXO cache.
4. Add integration tests for successful submit flow.

### Phase 4: State Store and Recovery

1. Add schema and lifecycle transitions.
2. Add sender lock and nonce-gap ordering.
3. Add recovery polling and timeout transitions.
4. Add restart/resume tests.

### Phase 5: Hardening and UX

1. Add structured error mapping.
2. Add verbose mining progress.
3. Add secret redaction and zeroization checks.
4. Add deterministic harness scenarios for fallback and explicit Kaspa keys.

## 10. Required Test Matrix

These are v2-specific tests. Full E2E scenario coverage remains in integration plan Section 17.2.

### 10.1 Key Resolution Tests

1. Explicit `--private-key-kaspa` overrides everything.
2. `KASPA_PRIVATE_KEY` is used when CLI value is missing.
3. TOML Kaspa key source is used when CLI/ENV are missing.
4. No Kaspa key + EVM private key -> fallback success.
5. No Kaspa key + EVM mnemonic -> fallback success.
6. No Kaspa key + hardware-only EVM signer -> actionable failure.

### 10.2 In-Process Runtime Tests

1. IGRA submit path does not spawn subprocesses.
2. Prefix mining timeout and retry behavior.
3. Insufficient UTXO error mapping.
4. Receipt polling timeout transitions.
5. Sender lock contention and lock-timeout behavior.

### 10.3 End-to-End Tests

1. `cast send` with explicit Kaspa key.
2. `cast send` with fallback to EVM key.
3. `forge script --broadcast` then `--resume` after restart.
4. Network mismatch fail-fast (EL chain ID, Kaspa network).
5. Nonce-gap ordering across concurrent sends from same account.

## 11. Acceptance Criteria for v2

v2 is complete only when all are true:

1. IGRA write path uses in-process kaswallet/rusty-kaspa APIs only.
2. `--private-key-kaspa` and `KASPA_*` variables are functional.
3. Missing explicit Kaspa key correctly falls back to EVM signer material.
4. v2 test matrix (Section 10) is green in CI.
5. No regression for non-IGRA command behavior.
6. Mining SLO meets targets for 2-byte prefix (p50 < 5s, p95 < 30s, p99 < 120s) in deterministic harness.
7. Security checklist passes: secret redaction, zeroization, no secret log leakage.
8. Observability is present per integration plan Section 13 (metrics + structured logs).
9. User-facing docs for IGRA mode are updated.
10. Dependency SHAs are pinned and reproducible in CI.

## 12. Common Issues and Troubleshooting

### 12.1 Cannot derive Kaspa key from current EVM signer

Cause:

1. EVM signer type does not expose raw key material (for example hardware-only).

Remediation:

1. Provide explicit Kaspa key material via `--private-key-kaspa` or `--mnemonic-kaspa`.

### 12.2 Insufficient Kaspa UTXOs for fee payment

Cause:

1. Kaspa wallet has insufficient spendable UTXOs.

Remediation:

1. Fund the Kaspa signer address.
2. Wait for pending spends to settle.
3. Retry after UTXO cache TTL or force refresh via next command cycle.

### 12.3 Mining timeout

Cause:

1. Prefix mining exceeded `mining_timeout_secs` under current CPU load or difficulty.

Remediation:

1. Retry (transient slow periods are common).
2. Increase `igra.mining_timeout_secs`.
3. Reduce host contention (CPU-heavy parallel jobs).

### 12.4 Network mismatch errors

Cause:

1. EL RPC and Kaspa RPC are not on the expected network pair.

Remediation:

1. Verify `igra.network_profile`.
2. Verify `el_rpc_url` and `kaspa_rpc_url`.
3. Set `expected_el_chain_id` explicitly for strict validation.

## 13. Appendix A: Glossary

1. EL (Execution Layer): EVM-compatible IGRA L2 execution chain.
2. Kaspa L1: base-layer chain carrying wrapped IGRA payload transactions.
3. Payload nonce: 4-byte mining field in Kaspa payload (independent from account nonce).
4. L2 nonce: Ethereum account nonce inside signed L2 transaction.
5. RPC fingerprint: hash of RPC endpoint and network identifiers for cache safety.
6. Correlation ID: unique identifier used to trace one submission flow.
7. Lifecycle state: transaction pipeline status from raw tx intake to confirmation/failure.
8. `BLOCKED_NONCE_GAP`: waiting state when a lower nonce from same sender is pending.
9. Sender lock: per-sender coordination lock enforcing nonce order across processes.
