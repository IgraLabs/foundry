# IGRA Exit Unsigned Transaction Runbook

This document describes how to build, verify, sign, and broadcast an IGRA exit transaction using
the `cast igra` CLI and the official `kaspawallet` signer format.

The current implementation supports IGRA exit transactions with protocol payload header `0x93`.
It intentionally does not implement `0x95`.

## Artifacts

The builder produces two files:

- `unsigned-exit.json`: a manifest for human and machine verification.
- `unsigned-exit.hex`: a hex-encoded official `kaspawallet` `PartiallySignedTransaction`.

Every signer should receive both files and verify them before signing.

## Build Cast

From the Foundry repository:

```bash
git switch roman/igra-exit-lane-id
cargo build -p cast
```

Use the local binary:

```bash
./target/debug/cast igra --help
```

## Multisig Address Helpers

Before building an exit transaction, operators can independently check the path and derive or verify
the canonical multisig address from the same kpubs used by the unsigned builder.

The helper public-keys file should contain only public multisig fields copied from the official
kaspawallet keys JSON:

```json
{
  "publicKeys": [
    "kpub_SIGNER_1_MULTISIG_MASTER_XPUB",
    "kpub_SIGNER_2_MULTISIG_MASTER_XPUB",
    "kpub_SIGNER_3_MULTISIG_MASTER_XPUB"
  ],
  "minimumSignatures": 2,
  "ecdsa": false
}
```

Do not include encrypted mnemonics, private keys, passwords, or other wallet-secret fields in this
helper file. The `publicKeys` values must be the multisig master kpubs from kaspawallet, not
already-derived child kpubs.

Check that a path is the canonical IGRA receive shape:

```bash
./target/debug/cast igra check-msig-path \
  --path m/0/0/1
```

Expected output:

```json
{
  "path": "m/0/0/1",
  "canonical": true,
  "cosigner_index": 0,
  "keychain": 0,
  "address_index": 1
}
```

The path format is:

```text
m/<cosignerIndex>/<keychain>/<addressIndex>
```

For canonical IGRA receive addresses, `cosignerIndex` must be `0`, `keychain` must be `0`, and all
indexes must be non-hardened. `cosignerIndex = 0` means the first signer after sorting the bridge
kpubs. `keychain = 0` means the external receive keychain used by kaspawallet.

Derive the canonical address from an official-style public keys file:

```bash
./target/debug/cast igra derive-msig-address \
  --network mainnet \
  --path m/0/0/1 \
  --keys-file msig-public-keys.json
```

For example, with the current mainnet test multisig public keys:

```json
{
  "publicKeys": [
    "kpub2J6iiGuzPiZMkr275HtuRX7Z5MdPaWj5piY4iZNYJhwMtDY4AEJ4bv6hXXms39kcsg1byFPCb8LeP6S7aMABFQEBWevyLxN9a4Q7wsbxqVB",
    "kpub2JWDD3DcwxNQPQ4sTRdVuPWoK3ZjmohQi1ZQ6kjAW7sYVRbidWFC1taYm6BUg9oySMEhP8Pte7A19zxG86NRqVnuX1jZYp6bZXBRhLwTdRX",
    "kpub2KU4xudChpxnqvDBMUSx3g6mGYNjeDnzy12xJ5LF4Z3aFQe9ZAo8wJoFUvHqRThLNs6MkTU4sXpaiYmxH8jYSFw3n6KuynAQXqBJeNMNRvx"
  ],
  "minimumSignatures": 2,
  "ecdsa": false
}
```

the helper derives:

```text
kaspa:pq0nm7uwyjh6fnhyxt29dd9kmk9pdw0rzjd239yumk9l3wj7mdekvwzglcu9r
```

The same command also supports repeated `--kpub` arguments instead of `--keys-file`:

```bash
./target/debug/cast igra derive-msig-address \
  --network mainnet \
  --path m/0/0/1 \
  --minimum-signatures 2 \
  --kpub kpub_SIGNER_1_MULTISIG_MASTER_XPUB \
  --kpub kpub_SIGNER_2_MULTISIG_MASTER_XPUB \
  --kpub kpub_SIGNER_3_MULTISIG_MASTER_XPUB
```

Verify an expected address:

```bash
./target/debug/cast igra verify-msig-address \
  --address kaspa:EXPECTED_MULTISIG_ADDRESS \
  --network mainnet \
  --path m/0/0/1 \
  --keys-file msig-public-keys.json
```

These helpers sort the master kpubs, derive per-path xpubs, build the multisig redeem script, derive
the P2SH script/address, and report the derived keys and script data as JSON. `verify-msig-address`
exits with an error if the derived address does not match `--address`.

