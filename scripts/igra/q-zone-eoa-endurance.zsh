# Falcon-L5 q-zone EOA endurance runner.
#
# Default run target:
#   env zsh scripts/q-zone-eoa-endurance.zsh
#
# Useful short smoke:
#   IGRA_Q_DURATION_SECONDS=60 IGRA_Q_TARGET_TPS=2 env zsh scripts/q-zone-eoa-endurance.zsh

set -e
set +x
unsetopt xtrace verbose 2>/dev/null || true

ROOT="${IGRA_Q_ROOT:-/Users/user/Source/igra/quantum-logic-zone}"
CAST="${IGRA_Q_CAST:-$ROOT/foundry/target/debug/cast}"
RPC="${IGRA_Q_RPC:-http://127.0.0.1:49545}"
KASPA_RPC="${IGRA_Q_KASPA_RPC:-grpc://stage-roman.igralabs.com:56210}"
CHAIN_ID="${IGRA_Q_CHAIN_ID:-48836}"
GAS_PRICE="${IGRA_Q_GAS_PRICE:-1}"
KASPA_NETWORK="${IGRA_Q_KASPA_NETWORK:-testnet-10}"
TX_ID_PREFIX="${IGRA_Q_TX_ID_PREFIX:-97b4}"
LANE_ID="${IGRA_Q_LANE_ID:-97b10000}"
KASPA_MNEMONIC="${IGRA_Q_KASPA_MNEMONIC:-abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about}"
MASTER_SEED="${IGRA_Q_MASTER_SEED:-696772612d712d6c6f6769632d7a6f6e652d66616c636f6e2d6c352d73746167696e672d736d6f6b652d7631}"

DURATION_SECONDS="${IGRA_Q_DURATION_SECONDS:-3600}"
TARGET_TPS="${IGRA_Q_TARGET_TPS:-2}"
NUM_SENDERS="${IGRA_Q_NUM_SENDERS:-24}"
VALUE_WEI="${IGRA_Q_VALUE_WEI:-1}"
FUND_SENDERS="${IGRA_Q_FUND_SENDERS:-true}"
FUND_AMOUNT_WEI="${IGRA_Q_FUND_AMOUNT_WEI:-1000000000000}"
MIN_Q_BALANCE_WEI="${IGRA_Q_MIN_Q_BALANCE_WEI:-100000000000}"
HEALTH_INTERVAL_SECONDS="${IGRA_Q_HEALTH_INTERVAL_SECONDS:-30}"
RECEIPT_RECOVERY_TIMEOUT_SECONDS="${IGRA_Q_RECEIPT_RECOVERY_TIMEOUT_SECONDS:-90}"
SENDER_SEED_PREFIX="${IGRA_Q_SENDER_SEED_PREFIX:-igra-q-endurance-eoa}"
RUN_ID="${IGRA_Q_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
RUN_DIR="${IGRA_Q_ENDURANCE_DIR:-/tmp/igra-q-eoa-endurance-$RUN_ID}"
CONFIG="$RUN_DIR/foundry-igra.toml"
RESULTS="$RUN_DIR/tx-results.jsonl"
HEALTH="$RUN_DIR/health.jsonl"
SUMMARY="$RUN_DIR/summary.txt"
STOP_FILE="$RUN_DIR/.stop"
CONSOLE="$RUN_DIR/console.log"
CONSOLE_TO_FILE="${IGRA_Q_CONSOLE_TO_FILE:-false}"

mkdir -p "$RUN_DIR"
: > "$RESULTS"
: > "$HEALTH"

if [ "$CONSOLE_TO_FILE" = "true" ]; then
  printf 'run_dir=%s\n' "$RUN_DIR"
  printf 'console_log=%s\n' "$CONSOLE"
  exec > "$CONSOLE" 2>&1
fi

cat > "$CONFIG" <<EOF
[profile.default]
eth_rpc_url = "$RPC"

