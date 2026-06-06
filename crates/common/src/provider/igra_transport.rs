//! IGRA-aware transport wrapper.

use crate::igra_store::{
    IGRA_NONCE_GAP_ERROR_CODE, IGRA_NONCE_REPLACEMENT_CANDIDATE_ERROR_CODE, IgraStore,
    IgraStoreConfig, IgraStoreError, NonceOrdering, TxLifecycleState, TxLifecycleUpdate,
};
use alloy_consensus::{
    Transaction as AlloyTransaction, TxEnvelope, transaction::SignerRecoverable,
};
use alloy_json_rpc::{
    Id, Request, RequestPacket, Response, ResponsePacket, ResponsePayload, SerializedRequest,
};
use alloy_primitives::{B256, hex, utils::keccak256};
use alloy_provider::network::eip2718::Decodable2718;
use alloy_signer_local::PrivateKeySigner;
use alloy_transport::{TransportError, TransportErrorKind, TransportFut};
use async_trait::async_trait;
use foundry_config::{Config, IgraKaspaWalletConfig};
use kaspa_addresses::{
    Address as KaspaAddress, Prefix as KaspaAddressPrefix, Version as KaspaAddressVersion,
};
use kaspa_bip32::secp256k1::SecretKey as KaspaSecretKey;
use kaspa_bip32::{
    ChildNumber as KaspaChildNumber, DerivationPath as KaspaDerivationPath,
    ExtendedPrivateKey as KaspaExtendedPrivateKey, Language as KaspaLanguage,
    Mnemonic as KaspaMnemonic,
};
use kaspa_consensus_core::{
    config::params::Params as KaspaParams,
    mass::MassCalculator as KaspaMassCalculator,
    network::NetworkType as KaspaNetworkType,
    sign::{sign_with_multiple_v2 as kaspa_sign_with_multiple_v2, verify as kaspa_verify},
    subnets::SubnetworkId,
    tx::{
        ScriptPublicKey, SignableTransaction as KaspaSignableTransaction,
        Transaction as KaspaTransaction, TransactionInput as KaspaTransactionInput,
        TransactionOutput as KaspaTransactionOutput, UtxoEntry as KaspaUtxoEntry,
    },
};
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::{RpcTransaction, RpcUtxosByAddressesEntry, api::rpc::RpcApi};
use kaspa_txscript::pay_to_address_script;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex as TokioMutex;
use tower::Service;
use tracing::{info, warn};

/// Error returned when unsupported send methods are used in IGRA mode.
pub const IGRA_SEND_TRANSACTION_UNSUPPORTED_ERROR: &str =
    "IGRA mode requires raw signed transactions; eth_sendTransaction* is not supported";
/// Error returned when unsupported signed transaction envelopes are used in IGRA mode.
pub const IGRA_EIP4844_UNSUPPORTED_ERROR: &str =
    "IGRA unsupported transaction type: EIP-4844 (blob transactions)";
/// Error returned when unsupported EIP-7702 envelopes are used in IGRA mode.
pub const IGRA_EIP7702_UNSUPPORTED_ERROR: &str = "IGRA unsupported transaction type: EIP-7702";
/// Error code returned when payload prefix mining times out.
pub const IGRA_MINING_TIMEOUT_ERROR_CODE: &str = "IGRA_MINING_001";
/// Error code returned when the embedded L2 tx exceeds IGRA payload size limits.
pub const IGRA_L2DATA_TOO_LARGE_ERROR_CODE: &str = "IGRA_PAYLOAD_001";
/// Error returned when no usable Kaspa key material is available for IGRA submission.
pub const IGRA_KEY_RESOLUTION_ERROR: &str = "IGRA key resolution error: cannot derive Kaspa key from current EVM signer; provide --private-key-kaspa or --mnemonic-kaspa";
/// Error returned when a raw transaction is not valid for the Falcon-L5 q-zone transport mode.
pub const IGRA_Q_RAW_TRANSACTION_ERROR: &str = "IGRA q-zone raw transaction error";

const DEFAULT_MINING_TIMEOUT_SECS: u64 = 120;
const MAX_STANDARD_KASPA_TX_MASS: u64 = 100_000;
// Per IGRA Transaction Protocol: L2Data (raw EVM tx bytes) must not exceed this.
const IGRA_MAX_L2DATA_BYTES: usize = 24_800;
const IGRA_VERSION: u8 = 0x9;
const IGRA_CANONICAL_RAW_TX_TYPE: u8 = 0x4;
const IGRA_LOGIC_ZONE_TX_TYPE: u8 = 0x0f;
const IGRA_LOGIC_ZONE_HEADER_SIZE: usize = 4;
const IGRA_Q_RAW_TX_MAX_BYTES: usize = IGRA_MAX_L2DATA_BYTES - IGRA_LOGIC_ZONE_HEADER_SIZE;
const IGRA_Q_L2DATA_MAX_BYTES: usize = IGRA_MAX_L2DATA_BYTES;
const IGRA_Q_ENVELOPE_VERSION: u8 = 0x01;
const IGRA_FALCON_L5_Q_ZONE_ID: u16 = 0x0002;
const IGRA_Q_ENTRY_TX_TYPE: u8 = 0x02;
const IGRA_Q_RAW_TX_TYPE: u8 = 0x04;
const IGRA_FALCON_L5_TX_TYPE: u8 = 0x7c;
const IGRA_Q_ENTRY_BYTES: usize = 28;
const IGRA_Q_ENTRY_ADDRESS_BYTES: usize = 20;
const CACHE_TTL_SECS: u64 = 20;
const BASE_SUBMIT_FEE_SOMPI: u64 = 200_000;
const INITIAL_FEE_PER_PAYLOAD_BYTE_SOMPI: u64 = 200;
const EXTRA_INPUT_FEE_SOMPI: u64 = 100_000;
const CURRENT_KASPA_MIN_RELAY_FEE_PER_KG_SOMPI: u64 = 100_000;
const FEE_SELECTION_ATTEMPTS: usize = 4;
const MIN_CHANGE_SOMPI: u64 = 1_000;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Runtime IGRA settings required by the transport write path.
#[derive(Clone, Debug, Default)]
pub struct IgraTransportConfig {
    pub tx_id_prefix: Option<String>,
    pub mining_timeout_secs: Option<u64>,
    pub kaspa_rpc_url: Option<String>,
    pub kaspa_network: Option<String>,
    /// Payload compression mode for L2Data inside the Kaspa payload.
    pub payload_compression: Option<String>,
    /// Target IGRA logic zone for raw transaction submission.
    pub logic_zone: Option<String>,
    pub kaspa_wallet: IgraKaspaWalletConfig,
}

/// Payload submission request forwarded to a Kaspa submitter implementation.
#[derive(Clone, Debug)]
pub struct IgraSubmitRequest {
    pub l2_tx_hash: String,
    pub raw_tx_bytes: Vec<u8>,
    pub payload_kind: IgraPayloadKind,
    pub tx_id_prefix: String,
    pub mining_timeout_secs: u64,
    pub kaspa_rpc_url: Option<String>,
    pub kaspa_network: Option<String>,
    pub payload_compression: Option<String>,
    pub logic_zone: Option<String>,
    pub entry_lock_script_pubkey: Option<String>,
    pub kaspa_wallet: IgraKaspaWalletConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgraPayloadKind {
    CanonicalRawTx,
    FalconL5RawTx,
    FalconL5Entry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IgraLogicZone {
    Canonical,
    FalconL5,
}

impl IgraLogicZone {
    fn from_config(value: Option<&str>) -> Result<Self, String> {
        let value = value.unwrap_or("canonical").trim().to_ascii_lowercase();
        match value.as_str() {
            "" | "canonical" => Ok(Self::Canonical),
            "falcon-l5" => Ok(Self::FalconL5),
            _ => Err(format!(
                "IGRA config error: `logic_zone` is invalid (supported: canonical, falcon-l5)"
            )),
        }
    }

    const fn is_falcon_l5(self) -> bool {
        matches!(self, Self::FalconL5)
    }
}

#[derive(Clone, Debug)]
struct IgraPayloadData {
    header: u8,
    l2data: Vec<u8>,
    max_l2data_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EntryDepositOutput {
    amount_sompi: u64,
    lock_script_pubkey: Vec<u8>,
}

/// Result returned by a Kaspa submitter implementation.
#[derive(Clone, Debug)]
pub struct IgraSubmitResult {
    pub kaspa_tx_id: String,
    pub payload_nonce: u64,
}

/// Abstraction over Kaspa submission to keep IGRA transport testable.
#[async_trait]
pub trait IgraPayloadSubmitter: Send + Sync + std::fmt::Debug {
    async fn submit_payload(&self, request: &IgraSubmitRequest)
    -> Result<IgraSubmitResult, String>;
}

#[derive(Clone)]
struct CachedUtxoSet {
    cache_key: String,
    fetched_at: Instant,
    entries: Vec<RpcUtxosByAddressesEntry>,
}

/// Default submitter implementation backed by in-process Kaspa RPC, signing, and broadcast.
#[derive(Clone)]
pub struct InProcessKaspaPayloadSubmitter {
    utxo_cache: Arc<TokioMutex<Option<CachedUtxoSet>>>,
    utxo_cache_epoch: Arc<AtomicU64>,
}

impl std::fmt::Debug for InProcessKaspaPayloadSubmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessKaspaPayloadSubmitter").finish()
    }
}

impl Default for InProcessKaspaPayloadSubmitter {
    fn default() -> Self {
        Self {
            utxo_cache: Arc::new(TokioMutex::new(None)),
            utxo_cache_epoch: Arc::new(AtomicU64::new(1)),
        }
    }
}

#[async_trait]
impl IgraPayloadSubmitter for InProcessKaspaPayloadSubmitter {
    async fn submit_payload(
        &self,
        request: &IgraSubmitRequest,
    ) -> Result<IgraSubmitResult, String> {
        let payload_data = build_igra_l2data(
            &request.raw_tx_bytes,
            request.payload_compression.as_deref(),
            request.logic_zone.as_deref(),
            request.payload_kind,
        )?;
        if payload_data.l2data.len() > payload_data.max_l2data_bytes {
            return Err(format!(
                "{IGRA_L2DATA_TOO_LARGE_ERROR_CODE}: L2Data size {} bytes exceeds max {} bytes",
                payload_data.l2data.len(),
                payload_data.max_l2data_bytes
            ));
        }
        let entry_deposit = entry_deposit_output_for_request(
            request.payload_kind,
            &request.raw_tx_bytes,
            request.entry_lock_script_pubkey.as_deref(),
        )?;

        let rpc_url = request
            .kaspa_rpc_url
            .as_deref()
            .ok_or_else(|| "IGRA config error: `kaspa_rpc_url` is required".to_string())?;
        let network = request
            .kaspa_network
            .as_deref()
            .ok_or_else(|| "IGRA config error: `kaspa_network` is required".to_string())?;
        let (network_type, address_prefix) = kaspa_network_descriptor(network)?;
        let private_key = resolve_kaspa_private_key(&request.kaspa_wallet)?;
        let source_address = kaspa_address_from_private_key(&private_key, address_prefix)?;
        info!(
            "IGRA submit: kaspa_source_address={} kaspa_network={} kaspa_rpc_url={} l2_tx_hash={}",
            source_address, network, rpc_url, request.l2_tx_hash
        );
        let mut client = GrpcClient::connect(rpc_url.to_string())
            .await
            .map_err(|err| format!("IGRA submit error: failed to connect to Kaspa RPC: {err}"))?;

        let prefix = normalize_hex_prefix(request.tx_id_prefix.clone());
        if prefix.is_empty() {
            return Err("IGRA config error: `tx_id_prefix` cannot be empty".to_string());
        }
        let prefix_bytes = hex::decode(prefix.clone())
            .map_err(|err| format!("IGRA config error: `tx_id_prefix` is invalid hex: {err}"))?;
        let mining_timeout = Duration::from_secs(request.mining_timeout_secs);

        for attempt in 0..=1 {
            let force_refresh = attempt > 0;
            let utxos = self
                .load_utxos(&mut client, rpc_url, network, &source_address, force_refresh)
                .await?;

            // Allow one forced refresh pass in case the cache is stale or the node just finished syncing.
            if utxos.is_empty() {
                if !force_refresh {
                    continue;
                }

                let mut message = format!(
                    "IGRA submit error: insufficient Kaspa UTXOs for fee payment (source address: {source_address})"
                );
                if request.kaspa_wallet.mnemonic.is_some()
                    && request.kaspa_wallet.mnemonic_passphrase.is_none()
                {
                    message.push_str(
                        "; hint: if this mnemonic was created/imported with a non-empty BIP39 passphrase, set --mnemonic-passphrase-kaspa (or KASPA_MNEMONIC_PASSPHRASE) to match the funded address",
                    );
                }
                return Err(message);
            }

            let private_key_for_build = private_key;
            let source_address_for_build = source_address.clone();
            let l2data_for_build = payload_data.l2data.clone();
            let prefix_for_build = prefix_bytes.clone();
            let mining_timeout_for_build = mining_timeout;
            let payload_header = payload_data.header;
            let entry_deposit_for_build = entry_deposit.clone();
            let (payload_nonce, transaction) = tokio::task::spawn_blocking(move || {
                mine_and_build_signed_payload_transaction(
                    &private_key_for_build,
                    &source_address_for_build,
                    network_type,
                    payload_header,
                    &l2data_for_build,
                    &prefix_for_build,
                    mining_timeout_for_build,
                    &utxos,
                    entry_deposit_for_build.as_ref(),
                )
            })
            .await
            .map_err(|err| {
                format!("IGRA submit error: failed to join Kaspa tx builder task: {err}")
            })??;
            // Invalidate local UTXO cache before broadcast so concurrent reads cannot reuse
            // potentially spent entries from this submission attempt.
            self.invalidate_utxo_cache().await;
            let rpc_transaction = RpcTransaction::from(&transaction);
            match client.submit_transaction(rpc_transaction, false).await {
                Ok(tx_id) => {
                    self.invalidate_utxo_cache().await;
                    let kaspa_tx_id = tx_id.to_string();
                    info!(
                        "IGRA submit: kaspa_tx_id={} payload_nonce={} payload_header=0x{:02x} l2data_len={} payload_compression={} l2_tx_hash={}",
                        kaspa_tx_id,
                        payload_nonce,
                        payload_data.header,
                        payload_data.l2data.len(),
                        request.payload_compression.as_deref().unwrap_or("none").trim(),
                        request.l2_tx_hash
                    );
                    return Ok(IgraSubmitResult { kaspa_tx_id, payload_nonce });
                }
                Err(err) => {
                    if attempt == 1 {
                        return Err(format!("IGRA submit error: {err}"));
                    }
                }
            }
        }

        Err("IGRA submit error: failed to submit Kaspa transaction".to_string())
    }
}

impl InProcessKaspaPayloadSubmitter {
    async fn invalidate_utxo_cache(&self) {
        self.utxo_cache_epoch.fetch_add(1, Ordering::Relaxed);
        let mut cache_guard = self.utxo_cache.lock().await;
        *cache_guard = None;
    }