## Input JSON

Create an input file, for example `exit-input.json`:

```json
{
  "locking_utxos": [
    {
      "transaction_id": "PUT_REAL_KASPA_UTXO_TXID_HERE",
      "index": 0,
      "amount_sompi": 200000000,
      "amount_kas": "2.00000000",
      "address": "kaspa:PUT_CANONICAL_MULTISIG_ADDRESS_HERE",
      "script_public_key": {
        "version": 0,
        "script": "aa205933185b78c71f0833770ca4aa6b62423af00d0efc2832025a23999543f220f787"
      },
      "derivation_path": "m/0/0/1"
    }
  ],
  "exits": [
    {
      "message_id": "PUT_32_BYTE_EXIT_MESSAGE_ID_HEX_HERE",
      "recipient": "kaspa:qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqkx9awp4e",
      "amount_sompi": 99000000,
      "amount_kas": "0.99000000"
    }
  ],
  "change": {
    "derivation_path": "m/0/0/2",
    "amount_sompi": 100000000,
    "amount_kas": "1.00000000",
    "address": "kaspa:PUT_CHANGE_MULTISIG_ADDRESS_HERE"
  },
  "fee_sompi": 1000000,
  "fee_kas": "0.01000000",
  "multisig": {
    "minimum_signatures": 2,
    "extended_public_keys": [
      "kpub_SIGNER_1_MULTISIG_MASTER_XPUB",
      "kpub_SIGNER_2_MULTISIG_MASTER_XPUB",
      "kpub_SIGNER_3_MULTISIG_MASTER_XPUB"
    ],
    "ecdsa": false
  }
}
```

Rules:

- `script_public_key.script` must be the IGRA KAS locking script:
  `aa205933185b78c71f0833770ca4aa6b62423af00d0efc2832025a23999543f220f787`.
- `message_id` must be exactly 32 bytes, hex encoded.
- `recipient` must match the selected network. Use `kaspa:` for mainnet.
- `sum(locking_utxos.amount_sompi)` must equal `sum(exits.amount_sompi) + fee_sompi`.
- If `change` is present, the arithmetic becomes
  `sum(locking_utxos.amount_sompi) = sum(exits.amount_sompi) + change.amount_sompi + fee_sompi`.
- Kaspa storage mass is value-sensitive. Many small outputs, or very small change outputs, can be
  non-standard even when the transaction byte size looks small.
- `amount_kas`, `fee_kas`, and address fields are optional in the input JSON, but recommended for
  operator review. `amount_sompi` remains authoritative. If a readable KAS amount is present, the
  CLI verifies it is exactly equal to the sompi value. If `locking_utxos[*].address` or
  `change.address` is present, the CLI verifies it matches the script or derived multisig path.
- `extended_public_keys` must be official kaspawallet multisig master public keys, not single-sig
  wallet public keys.
- `derivation_path` must be the exact official kaspawallet path for the UTXO being spent. For IGRA
  multisig locking UTXOs, the CLI enforces the canonical receive-path shape `m/0/0/<index>`, where
  cosigner index `0` means the first signer after sorting all bridge kpubs, and keychain `0` means
  external receive.
- `change` is optional. If present, it creates one extra output after all exit outputs. This output
  is not included in the IGRA payload message ID list; it is locked back to the canonical bridge
  multisig P2SH script at `change.derivation_path`.

### Change Back To The Same Canonical Multisig Address

Operators may intentionally send change back to the same canonical multisig address that provided
the input UTXO. To do that, set `change.derivation_path` to the same path as the spent
`locking_utxos[*].derivation_path`.

Example:

```json
{
  "locking_utxos": [
    {
      "transaction_id": "PUT_REAL_KASPA_UTXO_TXID_HERE",
      "index": 0,
      "amount_sompi": 200000000,
      "amount_kas": "2.00000000",
      "address": "kaspa:PUT_CANONICAL_MULTISIG_ADDRESS_HERE",
      "script_public_key": {
        "version": 0,
        "script": "aa205933185b78c71f0833770ca4aa6b62423af00d0efc2832025a23999543f220f787"
      },
      "derivation_path": "m/0/0/1"
    }
  ],
  "exits": [
    {
      "message_id": "PUT_32_BYTE_EXIT_MESSAGE_ID_HEX_HERE",
      "recipient": "kaspa:qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqkx9awp4e",
      "amount_sompi": 99000000,
      "amount_kas": "0.99000000"
    }
  ],
  "change": {
    "derivation_path": "m/0/0/1",
    "amount_sompi": 100000000,
    "amount_kas": "1.00000000",
    "address": "kaspa:PUT_CANONICAL_MULTISIG_ADDRESS_HERE"
  },
  "fee_sompi": 1000000,
  "fee_kas": "0.01000000",
  "multisig": {
    "minimum_signatures": 2,
    "extended_public_keys": [
      "kpub_SIGNER_1_MULTISIG_MASTER_XPUB",
      "kpub_SIGNER_2_MULTISIG_MASTER_XPUB",
      "kpub_SIGNER_3_MULTISIG_MASTER_XPUB"
    ],
    "ecdsa": false
  }
}
```

