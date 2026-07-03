use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit},
};
use clap::{Args, Subcommand};
use eyre::{Result, WrapErr, bail, eyre};
use foundry_common::igra_bundle::verify_bundle_integral;
use foundry_common::igra_exit::{
    BuildExitInput, BuildExitOptions, MultisigAddressInput, SignExitOptions, VerifyExitOptions,
    broadcast_wallet_transaction, build_unsigned_exit, check_multisig_derivation_path,
    decode_wallet_transaction, derive_multisig_address, inspect_exit_wallet_transaction,
    sign_exit_wallet_transaction, verify_multisig_address, verify_unsigned_exit,
};
use serde::Deserialize;
use std::{fs, path::PathBuf, time::Duration};

#[derive(Debug, Args)]
pub struct IgraArgs {
    #[command(subcommand)]
    pub command: IgraSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum IgraSubcommand {
    /// Build a wallet-compatible unsigned IGRA exit transaction.
    #[command(name = "build-exit")]
    BuildExit(BuildExitArgs),
    /// Verify an unsigned or partially signed IGRA exit transaction.
    #[command(name = "verify-exit")]
    VerifyExit(VerifyExitArgs),
    /// Sign an IGRA exit PST with a kaspawallet keys.json, mnemonic, or multisig master kprv.
    #[command(name = "sign-exit")]
    SignExit(SignExitArgs),
    /// Decode an IGRA exit PST hex file for offline human inspection.
    #[command(name = "inspect-exit")]
    InspectExit(InspectExitArgs),
    /// Derive an official kaspawallet multisig address from kpubs and a path.
    #[command(name = "derive-msig-address")]
    DeriveMsigAddress(DeriveMsigAddressArgs),
    /// Verify that an address matches kpubs and a kaspawallet multisig path.
    #[command(name = "verify-msig-address")]
    VerifyMsigAddress(VerifyMsigAddressArgs),
    /// Check that a derivation path is the canonical IGRA receive shape.
    #[command(name = "check-msig-path")]
    CheckMsigPath(CheckMsigPathArgs),
    /// Recompute and verify manifest.bundleIntegral.value from a bundle manifest.
    #[command(name = "verify-bundle-integral")]
    VerifyBundleIntegral(VerifyBundleIntegralArgs),
}

#[derive(Debug, Args)]
pub struct BuildExitArgs {
    /// Kaspa network. Use `mainnet` for real mainnet exits.
    #[arg(long)]
    pub network: String,

    /// Required Kaspa txid prefix, hex encoded. Example: 97b1.
    #[arg(long)]
    pub tx_id_prefix: String,

    /// IGRA Kaspa lane id/subnetwork id. Use 97b10000 for the canonical IGRA lane.
    #[arg(long)]
    pub lane_id: String,

    /// JSON input describing KAS locking UTXOs, exit messages, recipients, fee, and multisig keys.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub input: PathBuf,

    /// Output JSON manifest path.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub out_json: PathBuf,

    /// Output kaspawallet PartiallySignedTransaction hex path.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub out_hex: PathBuf,

    /// Prefix mining timeout in seconds. Use 0 to disable the timeout.
    #[arg(long, default_value_t = 120)]
    pub mining_timeout_secs: u64,

    /// Optional inclusive max payload nonce for bounded tests/rehearsals.
    #[arg(long)]
    pub max_nonce: Option<u32>,

    /// Allow non-official IGRA locking scripts. Testing only; never use for bridge exits.
    #[arg(long)]
    pub allow_non_igra_lock_script_for_testing: bool,

    /// Permit mass-invalid artifacts for signing rehearsal only. Do not broadcast these.
    #[arg(long)]
    pub allow_mass_limit_override_for_testing: bool,

    /// Overwrite existing output files.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct VerifyExitArgs {
    /// JSON manifest produced by `cast igra build-exit`.
    #[arg(long = "manifest", value_hint = clap::ValueHint::FilePath)]
    pub manifest_json: PathBuf,

    /// kaspawallet PartiallySignedTransaction hex file.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub hex: PathBuf,

    /// Permit signatures in the wallet hex while still checking tx body and protocol invariants.
    #[arg(long)]
    pub allow_signatures: bool,

    /// Require every input to have at least minimum_signatures populated signatures.
    #[arg(long)]
    pub require_fully_signed: bool,

    /// Submit the verified Kaspa transaction to a Kaspa RPC endpoint.
    #[arg(long)]
    pub broadcast: bool,