    async fn load_utxos(
        &self,
        client: &mut GrpcClient,
        rpc_url: &str,
        network: &str,
        source_address: &KaspaAddress,
        force_refresh: bool,
    ) -> Result<Vec<RpcUtxosByAddressesEntry>, String> {
        let cache_key = format!("{rpc_url}|{network}|{source_address}");
        let read_epoch = self.utxo_cache_epoch.load(Ordering::Relaxed);
        if !force_refresh {
            let cache_guard = self.utxo_cache.lock().await;
            if let Some(cache) = cache_guard.as_ref()
                && cache.cache_key == cache_key
                && cache.fetched_at.elapsed() <= Duration::from_secs(CACHE_TTL_SECS)
            {
                return Ok(cache.entries.clone());
            }
        }

        let entries = client
            .get_utxos_by_addresses(vec![source_address.clone()])
            .await
            .map_err(|err| format!("IGRA submit error: failed to load Kaspa UTXOs: {err}"))?;

        let mut cache_guard = self.utxo_cache.lock().await;
        let current_epoch = self.utxo_cache_epoch.load(Ordering::Relaxed);
        if force_refresh || current_epoch == read_epoch {
            *cache_guard = Some(CachedUtxoSet {
                cache_key,
                fetched_at: Instant::now(),
                entries: entries.clone(),
            });
        }
        Ok(entries)
    }
}

fn kaspa_network_descriptor(
    network: &str,
) -> Result<(KaspaNetworkType, KaspaAddressPrefix), String> {
    match network {
        "mainnet" => Ok((KaspaNetworkType::Mainnet, KaspaAddressPrefix::Mainnet)),
        "testnet-10" => Ok((KaspaNetworkType::Testnet, KaspaAddressPrefix::Testnet)),
        "devnet" => Ok((KaspaNetworkType::Devnet, KaspaAddressPrefix::Devnet)),
        "simnet" => Ok((KaspaNetworkType::Simnet, KaspaAddressPrefix::Simnet)),
        "custom" => Err("IGRA config error: `kaspa_network=custom` requires explicit in-process network mapping".to_string()),
        other => Err(format!("IGRA config error: unsupported kaspa_network `{other}`")),
    }
}

fn kaspa_address_from_private_key(
    private_key: &[u8; 32],
    prefix: KaspaAddressPrefix,
) -> Result<KaspaAddress, String> {
    let secret = KaspaSecretKey::from_slice(private_key)
        .map_err(|err| format!("IGRA key resolution error: invalid private key bytes: {err}"))?;
    let public_key = kaspa_bip32::secp256k1::PublicKey::from_secret_key_global(&secret);
    let payload = public_key.x_only_public_key().0.serialize();
    Ok(KaspaAddress::new(prefix, KaspaAddressVersion::PubKey, &payload))
}

fn resolve_kaspa_private_key(config: &IgraKaspaWalletConfig) -> Result<[u8; 32], String> {
    if let Some(private_key) = config.private_key.as_deref() {
        return parse_private_key_hex(private_key);
    }

    if let Some(mnemonic) = config.mnemonic.as_deref() {
        return resolve_mnemonic_private_key(
            mnemonic,
            config.mnemonic_passphrase.as_deref(),
            config.mnemonic_derivation_path.as_deref(),
            config.mnemonic_index.unwrap_or(0),
        );
    }

    if config.keystore.is_some() || config.keystore_account.is_some() {
        return resolve_keystore_private_key(config);
    }

    Err(IGRA_KEY_RESOLUTION_ERROR.to_string())
}

fn parse_private_key_hex(private_key: &str) -> Result<[u8; 32], String> {
    let private_key = private_key.trim();
    let key = private_key.parse::<B256>().map_err(|_| {
        format!("{IGRA_KEY_RESOLUTION_ERROR}: provided --private-key-kaspa value is invalid hex")
    })?;
    Ok(key.0)
}

fn resolve_mnemonic_private_key(
    mnemonic: &str,
    passphrase: Option<&str>,
    derivation_path: Option<&str>,
    index: u32,
) -> Result<[u8; 32], String> {
    let phrase = if Path::new(mnemonic).is_file() {
        fs::read_to_string(mnemonic).map_err(|err| {
            format!("IGRA key resolution error: failed to read mnemonic file: {err}")
        })?
    } else {
        mnemonic.to_string()
    };
    let phrase = phrase.split_whitespace().collect::<Vec<_>>().join(" ");

    // IMPORTANT:
    // Derive keys exactly like `kaspa-cli` (rusty-kaspa wallet):
    // - BIP39 seed from mnemonic (+ optional passphrase)
    // - BIP32 master key
    // - BIP44-ish path: m/44'/111111'/0'/0/<index> by default (single-sig receive chain)
    let kaspa_mnemonic = KaspaMnemonic::new(phrase, KaspaLanguage::English)
        .map_err(|err| format!("IGRA key resolution error: invalid Kaspa mnemonic: {err}"))?;
    let seed = kaspa_mnemonic.to_seed(passphrase.unwrap_or_default());

    let xprv = KaspaExtendedPrivateKey::<KaspaSecretKey>::new(seed).map_err(|err| {
        format!(
            "IGRA key resolution error: failed to derive Kaspa master key from mnemonic seed: {err}"
        )
    })?;

    let secret = if let Some(path) = derivation_path {
        let path = path.parse::<KaspaDerivationPath>().map_err(|err| {
            format!("IGRA key resolution error: invalid Kaspa derivation path: {err}")
        })?;
        *xprv
            .derive_path(&path)
            .map_err(|err| {
                format!("IGRA key resolution error: failed to derive Kaspa key by path: {err}")
            })?
            .private_key()
    } else {
        let base = "m/44'/111111'/0'/0".parse::<KaspaDerivationPath>().map_err(|err| {
            format!(
                "IGRA key resolution error: failed to parse default Kaspa derivation path: {err}"
            )
        })?;
        let base = xprv.derive_path(&base).map_err(|err| {
            format!("IGRA key resolution error: failed to derive default Kaspa base key: {err}")
        })?;
        *base
            .derive_child(KaspaChildNumber::new(index, false).map_err(|err| {
                format!("IGRA key resolution error: invalid Kaspa mnemonic index: {err}")
            })?)
            .map_err(|err| {
                format!("IGRA key resolution error: failed to derive Kaspa key by index: {err}")
            })?
            .private_key()
    };

    Ok(secret.secret_bytes())
}

fn resolve_keystore_private_key(config: &IgraKaspaWalletConfig) -> Result<[u8; 32], String> {
    let path = resolve_keystore_path(config)?;
    let password = config.password.as_deref().ok_or_else(|| {
        "IGRA key resolution error: kaspa keystore password is required; set --password-kaspa or KASPA_PASSWORD".to_string()
    })?;
    let signer = PrivateKeySigner::decrypt_keystore(&path, password).map_err(|err| {
        format!("IGRA key resolution error: failed to decrypt kaspa keystore: {err}")
    })?;
    Ok(signer.credential().to_bytes().into())
}

fn resolve_keystore_path(config: &IgraKaspaWalletConfig) -> Result<PathBuf, String> {
    if let Some(path) = config.keystore.as_ref() {
        return Ok(PathBuf::from(path));
    }

    if let Some(account) = config.keystore_account.as_ref() {
        let keystore_dir = Config::foundry_keystores_dir().ok_or_else(|| {
            "IGRA key resolution error: could not resolve default foundry keystore directory"
                .to_string()
        })?;
        return Ok(keystore_dir.join(account));
    }

    Err("IGRA key resolution error: kaspa keystore path or account is required".to_string())
}

fn initial_fee_sompi(payload_len: usize, inputs: usize) -> u64 {
    let payload_len = u64::try_from(payload_len).unwrap_or(u64::MAX);
    let input_tail = u64::try_from(inputs.saturating_sub(1)).unwrap_or(u64::MAX);
    BASE_SUBMIT_FEE_SOMPI
        .max(payload_len.saturating_mul(INITIAL_FEE_PER_PAYLOAD_BYTE_SOMPI))
        .saturating_add(input_tail.saturating_mul(EXTRA_INPUT_FEE_SOMPI))
}

fn minimum_relay_fee_sompi_for_mass(mass: u64) -> u64 {
    let mut fee = mass.saturating_mul(CURRENT_KASPA_MIN_RELAY_FEE_PER_KG_SOMPI) / 1000;
    if fee == 0 {
        fee = CURRENT_KASPA_MIN_RELAY_FEE_PER_KG_SOMPI;
    }
    fee
}

/// Transport wrapper that applies IGRA-specific request interception.
#[derive(Clone, Debug)]
pub struct IgraTransport<T> {
    inner: T,
    enabled: bool,
    store: Option<Arc<IgraStore>>,
    tx_id_prefix: Option<String>,
    mining_timeout: Duration,
    kaspa_rpc_url: Option<String>,
    kaspa_network: Option<String>,
    payload_compression: Option<String>,
    logic_zone: IgraLogicZone,
    kaspa_wallet: IgraKaspaWalletConfig,
    submitter: Arc<dyn IgraPayloadSubmitter>,
}

impl<T> IgraTransport<T> {
    /// Creates a new IGRA transport wrapper.
    pub fn new(inner: T, enabled: bool) -> Self {
        Self {
            inner,
            enabled,
            store: None,
            tx_id_prefix: None,
            mining_timeout: Duration::from_secs(DEFAULT_MINING_TIMEOUT_SECS),
            kaspa_rpc_url: None,
            kaspa_network: None,
            payload_compression: None,
            logic_zone: IgraLogicZone::Canonical,
            kaspa_wallet: IgraKaspaWalletConfig::default(),
            submitter: Arc::new(InProcessKaspaPayloadSubmitter::default()),
        }
    }

