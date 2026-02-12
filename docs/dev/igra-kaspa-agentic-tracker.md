# IGRA-Kaspa Agentic Implementation Tracker

Source spec: `docs/dev/igra-kaspa-integration-plan.md`

## Execution Rules

1. Each item must run at least 2 implementation/review ping-pong cycles.
2. Roles:
   - Implementer: Codex CLI (`codex exec`)
   - Reviewer: Claude CLI (`claude -p`)
   - Orchestrator: this session
3. Each cycle records:
   - implementation prompt + result
   - review findings
   - fixes applied
   - tests run
   - remaining risks

## Item Map

1. Config contract, mode activation, precedence, validation, network safety, signer guardrails.
2. Provider/transport interception (`eth_sendRawTransaction`) + method rejection matrix.
3. In-process submit pipeline skeleton (payload encode path + tx-type gates).
4. Lifecycle persistence store (`tx_map`, `sender_nonce_state`, `sender_locks`) + migration metadata.
5. Sender lock lease algorithm + nonce ordering/gap handling.
6. Recovery and polling/backoff from `KASPA_BROADCASTED`.
7. Cache behavior, invalidation, retention, GC, DB profile isolation.
8. CLI UX: `cast igra-status`, `cast receipt` metadata bridge, dry-run flags.
9. Observability and error catalog wiring.
10. Deterministic test harness + integration scenarios + CI gates.

## Status Board

- [x] Item 1
- [x] Item 2
- [x] Item 3
- [x] Item 4
- [x] Item 5
- [x] Item 6
- [x] Item 7
- [x] Item 8
- [x] Item 9
- [x] Item 10

## Detailed Log

### Item 1: Config + Guardrails

#### Cycle 1
- Implementer run: `codex exec` added `crates/config/src/igra.rs`, `crates/common/src/igra.rs`, config wiring (`Config.igra`), standalone section registration, and guardrail calls in `cast send` + `forge create`.
- Reviewer run: `claude -p` review found wrapper/helper cleanup and missing coverage gaps.
- Fix run: removed per-command wrapper helpers, updated guardrail calls to shared helper directly, added config/guardrail tests, fixed forge config test struct/JSON expectations.
- Tests:
  - `cargo test -p foundry-config test_igra -- --nocapture` passed
  - `cargo test -p cast guardrails_ -- --nocapture` passed
  - `cargo test -p forge guardrails_ -- --nocapture` passed
- Result: baseline Item 1 functionality landed; moved to hardening cycle.

#### Cycle 2
- Implementer run: orchestration fix pass tightened config quality in `crates/config/src/igra.rs` and `crates/common/src/igra.rs` (removed redundant startup wrapper, added scheme validation, added missing validation tests).
- Reviewer run: `claude -p` pass; some findings were false positives, one valid gap (extra validation coverage) addressed.
- Fix run: added tests for zero chain id, zero timeout, whitespace-as-missing, supported network matrix, odd-length prefix case.
- Tests:
  - `cargo test -p foundry-config igra -- --nocapture` passed
  - `cargo test -p cast guardrails_ -- --nocapture` passed
  - `cargo test -p forge guardrails_ -- --nocapture` passed
- Result: `NO_BLOCKING_FINDINGS` on final Claude pass; Item 1 complete.

#### Cycle 3 (optional hardening)
- Implementer run: not required.
- Reviewer run: not required.
- Fix run: not required.
- Tests: not required.
- Result: skipped by design (two completed cycles satisfied requirement).

### Item 2: Transport Interception

#### Cycle 1
- Implementer run: `codex exec` introduced `crates/common/src/provider/igra_transport.rs` and provider wiring in `crates/common/src/provider/mod.rs`.
- Reviewer run: `claude -p` reviewed transport behavior and suggested optimization items.
- Fix run: reduced per-request overhead in `IgraTransport::request` by cloning only inner transport and loosening generic bounds to `T::Future: Send + 'static`.
- Tests:
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed (3 tests).
- Result: cycle closed with `NO_BLOCKING_FINDINGS`.

#### Cycle 2
- Implementer run: `codex exec` hardening pass added edge-case batch/non-send tests in `crates/common/src/provider/igra_transport.rs`.
- Reviewer run: `claude -p` on updated diff reported `NO_BLOCKING_FINDINGS`.
- Fix run: no code fix required after final review.
- Tests:
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed (6 tests).
- Result: Item 2 complete with two full ping-pong cycles.

