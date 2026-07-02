# IGRA Mainnet Exit Bundle Operations

Last updated: 2026-04-27, Asia/Jerusalem

This runbook records the production process used for real mainnet exit batches.
The `exit-3` flow is the current daily candidate flow: it uses the improved
bundle manifest, bundle-integral signature verification, contract authenticity
preverification, live L2 receipt checks, live Kaspa UTXO checks, mass guardrails,
and kaspawallet-compatible multisig signing artifacts.

Use this document as the prompt context for another Codex instance when asking it
to process a new daily exit bundle.

## Bundle Modes

There are now two valid production entry points.

### Mode A: unsigned-first

The bundle arrives as an `igra-exits` bundle without signer-1 applied yet.

Typical operator flow:

1. verify the bundle
2. build and verify the unsigned transaction
3. signer 1 signs
4. signer 2 signs
5. broadcast

This is the mode where you may sign first and signer 2 signs after you.

### Mode B: signer-1-first

The bundle arrives as a `signed-1` bundle because signer 1 already built and
signed it before sending it to you.

Typical operator flow:

1. verify the current `signed-1` bundle
2. verify the embedded parent `igra-exits` bundle
3. rebuild the unsigned artifacts from `input.json`
4. confirm the rebuild matches the provided unsigned JSON/hex exactly
5. verify the signer-1 artifact against that exact unsigned base
6. signer 2 signs
7. broadcast

Do not assume every batch starts unsigned. Check `manifest.kind` first.

## Known Endpoints

Igra L2:

- RPC: `https://rpc.igralabs.com:8545`
- Explorer UI: `https://explorer.igralabs.com`
- Chain ID: `38833`

Kaspa L1:

- Public explorer UI: `https://explorer.kaspa.org/txs/<txid>`
- Public explorer API transaction endpoint: `https://api.kaspa.org/transactions/<txid>`
- Public explorer API address UTXOs endpoint: `https://api.kaspa.org/addresses/<kaspa-address>/utxos`
- Public explorer API address UTXO count endpoint: `https://api.kaspa.org/addresses/<kaspa-address>/utxos/count`

Kaspa node / wallet:

- `cast igra verify-exit --broadcast` submits the fully signed lane v1 transaction through Kaspa RPC.
- The wallet daemon can use an unrelated non-ECDSA `keys.json` for broadcast. It does not need official bridge private keys.
- The daemon must connect to a synced mainnet `kaspad`.
- Do not use an ECDSA daemon wallet for these bridge transactions because the official bridge multisig is `ecdsa=false`.

Optional staging node context observed during investigation:

- Host mentioned: `stage-roman.igralabs.com`
- Public `16110` was refused during checks.
- Public `26210` answered but did not expose gRPC reflection.
- This was not required for the successful production verification flow; public Kaspa explorer API plus `kaspawallet` broadcast was used.

## Operator Prerequisites

The machine processing the bundle needs:

- This repo built with the exit CLI: `cargo build -p cast`.
- `jq`, `node`, `curl`, `openssl`, `xxd`, and `/usr/bin/shasum`.
- Official `kaspawallet` for signing/broadcast verification workflows.
- The bundle directory, parent funding UTXO file, and `keb_manifest_signing_pub.pem`.
- Network access to `https://rpc.igralabs.com:8545` and `https://api.kaspa.org`.

For another Codex instance, give it this runbook, the new `/tmp/exit-N` folder,
and ask it to follow the daily flow without committing artifacts unless
explicitly requested.

## Daily Flow Summary

For each new `exit-N` directory:

1. Copy the bundle to a readable working directory, usually `/tmp/exit-N`.
2. Locate the `.bundle` directory, funding UTXO file, and `keb_manifest_signing_pub.pem`.
3. Read `manifest.kind` and decide whether the batch is `unsigned-first` or `signed-1-first`.
4. Verify the official bridge multisig address derives from the known kpubs at `m/0/0/1`.
5. Verify every `manifest.json.files[]` SHA-256 entry.
6. Verify the manifest signature with `keb_manifest_signing_pub.pem`.
7. If the bundle is `signed-1-first`, also verify the embedded parent `igra-exits` bundle manifest, integral, and signature.
8. Verify `derived/checks.json`, `derived/verify.checks.json`, `derived/contract.preverify.json`, and `derived/kaspa-exit-tx-artifacts-verify.report.json` when present.
9. Independently query `https://rpc.igralabs.com:8545` for all exit transaction receipts.
10. Independently query `https://api.kaspa.org` for all proposed funding UTXOs.
11. If needed, build `exit-N-official-bridge.input.json` with all exit outputs and change back to the official bridge address/path.
12. If needed, build the unsigned transaction with `cast igra build-exit`.
13. Verify unsigned JSON and kaspawallet hex with `cast igra verify-exit`.
14. If signer 1 already built the unsigned transaction, rebuild it locally from `input.json` and require an exact JSON/hex match.
15. Check mass and fee guardrails.
16. Produce `exit-N-official-bridge-report.md`.
17. Verify signer 1 output if present, verify signer 2 output, then broadcast.
18. After broadcast, verify explorer acceptance, spent funding UTXOs, and change UTXO.

Never skip the live UTXO re-check immediately before signing/broadcast. A valid
bundle can still become unspendable if the selected Kaspa UTXO was already spent.

## Official Bridge Constants

Official bridge Kaspa address:

```text
kaspa:ppvnxxzm0rr37zpnwux2f2ntvfpr4uqdpm7zsvsztg3en92r7gs0wkmr72q9n
```

Official bridge script public key:

```text
aa205933185b78c71f0833770ca4aa6b62423af00d0efc2832025a23999543f220f787
```

Official bridge multisig public keys:

```json
{
  "publicKeys": [
    "kpub2HoLSHkWgT8VxmjL7Qv2hbh5Jq9h11XmmPmy3ua2QH89iVNzv6W55ZLy4dVAV3ArUMEAFZWmdADauHTbLCGQ54HyBqgeKTjB3Mdv8kxjetC",
    "kpub2HsAfqNwGLzHhbbmGfAHtkgMM26VfqKsqKuCDAMAw4SMAoUx8YpoKcYq9tBCBqJXirASDtqo3iwcQtSF9d2MKCuLbuPPzTgyH8C5dMMb5Ms",
    "kpub2JeC9uSRRMjr2ExKPtB7UJJo134UFwg6MaToXQefhJv2tgvx4aWah7UfbGM72iF2gpxgHSUGBVu7J5a5wnnrQuAqHNusi9i35XwHfKZgmnr"
  ],
  "minimumSignatures": 2,
  "ecdsa": false
}
```

Canonical derivation path used for the official bridge address:

```text
m/0/0/1
```

The path means:

- `0`: canonical sorted-signer cosigner index.
- `0`: external receive keychain.
- `1`: address index.

The same path is used for official bridge spend UTXOs and for change back to the official bridge address.

Store the keys file for CLI use:

