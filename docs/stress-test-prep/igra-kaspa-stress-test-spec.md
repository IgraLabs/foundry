# IGRA-Kaspa Stress Test Spec (1000 Wallets, 1000 Contracts)

## 1. Goal and Pass/Fail Criteria

Define a reproducible stress test where:

- `target_tps` is an input parameter.
- Load is distributed across Foundry workers.
- Each worker uses exactly one EVM wallet, one Kaspa wallet, and one contract address.
- Mapping is index-based and uses `wallet_start_index`.

Primary target example: `500 TPS` for long-duration runs (for example 12h).

Scope note:

- This spec is devnet-first. `testnet-10` is supported as a selectable network mode, but
  operational testnet runbooks are out of scope for this revision.

### 1.1 Campaign pass criteria

A campaign is considered **PASS** only if all are true:

- Sustained throughput: aggregate TPS is at least `95%` of `target_tps` for at least `95%` of 1-minute windows.
- Reliability: end-to-end failed tx ratio is at most `1.0%` using the normalized denominator below.
- Stability: no more than `2%` of workers are stalled for longer than `60s` at any point.
- Mapping correctness: zero wallet/contract index collisions and zero out-of-range indices.

Otherwise campaign is **FAIL** and must be treated as capacity or implementation regression.

### 1.2 Failure taxonomy (required)

For consistency across teams, reliability must use:

- `accepted`: final success acknowledged by IGRA/Kaspa completion flow.
- `rejected`: validation/protocol rejection with terminal error.
- `timeout`: no terminal result within configured terminal timeout.
- `dropped`: never accepted and never terminally rejected but aged out.
- `terminal_error`: internal runner error where tx cannot progress.

Examples:

- `rejected`: EVM revert, invalid nonce, fee below enforced minimum.
- `timeout`: no terminal result within configured timeout window.
- `dropped`: mempool-evicted or never finalized before shutdown grace expires.
- `terminal_error`: RPC unavailable, signing failure, unrecoverable worker runtime error.

Normalized failure ratio:

- `failure_ratio = (rejected + timeout + dropped + terminal_error) / (accepted + rejected + timeout + dropped + terminal_error)`

Replacement handling:

- A superseded tx attempt for nonce `n` is **not** counted as failure if another replacement for same
  nonce `n` is later accepted.

## 2. Fixed Inputs and Artifacts

### 2.1 Wallet source

`docs/stress-test-prep/wallets_1000.json`

Each entry includes:

- `mnemonic`
- `kaspa_private_key`
- `kaspa_address`
- `ethereum_private_key`
- `ethereum_address`

Index range is `0..999`.

### 2.2 DevOps-provided runtime infrastructure (required)

Runner assumes DevOps already provides:

- Reth node up/running, with required wallet and contract pre-funding applied.
- Kaspad for the selected stress network up/running, with required wallet pre-funding applied.
- Kaspa miner(s) running so Kaspa network is operational.

Runner scope starts only after this infra is healthy.

### 2.3 RPC endpoint list JSON (required)

Runner accepts DevOps-provided JSON with endpoint lists for both layers:

```json
{
  "igra_rpc_urls": ["https://igra-rpc-1:8545"],
  "kaspa_rpc_urls": ["grpc://kaspa-rpc-1:16210"]
}
```

Rules:

- Single endpoint per list is valid.
- Multiple endpoints per list is valid.
- Empty list is invalid.

### 2.4 EVM alloc fragment (reference)

`docs/stress-test-prep/reth_alloc_1000eth.json`

- Reth genesis `alloc` fragment.
- Pre-funds EVM addresses from `wallets_1000.json` with `1000 iKAS` each.

Reference artifact only; runtime provisioning is DevOps-owned.

### 2.5 Contract address range

1000 independent predeployed contracts are contiguous:

- Start: `0x0000000000000000000000000000000000005000`
- End: `0x00000000000000000000000000000000000053e7`
- Count: `0x53e7 - 0x5000 + 1 = 0x3e8 = 1000`

Contract for wallet index `i`:

- `contract_addr(i) = 0x0000000000000000000000000000000000005000 + i`
- Addresses are represented in canonical lowercase hex for deterministic generation; checksum
  formatting is optional at display time.

### 2.6 Contract runtime fragment (reference)

Genesis must include contract runtime code for all addresses `0x5000..0x53e7`.

Required shape per address:

```json
{
  "0x0000000000000000000000000000000000005000": {
    "balance": "0x0",
    "code": "0x<runtime_bytecode_hex>"
  }
}
```

Reference artifact only; runtime provisioning is DevOps-owned.

### 2.7 Pinned contract workload (exact behavior)

The stress call target is the EVM-team-provided transfer/balance simulator runtime:

- Runtime bytecode:
  `0x5f3560e01c63a9059cbb81146031576370a0823114601b575f80fd5b60243603602d57600435545f5260205ff35b5f80fd5b60443603604c57600435602435908133555560015f5260205ff35b5f80fd`
- Supported selector `transfer(address,uint256)`: `0xa9059cbb`
- Supported selector `balanceOf(address)`: `0x70a08231`
- Any other selector or invalid calldata length reverts.

Stress workload uses **state-changing** `transfer(address,uint256)` transactions.

Canonical transfer calldata (exact):

- bytes length must be `68` (`4 + 32 + 32`)
- selector: `0xa9059cbb`
- `to` argument: `wallets_1000[(wallet_index + 1) % 1000].ethereum_address`
- `amount` argument: constant `1`