    /// Sets runtime IGRA settings used by the raw-submit interception path.
    pub fn with_transport_config(mut self, config: IgraTransportConfig) -> Self {
        self.tx_id_prefix = config.tx_id_prefix.map(normalize_hex_prefix);
        self.mining_timeout =
            Duration::from_secs(config.mining_timeout_secs.unwrap_or(DEFAULT_MINING_TIMEOUT_SECS));
        self.kaspa_rpc_url = config.kaspa_rpc_url;
        self.kaspa_network = config.kaspa_network;
        self.payload_compression = config.payload_compression;
        self.logic_zone = IgraLogicZone::from_config(config.logic_zone.as_deref())
            .unwrap_or(IgraLogicZone::Canonical);
        self.kaspa_wallet = config.kaspa_wallet;
        self
    }

    /// Overrides the payload submitter implementation.
    pub fn with_submitter(mut self, submitter: Arc<dyn IgraPayloadSubmitter>) -> Self {
        self.submitter = submitter;
        self
    }

    /// Enables IGRA SQLite persistence for raw-send lifecycle tracking.
    pub fn with_store_config(mut self, config: IgraStoreConfig) -> Self {
        if self.kaspa_rpc_url.is_none() {
            self.kaspa_rpc_url = config.kaspa_rpc_url.clone();
        }
        if self.kaspa_network.is_none() {
            self.kaspa_network = config.kaspa_network.clone();
        }
        if self.enabled {
            match IgraStore::new(config) {
                Ok(store) => self.store = Some(Arc::new(store)),
                Err(err) => warn!("failed to initialize IGRA tx-map store: {err}"),
            }
        }
        self
    }

    /// Enables IGRA SQLite persistence for raw-send lifecycle tracking and returns initialization
    /// errors to the caller.
    pub fn try_with_store_config(mut self, config: IgraStoreConfig) -> Result<Self, String> {
        if self.kaspa_rpc_url.is_none() {
            self.kaspa_rpc_url = config.kaspa_rpc_url.clone();
        }
        if self.kaspa_network.is_none() {
            self.kaspa_network = config.kaspa_network.clone();
        }
        if self.enabled {
            let store = IgraStore::new(config).map_err(|err| err.to_string())?;
            self.store = Some(Arc::new(store));
        }
        Ok(self)
    }

    #[cfg(test)]
    fn with_store_for_tests(mut self, store: IgraStore) -> Self {
        self.store = Some(Arc::new(store));
        self
    }

    #[cfg(test)]
    fn with_submitter_for_tests(mut self, submitter: Arc<dyn IgraPayloadSubmitter>) -> Self {
        self.submitter = submitter;
        self
    }

    fn is_unsupported_method(method: &str) -> bool {
        matches!(
            method,
            "eth_sendTransaction" | "eth_sendTransactionSync" | "eth_sendRawTransactionSync"
        )
    }

    fn rejection_reason(&self, request: &RequestPacket) -> Option<String> {
        if !self.enabled {
            return None;
        }

        request.requests().iter().find_map(|request| self.request_rejection_reason(request))
    }

    fn request_rejection_reason(&self, request: &SerializedRequest) -> Option<String> {
        if Self::is_unsupported_method(request.method()) {
            return Some(IGRA_SEND_TRANSACTION_UNSUPPORTED_ERROR.to_string());
        }

        if request.method() != "eth_sendRawTransaction" {
            return None;
        }

        if self.logic_zone.is_falcon_l5() {
            return match Self::q_raw_tx_bytes(request) {
                Ok(_) => None,
                Err(err) => Some(format!("{IGRA_Q_RAW_TRANSACTION_ERROR}: {err}")),
            };
        }

        match Self::raw_tx_type(request) {
            Ok(RawIgraTxType::Legacy | RawIgraTxType::Eip2930 | RawIgraTxType::Eip1559) => None,
            Ok(RawIgraTxType::Eip4844) => Some(IGRA_EIP4844_UNSUPPORTED_ERROR.to_string()),
            Ok(RawIgraTxType::Eip7702) => Some(IGRA_EIP7702_UNSUPPORTED_ERROR.to_string()),
            Ok(RawIgraTxType::Unknown(ty)) => {
                Some(format!("IGRA unsupported transaction type: 0x{ty:02x}"))
            }
            Err(err) => Some(format!("IGRA raw transaction decode error: {err}")),
        }
    }

    fn raw_tx_type(request: &SerializedRequest) -> Result<RawIgraTxType, String> {
        let raw_tx = Self::raw_tx_bytes(request)?;
        let first = *raw_tx.first().ok_or("empty raw transaction bytes")?;

        // Legacy transactions are RLP lists, which always start at 0xc0 or above.
        if first >= 0xc0 {
            return Ok(RawIgraTxType::Legacy);
        }

        let ty = match first {
            0x01 => RawIgraTxType::Eip2930,
            0x02 => RawIgraTxType::Eip1559,
            0x03 => RawIgraTxType::Eip4844,
            0x04 => RawIgraTxType::Eip7702,
            _ => RawIgraTxType::Unknown(first),
        };
        Ok(ty)
    }

    fn reject_error(reason: &str) -> TransportError {
        TransportErrorKind::custom_str(reason)
    }

    fn raw_tx_bytes(request: &SerializedRequest) -> Result<Vec<u8>, String> {
        let raw = request.params().ok_or("missing params")?;
        let params: Vec<Value> =
            serde_json::from_str(raw.get()).map_err(|err| format!("invalid params JSON: {err}"))?;
        let encoded =
            params.first().and_then(Value::as_str).ok_or("expected params[0] hex string")?;
        let encoded = encoded.strip_prefix("0x").unwrap_or(encoded);
        hex::decode(encoded).map_err(|err| format!("invalid raw tx hex: {err}"))
    }

    fn q_raw_tx_bytes(request: &SerializedRequest) -> Result<Vec<u8>, String> {
        let raw_tx = Self::raw_tx_bytes(request)?;
        validate_q_raw_tx(&raw_tx)?;
        Ok(raw_tx)
    }

    fn single_send_raw_request(request: &RequestPacket) -> Option<&SerializedRequest> {
        match request {
            RequestPacket::Single(req) if req.method() == "eth_sendRawTransaction" => Some(req),
            _ => None,
        }
    }

    fn raw_send_request(&self, request: &RequestPacket) -> Result<Option<RawSendRequest>, String> {
        let request = match Self::single_send_raw_request(request) {
            Some(request) => request,
            None => return Ok(None),
        };
        let raw_tx = Self::raw_tx_bytes(request)?;
        let metadata = if self.logic_zone.is_falcon_l5() {
            validate_q_raw_tx(&raw_tx)?;
            Self::q_raw_tx_metadata_from_raw_tx(&raw_tx)
        } else {
            // Validate tx type up-front so we can fail before doing any expensive Kaspa work.
            // We no longer embed the tx type in the payload; the canonical header remains 0x94.
            let _tx_type_nibble = Self::raw_tx_type(request)?
                .tx_type_nibble()
                .ok_or_else(|| "unsupported tx type for IGRA payload header".to_string())?;
            Self::raw_tx_metadata_from_raw_tx(&raw_tx)?
        };

        Ok(Some(RawSendRequest { id: request.id().clone(), raw_tx, metadata }))
    }

    fn tracked_send(&self, metadata: &RawTxMetadata) -> Option<TrackedSend> {
        let store = self.store.as_ref()?.clone();
        let sender = metadata.sender.clone()?;
        let nonce = metadata.nonce?;
        let correlation_id = build_correlation_id(&metadata.l2_tx_hash);
        let lock_owner_id = build_lock_owner_id(&sender, nonce, &correlation_id);

        Some(TrackedSend {
            store,
            l2_tx_hash: metadata.l2_tx_hash.clone(),
            sender,
            l2_nonce: nonce,
            correlation_id,
            lock_owner_id,
        })
    }

    #[cfg(test)]
    fn raw_tx_metadata(request: &SerializedRequest) -> Result<RawTxMetadata, String> {
        let raw_tx = Self::raw_tx_bytes(request)?;
        Self::raw_tx_metadata_from_raw_tx(&raw_tx)
    }

    fn raw_tx_metadata_from_raw_tx(raw_tx: &[u8]) -> Result<RawTxMetadata, String> {
        let l2_tx_hash = format!("0x{}", hex::encode(keccak256(raw_tx)));

        let mut raw_tx_slice = raw_tx;
        let decoded: TxEnvelope = Decodable2718::decode_2718(&mut raw_tx_slice)
            .map_err(|err| format!("invalid raw tx bytes (EIP-2718 decode failed): {err}"))?;
        let sender = decoded
            .recover_signer()
            .map_err(|err| format!("invalid raw tx signature (failed to recover signer): {err}"))?;
        let nonce = decoded.nonce();

        Ok(RawTxMetadata { l2_tx_hash, sender: Some(format!("{sender:#x}")), nonce: Some(nonce) })
    }

    fn q_raw_tx_metadata_from_raw_tx(raw_tx: &[u8]) -> RawTxMetadata {
        let l2_tx_hash = format!("0x{}", hex::encode(keccak256(raw_tx)));
        RawTxMetadata { l2_tx_hash, sender: None, nonce: None }
    }

    fn persist_transition_safe(
        tracked: &TrackedSend,
        state: TxLifecycleState,
        payload_nonce: Option<u64>,
        kaspa_tx_id: Option<String>,
        last_error_code: Option<String>,
        last_error_message: Option<String>,
        increment_attempts: bool,
    ) {
        let update = TxLifecycleUpdate {
            l2_tx_hash: tracked.l2_tx_hash.clone(),
            sender: tracked.sender.clone(),
            l2_nonce: tracked.l2_nonce,
            payload_nonce,
            kaspa_tx_id,
            state,
            correlation_id: tracked.correlation_id.clone(),
            last_error_code,
            last_error_message,
            increment_attempts,
        };

        if let Err(err) = tracked.store.persist_transition(&update) {
            warn!(
                "failed to persist IGRA tx lifecycle state {} for {}: {err}",
                state.as_str(),
                tracked.l2_tx_hash
            );
        } else {
            info!(
                target: "igra.lifecycle",
                l2_tx_hash = %tracked.l2_tx_hash,
                sender = %tracked.sender,
                l2_nonce = tracked.l2_nonce,
                state = state.as_str(),
                correlation_id = %tracked.correlation_id,
                last_error_code = ?update.last_error_code.as_deref(),
                "persisted IGRA tx lifecycle transition"
            );
        }
    }

    fn success_l2_tx_hash_response(
        id: Id,
        l2_tx_hash: &str,
    ) -> Result<ResponsePacket, TransportError> {
        let payload = raw_json_string(l2_tx_hash).map(ResponsePayload::Success).map_err(|err| {
            Self::reject_error(&format!("failed to serialize IGRA response payload: {err}"))
        })?;
        Ok(ResponsePacket::Single(Response { id, payload }))
    }