```bash
cat >/tmp/igra-official-bridge-public-keys.json <<'JSON'
{
  "publicKeys": [
    "kpub2HoLSHkWgT8VxmjL7Qv2hbh5Jq9h11XmmPmy3ua2QH89iVNzv6W55ZLy4dVAV3ArUMEAFZWmdADauHTbLCGQ54HyBqgeKTjB3Mdv8kxjetC",
    "kpub2HsAfqNwGLzHhbbmGfAHtkgMM26VfqKsqKuCDAMAw4SMAoUx8YpoKcYq9tBCBqJXirASDtqo3iwcQtSF9d2MKCuLbuPPzTgyH8C5dMMb5Ms",
    "kpub2JeC9uSRRMjr2ExKPtB7UJJo134UFwg6MaToXQefhJv2tgvx4aWah7UfbGM72iF2gpxgHSUGBVu7J5a5wnnrQuAqHNusi9i35XwHfKZgmnr"
  ],
  "minimumSignatures": 2,
  "ecdsa": false
}
JSON
```

Verify address derivation before every production batch:

```bash
./target/debug/cast igra verify-msig-address \
  --address kaspa:ppvnxxzm0rr37zpnwux2f2ntvfpr4uqdpm7zsvsztg3en92r7gs0wkmr72q9n \
  --network mainnet \
  --path m/0/0/1 \
  --keys-file /tmp/igra-official-bridge-public-keys.json \
  --json
```

Expected:

```json
{
  "matches": true
}
```

## Directory Layout

Input bundle examples:

```text
Downloads/exit-0/
Downloads/exit-1/
```

Typical contents:

```text
funding_utxos.json
funding-utxos.json
keb_manifest_signing_pub.pem
keb-from-<from>-to-<to>-<timestamp>.bundle/
keb-from-<from>-to-<to>-<timestamp>-signed-1.bundle/
keb-from-<from>-to-<to>-<timestamp>.bundle.zip
keb-from-<from>-to-<to>-<timestamp>-signed-1.bundle.zip
```

The funding file has appeared with both spellings. Prefer the file that is
present in the parent `exit-N` directory:

- `funding_utxos.json`
- `funding-utxos.json`

Do not use a bundle-local funding file unless the operator explicitly says it is
the proposal for this batch.

Bundle contents:

```text
manifest.json
manifest.signature.b64
bundle-manifest.igra-exits.json
bundle-signature.igra-exits.json
derived/exit.data.json
derived/checks.json
derived/contract.preverify.json
derived/verify.checks.json
derived/tree.data.json
derived/tree.snapshot.json
derived/checkpoint.end.json
derived/funding-utxos.json
derived/exit-N-official-bridge.input.json
derived/exit-N-official-bridge.unsigned.json
derived/exit-N-official-bridge.unsigned.hex
derived/exit-N-official-bridge.signed-1.hex
derived/Kaswallet-report.txt
derived/Kaswallet-report.signed-1.txt
derived/kaspa-exit-tx-artifacts-verify.report.json
raw/checkpoints.json
raw/keb_burn_logs.json
raw/keb_exit_logs.json
raw/hook_inserted_logs.json
raw/successful_exit_logs.json
raw/tx/*.tx.json
raw/tx/*.receipt.json
refs/kas-exit-bridge-contract-authenticity.expected.json
refs/kas-exit-bridge-exit-pipeline-annex.md
refs/kas-exit-bridge-exit-verification-methodology.md
refs/kaspaExitTransaction/kas-entry-multisig.expected.json
refs/keb-signers.json
```

Notes:

- older bundles may use `refs/kas-exit-bridge-query-audit-methodology.md`
- newer bundles use `refs/kas-exit-bridge-exit-verification-methodology.md`
- newer `signed-1` bundles include both the current signed-1 manifest and the
  embedded parent `igra-exits` manifest/signature

Important operational note:

- macOS privacy controls may block direct reads from `~/Downloads`.
- If commands fail with `Operation not permitted`, copy the whole bundle folder to `/tmp` and work there:

```bash
cp -R ~/Downloads/exit-N /tmp/exit-N
```

After copying, the working directory may be nested, for example:

```text
/tmp/exit-1/exit-1
```

Use the actual directory that contains the funding JSON and the `.bundle` directory.

Set robust shell variables for the run:

```bash
BASE=/tmp/exit-N
BATCH_NAME=exit-N
BUNDLE=$(find "$BASE" -maxdepth 1 -type d -name 'keb-from-*.bundle' | head -n 1)
FUNDING=$(find "$BASE" -maxdepth 1 -type f \( -name 'funding_utxos.json' -o -name 'funding-utxos.json' \) | head -n 1)
MANIFEST_PUB_KEY="$BASE/keb_manifest_signing_pub.pem"
export BASE BATCH_NAME BUNDLE FUNDING MANIFEST_PUB_KEY

test -n "$BUNDLE"
test -n "$FUNDING"
test -f "$MANIFEST_PUB_KEY"
```

If the bundle is nested one level deeper after copying from Downloads, set
`BASE` to that nested directory instead.

## Bundle Verification Checklist

### Alignment With Verification Methodology

The exit bundle checks are aligned with
the methodology file referenced by the bundle itself.

Older bundles may point to:

- `refs/kas-exit-bridge-query-audit-methodology.md`

Newer bundles may point to:

- `refs/kas-exit-bridge-exit-verification-methodology.md`

The newer methodology extends the older query/audit model with:

- contract pre-verification rules
- parent-bundle chain checks
- deterministic bundle-integrity validation order
- Kaspa artifact checks `ARTF-*`
- stage gates for `unsigned`, `signed-1`, and future `signed-2`

This runbook consumes those artifacts and adds the remaining Kaspa L1 controls
needed to build, sign, broadcast, and later audit the real payout transaction.

Methodology provenance:

- `manifest.json` records the methodology path and expected SHA-256.
- `manifest.json` records `bundleIntegral.value`; in the exit-3 format,
  `manifest.signature.b64` is an RSA PKCS#1 SHA-256 signature over that
  bundle-integral digest.
- `keb_manifest_signing_pub.pem` is the public verification key. The expected
  key ID in the manifest is `prod-keb-manifest-v1`.
- The manifest file-hash loop verifies every bundled raw and derived artifact
  against the bundle manifest.
- `derived/contract.preverify.json` records contract code hash and proxy slot
  checks at the start and end of the bundle range.
- `derived/exit.data.json`, `derived/checks.json`,
  `derived/verify.checks.json`, `derived/tree.data.json`,
  `derived/tree.snapshot.json`, `derived/checkpoint.end.json`, and
  `derived/contract.preverify.json` are the core methodology output model used
  by this runbook.
- newer `signed-1` bundles also include
  `derived/kaspa-exit-tx-artifacts-verify.report.json`, which should be treated
  as a first-class artifact-verification summary, not just a convenience file.

Per-exit checks in `derived/checks.json` map to the methodology as follows:

