# IGRA-Kaspa Stress Runner Implementation Plan (Revised for Team Review)

Status: draft for review before coding  
Primary spec: `docs/stress-test-prep/igra-kaspa-stress-test-spec.md`

## 1. Scope

This plan defines exact implementation steps for the stress runner.

Goals:

- Deliver a production-grade stress runner for deterministic IGRA write-path campaigns
  (devnet-first).
- Implement all required spec behaviors: manifest, metrics, pass/fail logic, warm-up barrier,
  preflight and calibration flows, endpoint failover, and graceful shutdown semantics.

Non-goals for this wave:

- Multi-coordinator distributed orchestration.
- Dynamic worker autoscaling mid-campaign.
- Continue-with-N-1 policy after worker loss (initial policy remains fail-fast).

## 2. Baseline, Gaps, and Ownership Boundaries

Current baseline:

- `crates/igra-loadgen/src/main.rs` sends raw EIP-1559 txs via IGRA transport.
- `scripts/igra/testnet-stress.sh` provides shell-driven stress flow.

Confirmed gaps vs spec:

- Missing campaign manifest and metrics streams.
- Missing warm-up barrier and start synchronization.
- Missing pass/fail 95%/95% window evaluation logic.
- Missing calibration and preflight first-class modes.
- Missing deterministic wallet/contract mapping with range validation.
- Missing endpoint-list + retry/failover/degraded-pause policy.
- Missing explicit recipient mode support (`ring|random-seeded`).
- Missing network-derived defaults and Kaspa address-prefix validation.
- Missing preflight checks (UTXO depth, fee floor, contract code existence).

Kaspa operation ownership (explicit):

- Kaspa tx construction, txid-prefix mining, signing, and broadcast are already implemented in
  `crates/common/src/provider/igra_transport.rs` (`InProcessKaspaPayloadSubmitter`).
- `igra-loadgen` will not duplicate cryptographic Kaspa tx construction logic in this wave.
- `igra-loadgen` will add a dedicated `src/kaspa.rs` integration layer for:
  - Kaspa config/profile resolution,
  - UTXO depth preflight checks,
  - refill-lag measurements and metrics hooks,
  - RPC-side validation and campaign-time Kaspa observability.

## 3. Implementation Targets and Files

Primary target:

- `crates/igra-loadgen`

Secondary touchpoints:

- `scripts/igra/testnet-stress.sh` (compatibility adapter).
- `scripts/igra/deterministic-harness.sh` (optional inclusion of loadgen tests).
- `docs/stress-test-prep/igra-kaspa-stress-test-spec.md` (only if schema naming changes).

## 4. Module Layout (Revised)

Refactor `crates/igra-loadgen/src/main.rs` into:

- `crates/igra-loadgen/src/main.rs`
  - thin entrypoint; exit-code mapping and top-level error handling.
- `crates/igra-loadgen/src/cli.rs`
  - clap CLI/env definition, validation, help text.
- `crates/igra-loadgen/src/config.rs`
  - resolved config, source attribution, network profile defaults.
- `crates/igra-loadgen/src/wallets.rs`
  - wallet JSON loading and index/range validation.
- `crates/igra-loadgen/src/contracts.rs`
  - contract index/address mapping and recipient-selection policies.
- `crates/igra-loadgen/src/endpoints.rs`
  - endpoint parsing, health, retry/failover, endpoint fingerprint hash.
- `crates/igra-loadgen/src/kaspa.rs`
  - Kaspa integration layer:
    - profile wiring into IGRA transport config,
    - UTXO depth checks and refill-lag sampling,
    - Kaspa-side telemetry hooks.
- `crates/igra-loadgen/src/worker.rs`
  - worker lifecycle, pacing, stop conditions, report plumbing.
- `crates/igra-loadgen/src/nonce.rs`
  - nonce stream ownership, reconciliation, gap handling.