    /// Sends a request through the wrapped transport unless blocked by IGRA interception.
    pub fn request(&self, request: RequestPacket) -> TransportFut<'static>
    where
        T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
            + Clone
            + Send
            + 'static,
        T::Future: Send + 'static,
    {
        // IGRA adapter may reject L2 txs if `maxPriorityFeePerGas` is below a protocol minimum.
        //
        // Foundry (via alloy) derives EIP-1559 fees from `eth_maxPriorityFeePerGas`, which can be
        // much lower than `eth_gasPrice` on IGRA networks. To make "normal" `cast send` /
        // `forge script --broadcast` flows work without requiring extra flags, we clamp the
        // priority-fee estimator upward by responding to `eth_maxPriorityFeePerGas` with the value
        // returned by `eth_gasPrice` when IGRA mode is enabled.
        if self.enabled {
            if let Some(single) = request.as_single() {
                if single.method() == "eth_maxPriorityFeePerGas" {
                    let id = single.id().clone();
                    let req = match Request::new("eth_gasPrice", id, ()).serialize() {
                        Ok(req) => req,
                        Err(err) => {
                            return Box::pin(async move {
                                Err(Self::reject_error(&format!(
                                    "IGRA fee override error: failed to build eth_gasPrice request: {err}"
                                )))
                            });
                        }
                    };

                    let pkt = RequestPacket::Single(req);
                    let mut inner = self.inner.clone();
                    return Box::pin(async move { inner.call(pkt).await });
                }
            }
        }

        if let Some(reason) = self.rejection_reason(&request) {
            return Box::pin(async move { Err(Self::reject_error(&reason)) });
        }

        let raw_send = if self.enabled {
            match self.raw_send_request(&request) {
                Ok(send) => send,
                Err(err) => return Box::pin(async move { Err(Self::reject_error(&err)) }),
            }
        } else {
            None
        };

        if let Some(raw_send) = raw_send {
            let tracked_send = self.tracked_send(&raw_send.metadata);
            let tx_id_prefix = self.tx_id_prefix.clone();
            let mining_timeout = self.mining_timeout;
            let kaspa_rpc_url = self.kaspa_rpc_url.clone();
            let kaspa_network = self.kaspa_network.clone();
            let payload_compression = self.payload_compression.clone();
            let logic_zone = self.logic_zone;
            let kaspa_wallet = self.kaspa_wallet.clone();
            let submitter = self.submitter.clone();

            return Box::pin(async move {
                let mut lock_acquired = false;
                let mut in_order_submit = false;
                let mut replacement_observation: Option<(String, String)> = None;
                let tracked = tracked_send;

                let result: Result<ResponsePacket, TransportError> = async {
                    let tx_id_prefix = tx_id_prefix.ok_or_else(|| {
                        Self::reject_error("IGRA config error: `tx_id_prefix` is required")
                    })?;

                    if let Some(tracked) = tracked.as_ref() {
                        Self::persist_transition_safe(
                            tracked,
                            TxLifecycleState::ReceivedRawL2,
                            None,
                            None,
                            None,
                            None,
                            false,
                        );

                        let store = tracked.store.clone();
                        let sender = tracked.sender.clone();
                        let lock_owner_id = tracked.lock_owner_id.clone();
                        let l2_nonce = tracked.l2_nonce;
                        let l2_tx_hash = tracked.l2_tx_hash.clone();
                        let lock_result = tokio::task::spawn_blocking(move || {
                            store.acquire_sender_lock_and_classify_nonce(
                                &sender,
                                &lock_owner_id,
                                l2_nonce,
                                &l2_tx_hash,
                            )
                        })
                        .await
                        .map_err(|err| {
                            Self::reject_error(&format!("IGRA sender lock task join failure: {err}"))
                        })?;

                        match lock_result {
                            Ok(NonceOrdering::InOrder { .. }) => {
                                lock_acquired = true;
                                in_order_submit = true;
                            }
                            Ok(NonceOrdering::Stale { replacement_candidate, .. }) => {
                                lock_acquired = true;
                                if replacement_candidate {
                                    let message = format!(
                                        "stale nonce replacement candidate for sender {} nonce {} tx {}",
                                        tracked.sender, tracked.l2_nonce, tracked.l2_tx_hash
                                    );
                                    let code =
                                        IGRA_NONCE_REPLACEMENT_CANDIDATE_ERROR_CODE.to_string();
                                    replacement_observation = Some((code.clone(), message.clone()));
                                    Self::persist_transition_safe(
                                        tracked,
                                        TxLifecycleState::StaleReplacementCandidate,
                                        None,
                                        None,
                                        Some(code),
                                        Some(message),
                                        false,
                                    );
                                }
                            }
                            Err(err @ IgraStoreError::LockTimeout { .. }) => {
                                Self::persist_transition_safe(
                                    tracked,
                                    TxLifecycleState::BlockedLockTimeout,
                                    None,
                                    None,
                                    err.code().map(str::to_string),
                                    Some(err.to_string()),
                                    false,
                                );
                                return Err(Self::reject_error(&err.to_string()));
                            }
                            Err(err @ IgraStoreError::BlockedNonceGap { .. }) => {
                                Self::persist_transition_safe(
                                    tracked,
                                    TxLifecycleState::BlockedNonceGap,
                                    None,
                                    None,
                                    Some(IGRA_NONCE_GAP_ERROR_CODE.to_string()),
                                    Some(err.to_string()),
                                    false,
                                );
                                return Err(Self::reject_error(&err.to_string()));
                            }
                            Err(err) => {
                                warn!(
                                    "non-fatal IGRA sender lock/classification failure for {}: {err}",
                                    tracked.sender
                                );
                            }
                        }
                    }

                    let replacement_code =
                        replacement_observation.as_ref().map(|(code, _)| code.clone());
                    let replacement_message =
                        replacement_observation.as_ref().map(|(_, message)| message.clone());
                    if let Some(tracked) = tracked.as_ref() {
                        Self::persist_transition_safe(
                            tracked,
                            TxLifecycleState::KaspaUnsignedCreated,
                            None,
                            None,
                            replacement_code.clone(),
                            replacement_message.clone(),
                            false,
                        );
                    }

                    let submit_request = IgraSubmitRequest {
                        l2_tx_hash: raw_send.metadata.l2_tx_hash.clone(),
                        raw_tx_bytes: raw_send.raw_tx.clone(),
                        payload_kind: match logic_zone {
                            IgraLogicZone::Canonical => IgraPayloadKind::CanonicalRawTx,
                            IgraLogicZone::FalconL5 => IgraPayloadKind::FalconL5RawTx,
                        },
                        tx_id_prefix: tx_id_prefix.clone(),
                        mining_timeout_secs: mining_timeout.as_secs(),
                        kaspa_rpc_url,
                        kaspa_network,
                        payload_compression: payload_compression.clone(),
                        logic_zone: Some(
                            match logic_zone {
                                IgraLogicZone::Canonical => "canonical",
                                IgraLogicZone::FalconL5 => "falcon-l5",
                            }
                            .to_string(),
                        ),
                        entry_lock_script_pubkey: None,
                        kaspa_wallet,
                    };
                    let submit_result = submitter
                        .submit_payload(&submit_request)
                        .await
                        .map_err(|err| Self::reject_error(&err))?;
                    let kaspa_tx_id = submit_result.kaspa_tx_id.clone();
                    let payload_nonce = submit_result.payload_nonce;

                    if let Some(tracked) = tracked.as_ref() {
                        Self::persist_transition_safe(
                            tracked,
                            TxLifecycleState::KaspaPrefixMined,
                            Some(payload_nonce),
                            None,
                            replacement_code.clone(),
                            replacement_message.clone(),
                            false,
                        );
                        Self::persist_transition_safe(
                            tracked,
                            TxLifecycleState::KaspaSigned,
                            Some(payload_nonce),
                            None,
                            replacement_code.clone(),
                            replacement_message.clone(),
                            false,
                        );
                        Self::persist_transition_safe(
                            tracked,
                            TxLifecycleState::KaspaBroadcasted,
                            Some(payload_nonce),
                            Some(kaspa_tx_id.clone()),
                            replacement_code,
                            replacement_message,
                            false,
                        );
                        if in_order_submit {
                            let store = tracked.store.clone();
                            let sender = tracked.sender.clone();
                            let nonce = tracked.l2_nonce;
                            match tokio::task::spawn_blocking(move || {
                                store.mark_submitted_in_order_nonce(&sender, nonce)
                            })
                            .await
                            {
                                Ok(Ok(())) => {}
                                Ok(Err(err)) => {
                                    warn!("failed to advance IGRA next-expected nonce: {err}")
                                }
                                Err(err) => warn!(
                                    "failed to join IGRA next-expected nonce task: {err}"
                                ),
                            }
                        }
                    }

                    Self::success_l2_tx_hash_response(
                        raw_send.id.clone(),
                        &raw_send.metadata.l2_tx_hash,
                    )
                }
                .await;

                if let Some(tracked) = tracked.as_ref() {
                    if let Err(err) = &result {
                        let err_text = err.to_string();
                        let err_code = if err_text.contains(IGRA_MINING_TIMEOUT_ERROR_CODE) {
                            Some(IGRA_MINING_TIMEOUT_ERROR_CODE.to_string())
                        } else {
                            None
                        };
                        Self::persist_transition_safe(
                            tracked,
                            TxLifecycleState::FailedRecoverable,
                            None,
                            None,
                            err_code,
                            Some(err_text),
                            true,
                        );
                    }

                    if lock_acquired {
                        let store = tracked.store.clone();
                        let sender = tracked.sender.clone();
                        let lock_owner_id = tracked.lock_owner_id.clone();
                        match tokio::task::spawn_blocking(move || {
                            store.release_sender_lock(&sender, &lock_owner_id)
                        })
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(err)) => warn!("failed to release IGRA sender lock: {err}"),
                            Err(err) => {
                                warn!("failed to join IGRA sender lock release task: {err}")
                            }
                        }
                    }
                }

                result
            });
        }

        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(request).await })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawIgraTxType {
    Legacy,
    Eip2930,
    Eip1559,
    Eip4844,
    Eip7702,
    Unknown(u8),
}

impl RawIgraTxType {
    const fn tx_type_nibble(self) -> Option<u8> {
        match self {
            Self::Legacy => Some(0),
            Self::Eip2930 => Some(1),
            Self::Eip1559 => Some(2),
            Self::Eip4844 | Self::Eip7702 | Self::Unknown(_) => None,
        }
    }
}

#[derive(Clone, Debug)]
struct RawSendRequest {
    id: Id,
    raw_tx: Vec<u8>,
    metadata: RawTxMetadata,
}

#[derive(Clone, Debug)]
struct RawTxMetadata {
    l2_tx_hash: String,
    sender: Option<String>,
    nonce: Option<u64>,
}

#[derive(Clone, Debug)]
struct TrackedSend {
    store: Arc<IgraStore>,
    l2_tx_hash: String,
    sender: String,
    l2_nonce: u64,
    correlation_id: String,
    lock_owner_id: String,
}

fn normalize_hex_prefix(prefix: String) -> String {
    prefix.trim().trim_start_matches("0x").to_ascii_lowercase()
}

fn raw_json_string(value: &str) -> Result<Box<serde_json::value::RawValue>, serde_json::Error> {
    serde_json::value::RawValue::from_string(serde_json::to_string(value)?)
}

/// Build an IGRA payload for embedding into a Kaspa L1 TX.
///
/// Spec (IGRA Transaction Protocol):
/// - 1 byte header: `(version << 4) | txTypeId`, where version=0x9.
///   - txTypeId=0x4: raw EVM tx (uncompressed)
///   - txTypeId=0x5: zlib-compressed raw EVM tx
/// - L2Data bytes (raw tx or zlib-compressed raw tx)
/// - 4-byte nonce (used only for txid prefix mining)
fn build_payload_with_nonce(header: u8, l2data: &[u8], nonce: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + l2data.len().saturating_add(4));
    payload.push(header);
    payload.extend_from_slice(l2data);
    // The spec treats the payload nonce as an opaque 4-byte value for txid mining.
    // We encode it as big-endian to match the kaspa-cli / kaswallet derivation and IGRA adapter
    // test-vectors.
    payload.extend_from_slice(&nonce.to_be_bytes());
    payload
}

fn build_igra_l2data(
    raw_tx: &[u8],
    payload_compression: Option<&str>,
    logic_zone: Option<&str>,
    payload_kind: IgraPayloadKind,
) -> Result<IgraPayloadData, String> {
    let logic_zone = IgraLogicZone::from_config(logic_zone)?;
    let mode = payload_compression.unwrap_or("none").trim().to_ascii_lowercase();

    match payload_kind {
        IgraPayloadKind::CanonicalRawTx if logic_zone.is_falcon_l5() => {
            return Err(
                "IGRA config error: canonical raw transactions cannot target falcon-l5 q-zone"
                    .to_string(),
            );
        }
        IgraPayloadKind::FalconL5RawTx | IgraPayloadKind::FalconL5Entry
            if !logic_zone.is_falcon_l5() =>
        {
            return Err(
                "IGRA config error: Falcon-L5 q payloads require logic_zone=falcon-l5".to_string()
            );
        }
        _ => {}
    }

    if matches!(payload_kind, IgraPayloadKind::FalconL5RawTx | IgraPayloadKind::FalconL5Entry) {
        if !matches!(mode.as_str(), "" | "none") {
            return Err(
                "IGRA config error: q-zone does not support `payload_compression`; use `none`"
                    .to_string(),
            );
        }

        let zone_tx_type = match payload_kind {
            IgraPayloadKind::FalconL5RawTx => {
                validate_q_raw_tx(raw_tx)?;
                IGRA_Q_RAW_TX_TYPE
            }
            IgraPayloadKind::FalconL5Entry => {
                validate_q_entry(raw_tx)?;
                IGRA_Q_ENTRY_TX_TYPE
            }
            IgraPayloadKind::CanonicalRawTx => unreachable!(),
        };

        let mut l2data = Vec::with_capacity(IGRA_LOGIC_ZONE_HEADER_SIZE + raw_tx.len());
        l2data.push(IGRA_Q_ENVELOPE_VERSION);
        l2data.extend_from_slice(&IGRA_FALCON_L5_Q_ZONE_ID.to_be_bytes());
        l2data.push(zone_tx_type);
        l2data.extend_from_slice(raw_tx);

        return Ok(IgraPayloadData {
            header: (IGRA_VERSION << 4) | IGRA_LOGIC_ZONE_TX_TYPE,
            l2data,
            max_l2data_bytes: IGRA_Q_L2DATA_MAX_BYTES,
        });
    }

    match mode.as_str() {
        "" | "none" => Ok(IgraPayloadData {
            header: (IGRA_VERSION << 4) | IGRA_CANONICAL_RAW_TX_TYPE,
            l2data: raw_tx.to_vec(),
            max_l2data_bytes: IGRA_MAX_L2DATA_BYTES,
        }),
        // The IGRA protocol defines a zipped payload type, but it is not deployed/accepted on
        // galleon testnet at the moment. Keep v1 deterministic by only supporting uncompressed.
        "zlib" => {
            Err("IGRA config error: `payload_compression=zlib` is not implemented; use `none`"
                .to_string())
        }
        _ => Err(format!("IGRA config error: `payload_compression` is invalid (supported: none)")),
    }
}