- `eventCardinalityByTxStatus`: exactly one audited event of each type for
  successful exits, and zero audited events for reverted exits.
- `dispatchMessageDecodesCorrectly`: `Mailbox.Dispatch.message` decodes with
  the expected envelope/body schema.
- `messageIdKeccakDispatchMessage`: `messageId == keccak256(Dispatch.message)`.
- `messageIdMatchesExitRequestedDispatchIdDispatchInserted`: the same
  `messageId` is present in `ExitRequested`, `DispatchId`, `Dispatch`, and
  `InsertedIntoTree`.
- `dispatchMessageMatchesRequestExitKasPayoutAndUnlockAmount`: decoded
  `requestId`, `unlockAmountSompi`, and `kasPayoutAddress` match the audited
  exit data.
- `msgValueEqualsBurnIKasAmount`: transaction `msg.value` equals the
  `BurnIKas.amount` event value.

Global and tree checks in `derived/verify.checks.json` map as follows:

- `countDeltaMatchesInsertedEvents`: tree checkpoint count delta equals hook
  `InsertedIntoTree` event count.
- `noDuplicateMessageIds`: no duplicate hook message IDs.
- `noDuplicateLeafIndices`: no duplicate hook leaf indices.
- `noLeafIndexGaps`: hook leaf indices are contiguous across the audited range.
- `allSuccessfulExitInsertedEventsPresentAndMatching`: every successful exit's
  `(txHash, messageId, index)` is present in hook-level tree data.
- `messageIdKeccakDispatchMessageFailures: 0`: run-level summary of the per-exit
  message hash check.
- `requestIdDeltaMatchesSuccessfulExits`,
  `dispatchRequestIdIncrementalNoGaps`, and
  `dispatchRequestIdRangeMatchesCheckpoints`: additional request-ID checkpoint
  consistency checks beyond the base methodology.
- `totalBurnedDeltaMatchesSuccessfulBurns`: additional global accounting check
  for burned amount consistency.
- `dispatchOuterConstantFieldsExceptNonce`: additional invariant that dispatch
  envelope fields stay constant across exits except the nonce.

Important limitations to record per bundle:

- `rootReplay.enabled: false` means Hyperlane Merkle root replay was not
  independently recomputed for that bundle. The run still checks count/index
  continuity and exit-to-hook inclusion.
- For successful internal calls, strict verification of original
  `requestExit(kasPayoutAddress, unlockAmountSompi)` input requires
  `debug_traceTransaction`. If traces were unavailable during bundle generation,
  this must be treated as an explicit limitation of the bundle evidence.
- Exhaustive enumeration of reverted internal `requestExit` attempts also
  requires traces. The runbook verifies reported bundle outputs; it does not
  independently rediscover every possible reverted internal call.

The Kaspa L1 checks later in this runbook are outside the query methodology.
They verify spend-side safety: official bridge multisig derivation, funding UTXO
existence and outpoint indices, output amounts, change path/address, transaction
mass/fees, unsigned JSON/hex consistency, signer progress, broadcast, and public
explorer acceptance.

For newer `signed-1` bundles, also treat these as required:

- current bundle `manifest.kind` matches the stage you received
- embedded parent bundle integral/signature verifies
- embedded parent bundle identity/integral matches the linkage recorded in the
  current signed-1 manifest
- unsigned rebuild from `input.json` matches the provided unsigned JSON/hex exactly
- signer-1 artifact verifies against that exact unsigned base

Set paths if they were not already set:

```bash
BASE=/tmp/exit-N
BATCH_NAME=exit-N
BUNDLE="$BASE/keb-from-<from>-to-<to>-<timestamp>.bundle"
FUNDING="$BASE/funding_utxos.json"
MANIFEST_PUB_KEY="$BASE/keb_manifest_signing_pub.pem"
export BASE BATCH_NAME BUNDLE FUNDING MANIFEST_PUB_KEY
```

Inspect manifest:

```bash
jq '{
  kind,
  schemaVersion,
  createdAt,
  context,
  files_count:(.files|length),
  methodology,
  bundleIntegral,
  bundleIdentity,
  signature,
  artifactChecksums
}' \
  "$BUNDLE/manifest.json"
```

Important:

- `manifest.kind = "kas-exit-bridge-igra-exits-bundle"` means unsigned-first mode
- `manifest.kind = "kas-exit-bridge-signed-1-bundle"` means signer-1-first mode

If the current bundle is `signed-1-first`, also inspect the embedded parent manifest:

```bash
jq '{
  kind,
  schemaVersion,
  createdAt,
  bundleIntegral,
  bundleIdentity,
  methodology
}' \
  "$BUNDLE/bundle-manifest.igra-exits.json"
```

Verify all files listed in the manifest:

```bash
cd "$BUNDLE"
jq -r '.files[] | [.path, .sha256] | @tsv' manifest.json |
while IFS=$'\t' read -r path expected; do
  actual=$(/usr/bin/shasum -a 256 "$path" | /usr/bin/awk '{print $1}')
  if [ "$actual" != "$expected" ]; then
    printf 'MISMATCH\t%s\texpected=%s\tactual=%s\n' "$path" "$expected" "$actual"
  fi
done
```

Expected: no output.

If present, verify the embedded parent `igra-exits` manifest files too:

```bash
cd "$BUNDLE"
jq -r '.files[] | [.path, .sha256] | @tsv' bundle-manifest.igra-exits.json |
while IFS=$'\t' read -r path expected; do
  actual=$(/usr/bin/shasum -a 256 "$path" | /usr/bin/awk '{print $1}')
  if [ "$actual" != "$expected" ]; then
    printf 'PARENT_MISMATCH\t%s\texpected=%s\tactual=%s\n' "$path" "$expected" "$actual"
  fi
done
```

### Bundle Integral Verification

Use the built-in Rust verifier to recompute `manifest.bundleIntegral.value`
exactly as the bundle producer does:

```bash
./target/debug/cast igra verify-bundle-integral \
  --manifest "$BUNDLE/manifest.json"
```

Expected:

```json
{
  "ok": true,
  "algorithm": "sha256",
  "canonicalization": "json-c14n-sorted-keys-no-whitespace-utf8",
  "expected_value": "<manifest.bundleIntegral.value>",
  "computed_value": "<manifest.bundleIntegral.value>",
  "matches": true
}
```

This verifier applies the producer-side rules:

1. start from `manifest.json`
2. remove top-level `signature`
3. keep `bundleIntegral`, but remove `bundleIntegral.value`
4. canonicalize JSON with lexicographically sorted object keys, preserved array order, and no extra whitespace
5. UTF-8 encode that canonical JSON
6. compute SHA-256
7. compare against `manifest.bundleIntegral.value`

Save the machine-readable result:

```bash
./target/debug/cast igra verify-bundle-integral \
  --manifest "$BUNDLE/manifest.json" \
  > "$BASE/${BATCH_NAME}-bundle-integral-verify.json"
```

### Manifest Signature Verification