    /// Kaspa RPC URL used for --broadcast, for example grpc://127.0.0.1:16110.
    #[arg(long, env = "KASPA_RPC_URL")]
    pub kaspa_rpc_url: Option<String>,

    /// Allow non-official IGRA locking scripts. Testing only; never use for bridge exits.
    #[arg(long)]
    pub allow_non_igra_lock_script_for_testing: bool,
}

#[derive(Debug, Args)]
pub struct SignExitArgs {
    /// JSON manifest produced by `cast igra build-exit`.
    #[arg(long = "manifest", value_hint = clap::ValueHint::FilePath)]
    pub manifest_json: PathBuf,

    /// Input kaspawallet PartiallySignedTransaction hex file.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub hex: PathBuf,

    /// Output kaspawallet PartiallySignedTransaction hex file.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub out_hex: PathBuf,

    /// File containing one signer mnemonic phrase.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub mnemonic_file: Option<PathBuf>,

    /// File containing one kaspawallet multisig master private key, usually kprv...
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub kprv_file: Option<PathBuf>,

    /// Go kaspawallet keys.json file containing encryptedMnemonics.
    #[arg(long = "keys-file", value_hint = clap::ValueHint::FilePath)]
    pub keys_file: Option<PathBuf>,

    /// File containing the Go kaspawallet keys.json password.
    #[arg(long = "keys-password-file", value_hint = clap::ValueHint::FilePath, requires = "keys_file")]
    pub keys_password_file: Option<PathBuf>,

    /// Go kaspawallet keys.json password in cleartext. Prefer the hidden prompt or --keys-password-file.
    #[arg(
        long = "unsafe-keys-password",
        env = "KASPAWALLET_KEYS_PASSWORD",
        hide_env_values = true
    )]
    pub unsafe_keys_password: Option<String>,

    /// Drop existing PST signatures before signing. Use to recover from signatures made by the wrong signer.
    #[arg(long)]
    pub clear_existing_signatures: bool,

    /// Overwrite existing output file.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct InspectExitArgs {
    /// Kaspa network used for address encoding. Use `mainnet` for real exits.
    #[arg(long)]
    pub network: String,

    /// kaspawallet PartiallySignedTransaction hex file.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub hex: PathBuf,
}

#[derive(Debug, Args)]
pub struct DeriveMsigAddressArgs {
    /// Kaspa network. Use `mainnet` for real mainnet addresses.
    #[arg(long)]
    pub network: String,

    /// Official kaspawallet multisig receive path, usually m/0/0/<index>.
    #[arg(long)]
    pub path: String,

    /// Required signatures. Optional when --keys-file contains minimumSignatures.
    #[arg(long)]
    pub minimum_signatures: Option<u32>,

    /// Multisig master public key. Repeat once per signer.
    #[arg(long = "kpub")]
    pub kpubs: Vec<String>,

    /// Optional official-style keys JSON with publicKeys, minimumSignatures, and ecdsa.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub keys_file: Option<PathBuf>,

    /// Use ECDSA multisig derivation.
    #[arg(long)]
    pub ecdsa: bool,
}

#[derive(Debug, Args)]
pub struct VerifyMsigAddressArgs {
    /// Expected Kaspa multisig address.
    #[arg(long)]
    pub address: String,

    #[command(flatten)]
    pub derive: DeriveMsigAddressArgs,
}

#[derive(Debug, Args)]
pub struct CheckMsigPathArgs {
    /// Official kaspawallet multisig receive path to validate.
    #[arg(long)]
    pub path: String,
}

#[derive(Debug, Args)]
pub struct VerifyBundleIntegralArgs {
    /// Bundle manifest.json path.
    #[arg(long = "manifest", value_hint = clap::ValueHint::FilePath)]
    pub manifest_json: PathBuf,
}