fn validate_q_raw_tx(raw_tx: &[u8]) -> Result<(), String> {
    let first = raw_tx.first().ok_or("empty raw q transaction bytes")?;
    if *first != IGRA_FALCON_L5_TX_TYPE {
        return Err(format!(
            "expected Falcon-L5 q transaction type 0x{IGRA_FALCON_L5_TX_TYPE:02x}, got 0x{first:02x}"
        ));
    }
    if raw_tx.len() > IGRA_Q_RAW_TX_MAX_BYTES {
        return Err(format!(
            "raw q transaction size {} bytes exceeds max {} bytes",
            raw_tx.len(),
            IGRA_Q_RAW_TX_MAX_BYTES
        ));
    }
    Ok(())
}

fn validate_q_entry(entry: &[u8]) -> Result<(), String> {
    if entry.len() != IGRA_Q_ENTRY_BYTES {
        return Err(format!(
            "q Entry payload size {} bytes must equal {IGRA_Q_ENTRY_BYTES} bytes",
            entry.len()
        ));
    }
    Ok(())
}

fn q_entry_amount_sompi(entry: &[u8]) -> Result<u64, String> {
    validate_q_entry(entry)?;
    let amount_bytes: [u8; 8] = entry[IGRA_Q_ENTRY_ADDRESS_BYTES..IGRA_Q_ENTRY_BYTES]
        .try_into()
        .map_err(|_| "q Entry amount bytes are malformed".to_string())?;
    Ok(u64::from_le_bytes(amount_bytes))
}

fn parse_lock_script_pubkey_hex(value: &str) -> Result<Vec<u8>, String> {
    let value = value.trim().trim_start_matches("0x");
    if value.is_empty() {
        return Err("Entry lock script pubkey cannot be empty".to_string());
    }
    if value.len() % 2 != 0 {
        return Err(
            "Entry lock script pubkey must have an even number of hex characters".to_string()
        );
    }
    hex::decode(value).map_err(|err| format!("Entry lock script pubkey must be hex-encoded: {err}"))
}

fn entry_deposit_output_for_request(
    payload_kind: IgraPayloadKind,
    raw_tx: &[u8],
    entry_lock_script_pubkey: Option<&str>,
) -> Result<Option<EntryDepositOutput>, String> {
    if payload_kind != IgraPayloadKind::FalconL5Entry {
        return Ok(None);
    }

    let lock_script_pubkey = entry_lock_script_pubkey.ok_or_else(|| {
        "IGRA q Entry requires `entry_lock_script_pubkey` in [igra] or --entry-lock-script-pubkey"
            .to_string()
    })?;
    Ok(Some(EntryDepositOutput {
        amount_sompi: q_entry_amount_sompi(raw_tx)?,
        lock_script_pubkey: parse_lock_script_pubkey_hex(lock_script_pubkey)?,
    }))
}

fn mine_and_build_signed_payload_transaction(
    private_key: &[u8; 32],
    source_address: &KaspaAddress,
    network_type: KaspaNetworkType,
    payload_header: u8,
    l2data: &[u8],
    tx_id_prefix: &[u8],
    timeout: Duration,
    utxos: &[RpcUtxosByAddressesEntry],
    entry_deposit: Option<&EntryDepositOutput>,
) -> Result<(u64, KaspaTransaction), String> {
    if utxos.is_empty() {
        return Err(format!(
            "IGRA submit error: insufficient Kaspa UTXOs for fee payment (source address: {source_address})"
        ));
    }

    if tx_id_prefix.is_empty() {
        return Err("IGRA config error: `tx_id_prefix` cannot be empty".to_string());
    }

    // IGRA payload: 1-byte header + L2Data + 4-byte nonce.
    let payload_len = 1usize.saturating_add(l2data.len()).saturating_add(4);

    let mut sorted = utxos.to_vec();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.utxo_entry.amount));
    let deposit_amount = entry_deposit.map(|deposit| deposit.amount_sompi).unwrap_or_default();
    let minimum_change = if entry_deposit.is_some() { 0 } else { MIN_CHANGE_SOMPI };
    let source_script_public_key = pay_to_address_script(source_address);
    let mass_calculator =
        KaspaMassCalculator::new_with_consensus_params(&KaspaParams::from(network_type));

    let mut fee_floor = initial_fee_sompi(payload_len, 1);
    for attempt in 0..FEE_SELECTION_ATTEMPTS {
        let mut selected = Vec::new();
        let mut total_input = 0u64;
        for entry in sorted.iter().cloned() {
            total_input = total_input.saturating_add(entry.utxo_entry.amount);
            selected.push(entry);
            let selected_fee = fee_floor.max(initial_fee_sompi(payload_len, selected.len()));
            let required_total =
                deposit_amount.saturating_add(selected_fee).saturating_add(minimum_change);
            if total_input >= required_total {
                break;
            }
        }

        let fee = fee_floor.max(initial_fee_sompi(payload_len, selected.len()));
        let required_total = deposit_amount.saturating_add(fee).saturating_add(minimum_change);
        if total_input < required_total {
            return Err(format!(
                "IGRA submit error: insufficient Kaspa UTXOs for fee payment (source address: {source_address})"
            ));
        }

        let inputs = selected
            .iter()
            .map(|entry| {
                KaspaTransactionInput::new(entry.outpoint.clone().into(), Vec::new(), 0, 1)
            })
            .collect::<Vec<_>>();
        let mut outputs = Vec::new();
        if let Some(entry_deposit) = entry_deposit {
            outputs.push(KaspaTransactionOutput::new(
                entry_deposit.amount_sompi,
                ScriptPublicKey::from_vec(0, entry_deposit.lock_script_pubkey.clone()),
            ));
            let change_value =
                total_input.saturating_sub(entry_deposit.amount_sompi).saturating_sub(fee);
            if change_value >= MIN_CHANGE_SOMPI {
                outputs.push(KaspaTransactionOutput::new(
                    change_value,
                    source_script_public_key.clone(),
                ));
            }
        } else {
            let output_value = total_input.saturating_sub(fee);
            if output_value < MIN_CHANGE_SOMPI {
                return Err(format!(
                    "IGRA submit error: insufficient Kaspa UTXOs for fee payment (source address: {source_address})"
                ));
            }
            outputs
                .push(KaspaTransactionOutput::new(output_value, source_script_public_key.clone()));
        }

        let payload = build_payload_with_nonce(payload_header, l2data, 0);
        let nonce_offset = payload.len().saturating_sub(4);
        let mut tx =
            KaspaTransaction::new(0, inputs, outputs, 0, SubnetworkId::default(), 0, payload);

        let start = Instant::now();
        let mut nonce = 0_u32;
        loop {
            if start.elapsed() > timeout {
                return Err(format!(
                    "{IGRA_MINING_TIMEOUT_ERROR_CODE}: timed out mining kaspa txid prefix after {}ms",
                    timeout.as_millis()
                ));
            }

            tx.payload[nonce_offset..].copy_from_slice(&nonce.to_be_bytes());
            tx.finalize();
            let tx_id = tx.id();
            if tx_id.as_bytes().starts_with(tx_id_prefix) {
                break;
            }

            nonce = nonce.wrapping_add(1);
            if nonce == 0 {
                // Extremely unlikely: exhausted full u32 space. Perturb outputs to create variance.
                if let Some(first) = tx.outputs.first_mut() {
                    first.value = first.value.saturating_sub(1);
                }
                tx.finalize();
            }
        }

        // Sign the mined transaction once.
        // Safety: verify txid prefix on the fully signed transaction as well.
        let entries = selected
            .iter()
            .map(|entry| KaspaUtxoEntry {
                amount: entry.utxo_entry.amount,
                script_public_key: entry.utxo_entry.script_public_key.clone(),
                block_daa_score: entry.utxo_entry.block_daa_score,
                is_coinbase: entry.utxo_entry.is_coinbase,
            })
            .collect::<Vec<_>>();

        let signable = KaspaSignableTransaction::with_entries(tx, entries);
        let signed = kaspa_sign_with_multiple_v2(signable, std::slice::from_ref(private_key))
            .fully_signed()
            .map_err(|err| format!("IGRA submit error: failed to sign Kaspa tx: {err}"))?;
        kaspa_verify(&signed.as_verifiable())
            .map_err(|err| format!("IGRA submit error: invalid Kaspa signature set: {err}"))?;

        if !signed.tx.id().as_bytes().starts_with(tx_id_prefix) {
            return Err("IGRA submit error: mined Kaspa txid prefix changed after signing; refusing to broadcast".to_string());
        }

        let non_contextual = mass_calculator.calc_non_contextual_masses(&signed.tx);
        let contextual =
            mass_calculator.calc_contextual_masses(&signed.as_verifiable()).ok_or_else(|| {
                "IGRA submit error: failed to calculate Kaspa tx storage mass".to_string()
            })?;
        let mass = contextual.max(non_contextual);
        if mass > MAX_STANDARD_KASPA_TX_MASS {
            return Err(format!(
                "IGRA submit error: Kaspa transaction mass {mass} exceeds standard limit {MAX_STANDARD_KASPA_TX_MASS}"
            ));
        }

        let output_total =
            signed.tx.outputs.iter().fold(0_u64, |sum, output| sum.saturating_add(output.value));
        let actual_fee = total_input.saturating_sub(output_total);
        let required_relay_fee = minimum_relay_fee_sompi_for_mass(mass);
        if actual_fee >= required_relay_fee {
            let tx = signed.tx;
            tx.set_mass(mass);
            return Ok((nonce as u64, tx));
        }

        if attempt + 1 == FEE_SELECTION_ATTEMPTS {
            return Err(format!(
                "IGRA submit error: Kaspa transaction fee {actual_fee} is below required relay fee {required_relay_fee} for mass {mass}"
            ));
        }

        fee_floor = required_relay_fee;
    }

    Err("IGRA submit error: failed to select a sufficient Kaspa relay fee".to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn next_request_counter() -> u64 {
    REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn build_correlation_id(l2_tx_hash: &str) -> String {
    let millis = now_ms();
    let pid = std::process::id();
    let counter = next_request_counter();
    let entropy = format!("{l2_tx_hash}:{millis}:{pid}:{counter}");
    let digest = hex::encode(keccak256(entropy.as_bytes()));
    let random_hex = &digest[..8];
    format!("{millis}-{pid}-{counter}-{random_hex}")
}

fn build_lock_owner_id(sender: &str, nonce: u64, correlation_id: &str) -> String {
    format!("{sender}:{nonce}:{correlation_id}")
}

impl<T> Service<RequestPacket> for IgraTransport<T>
where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Clone
        + Send
        + 'static,
    T::Future: Send + 'static,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    #[inline]
    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, req: RequestPacket) -> Self::Future {
        self.request(req)
    }
}