For the exit-3 daily-candidate format, verify `manifest.signature.b64` with the
provided `keb_manifest_signing_pub.pem`. The signature is not over the literal
pretty-printed `manifest.json` bytes. It verifies the SHA-256 digest equal to
the recomputed `manifest.bundleIntegral.value`.

Create a reproducible check artifact:

```bash
SIG_B64="$BUNDLE/manifest.signature.b64"
SIG_BIN="$BASE/manifest.signature.bin"
INTEGRAL_DIGEST_BIN="$BASE/manifest.bundleIntegral.digest.bin"
SIG_CHECK="$BASE/${BATCH_NAME}-manifest-signature-check.json"

openssl base64 -d -in "$SIG_B64" -out "$SIG_BIN"
jq -r '.bundleIntegral.value' "$BUNDLE/manifest.json" | xxd -r -p > "$INTEGRAL_DIGEST_BIN"

openssl pkeyutl -verify \
  -pubin \
  -inkey "$MANIFEST_PUB_KEY" \
  -sigfile "$SIG_BIN" \
  -in "$INTEGRAL_DIGEST_BIN" \
  -pkeyopt rsa_padding_mode:pkcs1 \
  -pkeyopt digest:sha256
```

Expected:

```text
Signature Verified Successfully
```

For `signed-1-first` mode, also verify the embedded parent `igra-exits` bundle
signature. In the new structure, `bundle-signature.igra-exits.json` is raw
base64 content, not a JSON object:

```bash
PARENT_SIG_B64="$BUNDLE/bundle-signature.igra-exits.json"
PARENT_SIG_BIN="$BASE/parent-bundle.signature.bin"
PARENT_DIGEST_BIN="$BASE/parent-bundle.bundleIntegral.digest.bin"

openssl base64 -d -A -in "$PARENT_SIG_B64" -out "$PARENT_SIG_BIN"
jq -r '.bundleIntegral.value' "$BUNDLE/bundle-manifest.igra-exits.json" | xxd -r -p > "$PARENT_DIGEST_BIN"

openssl pkeyutl -verify \
  -pubin \
  -inkey "$MANIFEST_PUB_KEY" \
  -sigfile "$PARENT_SIG_BIN" \
  -in "$PARENT_DIGEST_BIN" \
  -pkeyopt rsa_padding_mode:pkcs1 \
  -pkeyopt digest:sha256
```

Required:

- current bundle signature verifies
- parent `igra-exits` bundle signature verifies
- current bundle linkage fields match the parent manifest:
  - `sourceBundle.bundleIdentity.value`
  - `parentStageManifest.integral`

Optional recovery check:

```bash
openssl pkeyutl -verifyrecover \
  -pubin \
  -inkey "$MANIFEST_PUB_KEY" \
  -in "$SIG_BIN" \
  -pkeyopt rsa_padding_mode:pkcs1 |
xxd -p -c 256
```

Expected suffix:

```text
<manifest.bundleIntegral.value>
```

For exit-3 this recovered DigestInfo was:

```text
3031300d06096086480165030402010500042061b7f9363b4026c4dc9d8d6c92d73c64be97ad699fbc748d588a77b59c7c0243
```

Also check that `derived/verify.checks.json` agrees:

```bash
jq '.metadata.bundleManifest' "$BUNDLE/derived/verify.checks.json"
```

Expected:

```json
{
  "checked": true,
  "signatureVerified": true,
  "keyId": "prod-keb-manifest-v1",
  "errorCount": 0
}
```

Save a compact machine-readable record:

```bash
PUB_SHA=$(/usr/bin/shasum -a 256 "$MANIFEST_PUB_KEY" | awk '{print $1}')
SIG_B64_SHA=$(/usr/bin/shasum -a 256 "$SIG_B64" | awk '{print $1}')
SIG_BIN_SHA=$(/usr/bin/shasum -a 256 "$SIG_BIN" | awk '{print $1}')
INTEGRAL=$(jq -r '.bundleIntegral.value' "$BUNDLE/manifest.json")
RECOVERED=$(openssl pkeyutl -verifyrecover -pubin -inkey "$MANIFEST_PUB_KEY" -in "$SIG_BIN" -pkeyopt rsa_padding_mode:pkcs1 | xxd -p -c 256)

jq -n \
  --arg checked_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg pub "$MANIFEST_PUB_KEY" \
  --arg pub_sha "$PUB_SHA" \
  --arg sig "$SIG_B64" \
  --arg sig_sha "$SIG_B64_SHA" \
  --arg sig_bin_sha "$SIG_BIN_SHA" \
  --arg digest "$INTEGRAL" \
  --arg recovered "$RECOVERED" \
  '{
    checked_at:$checked_at,
    public_key:{path:$pub, sha256:$pub_sha, key_type:"rsa"},
    signature:{path:$sig, encoding:"base64", algorithm:"sha256-sign", key_id:"prod-keb-manifest-v1", sha256:$sig_sha, decoded_sha256:$sig_bin_sha},
    signed_digest:{source:"manifest.bundleIntegral.value", sha256_digest_hex:$digest, recovered_pkcs1_digestinfo_hex:$recovered, recovered_digest_matches_manifest_bundle_integral:($recovered|endswith($digest))},
    verified:true
  }' > "$SIG_CHECK"
```

Required:

- OpenSSL prints `Signature Verified Successfully`.
- The recovered PKCS#1 DigestInfo ends with `manifest.bundleIntegral.value`.
- `derived/verify.checks.json.metadata.bundleManifest.signatureVerified` is `true`.

### Contract Authenticity Verification

For the improved daily format, inspect the preverification file:

```bash
jq '{
  expectedValuesSha256:.metadata.expectedValuesSha256,
  chainId:.metadata.chainId,
  startStateBlock:.metadata.startStateBlock,
  endStateBlock:.metadata.endStateBlock,
  contracts:[.contracts[] | {
    id,
    address,
    allMatch,
    start:{blockTag:.start.blockTag, codeHashMatches:.start.codeHashMatches, allMatch:.start.allMatch},
    end:{blockTag:.end.blockTag, codeHashMatches:.end.codeHashMatches, allMatch:.end.allMatch}
  }]
}' "$BUNDLE/derived/contract.preverify.json"
```

Required:

- Every contract has `allMatch: true`.
- Start and end `codeHashMatches` are true.
- Start and end slot checks all match.

Check derived exit checks:

```bash
jq '{
  globalErrors,
  exit_count:(.exits|length),
  statuses:([.exits[].status] | group_by(.) | map({status:.[0], count:length})),
  exits_with_errors:([.exits[] | select(.errors? and (.errors|length>0))] | length)
}' "$BUNDLE/derived/checks.json"
```

Check full verification summary:

```bash
jq '{totals, checks, error_count:(.errors|length), errors:.errors}' \
  "$BUNDLE/derived/verify.checks.json"
```

Expected:

- all exits are `success`
- `globalErrors.exit` is empty
- `globalErrors.tree` is empty
- `error_count` is `0`