#[derive(Debug, Deserialize)]
struct KaspawalletPublicKeysFile {
    #[serde(rename = "publicKeys")]
    public_keys: Option<Vec<String>>,
    #[serde(rename = "minimumSignatures")]
    minimum_signatures: Option<u32>,
    #[serde(default)]
    ecdsa: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[allow(dead_code)]
struct GoKaspawalletKeysFile {
    version: u32,
    #[serde(default)]
    num_threads: u8,
    #[serde(rename = "encryptedMnemonics")]
    encrypted_mnemonics: Vec<GoKaspawalletEncryptedMnemonic>,
    #[serde(default)]
    public_keys: Vec<String>,
    #[serde(default)]
    minimum_signatures: u32,
    #[serde(default)]
    cosigner_index: u32,
    #[serde(default)]
    last_used_external_index: u32,
    #[serde(default)]
    last_used_internal_index: u32,
    #[serde(default)]
    ecdsa: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoKaspawalletEncryptedMnemonic {
    cipher: String,
    salt: String,
}

impl IgraArgs {
    pub async fn run(self) -> Result<()> {
        match self.command {
            IgraSubcommand::BuildExit(args) => args.run(),
            IgraSubcommand::VerifyExit(args) => args.run().await,
            IgraSubcommand::SignExit(args) => args.run(),
            IgraSubcommand::InspectExit(args) => args.run(),
            IgraSubcommand::DeriveMsigAddress(args) => args.run(),
            IgraSubcommand::VerifyMsigAddress(args) => args.run(),
            IgraSubcommand::CheckMsigPath(args) => args.run(),
            IgraSubcommand::VerifyBundleIntegral(args) => args.run(),
        }
    }
}

impl BuildExitArgs {
    fn run(self) -> Result<()> {
        if !self.force {
            for path in [&self.out_json, &self.out_hex] {
                if path.exists() {
                    bail!(
                        "output file already exists: {} (pass --force to overwrite)",
                        path.display()
                    );
                }
            }
        }

        let input_json = fs::read_to_string(&self.input)?;
        let input: BuildExitInput = serde_json::from_str(&input_json)?;
        let output = build_unsigned_exit(
            input,
            BuildExitOptions {
                network: self.network,
                tx_id_prefix: self.tx_id_prefix,
                lane_id: self.lane_id,
                mining_timeout: Duration::from_secs(self.mining_timeout_secs),
                max_nonce: self.max_nonce,
                allow_non_igra_lock_script_for_testing: self.allow_non_igra_lock_script_for_testing,
                allow_mass_limit_override_for_testing: self.allow_mass_limit_override_for_testing,
            },
        )?;

        if let Some(parent) = self.out_json.parent().filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        if let Some(parent) = self.out_hex.parent().filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        fs::write(&self.out_json, serde_json::to_string_pretty(&output.manifest)?)?;
        fs::write(&self.out_hex, format!("{}\n", output.wallet_hex))?;

        foundry_common::sh_println!(
            "built IGRA exit tx {} with nonce {}",
            output.manifest.protocol.kaspa_tx_id,
            output.manifest.protocol.nonce
        )?;
        foundry_common::sh_println!("manifest: {}", self.out_json.display())?;
        foundry_common::sh_println!("wallet hex: {}", self.out_hex.display())?;
        Ok(())
    }
}

impl VerifyExitArgs {
    async fn run(self) -> Result<()> {
        let manifest_json = fs::read_to_string(&self.manifest_json)?;
        let manifest = serde_json::from_str(&manifest_json)?;
        let wallet_hex = fs::read_to_string(&self.hex)?;
        let allow_signatures = self.allow_signatures || self.broadcast;
        let require_fully_signed = self.require_fully_signed || self.broadcast;
        let report = verify_unsigned_exit(
            &manifest,
            &wallet_hex,
            VerifyExitOptions {
                allow_signatures,
                require_fully_signed,
                allow_non_igra_lock_script_for_testing: self.allow_non_igra_lock_script_for_testing,
            },
        )?;

        let mut output = serde_json::json!({
            "ok": true,
            "kaspa_tx_id": report.kaspa_tx_id,
            "payload_nonce": report.payload_nonce,
            "inputs": report.input_count,
            "outputs": report.output_count,
            "signed_inputs": report.signed_inputs,
            "fully_signed": report.fully_signed,
        });

        if self.broadcast {
            let kaspa_rpc_url = self
                .kaspa_rpc_url
                .as_deref()
                .ok_or_else(|| eyre!("--kaspa-rpc-url is required with --broadcast"))?;
            let decoded_tx = decode_wallet_transaction(&wallet_hex)?;
            let local_tx_id = decoded_tx.id().to_string();
            if local_tx_id != report.kaspa_tx_id {
                bail!(
                    "decoded wallet txid mismatch before broadcast: expected {}, actual {local_tx_id}",
                    report.kaspa_tx_id
                );
            }
            let submitted_tx_id =
                broadcast_wallet_transaction(&manifest, &wallet_hex, kaspa_rpc_url).await?;
            output.as_object_mut().expect("verify-exit output is an object").insert(
                "broadcast".to_string(),
                serde_json::json!({
                    "submitted": true,
                    "kaspa_rpc_url": kaspa_rpc_url,
                    "kaspa_tx_id": submitted_tx_id,
                }),
            );
        }

        foundry_common::sh_println!("{}", serde_json::to_string_pretty(&output)?)?;
        Ok(())
    }
}

impl SignExitArgs {
    fn run(self) -> Result<()> {
        if self.out_hex.exists() && !self.force {
            bail!(
                "output file already exists: {} (pass --force to overwrite)",
                self.out_hex.display()
            );
        }
        if self.mnemonic_file.is_none() && self.kprv_file.is_none() && self.keys_file.is_none() {
            bail!("provide --keys-file, --mnemonic-file, or --kprv-file");
        }

        let manifest_json = fs::read_to_string(&self.manifest_json)?;
        let manifest = serde_json::from_str(&manifest_json)?;
        let wallet_hex = fs::read_to_string(&self.hex)?;
        let mut mnemonics = Vec::new();
        let mut go_kaspawallet_mnemonics = Vec::new();
        let mut master_private_keys = Vec::new();

        if let Some(path) = self.mnemonic_file.as_ref() {
            let mnemonic = fs::read_to_string(path)?;
            mnemonics.push(mnemonic.trim().to_string());
        }
        if let Some(path) = self.keys_file.as_ref() {
            let keys_json = fs::read_to_string(path)?;
            let password = self.resolve_keys_file_password()?;
            go_kaspawallet_mnemonics
                .extend(decrypt_go_kaspawallet_mnemonics(&keys_json, password.as_bytes())?);
        }
        if let Some(path) = self.kprv_file.as_ref() {
            let kprv = fs::read_to_string(path)?;
            master_private_keys.push(kprv.trim().to_string());
        }

        let output = sign_exit_wallet_transaction(
            &manifest,
            &wallet_hex,
            SignExitOptions {
                mnemonics,
                go_kaspawallet_mnemonics,
                master_private_keys,
                clear_existing_signatures: self.clear_existing_signatures,
                allow_non_igra_lock_script_for_testing: false,
            },
        )?;

        if let Some(parent) = self.out_hex.parent().filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.out_hex, format!("{}\n", output.wallet_hex))?;