impl<T> Service<RequestPacket> for &IgraTransport<T>
where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Clone
        + Send
        + 'static,
    T::Future: Send + 'static,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    #[inline]
    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, req: RequestPacket) -> Self::Future {
        self.request(req)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IGRA_EIP4844_UNSUPPORTED_ERROR, IGRA_EIP7702_UNSUPPORTED_ERROR,
        IGRA_SEND_TRANSACTION_UNSUPPORTED_ERROR, IgraPayloadSubmitter, IgraTransport,
        IgraTransportConfig, KaspaAddress, KaspaAddressPrefix, KaspaAddressVersion,
        kaspa_address_from_private_key, resolve_mnemonic_private_key,
    };
    use crate::igra_store::{
        IGRA_NONCE_REPLACEMENT_CANDIDATE_ERROR_CODE, IgraStore, IgraStoreConfig, TxLifecycleState,
        TxLifecycleUpdate,
    };
    use alloy_json_rpc::{Id, Request, RequestPacket, Response, ResponsePacket, ResponsePayload};
    use alloy_primitives::{hex, utils::keccak256};
    use alloy_transport::{TransportError, TransportFut};
    use foundry_config::IgraKaspaWalletConfig;
    use serde_json::value::RawValue;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tower::Service;

    #[test]
    #[ignore = "development helper: prints derived Kaspa addresses for candidate derivation schemes"]
    fn probe_kaspa_mnemonic_derivation_candidates() {
        let mnemonic = "test test test test test test test test test test test junk";
        let expected = "kaspatest:qzf364tlnl7ja0w65ydu0m5l70pur2hcm3l3ahkmhs660zcyf7cvuf6uznufr";

        let schemes = [
            ("gen1_default_receive_idx0", None),
            ("gen1_full_m_44_111111_0_0_0", Some("m/44'/111111'/0'/0/0")),
            ("gen1_full_m_44_111111_0_0_1", Some("m/44'/111111'/0'/0/1")),
            ("gen1_full_m_45_111111_0_0_0", Some("m/45'/111111'/0'/0/0")),
            ("gen1_depth3_m_44_111111_0", Some("m/44'/111111'/0'")),
            ("gen1_depth4_m_44_111111_0_0", Some("m/44'/111111'/0'/0")),
            ("gen1_depth4_change1_m_44_111111_0_1", Some("m/44'/111111'/0'/1")),
            ("legacy_gen0_m_44_972_0_0_0", Some("m/44'/972/0'/0'/0'")),
            ("legacy_gen0_m_44_972_0_0_1", Some("m/44'/972/0'/0'/1'")),
            ("eth_like_m_44_60_0_0_0", Some("m/44'/60'/0'/0/0")),
        ];

        for (name, path) in schemes {
            let private_key =
                resolve_mnemonic_private_key(mnemonic, None, path, 0).expect("derive kaspa key");
            let address = kaspa_address_from_private_key(&private_key, KaspaAddressPrefix::Testnet)
                .expect("derive kaspa address")
                .to_string();
            println!("{name}: {address}{}", if address == expected { "  <== MATCH" } else { "" });
        }

        // Some CLIs display a 4-byte account id/fingerprint that might also be used as the account index.
        // The sample CLI output shows `[1a1f47ce]`.
        let maybe_account_index = 0x1a1f_47ceu32;
        let maybe_path = format!("m/44'/111111'/{maybe_account_index}'/0/0");
        let maybe_key = resolve_mnemonic_private_key(mnemonic, None, Some(&maybe_path), 0)
            .expect("derive kaspa key");
        let maybe_addr = kaspa_address_from_private_key(&maybe_key, KaspaAddressPrefix::Testnet)
            .expect("derive kaspa address")
            .to_string();
        println!(
            "maybe_account_index_1a1f47ce: {maybe_addr}{}",
            if maybe_addr == expected { "  <== MATCH" } else { "" }
        );

        // Some wallet implementations do not use account_index=0 for the first visible account.
        // Brute force a reasonable range for the canonical BIP44 receive address (index 0).
        for account_index in 0u32..=2000u32 {
            let path = format!("m/44'/111111'/{account_index}'/0/0");
            let private_key = resolve_mnemonic_private_key(mnemonic, None, Some(&path), 0)
                .expect("derive kaspa key");
            let address = kaspa_address_from_private_key(&private_key, KaspaAddressPrefix::Testnet)
                .expect("derive kaspa address")
                .to_string();
            if address == expected {
                println!("ACCOUNT_INDEX_MATCH path={path} address={address}");
                break;
            }
        }

        // Also probe BIP32 hardened/non-hardened combinations for the "shape"
        // m/<purpose>/<coin>/<account>/<change>/<index>, fixing account=0, change=0, index=0.
        // This is the most likely source of an unexpected default deposit address mismatch.
        let purpose_vals = [44u32, 45u32];
        let coin_vals = [111111u32, 972u32, 60u32];
        let bools = [false, true];
        let seg = |n: u32, hardened: bool| -> String {
            if hardened { format!("{n}'") } else { n.to_string() }
        };
        for purpose in purpose_vals {
            for purpose_h in bools {
                for coin in coin_vals {
                    for coin_h in bools {
                        for acct_h in bools {
                            for change_h in bools {
                                for idx_h in bools {
                                    let path = format!(
                                        "m/{}/{}/{}/{}/{}",
                                        seg(purpose, purpose_h),
                                        seg(coin, coin_h),
                                        seg(0, acct_h),
                                        seg(0, change_h),
                                        seg(0, idx_h),
                                    );
                                    let private_key = resolve_mnemonic_private_key(
                                        mnemonic,
                                        None,
                                        Some(&path),
                                        0,
                                    )
                                    .expect("derive kaspa key");
                                    let address = kaspa_address_from_private_key(
                                        &private_key,
                                        KaspaAddressPrefix::Testnet,
                                    )
                                    .expect("derive kaspa address")
                                    .to_string();
                                    if address == expected {
                                        println!("COMBO_MATCH path={path} address={address}");
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        println!("COMBO_MATCH not found in probed BIP32 shape set");
    }

    #[test]
    fn kaspa_address_version_of_expected_deposit_address_is_pubkey() {
        let expected = "kaspatest:qzf364tlnl7ja0w65ydu0m5l70pur2hcm3l3ahkmhs660zcyf7cvuf6uznufr";
        let addr = KaspaAddress::try_from(expected).expect("parse kaspa address");
        assert_eq!(addr.prefix, KaspaAddressPrefix::Testnet);
        assert_eq!(addr.version, KaspaAddressVersion::PubKey);
        assert_eq!(addr.payload.len(), 32);
    }

    #[test]
    fn kaspa_mnemonic_default_deposit_address_matches_expected_with_non_empty_passphrase() {
        // This matches the address shown by the user's `kaspa-cli` for this mnemonic when the
        // BIP39 passphrase (aka "recovery passphrase") is set to the same 12-word string.
        //
        // If the passphrase is empty, the derived seed and thus the deposit address will differ.
        let mnemonic = "test test test test test test test test test test test junk";
        let private_key = resolve_mnemonic_private_key(mnemonic, Some(mnemonic), None, 0)
            .expect("derive kaspa key");
        let address = kaspa_address_from_private_key(&private_key, KaspaAddressPrefix::Testnet)
            .expect("derive kaspa address");

        // Requirement: must match the deposit address shown by `kaspa-cli` for this mnemonic.
        let expected = "kaspatest:qzf364tlnl7ja0w65ydu0m5l70pur2hcm3l3ahkmhs660zcyf7cvuf6uznufr";
        assert_eq!(address.to_string(), expected);
    }

    #[test]
    fn kaspa_mnemonic_default_deposit_address_differs_with_empty_passphrase() {
        // Same mnemonic but empty passphrase.
        let mnemonic = "test test test test test test test test test test test junk";
        let private_key =
            resolve_mnemonic_private_key(mnemonic, None, None, 0).expect("derive kaspa key");
        let address = kaspa_address_from_private_key(&private_key, KaspaAddressPrefix::Testnet)
            .expect("derive kaspa address");

        // Documented behavior: empty passphrase is a different seed, thus different address.
        assert_ne!(
            address.to_string(),
            "kaspatest:qzf364tlnl7ja0w65ydu0m5l70pur2hcm3l3ahkmhs660zcyf7cvuf6uznufr"
        );
        assert_eq!(
            address.to_string(),
            "kaspatest:qzy7rgry649xpl6czj3ferxle8ls5ent0eg39xuhmujup0jlwsq3g67auy2y6"
        );
    }

    #[derive(Clone, Debug, Default)]
    struct RecordingTransport {
        calls: Arc<AtomicUsize>,
    }

    impl RecordingTransport {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Service<RequestPacket> for RecordingTransport {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: RequestPacket) -> Self::Future {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(success_response(request)) })
        }
    }

    #[derive(Clone, Debug)]
    struct RecordingSubmitter {
        calls: Arc<AtomicUsize>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        delay: Duration,
        result: Result<super::IgraSubmitResult, String>,
    }

    impl RecordingSubmitter {
        fn success() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: Arc::new(AtomicUsize::new(0)),
                delay: Duration::from_millis(0),
                result: Ok(super::IgraSubmitResult {
                    kaspa_tx_id: "kaspa-tx-id-1".to_string(),
                    payload_nonce: 0,
                }),
            }
        }

        fn failure(message: &str) -> Self {
            Self { result: Err(message.to_string()), ..Self::success() }
        }

        fn delayed_success(delay: Duration) -> Self {
            Self { delay, ..Self::success() }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn max_in_flight(&self) -> usize {
            self.max_in_flight.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl IgraPayloadSubmitter for RecordingSubmitter {
        async fn submit_payload(
            &self,
            _request: &super::IgraSubmitRequest,
        ) -> Result<super::IgraSubmitResult, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(current, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn success_response(request: RequestPacket) -> ResponsePacket {
        match request {
            RequestPacket::Single(request) => ResponsePacket::Single(Response {
                id: request.id().clone(),
                payload: ResponsePayload::Success(raw_null()),
            }),
            RequestPacket::Batch(requests) => ResponsePacket::Batch(
                requests
                    .into_iter()
                    .map(|request| Response {
                        id: request.id().clone(),
                        payload: ResponsePayload::Success(raw_null()),
                    })
                    .collect(),
            ),
        }
    }

    fn raw_null() -> Box<RawValue> {
        RawValue::from_string("null".to_string()).expect("null is valid JSON")
    }

    fn single_success_string(response: ResponsePacket) -> String {
        match response {
            ResponsePacket::Single(response) => match response.payload {
                ResponsePayload::Success(raw) => serde_json::from_str(raw.get())
                    .expect("success payload should decode as string"),
                ResponsePayload::Failure(err) => panic!("unexpected failure payload: {err:?}"),
            },
            ResponsePacket::Batch(_) => panic!("expected single response packet"),
        }
    }

    fn request_packet(method: &str) -> RequestPacket {
        let request: Request<Vec<()>> = Request::new(method.to_string(), Id::Number(1), vec![]);
        RequestPacket::Single(request.serialize().expect("request serialization should succeed"))
    }

    fn send_raw_packet(raw_tx_bytes: &[u8]) -> RequestPacket {
        let encoded = format!("0x{}", hex::encode(raw_tx_bytes));
        let request: Request<Vec<String>> =
            Request::new("eth_sendRawTransaction".to_string(), Id::Number(1), vec![encoded]);
        RequestPacket::Single(request.serialize().expect("request serialization should succeed"))
    }

    fn test_store(name: &str) -> IgraStore {
        let path = std::env::temp_dir().join(format!(
            "foundry-igra-transport-tests-{name}-{}-{}.sqlite",
            std::process::id(),
            super::now_ms()
        ));
        IgraStore::new(IgraStoreConfig {
            db_path: Some(path),
            kaspa_network: Some("testnet-10".to_string()),
            expected_el_chain_id: Some(1337),
            ..Default::default()
        })
        .expect("store initialization should succeed")
    }

    fn test_transport_config() -> IgraTransportConfig {
        IgraTransportConfig {
            tx_id_prefix: Some("00".to_string()),
            mining_timeout_secs: Some(2),
            kaspa_rpc_url: Some("grpc://127.0.0.1:16110".to_string()),
            kaspa_network: Some("testnet-10".to_string()),
            payload_compression: None,
            logic_zone: None,
            kaspa_wallet: IgraKaspaWalletConfig::default(),
        }
    }

    fn q_test_transport_config() -> IgraTransportConfig {
        IgraTransportConfig { logic_zone: Some("falcon-l5".to_string()), ..test_transport_config() }
    }

    fn batch_request_packet(methods: &[&str]) -> RequestPacket {
        let requests = methods
            .iter()
            .enumerate()
            .map(|(idx, method)| {
                if *method == "eth_sendRawTransaction" {
                    let raw = format!("0x{}", hex::encode([0xc0]));
                    let request: Request<Vec<String>> = Request::new(
                        (*method).to_string(),
                        Id::Number((idx as u64) + 1),
                        vec![raw],
                    );
                    request.serialize().expect("request serialization should succeed")
                } else {
                    let request: Request<Vec<()>> =
                        Request::new((*method).to_string(), Id::Number((idx as u64) + 1), vec![]);
                    request.serialize().expect("request serialization should succeed")
                }
            })
            .collect();
        RequestPacket::Batch(requests)
    }

    #[tokio::test]
    async fn igra_transport_allows_raw_signed_send() {
        let inner = RecordingTransport::default();
        let submitter = RecordingSubmitter::success();
        let transport = IgraTransport::new(inner.clone(), true)
            .with_transport_config(test_transport_config())
            .with_submitter_for_tests(Arc::new(submitter.clone()));
        // Valid legacy signed transaction (nonce=2), copied from existing test fixtures.
        let raw_tx = hex::decode("f86b02843b9aca00830186a094d3e8763675e4c425df46cc3b5c0f6cbdac39604687038d7ea4c68000802ba00eb96ca19e8a77102767a41fc85a36afd5c61ccb09911cec5d3e86e193d9c5aea03a456401896b1b6055311536bf00a718568c744d8c1f9df59879e8350220ca18")
            .expect("raw tx hex should decode");
        let expected_l2_hash = format!("0x{}", hex::encode(keccak256(&raw_tx)));

        let response = transport
            .request(send_raw_packet(&raw_tx))
            .await
            .expect("eth_sendRawTransaction should be intercepted in IGRA mode");

        assert_eq!(single_success_string(response), expected_l2_hash);
        assert_eq!(inner.calls(), 0, "inner transport should not be called");
        assert_eq!(submitter.calls(), 1, "submitter should be called once");
    }

    #[tokio::test]
    async fn igra_transport_allows_eip2930_raw_txs() {
        let inner = RecordingTransport::default();
        let submitter = RecordingSubmitter::success();
        let transport = IgraTransport::new(inner.clone(), true)
            .with_transport_config(test_transport_config())
            .with_submitter_for_tests(Arc::new(submitter.clone()));

        use alloy_consensus::{Signed, TxEip2930};
        use alloy_network::TxSignerSync;
        use alloy_primitives::{Bytes, TxKind, U256, address};
        use alloy_signer_local::PrivateKeySigner;

        let signer = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
            .parse::<PrivateKeySigner>()
            .expect("signer parse");
        let mut tx = TxEip2930 {
            chain_id: 1,
            nonce: 0,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(address!("d3e8763675e4c425df46cc3b5c0f6cbdac396046")),
            value: U256::ZERO,
            input: Bytes::default(),
            access_list: Default::default(),
        };
        let sig = signer.sign_transaction_sync(&mut tx).expect("sign eip2930");
        let signed = Signed::new_unhashed(tx, sig);
        let mut raw_tx = Vec::with_capacity(signed.eip2718_encoded_length());
        signed.eip2718_encode(&mut raw_tx);

        transport
            .request(send_raw_packet(&raw_tx))
            .await
            .expect("EIP-2930 raw tx should be intercepted in IGRA mode");

        assert_eq!(inner.calls(), 0, "inner transport should not be called");
        assert_eq!(submitter.calls(), 1, "submitter should be called once");
    }

    #[tokio::test]
    async fn igra_transport_allows_eip1559_raw_txs() {
        let inner = RecordingTransport::default();
        let submitter = RecordingSubmitter::success();
        let transport = IgraTransport::new(inner.clone(), true)
            .with_transport_config(test_transport_config())
            .with_submitter_for_tests(Arc::new(submitter.clone()));

        use alloy_consensus::{Signed, TxEip1559};
        use alloy_network::TxSignerSync;
        use alloy_primitives::{Bytes, TxKind, U256, address};
        use alloy_signer_local::PrivateKeySigner;

        let signer = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
            .parse::<PrivateKeySigner>()
            .expect("signer parse");
        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(address!("d3e8763675e4c425df46cc3b5c0f6cbdac396046")),
            value: U256::ZERO,
            input: Bytes::default(),
            access_list: Default::default(),
        };
        let sig = signer.sign_transaction_sync(&mut tx).expect("sign eip1559");
        let signed = Signed::new_unhashed(tx, sig);
        let mut raw_tx = Vec::with_capacity(signed.eip2718_encoded_length());
        signed.eip2718_encode(&mut raw_tx);

        transport
            .request(send_raw_packet(&raw_tx))
            .await
            .expect("EIP-1559 raw tx should be intercepted in IGRA mode");

        assert_eq!(inner.calls(), 0, "inner transport should not be called");
        assert_eq!(submitter.calls(), 1, "submitter should be called once");
    }

    #[tokio::test]
    async fn igra_transport_allows_falcon_l5_q_raw_tx_without_evm_decode() {
        let inner = RecordingTransport::default();
        let submitter = RecordingSubmitter::success();
        let transport = IgraTransport::new(inner.clone(), true)
            .with_transport_config(q_test_transport_config())
            .with_submitter_for_tests(Arc::new(submitter.clone()));
        let raw_tx = [super::IGRA_FALCON_L5_TX_TYPE, 0xf0, 0x0d];
        let expected_l2_hash = format!("0x{}", hex::encode(keccak256(raw_tx)));

        let response = transport
            .request(send_raw_packet(&raw_tx))
            .await
            .expect("Falcon-L5 q raw tx should be intercepted in IGRA q-zone mode");

        assert_eq!(single_success_string(response), expected_l2_hash);
        assert_eq!(inner.calls(), 0, "inner transport should not be called");
        assert_eq!(submitter.calls(), 1, "submitter should be called once");
    }

    #[tokio::test]
    async fn igra_transport_persists_happy_path_lifecycle_sequence() {
        let inner = RecordingTransport::default();
        let store = test_store("happy-sequence");
        let store_reader = store.clone();
        let submitter = RecordingSubmitter::success();
        let transport = IgraTransport::new(inner.clone(), true)
            .with_store_for_tests(store)
            .with_transport_config(test_transport_config())
            .with_submitter_for_tests(Arc::new(submitter.clone()));

        // Valid legacy signed transaction (nonce=2), copied from existing test fixtures.
        let raw_tx = hex::decode("f86b02843b9aca00830186a094d3e8763675e4c425df46cc3b5c0f6cbdac39604687038d7ea4c68000802ba00eb96ca19e8a77102767a41fc85a36afd5c61ccb09911cec5d3e86e193d9c5aea03a456401896b1b6055311536bf00a718568c744d8c1f9df59879e8350220ca18")
            .expect("raw tx hex should decode");
        let l2_tx_hash = format!("0x{}", hex::encode(keccak256(&raw_tx)));

        transport
            .request(send_raw_packet(&raw_tx))
            .await
            .expect("raw tx should be intercepted and persisted");

        assert_eq!(inner.calls(), 0, "inner transport should not be called");
        assert_eq!(submitter.calls(), 1, "submitter should be called once");

        let record = store_reader
            .load_tx(&l2_tx_hash)
            .expect("store read should succeed")
            .expect("tx should be persisted");
        assert_eq!(record.state, TxLifecycleState::KaspaBroadcasted);
        assert_eq!(record.kaspa_tx_id.as_deref(), Some("kaspa-tx-id-1"));
        assert_eq!(record.attempts, 0);
        assert!(record.updated_at_ms >= record.created_at_ms);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn igra_transport_serializes_concurrent_same_sender_requests() {
        let inner = RecordingTransport::default();
        let store = test_store("concurrent-same-sender");
        let submitter = RecordingSubmitter::delayed_success(Duration::from_millis(100));
        let transport = IgraTransport::new(inner.clone(), true)
            .with_store_for_tests(store)
            .with_transport_config(test_transport_config())
            .with_submitter_for_tests(Arc::new(submitter.clone()));

        let raw_tx = hex::decode("f86b02843b9aca00830186a094d3e8763675e4c425df46cc3b5c0f6cbdac39604687038d7ea4c68000802ba00eb96ca19e8a77102767a41fc85a36afd5c61ccb09911cec5d3e86e193d9c5aea03a456401896b1b6055311536bf00a718568c744d8c1f9df59879e8350220ca18")
            .expect("raw tx hex should decode");
        let packet_1 = send_raw_packet(&raw_tx);
        let packet_2 = send_raw_packet(&raw_tx);

        let transport_1 = transport.clone();
        let transport_2 = transport.clone();
        let handle_1 = tokio::spawn(async move { transport_1.request(packet_1).await });
        let handle_2 = tokio::spawn(async move { transport_2.request(packet_2).await });

        let result_1 = handle_1.await.expect("first request task should complete");
        let result_2 = handle_2.await.expect("second request task should complete");
        result_1.expect("first request should succeed");
        result_2.expect("second request should succeed");

        assert_eq!(inner.calls(), 0, "inner transport should not be called");
        assert_eq!(submitter.calls(), 2, "submitter should be called for both requests");
        assert_eq!(
            submitter.max_in_flight(),
            1,
            "same-sender requests should be serialized behind sender lock"
        );
    }

    #[tokio::test]
    async fn igra_transport_persists_stale_replacement_candidate_observability() {
        let inner = RecordingTransport::default();
        let store = test_store("stale-replacement-observable");
        let store_reader = store.clone();
        let submitter = RecordingSubmitter::success();
        let transport = IgraTransport::new(inner.clone(), true)
            .with_store_for_tests(store)
            .with_transport_config(test_transport_config())
            .with_submitter_for_tests(Arc::new(submitter.clone()));

        let raw_tx = hex::decode("f86b02843b9aca00830186a094d3e8763675e4c425df46cc3b5c0f6cbdac39604687038d7ea4c68000802ba00eb96ca19e8a77102767a41fc85a36afd5c61ccb09911cec5d3e86e193d9c5aea03a456401896b1b6055311536bf00a718568c744d8c1f9df59879e8350220ca18")
            .expect("raw tx hex should decode");
        let request = send_raw_packet(&raw_tx);
        let metadata = match &request {
            RequestPacket::Single(serialized) => {
                IgraTransport::<RecordingTransport>::raw_tx_metadata(serialized)
                    .expect("raw metadata decode should succeed")
            }
            RequestPacket::Batch(_) => {
                unreachable!("send_raw_packet always creates single request")
            }
        };
        let sender = metadata.sender.expect("metadata sender should exist");
        let nonce = metadata.nonce.expect("metadata nonce should exist");
        let incoming_hash = metadata.l2_tx_hash;

        store_reader
            .persist_transition(&TxLifecycleUpdate {
                l2_tx_hash: "0xfeedface".to_string(),
                sender: sender.clone(),
                l2_nonce: nonce,
                payload_nonce: Some(nonce),
                kaspa_tx_id: None,
                state: TxLifecycleState::KaspaBroadcasted,
                correlation_id: "existing-correlation".to_string(),
                last_error_code: None,
                last_error_message: None,
                increment_attempts: false,
            })
            .expect("existing tx_map entry should persist");
        store_reader
            .mark_submitted_in_order_nonce(&sender, nonce)
            .expect("next expected nonce should advance to make incoming nonce stale");

        transport.request(request).await.expect("stale replacement candidate should be allowed");
        assert_eq!(submitter.calls(), 1, "submitter should be called once");

        let record = store_reader
            .load_tx(&incoming_hash)
            .expect("store read should succeed")
            .expect("incoming tx should be persisted");
        assert_eq!(record.state, TxLifecycleState::KaspaBroadcasted);
        assert_eq!(
            record.last_error_code.as_deref(),
            Some(IGRA_NONCE_REPLACEMENT_CANDIDATE_ERROR_CODE)
        );
        assert!(
            record
                .last_error_message
                .as_deref()
                .is_some_and(|message| message.contains("stale nonce replacement candidate")),
            "replacement candidate path should be persisted with explicit observability message"
        );
    }

    #[tokio::test]
    async fn igra_transport_rejects_unsupported_send_methods() {
        let inner = RecordingTransport::default();
        let transport = IgraTransport::new(inner.clone(), true);

        for method in
            ["eth_sendTransaction", "eth_sendTransactionSync", "eth_sendRawTransactionSync"]
        {
            let err = transport
                .request(request_packet(method))
                .await
                .expect_err("unsupported send methods should be rejected in IGRA mode");
            assert!(
                err.to_string().contains(IGRA_SEND_TRANSACTION_UNSUPPORTED_ERROR),
                "expected clear IGRA error for method {method}, got: {err}"
            );
        }

        assert_eq!(inner.calls(), 0, "inner transport should not be called on rejected methods");
    }

    #[tokio::test]
    async fn igra_transport_rejects_eip4844_raw_txs() {
        let inner = RecordingTransport::default();
        let transport = IgraTransport::new(inner.clone(), true);

        let err = transport
            .request(send_raw_packet(&[0x03, 0x00]))
            .await
            .expect_err("EIP-4844 raw tx should be rejected in IGRA mode");

        assert!(err.to_string().contains(IGRA_EIP4844_UNSUPPORTED_ERROR));
        assert_eq!(inner.calls(), 0, "inner transport should not be called on rejected tx");
    }

    #[tokio::test]
    async fn igra_transport_rejects_eip7702_raw_txs() {
        let inner = RecordingTransport::default();
        let transport = IgraTransport::new(inner.clone(), true);

        let err = transport
            .request(send_raw_packet(&[0x04, 0x00]))
            .await
            .expect_err("EIP-7702 raw tx should be rejected in IGRA mode");

        assert!(err.to_string().contains(IGRA_EIP7702_UNSUPPORTED_ERROR));
        assert_eq!(inner.calls(), 0, "inner transport should not be called on rejected tx");
    }

    #[tokio::test]
    async fn igra_transport_rejects_unknown_typed_raw_txs() {
        let inner = RecordingTransport::default();
        let transport = IgraTransport::new(inner.clone(), true);

        let err = transport
            .request(send_raw_packet(&[0x7f, 0x00]))
            .await
            .expect_err("unknown typed raw tx should be rejected in IGRA mode");

        assert!(err.to_string().contains("IGRA unsupported transaction type: 0x7f"));
        assert_eq!(inner.calls(), 0, "inner transport should not be called on rejected tx");
    }

    #[tokio::test]
    async fn igra_transport_q_zone_rejects_canonical_raw_txs() {
        let inner = RecordingTransport::default();
        let transport = IgraTransport::new(inner.clone(), true)
            .with_transport_config(q_test_transport_config());

        let err = transport
            .request(send_raw_packet(&[0x02, 0x00]))
            .await
            .expect_err("canonical EIP-1559 raw tx should be rejected in q-zone mode");

        assert!(err.to_string().contains(super::IGRA_Q_RAW_TRANSACTION_ERROR));
        assert!(err.to_string().contains("expected Falcon-L5 q transaction type 0x7c"));
        assert_eq!(inner.calls(), 0, "inner transport should not be called on rejected q tx");
    }

    #[tokio::test]
    async fn igra_transport_rejects_malformed_send_raw_transaction_params() {
        let inner = RecordingTransport::default();
        let transport = IgraTransport::new(inner.clone(), true);

        let non_hex_request: Request<Vec<String>> = Request::new(
            "eth_sendRawTransaction".to_string(),
            Id::Number(1),
            vec!["not-hex".to_string()],
        );
        let missing_param_request: Request<Vec<String>> =
            Request::new("eth_sendRawTransaction".to_string(), Id::Number(2), vec![]);

        for request in [non_hex_request, missing_param_request] {
            let err = transport
                .request(RequestPacket::Single(
                    request.serialize().expect("request serialization should succeed"),
                ))
                .await
                .expect_err("malformed eth_sendRawTransaction params should be rejected");
            assert!(
                err.to_string().contains("IGRA raw transaction decode error"),
                "expected IGRA decode error, got: {err}"
            );
        }

        assert_eq!(inner.calls(), 0, "inner transport should not be called on rejected tx");
    }

    #[tokio::test]
    async fn igra_transport_rejects_batch_with_unsupported_send_method() {
        let inner = RecordingTransport::default();
        let transport = IgraTransport::new(inner.clone(), true);

        let err = transport
            .request(batch_request_packet(&[
                "eth_blockNumber",
                "eth_sendTransaction",
                "eth_chainId",
            ]))
            .await
            .expect_err("batch containing unsupported send method should be rejected in IGRA mode");

        assert!(
            err.to_string().contains(IGRA_SEND_TRANSACTION_UNSUPPORTED_ERROR),
            "expected clear IGRA error for rejected batch, got: {err}"
        );
        assert_eq!(inner.calls(), 0, "inner transport should not be called on rejected batch");
    }

    #[tokio::test]
    async fn igra_transport_allows_batch_with_only_allowed_methods() {
        let inner = RecordingTransport::default();
        let transport = IgraTransport::new(inner.clone(), true);

        transport
            .request(batch_request_packet(&[
                "eth_sendRawTransaction",
                "eth_blockNumber",
                "eth_chainId",
            ]))
            .await
            .expect("batch with only allowed methods should pass through in IGRA mode");

        assert_eq!(inner.calls(), 1, "inner transport should be called once");
    }

    #[tokio::test]
    async fn igra_transport_allows_non_send_method_in_igra_mode() {
        let inner = RecordingTransport::default();
        let transport = IgraTransport::new(inner.clone(), true);

        transport
            .request(request_packet("eth_blockNumber"))
            .await
            .expect("non-send method should pass through in IGRA mode");

        assert_eq!(inner.calls(), 1, "inner transport should be called once");
    }

    #[tokio::test]
    async fn igra_transport_disabled_mode_passes_through() {
        let inner = RecordingTransport::default();
        let transport = IgraTransport::new(inner.clone(), false);

        transport
            .request(request_packet("eth_sendTransaction"))
            .await
            .expect("disabled IGRA mode should pass through");

        assert_eq!(inner.calls(), 1, "inner transport should be called once");
    }

    #[test]
    fn igra_payload_format_prefixes_header_and_appends_be_u32_nonce() {
        let raw_tx = [0x01, 0x02, 0x03];
        let payload = super::build_payload_with_nonce(0x94, &raw_tx, 0x01020304);
        assert_eq!(payload[0], 0x94, "expected IGRA (v=0x9, type=0x4) header");
        assert_eq!(&payload[1..4], &raw_tx);
        assert_eq!(&payload[4..8], &[0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn igra_q_zone_payload_wraps_q_raw_tx_under_0x9f() {
        let raw_tx = [super::IGRA_FALCON_L5_TX_TYPE, 0x01, 0x02];
        let payload_data = super::build_igra_l2data(
            &raw_tx,
            None,
            Some("falcon-l5"),
            super::IgraPayloadKind::FalconL5RawTx,
        )
        .expect("q l2data builds");
        let payload =
            super::build_payload_with_nonce(payload_data.header, &payload_data.l2data, 0x01020304);

        assert_eq!(payload_data.header, 0x9f);
        assert_eq!(payload[0], 0x9f);
        assert_eq!(&payload[1..5], &[0x01, 0x00, 0x02, 0x04]);
        assert_eq!(&payload[5..8], &raw_tx);
        assert_eq!(&payload[8..12], &[0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn igra_q_zone_payload_wraps_q_entry_under_0x9f() {
        let mut entry = [0u8; super::IGRA_Q_ENTRY_BYTES];
        entry[..20].copy_from_slice(&[0x11; 20]);
        entry[20..].copy_from_slice(&123_456u64.to_le_bytes());
        let payload_data = super::build_igra_l2data(
            &entry,
            None,
            Some("falcon-l5"),
            super::IgraPayloadKind::FalconL5Entry,
        )
        .expect("q entry l2data builds");
        let payload =
            super::build_payload_with_nonce(payload_data.header, &payload_data.l2data, 0x01020304);

        assert_eq!(payload_data.header, 0x9f);
        assert_eq!(payload[0], 0x9f);
        assert_eq!(&payload[1..5], &[0x01, 0x00, 0x02, 0x02]);
        assert_eq!(&payload[5..33], &entry);
        assert_eq!(&payload[33..37], &[0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn igra_q_entry_deposit_output_uses_payload_amount_and_lock_script() {
        let mut entry = [0u8; super::IGRA_Q_ENTRY_BYTES];
        entry[..20].copy_from_slice(&[0x11; 20]);
        entry[20..].copy_from_slice(&123_456u64.to_le_bytes());

        let deposit = super::entry_deposit_output_for_request(
            super::IgraPayloadKind::FalconL5Entry,
            &entry,
            Some("0x203705fd"),
        )
        .expect("q Entry output mode builds")
        .expect("q Entry uses deposit output");

        assert_eq!(deposit.amount_sompi, 123_456);
        assert_eq!(deposit.lock_script_pubkey, hex::decode("203705fd").unwrap());
    }

    #[test]
    fn igra_q_entry_deposit_output_requires_lock_script() {
        let entry = [0u8; super::IGRA_Q_ENTRY_BYTES];

        let err = super::entry_deposit_output_for_request(
            super::IgraPayloadKind::FalconL5Entry,
            &entry,
            None,
        )
        .expect_err("q Entry requires lock script");

        assert!(err.contains("entry_lock_script_pubkey"));
    }

    #[test]
    fn igra_q_entry_deposit_output_is_not_used_for_q_raw_tx() {
        let output = super::entry_deposit_output_for_request(
            super::IgraPayloadKind::FalconL5RawTx,
            &[super::IGRA_FALCON_L5_TX_TYPE],
            None,
        )
        .expect("q RawTx should not require entry lock script");

        assert!(output.is_none());
    }

    #[test]
    fn igra_q_zone_payload_enforces_shared_vanilla_l2data_cap() {
        let raw_tx = vec![super::IGRA_FALCON_L5_TX_TYPE; super::IGRA_Q_RAW_TX_MAX_BYTES];
        let payload_data = super::build_igra_l2data(
            &raw_tx,
            None,
            Some("falcon-l5"),
            super::IgraPayloadKind::FalconL5RawTx,
        )
        .expect("q tx at cap builds");
        assert_eq!(payload_data.l2data.len(), super::IGRA_MAX_L2DATA_BYTES);

        let oversized = vec![super::IGRA_FALCON_L5_TX_TYPE; super::IGRA_Q_RAW_TX_MAX_BYTES + 1];
        let err = super::build_igra_l2data(
            &oversized,
            None,
            Some("falcon-l5"),
            super::IgraPayloadKind::FalconL5RawTx,
        )
        .expect_err("q tx above cap rejected");
        assert!(err.contains("raw q transaction size"));
        assert!(err.contains(&super::IGRA_Q_RAW_TX_MAX_BYTES.to_string()));
    }

    #[test]
    fn igra_kaspa_relay_fee_uses_current_minimum_fee_floor() {
        assert_eq!(super::minimum_relay_fee_sompi_for_mass(7_304), 730_400);
        assert_eq!(super::minimum_relay_fee_sompi_for_mass(1), 100);
        assert_eq!(
            super::minimum_relay_fee_sompi_for_mass(0),
            super::CURRENT_KASPA_MIN_RELAY_FEE_PER_KG_SOMPI
        );
    }

    #[test]
    fn igra_q_zone_payload_rejects_compression() {
        let raw_tx = [super::IGRA_FALCON_L5_TX_TYPE, 0x01, 0x02];
        let err = super::build_igra_l2data(
            &raw_tx,
            Some("zlib"),
            Some("falcon-l5"),
            super::IgraPayloadKind::FalconL5RawTx,
        )
        .expect_err("q-zone compression is rejected");

        assert!(err.contains("q-zone does not support"));
    }

    #[tokio::test]
    async fn igra_transport_submitter_error_marks_failed_without_inner_call() {
        let inner = RecordingTransport::default();
        let store = test_store("submitter-error");
        let store_reader = store.clone();
        let submitter = RecordingSubmitter::failure("kaspa submit failed");
        let transport = IgraTransport::new(inner.clone(), true)
            .with_store_for_tests(store)
            .with_transport_config(test_transport_config())
            .with_submitter_for_tests(Arc::new(submitter.clone()));

        let raw_tx = hex::decode("f86b02843b9aca00830186a094d3e8763675e4c425df46cc3b5c0f6cbdac39604687038d7ea4c68000802ba00eb96ca19e8a77102767a41fc85a36afd5c61ccb09911cec5d3e86e193d9c5aea03a456401896b1b6055311536bf00a718568c744d8c1f9df59879e8350220ca18")
            .expect("raw tx hex should decode");
        let l2_tx_hash = format!("0x{}", hex::encode(keccak256(&raw_tx)));

        let err = transport
            .request(send_raw_packet(&raw_tx))
            .await
            .expect_err("submitter failure should bubble as transport error");
        assert!(err.to_string().contains("kaspa submit failed"));
        assert_eq!(inner.calls(), 0, "inner transport should not be called");
        assert_eq!(submitter.calls(), 1, "submitter should be called once");

        let record = store_reader
            .load_tx(&l2_tx_hash)
            .expect("store read should succeed")
            .expect("tx should be persisted");
        assert_eq!(record.state, TxLifecycleState::FailedRecoverable);
        assert!(
            record
                .last_error_message
                .as_deref()
                .is_some_and(|msg| msg.contains("kaspa submit failed"))
        );
    }
}