### Item 3: Submit Pipeline Skeleton

#### Cycle 1
- Implementer run: orchestration pass extended `crates/common/src/provider/igra_transport.rs` to gate `eth_sendRawTransaction` envelope types in IGRA mode:
  - allow: legacy (0), EIP-2930 (1), EIP-1559 (2)
  - reject: EIP-4844 (3), EIP-7702 (4), unknown typed envelopes, malformed raw params
- Reviewer run: `claude -p` produced one major false positive (2930 rejection suggestion conflicting with locked v1 spec) and no accepted blocking defects.
- Fix run: adjusted failing batch helper to provide valid raw payload when batch includes `eth_sendRawTransaction`.
- Tests:
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed (9 tests).
- Result: tx-type gate path implemented and verified.

#### Cycle 2
- Implementer run: `codex exec` hardening added spec-locking tests for:
  - EIP-2930 allowed
  - EIP-1559 allowed
  - malformed raw params rejected with IGRA decode error
- Reviewer run: `claude -p` final pass reported `NO_BLOCKING_FINDINGS` with explicit confirmation of supported/rejected tx-type matrix.
- Fix run: no additional code changes required post-review.
- Tests:
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed (12 tests).
- Result: Item 3 tx-type-gate scope complete; payload encode + Kaspa submit skeleton still pending.

#### Cycle 3
- Implementer run: payload encode/mining/submit skeleton completed in `crates/common/src/provider/igra_transport.rs`:
  - payload format finalized (`version|tx_type` header + raw tx + 4-byte payload nonce)
  - prefix mining with timeout classification (`IGRA_MINING_001`)
  - submit abstraction (`IgraPayloadSubmitter`) + default `KaswalletPayloadSubmitter`
  - lifecycle persistence for submit path (`RECEIVED_RAW_L2 -> KASPA_BROADCASTED` + failure persistence)
- Implementer run (wiring hardening): provider/config integration in `crates/common/src/provider/mod.rs` and `crates/config/src/igra.rs`:
  - fail-fast IGRA validation at provider construction
  - explicit `mining_timeout_secs` config support and precedence
  - fail-fast store initialization (`try_with_store_config`) instead of warn-and-continue.
- Reviewer run: Claude pass flagged timeout semantics, missing upfront validation invocation, and swallowed store-init failures.
- Fix run: addressed all accepted findings, reran focused suites, and re-reviewed.
- Tests:
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed (18 tests)
  - `cargo test -p foundry-common igra_store -- --nocapture` passed
  - `cargo test -p foundry-common provider::tests -- --nocapture` passed
  - `cargo test -p foundry-config igra -- --nocapture` passed
- Result: `NO_BLOCKING_FINDINGS` after fix/review loop; Item 3 complete for v1 submit-pipeline skeleton scope.

### Item 4: Persistence + Migration

#### Cycle 1
- Implementer run: `codex exec` introduced SQLite-backed IGRA persistence in `crates/common/src/igra_store.rs` with schema tables (`schema_meta`, `tx_map`, `sender_nonce_state`, `sender_locks`), WAL/busy-timeout setup, schema version metadata, and transport-side lifecycle persistence wiring in `crates/common/src/provider/igra_transport.rs`.
- Reviewer run: `claude -p` review flagged atomicity/race concerns, overflow handling gaps, and missing replacement observability in stale nonce paths.
- Fix run: prepared itemized remediation scope (atomic lock+classify + overflow + replacement observability + concurrency tests).
- Tests:
  - `cargo test -p foundry-common igra_store -- --nocapture` passed
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed (pre-hardening baseline)
- Result: cycle complete; moved to hardening/remediation.

#### Cycle 2
- Implementer run: fix pass merged lock+nonce classification into one store call (`acquire_sender_lock_and_classify_nonce`), added overflow error code/path (`IGRA_NONCE_003`), stale replacement observability (`IGRA_NONCE_004` + `STALE_REPLACEMENT_CANDIDATE`), and added tests for lock reacquire, overflow, replacement observability, and concurrent same-sender serialization.
- Reviewer run: `claude -p` review on narrowed patch returned final `NO_BLOCKING_FINDINGS` after deep pass.
- Fix run: corrected test-runtime starvation artifact in concurrent transport test (spawned separate tasks), and tightened owner-id uniqueness with correlation-based lock owner IDs.
- Tests:
  - `cargo test -p foundry-common igra_store -- --nocapture` passed (7 tests)
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed (15 tests)
- Result: Item 4 complete.

