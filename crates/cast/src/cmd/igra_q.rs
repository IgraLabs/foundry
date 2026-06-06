//! Cast helpers for IGRA Falcon-L5 q-zone transactions.

use crate::SimpleCast;
use alloy_primitives::{Address, Bytes, U256, hex, keccak256};
use clap::Parser;
use eyre::{Result, WrapErr};
use foundry_cli::{opts::RpcOpts, utils::LoadConfig};
use foundry_common::{
    igra_q_tx::{
        IgraFalconL5TransactionRequest, encode_q_entry_payload, falcon_l5_pubkey_to_address,
        generate_falcon_l5_keypair, generate_falcon_l5_keypair_from_seed, parse_private_key_hex,
    },
    provider::igra_transport::{
        IgraPayloadKind, IgraPayloadSubmitter, IgraSubmitRequest, InProcessKaspaPayloadSubmitter,
    },
    shell,
};
use foundry_wallets::WalletOpts;
use std::str::FromStr;

const DEFAULT_IGRA_MINING_TIMEOUT_SECS: u64 = 120;

/// CLI arguments for `cast igra-q-address`.
#[derive(Debug, Parser)]
pub struct IgraQAddressArgs {
    /// Falcon-L5 private key, hex-encoded.
    #[arg(long = "private-key-q", env = "IGRA_Q_PRIVATE_KEY", hide_env_values = true)]
    private_key_q: String,
}

impl IgraQAddressArgs {
    pub fn run(self) -> Result<()> {
        let private_key = parse_private_key_hex(&self.private_key_q)?;
        let public_key = private_key.public_key();
        let address = falcon_l5_pubkey_to_address(&public_key);

        if shell::is_json() {
            sh_println!(
                "{}",
                serde_json::json!({
                    "address": format!("{address:#x}"),
                    "public_key": hex::encode_prefixed(public_key.as_bytes()),
                })
            )?;
        } else {
            sh_println!("{address:#x}")?;
        }

        Ok(())
    }
}

/// CLI arguments for `cast igra-q-keygen`.
#[derive(Debug, Parser)]
pub struct IgraQKeygenArgs {
    /// Deterministic seed for test vectors, hex-encoded. Omit for system entropy.
    #[arg(long = "seed-hex", hide = true)]
    seed_hex: Option<String>,
}

impl IgraQKeygenArgs {
    pub fn run(self) -> Result<()> {
        let (private_key, public_key) = if let Some(seed_hex) = self.seed_hex {
            let seed = decode_hex(&seed_hex, "seed-hex")?;
            generate_falcon_l5_keypair_from_seed(&seed)?
        } else {
            generate_falcon_l5_keypair()?
        };
        let address = falcon_l5_pubkey_to_address(&public_key);
        let private_key_hex = hex::encode_prefixed(private_key.as_bytes());
        let public_key_hex = hex::encode_prefixed(public_key.as_bytes());

        if shell::is_json() {
            sh_println!(
                "{}",
                serde_json::json!({
                    "private_key": private_key_hex,
                    "public_key": public_key_hex,
                    "address": format!("{address:#x}"),
                })
            )?;
        } else {
            sh_println!("private_key={private_key_hex}")?;
            sh_println!("public_key={public_key_hex}")?;
            sh_println!("address={address:#x}")?;
        }

        Ok(())
    }
}

/// CLI arguments for `cast igra-q-mktx`.
#[derive(Debug, Parser)]
pub struct IgraQMakeTxArgs {
    /// Falcon-L5 private key, hex-encoded.
    #[arg(long = "private-key-q", env = "IGRA_Q_PRIVATE_KEY", hide_env_values = true)]
    private_key_q: String,

    /// q-ethrex chain ID.
    #[arg(long)]
    chain_id: u64,

    /// q-zone sender nonce.
    #[arg(long)]
    nonce: u64,

    /// q-zone max priority fee per gas.
    #[arg(long, default_value_t = 0)]
    max_priority_fee_per_gas: u64,

    /// q-zone max fee per gas.
    #[arg(long)]
    max_fee_per_gas: u64,

    /// q-zone gas limit.
    #[arg(long)]
    gas_limit: u64,

    /// Destination address. Omit for contract creation.
    #[arg(long)]
    to: Option<Address>,

    /// Value in q-zone wei-style units.
    #[arg(long, default_value = "0")]
    value: String,

    /// Raw hex-encoded calldata. Used instead of [SIG] and [ARGS].
    #[arg(long, conflicts_with_all = &["sig", "args"])]
    data: Option<String>,

    /// Function signature to ABI-encode as calldata.
    sig: Option<String>,

    /// Function arguments for [SIG].
    #[arg(allow_negative_numbers = true)]
    args: Vec<String>,
}