- `crates/igra-loadgen/src/replacement.rs`
  - replacement policy/backoff/fee-bump rules and invariants.
- `crates/igra-loadgen/src/modes.rs`
  - `full-cycle`, `prebuild-send`, `calibration`, `preflight-sample`.
- `crates/igra-loadgen/src/preflight.rs`
  - balance checks, contract-code checks, fee-floor observation, UTXO checks.
- `crates/igra-loadgen/src/metrics.rs`
  - 1s sampling, rolling 60s windows, JSONL buffering and flush.
- `crates/igra-loadgen/src/evaluator.rs`
  - pass/fail rules (95%/95%, failure ratio, stall limits).
- `crates/igra-loadgen/src/manifest.rs`
  - pre-run and post-run `campaign-manifest.json`.
- `crates/igra-loadgen/src/coordinator.rs`
  - lifecycle state machine, warm-up barrier, drain, signal orchestration.
- `crates/igra-loadgen/src/errors.rs`
  - typed error codes mapped to failure taxonomy classes.

## 5. Precise Phase Plan

## Phase 0: Skeleton and Responsibility Contract (1.0 day)

Files:

- all new module stubs from section 4.

Implement:

- move current logic behind module interfaces without behavior changes.
- add explicit internal boundary docs:
  - `igra-loadgen` integration vs `foundry-common` Kaspa submission ownership.

Acceptance:

- `cargo check -p igra-loadgen` passes.
- CLI behavior unchanged from baseline.

## Phase 1: Config, Mapping, and Network Defaults (1.8 days)

Files:

- `src/cli.rs`, `src/config.rs`, `src/wallets.rs`, `src/contracts.rs`.

Implement:

- full CLI/ENV contract from spec.
- worker-count resolution and bounds validation.
- deterministic wallet/contract mapping.
- network profile enum with derived defaults:
  - `network -> tx_id_prefix`,
  - `network -> expected Kaspa address prefix`.
- recipient mode implementation:
  - `ring`,
  - `random-seeded` with deterministic seed behavior.

Acceptance:

- invalid ranges/underprovision fail fast with explicit diagnostics.
- same seed yields stable recipient sequence across runs.
- address prefix mismatches fail preflight.

## Phase 2: Coordinator, Metrics, Manifest, and Pass/Fail Engine (4.0 days)

Files:

- `src/coordinator.rs`, `src/metrics.rs`, `src/evaluator.rs`, `src/manifest.rs`, `src/errors.rs`.

Implement:

- lifecycle state machine:
  - `init -> warmup -> running -> degraded_pause -> drain -> finalized`.
- warm-up barrier with timeout and synchronized timed start.
- pre-run + post-run manifest emission.
- metrics stream:
  - 1s sampling,
  - JSONL (`metrics.ndjson`),
  - buffered flush every 10s, up to 100 samples per flush.
- pass/fail evaluator:
  - 60s windows from 1s samples,
  - 95%/95% throughput rule,
  - failure-ratio threshold,
  - worker-stall threshold.

Acceptance:

- manifest schema fields match spec including `schema_version` and `endpoint_set_sha256`.
- windowing logic passes deterministic tests.
- warm-up timeout fails before timed phase.

## Phase 3a: Kaspa Integration Layer (2.5 days)

Files:

- `src/kaspa.rs`, `src/modes.rs`, `src/preflight.rs`.

Implement:

- integrate network/Kaspa settings into `IgraTransportConfig`.
- explicit configuration controls for:
  - `kaspa_utxo_mode`, `kaspa_fee_mode`, `kaspa_fee_bucket`,
  - mining timeout, tx-id prefix behavior.
- prebuild-oriented UTXO observability:
  - spendable UTXO counting,
  - per-wallet depth checks against formula,
  - refill-lag measurement helpers (`confirm + query + retry` latency budget).
- Kaspa-side campaign telemetry fields for manifest/metrics.

Acceptance:

- UTXO depth check is deterministic and produces per-wallet deficits.
- Kaspa integration tests pass with IGRA transport submitter in place.
- no duplicated Kaspa cryptographic tx build logic added to loadgen.

## Phase 3b: Worker, Nonce, Replacement, and Shutdown (4.0 days)

Files:

- `src/worker.rs`, `src/nonce.rs`, `src/replacement.rs`, `src/coordinator.rs`.

Implement:

- worker execution paths for `full-cycle` and `prebuild-send`.
- nonce reconciliation algorithm per spec.
- replacement behavior:
  - timeout/backoff sequence,
  - fee bump percentage,
  - EIP-1559 invariant enforcement.
- worker fail-fast policy.
- graceful shutdown:
  - SIGINT -> exit 130 after artifact finalization,
  - SIGTERM -> exit 143 after artifact finalization,
  - `shutdown_grace_secs` drain behavior.

Acceptance:

- nonce gap recovery tests pass.
- replacement invariants are enforced.
- signal handling finalizes manifest/metrics then exits with correct code.

## Phase 4: Endpoint Pooling and Outage Handling (2.0 days)

Files:

- `src/endpoints.rs`, `src/coordinator.rs`, `src/worker.rs`.

Implement:

- endpoint-list JSON loading and precedence over direct URLs.
- persistent clients per endpoint.
- selection modes: `random-per-step`, `sticky-per-instance`.
- retries, rotation, and degraded-pause policy.
- endpoint fingerprint algorithm:
  - sort all URLs lexicographically,
  - join with `\n`,
  - SHA256 lowercase hex.

Acceptance:

- deterministic `endpoint_set_sha256`.
- outage durations are captured in manifest post-run results.

## Phase 5: Preflight and Calibration Expansion (3.5 days)

Files:

- `src/preflight.rs`, `src/modes.rs`, `src/manifest.rs`, `src/kaspa.rs`.

Implement:

- calibration mode:
  - fixed-volume run,
  - required p50/p95 outputs,
  - fail calibration if failure ratio > 5%.
- preflight-sample mode:
  - fee and retry overhead sampling for budgeting.
- UTXO depth validation:
  - `utxos_per_wallet_min = ceil(worker_tps * (prebuild_horizon_secs + utxo_refill_lag_secs) * utxo_safety_factor)`.
- fee-floor observation method:
  - `eth_gasPrice`,
  - optional low-fee probe,
  - `igra_min_fee_floor_gwei_observed` persisted in manifest.
- contract-code verification:
  - sample 10 deterministic addresses in selected range,
  - `eth_getCode` check,
  - fail-fast if empty code on any sample.

Acceptance:

- preflight failure reasons are explicit and actionable.
- calibration/preflight modes exit without starting timed campaign.
- observed fee floor is present in manifest even when expected floor is unset.

## Phase 6: Script Compatibility and Docs (1.5 days)

Files:

- `scripts/igra/testnet-stress.sh`
- `docs/stress-test-prep/igra-kaspa-stress-test-spec.md` (if key names changed)
- this implementation plan doc

Implement:

- make script call `igra-loadgen` as canonical engine.
- preserve key legacy env aliases for team continuity.
- document artifact paths and minimal commands for:
  - 10-account runs,
  - 500 TPS planning runs.

Acceptance:

- existing script entrypoint remains usable.
- generated artifacts are stable and discoverable.

## 6. Revised Effort Estimate

Single engineer:

- Phase 0: 1.0
- Phase 1: 1.8
- Phase 2: 4.0
- Phase 3a: 2.5
- Phase 3b: 4.0
- Phase 4: 2.0
- Phase 5: 3.5
- Phase 6: 1.5
- Total: 20.3 engineer-days (about 4 calendar weeks with review overhead)

Two engineers (parallel where possible):

- about 12-14 calendar days.

Suggested split:

- Engineer A: phases 3a, 3b, 4.
- Engineer B: phases 1, 2, 5, 6.

