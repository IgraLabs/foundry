#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}"
export CARGO_BUILD_TARGET_DIR="${CARGO_BUILD_TARGET_DIR:-$CARGO_TARGET_DIR}"
if [[ "${IGRA_USE_RUSTC_WRAPPER:-0}" != "1" ]]; then
  unset RUSTC_WRAPPER
fi
if [[ "${IGRA_OFFLINE:-0}" == "1" ]]; then
  export DOCS_RS=1
fi

echo "[igra-harness] running deterministic IGRA suite"

cargo test -p foundry-config igra -- --nocapture
cargo test -p foundry-common provider::tests -- --nocapture
cargo test -p foundry-common igra_store -- --nocapture
cargo test -p foundry-common igra_transport -- --nocapture
cargo test -p forge-script guardrails_ -- --nocapture
cargo test -p cast guardrails_ -- --nocapture
cargo test -p forge guardrails_ -- --nocapture

if [[ "${IGRA_ENABLE_NETWORK_SCENARIOS:-0}" == "1" ]]; then
  if [[ -z "${IGRA_EL_RPC_URL:-}" || -z "${IGRA_KASPA_RPC_URL:-}" ]]; then
    echo "[igra-harness] IGRA_ENABLE_NETWORK_SCENARIOS=1 requires IGRA_EL_RPC_URL and IGRA_KASPA_RPC_URL"
    exit 1
  fi

  echo "[igra-harness] network scenarios enabled (current suite uses deterministic unit/integration checks only)"
  echo "[igra-harness] endpoints: EL=${IGRA_EL_RPC_URL} KASPA=${IGRA_KASPA_RPC_URL}"
fi

echo "[igra-harness] deterministic suite complete"
