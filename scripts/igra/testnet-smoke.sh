#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET_DIR="${TARGET_DIR:-${ROOT_DIR}/target}"
CAST_BIN="${CAST_BIN:-${TARGET_DIR}/debug/cast}"
FORGE_BIN="${FORGE_BIN:-${TARGET_DIR}/debug/forge}"

# Avoid inheriting an unwritable global CARGO_TARGET_DIR (e.g. external volume paths).
export CARGO_TARGET_DIR="${TARGET_DIR}"
export RUSTC_WRAPPER=

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "[igra-smoke] missing required command: $1"
    exit 1
  fi
}

for cmd in cargo jq mktemp; do
  require_cmd "$cmd"
done

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  echo "[igra-smoke] building cast/forge binaries in ${TARGET_DIR}"
  (cd "${ROOT_DIR}" && cargo build -p cast -p forge >/dev/null)
fi

IGRA_EL_RPC_URL="${IGRA_EL_RPC_URL:-https://galleon-testnet.igralabs.com:8545}"
IGRA_KASPA_RPC_URL="${IGRA_KASPA_RPC_URL:-grpc://stage-roman.igralabs.com:16210}"
IGRA_PRIVATE_KEY="${IGRA_PRIVATE_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
IGRA_PRIVATE_KEY_KASPA="${IGRA_PRIVATE_KEY_KASPA:-}"
IGRA_MNEMONIC_KASPA="${IGRA_MNEMONIC_KASPA:-}"
# Passphrase semantics:
# - default is empty passphrase (standard BIP39)
# - to use `passphrase == mnemonic`, set IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC=1
# - to explicitly force empty passphrase, set IGRA_MNEMONIC_PASSPHRASE_KASPA_EMPTY=1
IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC="${IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC:-0}"
IGRA_MNEMONIC_PASSPHRASE_KASPA_EMPTY="${IGRA_MNEMONIC_PASSPHRASE_KASPA_EMPTY:-0}"
IGRA_MNEMONIC_PASSPHRASE_KASPA_IS_SET=0
if [[ -n "${IGRA_MNEMONIC_PASSPHRASE_KASPA+x}" ]]; then
  IGRA_MNEMONIC_PASSPHRASE_KASPA_IS_SET=1
fi
IGRA_MNEMONIC_PASSPHRASE_KASPA="${IGRA_MNEMONIC_PASSPHRASE_KASPA-}"
IGRA_MNEMONIC_DERIVATION_PATH_KASPA="${IGRA_MNEMONIC_DERIVATION_PATH_KASPA:-}"
IGRA_MNEMONIC_INDEX_KASPA="${IGRA_MNEMONIC_INDEX_KASPA:-}"

IGRA_EXPECTED_CHAIN_ID="${IGRA_EXPECTED_CHAIN_ID:-}"
IGRA_KASPA_NETWORK="${IGRA_KASPA_NETWORK:-testnet-10}"
# Viaduct Transaction ID Prefix (network specific):
# - galleon-testnet (testnet-10): 97b4
# - mainnet: 97b1
IGRA_TX_ID_PREFIX="${IGRA_TX_ID_PREFIX:-97b4}"
IGRA_EL_RECEIPT_TIMEOUT_SECS="${IGRA_EL_RECEIPT_TIMEOUT_SECS:-300}"
IGRA_MINING_TIMEOUT_SECS="${IGRA_MINING_TIMEOUT_SECS:-120}"
IGRA_SENDER_LOCK_TIMEOUT_SECS="${IGRA_SENDER_LOCK_TIMEOUT_SECS:-60}"
# Keep v1 deterministic: uncompressed L2Data payload (header 0x94).
IGRA_PAYLOAD_COMPRESSION="${IGRA_PAYLOAD_COMPRESSION:-none}"
CHECK_KASPA_MEMPOOL="${CHECK_KASPA_MEMPOOL:-1}"

MINT_CONTRACT="${MINT_CONTRACT:-0xB981f5B62d94285976C9Bdaea65193BBe906972E}"
MINT_AMOUNT="${MINT_AMOUNT:-5000000}"
MINT_TO="${MINT_TO:-}"

RUN_FORGE_SCRIPT="${RUN_FORGE_SCRIPT:-1}"
KEEP_TMP="${KEEP_TMP:-0}"
SEND_ASYNC="${SEND_ASYNC:-1}"
CAST_LEGACY="${CAST_LEGACY:-0}"

MNEMONIC_PASSPHRASE_EFFECTIVE=""
if [[ "${IGRA_MNEMONIC_PASSPHRASE_KASPA_EMPTY}" == "1" ]]; then
  IGRA_MNEMONIC_PASSPHRASE_KASPA_IS_SET=1
  MNEMONIC_PASSPHRASE_EFFECTIVE=""