Summarize exit data:

```bash
jq '{
  metadata:.metadata,
  exit_count:(.exits|length),
  total_sompi:([.exits[].unlockAmountSompi|tonumber]|add),
  total_kas:(([.exits[].unlockAmountSompi|tonumber]|add)/100000000),
  request_ids:[.exits[].requestId],
  exits:[.exits[]|{
    requestId,
    blockNum,
    txHash,
    insertedIntoTreeIndex,
    unlockAmountSompi,
    kas:((.unlockAmountSompi|tonumber)/100000000),
    burnWei,
    messageId,
    payout:.dispatchMessageDecoded.body.kasPayoutAddress,
    nonce:.dispatchMessageDecoded.outer.nonce,
    originBurner:.dispatchMessageDecoded.body.originBurner
  }]
}' "$BUNDLE/derived/exit.data.json"
```

Check Merkle tree range:

```bash
jq '{metadata:.metadata, event_count:(.events|length), first:.events[0], last:.events[-1]}' \
  "$BUNDLE/derived/tree.data.json"
```

Cross-check raw files against derived exits:

```bash
cd "$BUNDLE"
node -e '
const fs=require("fs");
const exits=JSON.parse(fs.readFileSync("derived/exit.data.json","utf8")).exits;
const tree=JSON.parse(fs.readFileSync("derived/tree.data.json","utf8")).events;
const burn=JSON.parse(fs.readFileSync("raw/keb_burn_logs.json","utf8"));
const exitLogs=JSON.parse(fs.readFileSync("raw/keb_exit_logs.json","utf8"));
const errors=[];
const treeByMsg=new Map(tree.map(e=>[e.messageId.toLowerCase(),e]));
const burnByTx=new Map(burn.map(e=>[e.transactionHash.toLowerCase(),e]));
const exitByTx=new Map(exitLogs.map(e=>[e.transactionHash.toLowerCase(),e]));
for (const e of exits) {
  const tx=e.txHash.toLowerCase();
  const msg=e.messageId.toLowerCase();
  const rawTx=`raw/tx/${tx}.tx.json`;
  const rawReceipt=`raw/tx/${tx}.receipt.json`;
  if (!fs.existsSync(rawTx)) errors.push(`missing raw tx ${tx}`);
  if (!fs.existsSync(rawReceipt)) errors.push(`missing raw receipt ${tx}`);
  const receipt=fs.existsSync(rawReceipt)?JSON.parse(fs.readFileSync(rawReceipt,"utf8")):null;
  if (receipt && receipt.status!==1 && receipt.status!=="0x1") errors.push(`receipt not success ${tx}: ${receipt.status}`);
  const te=treeByMsg.get(msg);
  if (!te) errors.push(`missing tree event for request ${e.requestId}`);
  else if (Number(te.index)!==Number(e.insertedIntoTreeIndex)) errors.push(`tree index mismatch request ${e.requestId}`);
  if (!burnByTx.has(tx)) errors.push(`missing burn log for ${tx}`);
  if (!exitByTx.has(tx)) errors.push(`missing exit log for ${tx}`);
  const sompi=BigInt(e.unlockAmountSompi);
  const burnWei=BigInt(e.burnWei);
  if (burnWei !== sompi * 10000000000n) errors.push(`burn amount mismatch request ${e.requestId}`);
}
const ids=exits.map(e=>e.requestId);
const noGaps=ids.every((id,i)=>id===ids[0]+i);
const total=exits.reduce((a,e)=>a+BigInt(e.unlockAmountSompi),0n);
console.log(JSON.stringify({
  exitCount:exits.length,
  requestIdStart:ids[0],
  requestIdEnd:ids[ids.length-1],
  requestIdsNoGaps:noGaps,
  totalSompi:total.toString(),
  totalKas:Number(total)/1e8,
  rawTxFilesChecked:exits.length,
  rawReceiptFilesChecked:exits.length,
  errors
}, null, 2));
'
```

Expected: `errors: []`.

Check Igra RPC receipts:

```bash
cd "$BUNDLE"
node -e '
const fs=require("fs");
const exits=JSON.parse(fs.readFileSync("derived/exit.data.json","utf8")).exits;
process.stdout.write(JSON.stringify(exits.map((e,i)=>({
  jsonrpc:"2.0",
  id:i,
  method:"eth_getTransactionReceipt",
  params:[e.txHash]
}))));
' |
curl -sS -H 'content-type: application/json' --data-binary @- https://rpc.igralabs.com:8545 |
node -e '
const fs=require("fs");
const input=fs.readFileSync(0,"utf8");
let res;
try { res=JSON.parse(input); } catch(e) { console.error(input); process.exit(1); }
const exits=JSON.parse(fs.readFileSync("derived/exit.data.json","utf8")).exits;
const errors=[];
for (const r of res) {
  const e=exits[r.id];
  if (!r.result) { errors.push(`missing receipt for ${e.txHash}`); continue; }
  const bn=parseInt(r.result.blockNumber,16);
  if (bn!==e.blockNum) errors.push(`block mismatch ${e.txHash}: rpc=${bn} bundle=${e.blockNum}`);
  if (String(r.result.status).toLowerCase()!=="0x1") errors.push(`status mismatch ${e.txHash}: ${r.result.status}`);
  if (r.result.transactionHash.toLowerCase()!==e.txHash.toLowerCase()) errors.push(`tx hash mismatch id ${r.id}`);
}
console.log(JSON.stringify({rpcReceiptsChecked:res.length, errors}, null, 2));
'
```

Expected: `errors: []`.

## Funding UTXO Verification

Use the parent funding file unless the bundle-local file is intentionally populated.
During older runs the parent file contained the real proposal and the bundle-local file was empty.

The supported parent file names are:

- `funding_utxos.json`
- `funding-utxos.json`

Summarize funding:

```bash
jq '{
  count:length,
  total_sompi:([.[].bridge_utxo.amount_sompi // .[].amount_sompi] | add),
  total_kas:(([.[].bridge_utxo.amount_sompi // .[].amount_sompi] | add)/100000000),
  head:.[0],
  tail:.[-1]
}' "$FUNDING"
```

Check every proposed funding UTXO is still unspent at the official bridge address:

```bash
LIVE_UTXO_CHECK="$BASE/exit-N-funding-utxo-live-check.json"

curl -sS \
  https://api.kaspa.org/addresses/kaspa:ppvnxxzm0rr37zpnwux2f2ntvfpr4uqdpm7zsvsztg3en92r7gs0wkmr72q9n/utxos |
jq --slurpfile funding "$FUNDING" '
  def wanted($u):
    any($funding[0][]; .bridge_utxo.transaction_id == $u.outpoint.transactionId and (.bridge_utxo.output_index|tonumber) == ($u.outpoint.index|tonumber));
  [ .[] | select(wanted(.)) ] as $matches |
  {
    checked_at:now|todate,
    source:"https://api.kaspa.org/addresses/<official>/utxos",
    expected:($funding[0]|length),
    matches:($matches|length),
    entries:$matches
  }' > "$LIVE_UTXO_CHECK"

jq '{expected, matches, entries:[.entries[]|{txid:.outpoint.transactionId,index:.outpoint.index,amount:.utxoEntry.amount,script:.utxoEntry.scriptPublicKey.scriptPublicKey,daa:.utxoEntry.blockDaaScore}]}' "$LIVE_UTXO_CHECK"
```

