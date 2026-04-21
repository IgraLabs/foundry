use clap::{Args, Subcommand};
use eyre::{Result, bail};
use foundry_common::igra_exit::{
    BuildExitInput, BuildExitOptions, VerifyExitOptions, build_unsigned_exit, verify_unsigned_exit,
};
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

    /// Allow non-official IGRA locking scripts. Testing only; never use for bridge exits.
    #[arg(long)]
    pub allow_non_igra_lock_script_for_testing: bool,
}

impl IgraArgs {
    pub async fn run(self) -> Result<()> {
        match self.command {
            IgraSubcommand::BuildExit(args) => args.run(),
            IgraSubcommand::VerifyExit(args) => args.run(),
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
    fn run(self) -> Result<()> {
        let manifest_json = fs::read_to_string(&self.manifest_json)?;
        let manifest = serde_json::from_str(&manifest_json)?;
        let wallet_hex = fs::read_to_string(&self.hex)?;
        let report = verify_unsigned_exit(
            &manifest,
            &wallet_hex,
            VerifyExitOptions {
                allow_signatures: self.allow_signatures,
                require_fully_signed: self.require_fully_signed,
                allow_non_igra_lock_script_for_testing: self.allow_non_igra_lock_script_for_testing,
            },
        )?;

        foundry_common::sh_println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "kaspa_tx_id": report.kaspa_tx_id,
                "payload_nonce": report.payload_nonce,
                "inputs": report.input_count,
                "outputs": report.output_count,
                "signed_inputs": report.signed_inputs,
                "fully_signed": report.fully_signed,
            }))?
        )?;
        Ok(())
    }
}