[igra]
enabled = true
el_rpc_url = "$RPC"
kaspa_rpc_url = "$KASPA_RPC"
expected_el_chain_id = $CHAIN_ID
kaspa_network = "$KASPA_NETWORK"
tx_id_prefix = "$TX_ID_PREFIX"
lane_id = "$LANE_ID"
el_receipt_timeout_secs = 180
mining_timeout_secs = 120
sender_lock_timeout_secs = 180
payload_compression = "none"
logic_zone = "falcon-l5"
store_db_path = "$RUN_DIR/igra-store.sqlite"

[igra.kaspa_wallet]
mnemonic = "$KASPA_MNEMONIC"
EOF

json_rpc() {
  local method="$1"
  local params="$2"
  curl -sS -m 10 -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}" \
    "$RPC"
}

now_ms() {
  perl -MTime::HiRes=time -e 'printf "%d\n", time() * 1000'
}

sleep_ms() {
  local ms="$1"
  if [ "$ms" -gt 0 ]; then
    sleep "$(awk -v ms="$ms" 'BEGIN { printf "%.3f", ms / 1000 }')"
  fi
}

hex_to_dec() {
  local value="${1#0x}"
  if [ -z "$value" ]; then
    printf '0'
  else
    printf '%d' "$((16#$value))"
  fi
}

q_key_json_from_seed() {
  "$CAST" --json igra-q-keygen --seed-hex "$1"
}

q_nonce_hex() {
  local address="$1"
  json_rpc eth_getTransactionCount "[\"$address\",\"latest\"]" | jq -r .result
}

q_balance_hex() {
  local address="$1"
  json_rpc eth_getBalance "[\"$address\",\"latest\"]" | jq -r .result
}

q_tx_hash() {
  local raw="$1"
  "$CAST" keccak "$raw"
}

q_receipt_by_hash() {
  local tx_hash="$1"
  json_rpc eth_getTransactionReceipt "[\"$tx_hash\"]" | jq -c '.result // empty'
}

receipt_from_publish_output() {
  local output="$1"
  local receipt receipt_status tx_hash recovered_receipt

  receipt="$(printf '%s\n' "$output" | sed -n '/^[[:space:]]*{/p' | jq -c '
    if type == "object" then
      if has("result") then .result else . end
    else
      empty
    end
    | select(.transactionHash? != null)
  ' 2>/dev/null || true)"

  if [ -z "$receipt" ] || [ "$receipt" = "null" ]; then
    return 1
  fi

  receipt_status="$(jq -r '.status // empty' <<<"$receipt")"
  if [ "$receipt_status" = "0x1" ]; then
    printf '%s\n' "$receipt"
    return 0
  fi

  tx_hash="$(jq -r '.transactionHash // empty' <<<"$receipt")"
  if [ -n "$tx_hash" ] && recovered_receipt="$(wait_for_q_receipt "$tx_hash")"; then
    printf '%s\n' "$recovered_receipt"
    return 0
  fi

  return 1
}

wait_for_q_receipt() {
  local tx_hash="$1"
  local deadline=$(( $(date +%s) + RECEIPT_RECOVERY_TIMEOUT_SECONDS ))
  local receipt

  while [ "$(date +%s)" -lt "$deadline" ]; do
    receipt="$(q_receipt_by_hash "$tx_hash" 2>/dev/null || true)"
    if [ -n "$receipt" ] && [ "$receipt" != "null" ]; then
      printf '%s\n' "$receipt"
      return 0
    fi
    sleep 1
  done

  return 1
}

q_publish_with_config() {
  local private_key="$1"
  local nonce="$2"
  local gas_limit="$3"
  local config_path="$4"
  local work_dir="$5"
  shift 5

  local raw
  raw=$("$CAST" igra-q-mktx \
    --private-key-q "$private_key" \
    --chain-id "$CHAIN_ID" \
    --nonce "$nonce" \
    --max-fee-per-gas "$GAS_PRICE" \
      --gas-limit "$gas_limit" \
      "$@")

  local tx_hash publish_output rc recovered_receipt
  tx_hash="$(q_tx_hash "$raw")"

  if publish_output=$(cd "$work_dir" && FOUNDRY_CONFIG="$config_path" "$CAST" publish "$raw" 2>&1); then
    if recovered_receipt="$(receipt_from_publish_output "$publish_output")"; then
      printf '%s\n' "$recovered_receipt"
    else
      printf '%s\n' "$publish_output"
    fi
    return 0
  else
    rc=$?
  fi

  if recovered_receipt="$(receipt_from_publish_output "$publish_output")"; then
    printf '%s\n' "$recovered_receipt"
    return 0
  fi

  if recovered_receipt="$(wait_for_q_receipt "$tx_hash")"; then
    printf '%s\n' "$recovered_receipt"
    return 0
  fi

  case "$publish_output" in
    *"server returned a null response"*|*"null response"*)
      if recovered_receipt="$(wait_for_q_receipt "$tx_hash")"; then
        printf '%s\n' "$recovered_receipt"
        return 0
      fi
      ;;
  esac

  printf '%s\n' "$publish_output"
  return "$rc"
}