Recipient selection rationale:

- Default recipient mode is deterministic ring mapping (`i -> (i+1)%N`) to keep workload reproducible.
- This can introduce shared-state interaction patterns by design; it is acceptable for devnet-first
  stress because determinism is prioritized over perfect isolation.
- Optional alternative mode is random recipient selection with fixed seed for reproducibility.

State precondition for stable long-run measurements:

- Each worker sends `warmup_txs_per_worker` warm-up transfers (default `1`) before timed run starts.
- Warm-up tx is excluded from TPS/error metrics.
- Use measured steady-state gas from calibration for budgeting and capacity planning.
- Timed phase starts only after warm-up barrier: all workers must report warm-up completion.

Warm-up coordination protocol:

1. All workers enter warm-up phase in parallel.
2. Each worker sends warm-up tx(s) and waits for acceptance.
3. Each worker reports `warmup_complete`.
4. Coordinator waits for all workers or `warmup_barrier_timeout_secs`.
5. If timeout occurs, campaign fails before timed phase.
6. Coordinator emits shared `timed_start_utc`, then all workers start timed phase.

Contract storage initialization and verification:

- Contracts are expected to start from zero storage unless DevOps explicitly defines otherwise.
- Runner preflight must verify contract code exists at sampled addresses in the selected index range.
- If code is missing on any sampled address, campaign must fail fast before worker start.
- First writes are expected to be more expensive than steady-state updates; long-run budgeting must use
  measured steady-state gas from calibration.

Observed gas from EVM team:

- `transfer(address,uint256)` first writes from zero slots: `~65,671 gas`
- `balanceOf(address)` as transaction: `~23,625 gas`

Planning defaults:

- Set tx gas limit to `80,000` for transfer workload.
- First-write transfer cost can be around `~65k`, but long runs should use measured steady-state gas
  from preflight as the source of truth.
- Typical steady-state transfer gas after warm-up is around `~45,000` (empirical baseline, varies
  with storage access patterns and recipient behavior).

## 3. Worker Model and Mapping

### 3.1 Canonical worker unit

One worker is one OS process with:

- one EVM signer,
- one Kaspa signer,
- one contract target.

### 3.2 Mapping rules

For worker ordinal `k` (0-based):

- `wallet_index = wallet_start_index + k`
- `evm_wallet = wallets_1000[wallet_index].ethereum_private_key`
- `kaspa_wallet = wallets_1000[wallet_index].kaspa_private_key`
- `contract_addr = contract_addr(wallet_index)`

Constraints:

- `wallet_index` must stay in `0..999`.
- `wallet_start_index + worker_count <= 1000`.

### 3.3 Multi-host sharding

If workers are split across hosts:

- Each host must be assigned disjoint `wallet_start_index` ranges.
- No shared wallet index across hosts.
- Global mapping remains one-to-one by index.

### 3.4 Wallet ceiling and extension path

- Current dataset ceiling is `1000` wallet pairs, so with baseline `5 TPS/worker` the practical
  ceiling is ~`5000 TPS`.
- To target above this ceiling, provide larger wallet dataset and matching contract range, then run
  the same mapping rules with updated bounds.

## 4. Throughput Model

Conservative default:

- `instance_safe_tps = 5`

Computed workers:

- `worker_count_required = ceil(target_tps / instance_safe_tps)`

Calibration override (recommended):

- If preflight measured worker throughput is available, derive:
  `instance_safe_tps_calibrated = floor(measured_worker_tps_p10 * 0.8)`.
- Use `max(1, min(instance_safe_tps, instance_safe_tps_calibrated))` as effective planning
  per-worker TPS for this campaign.

Examples:

- `target_tps=50` -> `10` workers
- `target_tps=100` -> `20` workers
- `target_tps=500` -> `100` workers

Example with index offset:

- `target_tps=500`, `wallet_start_index=100`
- workers use wallet indices `100..199`
- workers call contracts `0x...5064 .. 0x...50c7`

## 5. Kaspa Prefunding and Optional Fan-Out

Kaspa devnet prealloc (single-address) is supported in rusty-kaspa:

```bash
cargo run --bin kaspad --features devnet-prealloc -- \
  --devnet \
  --num-prealloc-utxos=1000 \
  --prealloc-address=kaspadev:YOUR_ADDRESS \
  --prealloc-amount=10000000000
```

Notes:

- Works on `--devnet` / `--simnet`.
- `--num-prealloc-utxos` and `--prealloc-address` must be set together.
- `--prealloc-amount` is in sompi (`10_000_000_000` = `100 KAS`).
- Prealloc funds a single address; multi-wallet funding requires fan-out.
- For `--network=testnet-10`, this prealloc flow is not available; wallet funding must come from
  normal external funding flow.

### 5.1 Optional fan-out step

Fan-out is optional and controlled by CLI/ENV.

- Use fan-out for first bootstrap or after depletion.
- Skip fan-out for repeated runs if balances/UTXO pools already prepared.

### 5.2 Quantified UTXO depth requirement (`prebuild-send`)

For each worker wallet:

- `worker_tps = per-worker send rate` (default `5`)
- `prebuild_horizon_secs = seconds of prebuilt queue to keep ready`
- `utxo_refill_lag_secs = kaspa_confirm_secs_p95 + rpc_retry_budget_secs`
- `utxo_safety_factor = default 1.5`
- Runner input for this value is `--utxo-refill-lag-secs` (set from recent calibration).