## 7. Test Strategy (Expanded)

## 7.1 Unit Tests

Add tests in module files and/or `crates/igra-loadgen/tests/`:

- network-derived defaults mapping and prefix validation.
- recipient determinism (`ring`, `random-seeded`).
- worker-count/bounds and mapping correctness.
- endpoint hash determinism.
- 60s windowing + 95%/95% evaluator correctness.
- failure taxonomy denominator correctness.
- fee-floor observation decision logic.
- UTXO-depth formula and deficit reporting.
- manifest and metrics serialization contracts.

Existing Kaspa correctness tests to keep green:

```bash
cargo test -p foundry-common igra_transport -- --nocapture
```

## 7.2 Integration Tests

- warm-up barrier success + timeout.
- worker crash -> fail-fast campaign termination.
- nonce divergence reconciliation.
- replacement backoff + cap behavior.
- UTXO depletion/insufficient depth detection.
- fee-floor mismatch detection.
- contract-code missing preflight failure.
- random-seeded reproducibility across repeated runs.
- signal handling exit code checks (130/143).

## 7.3 Regression Coverage

```bash
./scripts/igra/deterministic-harness.sh
KEEP_TMP=1 ./scripts/igra/testnet-smoke.sh
cargo test -p igra-loadgen -- --nocapture
```

## 7.4 Devnet E2E Campaign Gates

Required before merge:

1. 10-worker `full-cycle` timed run.
2. 10-worker `prebuild-send` timed run.
3. calibration-only run.
4. preflight-sample run.

Required artifacts:

- `campaign-manifest.json` (pre + post for timed campaigns).
- `metrics.ndjson`.
- calibration report JSON.
- preflight sample report JSON.

## 8. Review Gates

Mandatory gates:

1. After Phase 1: config/mapping/network defaults review.
2. After Phase 2: coordinator + manifest + metrics + evaluator review.
3. After Phase 3a: Kaspa integration review (UTXO/refill/telemetry).
4. After Phase 3b: worker + nonce + replacement + shutdown review.
5. After Phase 5: preflight/calibration/funding review.
6. After Phase 6: script/docs/operability review.

No advancement to next gate unless previous gate acceptance criteria pass.

## 9. Risk Register and Mitigations

Risk: throughput collapse from synchronous send behavior.  
Mitigation: prioritize prebuild-send queueing and async dispatch in Phase 3b.

Risk: RPC bottleneck misread as protocol bottleneck.  
Mitigation: endpoint pool, per-layer latency metrics, degraded-pause accounting.

Risk: non-deterministic inputs reduce comparability.  
Mitigation: strict manifest schema, deterministic endpoint hash, version pinning.

Risk: fee drift invalidates budget assumptions.  
Mitigation: required preflight sampling and fee-floor observation.

Risk: txid-prefix mining CPU bottleneck at high TPS.  
Mitigation: benchmark mining performance on target hardware and adjust safe TPS via calibration.

Risk: UTXO refill lag exceeds prebuild horizon and stalls workers.  
Mitigation: conservative depth formula with safety factor and refill-lag monitoring.

Risk: contract-code preflight adds startup overhead.  
Mitigation: sample fixed-size deterministic subset (10 addresses), run checks in parallel.

Risk: metrics buffer flush lag at high load.  
Mitigation: capped buffer size, periodic forced flush, and flush-lag metric with alert threshold.

## 10. Definition of Done

Implementation is complete when:

- all Phase 0-6 acceptance checks pass.
- all pre-implementation review gaps are explicitly closed.
- pass/fail evaluator correctly enforces 95%/95% window rule.
- UTXO depth validation fails fast on insufficient wallets.
- fee-floor observation is computed and persisted.
- contract-code preflight detects missing deployments.
- network-derived defaults and address-prefix validation are verified.
- signal handling finalizes artifacts and exits with 130/143 semantics.
- 10-worker campaigns and smoke/regression suites pass without IGRA regressions.