q_publish() {
  local private_key="$1"
  local nonce="$2"
  local gas_limit="$3"
  shift 3

  q_publish_with_config "$private_key" "$nonce" "$gas_limit" "$CONFIG" "$RUN_DIR" "$@"
}

emit_result() {
  local worker="$1"
  local nonce="$2"
  local started_ms="$3"
  local finished_ms="$4"
  local ok="$5"
  local tx_status="$6"
  local tx_hash="$7"
  local block="$8"
  local gas="$9"
  local error="${10}"

  jq -cn \
    --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --arg worker "$worker" \
    --arg nonce "$nonce" \
    --arg started_ms "$started_ms" \
    --arg finished_ms "$finished_ms" \
    --arg ok "$ok" \
    --arg status "$tx_status" \
    --arg tx_hash "$tx_hash" \
    --arg block "$block" \
    --arg gas "$gas" \
    --arg error "$error" \
    '{
      ts: $ts,
      worker: ($worker | tonumber),
      nonce: ($nonce | tonumber),
      started_ms: ($started_ms | tonumber),
      finished_ms: ($finished_ms | tonumber),
      latency_ms: (($finished_ms | tonumber) - ($started_ms | tonumber)),
      ok: ($ok == "true"),
      status: $status,
      tx_hash: $tx_hash,
      block: $block,
      gas: $gas,
      error: $error
    }' >> "$RESULTS"
}

health_loop() {
  while [ ! -f "$STOP_FILE" ]; do
    local ts block syncing peer_count pending latest
    ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    block="$(json_rpc eth_blockNumber "[]" | jq -r '.result // "null"' 2>/dev/null || printf 'error')"
    syncing="$(json_rpc eth_syncing "[]" | jq -r '.result // "null"' 2>/dev/null || printf 'error')"
    peer_count="$(json_rpc net_peerCount "[]" | jq -r '.result // "null"' 2>/dev/null || printf 'error')"
    pending="$(json_rpc txpool_status "[]" | jq -r '.result.pending // "null"' 2>/dev/null || printf 'null')"
    latest="$(wc -l < "$RESULTS" | tr -d ' ')"
    jq -cn \
      --arg ts "$ts" \
      --arg block "$block" \
      --arg syncing "$syncing" \
      --arg peer_count "$peer_count" \
      --arg pending "$pending" \
      --arg submitted_results "$latest" \
      '{ts:$ts, block:$block, syncing:$syncing, peer_count:$peer_count, pending:$pending, submitted_results:($submitted_results|tonumber)}' \
      >> "$HEALTH"
    sleep "$HEALTH_INTERVAL_SECONDS"
  done
}

print_receipt_line() {
  local label="$1"
  local receipt="$2"
  printf '%s status=%s tx=%s block=%s gas=%s\n' \
    "$label" \
    "$(jq -r .status <<<"$receipt")" \
    "$(jq -r .transactionHash <<<"$receipt")" \
    "$(jq -r .blockNumber <<<"$receipt")" \
    "$(jq -r .gasUsed <<<"$receipt")"
}

typeset -a sender_pks
typeset -a sender_addrs
typeset -a sender_nonces

printf 'run_dir=%s\n' "$RUN_DIR"
printf 'target_tps=%s duration_seconds=%s num_senders=%s\n' "$TARGET_TPS" "$DURATION_SECONDS" "$NUM_SENDERS"