elif [[ "${IGRA_MNEMONIC_PASSPHRASE_KASPA_IS_SET}" == "1" ]]; then
  MNEMONIC_PASSPHRASE_EFFECTIVE="${IGRA_MNEMONIC_PASSPHRASE_KASPA}"
elif [[ -n "${IGRA_MNEMONIC_KASPA}" && "${IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC}" == "1" ]]; then
  MNEMONIC_PASSPHRASE_EFFECTIVE="${IGRA_MNEMONIC_KASPA}"
fi

TMP_DIR=""
cleanup() {
  if [[ -n "${TMP_DIR}" && -d "${TMP_DIR}" && "${KEEP_TMP}" != "1" ]]; then
    rm -rf "${TMP_DIR}"
  fi
}
trap cleanup EXIT

normalize_uint() {
  local raw="$1"
  local token="${raw%% *}"
  if [[ "${token}" == 0x* ]]; then
    "${CAST_BIN}" to-dec "${token}"
  else
    echo "${token}"
  fi
}

echo "[igra-smoke] checking EL chain id via ${IGRA_EL_RPC_URL}"
CHAIN_ID="$("${CAST_BIN}" chain-id --rpc-url "${IGRA_EL_RPC_URL}")"
echo "[igra-smoke] EL chain id: ${CHAIN_ID}"

if [[ -n "${IGRA_EXPECTED_CHAIN_ID}" && "${IGRA_EXPECTED_CHAIN_ID}" != "${CHAIN_ID}" ]]; then
  echo "[igra-smoke] expected_el_chain_id mismatch: configured=${IGRA_EXPECTED_CHAIN_ID} actual=${CHAIN_ID}"
  exit 1
fi
IGRA_EXPECTED_CHAIN_ID="${IGRA_EXPECTED_CHAIN_ID:-${CHAIN_ID}}"

SENDER_ADDRESS="$("${CAST_BIN}" wallet address --private-key "${IGRA_PRIVATE_KEY}")"
if [[ -z "${MINT_TO}" ]]; then
  MINT_TO="${SENDER_ADDRESS}"
fi

TMP_DIR="$(mktemp -d "/tmp/igra-testnet-flow.XXXXXX")"
WORK_DIR="${TMP_DIR}/flow"
mkdir -p "${WORK_DIR}"
cd "${WORK_DIR}"

# Isolate Foundry cache per run so IGRA tx-map state (sender nonce tracking) doesn't
# leak across smoke runs and cause BLOCKED_NONCE_GAP errors.
# Note: foundry cache paths are derived from HOME, so we sandbox HOME to a temp dir.
HOME_DIR="${WORK_DIR}/home"
mkdir -p "${HOME_DIR}"
export HOME="${HOME_DIR}"

if [[ "${RUN_FORGE_SCRIPT}" == "1" ]]; then
  echo "[igra-smoke] initializing temporary forge project"
  "${FORGE_BIN}" init --force --no-git . >/dev/null
fi

cat > foundry.toml <<EOF
[profile.default]
eth_rpc_url = "${IGRA_EL_RPC_URL}"

[igra]
enabled = true
el_rpc_url = "${IGRA_EL_RPC_URL}"
kaspa_rpc_url = "${IGRA_KASPA_RPC_URL}"
expected_el_chain_id = ${IGRA_EXPECTED_CHAIN_ID}
kaspa_network = "${IGRA_KASPA_NETWORK}"
tx_id_prefix = "${IGRA_TX_ID_PREFIX}"
el_receipt_timeout_secs = ${IGRA_EL_RECEIPT_TIMEOUT_SECS}
mining_timeout_secs = ${IGRA_MINING_TIMEOUT_SECS}
sender_lock_timeout_secs = ${IGRA_SENDER_LOCK_TIMEOUT_SECS}
EOF

echo "payload_compression = \"${IGRA_PAYLOAD_COMPRESSION}\"" >> foundry.toml