Required:

- `matches == expected`
- every live amount equals the funding proposal amount
- every live script equals `aa205933185b78c71f0833770ca4aa6b62423af00d0efc2832025a23999543f220f787`

Optionally inspect each funding transaction by ID:

```bash
jq -r '.[].bridge_utxo.transaction_id' "$FUNDING" |
while read -r TXID; do
  curl -sS "https://api.kaspa.org/transactions/$TXID" |
  jq '{transaction_id,is_accepted,accepting_block_hash,accepting_block_time,accepting_block_blue_score,mass,payload,outputs:[.outputs[]|{index,amount,script_public_key_address,script_public_key_type,script_public_key}]}'
done
```

Required checks:

- transaction `is_accepted` is `true`
- target output index exists
- amount equals `bridge_utxo.amount_sompi`
- script equals `aa205933185b78c71f0833770ca4aa6b62423af00d0efc2832025a23999543f220f787`
- address equals `kaspa:ppvnxxzm0rr37zpnwux2f2ntvfpr4uqdpm7zsvsztg3en92r7gs0wkmr72q9n`
- UTXO is returned by the address UTXO endpoint

## Build Input JSON

Use:

- all exit messages from `derived/exit.data.json`
- `recipient = dispatchMessageDecoded.body.kasPayoutAddress`
- `amount_sompi = unlockAmountSompi`
- `message_id = messageId`
- `fee_sompi = 1000000` (`0.01000000 KAS`)
- one change output back to the official bridge address/path `m/0/0/1`

Generate input:

```bash
node <<'NODE'
const fs=require("fs");
const base=process.env.BASE;
const bundle=process.env.BUNDLE;
const fundingPath=process.env.FUNDING;
const batchName=process.env.BATCH_NAME || "exit-N";
const funding=JSON.parse(fs.readFileSync(fundingPath,"utf8"));
const exitData=JSON.parse(fs.readFileSync(`${bundle}/derived/exit.data.json`,"utf8"));
const keys=JSON.parse(fs.readFileSync("/tmp/igra-official-bridge-public-keys.json","utf8"));
const fee=1000000n;
const sompiToKas=(v)=>{
  v=BigInt(v);
  const whole=v/100000000n;
  const frac=(v%100000000n).toString().padStart(8,"0");
  return `${whole}.${frac}`;
};
const totalIn=funding.reduce((a,u)=>a+BigInt(u.bridge_utxo.amount_sompi),0n);
const totalExits=exitData.exits.reduce((a,e)=>a+BigInt(e.unlockAmountSompi),0n);
const change=totalIn-totalExits-fee;
if (change<=0n) throw new Error(`insufficient funding: in=${totalIn} exits=${totalExits} fee=${fee}`);
const input={
  locking_utxos:funding.map(u=>({
    transaction_id:u.bridge_utxo.transaction_id,
    index:u.bridge_utxo.output_index,
    amount_sompi:Number(u.bridge_utxo.amount_sompi),
    amount_kas:sompiToKas(u.bridge_utxo.amount_sompi),
    address:"kaspa:ppvnxxzm0rr37zpnwux2f2ntvfpr4uqdpm7zsvsztg3en92r7gs0wkmr72q9n",
    script_public_key:{version:0,script:u.bridge_utxo.script_public_key.replace(/^0x/,"")},
    derivation_path:"m/0/0/1"
  })),
  exits:exitData.exits.map(e=>({
    message_id:e.messageId,
    recipient:e.dispatchMessageDecoded.body.kasPayoutAddress,
    amount_sompi:Number(e.unlockAmountSompi),
    amount_kas:sompiToKas(e.unlockAmountSompi)
  })),
  change:{
    derivation_path:"m/0/0/1",
    amount_sompi:Number(change),
    amount_kas:sompiToKas(change),
    address:"kaspa:ppvnxxzm0rr37zpnwux2f2ntvfpr4uqdpm7zsvsztg3en92r7gs0wkmr72q9n"
  },
  fee_sompi:Number(fee),
  fee_kas:sompiToKas(fee),
  multisig:{
    minimum_signatures:keys.minimumSignatures,
    extended_public_keys:keys.publicKeys,
    ecdsa:keys.ecdsa
  }
};
fs.writeFileSync(`${base}/${batchName}-official-bridge.input.json`, JSON.stringify(input,null,2)+"\n");
console.log(JSON.stringify({
  path:`${base}/${batchName}-official-bridge.input.json`,
  totalIn:totalIn.toString(),
  totalExits:totalExits.toString(),
  fee:fee.toString(),
  change:change.toString(),
  changeKas:sompiToKas(change),
  exits:input.exits.length
}, null, 2));
NODE
```

Check balance:

```bash
jq '{
  input_total:([.locking_utxos[].amount_sompi]|add),
  exits_total:([.exits[].amount_sompi]|add),
  change:.change.amount_sompi,
  fee:.fee_sompi,
  balanced:(([.locking_utxos[].amount_sompi]|add)==(([.exits[].amount_sompi]|add)+.change.amount_sompi+.fee_sompi)),
  input_count:(.locking_utxos|length),
  exit_count:(.exits|length),
  locking_address:.locking_utxos[0].address,
  change_address:.change.address,
  path:.locking_utxos[0].derivation_path
}' "$BASE/${BATCH_NAME}-official-bridge.input.json"
```

Expected: `balanced: true`.

## Build And Verify Unsigned Transaction

Branch here based on bundle mode.

- In `unsigned-first` mode, build the unsigned transaction locally from the
  verified input JSON and continue normally.
- In `signed-1-first` mode, the bundle already includes:
  - `derived/${BATCH_NAME}-official-bridge.input.json`
  - `derived/${BATCH_NAME}-official-bridge.unsigned.json`
  - `derived/${BATCH_NAME}-official-bridge.unsigned.hex`
  - `derived/${BATCH_NAME}-official-bridge.signed-1.hex`

In `signed-1-first` mode, do **not** trust those artifacts blindly. Rebuild the
unsigned JSON/hex locally from the bundled input JSON and require an exact
byte-for-byte match before accepting signer 1.

Build:

```bash
./target/debug/cast igra build-exit \
  --network mainnet \
  --tx-id-prefix 97b1 \
  --lane-id 97b10000 \
  --input "$BASE/${BATCH_NAME}-official-bridge.input.json" \
  --out-json "$BASE/${BATCH_NAME}-official-bridge.unsigned.json" \
  --out-hex "$BASE/${BATCH_NAME}-official-bridge.unsigned.hex" \
  --force
```