Suggested estimation of `utxo_refill_lag_secs`:

- `utxo_refill_lag_secs ~= p95(kaspa_confirm_latency_secs) + p95(kaspa_utxo_query_latency_secs) + retry_budget_secs`
- Default `20` can be used when calibration data is not yet available.

Minimum UTXO count per wallet:

- `utxos_per_wallet_min = ceil(worker_tps * (prebuild_horizon_secs + utxo_refill_lag_secs) * utxo_safety_factor)`

Example:

- `worker_tps=5`, `prebuild_horizon_secs=120`, `utxo_refill_lag_secs=20`, `utxo_safety_factor=1.5`
- `utxos_per_wallet_min = ceil(5 * (120 + 20) * 1.5) = 1050`

If configured UTXO count is below this minimum, runner must fail fast before launch.

## 6. Stress Modes

### 6.1 Mode A: `full-cycle`

Per tx, worker does full path:

1. Build/sign EVM raw tx.
2. Submit via IGRA write path.
3. Mine Kaspa txid prefix and construct Kaspa payload tx.
4. Broadcast and await completion flow.

Use for lower TPS realism and correctness checks.

### 6.2 Mode B: `prebuild-send`

High-throughput mode separates preparation from dispatch:

1. Prebuilder prepares signed tx batches.
2. Sender loop dispatches prebuilt txs.
3. Confirmer tracks receipts/acceptance and updates state.

### 6.3 Nonce and resend rules (required)

`prebuild-send` must use deterministic nonce control:

- Per worker nonce stream is strictly monotonic.
- Prebuilder allocates nonces from `next_pending_nonce` snapshot.
- Sender never emits duplicate nonce unless replacement policy triggers.

Replacement policy:

- `pending_timeout_secs` default `10`.
- If tx with nonce `n` is pending longer than timeout, issue replacement for nonce `n`.
- Increase both `max_fee_per_gas` and `max_priority_fee_per_gas` by at least `10%` per replacement.
- Keep EIP-1559 invariant: `max_priority_fee_per_gas <= max_fee_per_gas`.
- Max replacements per nonce default `5`.
- Timeout backoff per replacement attempt: `10s -> 20s -> 40s` (cap at `60s`).

Gap recovery:

- If worker state and chain nonce diverge, reconcile from RPC nonce.
- Drop stale queued txs above divergence point.
- Rebuild queue from reconciled nonce.

Nonce reconciliation algorithm (required):

```text
expected_nonce = local_next_nonce
rpc_nonce = eth_getTransactionCount(sender, "pending")
if rpc_nonce == expected_nonce: continue
if rpc_nonce > expected_nonce:
  drop queued txs where nonce < rpc_nonce as stale
  reset local_next_nonce = rpc_nonce
  rebuild prebuild queue from rpc_nonce forward
if rpc_nonce < expected_nonce:
  pause sends for sender
  re-query pending nonce with backoff until convergence or timeout
  if timeout: mark worker stalled and trigger recovery policy
```

### 6.4 Worker failure and recovery policy

- First implementation policy is fail-fast for worker loss.
- If any worker exits non-zero or becomes unresponsive beyond `worker_recovery_timeout_secs`
  (default `120`), coordinator stops all workers and marks campaign `FAIL`.
- Runner still emits post-run manifest and partial metrics for analysis.
- Future relaxed policy (continue with N-1 workers) is out of scope for this revision.

## 7. RPC Endpoint Strategy

Support single or multiple RPC endpoints for both EL and Kaspa.

Required behavior:

- Keep persistent client pools per endpoint (no per-request socket creation).
- Selection chooses endpoint from healthy pool.
- Default selection: `random-per-step` over pooled clients.
- Optional selection: `sticky-per-instance`.
- If endpoint fails health checks, apply temporary backoff and retry with another endpoint.

Endpoint resolution order:

1. `--el-rpc-urls` / `--kaspa-rpc-urls` if provided.
2. Else `--rpc-endpoints-json`.
3. Else fail fast before starting workers.

Mid-campaign RPC outage policy:

- If at least one healthy endpoint remains for each layer, campaign continues.
- If all EL endpoints or all Kaspa endpoints are unhealthy, runner enters `degraded_pause` state.
- `degraded_pause_max_secs` default `120`; if exceeded, campaign is marked failed.
- All outage durations and affected workers must be recorded in campaign manifest results.

### 7.1 RPC retry and failover behavior

- Retry/backoff sequence per failed endpoint attempt: `1s, 2s, 4s, 8s` (cap `30s`).
- On failure, rotate request to next healthy endpoint in same layer pool.
- If no healthy endpoint exists for a layer, remain in `degraded_pause` until recovery or timeout.

## 8. CLI and ENV Contract

### 8.1 Core load parameters

- `--network` (`IGRA_STRESS_NETWORK`), default `devnet`, allowed values:
  `devnet|simnet|testnet-10`.
- `--target-tps` (`IGRA_STRESS_TARGET_TPS`), required.
- `--instance-safe-tps` (`IGRA_STRESS_INSTANCE_SAFE_TPS`), default `5`.
- `--worker-count` (`IGRA_STRESS_WORKER_COUNT`), optional explicit override.
- `--wallet-start-index` (`IGRA_STRESS_WALLET_START_INDEX`), default `0`.
- `--worker-recovery-timeout-secs` (`IGRA_STRESS_WORKER_RECOVERY_TIMEOUT_SECS`), default `120`.
- `--degraded-pause-max-secs` (`IGRA_STRESS_DEGRADED_PAUSE_MAX_SECS`), default `120`.