if [[ -n "${IGRA_PRIVATE_KEY_KASPA}" || -n "${IGRA_MNEMONIC_KASPA}" || -n "${IGRA_MNEMONIC_PASSPHRASE_KASPA}" || -n "${IGRA_MNEMONIC_DERIVATION_PATH_KASPA}" || -n "${IGRA_MNEMONIC_INDEX_KASPA}" ]]; then
  {
    echo ""
    echo "[igra.kaspa_wallet]"
    if [[ -n "${IGRA_PRIVATE_KEY_KASPA}" ]]; then
      echo "private_key = \"${IGRA_PRIVATE_KEY_KASPA}\""
    fi
    if [[ -n "${IGRA_MNEMONIC_KASPA}" ]]; then
      echo "mnemonic = \"${IGRA_MNEMONIC_KASPA}\""
    fi
    if [[ "${IGRA_MNEMONIC_PASSPHRASE_KASPA_IS_SET}" == "1" || "${IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC}" == "1" || "${IGRA_MNEMONIC_PASSPHRASE_KASPA_EMPTY}" == "1" ]]; then
      echo "mnemonic_passphrase = \"${MNEMONIC_PASSPHRASE_EFFECTIVE}\""
    fi
    if [[ -n "${IGRA_MNEMONIC_DERIVATION_PATH_KASPA}" ]]; then
      echo "mnemonic_derivation_path = \"${IGRA_MNEMONIC_DERIVATION_PATH_KASPA}\""
    fi
    if [[ -n "${IGRA_MNEMONIC_INDEX_KASPA}" ]]; then
      echo "mnemonic_index = ${IGRA_MNEMONIC_INDEX_KASPA}"
    fi
  } >> foundry.toml
fi

if [[ "${RUN_FORGE_SCRIPT}" == "1" ]]; then
  mkdir -p script
  cat > script/IgraMint.s.sol <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Script.sol";

interface IMintableToken {
    function mint(address to, uint256 amount) external returns (bool);
}

contract IgraMintScript is Script {
    function run() external {
        address token = vm.envAddress("MINT_CONTRACT");
        address recipient = vm.envAddress("MINT_TO");
        uint256 amount = vm.envUint("MINT_AMOUNT");

        vm.startBroadcast();
        IMintableToken(token).mint(recipient, amount);
        vm.stopBroadcast();
    }
}
EOF
fi

echo "[igra-smoke] using sender ${SENDER_ADDRESS}"
echo "[igra-smoke] mint target ${MINT_CONTRACT}"
echo "[igra-smoke] mint recipient ${MINT_TO}"
echo "[igra-smoke] mint amount (raw) ${MINT_AMOUNT}"

BAL_BEFORE_RAW="$("${CAST_BIN}" call "${MINT_CONTRACT}" "balanceOf(address)(uint256)" "${MINT_TO}" --rpc-url "${IGRA_EL_RPC_URL}")"
BAL_BEFORE="$(normalize_uint "${BAL_BEFORE_RAW}")"
echo "[igra-smoke] balance before: ${BAL_BEFORE}"

SEND_ARGS=(
  send "${MINT_CONTRACT}" "mint(address,uint256)" "${MINT_TO}" "${MINT_AMOUNT}"
  --rpc-url "${IGRA_EL_RPC_URL}"
  --private-key "${IGRA_PRIVATE_KEY}"
)
if [[ "${CAST_LEGACY}" == "1" ]]; then
  SEND_ARGS+=(--legacy)
fi
if [[ "${SEND_ASYNC}" == "1" ]]; then
  SEND_ARGS+=(--async)
else
  SEND_ARGS+=(--json)
fi
if [[ -n "${IGRA_PRIVATE_KEY_KASPA}" ]]; then
  SEND_ARGS+=(--private-key-kaspa "${IGRA_PRIVATE_KEY_KASPA}")
fi
if [[ -n "${IGRA_MNEMONIC_KASPA}" ]]; then
  SEND_ARGS+=(--mnemonic-kaspa "${IGRA_MNEMONIC_KASPA}")
fi
if [[ "${IGRA_MNEMONIC_PASSPHRASE_KASPA_IS_SET}" == "1" || "${IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC}" == "1" || "${IGRA_MNEMONIC_PASSPHRASE_KASPA_EMPTY}" == "1" ]]; then
  SEND_ARGS+=(--mnemonic-passphrase-kaspa "${MNEMONIC_PASSPHRASE_EFFECTIVE}")
fi
if [[ -n "${IGRA_MNEMONIC_DERIVATION_PATH_KASPA}" ]]; then
  SEND_ARGS+=(--mnemonic-derivation-path-kaspa "${IGRA_MNEMONIC_DERIVATION_PATH_KASPA}")
fi
if [[ -n "${IGRA_MNEMONIC_INDEX_KASPA}" ]]; then
  SEND_ARGS+=(--mnemonic-index-kaspa "${IGRA_MNEMONIC_INDEX_KASPA}")
fi