This creates an exit output to the recipient and a change output back to the same canonical bridge
multisig script/address derived from `m/0/0/1`.

This is valid when the bridge operations policy uses one canonical treasury address. If the policy
rotates addresses, use a fresh canonical receive path such as `m/0/0/2` for change instead.

## Build Unsigned Exit

For mainnet:

```bash
./target/debug/cast igra build-exit \
  --network mainnet \
  --tx-id-prefix 97b1 \
  --lane-id 97b10000 \
  --input exit-input.json \
  --out-json unsigned-exit.json \
  --out-hex unsigned-exit.hex
```

Useful optional flags:

- `--mining-timeout-secs 0`: disable timeout while mining the payload nonce.
- `--max-nonce <N>`: bound the nonce search for tests.
- `--lane-id 97b10000`: set the canonical IGRA Kaspa lane/subnetwork id. The builder accepts the
  4-byte namespace form (`97b10000`) or the full 20-byte subnetwork id
  (`97b1000000000000000000000000000000000000`).
  Lane exits are Kaspa v1/Toccata transactions; Foundry computes the per-input `computeBudget`
  from the bridge multisig signature script units instead of using legacy `sigOpCount`.
- `--allow-non-igra-lock-script-for-testing`: permit non-official multisig UTXOs for signing
  rehearsals only. Do not use this for real bridge exits from the official IGRA lock script.
- `--allow-mass-limit-override-for-testing`: emit artifacts even when Kaspa mass preflight says the
  transaction is non-standard. Use only for non-broadcast signing rehearsals.
- `--force`: overwrite existing output files.

The command mines the 4-byte payload nonce until the Kaspa transaction ID starts with the requested
hex prefix.

Before mining, the builder performs Kaspa mass preflight using the same KIP-0009 storage-mass
formula used by kaspad and a kaspawallet-style signed-compute estimate. For v0/pre-Toccata
transactions it applies the legacy `100000` standard mass cap. For v1 lane exits, the Toccata
standard cap is relaxed, so build fails if:

- effective mass exceeds the network block mass limit
- `fee_sompi` is below the minimum relay fee for the effective mass

For example, 20 outputs of `0.05 KAS` each can produce storage mass around `3975025`, which is above
the post-Toccata block mass limit and will not fit in a broadcastable exit.
Do not bypass this check for a transaction intended for broadcast. When mass preflight rejects a
batch, the error also estimates how many exits from the same input set may fit if the batch is split
and change is recalculated.

The output manifest echoes the normalized readable fields even if they were omitted from the input:

- `protocol.nonce` as a fixed-width 4-byte hex string, for example `0x0000b1b0`
- `protocol.lane_id`, for example `0x97b10000`
- `protocol.subnetwork_id`, for example `0x97b1000000000000000000000000000000000000`
- `locking_utxos[*].amount_kas` and `locking_utxos[*].address`
- `exits[*].amount_kas`
- `change.amount_kas` and `change.address`, when change is present
- `fee_kas`
- `total_input_kas`
- `total_output_kas`
- `mass.estimated_signed_compute_mass`
- `mass.transient_mass`
- `mass.storage_mass`
- `mass.effective_mass`
- `mass.minimum_relay_fee_sompi` and `mass.minimum_relay_fee_kas`
- `mass.standard_limit_exceeded`, `mass.block_limit_exceeded`, and
  `mass.fee_below_minimum_relay`

The exit payload format is:

```text
0x93 || message_id_1 || message_id_2 || ... || nonce_u32_be
```

The optional change output is a normal Kaspa transaction output. It is not encoded into the `0x93`
payload.

## Verify Unsigned Artifact

The builder should verify the unsigned files before sending them to signers:

```bash
./target/debug/cast igra verify-exit \
  --manifest unsigned-exit.json \
  --hex unsigned-exit.hex
```

Expected shape:

```json
{
  "ok": true,
  "kaspa_tx_id": "97b1...",
  "payload_nonce": 123,
  "inputs": 1,
  "outputs": 1,
  "signed_inputs": 0,
  "fully_signed": false
}
```

If this command fails, do not send the transaction for signing.

## Signer Verification

Each signer receives:

- `unsigned-exit.json`
- `unsigned-exit.hex`, or a partially signed successor such as `signed1.hex`

Before signing, each signer runs:

```bash
./target/debug/cast igra verify-exit \
  --manifest unsigned-exit.json \
  --hex unsigned-exit.hex
```

For partially signed files:

```bash
./target/debug/cast igra verify-exit \
  --manifest unsigned-exit.json \
  --hex signed1.hex \
  --allow-signatures
```

If the artifact was intentionally built from a non-official test multisig UTXO, signers must add
`--allow-non-igra-lock-script-for-testing` to their verification command. Official bridge exits
must not use that flag.

Each signer should inspect:

- `kaspa_tx_id`
- `payload_nonce`; in the manifest, `protocol.nonce` is the same value encoded as 4-byte hex
- `payload_header` in the manifest, which must be `0x93`
- `protocol.lane_id`, which should be `0x97b10000` for the canonical IGRA lane
- `protocol.subnetwork_id`, which should be `0x97b1000000000000000000000000000000000000`
- input UTXO txid, index, amount, locking script, and derivation path
- exit recipient addresses and amounts
- optional change amount and derivation path
- `fee_sompi`
- `mass`; all failure booleans must be `false` for a broadcast-intended transaction
- `tx_id_prefix`
- multisig xpubs and `minimum_signatures`

## Sign With Cast V1 Signer

Do not use Go `kaspawallet sign` for lane v1 exit transactions. It is valid for
normal Toccata wallet flows, but it does not sign this IGRA v1 subnetwork PST
shape correctly. Each offline signer should use the patched `cast igra
sign-exit` command with their protected Go `kaspawallet` `keys.json`. The
command decrypts `encryptedMnemonics` in memory using the wallet password and
does not print the mnemonic.

Signer 1:

```bash
./target/debug/cast igra sign-exit \
  --manifest unsigned-exit.json \
  --hex unsigned-exit.hex \
  --keys-file signer1.keys.json \
  --out-hex signed1.hex
```

If the command is attached to a terminal, it prompts for the Go wallet password.
For scripted offline signing, add `--keys-password-file signer1.password.txt`.

Fallbacks are still supported: use `--mnemonic-file signer1.mnemonic.txt` if the
signer intentionally exports a mnemonic, or `--kprv-file signer1.kprv.txt` if
the signer keeps the kaspawallet multisig master private key. The manifest
already contains the multisig kpubs and each input derivation path; the signer
only supplies their own secret.

If recovering from a PST that already contains invalid signatures, start again
from `unsigned-exit.hex`. If that file is unavailable, signer 1 may add
`--clear-existing-signatures` to discard the bad signature slots before signing.

Verify signer 1 output:

```bash
./target/debug/cast igra verify-exit \
  --manifest unsigned-exit.json \
  --hex signed1.hex \
  --allow-signatures
```

Expected:

```json
{
  "ok": true,
  "signed_inputs": 1,
  "fully_signed": false
}
```

Signer 2 signs signer 1 output:

```bash
./target/debug/cast igra sign-exit \
  --manifest unsigned-exit.json \
  --hex signed1.hex \
  --keys-file signer2.keys.json \
  --out-hex signed2.hex
```

Verify final output:

```bash
./target/debug/cast igra verify-exit \
  --manifest unsigned-exit.json \
  --hex signed2.hex \
  --allow-signatures \
  --require-fully-signed
```

Expected:

```json
{
  "ok": true,
  "fully_signed": true
}
```

Any 2 of the 3 signers can produce the final artifact. For example, signer 3 can sign `signed1.hex`
instead of signer 2:

```bash
./target/debug/cast igra sign-exit \
  --manifest unsigned-exit.json \
  --hex signed1.hex \
  --keys-file signer3.keys.json \
  --out-hex signed13.hex
```

Then verify:

```bash
./target/debug/cast igra verify-exit \
  --manifest unsigned-exit.json \
  --hex signed13.hex \
  --allow-signatures \
  --require-fully-signed
```

## Parse Before Broadcast

Use the official wallet parser:

```bash
kaspawallet parse \
  --keys-file signer1.json \
  --transaction-file signed2.hex
```

Confirm recipient, amount, fee, mass, and fee rate.

## Broadcast

