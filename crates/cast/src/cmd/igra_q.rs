//! Cast helpers for IGRA Falcon-L5 q-zone transactions.

use crate::SimpleCast;
use alloy_primitives::{Address, Bytes, U256, hex, keccak256};
use alloy_signer_local::coins_bip39::{English, Mnemonic};
use clap::Parser;
use eyre::{Result, WrapErr};
use foundry_cli::{opts::RpcOpts, utils::LoadConfig};
use foundry_common::{
    igra_q_tx::{
        FalconL5PrivateKey, IgraFalconL5TransactionRequest, encode_q_entry_payload,
        falcon_l5_pubkey_to_address, generate_falcon_l5_keypair,
        generate_falcon_l5_keypair_from_seed, parse_private_key_hex,
    },
    provider::igra_transport::{
        IgraPayloadKind, IgraPayloadSubmitter, IgraSubmitRequest, InProcessKaspaPayloadSubmitter,
    },
    shell,
};
use foundry_wallets::WalletOpts;
use hmac::{Hmac, Mac};
use sha2::Sha512;
use std::{fs, path::Path, str::FromStr};

const DEFAULT_IGRA_MINING_TIMEOUT_SECS: u64 = 120;
const Q_MNEMONIC_DOMAIN: &[u8] = b"IGRA_FALCON_L5_Q_MNEMONIC_V1";

type HmacSha512 = Hmac<Sha512>;

/// CLI arguments for `cast igra-q-address`.
#[derive(Debug, Parser)]
pub struct IgraQAddressArgs {
    /// Falcon-L5 private key, hex-encoded.
    #[arg(long = "private-key-q", env = "IGRA_Q_PRIVATE_KEY", hide_env_values = true)]
    private_key_q: Option<String>,

    /// BIP39 mnemonic phrase or file path for the Falcon-L5 q-zone key.
    #[arg(long = "mnemonic-q", env = "IGRA_Q_MNEMONIC", hide_env_values = true)]
    mnemonic_q: Option<String>,

    /// Optional BIP39 passphrase for --mnemonic-q.
    #[arg(
        long = "mnemonic-passphrase-q",
        env = "IGRA_Q_MNEMONIC_PASSPHRASE",
        hide_env_values = true
    )]
    mnemonic_passphrase_q: Option<String>,

    /// q-zone mnemonic account index.
    #[arg(long = "mnemonic-index-q", env = "IGRA_Q_MNEMONIC_INDEX", default_value_t = 0)]
    mnemonic_index_q: u32,
}

impl IgraQAddressArgs {
    pub fn run(self) -> Result<()> {
        let private_key = resolve_q_private_key(
            self.private_key_q.as_deref(),
            self.mnemonic_q.as_deref(),
            self.mnemonic_passphrase_q.as_deref(),
            self.mnemonic_index_q,
            true,
        )?;
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
    #[arg(long = "seed-hex", hide = true, conflicts_with = "mnemonic_q")]
    seed_hex: Option<String>,

    /// BIP39 mnemonic phrase or file path for deterministic q-zone key generation.
    #[arg(long = "mnemonic-q", env = "IGRA_Q_MNEMONIC", hide_env_values = true)]
    mnemonic_q: Option<String>,

    /// Optional BIP39 passphrase for --mnemonic-q.
    #[arg(
        long = "mnemonic-passphrase-q",
        env = "IGRA_Q_MNEMONIC_PASSPHRASE",
        hide_env_values = true
    )]
    mnemonic_passphrase_q: Option<String>,

    /// q-zone mnemonic account index.
    #[arg(long = "mnemonic-index-q", env = "IGRA_Q_MNEMONIC_INDEX", default_value_t = 0)]
    mnemonic_index_q: u32,
}

