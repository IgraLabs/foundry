# IGRA-Kaspa Integration Plan for Foundry Fork (v2)

## Scope

IGRA L2 is EVM-compatible for execution, but write-path submission is done via Kaspa L1 payload transactions:

- `Kaspa.L1.Tx.payload = IGRA L2 signed raw tx + payload nonce`

Goal: keep Foundry UX familiar (`cast send`, `cast publish`, `forge create --broadcast`, `forge script --broadcast/--resume`) while transparently routing L2 writes through Kaspa.

This document is implementation-focused and resolves the review gaps before coding starts.

## 0. Prerequisites and Resolved Decisions

These decisions are locked for v1 and must be implemented as written.

1. Submission interception is provider/transport-first, not per-command.
2. IGRA mode requires raw signed tx flow (`eth_sendRawTransaction` only).
3. `eth_sendTransaction*` methods are rejected in IGRA mode.
4. Supported L2 tx types: Legacy (0), EIP-2930 (1), EIP-1559 (2).
5. Unsupported in v1: EIP-4844 (3), EIP-7702 and unknown future typed envelopes.
6. Payload nonce is a per-transaction mining nonce (4 bytes), independent from L2 account nonce.
7. Kaswallet and rusty-kaspa are used as in-process dependencies (no user daemon), pinned to commit SHA from:
   - `IgraLabs/kaswallet` branch `<kaswallet-utxo-perf-branch>`
   - `IgraLabs/rusty-kaspa` branch `<rusty-kaspa-dev-branch>`
8. Transaction state and mapping are persisted in SQLite (`WAL`), with cross-process locking.
9. Network safety is fail-fast: EL chain ID and Kaspa network must match selected profile.
10. No dedicated cancellation command in v1; replacement is standard Ethereum nonce replacement semantics.

## 1. IGRA Mode Activation and Config Contract

## 1.1 Enablement

IGRA mode is off by default.

IGRA mode turns on when any is set:

1. CLI: `--igra`
2. Env: `FOUNDRY_IGRA_ENABLED=true`
3. Config: `igra.enabled = true`

## 1.2 Precedence

Per key precedence:

1. CLI flags
2. Environment variables
3. `foundry.toml`
4. Built-in defaults

## 1.3 Required Config and Defaults

