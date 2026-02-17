#!/usr/bin/env bash
set -euo pipefail

# IGRA testnet stress runner.
#
# Goals:
# - Generate many IGRA write-path submissions (eth_sendRawTransaction -> Kaspa payload tx).
# - Avoid cross-process SQLite contention by sandboxing each worker under its own HOME.
# - Keep knobs obvious so you can scale load without code changes.
#
# Notes on throughput limits (current implementation):
# - Kaspa txid-prefix mining is CPU-bound and scales ~linearly with cores and ~exponentially with
#   prefix length (2 bytes => ~65k tries on average).
# - Parallelism needs enough Kaspa UTXOs per worker; sharing one Kaspa key across workers often
#   results in UTXO contention/double-spend submission failures.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET_DIR="${TARGET_DIR:-${ROOT_DIR}/target}"
CAST_BIN="${CAST_BIN:-${TARGET_DIR}/debug/cast}"

export CARGO_TARGET_DIR="${TARGET_DIR}"
export RUSTC_WRAPPER=

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "[igra-stress] missing required command: $1"
    exit 1
  fi
}

for cmd in cargo jq mktemp python3; do
  require_cmd "$cmd"
done

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  echo "[igra-stress] building cast binary in ${TARGET_DIR}"
  (cd "${ROOT_DIR}" && cargo build -p cast >/dev/null)
fi

IGRA_EL_RPC_URL="${IGRA_EL_RPC_URL:-https://galleon-testnet.igralabs.com:8545}"
IGRA_KASPA_RPC_URL="${IGRA_KASPA_RPC_URL:-grpc://stage-roman.igralabs.com:16210}"
IGRA_KASPA_NETWORK="${IGRA_KASPA_NETWORK:-testnet-10}"
# Viaduct Transaction ID Prefix (network specific):
# - galleon-testnet (testnet-10): 97b4
# - mainnet: 97b1
IGRA_TX_ID_PREFIX="${IGRA_TX_ID_PREFIX:-97b4}"
IGRA_EL_RECEIPT_TIMEOUT_SECS="${IGRA_EL_RECEIPT_TIMEOUT_SECS:-300}"
IGRA_MINING_TIMEOUT_SECS="${IGRA_MINING_TIMEOUT_SECS:-120}"
IGRA_SENDER_LOCK_TIMEOUT_SECS="${IGRA_SENDER_LOCK_TIMEOUT_SECS:-60}"
IGRA_PAYLOAD_COMPRESSION="${IGRA_PAYLOAD_COMPRESSION:-none}"

# Load knobs.
IGRA_STRESS_WORKERS="${IGRA_STRESS_WORKERS:-1}"
IGRA_STRESS_TXS_PER_WORKER="${IGRA_STRESS_TXS_PER_WORKER:-10}"
IGRA_STRESS_PROGRESS_EVERY="${IGRA_STRESS_PROGRESS_EVERY:-10}"

# EVM keys:
# - If IGRA_STRESS_WORKERS=1, use IGRA_PRIVATE_KEY.
# - If IGRA_STRESS_WORKERS>1, set IGRA_STRESS_EVM_KEYS as comma-separated list.
IGRA_PRIVATE_KEY="${IGRA_PRIVATE_KEY:-}"
IGRA_STRESS_EVM_KEYS="${IGRA_STRESS_EVM_KEYS:-}"

# Kaspa signer material (single-worker defaults).
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

MNEMONIC_PASSPHRASE_EFFECTIVE=""
if [[ "${IGRA_MNEMONIC_PASSPHRASE_KASPA_EMPTY}" == "1" ]]; then
  IGRA_MNEMONIC_PASSPHRASE_KASPA_IS_SET=1
  MNEMONIC_PASSPHRASE_EFFECTIVE=""
elif [[ "${IGRA_MNEMONIC_PASSPHRASE_KASPA_IS_SET}" == "1" ]]; then
  MNEMONIC_PASSPHRASE_EFFECTIVE="${IGRA_MNEMONIC_PASSPHRASE_KASPA}"