for i in $(seq 0 $((NUM_SENDERS - 1))); do
  seed_hex=$(printf '%s-%02d-20260606' "$SENDER_SEED_PREFIX" "$i" | xxd -p -c 256)
  key_json=$(q_key_json_from_seed "$seed_hex")
  sender_pks+=("$(jq -r .private_key <<<"$key_json")")
  sender_addrs+=("$(jq -r .address <<<"$key_json")")
done

if [ "$FUND_SENDERS" = "true" ]; then
  master_json=$(q_key_json_from_seed "$MASTER_SEED")
  master_pk=$(jq -r .private_key <<<"$master_json")
  master_addr=$(jq -r .address <<<"$master_json")
  master_nonce_hex=$(q_nonce_hex "$master_addr")
  master_nonce=$((16#${master_nonce_hex#0x}))
  master_balance_hex="$(q_balance_hex "$master_addr")"
  master_balance_dec="$(hex_to_dec "$master_balance_hex")"
  required_fund_balance=$((FUND_AMOUNT_WEI * NUM_SENDERS))
  printf 'funding_master=%s start_nonce=%s balance=%s\n' "$master_addr" "$master_nonce" "$master_balance_hex"
  if [ "$master_balance_dec" -lt "$required_fund_balance" ]; then
    printf 'funding master has insufficient q balance: address=%s balance=%s required_at_most=%s\n' \
      "$master_addr" "$master_balance_hex" "$required_fund_balance" >&2
    exit 1
  fi

  for i in $(seq 1 "$NUM_SENDERS"); do
    addr="${sender_addrs[$i]}"
    balance_hex="$(q_balance_hex "$addr")"
    balance_dec="$(hex_to_dec "$balance_hex")"
    printf 'sender_preflight index=%s address=%s balance=%s\n' "$((i - 1))" "$addr" "$balance_hex"
    if [ "$balance_dec" -lt "$MIN_Q_BALANCE_WEI" ]; then
      receipt=$(q_publish "$master_pk" "$master_nonce" 21000 --to "$addr" --value "$FUND_AMOUNT_WEI")
      print_receipt_line "fund_sender_$((i - 1))" "$receipt"
      receipt_status="$(jq -r .status <<<"$receipt")"
      if [ "$receipt_status" != "0x1" ]; then
        printf 'funding failed for sender index=%s address=%s\n' "$((i - 1))" "$addr" >&2
        exit 1
      fi
      master_nonce=$((master_nonce + 1))
    fi
  done
fi

for i in $(seq 1 "$NUM_SENDERS"); do
  nonce_hex="$(q_nonce_hex "${sender_addrs[$i]}")"
  sender_nonces+=("$((16#${nonce_hex#0x}))")
  printf 'sender_ready index=%s address=%s start_nonce=%s balance=%s\n' \
    "$((i - 1))" "${sender_addrs[$i]}" "${sender_nonces[$i]}" "$(q_balance_hex "${sender_addrs[$i]}")"
done

period_ms="$(awk -v n="$NUM_SENDERS" -v tps="$TARGET_TPS" 'BEGIN { printf "%d", (n / tps) * 1000 }')"
interval_ms="$(awk -v tps="$TARGET_TPS" 'BEGIN { printf "%d", (1 / tps) * 1000 }')"
end_epoch=$(( $(date +%s) + DURATION_SECONDS ))

trap 'touch "$STOP_FILE"; kill $(jobs -p) 2>/dev/null || true' INT TERM

health_loop &
health_pid=$!

worker_loop() {
  local worker_index="$1"
  local private_key="$2"
  local nonce="$3"
  local delay_ms="$4"
  local address="$5"
  local worker_dir="$RUN_DIR/worker-$worker_index"
  local worker_config="$worker_dir/foundry-igra.toml"

  mkdir -p "$worker_dir"
  awk -v path="$worker_dir/igra-store.sqlite" '
    /^store_db_path = / {
      print "store_db_path = \"" path "\""
      next
    }
    { print }
  ' "$CONFIG" > "$worker_config"

  sleep_ms "$delay_ms"
  printf 'worker_start index=%s address=%s nonce=%s delay_ms=%s period_ms=%s\n' \
    "$worker_index" "$address" "$nonce" "$delay_ms" "$period_ms"

  while [ "$(date +%s)" -lt "$end_epoch" ] && [ ! -f "$STOP_FILE" ]; do
    local started_ms finished_ms receipt tx_status tx_hash block gas sink error
    started_ms="$(now_ms)"
    sink=$(printf '0x%040x' $((0x400000 + worker_index * 100000 + nonce)))

    set +e
    receipt=$(q_publish_with_config \
      "$private_key" \
      "$nonce" \
      21000 \
      "$worker_config" \
      "$worker_dir" \
      --to "$sink" \
      --value "$VALUE_WEI" \
      2>&1)
    rc=$?
    set -e

    finished_ms="$(now_ms)"
    if [ "$rc" -eq 0 ] && jq -e . >/dev/null 2>&1 <<<"$receipt"; then
      tx_status="$(jq -r '.status // "null"' <<<"$receipt")"
      tx_hash="$(jq -r '.transactionHash // "null"' <<<"$receipt")"
      block="$(jq -r '.blockNumber // "null"' <<<"$receipt")"
      gas="$(jq -r '.gasUsed // "null"' <<<"$receipt")"
      emit_result "$worker_index" "$nonce" "$started_ms" "$finished_ms" true "$tx_status" "$tx_hash" "$block" "$gas" ""
      nonce=$((nonce + 1))
    else
      error="$(printf '%s' "$receipt" | tail -c 800)"
      emit_result "$worker_index" "$nonce" "$started_ms" "$finished_ms" false "" "" "" "" "$error"
      printf 'worker_error index=%s nonce=%s error=%s\n' "$worker_index" "$nonce" "$error" >&2
      break
    fi

    local elapsed_ms sleep_for_ms
    elapsed_ms=$((finished_ms - started_ms))
    sleep_for_ms=$((period_ms - elapsed_ms))
    sleep_ms "$sleep_for_ms"
  done

  printf 'worker_done index=%s final_nonce=%s\n' "$worker_index" "$nonce"
}

typeset -a worker_pids
for i in $(seq 1 "$NUM_SENDERS"); do
  delay_ms=$(( (i - 1) * interval_ms ))
  worker_loop "$((i - 1))" "${sender_pks[$i]}" "${sender_nonces[$i]}" "$delay_ms" "${sender_addrs[$i]}" &
  worker_pids+=("$!")
done

for pid in "${worker_pids[@]}"; do
  wait "$pid"
done
touch "$STOP_FILE"
wait "$health_pid" 2>/dev/null || true

success_count="$(jq -s 'map(select(.ok == true and .status == "0x1")) | length' "$RESULTS")"
failure_count="$(jq -s 'map(select(.ok != true or .status != "0x1")) | length' "$RESULTS")"
total_count="$(wc -l < "$RESULTS" | tr -d ' ')"
first_ms="$(jq -s 'map(.started_ms) | min // 0' "$RESULTS")"
last_ms="$(jq -s 'map(.finished_ms) | max // 0' "$RESULTS")"
elapsed_ms=$((last_ms - first_ms))
observed_tps="$(awk -v ok="$success_count" -v ms="$elapsed_ms" 'BEGIN { if (ms > 0) printf "%.4f", ok / (ms / 1000); else printf "0" }')"

{
  printf 'run_dir=%s\n' "$RUN_DIR"
  printf 'target_tps=%s\n' "$TARGET_TPS"
  printf 'duration_seconds=%s\n' "$DURATION_SECONDS"
  printf 'num_senders=%s\n' "$NUM_SENDERS"
  printf 'total_results=%s\n' "$total_count"
  printf 'success_count=%s\n' "$success_count"
  printf 'failure_count=%s\n' "$failure_count"
  printf 'elapsed_ms=%s\n' "$elapsed_ms"
  printf 'observed_success_tps=%s\n' "$observed_tps"
  printf 'results=%s\n' "$RESULTS"
  printf 'health=%s\n' "$HEALTH"
} > "$SUMMARY"

cat "$SUMMARY"
