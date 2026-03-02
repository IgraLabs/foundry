# IGRA Developer Guide: Sending EVM Transactions via Kaspa L1

## Overview

IGRA is an EVM execution layer built on top of Kaspa L1. Developers interact with IGRA using standard EVM tooling for **reads**, but transactions (writes) are submitted through Kaspa L1 rather than a traditional mempool.

**Key difference from standard EVM**: There is no `eth_sendRawTransaction` endpoint. Instead, signed EVM transactions are embedded as payload inside Kaspa L1 transactions. The IGRA sequencer picks them up from L1 and executes them on the EVM.

## How It Works

```
┌──────────────┐     ┌───────────────┐     ┌──────────────┐
│  Your App    │     │   Kaspa L1    │     │  IGRA (EVM)  │
│              │     │               │     │              │
│ 1. Build     │     │ 3. TX lands   │     │ 4. Sequencer │
│    signed    │────▶│    on L1      │────▶│    extracts   │
│    EVM TX    │     │    (1 sec)    │     │    payload    │
│              │     │               │     │              │
│ 2. Wrap in   │     │               │     │ 5. Executes  │
│    Kaspa TX  │     │               │     │    EVM TX    │
│    + mine    │     │               │     │    (~3 sec)  │
│    prefix    │     │               │     │              │
└──────────────┘     └───────────────┘     └──────────────┘
        │                                          │
        │         ┌───────────────┐                │
        └────────▶│  EVM RPC      │◀───────────────┘
           reads  │  (standard)   │  state updates
                  └───────────────┘
```

### Transaction Lifecycle

1. **Build & sign** a standard EVM transaction (EIP-1559 or legacy)
2. **Wrap** the raw signed TX bytes into an IGRA payload (1-byte header + TX bytes + 4-byte nonce)
3. **Mine** a Kaspa TX ID prefix — a small proof-of-work so the sequencer recognizes this TX
4. **Submit** the Kaspa TX to L1 via gRPC
5. **L2 confirms** within ~3 seconds. If it doesn't appear, it was dropped (no mempool, no retry)

## What Developers Need

### Configuration

| Parameter | Description | Example (Galleon Testnet) |
|-----------|-------------|---------------------------|
| EVM RPC URL | Standard JSON-RPC for reads | `https://galleon-testnet.igralabs.com:8545` |
| Kaspa gRPC URL | L1 node for TX submission | `grpc://95.217.73.85:16210` |
| Chain ID | EVM chain ID | `38836` |
| TX ID Prefix | Chain ID in hex (lowercase) | `97b4` |
| Kaspa Network | L1 network identifier | `testnet-10` |
| Gas Price | IGRA gas price (sompi-wei) | `2000000000000` (2 TKas) |

**Critical**: The TX ID prefix **must** equal the chain ID in hex. Discover it with:
```bash
# Get chain ID
cast chain-id --rpc-url https://galleon-testnet.igralabs.com:8545
# 38836

# Convert to hex
python3 -c "print(hex(38836))"
# 0x97b4  →  prefix = "97b4"
```

Wrong prefix = transactions silently dropped by L2. They land on Kaspa L1 but IGRA never processes them.

### Funded Kaspa Address

You need a Kaspa address with tKAS to pay L1 fees. Each IGRA transaction costs ~0.0022 KAS (220,000 sompi) in Kaspa fees. This is separate from EVM gas, which is paid in iKAS on L2.

## Reads: Standard EVM RPC (No Changes)

All read operations use standard EVM JSON-RPC. No modifications needed:

```bash
# Check balance
cast balance 0xYourAddress --rpc-url https://galleon-testnet.igralabs.com:8545

# Call a contract
cast call 0xContract "balanceOf(address)" 0xYourAddress --rpc-url ...

# Get transaction receipt
cast receipt 0xTxHash --rpc-url ...

# Get block number
cast block-number --rpc-url ...
```

Any EVM library (ethers.js, viem, web3.py, alloy) works for reads.

## Writes: The IGRA Path

### Payload Format

Every EVM transaction is wrapped in a Kaspa TX payload:

```
┌─────────┬──────────────────────────┬─────────┐
│ Header  │       L2Data             │  Nonce  │
│ 1 byte  │  raw signed EVM TX bytes │ 4 bytes │
└─────────┴──────────────────────────┴─────────┘

Header = (IGRA_VERSION << 4) | TX_TYPE
       = (0x9 << 4) | 0x4
       = 0x94  (uncompressed raw TX)

Nonce = opaque 4-byte value, iterated during prefix mining
Max L2Data size: 24,800 bytes
```

### Prefix Mining