### Item 5: Sender Lock + Nonce Ordering

#### Cycle 1
- Implementer run: initial sender lock + nonce ordering shipped in store/transport with lock timeout behavior and nonce-gap classification (`IGRA_NONCE_001`, `IGRA_NONCE_002`).
- Reviewer run: `claude -p` requested stronger atomicity and explicit stale/replacement handling.
- Fix run: promoted store API to atomic lock+classify call and added stale replacement detection against existing sender+nonce records.
- Tests:
  - `cargo test -p foundry-common igra_store -- --nocapture` (expanded lock/nonce suite)
- Result: moved to final hardening cycle.

#### Cycle 2
- Implementer run: hardened lock-owner uniqueness and task-level concurrency test reliability; preserved strict nonce-gap behavior and explicit stale replacement observability.
- Reviewer run: `claude -p` final pass returned `NO_BLOCKING_FINDINGS`.
- Fix run: resolved one flaky expectation in atomic lock test and converted concurrent transport test to multi-thread + spawned tasks.
- Tests:
  - `cargo test -p foundry-common igra_store -- --nocapture` passed
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed
- Result: Item 5 complete.

### Item 6: Recovery + Polling

#### Cycle 1
- Implementer run: `crates/cast/src/tx.rs` receipt path updated to explicit timeout-aware polling when tx is not yet indexed:
  - exponential backoff (`1s -> 2s -> 4s -> ... capped at 10s`)
  - timeout bound (`igra.el_receipt_timeout_secs`, default 300s in receipt path)
  - handoff to `PendingTransactionBuilder` for confirmation wait with remaining timeout budget.
- Reviewer run: `claude -p` flagged confirmation semantics and timeout edge-cases.
- Fix run: ensured backoff path always transitions through `PendingTransactionBuilder` with required confirmations and explicit remaining timeout.
- Tests:
  - `cargo test -p cast guardrails_ -- --nocapture` passed (compile + existing receipt/send paths)
- Result: moved to final re-review.

#### Cycle 2
- Reviewer run: final Claude pass reported `NO_BLOCKING_FINDINGS`.
- Fix run: no additional functional changes required.
- Tests:
  - `cargo test -p cast guardrails_ -- --nocapture` passed
- Result: Item 6 complete for receipt recovery/polling scope.

### Item 7: Cache + GC

#### Cycle 1
- Implementer run: added cache/profile enforcement in `crates/common/src/igra_store.rs`:
  - profile metadata keys in `schema_meta`
  - startup invalidation for chain/network/RPC fingerprint drift
  - retention pruning (`completed_retention_hours`, `failed_retention_hours`)
  - size budget enforcement (`max_db_size_mb`)
  - new config fields wired from `crates/config/src/igra.rs` through `crates/common/src/provider/mod.rs`.
- Reviewer run: `claude -p` flagged cache-read side-effects in `cast receipt`/`cast igra-status`, metadata transition edge-case, and pruning/index concerns.
- Fix run: introduced `IgraStore::open_read_only` for status/receipt reads, changed metadata mismatch logic to include `None -> Some`, added prune index, and made size-budget pruning iterative.
- Tests:
  - `cargo test -p foundry-config igra -- --nocapture` passed
  - `cargo test -p foundry-common igra_store -- --nocapture` passed (8 tests)
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed
- Result: moved to final review cycle.

#### Cycle 2
- Reviewer run: final Claude pass returned `NO_BLOCKING_FINDINGS`.
- Fix run: no additional functional changes required after final pass.
- Tests:
  - `cargo test -p foundry-common igra_store -- --nocapture` passed
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed
- Result: Item 7 complete.

### Item 8: User Introspection + Dry-run