```toml
[igra]
enabled = false
network_profile = "testnet-10" # mainnet | testnet-10 | devnet | simnet | custom

# Required when enabled
el_rpc_url = "http://127.0.0.1:8545"
kaspa_rpc_url = "grpc://127.0.0.1:16110"
expected_el_chain_id = 1337
kaspa_network = "testnet-10"

# Mining + submission
tx_id_prefix = "97b4" # testnet-10 (galleon-testnet). Use "97b1" for mainnet.
kaspa_submit_timeout_secs = 30
mining_timeout_secs = 120
kaspa_acceptance_confirmations = 0 # v1 default: mempool acceptance only
el_receipt_timeout_secs = 300
el_confirmations = 1

# Payload/size policy
max_l2_tx_bytes = 131072
max_kaspa_compute_mass = 80000
payload_compression = "none" # v1 only: UnzippedPayload (0x94). ZippedPayload is not implemented.

# Reliability
max_retries = 3 # mining and other transient pre-broadcast failures
max_network_retries = 5 # transient RPC failures before broadcast is accepted
retry_backoff_ms = 250
retry_backoff_max_ms = 5000
max_reorg_depth = 64
sender_lock_timeout_secs = 60

[igra.wallet]
key_source = "mnemonic_env" # mnemonic_env | keys_file
mnemonic_env = "IGRA_MNEMONIC"
keys_file = "~/.kaswallet/testnet-10/keys.json"
password_env = "KASWALLET_PASSWORD"
password_cache_ttl_secs = 0 # 0 means command-lifetime only (default)

# Kaspa derivation defaults (v1)
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

## 1.4 Network Profiles and Switching

Profiles provide defaults for kaspa network, timeout class, and confirmation depth.

1. `mainnet`
   - `kaspa_network=mainnet`
   - `el_confirmations=12`
   - stricter default timeouts
2. `testnet-10`
   - `kaspa_network=testnet-10`
   - `el_confirmations=1`
3. `devnet`
   - `kaspa_network=devnet`
   - short timeouts for local testing
4. `simnet`
   - `kaspa_network=simnet`
   - short timeouts
5. `custom`
   - all required keys must be explicit

Switching is explicit through `--igra-network-profile` or `igra.network_profile`.

## 1.5 Validation and Fail-Fast Errors

When IGRA mode is enabled, startup validation fails before command execution if:

1. `el_rpc_url` missing/unreachable.
2. `kaspa_rpc_url` missing/unreachable.
3. `expected_el_chain_id` mismatches actual `eth_chainId`.
4. `kaspa_network` mismatches Kaspa node reported network.
5. Mainnet/testnet cross-combination detected.
6. wallet source is invalid or secret env vars are missing.
7. `tx_id_prefix` is not valid even-length hex or is empty.
8. `max_l2_tx_bytes` or timeouts are out of bounds.

Example errors:

- `IGRA config error: expected EL chain_id=1337, got 1`
- `IGRA config error: kaspa_network=testnet-10 but node reports mainnet`
- `IGRA config error: wallet.key_source=mnemonic_env requires IGRA_MNEMONIC`

## 1.6 Wallet Password Caching Lifetime

v1 policy:

1. Password is loaded from `password_env` and kept in memory only for the command lifecycle.
2. Default `password_cache_ttl_secs=0` means no extra cache beyond active command execution.
3. If `password_cache_ttl_secs > 0` is enabled later, cache remains process-local only and is never persisted.
4. Password material must be zeroized on drop where supported by library types.

## 2. Interception Architecture (Provider/Transport First)

Primary interception is centralized in provider builders:

- `crates/common/src/provider/mod.rs:305`
- `crates/common/src/provider/mod.rs:367`

Add `IgraTransport<T>` that wraps existing transport and intercepts write methods.

Behavior:

1. Intercept `eth_sendRawTransaction`.
2. Decode/validate raw tx envelope.
3. Submit through in-process IGRA submitter pipeline.
4. Return L2 tx hash as RPC result.
5. Pass through non-write methods unchanged.

Why this shape:

- Covers most Foundry write paths automatically.
- Minimizes fragile command-specific forks.
- Keeps command-level code limited to guardrails and UX.

## 3. Command and RPC Behavior in IGRA Mode

## 3.1 RPC Method Matrix

1. `eth_sendRawTransaction`: supported.
2. `eth_sendTransaction`: rejected.
3. `eth_sendTransactionSync`: rejected.
4. `eth_sendRawTransactionSync`: rejected in v1.

Standard error:

- `IGRA mode requires raw signed transactions; eth_sendTransaction* is not supported`

## 3.2 Signer Compatibility Matrix

Supported now:

1. Local private key.
2. Mnemonic/keystore signers that yield raw tx bytes.
3. Hardware signers only through raw-sign flow.

Rejected in v1 (clear error):

1. `--unlocked` flows.
2. Browser wallet flows that perform wallet-side send.

Affected paths to guard explicitly:

- `crates/cast/src/cmd/send.rs:287`
- `crates/script/src/broadcast.rs:141`
- `crates/forge/src/cmd/create.rs:170`

## 4. Transaction Model

## 4.1 Supported Ethereum Envelope Types (v1)

1. Legacy (type 0): supported.
2. EIP-2930 (type 1): supported.
3. EIP-1559 (type 2): supported.
4. EIP-4844 (type 3): rejected.
5. EIP-7702: rejected.

Reject message for unsupported typed tx:

- `IGRA unsupported transaction type: EIP-4844 (blob transactions)`

## 4.2 Payload Format

Payload bytes:

1. 1 byte: `version(4 bits) | tx_type(4 bits)`
2. variable: raw L2 signed tx bytes
3. 4 bytes: payload mining nonce (big-endian)

## 4.3 Nonce Semantics and Concurrency

Important distinction:

1. L2 nonce: inside signed Ethereum tx (account ordering/replacement semantics).
2. Payload nonce: 4-byte mining search field for Kaspa tx-id prefix.

Rules:

1. Payload nonce is independent from L2 nonce.
2. Payload nonce starts at `0` for each submission attempt and is mined until prefix matches.
3. Payload nonce has no cross-transaction uniqueness requirement.
4. Final mined payload nonce is persisted for replay/debug only.
5. Nonce gaps are only relevant for L2 account nonce, not payload nonce.
6. On retry after timeout/failure, payload nonce search restarts from `0` (no continuation from prior attempt).

## 4.4 L2 Nonce Gap Handling

Per sender address, IGRA submitter tracks L2 nonce flow:

1. Decode sender + L2 nonce from raw tx before Kaspa wrapping.
2. If nonce == next expected pending nonce -> submit immediately.
3. If nonce > expected -> move to `BLOCKED_NONCE_GAP` and wait for missing nonce(s).
4. If nonce < expected -> treat as possible replacement/duplicate path.

This prevents out-of-order same-account submission across parallel processes.

## 5. Gas, Fee, and UTXO Policy

## 5.1 EL Gas Estimation Strategy

In IGRA mode:

1. Gas estimation remains EL-based (`eth_estimateGas`) before signing.
2. If user provides gas fields manually, respect them.
3. If EL estimation unavailable, fail with explicit remediation.

## 5.2 Kaspa Fee Strategy

Kaspa fee payment is independent from EVM gas and paid by configured Kaspa wallet.

Policy:

1. Query Kaspa `get_fee_estimate()`.
2. Use normal bucket fee rate, enforcing minimum 1.0 sompi/gram.
3. Apply optional max fee cap from config/CLI.
4. Reject if estimated fee exceeds configured max bound.
5. EIP-1559 priority/max fee fields do not map to Kaspa fee settings; Kaspa fee is independent.

## 5.3 UTXO Selection Strategy

v1 strategy (aligned with kaswallet behavior):

1. Exclude pending UTXOs.
2. Select spendable UTXOs from smallest upward.
3. Prefer at least two inputs when change is needed and feasible.
4. Keep change target above minimum practical threshold.
5. Enforce standard transaction mass bounds.

## 5.4 Insufficient Funds and UTXO Edge Cases

Explicit user errors:

1. No UTXOs available.
2. All UTXOs pending.
3. All spendable UTXOs are dust under current fee rate.
4. Fee + amount exceeds available balance.

Remediation included in message:

- fund Kaspa wallet
- wait for pending UTXOs
- reduce fee cap or tx size
- run consolidation command (future)

## 5.5 Fragmentation and Consolidation

v1 behavior:

1. Auto-split/compound when mass would exceed standard limit.
2. No standalone `cast kaspa-consolidate` command yet.
3. Emit warning when fragmentation materially degrades send latency.

## 6. Transaction Size and Payload Limits

Preflight limits in IGRA mode:

1. `raw_l2_tx.len() <= max_l2_tx_bytes` (default 128 KiB).
2. Estimated Kaspa compute mass must be `<= max_kaspa_compute_mass` (default 80,000).
3. If limit exceeded, reject before mining/signing.

Compression policy:

1. `payload_compression=off` in v1 (deterministic behavior first).
2. Compression is future work after compatibility test coverage.

## 7. Prefix Mining Policy

## 7.1 Mining Execution

1. CPU-intensive nonce search runs in blocking worker threads.
2. Base strategy: single lane per tx.
3. Optional parallel lane count configurable later (`igra.mining_threads`).

## 7.2 Expected Durations (SLO for default 2-byte prefix)

Target baseline on modern developer hardware:

1. p50 < 5s
2. p95 < 30s
3. p99 < 120s

These are operational targets, not protocol guarantees.

## 7.3 Timeout and Fallback

1. Hard timeout at `mining_timeout_secs`.
2. On timeout, mark `FAILED_RECOVERABLE`.
3. Retry with bounded exponential backoff and jitter.
4. Retry budget is classed:
   - mining timeout/exhaustion: up to `max_retries`
   - transient pre-broadcast RPC errors: up to `max_network_retries`
   - post-broadcast receipt/indexing waits: bounded by `el_receipt_timeout_secs` (time-based, not retry-count based)
5. After retry/time budget exhausted -> `FAILED_PERMANENT`.

## 7.4 User Progress for Long Operations

For interactive commands (`cast send`, `forge script --broadcast`):

1. Print periodic progress every 5s (nonces tried, hash rate, elapsed).
2. Show correlation ID and current lifecycle state.

## 8. Lifecycle State Machine and Return Semantics

## 8.1 Persisted States

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

`KASPA_ACCEPTED` definition in v1:

1. Reached when Kaspa node confirms the tx is accepted into mempool.
2. No Kaspa block confirmation wait in v1 (`kaspa_acceptance_confirmations=0`).
3. EL confirmation remains the final success gate for default synchronous UX.

## 8.2 Command Return Behavior

1. `cast send --async`: return after `KASPA_BROADCASTED`, print L2 hash + correlation ID.
2. `cast send` default: wait until `EL_CONFIRMED`.
3. Receipt/indexer polling window starts at `KASPA_BROADCASTED` and runs for `el_receipt_timeout_secs`.
4. Polling backoff is exponential with cap: `1s, 2s, 4s, 8s, 10s...` until timeout.
5. `cast send --sync`: rejected in IGRA mode.
6. `forge create --broadcast`: wait for `EL_CONFIRMED`.
7. `forge script --broadcast`: record L2 hashes in sequence, map via SQLite.
8. `forge script --resume`: recover via persisted state + mapping.

## 8.3 Network Partition Scenarios

1. EL reachable, Kaspa unreachable:
   - fail before wrapping/broadcast, `FAILED_RECOVERABLE`.
2. Kaspa reachable, EL unreachable:
   - proceed to Kaspa broadcast, then poll EL later until timeout.
3. both unreachable:
   - immediate fail with retriable classification.

## 8.4 Reorg Handling

1. Re-validate receipt block hash while waiting confirmations.
2. If receipt disappears, move back to `EL_INDEXED` polling.
3. If reorg depth exceeds `max_reorg_depth`, classify permanent failure and require manual resend.

## 8.5 Time and Clock Requirements

1. Use monotonic clocks (`Instant`) for all timeout accounting.
2. Use wall clock only for logs/metadata.
3. Timeout logic must be robust under system clock drift.

## 9. Persistence, Cache, and GC

## 9.1 Storage Schema

SQLite path:

- `<igra.cache.dir>/tx-map-v1-{kaspa_network}-{expected_el_chain_id}.sqlite`

`tx_map` required columns:

1. `l2_tx_hash` (PK)
2. `sender`
3. `l2_nonce`
4. `payload_nonce`
5. `kaspa_tx_id`
6. `state`
7. `chain_id`
8. `kaspa_network`
9. `el_rpc_fingerprint`
10. `kaspa_rpc_fingerprint`
11. `correlation_id`
12. `attempts`
13. `last_error_code`
14. `last_error_message`
15. `origin`
16. `sequence_path`
17. `sequence_index`
18. `created_at_ms`
19. `updated_at_ms`

Auxiliary table:

- `sender_nonce_state(sender PRIMARY KEY, next_expected_nonce, updated_at_ms)`
- `sender_locks(sender PRIMARY KEY, owner_id, lease_until_ms, updated_at_ms)`

RPC fingerprint definition:

1. `el_rpc_fingerprint = sha256(canonical_el_url || "|" || actual_el_chain_id || "|" || "el")`
2. `kaspa_rpc_fingerprint = sha256(canonical_kaspa_url || "|" || actual_kaspa_network || "|" || "kaspa")`
3. Fingerprints are computed from runtime-resolved identity, not only config text values.

## 9.2 Concurrency and Locking

1. SQLite `WAL` mode + `busy_timeout`.
2. Transactional update for state transitions.
3. Sender-level locking uses lease rows in `sender_locks`, not assumed row-level DB locks.
4. Lock acquire algorithm:
   - `BEGIN IMMEDIATE`
   - delete expired lease rows (`lease_until_ms < now_ms`)
   - attempt `INSERT sender_locks(sender, owner_id, lease_until_ms, updated_at_ms)`
   - on conflict, retry with backoff until `sender_lock_timeout_secs`
5. Lease renewal (heartbeat) occurs while processing long operations.
6. On lock timeout, command fails with `IGRA_NONCE_002` and does not submit out-of-order.
7. Deadlocks are avoided by design because only one sender lock is acquired at a time.
8. Atomic writes only; no mutable JSON transaction cache.

## 9.3 Cache Invalidation

Invalidate wallet/UTXO cache on:

1. insufficient funds
2. pending/double-spend conflict
3. endpoint fingerprint change
4. chain/network mismatch
5. TTL expiry
6. DB path profile mismatch (different `{kaspa_network, expected_el_chain_id}`)

## 9.4 Cleanup and Retention

1. Completed rows retained `completed_retention_hours` (default 7 days).
2. Failed rows retained `failed_retention_hours` (default 30 days).
3. Background GC runs at startup and periodically for long-running commands.
4. If DB exceeds `max_db_size_mb`, prune oldest completed first, then failed.
5. No destructive compaction during active send path; vacuum only in maintenance window.

## 9.5 Schema Versioning and Migration

Migration strategy:

1. `schema_meta(version INTEGER PRIMARY KEY, applied_at_ms INTEGER)` table is mandatory.
2. Migrations are forward-only and transactional.
3. v2+ schema additions use additive `ALTER TABLE ... ADD COLUMN` where possible.
4. Existing rows are preserved; default values are backfilled deterministically.
5. On startup, if migration fails, IGRA mode fails fast with actionable error and no partial write-path execution.

## 10. Ordering, Replacement, and Cancellation

## 10.1 Ordering Guarantees

For same sender:

1. Local and cross-process sends are serialized by sender lease lock (`sender_locks`).
2. Submission order follows L2 nonce order.
3. Higher nonce tx waits in `BLOCKED_NONCE_GAP` until predecessor nonce is observed/submitted.

For different senders:

- fully parallel.

## 10.2 Replacement and Cancel Semantics

1. No special Kaspa RBF feature in v1.
2. Ethereum-style replacement is supported: new signed tx with same `(sender, l2_nonce)` and higher fee.
3. Replacement creates a new `l2_tx_hash`; mapping keeps both hashes and marks superseded row.
4. "Cancel" is user-level replacement with noop/self-transfer tx at same nonce.

## 11. User-Facing Introspection and Dry-Run

## 11.1 New Status Command

Add:

- `cast igra-status <l2_tx_hash>`

Output includes:

1. lifecycle state
2. kaspa tx id
3. sender + l2 nonce
4. attempts + last error
5. elapsed time and correlation ID

## 11.2 `cast receipt` Integration

`cast receipt <l2_hash>` in IGRA mode also shows bridge metadata when available:

1. `kaspa_tx_id`
2. `kaspa_block_hash` (if mined)
3. `igra_state`
4. `mining_duration_ms`
5. `correlation_id`

## 11.3 Dry-Run / Simulation

Add:

- `cast send --igra-dry-run`
- `forge script --broadcast --igra-dry-run`

Dry-run performs:

1. envelope parse and type validation
2. chain/network safety checks
3. payload size and mass checks
4. kaspa fee estimation
5. nonce-gap classification

No Kaspa submission or EL write occurs.

## 12. Error Catalog (v1)

Each error has stable code, message, and remediation hint.

1. `IGRA_CFG_001`: missing required config key.
2. `IGRA_CFG_002`: EL chain ID mismatch.
3. `IGRA_CFG_003`: Kaspa network mismatch.
4. `IGRA_SIG_001`: unsupported signer mode (`--unlocked` or browser).
5. `IGRA_TX_001`: unsupported transaction envelope type.
6. `IGRA_TX_002`: payload too large / mass too high.
7. `IGRA_NONCE_001`: blocked nonce gap.
8. `IGRA_NONCE_002`: sender lock acquisition timeout.
9. `IGRA_FEE_001`: insufficient Kaspa funds.
10. `IGRA_FEE_002`: fee policy below minimum.
11. `IGRA_MINING_001`: mining timeout.
12. `IGRA_NET_001`: Kaspa RPC unavailable.
13. `IGRA_NET_002`: EL RPC unavailable during receipt wait.
14. `IGRA_STATE_001`: persistence/state transition conflict.
15. `IGRA_RECOVERY_001`: retry budget exhausted.

All user-facing errors include `action=` guidance and `correlation_id=`.

## 13. Observability and Operations

## 13.1 Metrics

Expose at minimum:

1. `igra_tx_submissions_total{origin,state}`
2. `igra_tx_failures_total{code}`
3. `igra_mining_duration_seconds`
4. `igra_mining_hash_rate`
5. `igra_receipt_latency_seconds`
6. `igra_utxo_select_duration_seconds`
7. `igra_db_lock_wait_seconds`
8. `igra_retry_attempts_total`

## 13.2 Structured Logs

Every submission log event includes:

1. `correlation_id`
2. `l2_tx_hash`
3. `kaspa_tx_id` (when available)
4. `sender`
5. `l2_nonce`
6. `state`
7. `attempt`
8. `elapsed_ms`
9. `error_code` (if any)

Correlation ID format:

1. `{unix_millis}-{pid}-{thread_id}-{counter}-{random_u32_hex}`
2. Counter is process-local monotonic atomic.
3. Example: `1707648123456-98765-1-42-a3f2b8c1`

## 13.3 Debug Mode

`FOUNDRY_IGRA_DEBUG=1` enables:

1. per-state transition logs
2. mining progress logs
3. RPC retry traces
4. cache invalidation reasons

## 14. Security Requirements

1. Mnemonic/password are never logged.
2. Sensitive buffers are zeroized after use where library support exists.
3. Secrets are loaded from env/file only (no CLI echo).
4. Key material lives in-process only; no long-lived external signer daemon required.
5. Strict file permission checks for keys file.
6. Correlation IDs must not embed secrets.

## 15. Rate Limiting and Backoff

1. Separate token buckets for EL RPC and Kaspa RPC.
2. Default limits from `[igra.rpc]` section.
3. Bounded exponential backoff with jitter.
4. Retry only for classified transient failures.
5. Hard-stop on deterministic user/config errors.

## 16. Dependency Policy

Pin immediately in the integration branch (before first green):

1. Add git dependencies pinned to commit SHA.
2. Keep branch names only as provenance notes.
3. CI validates lockfile and fails if SHA drifts unexpectedly.

Update procedure:

1. bump SHA
2. run full deterministic IGRA suite
3. merge only if suite is green

## 17. Deterministic Test Harness

## 17.1 Required Local Stack

1. Kaspa node (selected network profile)
2. IGRA EL node
3. indexer/bridge component that reflects Kaspa payload tx into EL (configurable lag)
4. deterministic funded wallets from test mnemonic

## 17.2 Required Scenarios

1. cold start `cast send`
2. warm cache `cast send`
3. concurrent sends same sender (ordering + gap handling)
4. concurrent sends different senders (parallel)
5. `forge create --broadcast`
6. `forge script --broadcast` multi-tx
7. forced crash then `forge script --resume`
8. EL lag with Kaspa already broadcasted
9. EL reorg within depth
10. EL reorg beyond depth (permanent failure)
11. Kaspa RPC outage and recovery
12. EL RPC outage and recovery
13. insufficient fee funds and remediation error path
14. oversized payload rejection
15. replacement tx (same sender+nonce, higher fee)
16. dry-run command behavior
17. DB GC retention behavior
18. schema migration test for `tx-map-v1`
19. sender lock lease expiry + reacquire correctness across two processes
20. cache isolation across `testnet-10` and `mainnet` DB files

## 18. Step-by-Step Implementation Plan

## Phase 0: Config Contract + Guardrails

1. Implement full `igra` config schema and precedence.
2. Add profile-aware chain/network validation.
3. Add signer/method guardrails and stable error codes.

Deliverable: deterministic startup validation and clear user failures.

## Phase 1: Transport Interception

1. Add `IgraTransport<T>`.
2. Inject in provider build paths.
3. Intercept `eth_sendRawTransaction`; reject unsupported methods.

Deliverable: centralized interception active in IGRA mode.

## Phase 2: Submit Pipeline Core

1. Decode envelope, validate tx type, extract sender/l2 nonce.
2. Build Kaspa payload and run prefix mining.
3. Sign/broadcast via in-process kaswallet.

Deliverable: raw tx write path works end-to-end.

## Phase 3: Nonce Ordering + Lifecycle Persistence

1. Add `tx_map` + `sender_nonce_state` tables.
2. Implement sender lock + nonce-gap queue.
3. Persist transitions and recovery metadata.

Deliverable: restart-safe ordering and resume semantics.

## Phase 4: Cache, GC, and Recovery

1. Add UTXO/session cache with invalidation triggers.
2. Implement retention GC and DB size caps.
3. Add bounded retry engine with transient/permanent classification.

Deliverable: short-lived commands improve latency without correctness regressions.

## Phase 5: UX and Introspection

1. Add `cast igra-status`.
2. Extend `cast receipt` output with IGRA metadata.
3. Add `--igra-dry-run` behavior for cast/forge broadcast flows.

Deliverable: user-visible debuggability and safe simulation mode.

## Phase 6: Observability and Security Hardening

1. Add metrics, structured logs, and debug mode.
2. Add key handling hardening (zeroization, permission checks).
3. Add rate limiter/backoff and runbook docs.

Deliverable: operable and secure release candidate.

## Phase 7: Deterministic E2E and CI Gates

1. Build reproducible local integration stack scripts.
2. Add required scenario matrix to CI.
3. Enforce dependency SHA pinning and compatibility checks.

Deliverable: merge gate for IGRA mode is fully deterministic and repeatable.