elif [[ -n "${IGRA_MNEMONIC_KASPA}" && "${IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC}" == "1" ]]; then
  MNEMONIC_PASSPHRASE_EFFECTIVE="${IGRA_MNEMONIC_KASPA}"
fi

# Multi-worker Kaspa keys (recommended): comma-separated list matching IGRA_STRESS_WORKERS.
IGRA_STRESS_KASPA_PRIVATE_KEYS="${IGRA_STRESS_KASPA_PRIVATE_KEYS:-}"
ALLOW_SHARED_KASPA_KEY="${ALLOW_SHARED_KASPA_KEY:-0}"

# Target tx (defaults match scripts/igra/testnet-smoke.sh).
STRESS_CONTRACT="${STRESS_CONTRACT:-0xB981f5B62d94285976C9Bdaea65193BBe906972E}"
STRESS_SIG="${STRESS_SIG:-mint(address,uint256)}"
STRESS_TO="${STRESS_TO:-}"
STRESS_AMOUNT="${STRESS_AMOUNT:-1}"
CAST_LEGACY="${CAST_LEGACY:-0}"

py_ms() {
  python3 -c 'import time; print(int(time.time_ns()//1_000_000))'
}

split_csv() {
  local input="${1:-}"
  local out_var="$2"
  local out_ref=()
  if [[ -z "${input}" ]]; then
    eval "${out_var}=()"
    return 0
  fi
  # No fancy CSV; we expect comma-separated tokens.
  local IFS=,
  read -r -a out_ref <<< "${input}"
  eval "${out_var}=(\"\${out_ref[@]}\")"
}

echo "[igra-stress] checking EL chain id via ${IGRA_EL_RPC_URL}"
CHAIN_ID="$("${CAST_BIN}" chain-id --rpc-url "${IGRA_EL_RPC_URL}")"
echo "[igra-stress] EL chain id: ${CHAIN_ID}"

if [[ -z "${STRESS_TO}" ]]; then
  if [[ -n "${IGRA_PRIVATE_KEY}" ]]; then
    STRESS_TO="$("${CAST_BIN}" wallet address --private-key "${IGRA_PRIVATE_KEY}")"
  else
    # We'll compute per-worker when using IGRA_STRESS_EVM_KEYS.
    STRESS_TO=""
  fi
fi

EVM_KEYS=()
split_csv "${IGRA_STRESS_EVM_KEYS}" EVM_KEYS
if [[ "${IGRA_STRESS_WORKERS}" -le 1 ]]; then
  if [[ -z "${IGRA_PRIVATE_KEY}" && "${#EVM_KEYS[@]}" -eq 0 ]]; then
    echo "[igra-stress] missing EVM signer: set IGRA_PRIVATE_KEY (or IGRA_STRESS_EVM_KEYS)"
    exit 1
  fi
  if [[ "${#EVM_KEYS[@]}" -eq 0 ]]; then
    EVM_KEYS=("${IGRA_PRIVATE_KEY}")
  fi
else
  if [[ "${#EVM_KEYS[@]}" -lt "${IGRA_STRESS_WORKERS}" ]]; then
    echo "[igra-stress] IGRA_STRESS_WORKERS=${IGRA_STRESS_WORKERS} requires IGRA_STRESS_EVM_KEYS with >= that many keys"
    echo "[igra-stress] hint: IGRA_STRESS_EVM_KEYS=0x...,0x...,0x..."
    exit 1
  fi
fi

KASPA_KEYS=()
split_csv "${IGRA_STRESS_KASPA_PRIVATE_KEYS}" KASPA_KEYS
if [[ "${IGRA_STRESS_WORKERS}" -gt 1 ]]; then
  if [[ "${#KASPA_KEYS[@]}" -lt "${IGRA_STRESS_WORKERS}" && "${ALLOW_SHARED_KASPA_KEY}" != "1" ]]; then
    echo "[igra-stress] multi-worker run without per-worker Kaspa keys is likely to fail due to UTXO contention."
    echo "[igra-stress] set IGRA_STRESS_KASPA_PRIVATE_KEYS with >= IGRA_STRESS_WORKERS keys, or set ALLOW_SHARED_KASPA_KEY=1 to proceed anyway."
    exit 1
  fi
