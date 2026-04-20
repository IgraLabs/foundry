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
git switch roman/igra-exit-unsigned-cli
cargo build -p cast
```

Use the local binary:

```bash
./target/debug/cast igra --help
```

## Input JSON

Create an input file, for example `exit-input.json`:

```json
{
  "locking_utxos": [
    {
      "transaction_id": "PUT_REAL_KASPA_UTXO_TXID_HERE",
      "index": 0,
      "amount_sompi": 100000000,
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
      "amount_sompi": 99000000
    }
  ],
  "change": {
    "derivation_path": "m/0/0/2",
    "amount_sompi": 500000
  },
  "fee_sompi": 1000000,
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
      "amount_sompi": 100500000,
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
      "amount_sompi": 99000000
    }
  ],
  "change": {
    "derivation_path": "m/0/0/1",
    "amount_sompi": 500000
  },
  "fee_sompi": 1000000,
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
  --input exit-input.json \
  --out-json unsigned-exit.json \
  --out-hex unsigned-exit.hex
```

Useful optional flags:

- `--mining-timeout-secs 0`: disable timeout while mining the payload nonce.
- `--max-nonce <N>`: bound the nonce search for tests.
- `--force`: overwrite existing output files.

The command mines the 4-byte payload nonce until the Kaspa transaction ID starts with the requested
hex prefix.

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

Each signer should inspect:

- `kaspa_tx_id`
- `payload_nonce`
- `payload_header` in the manifest, which must be `0x93`
- input UTXO txid, index, amount, locking script, and derivation path
- exit recipient addresses and amounts
- optional change amount and derivation path
- `fee_sompi`
- `tx_id_prefix`
- multisig xpubs and `minimum_signatures`

## Sign With Official Kaspawallet

Signer 1:

```bash
kaspawallet sign \
  --keys-file signer1.json \
  --password 'SIGNER_1_PASSWORD' \
  --transaction-file unsigned-exit.hex \
  > signed1.hex
```

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
kaspawallet sign \
  --keys-file signer2.json \
  --password 'SIGNER_2_PASSWORD' \
  --transaction-file signed1.hex \
  > signed2.hex
```

The official wallet should print:

```text
The transaction is signed and ready to broadcast
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
kaspawallet sign \
  --keys-file signer3.json \
  --password 'SIGNER_3_PASSWORD' \
  --transaction-file signed1.hex \
  > signed13.hex
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

With a synced official wallet daemon connected to mainnet:

```bash
kaspawallet broadcast \
  --transaction-file signed2.hex
```

Only broadcast after the final artifact passes:

```bash
./target/debug/cast igra verify-exit \
  --manifest unsigned-exit.json \
  --hex signed2.hex \
  --allow-signatures \
  --require-fully-signed
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
9. Multisig xpubs are official kaspawallet multisig master xpubs.
10. The final signed file passes `verify-exit --allow-signatures --require-fully-signed`.

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