impl IgraQKeygenArgs {
    pub fn run(self) -> Result<()> {
        let (private_key, public_key) = if let Some(seed_hex) = self.seed_hex {
            if self.mnemonic_passphrase_q.is_some() {
                eyre::bail!("--mnemonic-passphrase-q requires --mnemonic-q");
            }
            let seed = decode_hex(&seed_hex, "seed-hex")?;
            generate_falcon_l5_keypair_from_seed(&seed)?
        } else if let Some(mnemonic) = self.mnemonic_q.as_deref() {
            let private_key = private_key_from_q_mnemonic(
                mnemonic,
                self.mnemonic_passphrase_q.as_deref(),
                self.mnemonic_index_q,
                true,
            )?;
            let public_key = private_key.public_key();
            (private_key, public_key)
        } else {
            if self.mnemonic_passphrase_q.is_some() {
                eyre::bail!("--mnemonic-passphrase-q requires --mnemonic-q");
            }
            generate_falcon_l5_keypair()?
        };
        let address = falcon_l5_pubkey_to_address(&public_key);
        let private_key_hex = hex::encode_prefixed(private_key.as_bytes());
        let public_key_hex = hex::encode_prefixed(public_key.as_bytes());

        if shell::is_json() {
            let mut output = serde_json::json!({
                "private_key": private_key_hex,
                "public_key": public_key_hex,
                "address": format!("{address:#x}"),
            });
            if self.mnemonic_q.is_some() {
                output["mnemonic_index_q"] = serde_json::json!(self.mnemonic_index_q);
            }
            sh_println!("{output}")?;
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
    private_key_q: Option<String>,

    /// BIP39 mnemonic phrase or file path for the Falcon-L5 q-zone signing key.
    #[arg(long = "mnemonic-q", env = "IGRA_Q_MNEMONIC", hide_env_values = true)]
    mnemonic_q: Option<String>,

    /// Optional BIP39 passphrase for --mnemonic-q.
    #[arg(
        long = "mnemonic-passphrase-q",
        env = "IGRA_Q_MNEMONIC_PASSPHRASE",
        hide_env_values = true
    )]
    mnemonic_passphrase_q: Option<String>,

    /// q-zone mnemonic account index.
    #[arg(long = "mnemonic-index-q", env = "IGRA_Q_MNEMONIC_INDEX", default_value_t = 0)]
    mnemonic_index_q: u32,

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
        let private_key = resolve_q_private_key(
            self.private_key_q.as_deref(),
            self.mnemonic_q.as_deref(),
            self.mnemonic_passphrase_q.as_deref(),
            self.mnemonic_index_q,
            true,
        )?;
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

/// CLI arguments for `cast igra-entry`.
#[derive(Debug, Parser)]
pub struct IgraEntryArgs {
    /// Canonical first-zone recipient address.
    #[arg(long)]
    address: Address,

    /// Amount to credit, in sompi, before ethrex applies its iKAS conversion.
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

impl IgraEntryArgs {
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
                    "IGRA Entry requires `entry_lock_script_pubkey` in [igra] or --entry-lock-script-pubkey"
                )
            })?;

        let entry_payload = encode_q_entry_payload(self.address, self.amount_sompi);
        let entry_hash = format!("0x{}", hex::encode(keccak256(entry_payload)));
        let submit_request = IgraSubmitRequest {
            l2_tx_hash: entry_hash.clone(),
            raw_tx_bytes: entry_payload.to_vec(),
            payload_kind: IgraPayloadKind::CanonicalEntry,
            tx_id_prefix: required_igra_string("tx_id_prefix", config.igra.tx_id_prefix)?,
            lane_id: required_igra_string("lane_id", config.igra.lane_id)?,
            mining_timeout_secs: config
                .igra
                .mining_timeout_secs
                .unwrap_or(DEFAULT_IGRA_MINING_TIMEOUT_SECS),
            kaspa_rpc_url: config.igra.kaspa_rpc_url,
            kaspa_network: config.igra.kaspa_network,
            payload_compression: None,
            logic_zone: Some("canonical".to_string()),
            entry_lock_script_pubkey: Some(entry_lock_script_pubkey),
            kaspa_wallet: config.igra.kaspa_wallet,
        };

        let submitter = InProcessKaspaPayloadSubmitter::default();
        let result = submitter.submit_payload(&submit_request).await.map_err(eyre::Report::msg)?;

        sh_println!(
            "{}",
            serde_json::json!({
                "entry_hash": entry_hash,
                "address": format!("{:#x}", self.address),
                "amount_sompi": self.amount_sompi,
                "kaspa_tx_id": result.kaspa_tx_id,
                "payload_nonce": result.payload_nonce,
            })
        )?;

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
            lane_id: required_igra_string("lane_id", config.igra.lane_id)?,
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

fn resolve_q_private_key(
    private_key_q: Option<&str>,
    mnemonic_q: Option<&str>,
    mnemonic_passphrase_q: Option<&str>,
    mnemonic_index_q: u32,
    warn_on_short_mnemonic: bool,
) -> Result<FalconL5PrivateKey> {
    match (private_key_q, mnemonic_q) {
        (Some(_), Some(_)) => {
            eyre::bail!("provide only one q key source: --private-key-q or --mnemonic-q")
        }
        (Some(private_key), None) => {
            if mnemonic_passphrase_q.is_some() {
                eyre::bail!("--mnemonic-passphrase-q requires --mnemonic-q");
            }
            parse_private_key_hex(private_key).map_err(eyre::Report::from)
        }
        (None, Some(mnemonic)) => private_key_from_q_mnemonic(
            mnemonic,
            mnemonic_passphrase_q,
            mnemonic_index_q,
            warn_on_short_mnemonic,
        ),
        (None, None) => {
            eyre::bail!("missing q key source: provide --private-key-q or --mnemonic-q")
        }
    }
}