fi

TMP_DIR="$(mktemp -d "/tmp/igra-testnet-stress.XXXXXX")"
cleanup() {
  if [[ -d "${TMP_DIR}" && "${KEEP_TMP:-0}" != "1" ]]; then
    rm -rf "${TMP_DIR}"
  fi
}
trap cleanup EXIT

echo "[igra-stress] tmp dir: ${TMP_DIR}"
echo "[igra-stress] workers=${IGRA_STRESS_WORKERS} txs_per_worker=${IGRA_STRESS_TXS_PER_WORKER}"

run_worker() {
  local idx="$1"
  local evm_key="$2"
  local kaspa_key="${3:-}"

  local worker_dir="${TMP_DIR}/w${idx}"
  local home_dir="${worker_dir}/home"
  local work_dir="${worker_dir}/work"
  local log_dir="${worker_dir}/logs"
  mkdir -p "${home_dir}" "${work_dir}" "${log_dir}"

  local worker_to="${STRESS_TO}"
  if [[ -z "${worker_to}" ]]; then
    worker_to="$("${CAST_BIN}" wallet address --private-key "${evm_key}")"
  fi

  cat > "${work_dir}/foundry.toml" <<EOF
[profile.default]
eth_rpc_url = "${IGRA_EL_RPC_URL}"

[igra]
enabled = true
el_rpc_url = "${IGRA_EL_RPC_URL}"
kaspa_rpc_url = "${IGRA_KASPA_RPC_URL}"
expected_el_chain_id = ${CHAIN_ID}
kaspa_network = "${IGRA_KASPA_NETWORK}"
tx_id_prefix = "${IGRA_TX_ID_PREFIX}"
el_receipt_timeout_secs = ${IGRA_EL_RECEIPT_TIMEOUT_SECS}
mining_timeout_secs = ${IGRA_MINING_TIMEOUT_SECS}
sender_lock_timeout_secs = ${IGRA_SENDER_LOCK_TIMEOUT_SECS}
payload_compression = "${IGRA_PAYLOAD_COMPRESSION}"
EOF

  # Prefer per-worker Kaspa private keys when provided.
  if [[ -n "${kaspa_key}" ]]; then
    {
      echo ""
      echo "[igra.kaspa_wallet]"
      echo "private_key = \"${kaspa_key}\""
    } >> "${work_dir}/foundry.toml"
  else
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
      } >> "${work_dir}/foundry.toml"
    fi
  fi

  local ok=0
  local fail=0
  local start_ms
  start_ms="$(py_ms)"

  local i=1
  while [[ "${i}" -le "${IGRA_STRESS_TXS_PER_WORKER}" ]]; do
    local t0
    t0="$(py_ms)"

    local -a send_args=(
      send "${STRESS_CONTRACT}" "${STRESS_SIG}" "${worker_to}" "${STRESS_AMOUNT}"
      --rpc-url "${IGRA_EL_RPC_URL}"
      --private-key "${evm_key}"
      --async
    )
    if [[ "${CAST_LEGACY}" == "1" ]]; then
      send_args+=(--legacy)
    fi
    if [[ -n "${kaspa_key}" ]]; then
      send_args+=(--private-key-kaspa "${kaspa_key}")
    fi

    if (cd "${work_dir}" && HOME="${home_dir}" "${CAST_BIN}" "${send_args[@]}") \
      > "${log_dir}/send-${i}.out" 2> "${log_dir}/send-${i}.err"
    then
      ok=$((ok + 1))
    else
      fail=$((fail + 1))
    fi

    local t1
    t1="$(py_ms)"
    if [[ "${IGRA_STRESS_PROGRESS_EVERY}" -gt 0 && $((i % IGRA_STRESS_PROGRESS_EVERY)) -eq 0 ]]; then
      echo "[igra-stress][w${idx}] progress i=${i}/${IGRA_STRESS_TXS_PER_WORKER} ok=${ok} fail=${fail} last_ms=$((t1 - t0))"
    fi

    i=$((i + 1))
  done

  local end_ms
  end_ms="$(py_ms)"
  local elapsed_ms=$((end_ms - start_ms))
  local tps="0"
  if [[ "${elapsed_ms}" -gt 0 ]]; then
    tps="$(python3 -c "print(round(${ok} / (${elapsed_ms} / 1000.0), 4))")"
  fi

  jq -n \
    --arg worker "w${idx}" \
    --arg evm_sender "$("${CAST_BIN}" wallet address --private-key "${evm_key}")" \
    --arg kaspa_key_used "${kaspa_key}" \
    --arg contract "${STRESS_CONTRACT}" \
    --arg sig "${STRESS_SIG}" \
    --arg to "${worker_to}" \
    --arg amount "${STRESS_AMOUNT}" \
    --arg el_rpc "${IGRA_EL_RPC_URL}" \
    --arg kaspa_rpc "${IGRA_KASPA_RPC_URL}" \
    --arg kaspa_network "${IGRA_KASPA_NETWORK}" \
    --arg prefix "${IGRA_TX_ID_PREFIX}" \
    --argjson ok "${ok}" \
    --argjson fail "${fail}" \
    --argjson elapsed_ms "${elapsed_ms}" \
    --argjson tps "${tps}" \
    '{worker: $worker, evm_sender: $evm_sender, kaspa_private_key: (if ($kaspa_key_used|length) > 0 then $kaspa_key_used else null end), target: {contract: $contract, sig: $sig, to: $to, amount: $amount}, endpoints: {el_rpc_url: $el_rpc, kaspa_rpc_url: $kaspa_rpc, kaspa_network: $kaspa_network, tx_id_prefix: $prefix}, results: {ok: $ok, fail: $fail, elapsed_ms: $elapsed_ms, ok_tps: $tps}}' \
    > "${worker_dir}/summary.json"
}