Worker-count resolution:

- If `--worker-count` unset: compute `ceil(target_tps / instance_safe_tps)`.
- If `--worker-count` set and is less than computed required workers: fail fast.
- Optional escape hatch may be added later (`--allow-underprovision`), default behavior is fail.

Network-derived behavior:

- `IGRA_TX_ID_PREFIX` default is derived from selected network profile and may be overridden
  explicitly by `--tx-id-prefix`.
- Kaspa address prefix validation must match selected network (`kaspadev:` / `kaspasim:` /
  `kaspatest:`).

### 8.2 Wallet/contract datasets

- `--wallets-json` (`IGRA_STRESS_WALLETS_JSON`), default `docs/stress-test-prep/wallets_1000.json`.
- `--contract-base-address` (`IGRA_STRESS_CONTRACT_BASE`), default `0x0000000000000000000000000000000000005000`.
- `--contract-end-address` (`IGRA_STRESS_CONTRACT_END`), default `0x00000000000000000000000000000000000053e7`.

### 8.3 Mode and stop condition

- `--mode` (`IGRA_STRESS_MODE`): `full-cycle` or `prebuild-send`.
- `--duration-secs` (`IGRA_STRESS_DURATION_SECS`) or `--total-txs` (`IGRA_STRESS_TOTAL_TXS`).
- `--calibration-mode` (`IGRA_STRESS_CALIBRATION_MODE`): `0|1`, default `0`.
- `--preflight-sample-mode` (`IGRA_STRESS_PREFLIGHT_SAMPLE_MODE`): `0|1`, default `0`.

Stop-condition rules:

- Exactly one of `duration-secs` or `total-txs` must be set.
- If both are set, fail fast.
- If none are set, fail fast.
- If `calibration-mode=1`, runner executes fixed calibration run (`2000` tx default unless
  overridden) and exits with calibration report.
- If `preflight-sample-mode=1`, runner executes fixed fee-sampling run (`1000` tx default unless
  overridden) and exits with budget estimate report.

### 8.4 Kaspa fan-out controls (optional)

- `--kaspa-fanout` (`IGRA_STRESS_KASPA_FANOUT`): `0|1`, default `0`.
- `--kaspa-funder-private-key` (`IGRA_STRESS_KASPA_FUNDER_PRIVATE_KEY`) or mnemonic source.
- `--kaspa-fanout-amount-sompi` (`IGRA_STRESS_KASPA_FANOUT_AMOUNT_SOMPI`).
- `--kaspa-fanout-utxos-per-wallet` (`IGRA_STRESS_KASPA_FANOUT_UTXOS_PER_WALLET`).
- `--prebuild-horizon-secs` (`IGRA_STRESS_PREBUILD_HORIZON_SECS`), default `120`.
- `--utxo-refill-lag-secs` (`IGRA_STRESS_UTXO_REFILL_LAG_SECS`), default `20`.
- `--utxo-safety-factor` (`IGRA_STRESS_UTXO_SAFETY_FACTOR`), default `1.5`.
- `--preflight-balance-check` (`IGRA_STRESS_PREFLIGHT_BALANCE_CHECK`): `0|1`, default `1`.

### 8.5 RPC inputs and selection

- `--rpc-endpoints-json` (`IGRA_STRESS_RPC_ENDPOINTS_JSON`), path to DevOps endpoint-list JSON.
- `--el-rpc-urls` (`IGRA_STRESS_EL_RPC_URLS`), comma-separated list.
- `--kaspa-rpc-urls` (`IGRA_STRESS_KASPA_RPC_URLS`), comma-separated list.
- `--rpc-selection-mode` (`IGRA_STRESS_RPC_SELECTION_MODE`): `random-per-step|sticky-per-instance`.
- `--rpc-random-seed` (`IGRA_STRESS_RPC_RANDOM_SEED`), optional.

### 8.6 Per-tx execution knobs

- `--tx-id-prefix` (`IGRA_TX_ID_PREFIX`), network-specific default (derived from
  `IGRA_STRESS_NETWORK` unless explicitly overridden).
- `--recipient-mode` (`IGRA_STRESS_RECIPIENT_MODE`): `ring|random-seeded`, default `ring`.
- `--recipient-random-seed` (`IGRA_STRESS_RECIPIENT_RANDOM_SEED`), required when
  `recipient-mode=random-seeded`.
- `--warmup-txs-per-worker` (`IGRA_STRESS_WARMUP_TXS_PER_WORKER`), default `1`.
- `--warmup-barrier-timeout-secs` (`IGRA_STRESS_WARMUP_BARRIER_TIMEOUT_SECS`), default `60`.
- `--mining-timeout-secs` (`IGRA_MINING_TIMEOUT_SECS`).
- `--gas-limit` (default `80000` for transfer workload).
- `--max-fee-per-gas`, `--max-priority-fee-per-gas`.
- `--pending-timeout-secs` (`IGRA_STRESS_PENDING_TIMEOUT_SECS`), default `10`.
- `--max-replacements-per-nonce` (`IGRA_STRESS_MAX_REPLACEMENTS_PER_NONCE`), default `5`.
- `--replacement-fee-bump-pct` (`IGRA_STRESS_REPLACEMENT_FEE_BUMP_PCT`), default `10`.
- `--replacement-timeout-cap-secs` (`IGRA_STRESS_REPLACEMENT_TIMEOUT_CAP_SECS`), default `60`.
- `--kaspa-fee-mode` (`IGRA_STRESS_KASPA_FEE_MODE`): `estimate|fixed`, default `estimate`.
- `--kaspa-fee-bucket` (`IGRA_STRESS_KASPA_FEE_BUCKET`): `priority|normal|low`, default `normal`.
- `--shutdown-grace-secs` (`IGRA_STRESS_SHUTDOWN_GRACE_SECS`), default `120`.