echo "[igra-smoke] sending cast mint tx (IGRA mode -> Kaspa wrap path)"
if ! "${CAST_BIN}" "${SEND_ARGS[@]}" > cast-send.json 2> cast-send.err; then
  cat cast-send.err >&2
  if grep -q "insufficient Kaspa UTXOs for fee payment" cast-send.err &&
    [[ -z "${IGRA_PRIVATE_KEY_KASPA}" && -z "${IGRA_MNEMONIC_KASPA}" ]]
  then
    echo "[igra-smoke] hint: no Kaspa UTXOs found for fallback signer." >&2
    echo "[igra-smoke] set IGRA_MNEMONIC_KASPA (or IGRA_PRIVATE_KEY_KASPA) to a funded Kaspa key." >&2
    echo "[igra-smoke] if your wallet used a non-empty BIP39 passphrase, also set IGRA_MNEMONIC_PASSPHRASE_KASPA." >&2
  fi
  exit 1
fi

if [[ "${SEND_ASYNC}" == "1" ]]; then
  L2_TX_HASH="$(tr -d '\r' < cast-send.json | head -n 1 | tr -d '[:space:]')"
else
  L2_TX_HASH="$(jq -r '.transactionHash // .hash // empty' cast-send.json)"
fi
if [[ -z "${L2_TX_HASH}" || "${L2_TX_HASH}" == "null" || "${L2_TX_HASH}" != 0x* ]]; then
  echo "[igra-smoke] failed to extract L2 tx hash from cast-send.json"
  echo "[igra-smoke] cast-send.json content:"
  sed -n '1,20p' cast-send.json
  exit 1
fi
echo "[igra-smoke] L2 tx hash: ${L2_TX_HASH}"

echo "[igra-smoke] fetching cast igra-status (local cache)"
"${CAST_BIN}" igra-status "${L2_TX_HASH}" --rpc-url "${IGRA_EL_RPC_URL}" --json | tee cast-igra-status.json >/dev/null

KASPA_TX_ID="$(jq -r '.kaspa_tx_id // empty' cast-igra-status.json)"
KASPA_SOURCE_ADDRESS="$(grep -Eo \"kaspa_source_address=[^ ]+\" cast-send.err 2>/dev/null | head -n 1 | cut -d= -f2 || true)"
if [[ -n "${KASPA_TX_ID}" ]]; then
  echo "[igra-smoke] kaspa tx id: ${KASPA_TX_ID}"
  if [[ -n "${KASPA_SOURCE_ADDRESS}" ]]; then
    echo "[igra-smoke] kaspa source address: ${KASPA_SOURCE_ADDRESS}"
  fi
  if [[ "${CHECK_KASPA_MEMPOOL}" == "1" ]]; then
    echo "[igra-smoke] checking Kaspa mempool visibility via gRPC"
    (cd "${ROOT_DIR}" && cargo run -q -p igra-kaspa-derive --bin igra-kaspa-mempool -- \
      --kaspa-rpc-url "${IGRA_KASPA_RPC_URL}" \
      --tx-id "${KASPA_TX_ID}" \
      ) | tee kaspa-mempool.json >/dev/null || true
    FOUND_IN_MEMPOOL="$(jq -r '.found // false' kaspa-mempool.json 2>/dev/null || echo false)"
    echo "[igra-smoke] kaspa mempool found: ${FOUND_IN_MEMPOOL}"
  fi
fi

echo "[igra-smoke] waiting for cast receipt (timeout=${IGRA_EL_RECEIPT_TIMEOUT_SECS}s)"
start_ts="$(date +%s)"
backoff=1
while :; do
  if "${CAST_BIN}" receipt "${L2_TX_HASH}" --rpc-url "${IGRA_EL_RPC_URL}" --json --async > cast-receipt.json 2>/dev/null; then
    break
  fi

  now_ts="$(date +%s)"
  elapsed="$((now_ts - start_ts))"
  if (( elapsed >= IGRA_EL_RECEIPT_TIMEOUT_SECS )); then
    echo "[igra-smoke] receipt not found after ${elapsed}s"
    if [[ -n "${KASPA_TX_ID}" ]]; then
      echo "[igra-smoke] diagnostic: checking whether Kaspa tx is confirmed (UTXO index)"
      (cd "${ROOT_DIR}" && cargo run -q -p igra-kaspa-derive --bin igra-kaspa-utxos -- \
        --kaspa-rpc-url "${IGRA_KASPA_RPC_URL}" \
        --address "${KASPA_SOURCE_ADDRESS:-}" \
        ) > kaspa-utxos.json 2>/dev/null || true
      if [[ -s kaspa-utxos.json ]]; then
        if jq -e --arg tx "${KASPA_TX_ID}" '.utxos[]? | select(.txId == $tx)' kaspa-utxos.json >/dev/null 2>&1; then
          echo "[igra-smoke] kaspa tx appears in UTXO set: true (confirmed on Kaspa)"
        else
          echo "[igra-smoke] kaspa tx appears in UTXO set: false (may be dropped or not yet indexed)"
        fi
      fi
    fi
    exit 1
  fi

  echo "[igra-smoke] ... still waiting (${elapsed}s elapsed)"
  sleep "${backoff}"
  if (( backoff < 10 )); then
    backoff="$((backoff * 2))"
    if (( backoff > 10 )); then backoff=10; fi
  fi