Verify unsigned JSON/hex consistency:

```bash
./target/debug/cast igra verify-exit \
  --manifest "$BASE/${BATCH_NAME}-official-bridge.unsigned.json" \
  --hex "$BASE/${BATCH_NAME}-official-bridge.unsigned.hex"
```

Expected:

```json
{
  "ok": true,
  "signed_inputs": 0,
  "fully_signed": false
}
```

For `signed-1-first` mode, compare the rebuilt outputs to the bundled outputs:

```bash
shasum -a 256 \
  "$BUNDLE/derived/${BATCH_NAME}-official-bridge.unsigned.json" \
  "$BASE/${BATCH_NAME}-official-bridge.unsigned.json" \
  "$BUNDLE/derived/${BATCH_NAME}-official-bridge.unsigned.hex" \
  "$BASE/${BATCH_NAME}-official-bridge.unsigned.hex"
```

Required:

- bundled unsigned JSON hash equals rebuilt unsigned JSON hash
- bundled unsigned hex hash equals rebuilt unsigned hex hash
- rebuilt txid equals bundled txid
- rebuilt nonce equals bundled nonce

Review mass:

```bash
jq '{protocol, totals:{inputs:([.locking_utxos[].amount_sompi]|add), exits:([.exits[].amount_sompi]|add), change:(.change.amount_sompi // 0), fee:.fee_sompi}, mass}' \
  "$BASE/${BATCH_NAME}-official-bridge.unsigned.json"
```

Required:

- `standard_limit_exceeded: false`
- `block_limit_exceeded: false`
- `fee_below_minimum_relay: false`

## Signing Verification

Official `kaspawallet sign` writes the signed transaction hex to stdout.
Redirect stdout to the next artifact; do not assume a `-O` output flag exists.

There are two signing starts:

- `unsigned-first`: you start from `unsigned.hex`
- `signed-1-first`: you start from an already provided `signed-1.hex`

Signer 1 signs:

```bash
./kaspawallet sign \
  -F "$BASE/${BATCH_NAME}-official-bridge.unsigned.hex" \
  > "$BASE/${BATCH_NAME}-official-bridge.signed-1.hex"
```

Only run that command in `unsigned-first` mode.

In `signed-1-first` mode, verify the provided signer-1 artifact instead:

```bash
./target/debug/cast igra verify-exit \
  --manifest "$BASE/${BATCH_NAME}-official-bridge.unsigned.json" \
  --hex "$BUNDLE/derived/${BATCH_NAME}-official-bridge.signed-1.hex" \
  --allow-signatures
```

Then copy it into the working path you want to use for signer 2:

```bash
cp "$BUNDLE/derived/${BATCH_NAME}-official-bridge.signed-1.hex" \
  "$BASE/${BATCH_NAME}-official-bridge.signed-1.hex"
```

After signer 1:

```bash
./target/debug/cast igra verify-exit \
  --manifest "$BASE/${BATCH_NAME}-official-bridge.unsigned.json" \
  --hex "$BASE/${BATCH_NAME}-official-bridge.signed-1.hex" \
  --allow-signatures
```

Expected:

```json
{
  "ok": true,
  "signed_inputs": "<input_count>",
  "fully_signed": false
}
```

For exit-3, `<input_count>` is `2`. For older one-input batches it was `1`.

For `signed-1-first` mode, require all of the following before moving to signer 2:

- signer-1 txid equals the rebuilt unsigned txid
- signer-1 nonce equals the rebuilt unsigned nonce
- `signed_inputs == <input_count>`
- `fully_signed == false`
- the full-signature guard fails exactly as expected

The full-signature check should fail after signer 1:

```bash
./target/debug/cast igra verify-exit \
  --manifest "$BASE/${BATCH_NAME}-official-bridge.unsigned.json" \
  --hex "$BASE/${BATCH_NAME}-official-bridge.signed-1.hex" \
  --allow-signatures \
  --require-fully-signed
```

Expected failure:

```text
transaction is not fully signed according to minimum_signatures
```

Signer 2 signs:

```bash
./kaspawallet sign \
  -F "$BASE/${BATCH_NAME}-official-bridge.signed-1.hex" \
  > "$BASE/${BATCH_NAME}-official-bridge.signed-2.hex"
```

After signer 2:

```bash
./target/debug/cast igra verify-exit \
  --manifest "$BASE/${BATCH_NAME}-official-bridge.unsigned.json" \
  --hex "$BASE/${BATCH_NAME}-official-bridge.signed-2.hex" \
  --allow-signatures \
  --require-fully-signed
```

Expected:

```json
{
  "ok": true,
  "signed_inputs": "<input_count>",
  "fully_signed": true
}
```

Record SHA256 hashes:

```bash
shasum -a 256 \
  "$BASE/${BATCH_NAME}-official-bridge.unsigned.json" \
  "$BASE/${BATCH_NAME}-official-bridge.unsigned.hex" \
  "$BASE/${BATCH_NAME}-official-bridge.signed-1.hex" \
  "$BASE/${BATCH_NAME}-official-bridge.signed-2.hex"
```

## Broadcast

Before broadcast, re-check that the funding UTXO is still unspent:

```bash
curl -sS \
  https://api.kaspa.org/addresses/kaspa:ppvnxxzm0rr37zpnwux2f2ntvfpr4uqdpm7zsvsztg3en92r7gs0wkmr72q9n/utxos |
jq '.[] | select(.outpoint.transactionId=="<funding-txid>" and (.outpoint.index|tonumber)==<index>)'
```

Broadcast from `cast`; do not use older `kaspawallet broadcast` for lane v1
exits because those wallet builds can materialize `sigOpCount` instead of
`computeBudget`:

```bash
./target/debug/cast igra verify-exit \
  --manifest "$BASE/${BATCH_NAME}-official-bridge.unsigned.json" \
  --hex "$BASE/${BATCH_NAME}-official-bridge.signed-2.hex" \
  --broadcast \
  --kaspa-rpc-url grpc://127.0.0.1:16110 \
  --json
```

This path first verifies the signed artifact, requires a fully signed 2-of-3 transaction, decodes the kaspawallet protobuf, and then submits the Kaspa transaction over gRPC. The returned `kaspa_tx_id` must match the unsigned manifest.

The wallet daemon may require a `keys.json` to start, but this can be any unrelated non-ECDSA wallet.
Broadcast does not use private keys from the daemon wallet; it extracts the signed transaction and calls `SubmitTransaction`.

Expected broadcast output:

```text
Transactions were sent successfully
Transaction ID(s):
    <kaspa_tx_id from unsigned manifest>
```

## Post-Broadcast Verification

Check transaction acceptance:

```bash
TXID=<broadcast-txid>
curl -sS "https://api.kaspa.org/transactions/$TXID" |
jq '{
  transaction_id,
  is_accepted,
  accepting_block_hash,
  accepting_block_time,
  accepting_block_blue_score,
  mass,
  payload,
  inputs:[.inputs[] | {previous_outpoint_hash, previous_outpoint_index}],
  outputs:[.outputs[] | {
    index,
    amount,
    script_public_key_address,
    script_public_key_type,
    script_public_key
  }]
}'
```