#### Cycle 1
- Implementer run: added `cast igra-status` command in `crates/cast/src/opts.rs` and execution path in `crates/cast/src/args.rs`; added receipt JSON augmentation to include `"igra"` metadata when IGRA is enabled.
- Reviewer run: `claude -p` pass over item-7/8 scope; concerns addressed for cache-read safety by switching status/receipt lookup to `open_read_only`.
- Fix run: explicit `"igra": null` output when no cache row exists for receipt augmentation.
- Tests:
  - `cargo test -p cast guardrails_ -- --nocapture` passed
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed
- Result: `cast igra-status` + receipt metadata bridge complete; moved dry-run to cycle 2.

#### Cycle 2
- Implementer run: added `--dry-run` (`--simulate` alias) to `SendTxOpts` in `crates/cast/src/tx.rs`, wired through `crates/cast/src/cmd/send.rs` and `crates/cast/src/cmd/erc20.rs`:
  - dry-run prints prepared transaction payload and exits without broadcast
  - tempo/raw flows print raw tx hex for validation
  - browser signer dry-run prints request JSON and exits.
- Reviewer run: Claude review flagged one compatibility issue (`confs=0` forced to 1) in receipt polling path.
- Fix run: restored `confs=0` behavior while keeping exponential polling + timeout budget for `confs>0`; final Claude pass returned `NO_BLOCKING_FINDINGS`.
- Tests:
  - `cargo test -p cast guardrails_ -- --nocapture` passed
- Result: Item 8 complete.

### Item 9: Observability + Errors

#### Cycle 1
- Implementer run: added structured lifecycle transition logs in `crates/common/src/provider/igra_transport.rs` with correlation id, sender, nonce, state, and error code fields under `target: "igra.lifecycle"`.
- Reviewer run: included in Item 7/8/6 review passes; no blocking findings against logging wiring.
- Fix run: imported explicit tracing macros and verified compile/test coverage.
- Tests:
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed
- Remaining risks:
  - metrics export surface and full error-catalog docs/command output mapping are still open.

#### Cycle 2
- Reviewer run: final Claude pass over observability/error-catalog changes returned `NO_BLOCKING_FINDINGS`.
- Fix run: no further code changes required after final review.
- Tests:
  - `cargo test -p foundry-common exposes_error_catalog_entries -- --nocapture` passed
  - `cargo test -p foundry-common igra_transport -- --nocapture` passed
- Result: Item 9 complete (structured lifecycle logs + user-facing error catalog wiring in status output).

### Item 10: Deterministic Harness + CI

#### Cycle 1
- Implementer run: deterministic harness + CI gate added:
  - `scripts/igra/deterministic-harness.sh`
  - `.github/workflows/igra-deterministic.yml`
  - `docs/dev/igra-deterministic-harness.md`
  - `docs/dev/README.md` link update
- Reviewer run: deferred until cycle 2 to include command-path guardrail parity and harness command list finalization.
- Fix run: stabilized harness env handling (`CARGO_TARGET_DIR`, optional offline wrapper behavior) and validated full harness execution.
- Tests:
  - `IGRA_OFFLINE=1 CARGO_TARGET_DIR=target CARGO_BUILD_TARGET_DIR=target ./scripts/igra/deterministic-harness.sh` passed
- Result: baseline deterministic gate operational in local and CI contexts.

#### Cycle 2
- Implementer run: guardrail coverage parity for script write-paths:
  - `crates/script/src/lib.rs`
  - Added IGRA write-path guardrail enforcement in `ScriptArgs::preprocess` (active for `--broadcast|--resume|--verify`, bypass for pure simulation).
  - Added/expanded `guardrails_` tests including explicit `resume`, `verify`, supported signer path, and simulation bypass assertions.
  - Harness updated to include script guardrail suite: `cargo test -p forge-script guardrails_ -- --nocapture`.
- Reviewer run (Claude cycle 1): mixed findings with one accepted gap (missing explicit `resume`/`verify` guardrail tests).
- Fix run: added the missing tests; reran target suites.
- Reviewer run (Claude cycle 2, scoped diff): `NO_BLOCKING_FINDINGS` after clarifying intended semantics (`broadcast|resume|verify` are write-path triggers).
- Tests:
  - `cargo test -p forge-script guardrails_ -- --nocapture` passed (7 tests)
  - `cargo test -p cast guardrails_ -- --nocapture` passed
  - `cargo test -p forge guardrails_ -- --nocapture` passed
- Result: Item 10 complete for deterministic harness + CI + write-path guardrail test coverage scope.