The Kaspa TX hash (transaction ID) must start with the TX ID prefix bytes. This is a small proof-of-work:

1. Build the Kaspa TX with payload containing `[header][evm_tx_bytes][nonce=0]`
2. Hash the TX → check if `tx_id` starts with prefix bytes
3. If not, increment nonce (last 4 bytes of payload), re-hash, repeat
4. Typical iterations: ~65K for 2-byte prefix (~3-8 seconds on modern CPU)
5. After mining, sign the Kaspa TX and verify prefix still holds

### Step-by-Step Write Flow

```
1. SIGN EVM TX
   ├─ Build EIP-1559 TX (chain_id, nonce, gas_limit, max_fee_per_gas, to, value, data)
   ├─ Sign with your EVM private key
   └─ Encode as raw bytes (EIP-2718 envelope)

2. BUILD IGRA PAYLOAD
   ├─ header = 0x94
   ├─ l2data = raw_evm_tx_bytes
   └─ payload = [header] + [l2data] + [0x00000000]  (initial nonce)

3. SELECT KASPA UTXOs
   ├─ Query: kaspa_grpc.get_utxos_by_addresses([your_kaspa_address])
   ├─ Sort by amount DESC (prefer large UTXOs)
   ├─ Accumulate until: total >= fee + dust_threshold
   └─ Fee = 200,000 + ceil(payload_len / 1024) × 20,000 + (inputs-1) × 10,000

4. BUILD KASPA TX
   ├─ Inputs: selected UTXOs
   ├─ Outputs: [change_back_to_self]  (total_input - fee)
   └─ Payload: from step 2

5. MINE PREFIX
   ├─ Loop: mutate last 4 bytes of payload, finalize TX, check tx_id prefix
   ├─ Timeout: 120 seconds
   └─ Expected: ~3-8s for 2-byte prefix

6. SIGN KASPA TX
   ├─ Sign with Kaspa private key (secp256k1)
   ├─ Verify signatures
   ├─ Verify: signed tx_id still starts with prefix
   └─ Calculate mass (must be ≤ 100,000)

7. SUBMIT TO L1
   ├─ kaspa_grpc.submit_transaction(signed_tx)
   ├─ Retry on transient gRPC errors (3 retries, exponential backoff)
   └─ Return kaspa_tx_id

8. CONFIRM ON L2 (optional)
   ├─ Poll: eth_getTransactionReceipt(evm_tx_hash)
   ├─ Should appear within ~3 seconds
   └─ If not there after 3s, TX was dropped (permanently)
```

## L2 Behavior

IGRA has no mempool and no backlog:

- **Confirmation**: ~3 seconds after L1 inclusion
- **Dropped transactions**: If a TX doesn't appear on L2 within 3 seconds of L1 confirmation, it was permanently dropped. No retry mechanism exists at L2 level.
- **Nonce ordering**: Standard EVM nonce rules apply on L2. Gaps cause subsequent TXs to be dropped.
- **Gas price**: Must meet the L2 minimum (currently 2 TKas = 2×10¹² sompi-wei). Query with `eth_gasPrice`.

## Integration Options

### Option A: Foundry (cast/forge) — Works Today

IGRA mode is built into this Foundry fork. Configure in `foundry.toml`:

```toml
[igra]
enabled = true
el_rpc_url = "https://galleon-testnet.igralabs.com:8545"
kaspa_rpc_url = "grpc://95.217.73.85:16210"
kaspa_network = "testnet-10"
tx_id_prefix = "97b4"
expected_el_chain_id = 38836

[igra.kaspa_wallet]
private_key = "0x..."  # Your Kaspa private key (hex)
```

Then use `cast` normally — writes are automatically routed through Kaspa L1:

```bash
# Send iKAS (routed through Kaspa L1 automatically)
cast send 0xRecipient --value 1ether --private-key 0xEVM_KEY --rpc-url ...

# Deploy contract
forge create src/MyContract.sol:MyContract --private-key 0xEVM_KEY --rpc-url ...

# Reads work unchanged
cast call 0xContract "balanceOf(address)" 0xAddr --rpc-url ...
```

The transport layer handles UTXO selection, prefix mining, Kaspa signing, and submission transparently.

### Option B: RPC Proxy (Future)

A standalone HTTP server that accepts standard JSON-RPC:

- **Reads** (`eth_call`, `eth_getBalance`, etc.) → proxied to EVM RPC
- **Writes** (`eth_sendRawTransaction`) → intercepted, wrapped in Kaspa TX, mined, submitted to L1

Developer code stays 100% standard EVM — just change the RPC URL to point at the proxy. The proxy needs a funded Kaspa key for L1 fees.

