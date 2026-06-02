use clap::{Args, Subcommand};
use eyre::{Result, bail, eyre};
use foundry_common::igra_bundle::verify_bundle_integral;
use foundry_common::igra_exit::{
    BuildExitInput, BuildExitOptions, MultisigAddressInput, VerifyExitOptions,
    broadcast_wallet_transaction, build_unsigned_exit, check_multisig_derivation_path,
    decode_wallet_transaction, derive_multisig_address, verify_multisig_address,
    verify_unsigned_exit,
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

impl IgraArgs {
    pub async fn run(self) -> Result<()> {
        match self.command {
            IgraSubcommand::BuildExit(args) => args.run(),
            IgraSubcommand::VerifyExit(args) => args.run().await,
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
