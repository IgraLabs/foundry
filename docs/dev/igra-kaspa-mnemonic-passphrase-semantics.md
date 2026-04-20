# Kaspa Mnemonic Passphrase Semantics (Tri-State) for IGRA Foundry Fork

## Summary

We must distinguish three different user intents for Kaspa BIP39 passphrases:

1. **UNSET**: user did not provide a passphrase value at all.
2. **EMPTY**: user explicitly provided an empty passphrase (meaning “no passphrase”).
3. **NON-EMPTY**: user provided a non-empty passphrase.

Historically, some of our helper tooling effectively treated **EMPTY** the same as **UNSET** and applied a “testnet convenience” default: `passphrase = mnemonic`. This makes it impossible to express “explicitly no passphrase” while still keeping the convenience behavior for a specific funded wallet setup.

This document defines precise semantics and concrete knobs across:

- Core CLI (`cast`, `forge`) Kaspa wallet options
- Helper binaries (`igra-loadgen`, `kaspa_fund`)
- Repo scripts (`scripts/igra/testnet-smoke.sh`, `scripts/igra/testnet-stress.sh`)
- Documentation and tests
- Forge script broadcasting (`forge script --broadcast`) must also use IGRA transport interception
  and persist tx-map entries (so `cast igra-status` works on script-broadcast tx hashes).

## Goals

- Make passphrase behavior **unambiguous** and **reproducible** for beta users (Discord release).
- Preserve the ability to use the “passphrase equals mnemonic” setup without surprising default behavior.
- Ensure existing test flows still work (deterministic harness, smoke test, 10-account loadgen).

## Non-Goals

- We do not change Kaspa derivation logic (still matches `kaspa-cli` / rusty-kaspa scheme).
- We do not attempt to auto-detect which passphrase is correct.

## Principle: Standard BIP39 Defaults in Core CLI

In the core Foundry CLI surface (`cast`, `forge`):

- **Default passphrase is empty** when the user does not provide one (standard BIP39 behavior).
- “Passphrase equals mnemonic” must be **explicitly requested** (opt-in).

Rationale:
- Most users expect BIP39 passphrase to default to empty.
- Silent defaults that change derivation are the #1 cause of “funds not found” and “address mismatch” confusion.

## Required Semantics (All Surfaces)

When mnemonic-based Kaspa key derivation is used:

### Resolution order (highest precedence wins)

1. Explicit `passphrase-as-mnemonic` switch
2. Explicit passphrase value (including empty)
3. Default (empty passphrase)

### Tri-state mapping

- If `--...-passphrase-as-mnemonic` is set:
  - Effective passphrase = normalized mnemonic phrase content (file contents if mnemonic is a file path).
- Else if passphrase is provided explicitly (including empty):
  - Effective passphrase = provided value (may be `""`).
- Else (passphrase unset):
  - Effective passphrase = `""` (empty).

## CLI Contract (Core): `cast` / `forge`

Kaspa options are defined in `crates/wallets/src/opts.rs` and mapped into `IgraKaspaWalletConfig`.

### Add two explicit flags

- `--mnemonic-passphrase-kaspa-as-mnemonic`
  - Opt-in to `passphrase = mnemonic` behavior.
  - Works whether `--mnemonic-kaspa` is provided inline or as a file path.
  - Conflicts with `--mnemonic-passphrase-kaspa` and `--mnemonic-passphrase-kaspa-empty`.

- `--mnemonic-passphrase-kaspa-empty`
  - Explicitly sets passphrase to the empty string, without requiring tricky shell quoting.
  - Conflicts with `--mnemonic-passphrase-kaspa` and `--mnemonic-passphrase-kaspa-as-mnemonic`.

Environment variable equivalents:

- `KASPA_MNEMONIC_PASSPHRASE_AS_MNEMONIC=1`
- `KASPA_MNEMONIC_PASSPHRASE_EMPTY=1`

### Behavior changes

- No implicit default to `passphrase = mnemonic` in core CLI.
- Users with the “passphrase == mnemonic” wallet setup can use:
  - `--mnemonic-passphrase-kaspa-as-mnemonic`
  - or set `KASPA_MNEMONIC_PASSPHRASE_AS_MNEMONIC=1`

## Helper Binaries

### `igra-loadgen`

Current issue:
- Passphrase is a `String` with default `""`, so “unset” and “explicit empty” are indistinguishable.

Required changes:
- Change passphrase arg to `Option<String>` with no default.
- Add the same two explicit switches:
  - `--kaspa-mnemonic-passphrase-as-mnemonic`
  - `--kaspa-mnemonic-passphrase-empty`

Resolution:
- If `...-as-mnemonic` set: passphrase = mnemonic content
- Else if passphrase provided: use it (including empty)
- Else if `...-empty` set: passphrase = empty
- Else: passphrase = empty

### `kaspa_fund`

Current issue:
- Passphrase is a `String` with default `""` (cannot distinguish unset vs explicit empty via env).

Required changes:
- Change passphrase arg to `Option<String>` with no default.
- Add:
  - `--mnemonic-passphrase-as-mnemonic`
  - `--mnemonic-passphrase-empty`

## Shell Scripts (Repo Helpers)

Scripts currently used for testnet work:
- `scripts/igra/testnet-smoke.sh`
- `scripts/igra/testnet-stress.sh`

Required changes:
- Stop collapsing “unset” into an explicit empty string early in the script.
- Add an explicit opt-in switch for passphrase=mnemonic:
  - `IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC=1`
- Keep support for explicit empty passphrase:
  - `IGRA_MNEMONIC_PASSPHRASE_KASPA=""` means “no passphrase”, not “use mnemonic”.

Default:
- Scripts should be consistent with core CLI: if the user does nothing, passphrase is empty.

## Backward Compatibility / Migration

- Any existing workflow depending on “passphrase defaults to mnemonic” must be updated to set the explicit switch.
- We will update repo docs to show the explicit switch, and update scripts to support it.

## Test Plan (No Regression)

1. Deterministic harness must still pass:
   - `./scripts/igra/deterministic-harness.sh`
2. Testnet smoke still works:
   - Run `scripts/igra/testnet-smoke.sh` with:
     - `IGRA_MNEMONIC_KASPA=<mnemonic>`
     - `IGRA_MNEMONIC_PASSPHRASE_KASPA_AS_MNEMONIC=1` (for the funded testnet setup)
3. 10-account flow still works:
   - `docs/dev/igra-loadgen-10-accounts.md` updated
   - `igra-loadgen` run with either:
     - explicit `--kaspa-mnemonic-passphrase ...`
     - or `--kaspa-mnemonic-passphrase-as-mnemonic`

## Documentation Updates

Update these docs to reflect the new explicit switches:

- `docs/dev/igra-foundry-fork-dev-guide.md`
- `docs/dev/igra-loadgen-10-accounts.md`
