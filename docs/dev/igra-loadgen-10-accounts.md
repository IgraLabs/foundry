# IGRA-Kaspa Testnet Loadgen With 10 Accounts (Repro Guide)

This document explains how to reproduce the 10-account funding + load test setup used in this repo for the IGRA-Kaspa testnet integration.

## Prereqs

- Rust toolchain installed.
- Access to:
  - EL JSON-RPC endpoint (HTTPS): `https://galleon-testnet.igralabs.com:8545`
  - Kaspa gRPC endpoint: `grpc://stage-roman.igralabs.com:16210`
- Initial funds available on:
  - 1 EVM account (IKAS/native EL gas token) to fan out to 10 accounts
  - 1 Kaspa account (KAS) to fan out to 10 accounts

Notes:
- If you are using the same accounts as `scripts/igra/testnet-smoke.sh`, your funded Kaspa wallet likely uses the convention `passphrase == mnemonic`. Keep that consistent.
- For reliable RPC connectivity in sandboxed environments, `igra-loadgen` supports `--no-proxy` (disables automatic proxy detection for HTTP(S) RPC).

## Build Binaries

From repo root:

```bash
cd /Users/user/Source/igra/foundry

# Optional: keep all build artifacts local to the repo (recommended).
export CARGO_TARGET_DIR="$PWD/target"
export RUSTC_WRAPPER=

cargo build -p cast --release
cargo build -p igra-loadgen --release
cargo build -p foundry-common --bin kaspa_utxos --release
cargo build -p foundry-common --bin kaspa_fund --release
```

Binaries will be under `$CARGO_TARGET_DIR/release/`:
- `cast`
- `igra-loadgen`
- `kaspa_utxos`
- `kaspa_fund`

## Configuration (Endpoints)

Set these shell vars (or inline them in commands):

```bash
export EL_RPC_URL="https://galleon-testnet.igralabs.com:8545"
export KASPA_GRPC_URL="grpc://stage-roman.igralabs.com:16210"
export KASPA_NETWORK="testnet-10"

# Viaduct / adapter txid prefix for galleon testnet (testnet-10):
export IGRA_TX_ID_PREFIX="97b4"
```

## Generate 10 EVM + 10 Kaspa Accounts (Deterministic)

Create a temp working directory:

```bash
WORKDIR="$(mktemp -d /tmp/igra-loadgen-10.XXXXXX)"
echo "$WORKDIR"
```

### EVM keys (10)

Set the EVM mnemonic that holds your initial IKAS funds (example below uses the default Foundry/anvil mnemonic, DO NOT use this on mainnet):

```bash
export EVM_MNEMONIC="test test test test test test test test test test test junk"
```

Derive keys 0..9:

```bash
for i in $(seq 0 9); do
  "$CARGO_TARGET_DIR/release/cast" wallet private-key --mnemonic "$EVM_MNEMONIC" --mnemonic-index "$i"
done > "$WORKDIR/evm_keys.txt"

while read -r pk; do
  "$CARGO_TARGET_DIR/release/cast" wallet address --private-key "$pk"
done < "$WORKDIR/evm_keys.txt" > "$WORKDIR/evm_addrs.txt"
```

### Kaspa keys (10) via mnemonic derivation

Set the Kaspa mnemonic that holds your initial KAS funds:

```bash
export KASPA_MNEMONIC="(your kaspa mnemonic here)"

# If your funded wallet used a non-empty BIP39 passphrase, set it.
# For the smoke-test accounts used in this repo, the convention was: passphrase == mnemonic.
export KASPA_MNEMONIC_PASSPHRASE="$KASPA_MNEMONIC"
```

Print the derived (worker -> evm_sender + kaspa_address) mapping and store addresses:

```bash
"$CARGO_TARGET_DIR/release/igra-loadgen" \
  --el-rpc-url "$EL_RPC_URL" \
  --kaspa-rpc-url "$KASPA_GRPC_URL" \
  --kaspa-network "$KASPA_NETWORK" \
  --tx-id-prefix "$IGRA_TX_ID_PREFIX" \
  --evm-keys "$WORKDIR/evm_keys.txt" \
  --kaspa-mnemonic "$KASPA_MNEMONIC" \
  --kaspa-mnemonic-passphrase "$KASPA_MNEMONIC_PASSPHRASE" \
  --kaspa-mnemonic-index-start 0 \
  --kaspa-mnemonic-count 10 \
  --print-addresses > "$WORKDIR/addrs.json"

jq -r '.[].kaspa_address' "$WORKDIR/addrs.json" > "$WORKDIR/kaspa_addrs.txt"
```

## Fund 10 Kaspa Accounts (KAS)

Before funding, verify current UTXOs:

```bash
"$CARGO_TARGET_DIR/release/kaspa_utxos" "$KASPA_GRPC_URL" $(tr '\n' ' ' < "$WORKDIR/kaspa_addrs.txt")
```

Fan-out KAS from your funded Kaspa source key.

Important:
- Very small outputs can be rejected as non-standard due to storage-mass rules (KIP-0009).
- Use at least `1 KAS = 100000000 sompi` per worker; for stress tests, `10 KAS = 1000000000 sompi` per worker is fine.

Example: send **10 KAS** to workers 2..10 (leave worker 1 as the source):