done

echo "[igra-smoke] receipt found"

echo "[igra-smoke] refreshing cast igra-status (local cache)"
"${CAST_BIN}" igra-status "${L2_TX_HASH}" --rpc-url "${IGRA_EL_RPC_URL}" --json | tee cast-igra-status.json >/dev/null

BAL_AFTER_RAW="$("${CAST_BIN}" call "${MINT_CONTRACT}" "balanceOf(address)(uint256)" "${MINT_TO}" --rpc-url "${IGRA_EL_RPC_URL}")"
BAL_AFTER="$(normalize_uint "${BAL_AFTER_RAW}")"
echo "[igra-smoke] balance after: ${BAL_AFTER}"

if [[ "${RUN_FORGE_SCRIPT}" == "1" ]]; then
  echo "[igra-smoke] running forge script broadcast"
  export MINT_CONTRACT MINT_TO MINT_AMOUNT

  FORGE_ARGS=(
    script script/IgraMint.s.sol:IgraMintScript
    --rpc-url "${IGRA_EL_RPC_URL}"
    --broadcast
    --private-key "${IGRA_PRIVATE_KEY}"
    -vv
  )
  if [[ -n "${IGRA_PRIVATE_KEY_KASPA}" ]]; then
    FORGE_ARGS+=(--private-key-kaspa "${IGRA_PRIVATE_KEY_KASPA}")
  fi
  if [[ -n "${IGRA_MNEMONIC_KASPA}" ]]; then
    FORGE_ARGS+=(--mnemonic-kaspa "${IGRA_MNEMONIC_KASPA}")
  fi
  if [[ "${IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC}" == "1" ]]; then
    FORGE_ARGS+=(--mnemonic-passphrase-kaspa-as-mnemonic)
  elif [[ "${IGRA_MNEMONIC_PASSPHRASE_KASPA_EMPTY}" == "1" ]]; then
    FORGE_ARGS+=(--mnemonic-passphrase-kaspa-empty)
  elif [[ "${IGRA_MNEMONIC_PASSPHRASE_KASPA_IS_SET}" == "1" ]]; then
    FORGE_ARGS+=(--mnemonic-passphrase-kaspa "${IGRA_MNEMONIC_PASSPHRASE_KASPA}")
  fi
  if [[ -n "${IGRA_MNEMONIC_DERIVATION_PATH_KASPA}" ]]; then
    FORGE_ARGS+=(--mnemonic-derivation-path-kaspa "${IGRA_MNEMONIC_DERIVATION_PATH_KASPA}")
  fi
  if [[ -n "${IGRA_MNEMONIC_INDEX_KASPA}" ]]; then
    FORGE_ARGS+=(--mnemonic-index-kaspa "${IGRA_MNEMONIC_INDEX_KASPA}")
  fi

  "${FORGE_BIN}" "${FORGE_ARGS[@]}" | tee forge-script.log >/dev/null

  FORGE_BROADCAST_FILE="broadcast/IgraMint.s.sol/${CHAIN_ID}/run-latest.json"
  if [[ -f "${FORGE_BROADCAST_FILE}" ]]; then
    FORGE_TX_HASH="$(jq -r '.transactions[-1].hash // empty' "${FORGE_BROADCAST_FILE}")"
    if [[ -n "${FORGE_TX_HASH}" && "${FORGE_TX_HASH}" != "null" ]]; then
      echo "[igra-smoke] forge tx hash: ${FORGE_TX_HASH}"
      "${CAST_BIN}" receipt "${FORGE_TX_HASH}" --rpc-url "${IGRA_EL_RPC_URL}" --json | tee forge-receipt.json >/dev/null
      "${CAST_BIN}" igra-status "${FORGE_TX_HASH}" --rpc-url "${IGRA_EL_RPC_URL}" --json | tee forge-igra-status.json >/dev/null
    fi
  fi
fi

echo "[igra-smoke] complete"
echo "[igra-smoke] artifacts: ${WORK_DIR}"
if [[ "${KEEP_TMP}" != "1" ]]; then
  echo "[igra-smoke] set KEEP_TMP=1 to preserve artifacts"
fi