fn private_key_from_q_mnemonic(
    mnemonic_q: &str,
    passphrase: Option<&str>,
    index: u32,
    warn_on_short_mnemonic: bool,
) -> Result<FalconL5PrivateKey> {
    let (q_seed, word_count) = q_seed_from_mnemonic(mnemonic_q, passphrase, index)?;
    if warn_on_short_mnemonic {
        warn_if_short_q_mnemonic(word_count)?;
    }
    let (private_key, _) = generate_falcon_l5_keypair_from_seed(&q_seed)?;
    Ok(private_key)
}

fn q_seed_from_mnemonic(
    mnemonic_q: &str,
    passphrase: Option<&str>,
    index: u32,
) -> Result<([u8; 64], usize)> {
    let phrase = resolve_mnemonic_phrase(mnemonic_q)?;
    let word_count = phrase.split_whitespace().count();
    let mnemonic =
        Mnemonic::<English>::new_from_phrase(&phrase).wrap_err("invalid q-zone BIP39 mnemonic")?;
    let bip39_seed = mnemonic.to_seed(passphrase).wrap_err("failed to derive q-zone BIP39 seed")?;

    let mut mac =
        HmacSha512::new_from_slice(Q_MNEMONIC_DOMAIN).expect("HMAC-SHA512 accepts any key length");
    mac.update(&bip39_seed);
    mac.update(&index.to_be_bytes());
    let bytes = mac.finalize().into_bytes();
    let mut q_seed = [0u8; 64];
    q_seed.copy_from_slice(&bytes);

    Ok((q_seed, word_count))
}

fn resolve_mnemonic_phrase(mnemonic_q: &str) -> Result<String> {
    let phrase = if Path::new(mnemonic_q).is_file() {
        fs::read_to_string(mnemonic_q).wrap_err("failed to read q-zone mnemonic file")?
    } else {
        mnemonic_q.to_string()
    };
    Ok(phrase.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn warn_if_short_q_mnemonic(word_count: usize) -> Result<()> {
    if word_count == 12 {
        sh_warn!(
            "q-zone mnemonic warning: 12-word mnemonics provide about 128 bits of classical entropy and a reduced quantum brute-force margin; use a 24-word mnemonic for q-zone accounts."
        )?;
    } else if word_count < 24 {
        sh_warn!(
            "q-zone mnemonic warning: {word_count}-word mnemonics are below the recommended 24 words for q-zone accounts."
        )?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    const TWELVE_WORD_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn q_mnemonic_derivation_is_deterministic() {
        let key_a = private_key_from_q_mnemonic(TWELVE_WORD_MNEMONIC, None, 0, false).unwrap();
        let key_b = private_key_from_q_mnemonic(TWELVE_WORD_MNEMONIC, None, 0, false).unwrap();

        assert_eq!(key_a.to_bytes(), key_b.to_bytes());
        assert_eq!(
            falcon_l5_pubkey_to_address(&key_a.public_key()),
            falcon_l5_pubkey_to_address(&key_b.public_key())
        );
    }

    #[test]
    fn q_mnemonic_index_and_passphrase_separate_keys() {
        let base = private_key_from_q_mnemonic(TWELVE_WORD_MNEMONIC, None, 0, false).unwrap();
        let different_index =
            private_key_from_q_mnemonic(TWELVE_WORD_MNEMONIC, None, 1, false).unwrap();
        let different_passphrase =
            private_key_from_q_mnemonic(TWELVE_WORD_MNEMONIC, Some("q-pass"), 0, false).unwrap();

        assert_ne!(base.to_bytes(), different_index.to_bytes());
        assert_ne!(base.to_bytes(), different_passphrase.to_bytes());
    }

    #[test]
    fn q_mnemonic_derivation_normalizes_whitespace() {
        let compact = q_seed_from_mnemonic(TWELVE_WORD_MNEMONIC, None, 7).unwrap();
        let spaced = q_seed_from_mnemonic(
            " abandon  abandon abandon abandon abandon abandon\nabandon abandon abandon abandon abandon about ",
            None,
            7,
        )
        .unwrap();

        assert_eq!(compact.0, spaced.0);
        assert_eq!(compact.1, 12);
        assert_eq!(spaced.1, 12);
    }

    #[test]
    fn q_mnemonic_rejects_invalid_phrase() {
        let err = q_seed_from_mnemonic("not a valid q mnemonic", None, 0).unwrap_err();
        assert!(err.to_string().contains("invalid q-zone BIP39 mnemonic"));
    }
}