```bash
TO_ARGS=()
idx=0
while read -r addr; do
  idx=$((idx+1))
  if [ "$idx" -ge 2 ]; then
    TO_ARGS+=(--to "${addr}:1000000000")
  fi
done < "$WORKDIR/kaspa_addrs.txt"

"$CARGO_TARGET_DIR/release/kaspa_fund" \
  --kaspa-rpc-url "$KASPA_GRPC_URL" \
  --kaspa-network "$KASPA_NETWORK" \
  --mnemonic "$KASPA_MNEMONIC" \
  --mnemonic-passphrase "$KASPA_MNEMONIC_PASSPHRASE" \
  --mnemonic-index 0 \
  "${TO_ARGS[@]}"
```

Re-check UTXOs:

```bash
"$CARGO_TARGET_DIR/release/kaspa_utxos" "$KASPA_GRPC_URL" $(tr '\n' ' ' < "$WORKDIR/kaspa_addrs.txt")
```

## Fund 10 EVM Accounts (IKAS)

We fund EVM workers 2..10 from worker 1 by building raw txs with `cast mktx` and submitting via `curl`.
This avoids occasional issues with automatic proxy/DNS integration in some environments.

```bash
FUND_PK="$(head -n 1 "$WORKDIR/evm_keys.txt")"
FUND_ADDR="$("$CARGO_TARGET_DIR/release/cast" wallet address --private-key "$FUND_PK")"

CHAIN_ID_HEX="$(curl -sS -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
  "$EL_RPC_URL" | jq -r .result)"
CHAIN_ID=$((16#${CHAIN_ID_HEX#0x}))

GAS_PRICE_HEX="$(curl -sS -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_gasPrice","params":[]}' \
  "$EL_RPC_URL" | jq -r .result)"
GAS_PRICE=$((16#${GAS_PRICE_HEX#0x}))

NONCE_HEX="$(curl -sS -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_getTransactionCount","params":["'"$FUND_ADDR"'","pending"]}' \
  "$EL_RPC_URL" | jq -r .result)"
NONCE=$((16#${NONCE_HEX#0x}))

# Send 10 IKAS to each of workers 2..10.
for TO in $(tail -n +2 "$WORKDIR/evm_addrs.txt"); do
  RAW="$("$CARGO_TARGET_DIR/release/cast" mktx \
    --chain "$CHAIN_ID" \
    --private-key "$FUND_PK" \
    --legacy \
    --nonce "$NONCE" \
    --gas-limit 21000 \
    --gas-price "$GAS_PRICE" \
    --value 10000000000000000000 \
    "$TO")"

  curl -sS -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_sendRawTransaction","params":["'"$RAW"'"]}' \
    "$EL_RPC_URL" | jq -r '.error?.message // .result'

  NONCE=$((NONCE+1))
done
```

Verify balances:

```bash
while read -r a; do
  b="$(curl -sS -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBalance","params":["'"$a"'","latest"]}' \
    "$EL_RPC_URL" | jq -r .result)"
  echo "$a $b"
done < "$WORKDIR/evm_addrs.txt"
```

## Run The 10-Worker Load Test

Sanity run (target 5 TPS aggregate for 20s):

```bash
RUST_LOG=warn "$CARGO_TARGET_DIR/release/igra-loadgen" \
  --el-rpc-url "$EL_RPC_URL" \
  --kaspa-rpc-url "$KASPA_GRPC_URL" \
  --kaspa-network "$KASPA_NETWORK" \
  --tx-id-prefix "$IGRA_TX_ID_PREFIX" \
  --mining-timeout-secs 120 \
  --evm-keys "$WORKDIR/evm_keys.txt" \
  --kaspa-mnemonic "$KASPA_MNEMONIC" \
  --kaspa-mnemonic-passphrase "$KASPA_MNEMONIC_PASSPHRASE" \
  --kaspa-mnemonic-index-start 0 \
  --kaspa-mnemonic-count 10 \
  --tps 5 \
  --duration-secs 20 \
  --report-secs 5 \
  --no-proxy \
  --gas-limit 21000 \
  --max-fee-per-gas 2000000000000 \
  --max-priority-fee-per-gas 1000000000000
```

Max-throughput (send as fast as possible for 15s):

```bash
RUST_LOG=warn "$CARGO_TARGET_DIR/release/igra-loadgen" \
  --el-rpc-url "$EL_RPC_URL" \
  --kaspa-rpc-url "$KASPA_GRPC_URL" \
  --kaspa-network "$KASPA_NETWORK" \
  --tx-id-prefix "$IGRA_TX_ID_PREFIX" \
  --mining-timeout-secs 120 \
  --evm-keys "$WORKDIR/evm_keys.txt" \
  --kaspa-mnemonic "$KASPA_MNEMONIC" \
  --kaspa-mnemonic-passphrase "$KASPA_MNEMONIC_PASSPHRASE" \
  --kaspa-mnemonic-index-start 0 \
  --kaspa-mnemonic-count 10 \
  --tps 0 \
  --duration-secs 15 \
  --report-secs 5 \
  --no-proxy \
  --gas-limit 21000 \
  --max-fee-per-gas 2000000000000 \
  --max-priority-fee-per-gas 1000000000000
```

## Notes / Gotchas

- With `tx_id_prefix=97b4` (2 bytes), txid prefix mining dominates CPU and limits achievable TPS per worker. To reach very high TPS (e.g. 500 TPS) you will likely need:
  - a shorter prefix (1 byte) or a mode that relaxes/turns off prefix mining for stress windows, and
  - sufficient EL gas funding (current observed `eth_gasPrice` makes 500 TPS for hours extremely expensive).
- `kaspa_utxo_mode=chain` is used by `igra-loadgen` (default in this guide) to avoid UTXO contention and reduce RPC load at higher send rates.

