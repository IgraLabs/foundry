# IGRA Transaction Protocol

This document defines the requirements and interpretation rules for [L1-submitted Transactions](#11-l1-submitted-transactions) of the IGRA protocol — transactions posted to the KASPA base layer (L1) and consumed by the IGRA execution layer (L2).

> IGRA protocol may also define additional system-level transaction classes in the future, created directly on L2 without being posted to L1. These are mentioned here only for context and are **out of scope** for this document.

---

## Document Version History

| Version | Date       | Author     | Description                                                                                 |
|---------|------------|------------|---------------------------------------------------------------------------------------------|
| v0.0    | 2025-03-23 | IGRA Team  | Initial draft with support for 1-to-1 Payload transactions (zipped and unzipped).           |
| v0.1    | 2025-06-27 | IGRA Team  | Refactored following the introduction of Tx ID mining.                                      |
| v0.2    | 2025-11-26 | IGRA Team  | Removed L2 Start and Synthetic Transactions; updated supported types, payload semantics, and numbering. |

---

## 1. Transaction Classes in IGRA

IGRA currently defines a single class of transactions: [L1-submitted Transactions](#11-l1-submitted-transactions).

<a id="11-l1-submitted-transactions"></a>

### 1.1 L1-submitted Transactions

These are regular transactions posted on the KASPA base layer (L1) with a recognizable `Payload`. They are:

- Validated at both the L1 and L2 layers
- Used to carry executable L2 data or instructions
- The main subject of this document

#### Validation Rules

L1-submitted IGRA transactions are subject to **two layers of validation**:

##### L1-level Rules

These rules are based on the L1 chain state and require knowledge of L1 logic. As such, they **cannot** be validated by the IGRA execution layer.

A key requirement, applicable to **all** [L1-submitted Transactions](#11-l1-submitted-transactions), is that the L1 transaction ID must match a predefined pattern (see [IGRA Design Note: L1 Payload Format and Filtering by txID][l1-payload-format]).

Other L1-level validation rules include:

- A transaction must include the L1 `Payload` field starting with [`version`](#31-payload-format)
- A transaction may need to create or spend specific UTXOs (see [Entry transaction](#43-entry) for an example)
- Or, it must use specific locking and unlocking scripts

> Example: The [Entry Transaction](#43-entry) must create the [KAS Locking UTXO](#kas-locking-utxo).

##### L2-level Rules

These rules require knowledge of L2 logic and access to the L2 state.

Examples:
- The [L2 content](#31-payload-format) in the L1 payload must be an RLP encoded and correctly signed EVM transaction
- Metadata (in [metadata-based transactions](#metadata-based-transactions)) must match the current L2 state

---

## 2. L1-submitted Transaction Types

The following L1-submitted transaction types and their corresponding identifiers are defined:

| TxTypeId (4 bits) | Type Name                  | Category               | Status    |
|-------------------|----------------------------|------------------------|-----------|
| b0001             | L2 Param Update            | Metadata               | Reserved  |
| b0010             | Entry                      | Metadata               | Supported |
| b0011             | Exit                       | Metadata               | Supported |
| b0100             | 1-to-1 Unzipped Payload    | Raw L2 Tx              | Supported |
| b0101             | 1-to-1 Zipped Payload      | Raw L2 Tx              | Supported |
| b0110             | 1-to-many Unzipped Payload | Raw L2 Tx (batch)      | Reserved  |
| b0111             | 1-to-many Zipped Payload   | Raw L2 Tx (batch)      | Reserved  |
| b1000 .. b1110    | Reserved                   | —                      | Reserved  |
| b1111             | Service Transactions       | Metadata               | Supported |

## 3. L1 Payload

All L1-submitted IGRA transactions must embed L2-specific data in the `Payload` field of the L1 transaction.

### 3.1 Payload Format

All [L1-submitted Transactions](#2-l1-submitted-transaction-types) must include an L1 payload structured as follows:

```text
[Version (4 bits)] [txTypeId (4 bits)] [L2Data (≤24,800 bytes)] [Nonce (4 bytes)]
```

- `Version`: 4-bit identifier, set to `0x9`
- `txTypeId`: 4-bit field identifying the type of L2 data (see [Transaction Types](#2-l1-submitted-transaction-types))
- `L2Data`: L2-specific data, determined by `txTypeId`
- `Nonce`: 4-byte nonce used for tx ID mining

See the [IGRA Design Note: L1 Payload Format and Filtering by txID][l1-payload-format] for more details.

### 3.2 Payload Semantics

[L1-submitted Transaction types](#2-l1-submitted-transaction-types) fall into two semantic categories:

#### Raw L2 Transactions

Types: `b0100`, `b0101`, `b0110`, `b0111`

- Contain RLP-encoded L2 transaction(s), either compressed or uncompressed
- After processing (decompression and merging), the `L2Data` is sent *as-is* to the EL Client

#### Metadata-based Transactions

Types: `b0001` (not yet supported), `b0010`, `b0011`

- Do **not** include ready-to-execute L2 transactions
- Contain metadata used to **deterministically construct** L2 transactions

---

## 4. Supported Transaction Types

### 4.1 1-to-1 Unzipped Payload

**TxTypeId:** b0100

**Purpose:** Carries a single, uncompressed RLP-encoded L2 transaction.

**L1 Payload:**
```text
[0x94] [L2Data: RLP-encoded EVM transaction] [4-byte Nonce]
```

**Requirements:**

- L1 Tx ID must match a predefined pattern
- `L2Data` MUST contain a single RLP-encoded EVM transaction
- `L2Data` MUST not exceed 24,800 bytes
- No restrictions on UTXO inputs/outputs or scripts

**Interpretation:**

`L2Data` is parsed directly as RLP and then sent to the EL client via `eth_sendRawTransaction`

### 4.2 1-to-1 Zipped Payload

**TxTypeId:** b0101

**Purpose:** Same as [1-to-1 Unzipped Payload](#41-1-to-1-unzipped-payload), but uses ZLIB compression.

**L1 Payload:**
```text
[0x95] [L2Data: ZLIB-compressed RLP-encoded EVM transaction] [4-byte Nonce]
```

**Requirements:**
- L1 Tx ID must match a predefined pattern
- `L2Data` MUST be a ZLIB-compressed RLP-encoded EVM transaction
- Decompressed `L2Data` MUST not exceed 24,800 bytes
- No restrictions on UTXO inputs/outputs or scripts

**Interpretation:**

- Decompress with ZLIB
- Parse as RLP
- Submit to EL client via `eth_sendRawTransaction`

### 4.3 Entry

**TxTypeId:** b0010

**Purpose:** Bridges KAS to L2 by locking KAS and issuing an equivalent amount of iKAS.

See [IGRA Design Doc - Entry Transaction] for more details.

**L1 Payload:**
```text
[0x92] [L2Data (28 bytes): 20-byte recipient address, 8-byte amount] [4-byte Nonce]
```
**Requirements:**
- L1 Tx ID must match a predefined pattern
- The L1 transaction MUST create the [KAS Locking UTXO](#kas-locking-utxo)
- No restrictions on other inputs or outputs

**Interpretation:**

- `address`: highest 20 bytes of `L2Data` — L2 address to mint iKAS to
- `amount`: lowest 8 bytes of `L2Data` - (unsigned int), in 10^-8 KAS ("dwork" or "SOMPI")

The equivalent iKAS amount will be scaled to 10^-18 units and minted to the L2 address as an EVM "withdrawal" per [EIP-4895](https://github.com/ethereum/EIPs/blob/master/EIPS/eip-4895.md).

#### KAS Locking UTXO

- MUST be the first output created by the L1 transaction
- MUST match the amount in the payload (minimum: 1 KAS)
- MUST use the [Entry Locking Script](#entry-locking-script)

##### Entry Locking Script

KAS Locking UTXOs are locked with the script pubkey:
```text
aa205933185b78c71f0833770ca4aa6b62423af00d0efc2832025a23999543f220f787
```
Decoded, that locking script is:
```text
`OP_BLAKE2B`
`PUSH 32 <redeem-script-hash>`
`OP_EQUAL`
```
This is Kaspa script-hash locking: the output commits to the `BLAKE2b` hash of the redeem script, not directly to signer pubkeys.

### 4.4 Exit

**TxTypeId:** b0011

**Purpose:** Unlock KAS to one or more recepients on L1 to react on one or more iKAS burning transactions on L1.

See [IGRA Design Doc - Exit Transaction]() for more details.

**L1 Payload:**
```text
[0x93] [L2Data (N * 32 bytes): MessageId[0], ..., MessageId[N-1] ] [4-byte Nonce]
```
Where,
  - `N` - number of `messageId` values (at least one value MUST present)
  - `messageId` - ID of "exit message"  (defined by [IGRA Tech Specs - Igra Exit Bridge Message Format][exit-message-format])

> Exactly one exit message is dispatched over Igra Exit Bridge for every "exit" transaction on L2. The exit message contains iKAS amount burnt on L2 and the address to unlock the corresponding KAS amount to.
> See [IGRA Design Doc - Exit Transaction]() and [IGRA Tech Specs - Igra Exit Bridge Message Format][exit-message-format](IGRA%20Tech%20Specs%20-%20Igra%20Exit%20Bridge%20Message%20Format.md) for further details.

**Requirements:**
- L1 Tx ID must match a predefined pattern
- The L1 transaction MUST spend [KAS Locking UTXO(s)](#kas-locking-utxo) only
- The L1 transaction MUST create exactly one UTXO for every exit message with `MessageId` specified in the payload.

**Interpretation:**
- L1 transaction of this type MUST NOT trigger L2 transaction(s) or L2 block header mutations.


### 4.5 Service Transactions

**TxTypeId:** b1111

**Purpose:** Group L1 transactions which do NOT change L2 state but SHALL be filtered out by the Tx ID predefined pattern.

**L1 Payload:**
```text
[0x9F] [L2Data (4 .. 68 bytes): mandatory 4-byte TX_SUBTYPE, optional subtype-specific data] [4-byte Nonce]
```
The following `TX_SUBTYPE` defined:
  - `0x52454752` ("REGROUP")
    - ASCII literal for "REGR"
    - Used to group transactions which merge smaller (or split bigger) [KAS Locking UTXOs](#kas-locking-utxo) into a bigger (or smaller) [KAS Locking UTXO(s)](#kas-locking-utxo)
    - No subtype-specific data
    - L1 payload is `0x9f52454752`
  - `0x4d494752` ("MIGRATE")
    - ASCII literal for "MIGR"
    - Used to group transactions which transfer KAS locked into [KAS Locking UTXOs](#kas-locking-utxo) to UTXO(s) owned by newer bridge version
    - No subtype-specific data
    - L1 payload is `0x9f4d494752`

**Requirements:**
- L1 Tx ID must match a predefined pattern

**Interpretation:**
- L1 transaction of this type MUST NOT trigger L2 transaction(s) or L2 block header mutations.

---

## 5. Planned Transaction Types

The following types are defined but not yet supported:

- **L2 Param Update** — signals protocol-level updates
- **1-to-many** — atomic batch of L2 transactions
- **Many-to-1** — fragmented L2 txs over several L1 txs (useful for blob data)

[exit-message-format]: IGRA%20Tech%20Specs%20-%20Igra%20Exit%20Bridge%20Message%20Format.md