        foundry_common::sh_println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "kaspa_tx_id": output.kaspa_tx_id,
                "signed_pairs_added": output.signed_pairs_added,
                "signed_inputs": output.signed_inputs,
                "fully_signed": output.fully_signed,
                "out_hex": self.out_hex,
            }))?
        )?;
        Ok(())
    }

    fn resolve_keys_file_password(&self) -> Result<String> {
        match (&self.keys_password_file, &self.unsafe_keys_password) {
            (Some(_), Some(_)) => {
                bail!("use only one of --keys-password-file or --unsafe-keys-password")
            }
            (Some(path), None) => {
                let password = fs::read_to_string(path)?;
                Ok(password.trim_end_matches(['\r', '\n']).to_string())
            }
            (None, Some(password)) => Ok(password.clone()),
            (None, None) => rpassword::prompt_password("Enter Go kaspawallet keys.json password: ")
                .map_err(Into::into),
        }
    }
}

const GO_KASPAWALLET_DEFAULT_NUM_THREADS: u8 = 8;
const GO_KASPAWALLET_ARGON2_MEMORY_KIB: u32 = 64 * 1024;
const GO_KASPAWALLET_ARGON2_TIME_COST: u32 = 1;
const GO_KASPAWALLET_KEY_LEN: usize = 32;
const GO_KASPAWALLET_XCHACHA_NONCE_LEN: usize = 24;

fn decrypt_go_kaspawallet_mnemonics(keys_json: &str, password: &[u8]) -> Result<Vec<String>> {
    let keys_file: GoKaspawalletKeysFile =
        serde_json::from_str(keys_json).wrap_err("failed to parse Go kaspawallet keys.json")?;

    if keys_file.ecdsa {
        bail!("Go kaspawallet keys.json has ecdsa=true; sign-exit supports Schnorr keys only");
    }
    if keys_file.encrypted_mnemonics.is_empty() {
        bail!("Go kaspawallet keys.json contains no encryptedMnemonics");
    }

    let num_threads = go_kaspawallet_num_threads(&keys_file, password)?;
    keys_file
        .encrypted_mnemonics
        .iter()
        .enumerate()
        .map(|(index, encrypted)| {
            decrypt_go_kaspawallet_mnemonic(num_threads, encrypted, password)
                .wrap_err_with(|| format!("failed to decrypt Go kaspawallet mnemonic #{index}"))
        })
        .collect()
}