impl IgraQMakeTxArgs {
    pub fn run(self) -> Result<()> {
        let private_key = parse_private_key_hex(&self.private_key_q)?;
        let data = q_tx_data(self.data, self.sig, self.args)?;
        let tx = IgraFalconL5TransactionRequest {
            chain_id: self.chain_id,
            nonce: self.nonce,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas,
            max_fee_per_gas: self.max_fee_per_gas,
            gas_limit: self.gas_limit,
            to: self.to,
            value: parse_u256(&self.value)?,
            data,
        };
        let signed = tx.sign(&private_key)?;
        let raw_tx = hex::encode_prefixed(&signed.raw_tx);

        if shell::is_json() {
            sh_println!(
                "{}",
                serde_json::json!({
                    "raw": raw_tx,
                    "sender": format!("{:#x}", signed.sender),
                    "signing_hash": format!("{:#x}", signed.signing_hash),
                    "auth_len": signed.auth.len(),
                })
            )?;
        } else {
            sh_println!("{raw_tx}")?;
        }

        Ok(())
    }
}

/// CLI arguments for `cast igra-q-entry`.
#[derive(Debug, Parser)]
pub struct IgraQEntryArgs {
    /// q-zone recipient address.
    #[arg(long)]
    address: Address,

    /// Amount to credit, in sompi, before q-ethrex applies its iKAS conversion.
    #[arg(long)]
    amount_sompi: u64,

    /// Kaspa script public key that receives the Entry deposit output.
    #[arg(long, env = "IGRA_LOCK_SCRIPT_PUBKEY")]
    entry_lock_script_pubkey: Option<String>,

    #[command(flatten)]
    rpc: RpcOpts,

    #[command(flatten)]
    wallet: WalletOpts,
}

impl IgraQEntryArgs {
    pub async fn run(self) -> Result<()> {
        let mut config = self.rpc.load_config()?;
        if !config.igra.enabled {
            eyre::bail!("IGRA mode is not enabled in the active Foundry config");
        }
        self.wallet.apply_igra_kaspa_wallet_overrides(&mut config);
        let entry_lock_script_pubkey = self
            .entry_lock_script_pubkey
            .or_else(|| config.igra.entry_lock_script_pubkey.clone())
            .ok_or_else(|| {
                eyre::eyre!(
                    "IGRA q Entry requires `entry_lock_script_pubkey` in [igra] or --entry-lock-script-pubkey"
                )
            })?;

        let entry_payload = encode_q_entry_payload(self.address, self.amount_sompi);
        let entry_hash = format!("0x{}", hex::encode(keccak256(entry_payload)));
        let submit_request = IgraSubmitRequest {
            l2_tx_hash: entry_hash.clone(),
            raw_tx_bytes: entry_payload.to_vec(),
            payload_kind: IgraPayloadKind::FalconL5Entry,
            tx_id_prefix: required_igra_string("tx_id_prefix", config.igra.tx_id_prefix)?,
            mining_timeout_secs: config
                .igra
                .mining_timeout_secs
                .unwrap_or(DEFAULT_IGRA_MINING_TIMEOUT_SECS),
            kaspa_rpc_url: config.igra.kaspa_rpc_url,
            kaspa_network: config.igra.kaspa_network,
            payload_compression: None,
            logic_zone: Some("falcon-l5".to_string()),
            entry_lock_script_pubkey: Some(entry_lock_script_pubkey),
            kaspa_wallet: config.igra.kaspa_wallet,
        };

        let submitter = InProcessKaspaPayloadSubmitter::default();
        let result = submitter.submit_payload(&submit_request).await.map_err(eyre::Report::msg)?;

        sh_println!(
            "{}",
            serde_json::json!({
                "entry_hash": entry_hash,
                "q_address": format!("{:#x}", self.address),
                "amount_sompi": self.amount_sompi,
                "kaspa_tx_id": result.kaspa_tx_id,
                "payload_nonce": result.payload_nonce,
            })
        )?;

        Ok(())
    }
}

fn q_tx_data(data: Option<String>, sig: Option<String>, args: Vec<String>) -> Result<Bytes> {
    if let Some(data) = data {
        return Ok(Bytes::from(decode_hex(&data, "data")?));
    }

    let Some(sig) = sig else {
        return Ok(Bytes::new());
    };

    let encoded = SimpleCast::calldata_encode(sig, &args)?;
    Ok(Bytes::from(decode_hex(&encoded, "calldata")?))
}

fn decode_hex(value: &str, field: &str) -> Result<Vec<u8>> {
    let value = value.trim().trim_start_matches("0x");
    hex::decode(value).wrap_err_with(|| format!("invalid hex for {field}"))
}

fn parse_u256(value: &str) -> Result<U256> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix("0x") {
        return U256::from_str_radix(hex, 16).wrap_err("invalid hex value");
    }
    U256::from_str(value).wrap_err("invalid decimal value")
}

fn required_igra_string(field: &'static str, value: Option<String>) -> Result<String> {
    let value = value.ok_or_else(|| eyre::eyre!("IGRA config error: `{field}` is required"))?;
    if value.trim().is_empty() {
        eyre::bail!("IGRA config error: `{field}` is required");
    }
    Ok(value)
}
