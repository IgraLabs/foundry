# IGRA Deterministic Harness

This harness is the merge gate for IGRA transport/persistence behavior that must remain deterministic across platforms and CI runs.

## Commands

Run the deterministic suite:

```bash
./scripts/igra/deterministic-harness.sh
```

The script executes:

1. `cargo test -p foundry-config igra -- --nocapture`
2. `cargo test -p foundry-common provider::tests -- --nocapture`
3. `cargo test -p foundry-common igra_store -- --nocapture`
4. `cargo test -p foundry-common igra_transport -- --nocapture`
5. `cargo test -p cast guardrails_ -- --nocapture`
6. `cargo test -p forge guardrails_ -- --nocapture`

## Scenario Coverage Matrix

The deterministic harness currently enforces:

1. IGRA config validation and precedence safety.
2. Provider wiring + fail-fast behavior for invalid IGRA settings.
3. Sender lock serialization and nonce-gap handling.
4. Lifecycle persistence transitions (`RECEIVED_RAW_L2 -> KASPA_BROADCASTED`, failure states).
5. Tx envelope acceptance/rejection matrix for `eth_sendRawTransaction`.
6. Payload format (`version|tx_type` header + raw tx + 4-byte nonce).
7. Prefix mining behavior and timeout failure classification.
8. Submitter failure propagation and persistence of `FAILED_RECOVERABLE`.
9. Guardrail behavior for unsupported signer flows in `cast send` and `forge create`.

## Optional Network Inputs

Network endpoints can be provided for future network-backed scenarios:

- `IGRA_ENABLE_NETWORK_SCENARIOS=1`
- `IGRA_EL_RPC_URL=<el-rpc-url>`
- `IGRA_KASPA_RPC_URL=<kaspa-rpc-url>`

Current harness gate is deterministic and does not require live network connectivity.
