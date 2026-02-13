#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Avoid inheriting an unwritable global CARGO_TARGET_DIR (e.g. external volume paths).
export CARGO_TARGET_DIR="${ROOT_DIR}/target"
export RUSTC_WRAPPER=

MNEMONIC="${MNEMONIC:-test test test test test test test test test test test junk}"
# IMPORTANT: this is a BIP39 passphrase (kaspa-cli "recovery passphrase"), not a wallet password.
MNEMONIC_PASSPHRASE="${MNEMONIC_PASSPHRASE:-${MNEMONIC}}"
NETWORK="${NETWORK:-testnet-10}"
DERIVATION_PATH="${DERIVATION_PATH:-}"
INDEX="${INDEX:-0}"
EXPECTED_ADDRESS="${EXPECTED_ADDRESS:-kaspatest:qzf364tlnl7ja0w65ydu0m5l70pur2hcm3l3ahkmhs660zcyf7cvuf6uznufr}"

ARGS=(
  --mnemonic "${MNEMONIC}"
  --mnemonic-passphrase "${MNEMONIC_PASSPHRASE}"
  --network "${NETWORK}"
  --index "${INDEX}"
  --expected "${EXPECTED_ADDRESS}"
)
if [[ -n "${DERIVATION_PATH}" ]]; then
  ARGS+=(--derivation-path "${DERIVATION_PATH}")
fi

cd "${ROOT_DIR}"
cargo run -p igra-kaspa-derive --bin igra-kaspa-derive -- "${ARGS[@]}"