```
Your App  ──JSON-RPC──▶  IGRA Proxy  ──gRPC──▶  Kaspa L1  ──▶  IGRA L2
                              │
                              └──JSON-RPC──▶  EVM RPC (reads)
```

### Option C: SDK Library (Future)

A lightweight library exposing the core submission function:

```rust
// Pseudocode — Rust
let receipt = igra::send_transaction(
    signed_evm_tx,       // Raw signed EVM TX bytes
    kaspa_private_key,   // For L1 fee payment
    kaspa_grpc_url,      // L1 endpoint
    tx_id_prefix,        // "97b4"
).await?;
```

```javascript
// Pseudocode — JavaScript
const receipt = await igraSend({
  signedTx: '0x02f8...',       // Raw signed EVM TX
  kaspaKey: '0xabc...',        // Kaspa private key
  kaspaGrpc: 'grpc://...',     // L1 endpoint
  txIdPrefix: '97b4',
});
```

## Fee Model

### Kaspa L1 Fees (paid in KAS)

Per-transaction, paid by the Kaspa address that submits the wrapping TX:

| Component | Cost (sompi) | Notes |
|-----------|-------------|-------|
| Base fee | 200,000 | Every TX |
| Payload size | 20,000 per KiB | Typical EVM TX ≈ 120 bytes = 1 KiB |
| Extra inputs | 10,000 per input beyond first | Usually 1 input |
| **Typical total** | **~220,000** | **≈ 0.0022 KAS per TX** |

### EVM L2 Gas (paid in iKAS)

Standard EVM gas rules. The sender's EVM address must have iKAS balance for gas:

- Gas price: ~2 TKas (2×10¹² wei) — query with `eth_gasPrice`
- Simple transfer: 21,000 gas × 2×10¹² = 4.2×10¹⁶ wei ≈ 0.042 iKAS
- Contract calls: standard EVM gas metering

### UTXO Management

Each Kaspa TX consumes UTXOs and produces a change output. A single Kaspa address can submit transactions sequentially (each TX uses the change UTXO from the previous one). For parallel submission, split the balance across multiple Kaspa addresses.

Storage mass constraint: `C × (1/output_value)` where C = 10¹². Outputs smaller than ~0.1 KAS can hit the 100,000 mass limit.

## Constraints & Limits

| Constraint | Value | Notes |
|-----------|-------|-------|
| Max payload size | 24,800 bytes | Max EVM TX size that fits in one Kaspa TX |
| Max TX mass | 100,000 | Storage mass + compute mass combined |
| Min output value | ~0.1 KAS | Below this, storage mass exceeds limits |
| Prefix mining time | ~3-8s (2-byte prefix) | CPU-bound; increases exponentially with prefix length |
| L2 confirmation | ~3 seconds | No retry if dropped |
| gRPC connections | ~30 max concurrent | Single shared connection recommended |
| Supported TX types | Legacy, EIP-1559 | EIP-4844 (blob) and EIP-7702 not supported |

## Networks

### Galleon Testnet

| Endpoint | URL |
|----------|-----|
| EVM RPC | `https://galleon-testnet.igralabs.com:8545` |
| Kaspa gRPC | `grpc://95.217.73.85:16210` |
| Explorer | `https://explorer.galleon-testnet.igralabs.com/` |
| Chain ID | 38836 |
| TX ID Prefix | `97b4` |
| Kaspa Network | `testnet-10` |

### Galleon Mainnet

| Endpoint | URL |
|----------|-----|
| EVM RPC | `https://galleon.igralabs.com:8545` |
| Kaspa gRPC | `grpc://95.217.73.85:16110` |
| Chain ID | TBD |
| TX ID Prefix | Chain ID in hex |
| Kaspa Network | `mainnet` |

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| TX on L1 but not on L2 | Wrong TX ID prefix | Verify prefix = chain ID in hex |
| TX on L1 but not on L2 | EVM nonce gap | Check sender's L2 nonce, fill gaps sequentially |
| `IGRA_MINING_001` timeout | Prefix too long or unlucky | Increase `--mining-timeout-secs` |
| `insufficient Kaspa UTXOs` | Kaspa address needs funding | Send KAS to the address |
| TX mass exceeds 100,000 | Output too small or too many inputs | Consolidate UTXOs or increase output value |
| `ResourceExhausted` from gRPC | Too many gRPC connections | Reuse a single `GrpcClient` (it's `Clone`/`Arc`-based) |
| TX dropped after 3s | L2 rejected (bad nonce, low gas, etc.) | Check gas price ≥ `eth_gasPrice`, correct nonce |