fn go_kaspawallet_num_threads(keys_file: &GoKaspawalletKeysFile, password: &[u8]) -> Result<u8> {
    if keys_file.version != 0 {
        return Ok(GO_KASPAWALLET_DEFAULT_NUM_THREADS);
    }

    let first_guess = if keys_file.num_threads == 0 {
        let available = std::thread::available_parallelism().map_or(1, |threads| threads.get());
        available.min(u8::MAX as usize) as u8
    } else {
        keys_file.num_threads
    };

    if decrypt_go_kaspawallet_mnemonic(
        first_guess,
        keys_file
            .encrypted_mnemonics
            .first()
            .ok_or_else(|| eyre!("Go kaspawallet keys.json contains no encryptedMnemonics"))?,
        password,
    )
    .is_ok()
    {
        return Ok(first_guess);
    }

    for num_threads in 1..=u8::MAX {
        if num_threads == first_guess {
            continue;
        }
        if decrypt_go_kaspawallet_mnemonic(
            num_threads,
            keys_file
                .encrypted_mnemonics
                .first()
                .ok_or_else(|| eyre!("Go kaspawallet keys.json contains no encryptedMnemonics"))?,
            password,
        )
        .is_ok()
        {
            return Ok(num_threads);
        }
    }

    bail!("failed to decrypt Go kaspawallet keys.json; wrong password or unsupported key file")
}

fn decrypt_go_kaspawallet_mnemonic(
    num_threads: u8,
    encrypted: &GoKaspawalletEncryptedMnemonic,
    password: &[u8],
) -> Result<String> {
    let cipher =
        hex::decode(&encrypted.cipher).wrap_err("invalid hex in keys.json encrypted cipher")?;
    let salt = hex::decode(&encrypted.salt).wrap_err("invalid hex in keys.json encrypted salt")?;
    if cipher.len() < GO_KASPAWALLET_XCHACHA_NONCE_LEN {
        bail!("keys.json encrypted cipher is shorter than the XChaCha20-Poly1305 nonce");
    }

    let mut key = [0u8; GO_KASPAWALLET_KEY_LEN];
    let params = Params::new(
        GO_KASPAWALLET_ARGON2_MEMORY_KIB,
        GO_KASPAWALLET_ARGON2_TIME_COST,
        num_threads.into(),
        Some(GO_KASPAWALLET_KEY_LEN),
    )
    .map_err(|err| eyre!("invalid Go kaspawallet Argon2id parameters: {err}"))?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password, &salt, &mut key)
        .map_err(|err| eyre!("failed to derive Go kaspawallet encryption key: {err}"))?;

    let aead = XChaCha20Poly1305::new_from_slice(&key)
        .wrap_err("failed to initialize XChaCha20-Poly1305")?;
    let (nonce, ciphertext) = cipher.split_at(GO_KASPAWALLET_XCHACHA_NONCE_LEN);
    let plaintext = aead
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| eyre!("message authentication failed"))?;

    String::from_utf8(plaintext).wrap_err("decrypted Go kaspawallet mnemonic is not UTF-8")
}

impl InspectExitArgs {
    fn run(self) -> Result<()> {
        let wallet_hex = fs::read_to_string(&self.hex)?;
        let report = inspect_exit_wallet_transaction(&wallet_hex, &self.network)?;
        foundry_common::sh_println!("{}", serde_json::to_string_pretty(&report)?)?;
        Ok(())
    }
}

impl DeriveMsigAddressArgs {
    fn run(self) -> Result<()> {
        let input = self.into_multisig_address_input()?;
        let report = derive_multisig_address(input)?;
        foundry_common::sh_println!("{}", serde_json::to_string_pretty(&report)?)?;
        Ok(())
    }