Required:

- `is_accepted: true`
- transaction ID equals manifest `protocol.kaspa_tx_id`
- input references the expected funding outpoint
- payload equals manifest `protocol.payload_hex` without the `0x`
- output count equals `exits.length + 1` if change exists

Check old funding UTXO is spent:

```bash
curl -sS \
  https://api.kaspa.org/addresses/kaspa:ppvnxxzm0rr37zpnwux2f2ntvfpr4uqdpm7zsvsztg3en92r7gs0wkmr72q9n/utxos |
jq '[.[] | select(.outpoint.transactionId=="<funding-txid>" and (.outpoint.index|tonumber)==<funding-index>)] | {
  old_funding_utxo_still_unspent:(length>0),
  matches:.
}'
```

Expected:

```json
{
  "old_funding_utxo_still_unspent": false,
  "matches": []
}
```

Check change UTXO exists:

```bash
curl -sS \
  https://api.kaspa.org/addresses/kaspa:ppvnxxzm0rr37zpnwux2f2ntvfpr4uqdpm7zsvsztg3en92r7gs0wkmr72q9n/utxos |
jq '[.[] | select(.outpoint.transactionId=="<broadcast-txid>")]'
```

Expected:

- one change UTXO at the final output index
- amount equals manifest `change.amount_sompi`
- script equals official bridge script
- `isCoinbase: false`

Optionally check recipient UTXOs by address if the recipient has not already spent them:

```bash
curl -sS "https://api.kaspa.org/addresses/<recipient-address>/utxos" |
jq '[.[] | select(.outpoint.transactionId=="<broadcast-txid>")]'
```

Open UI for manual confirmation:

```text
https://explorer.kaspa.org/txs/<broadcast-txid>
```

## Report Requirements

Each bundle should get a report named:

```text
exit-N-official-bridge-report.md
```

The report must include:

1. Bundle path, block range, chain ID, contract addresses, and methodology hash.
2. Bundle kind and whether the batch was `unsigned-first` or `signed-1-first`.
3. Current bundle manifest hash check result.
4. Current manifest signature verification result, public key path, public key SHA-256,
   signature SHA-256, and the signed `bundleIntegral.value`.
5. If `signed-1-first`, embedded parent `igra-exits` manifest hash check,
   integral verification, signature verification, and linkage match result.
6. If `signed-1-first`, exact rebuild-match proof for bundled vs rebuilt
   unsigned JSON/hex.
7. Contract authenticity preverification result from `derived/contract.preverify.json`.
8. Derived `checks.json`, `verify.checks.json`, and `kaspa-exit-tx-artifacts-verify.report.json` results when present.
9. Raw-file cross-check result.
10. Igra RPC receipt check result.
11. Funding UTXO source file and whether bundle-local funding differs.
12. Funding transaction API acceptance result.
13. Funding UTXO unspent check before build/sign/broadcast.
14. Official bridge kpubs, `minimumSignatures`, `ecdsa`, path, derived address, and script.
15. Generated or bundled input JSON path and hash.
16. Unsigned JSON/hex paths and hashes.
17. `verify-exit` output for unsigned, signed-1, and signed-2.
18. Mass and fee report.
19. Broadcast command and output.
20. Post-broadcast `is_accepted` result, accepting block hash/score/time, and transaction mass.
21. Old funding UTXO spent confirmation.
22. All accepted outputs with index, request ID, amount, address, and script type.
23. Change UTXO confirmation.
24. Final status line.

## Completed Production Batches

### exit-0

- Bundle: `keb-from-1082603-to-4406399-20260421T155200Z.bundle`
- Exits: `2`
- Total exits: `5.87542780 KAS`
- Funding UTXO: `97b100bfe3343abb7bf50dd7ae1ef5739952d72bce9022cdb55c789b39c8de9c:0`
- Broadcast TXID: `97b177f5a3c6a5f5c13114bf977c31e1248b82aa9a64051a6231a0454e178ddc`
- Accepted: `true`
- Accepting block hash: `2c282c5a80332203c01b363370952f711ec229e898362b531b199cc7910cedd7`
- Change UTXO: `97b177f5a3c6a5f5c13114bf977c31e1248b82aa9a64051a6231a0454e178ddc:2`

### exit-1

- Bundle: `keb-from-4406400-to-4492799-20260421T160338Z.bundle`
- Exits: `20`
- Total exits: `138933.00000000 KAS`
- Funding UTXO: `97b136253c8111862b958d5a52a92a41865565bc86344c9f16bff25f25f3c901:0`
- Broadcast TXID: `97b1f2fd32312bb40c122b433d17355f4d0cd490882a6d9dde53702d083c42aa`
- Accepted: `true`
- Accepting block hash: `7e7119d886eca5a3f31d9a8f507e5f9dc9d3627e982f7dbac22e9882bf2d7727`
- Change UTXO: `97b1f2fd32312bb40c122b433d17355f4d0cd490882a6d9dde53702d083c42aa:20`

### exit-2

- Bundle: `keb-from-4492800-to-4579199-20260421T202614Z.bundle`
- Exits: `20`
- Total exits: `183621.63530300 KAS`
- Broadcast TXID: `97b19d4d29df4804cf5b0c3bab4b13946ff398b5c9bfb16e1348fcd33b646062`
- Accepted: `true`
- Report SHA256: `8c12e6727f684067ca73723d0544d32cda6a726704d31e008a80f00cecf81c09`

## Prepared Daily Candidate

### exit-3

- Bundle: `keb-from-4579200-to-4665599-20260423T124127Z.bundle`
- Format: improved daily-candidate format with manifest signature, bundle integral, artifact checksums, and contract preverification.
- Exits: `13`
- Total exits: `192115.00000000 KAS`
- Funding UTXOs:
  - `97b1cd42c1cc1374ba687da26c20d9a10aa8e10d3eef60352aac690408108c66:0`, `122500.00000000 KAS`
  - `97b1cf5f0f24f1d5ec1daf9a86e0a755e14ca5ddecf1db1fed65aadd4e343918:0`, `75877.02100000 KAS`
- Unsigned TXID: `97b16c9347c158ca3bc04012ee3d9fbce44877a2adf32ff3ead170b7337381c1`
- Inputs: `2`
- Outputs: `14` (`13` exits plus `1` change)
- Change: `6262.01100000 KAS`
- Fee: `0.01000000 KAS`
- Effective mass: `12870`
- Manifest signature: verified with `keb_manifest_signing_pub.pem`
- Signed bundle integral: `61b7f9363b4026c4dc9d8d6c92d73c64be97ad699fbc748d588a77b59c7c0243`
- Report: `/tmp/exit-3/exit-3-official-bridge-report.md`
- Status: unsigned artifact prepared and verified; awaiting signer 1.