### 8.7 Current defaults in `scripts/igra/testnet-stress.sh` (fees)

Quick summary:

| Layer | Default Mode | Who sets effective value |
|---|---|---|
| EVM | EIP-1559 (unless `CAST_LEGACY=1`) | `cast send` + EL RPC responses |
| Kaspa | Dynamic estimate (`normal` bucket) | Kaspa `get_fee_estimate` in IGRA transport |

Current script behavior does **not** set explicit numeric fee flags in `cast send`:

- `send` call uses:
  - `send ... --rpc-url "${IGRA_EL_RPC_URL}" --private-key "${evm_key}" --async`
- No `--gas-price` is passed.
- No `--priority-gas-price` is passed.
- No `--gas-limit` is passed.
- `--legacy` is passed only if `CAST_LEGACY=1` (default `CAST_LEGACY=0`).

Effective fee defaults from this script:

- EVM fee mode default is EIP-1559 transaction path (unless `CAST_LEGACY=1`).
- EVM fee values are auto-estimated by `cast send` via RPC when not explicitly provided.
- Kaspa fee is not set by script knobs and is determined by the IGRA/Kaspa send path defaults.

IGRA minimum fee floor is endpoint-config dependent (not hardcoded in this script):

- `rusty-kaspa-private/kaspad` IGRA adapter requires `--igra-min-fee-per-gas-gwei` when IGRA is enabled.
- `rusty-kaspa-private/igra/adapter` integration test configs commonly use `2000` gwei.
- `igra-rpc-provider` has default `min_protocol_fee_per_gas_gwei = 100` in `config.toml`.

Foundry-side compatibility behavior:

- In IGRA transport mode, `eth_maxPriorityFeePerGas` is clamped to `eth_gasPrice` so EIP-1559
  tip estimation tracks the endpoint's gas-price floor.

Kaspa fee behavior in Foundry IGRA transport:

- Default mode is dynamic estimation (`kaspa_fee_mode=estimate`, `kaspa_fee_bucket=normal`) via
  Kaspa `get_fee_estimate`.
- A protocol minimum of `1.0 sompi/gram` is enforced on the selected feerate.
- If fee-estimate RPC fails, transport falls back to a fixed heuristic fee formula.

Operational implication:

- For reproducible fee budgeting, production stress runs should set explicit fee parameters in the
  runner (or collect fresh fee samples right before the campaign).

### 8.8 Fee-floor pinning and verification

To avoid ambiguity across endpoint stacks, runner must support:

- `--igra-min-fee-floor-gwei-expected` (`IGRA_STRESS_IGRA_MIN_FEE_FLOOR_GWEI_EXPECTED`), optional.

Behavior:

- If this value is set, runner performs preflight checks against the active EL endpoint set and
  fails fast if observed floor behavior is below expected.
- Observed effective floor must be written to campaign manifest (section 12.1) even when expected
  value is not set.

Observed floor measurement method (required):

1. Query `eth_gasPrice` from active EL endpoint set and record value.
2. Optionally probe with low-fee signed test tx in calibration context.
3. If probe is rejected for low fee, raise candidate floor estimate.
4. Record `igra_min_fee_floor_gwei_observed` as the maximum enforced value seen across checks.

## 9. Kaspa Blockspace and Capacity Planning

Constants from local `../rusty-kaspa-private`:

- `max_block_mass = 500_000`
- `TRANSIENT_BYTE_TO_MASS_FACTOR = 4`
- Ten BPS block interval `= 100 ms`

### 9.1 Byte upper bound from mass

Upper bound per block:

- `max_block_bytes ~= 500_000 / 4 = 125_000 bytes`

At 10 BPS:

- `125_000 * 10 = 1_250_000 bytes/sec` (~1.25 MB/s upper bound)

### 9.2 Per-transaction byte estimate (IGRA path)

Typical ranges for simple transfer workload:

- EVM raw tx bytes `~200..260`
- IGRA payload overhead `+5`
- Kaspa envelope/signature/input/output `~150..280`

Planning estimate:

- `kaspa_tx_serialized_bytes ~= 380..545`
- Use `512 bytes/tx` conservative default

Note: do not double-count L2 raw tx bytes and Kaspa tx bytes. L2 raw tx is embedded in Kaspa payload tx.

### 9.3 Mandatory calibration loop before high TPS runs

Before `>=100 TPS` campaigns:

1. Run calibration sample (`>=2000` tx) with same mode and gas config.
2. Record measured `avg` and `p95` serialized Kaspa tx bytes.
3. Record observed accepted TPS, RPC p95 latency, and failure ratio.
4. Set production target to at most `80%` of measured stable ceiling.

Do not run 500 TPS long campaign without fresh calibration on current infra.

### 9.4 Calibration mode execution

Runner calibration mode (`--calibration-mode=1`) must:

1. Run fixed-volume calibration workload (default `2000` tx total).
2. Use same core config that will be used by planned campaign (mode/network/fee/rpc policy).
3. Output calibration report with:
   - `kaspa_tx_bytes_avg`, `kaspa_tx_bytes_p50`, `kaspa_tx_bytes_p95`
   - `worker_tps_avg`, `worker_tps_p50`, `worker_tps_p95`
   - `el_rpc_latency_p95_ms`, `kaspa_rpc_latency_p95_ms`
   - `failure_ratio`
4. Compute recommended ceiling for next campaign planning.
5. Fail calibration if `failure_ratio > 0.05`; do not proceed to full campaign until infra or
   config issues are resolved.

Calibration mode is a preflight job, not a full campaign.

## 10. Funds Budgeting Model (KAS + iKAS)

### 10.1 Core formulas

- `total_primary_txs = target_tps * duration_secs`
- `warmup_txs = worker_count * warmup_txs_per_worker` (default `warmup_txs_per_worker=1`)
- `retry_overhead_ratio` default `0.10` (derived from recent calibration if available)
- `total_paid_txs = ceil((total_primary_txs + warmup_txs) * (1 + retry_overhead_ratio))`
- `kas_fanout_reserve = (worker_count * kaspa_fanout_amount_sompi / 100_000_000) + fanout_fee_buffer_kas`
  when fan-out is enabled, otherwise `0`
- `required_kas = ((total_paid_txs * avg_kaspa_fee_sompi_per_tx) / 100_000_000) * kas_safety_factor + kas_fanout_reserve`
- `required_ikas = ((total_paid_txs * avg_evm_gas_used * avg_evm_gas_price_wei_per_gas) / 1e18) * ikas_safety_factor`

Recommended safety factors:

- `kas_safety_factor = 1.3`
- `ikas_safety_factor = 1.3`
- `fanout_fee_buffer_kas = 2` (default planning reserve when fan-out is enabled)

### 10.2 12h at 500 TPS workload size

- `duration_secs = 12 * 3600 = 43,200`
- `total_primary_txs = 500 * 43,200 = 21,600,000`

Budget must be computed from measured fee samples on target infra (do not assume static fees).

### 10.3 Required preflight fee sampling

Before final funding decision:

1. Send sample run (`>=1000` tx) with final gas config.
2. Measure:
   - average Kaspa fee in sompi per accepted tx,
   - average EVM gas used and effective gas price in wei/gas,
   - retry/replace overhead ratio.
3. Apply formulas above with `1.3x` safety factor.

## 11. Suggested Hardware by Target TPS

Using baseline `5 TPS/worker` safe planning (devnet-first, before campaign-specific calibration override).

| Target TPS | Workers | CPU (min) | RAM (min) | Notes |
|---|---:|---:|---:|---|
| 50 | 10 | 12 vCPU | 16 GB | single host usually enough |
| 100 | 20 | 24 vCPU | 32 GB | consider separating RPC from workers |
| 250 | 50 | 56 vCPU | 64 GB | multi-host recommended |
| 500 | 100 | 112 vCPU | 128 GB | multi-host required, RPC scaling likely needed |

Assumptions:

- ~1 vCPU per worker plus 10-15% control-plane overhead.
- ~0.8-1.2 GB RAM per worker depending queue depth/mode.
- `prebuild-send` needs more RAM due to prebuilt queues.

## 12. Version Pinning and Reproducibility

Every stress campaign report must include exact versions:

- Foundry fork commit hash.
- IGRA components commit hash.
- Reth commit/tag.
- `rusty-kaspa-private` commit/tag.
- Kaspaminer version/commit.

If any component version changes, previous capacity results are not comparable without rerun.

### 12.1 Mandatory campaign manifest JSON

Each campaign must emit a machine-readable manifest file:

- Path: `<campaign_output_dir>/campaign-manifest.json`
- Emitted twice:
  - pre-run snapshot (resolved configuration),
  - post-run finalized snapshot (resolved configuration + observed results).

Required fields:

- `schema_version` (for example `1.0.0`)
- `campaign_id`
- `started_at_utc`, `ended_at_utc`
- `mode`, `network`
- `target_tps`, `worker_count`, `wallet_start_index`
- `resolved_parameters` (full flattened key/value set used by runner after CLI+ENV+defaults resolution)
- `parameter_sources` (for each key: `cli|env|default|file`)
- `resolved_wallet_index_range`
- `resolved_contract_index_range`
- `resolved_endpoints`:
  - `igra_rpc_urls`
  - `kaspa_rpc_urls`
  - `endpoint_set_sha256` (stable hash fingerprint of endpoint set)
- `fee_policy`:
  - `igra_min_fee_floor_gwei_expected` (nullable)
  - `igra_min_fee_floor_gwei_observed`
  - `kaspa_fee_mode`
  - `kaspa_fee_bucket`
- `nonce_policy`:
  - `pending_timeout_secs`
  - `max_replacements_per_nonce`
  - `replacement_fee_bump_pct`
- `utxo_policy`:
  - `prebuild_horizon_secs`
  - `utxo_refill_lag_secs`
  - `utxo_safety_factor`
  - `utxos_per_wallet_min`
- `build_versions`:
  - foundry/IGRA/reth/rusty-kaspa-private/miner commit or tag
- `runtime`:
  - `host_count`
  - `runner_version`
  - `warmup_completed_at_utc`
- `results` (post-run required):
  - `accepted`
  - `rejected`
  - `timeout`
  - `dropped`
  - `terminal_error`
  - `failure_ratio`
  - `achieved_tps_avg`
  - `achieved_tps_p95_1m` (p95 of per-minute TPS window values)