pids=()
worker=1
while [[ "${worker}" -le "${IGRA_STRESS_WORKERS}" ]]; do
  evm_key="${EVM_KEYS[$((worker - 1))]}"
  kaspa_key=""
  if [[ "${#KASPA_KEYS[@]}" -ge "${worker}" ]]; then
    kaspa_key="${KASPA_KEYS[$((worker - 1))]}"
  elif [[ "${ALLOW_SHARED_KASPA_KEY}" == "1" ]]; then
    kaspa_key="${KASPA_KEYS[0]:-}"
  fi

  echo "[igra-stress] starting worker ${worker}"
  run_worker "${worker}" "${evm_key}" "${kaspa_key}" &
  pids+=("$!")
  worker=$((worker + 1))
done

exit_code=0
for pid in "${pids[@]}"; do
  if ! wait "${pid}"; then
    exit_code=1
  fi
done

# Aggregate results.
if ls "${TMP_DIR}"/w*/summary.json >/dev/null 2>&1; then
  jq -s '{workers: ., totals: {ok: (map(.results.ok // 0) | add), fail: (map(.results.fail // 0) | add), elapsed_ms: (map(.results.elapsed_ms // 0) | max), ok_tps: (if ((map(.results.elapsed_ms // 0) | max) > 0) then ((map(.results.ok // 0) | add) / ((map(.results.elapsed_ms // 0) | max) / 1000.0)) else 0 end)}}' \
    "${TMP_DIR}"/w*/summary.json \
    | tee "${TMP_DIR}/summary.json" >/dev/null
  echo "[igra-stress] summary written: ${TMP_DIR}/summary.json"
  echo "[igra-stress] logs under: ${TMP_DIR}/w*/logs/"
fi

exit "${exit_code}"