For lane/subnetwork v1 exit transactions, do not broadcast the signed PST directly
with older `kaspawallet broadcast`. Older wallet builds sign the PST correctly, but
their broadcast path can materialize v1 inputs with legacy `sigOpCount` instead of
`computeBudget`.

Only broadcast after the final artifact passes:

```bash
./target/debug/cast igra verify-exit \
  --manifest unsigned-exit.json \
  --hex signed2.hex \
  --allow-signatures \
  --require-fully-signed
```

Then use the Foundry verifier/broadcaster with a synced Kaspa RPC endpoint:

```bash
./target/debug/cast igra verify-exit \
  --manifest unsigned-exit.json \
  --hex signed2.hex \
  --broadcast \
  --kaspa-rpc-url grpc://127.0.0.1:16110 \
  --json
```

## Mainnet Checklist

Before signing or broadcasting:

1. `network` is `mainnet`.
2. Manifest protocol payload header is `0x93`.
3. Kaspa transaction ID starts with the required IGRA prefix.
4. Every input UTXO exists, is unspent, and has the expected amount.
5. Every input uses the IGRA KAS locking script.
6. Every input derivation path is known and correct for that UTXO.
7. Input total equals exit outputs plus optional change output plus fee.
8. Recipient addresses are correct mainnet `kaspa:` addresses.
9. Manifest `mass.standard_limit_exceeded`, `mass.block_limit_exceeded`, and
   `mass.fee_below_minimum_relay` are all `false`.
10. Multisig xpubs are official kaspawallet multisig master xpubs.
11. The final signed file passes `verify-exit --allow-signatures --require-fully-signed`.

## Official Kaspawallet Derivation Path Alignment

The implementation was checked against the official kaspawallet code under
`/Users/user/Source/igra/kaspad/cmd/kaspawallet`.

Important source rules:

- `libkaspawallet/bip39.go` defines the multisig master path as:

```text
m/45'/111111'/0'
```

- `MasterPublicKeyFromMnemonic(..., isMultisig=true)` returns the public key at that multisig
  master path. These are the `kpub...` values that must be used in `multisig.extended_public_keys`.
- `libkaspawallet/sign.go` decides whether an input is multisig by checking whether the partial
  input has more than one `PubKeySignaturePair`.
- For multisig, `sign.go` derives the signer's private key as:

```text
m/45'/111111'/0' + partiallySignedInput.DerivationPath
```

- The signer only signs if its derived public key string equals one of the partial input
  `PubKeySignaturePair.ExtendedPublicKey` values.
- `libkaspawallet/transaction.go` stores derived xpubs in `PubKeySignaturePair`, not master xpubs.
  Therefore the IGRA builder also derives each master xpub by the input `derivation_path` before
  writing the wallet protobuf.
- `libkaspawallet/transaction.go` sorts multisig master xpubs before building official wallet
  unsigned transactions. The IGRA builder canonicalizes the same way.
- `daemon/server/address.go` defines official wallet paths:

```text
multisig: m/<cosignerIndex>/<keychain>/<index>
single-sig: m/<keychain>/<index>
```

- Keychain constants are:

```text
external receive: 0
internal change: 1
```

- `new-address` uses:

```text
m/<wallet cosignerIndex>/0/<next external index>
```

  The first normal `new-address` external index is `1`.
- The wallet sync code scans every cosigner index and both keychains, because multisig wallets can
  receive funds on paths for any cosigner index.
- `create-unsigned-transaction` records the selected UTXO path as
  `s.walletAddressPath(utxo.address)` and places that string into the partially signed transaction.

Alignment conclusion:

- The builder is aligned with official signing if the provided `derivation_path` is the official
  path for the UTXO.
- The builder correctly writes per-input derived xpubs, not master xpubs.
- The builder correctly canonicalizes multisig xpub order before deriving per-input xpubs.
- A path like `m/0/0/0` is valid as a kaspawallet derivation path, but it is not the normal first
  path produced by `kaspawallet new-address`. For IGRA exit builds, the CLI now accepts only the
  canonical sorted-first-signer receive shape `m/0/0/<index>`.

## Known Limitations

- This runbook assumes a real UTXO already exists and is controlled by the intended IGRA locking
  script and signer set.
- The CLI does not discover the derivation path from chain data. Operators must supply the correct
  path.
- The CLI does not broadcast. Broadcasting is intentionally left to the official `kaspawallet`
  workflow after final verification.
- Interoperability has been tested with temporary official kaspawallet 2-of-3 key files and fake
  UTXO data. The test proves signer format compatibility, not that a fake UTXO can be broadcast.