    fn into_multisig_address_input(self) -> Result<MultisigAddressInput> {
        let mut public_keys = self.kpubs;
        let mut minimum_signatures = self.minimum_signatures;
        let mut ecdsa = self.ecdsa;

        if let Some(keys_file) = self.keys_file {
            if !public_keys.is_empty() {
                bail!("use either --keys-file or repeated --kpub arguments, not both");
            }
            let keys_json = fs::read_to_string(&keys_file)?;
            let keys: KaspawalletPublicKeysFile = serde_json::from_str(&keys_json)?;
            public_keys = keys.public_keys.ok_or_else(|| {
                eyre::eyre!("keys file {} is missing publicKeys", keys_file.display())
            })?;
            if minimum_signatures.is_none() {
                minimum_signatures = keys.minimum_signatures;
            }
            ecdsa = ecdsa || keys.ecdsa;
        }

        Ok(MultisigAddressInput {
            network: self.network,
            derivation_path: self.path,
            minimum_signatures: minimum_signatures
                .ok_or_else(|| eyre::eyre!("--minimum-signatures is required"))?,
            extended_public_keys: public_keys,
            ecdsa,
        })
    }
}

impl VerifyMsigAddressArgs {
    fn run(self) -> Result<()> {
        let expected_address = self.address;
        let input = self.derive.into_multisig_address_input()?;
        let report = verify_multisig_address(input, &expected_address)?;
        foundry_common::sh_println!("{}", serde_json::to_string_pretty(&report)?)?;
        if !report.matches {
            bail!(
                "multisig address mismatch: expected {}, actual {}",
                report.expected_address,
                report.actual_address
            );
        }
        Ok(())
    }
}

impl CheckMsigPathArgs {
    fn run(self) -> Result<()> {
        let report = check_multisig_derivation_path(&self.path)?;
        foundry_common::sh_println!("{}", serde_json::to_string_pretty(&report)?)?;
        Ok(())
    }
}

impl VerifyBundleIntegralArgs {
    fn run(self) -> Result<()> {
        let manifest_json = fs::read_to_string(&self.manifest_json)?;
        let manifest: serde_json::Value = serde_json::from_str(&manifest_json)?;
        let report = verify_bundle_integral(&manifest)?;
        foundry_common::sh_println!("{}", serde_json::to_string_pretty(&report)?)?;
        if !report.matches {
            bail!(
                "bundle integral mismatch: expected {}, computed {}",
                report.expected_value,
                report.computed_value
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const TEST_PASSWORD: &[u8] = b"test-password";

    fn encrypted_mnemonic_json(version: u32, num_threads: u8) -> serde_json::Value {
        let encrypted = encrypt_go_kaspawallet_test_mnemonic(num_threads);
        serde_json::json!({
            "version": version,
            "numThreads": if version == 0 { 1 } else { num_threads },
            "encryptedMnemonics": [encrypted],
            "publicKeys": [],
            "minimumSignatures": 2,
            "cosignerIndex": 0,
            "lastUsedExternalIndex": 0,
            "lastUsedInternalIndex": 0,
            "ecdsa": false
        })
    }

    fn encrypt_go_kaspawallet_test_mnemonic(num_threads: u8) -> serde_json::Value {
        let salt = [7u8; 16];
        let nonce = [9u8; GO_KASPAWALLET_XCHACHA_NONCE_LEN];
        let mut key = [0u8; GO_KASPAWALLET_KEY_LEN];
        let params = Params::new(
            GO_KASPAWALLET_ARGON2_MEMORY_KIB,
            GO_KASPAWALLET_ARGON2_TIME_COST,
            num_threads.into(),
            Some(GO_KASPAWALLET_KEY_LEN),
        )
        .unwrap();
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(TEST_PASSWORD, &salt, &mut key)
            .unwrap();
        let aead = XChaCha20Poly1305::new_from_slice(&key).unwrap();
        let mut cipher = nonce.to_vec();
        cipher.extend(aead.encrypt(XNonce::from_slice(&nonce), TEST_MNEMONIC.as_bytes()).unwrap());

        serde_json::json!({
            "cipher": hex::encode(cipher),
            "salt": hex::encode(salt)
        })
    }

    #[test]
    fn decrypts_go_kaspawallet_v1_keys_json() {
        let keys_json = encrypted_mnemonic_json(1, GO_KASPAWALLET_DEFAULT_NUM_THREADS);
        let mnemonics =
            decrypt_go_kaspawallet_mnemonics(&keys_json.to_string(), TEST_PASSWORD).unwrap();
        assert_eq!(mnemonics, vec![TEST_MNEMONIC]);
    }

    #[test]
    fn detects_go_kaspawallet_v0_num_threads() {
        let keys_json = encrypted_mnemonic_json(0, 2);
        let mnemonics =
            decrypt_go_kaspawallet_mnemonics(&keys_json.to_string(), TEST_PASSWORD).unwrap();
        assert_eq!(mnemonics, vec![TEST_MNEMONIC]);
    }
}