`endpoint_set_sha256` computation (required):

1. Collect `igra_rpc_urls` and `kaspa_rpc_urls` from resolved configuration.
2. Sort all URLs lexicographically.
3. Concatenate sorted URLs using newline (`\n`) separators.
4. Compute SHA256 over the concatenated byte string.
5. Encode hash as lowercase hexadecimal.

Campaigns without this manifest are non-reproducible and must be considered invalid for capacity
comparison.

### 12.2 Example `campaign-manifest.json`

```json
{
  "schema_version": "1.0.0",
  "campaign_id": "devnet-2026-02-17T12-00-00Z-500tps",
  "started_at_utc": "2026-02-17T12:00:00Z",
  "ended_at_utc": "2026-02-17T13:00:00Z",
  "mode": "prebuild-send",
  "network": "devnet",
  "target_tps": 500,
  "worker_count": 100,
  "wallet_start_index": 100,
  "resolved_parameters": {
    "IGRA_STRESS_TARGET_TPS": "500",
    "IGRA_STRESS_MODE": "prebuild-send",
    "IGRA_STRESS_NETWORK": "devnet"
  },
  "parameter_sources": {
    "IGRA_STRESS_TARGET_TPS": "cli",
    "IGRA_STRESS_MODE": "cli",
    "IGRA_STRESS_NETWORK": "default"
  },
  "resolved_wallet_index_range": "100..199",
  "resolved_contract_index_range": "100..199",
  "resolved_endpoints": {
    "igra_rpc_urls": ["http://127.0.0.1:8545"],
    "kaspa_rpc_urls": ["grpc://127.0.0.1:16210"],
    "endpoint_set_sha256": "f6f6d75fdb0dbf8f11bb44735c3f3f0f6f7cc66dd0ca8f1d4a1fb77a39db0c67"
  },
  "fee_policy": {
    "igra_min_fee_floor_gwei_expected": 2000,
    "igra_min_fee_floor_gwei_observed": 2000,
    "kaspa_fee_mode": "estimate",
    "kaspa_fee_bucket": "normal"
  },
  "nonce_policy": {
    "pending_timeout_secs": 10,
    "max_replacements_per_nonce": 5,
    "replacement_fee_bump_pct": 10
  },
  "utxo_policy": {
    "prebuild_horizon_secs": 120,
    "utxo_refill_lag_secs": 20,
    "utxo_safety_factor": 1.5,
    "utxos_per_wallet_min": 1050
  },
  "build_versions": {
    "foundry": "abc1234",
    "igra": "def5678",
    "reth": "v1.0.0",
    "rusty_kaspa_private": "9876fed",
    "kaspaminer": "mnr-0.3.1"
  },
  "runtime": {
    "host_count": 2,
    "runner_version": "0.1.0",
    "warmup_completed_at_utc": "2026-02-17T12:00:25Z"
  },
  "results": {
    "accepted": 1730000,
    "rejected": 2100,
    "timeout": 1300,
    "dropped": 400,
    "terminal_error": 15,
    "failure_ratio": 0.0022,
    "achieved_tps_avg": 481.4,
    "achieved_tps_p95_1m": 497.1
  }
}
```

## 13. Execution Sequence

### 13.1 One-time setup

1. Prepare `wallets_1000.json`.
2. Receive DevOps endpoint-list JSON for EL and Kaspa RPC.
3. Validate endpoint JSON schema and non-empty lists.
4. Validate runner can query sample EL and Kaspa endpoints before campaign day.

### 13.2 Per campaign

1. Choose `target_tps`, `wallet_start_index`, `mode`, stop condition.
2. Compute required workers and validate index bounds.
3. If `mode=prebuild-send`, validate UTXO depth formula.
4. Optionally run Kaspa fan-out (`kaspa-fanout=1`).
5. Run preflight balance checks for selected wallet index range when `preflight-balance-check=1`.
6. Run preflight calibration and fee sampling.
7. Resolve and record observed IGRA fee floor behavior for active endpoints.
8. Emit pre-run `campaign-manifest.json`.
9. Launch workers.
10. Monitor:
   - achieved TPS,
   - accepted/rejected/timeout/dropped/terminal_error counts,
   - EL pending depth,
   - Kaspa mempool/block fullness,
   - RPC latency/error rates,
   - worker stalls and nonce recovery events.
11. Evaluate pass/fail criteria from section 1.1.
12. Emit post-run finalized `campaign-manifest.json`.

Preflight balance check requirements:

- Check all selected EVM wallets for minimum native balance sufficient for:
  warm-up + target run + retry safety margin.
- Check all selected Kaspa wallets for:
  - minimum spendable UTXO count per section 5.2,
  - minimum total sompi for run budget share.
- If any wallet is below threshold, fail fast with wallet index list and deficit summary.

### 13.3 Metrics collection specification (required)

Collection format:

- Primary stream: JSON Lines (`metrics.ndjson`), one object per sample interval.
- Optional export: Prometheus endpoint for real-time dashboards.
- Write mode: buffered append with periodic flush every `10s` to reduce I/O overhead.
- Buffer size: up to `100` samples per flush.

Sampling frequency:

- Aggregate sampling interval: `1s`.
- Pass/fail windowing: roll-up into fixed `60s` windows from aggregate samples.

Required aggregate fields per sample:

- `ts_utc`
- `tps_1s`
- `accepted_1s`, `rejected_1s`, `timeout_1s`, `dropped_1s`, `terminal_error_1s`
- `el_rpc_p95_ms`, `kaspa_rpc_p95_ms`
- `el_pending_count`
- `kaspa_mempool_mass_estimate` (nullable; set `null` when source RPC/method is unavailable)
- `active_workers`, `stalled_workers`

Required per-worker fields per sample:

- `worker_id`
- `wallet_index`
- `tps_1s`
- `accepted_total`, `failed_total`
- `local_next_nonce`, `rpc_pending_nonce`
- `replacement_count_total`
- `last_error_code` (nullable)

Expected-value guidance (initial devnet baselines):

- `stalled_workers / active_workers` should stay below `0.02`.
- `el_rpc_p95_ms` should stay below `2000ms`; above this suggests RPC bottleneck.
- `kaspa_rpc_p95_ms` should stay below `2000ms`; above this suggests RPC bottleneck.
- `timeout_1s` sustained above `0.5%` of attempted tx/s is anomalous and should trigger alert.

Example `metrics.ndjson` line:

```json
{"ts_utc":"2026-02-17T12:10:01Z","tps_1s":492.3,"accepted_1s":492,"rejected_1s":1,"timeout_1s":0,"dropped_1s":0,"terminal_error_1s":0,"el_rpc_p95_ms":148,"kaspa_rpc_p95_ms":212,"el_pending_count":1830,"kaspa_mempool_mass_estimate":74211,"active_workers":100,"stalled_workers":1}
```

If mempool mass is unavailable on the active Kaspa RPC stack, emit the same field as `null`:

```json
{"ts_utc":"2026-02-17T12:10:02Z","tps_1s":489.0,"accepted_1s":489,"rejected_1s":0,"timeout_1s":0,"dropped_1s":1,"terminal_error_1s":0,"el_rpc_p95_ms":162,"kaspa_rpc_p95_ms":251,"el_pending_count":1792,"kaspa_mempool_mass_estimate":null,"active_workers":100,"stalled_workers":1}
```

Example per-worker `metrics.ndjson` line:

```json
{"ts_utc":"2026-02-17T12:10:01Z","worker_id":"worker-042","wallet_index":142,"tps_1s":4.9,"accepted_total":2941,"failed_total":7,"local_next_nonce":8083,"rpc_pending_nonce":8081,"replacement_count_total":3,"last_error_code":null}
```

### 13.4 Graceful shutdown policy

- On manual stop or stop-condition hit, runner enters `drain` mode.
- No new txs are enqueued after drain starts.
- In-flight txs are awaited up to `shutdown_grace_secs`.
- After grace timeout, unresolved txs are marked `dropped` and included in failure ratio.
- Partial campaigns are valid if manifest and metrics are complete and pass/fail is still computed.
- For signal termination, runner should use conventional exit codes (`130` for SIGINT, `143` for
  SIGTERM) after writing finalized artifacts.

### 13.5 Minimal command examples

Standard timed run:

```bash
./stress-runner \
  --network devnet \
  --target-tps 50 \
  --duration-secs 3600 \
  --mode prebuild-send \
  --wallet-start-index 0 \
  --rpc-endpoints-json endpoints.json
```

Calibration-only run:

```bash
./stress-runner \
  --network devnet \
  --mode prebuild-send \
  --calibration-mode 1 \
  --rpc-endpoints-json endpoints.json
```

Preflight-sample budget run:

```bash
./stress-runner \
  --network devnet \
  --mode prebuild-send \
  --preflight-sample-mode 1 \
  --rpc-endpoints-json endpoints.json
```

## 14. Validation Checklist

- One-to-one mapping for wallet and contract index.
- No overflow (`wallet_start_index + worker_count <= 1000`).
- Mode prerequisites met (`prebuild-send` UTXO depth, nonce manager, resend policy).
- RPC parsing works for one/many endpoints.
- Endpoint selection uses persistent pools and health/backoff.
- Stop-condition conflict handling is enforced.
- Failure taxonomy and denominator use section 1.2 definitions.
- IGRA fee-floor observation is captured and matches expected value when configured.
- Kaspa fee mode/bucket settings are captured.
- Warm-up barrier completed before timed phase.
- Steady-state gas measurement is recorded from calibration.
- No worker crash policy violations occurred during timed phase.
- Campaign manifest is emitted (pre-run and post-run) with required fields.
- Aggregate and per-worker metrics are emitted and persisted.

## 15. Reference Implementation Checklist

1. Parse and validate all CLI/ENV parameters.
2. Validate wallet index bounds and worker-count constraints.
3. Load wallet JSON and validate schema.
4. Validate RPC endpoint list and connectivity.
5. Resolve effective configuration and emit pre-run manifest skeleton.
6. Run preflight balance checks for selected wallet range.
7. If requested, execute fan-out and verify UTXO distribution.
8. Initialize persistent RPC client pools.
9. Initialize per-worker nonce managers.
10. Execute warm-up phase (`warmup_txs_per_worker`) and wait on warm-up barrier.
11. Start metrics collection stream (`metrics.ndjson`).
12. Launch timed stress phase.
13. Apply resend/replacement and recovery policies during run.
14. Enforce stop condition and enter drain mode.
15. Await in-flight tx completion up to `shutdown_grace_secs`.
16. Compute pass/fail using section 1.1 and section 1.2.
17. Emit finalized campaign manifest and summary report.
