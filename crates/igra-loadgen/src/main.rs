use alloy_consensus::{Signed, TxEip1559};
use alloy_json_rpc::{Id, Request, RequestPacket, ResponsePacket, ResponsePayload};
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, Bytes, TxKind, U256, hex};
use alloy_signer_local::PrivateKeySigner;
use alloy_transport::TransportError;
use chrono::{SecondsFormat, Utc};
use clap::{Parser, ValueEnum};
use eyre::{Context, Result, eyre};
use foundry_common::provider::{
    igra_transport::{IGRA_MINING_TIMEOUT_ERROR_CODE, IgraTransport, IgraTransportConfig},
    runtime_transport::{RuntimeTransport, RuntimeTransportBuilder},
};
use foundry_config::IgraKaspaWalletConfig;
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
    constants::STORAGE_MASS_PARAMETER as KASPA_STORAGE_MASS_PARAMETER,
    mass::MassCalculator as KaspaMassCalculator,
    network::NetworkType as KaspaNetworkType,
    sign::{sign_with_multiple_v2 as kaspa_sign_with_multiple_v2, verify as kaspa_verify},
    subnets::SubnetworkId,
    tx::{
        SignableTransaction as KaspaSignableTransaction, Transaction as KaspaTransaction,
        TransactionInput as KaspaTransactionInput, TransactionOutput as KaspaTransactionOutput,
        UtxoEntry as KaspaUtxoEntry,
    },
};
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::{RpcTransaction, RpcUtxosByAddressesEntry, api::rpc::RpcApi};
use kaspa_txscript::pay_to_address_script;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, Ordering},
};
use std::time::Duration;
use tokio::signal;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior, interval, timeout};
use tower::Service;

const DEFAULT_WALLETS_JSON: &str = "docs/stress-test-prep/wallets_1000.json";
const DEFAULT_CONTRACT_BASE: &str = "0x0000000000000000000000000000000000005000";
const DEFAULT_CONTRACT_END: &str = "0x00000000000000000000000000000000000053e7";
const MIN_CHANGE_SOMPI: u64 = 1_000;
const MAX_STANDARD_KASPA_TX_MASS: u64 = 100_000;
const FANOUT_TARGET_STORAGE_MASS: u64 = 50_000;

#[derive(Clone, Copy, Debug, ValueEnum, Serialize)]
#[value(rename_all = "kebab-case")]
enum Network {
    Devnet,
    Simnet,
    #[value(name = "testnet-10", alias = "testnet10")]
    Testnet10,
}

impl Network {
    fn as_str(self) -> &'static str {
        match self {
            Self::Devnet => "devnet",
            Self::Simnet => "simnet",
            Self::Testnet10 => "testnet-10",
        }
    }

    fn kaspa_address_prefix(self) -> &'static str {
        match self {
            Self::Devnet => "kaspadev:",
            Self::Simnet => "kaspasim:",
            Self::Testnet10 => "kaspatest:",
        }
    }

    fn default_tx_id_prefix(self) -> &'static str {
        match self {
            Self::Devnet => "01",
            Self::Simnet => "02",
            Self::Testnet10 => "97b4",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum, Serialize)]
#[value(rename_all = "kebab-case")]
enum Mode {
    FullCycle,
    PrebuildSend,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::FullCycle => "full-cycle",
            Self::PrebuildSend => "prebuild-send",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum, Serialize)]
#[value(rename_all = "kebab-case")]
enum RpcSelectionMode {
    RandomPerStep,
    StickyPerInstance,
}

#[derive(Clone, Copy, Debug, ValueEnum, Serialize)]
#[value(rename_all = "kebab-case")]
enum RecipientMode {
    Ring,
    RandomSeeded,
}

#[derive(Debug, Parser)]
#[command(about = "IGRA-Kaspa stress runner")]
struct Args {
    #[arg(long, env = "IGRA_STRESS_NETWORK", value_enum, default_value_t = Network::Devnet)]
    network: Network,

    #[arg(long, env = "IGRA_STRESS_MODE", value_enum, default_value_t = Mode::FullCycle)]
    mode: Mode,

    #[arg(long, env = "IGRA_STRESS_TARGET_TPS")]
    target_tps: Option<f64>,

    #[arg(long, env = "IGRA_STRESS_TPS")]
    legacy_tps: Option<f64>,

    #[arg(long, env = "IGRA_STRESS_INSTANCE_SAFE_TPS", default_value_t = 5.0)]
    instance_safe_tps: f64,

    #[arg(long, env = "IGRA_STRESS_WORKER_COUNT")]
    worker_count: Option<usize>,

    #[arg(long, env = "IGRA_STRESS_WALLET_START_INDEX", default_value_t = 0)]
    wallet_start_index: usize,

    #[arg(long, env = "IGRA_STRESS_DURATION_SECS")]
    duration_secs: Option<u64>,

    #[arg(long, env = "IGRA_STRESS_TOTAL_TXS")]
    total_txs: Option<u64>,

    #[arg(long, env = "IGRA_STRESS_TOTAL_TXS_PER_WORKER")]
    legacy_total_txs_per_worker: Option<u64>,

    #[arg(long, env = "IGRA_STRESS_CALIBRATION_MODE", default_value_t = 0)]
    calibration_mode: u8,

    #[arg(long, env = "IGRA_STRESS_PREFLIGHT_SAMPLE_MODE", default_value_t = 0)]
    preflight_sample_mode: u8,

    #[arg(long, env = "IGRA_STRESS_WALLETS_JSON", default_value = DEFAULT_WALLETS_JSON)]
    wallets_json: String,

    #[arg(long, env = "IGRA_STRESS_CONTRACT_BASE", default_value = DEFAULT_CONTRACT_BASE)]
    contract_base_address: String,

    #[arg(long, env = "IGRA_STRESS_CONTRACT_END", default_value = DEFAULT_CONTRACT_END)]
    contract_end_address: String,

    #[arg(long, env = "IGRA_STRESS_RECIPIENT_MODE", value_enum, default_value_t = RecipientMode::Ring)]
    recipient_mode: RecipientMode,

    #[arg(long, env = "IGRA_STRESS_RECIPIENT_RANDOM_SEED")]
    recipient_random_seed: Option<u64>,

    #[arg(long, env = "IGRA_STRESS_RPC_SELECTION_MODE", value_enum, default_value_t = RpcSelectionMode::RandomPerStep)]
    rpc_selection_mode: RpcSelectionMode,

    #[arg(long, env = "IGRA_STRESS_RPC_RANDOM_SEED")]
    rpc_random_seed: Option<u64>,

    #[arg(long, env = "IGRA_STRESS_RPC_ENDPOINTS_JSON")]
    rpc_endpoints_json: Option<String>,

    #[arg(long, env = "IGRA_STRESS_EL_RPC_URLS")]
    el_rpc_urls: Option<String>,

    #[arg(long, env = "IGRA_STRESS_KASPA_RPC_URLS")]
    kaspa_rpc_urls: Option<String>,

    #[arg(long, env = "IGRA_EL_RPC_URL")]
    legacy_el_rpc_url: Option<String>,

    #[arg(long, env = "IGRA_KASPA_RPC_URL")]
    legacy_kaspa_rpc_url: Option<String>,

    #[arg(long, env = "IGRA_TX_ID_PREFIX")]
    tx_id_prefix: Option<String>,

    #[arg(long, env = "IGRA_MINING_TIMEOUT_SECS", default_value_t = 120)]
    mining_timeout_secs: u64,

    #[arg(long, env = "IGRA_STRESS_WARMUP_TXS_PER_WORKER", default_value_t = 1)]
    warmup_txs_per_worker: u64,

    #[arg(long, env = "IGRA_STRESS_WARMUP_BARRIER_TIMEOUT_SECS", default_value_t = 60)]
    warmup_barrier_timeout_secs: u64,

    #[arg(long, env = "IGRA_STRESS_PENDING_TIMEOUT_SECS", default_value_t = 10)]
    pending_timeout_secs: u64,

    #[arg(long, env = "IGRA_STRESS_MAX_REPLACEMENTS_PER_NONCE", default_value_t = 5)]
    max_replacements_per_nonce: u64,

    #[arg(long, env = "IGRA_STRESS_REPLACEMENT_FEE_BUMP_PCT", default_value_t = 10)]
    replacement_fee_bump_pct: u64,

    #[arg(long, env = "IGRA_STRESS_REPLACEMENT_TIMEOUT_CAP_SECS", default_value_t = 60)]
    replacement_timeout_cap_secs: u64,

    #[arg(long, env = "IGRA_STRESS_SHUTDOWN_GRACE_SECS", default_value_t = 120)]
    shutdown_grace_secs: u64,

    #[arg(long, env = "IGRA_STRESS_DEGRADED_PAUSE_MAX_SECS", default_value_t = 120)]
    degraded_pause_max_secs: u64,

    #[arg(long, env = "IGRA_STRESS_WORKER_RECOVERY_TIMEOUT_SECS", default_value_t = 120)]
    worker_recovery_timeout_secs: u64,

    #[arg(long, env = "IGRA_STRESS_REPORT_SECS", default_value_t = 5)]
    report_secs: u64,

    #[arg(long, env = "IGRA_STRESS_GAS_LIMIT", default_value_t = 80_000)]
    gas_limit: u64,

    #[arg(long, env = "IGRA_STRESS_MAX_FEE_PER_GAS", default_value_t = 2_000_000_000)]
    max_fee_per_gas: u128,

    #[arg(long, env = "IGRA_STRESS_MAX_PRIORITY_FEE_PER_GAS", default_value_t = 1_000_000_000)]
    max_priority_fee_per_gas: u128,

    #[arg(long, env = "IGRA_STRESS_TO")]
    to: Option<String>,

    #[arg(long, env = "IGRA_STRESS_DATA")]
    data: Option<String>,

    #[arg(long, env = "IGRA_STRESS_EVM_KEYS")]
    evm_keys: Option<String>,

    #[arg(long, env = "IGRA_STRESS_KASPA_PRIVATE_KEYS")]
    kaspa_private_keys: Option<String>,

    #[arg(long, env = "IGRA_KASPA_MNEMONIC")]
    kaspa_mnemonic: Option<String>,

    #[arg(long, env = "IGRA_KASPA_MNEMONIC_PASSPHRASE")]
    kaspa_mnemonic_passphrase: Option<String>,

    #[arg(long, env = "IGRA_KASPA_MNEMONIC_PASSPHRASE_AS_MNEMONIC", default_value_t = false)]
    kaspa_mnemonic_passphrase_as_mnemonic: bool,

    #[arg(long, env = "IGRA_KASPA_MNEMONIC_PASSPHRASE_EMPTY", default_value_t = false)]
    kaspa_mnemonic_passphrase_empty: bool,

    #[arg(long, default_value_t = 0)]
    kaspa_mnemonic_index_start: u32,

    #[arg(long)]
    kaspa_mnemonic_count: Option<u32>,

    #[arg(long, env = "ALLOW_SHARED_KASPA_KEY", default_value_t = false)]
    allow_shared_kaspa_key: bool,

    #[arg(long, env = "IGRA_STRESS_KASPA_FEE_MODE", default_value = "estimate")]
    kaspa_fee_mode: String,

    #[arg(long, env = "IGRA_STRESS_KASPA_FEE_BUCKET", default_value = "normal")]
    kaspa_fee_bucket: String,

    #[arg(long, env = "IGRA_STRESS_PREFLIGHT_BALANCE_CHECK", default_value_t = 1)]
    preflight_balance_check: u8,

    #[arg(long, env = "IGRA_STRESS_PREBUILD_HORIZON_SECS", default_value_t = 120)]
    prebuild_horizon_secs: u64,

    #[arg(long, env = "IGRA_STRESS_UTXO_REFILL_LAG_SECS", default_value_t = 20)]
    utxo_refill_lag_secs: u64,

    #[arg(long, env = "IGRA_STRESS_UTXO_SAFETY_FACTOR", default_value_t = 1.5)]
    utxo_safety_factor: f64,

    #[arg(long, env = "IGRA_STRESS_KASPA_FANOUT", default_value_t = 0)]
    kaspa_fanout: u8,

    #[arg(long, env = "IGRA_STRESS_KASPA_FANOUT_SOURCE_PRIVATE_KEY")]
    kaspa_fanout_source_private_key: Option<String>,

    #[arg(long, env = "IGRA_STRESS_KASPA_FANOUT_SOURCE_MNEMONIC")]
    kaspa_fanout_source_mnemonic: Option<String>,

    #[arg(long, env = "IGRA_STRESS_KASPA_FANOUT_SOURCE_MNEMONIC_PASSPHRASE")]
    kaspa_fanout_source_mnemonic_passphrase: Option<String>,

    #[arg(
        long,
        env = "IGRA_STRESS_KASPA_FANOUT_SOURCE_MNEMONIC_PASSPHRASE_AS_MNEMONIC",
        default_value_t = false
    )]
    kaspa_fanout_source_mnemonic_passphrase_as_mnemonic: bool,

    #[arg(
        long,
        env = "IGRA_STRESS_KASPA_FANOUT_SOURCE_MNEMONIC_PASSPHRASE_EMPTY",
        default_value_t = false
    )]
    kaspa_fanout_source_mnemonic_passphrase_empty: bool,

    #[arg(long, env = "IGRA_STRESS_KASPA_FANOUT_SOURCE_MNEMONIC_INDEX", default_value_t = 0)]
    kaspa_fanout_source_mnemonic_index: u32,

    #[arg(long, env = "IGRA_STRESS_KASPA_FANOUT_AMOUNT_SOMPI", default_value_t = 100_000_000)]
    kaspa_fanout_amount_sompi: u64,

    #[arg(long, env = "IGRA_STRESS_KASPA_FANOUT_UTXOS_PER_WALLET", default_value_t = 1)]
    kaspa_fanout_utxos_per_wallet: u32,

    #[arg(long, env = "IGRA_STRESS_KASPA_FANOUT_MAX_OUTPUTS_PER_TX", default_value_t = 64)]
    kaspa_fanout_max_outputs_per_tx: usize,

    #[arg(long, env = "IGRA_STRESS_IGRA_MIN_FEE_FLOOR_GWEI_EXPECTED")]
    igra_min_fee_floor_gwei_expected: Option<u64>,

    #[arg(long)]
    campaign_output_dir: Option<String>,

    #[arg(long, env = "IGRA_NO_PROXY", default_value_t = false)]
    no_proxy: bool,

    #[arg(long, default_value_t = false)]
    print_addresses: bool,
}

#[derive(Clone, Debug)]
enum StopCondition {
    Duration(u64),
    TotalTxs(u64),
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct ResolvedConfig {
    network: Network,
    mode: Mode,
    target_tps: f64,
    worker_count: usize,
    wallet_start_index: usize,
    stop_condition: StopCondition,
    calibration_mode: bool,
    preflight_sample_mode: bool,
    wallets_json: PathBuf,
    tx_id_prefix: String,
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    mining_timeout_secs: u64,
    warmup_txs_per_worker: u64,
    warmup_barrier_timeout_secs: u64,
    pending_timeout_secs: u64,
    max_replacements_per_nonce: u64,
    replacement_fee_bump_pct: u64,
    replacement_timeout_cap_secs: u64,
    shutdown_grace_secs: u64,
    degraded_pause_max_secs: u64,
    worker_recovery_timeout_secs: u64,
    report_secs: u64,
    rpc_selection_mode: RpcSelectionMode,
    recipient_mode: RecipientMode,
    recipient_random_seed: u64,
    rpc_random_seed: u64,
    el_rpc_urls: Vec<String>,
    kaspa_rpc_urls: Vec<String>,
    endpoint_set_sha256: String,
    parameter_sources: HashMap<String, String>,
    contract_base_address: Address,
    contract_end_address: Address,
    preflight_balance_check: bool,
    prebuild_horizon_secs: u64,
    utxo_refill_lag_secs: u64,
    utxo_safety_factor: f64,
    kaspa_fanout_enabled: bool,
    kaspa_fanout_source_private_key: Option<String>,
    kaspa_fanout_source_mnemonic: Option<String>,
    kaspa_fanout_source_mnemonic_passphrase: Option<String>,
    kaspa_fanout_source_mnemonic_passphrase_as_mnemonic: bool,
    kaspa_fanout_source_mnemonic_passphrase_empty: bool,
    kaspa_fanout_source_mnemonic_index: u32,
    kaspa_fanout_amount_sompi: u64,
    kaspa_fanout_utxos_per_wallet: u32,
    kaspa_fanout_max_outputs_per_tx: usize,
    kaspa_fee_mode: String,
    kaspa_fee_bucket: String,
    igra_min_fee_floor_gwei_expected: Option<u64>,
    campaign_output_dir: PathBuf,
    no_proxy: bool,
    explicit_to: Option<Address>,
    explicit_data: Option<Vec<u8>>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
struct WalletEntry {
    mnemonic: String,
    kaspa_private_key: String,
    kaspa_address: String,
    ethereum_private_key: String,
    ethereum_address: String,
}

#[derive(Debug, Deserialize)]
struct RpcEndpointsFile {
    igra_rpc_urls: Vec<String>,
    kaspa_rpc_urls: Vec<String>,
}

#[derive(Clone, Debug)]
struct WorkerPlan {
    worker_id: usize,
    wallet_index: usize,
    evm_key: String,
    kaspa_key: String,
    kaspa_address: String,
    evm_sender: Address,
    contract: Address,
    recipient: Address,
    call_data: Vec<u8>,
    per_worker_total_target: Option<u64>,
}

#[derive(Clone, Debug)]
struct PrebuiltTx {
    nonce: u64,
    raw: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct WorkerCounters {
    accepted: u64,
    rejected: u64,
    timeout: u64,
    dropped: u64,
    terminal_error: u64,
    replacement_count_total: u64,
    local_next_nonce: u64,
    rpc_pending_nonce: u64,
    last_error_code: Option<String>,
    last_accept_instant: Option<Instant>,
    last_el_rpc_latency_ms: Option<u64>,
    last_kaspa_rpc_latency_ms: Option<u64>,
    active_endpoint: Option<String>,
}

#[derive(Clone, Debug)]
struct WorkerState {
    worker_id: usize,
    wallet_index: usize,
    counters: WorkerCounters,
    exited: bool,
}

#[derive(Clone, Debug, Default)]
struct AggregatedTotals {
    accepted: u64,
    rejected: u64,
    timeout: u64,
    dropped: u64,
    terminal_error: u64,
}

#[derive(Clone, Debug)]
struct AggregateSample {
    ts_utc: String,
    tps_1s: f64,
    accepted_1s: u64,
    rejected_1s: u64,
    timeout_1s: u64,
    dropped_1s: u64,
    terminal_error_1s: u64,
    el_rpc_p95_ms: Option<u64>,
    kaspa_rpc_p95_ms: Option<u64>,
    el_pending_count: u64,
    kaspa_mempool_mass_estimate: Option<u64>,
    active_workers: usize,
    stalled_workers: usize,
}

#[derive(Clone, Debug, Default)]
struct TransportHealth {
    consecutive_failures: u32,
    backoff_until: Option<Instant>,
}

#[derive(Clone, Debug)]
struct TransportSlot {
    endpoint_id: String,
    transport: IgraTransport<RuntimeTransport>,
}

#[derive(Clone, Debug)]
enum WorkerEvent {
    WarmupComplete { worker_id: usize },
    WorkerExit { worker_id: usize, error: Option<String> },
}

#[derive(Clone, Copy, Debug)]
enum FailureClass {
    Rejected,
    Timeout,
    TerminalError,
}

#[derive(Debug)]
struct RunResult {
    totals: AggregatedTotals,
    achieved_tps_avg: f64,
    achieved_tps_p95_1m: f64,
    failure_ratio: f64,
    max_stalled_ratio: f64,
    pass: bool,
    fail_reasons: Vec<String>,
    interrupted_exit_code: Option<i32>,
    warmup_completed_at_utc: Option<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = resolve_config(&args)?;

    let wallets = load_wallets(&cfg.wallets_json)?;
    let plans = resolve_workers(&args, &cfg, &wallets)?;

    if args.print_addresses {
        print_addresses(&plans);
        return Ok(());
    }

    fs::create_dir_all(&cfg.campaign_output_dir)
        .wrap_err("failed to create campaign output dir")?;

    if cfg.kaspa_fanout_enabled {
        run_kaspa_fanout(&cfg, &plans).await?;
    }

    let observed_fee_floor_gwei = preflight(&cfg, &plans).await?;

    let campaign_id = format!(
        "{}-{}-{}tps-{}",
        cfg.network.as_str(),
        cfg.mode.as_str(),
        cfg.target_tps.round() as u64,
        Utc::now().format("%Y%m%dT%H%M%SZ")
    );

    let started_at_utc = utc_now();
    emit_manifest(
        &cfg,
        &plans,
        &campaign_id,
        &started_at_utc,
        None,
        observed_fee_floor_gwei,
        None,
        None,
    )?;

    let run_result = run_campaign(&cfg, &plans, observed_fee_floor_gwei).await?;

    let ended_at_utc = utc_now();
    emit_manifest(
        &cfg,
        &plans,
        &campaign_id,
        &started_at_utc,
        Some(&ended_at_utc),
        observed_fee_floor_gwei,
        Some(&run_result),
        run_result.warmup_completed_at_utc.as_deref(),
    )?;

    if cfg.calibration_mode || cfg.preflight_sample_mode {
        emit_calibration_report(&cfg, &run_result)?;
    }

    let out = json!({
        "campaign_id": campaign_id,
        "pass": run_result.pass,
        "failure_ratio": run_result.failure_ratio,
        "achieved_tps_avg": run_result.achieved_tps_avg,
        "achieved_tps_p95_1m": run_result.achieved_tps_p95_1m,
        "totals": {
            "accepted": run_result.totals.accepted,
            "rejected": run_result.totals.rejected,
            "timeout": run_result.totals.timeout,
            "dropped": run_result.totals.dropped,
            "terminal_error": run_result.totals.terminal_error,
        },
        "max_stalled_ratio": run_result.max_stalled_ratio,
        "fail_reasons": run_result.fail_reasons,
        "artifacts": {
            "campaign_output_dir": cfg.campaign_output_dir,
            "manifest": cfg.campaign_output_dir.join("campaign-manifest.json"),
            "metrics": cfg.campaign_output_dir.join("metrics.ndjson"),
        }
    });
    println!("{}", serde_json::to_string_pretty(&out)?);

    if let Some(code) = run_result.interrupted_exit_code {
        std::process::exit(code);
    }

    if !run_result.pass {
        return Err(eyre!("campaign failed"));
    }

    Ok(())
}

fn resolve_config(args: &Args) -> Result<ResolvedConfig> {
    let cli_args: Vec<String> = std::env::args().collect();
    let target_tps = args.target_tps.or(args.legacy_tps).unwrap_or(0.0);

    if !target_tps.is_finite() {
        return Err(eyre!("target TPS must be finite"));
    }
    if !args.instance_safe_tps.is_finite() || args.instance_safe_tps <= 0.0 {
        return Err(eyre!("instance-safe-tps must be > 0 and finite"));
    }

    if target_tps <= 0.0 && args.calibration_mode == 0 && args.preflight_sample_mode == 0 {
        return Err(eyre!("target TPS must be > 0 (set --target-tps or IGRA_STRESS_TARGET_TPS)"));
    }

    let calibration_mode = args.calibration_mode == 1;
    let preflight_sample_mode = args.preflight_sample_mode == 1;

    if calibration_mode && preflight_sample_mode {
        return Err(eyre!("calibration mode and preflight-sample mode are mutually exclusive"));
    }

    let mut el_urls = parse_csv_list(args.el_rpc_urls.as_deref().unwrap_or(""));
    if el_urls.is_empty() {
        if let Some(single) = args.legacy_el_rpc_url.as_deref() {
            el_urls.push(single.to_string());
        }
    }

    let mut kaspa_urls = parse_csv_list(args.kaspa_rpc_urls.as_deref().unwrap_or(""));
    if kaspa_urls.is_empty() {
        if let Some(single) = args.legacy_kaspa_rpc_url.as_deref() {
            kaspa_urls.push(single.to_string());
        }
    }

    let mut rpc_loaded_from_file = false;
    if (el_urls.is_empty() || kaspa_urls.is_empty()) && args.rpc_endpoints_json.is_some() {
        let p = PathBuf::from(args.rpc_endpoints_json.as_deref().expect("checked is_some"));
        let parsed: RpcEndpointsFile = serde_json::from_slice(
            &fs::read(&p)
                .wrap_err_with(|| format!("failed to read endpoints file {}", p.display()))?,
        )
        .wrap_err("invalid rpc endpoints json")?;
        if el_urls.is_empty() {
            el_urls = parsed.igra_rpc_urls;
            rpc_loaded_from_file = true;
        }
        if kaspa_urls.is_empty() {
            kaspa_urls = parsed.kaspa_rpc_urls;
            rpc_loaded_from_file = true;
        }
    }

    if el_urls.is_empty() || kaspa_urls.is_empty() {
        return Err(eyre!(
            "missing RPC endpoints: set --el-rpc-urls/--kaspa-rpc-urls or --rpc-endpoints-json"
        ));
    }

    for url in &mut kaspa_urls {
        *url = normalize_kaspa_rpc_url(url);
    }

    let worker_count = args.worker_count.unwrap_or_else(|| {
        if target_tps > 0.0 {
            (target_tps / args.instance_safe_tps).ceil().max(1.0) as usize
        } else {
            1
        }
    });

    if worker_count == 0 {
        return Err(eyre!("worker count resolved to 0"));
    }
    if args.kaspa_fanout > 1 {
        return Err(eyre!("--kaspa-fanout must be 0 or 1"));
    }
    if args.kaspa_fanout == 1 {
        if args.kaspa_fanout_amount_sompi == 0 {
            return Err(eyre!("--kaspa-fanout-amount-sompi must be > 0"));
        }
        if args.kaspa_fanout_utxos_per_wallet == 0 {
            return Err(eyre!("--kaspa-fanout-utxos-per-wallet must be > 0"));
        }
        if args.kaspa_fanout_max_outputs_per_tx == 0 {
            return Err(eyre!("--kaspa-fanout-max-outputs-per-tx must be > 0"));
        }
        let min_amount_sompi =
            min_fanout_amount_sompi_for_standardness(args.kaspa_fanout_max_outputs_per_tx);
        if args.kaspa_fanout_amount_sompi < min_amount_sompi {
            return Err(eyre!(
                "--kaspa-fanout-amount-sompi={} is too low for standardness with --kaspa-fanout-max-outputs-per-tx={}; require at least {} sompi (~{:.4} KAS) to keep storage mass under {} (safety target {})",
                args.kaspa_fanout_amount_sompi,
                args.kaspa_fanout_max_outputs_per_tx,
                min_amount_sompi,
                sompi_to_kaspa(min_amount_sompi),
                MAX_STANDARD_KASPA_TX_MASS,
                FANOUT_TARGET_STORAGE_MASS
            ));
        }
    }

    let stop_condition = if calibration_mode {
        StopCondition::TotalTxs(args.total_txs.unwrap_or(2000))
    } else if preflight_sample_mode {
        StopCondition::TotalTxs(args.total_txs.unwrap_or(1000))
    } else {
        match (args.duration_secs, args.total_txs) {
            (Some(d), None) if d > 0 => StopCondition::Duration(d),
            (None, Some(t)) if t > 0 => StopCondition::TotalTxs(t),
            (None, None) => {
                if let Some(legacy_per_worker) = args.legacy_total_txs_per_worker {
                    StopCondition::TotalTxs(legacy_per_worker.saturating_mul(worker_count as u64))
                } else {
                    return Err(eyre!("set exactly one of --duration-secs or --total-txs"));
                }
            }
            _ => {
                return Err(eyre!("set exactly one of --duration-secs or --total-txs"));
            }
        }
    };

    let output_dir = if let Some(v) = args.campaign_output_dir.as_deref() {
        PathBuf::from(v)
    } else {
        PathBuf::from(format!("/tmp/igra-loadgen-{}", Utc::now().format("%Y%m%dT%H%M%SZ")))
    };

    let tx_id_prefix = normalize_hex_prefix(
        args.tx_id_prefix
            .clone()
            .unwrap_or_else(|| args.network.default_tx_id_prefix().to_string()),
    );

    if tx_id_prefix.is_empty() {
        return Err(eyre!("tx-id-prefix cannot be empty"));
    }

    let contract_base_address =
        parse_address(&args.contract_base_address).wrap_err("invalid --contract-base-address")?;
    let contract_end_address =
        parse_address(&args.contract_end_address).wrap_err("invalid --contract-end-address")?;

    if contract_end_address.as_slice() < contract_base_address.as_slice() {
        return Err(eyre!("contract end address must be >= base address"));
    }

    let explicit_to = args.to.as_deref().map(parse_address).transpose().wrap_err("invalid --to")?;
    let explicit_data =
        args.data.as_deref().map(parse_hex_bytes).transpose().wrap_err("invalid --data")?;

    let endpoint_set_sha256 = endpoint_set_sha256(&el_urls, &kaspa_urls);

    let mut parameter_sources = HashMap::new();
    parameter_sources.insert(
        "IGRA_STRESS_NETWORK".to_string(),
        resolve_source(&cli_args, &["--network"], "IGRA_STRESS_NETWORK", false, "default"),
    );
    parameter_sources.insert(
        "IGRA_STRESS_MODE".to_string(),
        resolve_source(&cli_args, &["--mode"], "IGRA_STRESS_MODE", false, "default"),
    );
    parameter_sources.insert(
        "IGRA_STRESS_TARGET_TPS".to_string(),
        resolve_source(
            &cli_args,
            &["--target-tps", "--legacy-tps"],
            "IGRA_STRESS_TARGET_TPS",
            false,
            if target_tps > 0.0 { "resolved" } else { "default" },
        ),
    );
    parameter_sources.insert(
        "IGRA_STRESS_EL_RPC_URLS".to_string(),
        resolve_source(
            &cli_args,
            &["--el-rpc-urls", "--legacy-el-rpc-url"],
            "IGRA_STRESS_EL_RPC_URLS",
            rpc_loaded_from_file,
            "default",
        ),
    );
    parameter_sources.insert(
        "IGRA_STRESS_KASPA_RPC_URLS".to_string(),
        resolve_source(
            &cli_args,
            &["--kaspa-rpc-urls", "--legacy-kaspa-rpc-url"],
            "IGRA_STRESS_KASPA_RPC_URLS",
            rpc_loaded_from_file,
            "default",
        ),
    );
    parameter_sources.insert(
        "IGRA_STRESS_WORKER_COUNT".to_string(),
        resolve_source(
            &cli_args,
            &["--worker-count"],
            "IGRA_STRESS_WORKER_COUNT",
            false,
            if args.worker_count.is_some() { "cli" } else { "derived" },
        ),
    );

    Ok(ResolvedConfig {
        network: args.network,
        mode: args.mode,
        target_tps,
        worker_count,
        wallet_start_index: args.wallet_start_index,
        stop_condition,
        calibration_mode,
        preflight_sample_mode,
        wallets_json: PathBuf::from(args.wallets_json.clone()),
        tx_id_prefix,
        gas_limit: args.gas_limit,
        max_fee_per_gas: args.max_fee_per_gas,
        max_priority_fee_per_gas: args.max_priority_fee_per_gas,
        mining_timeout_secs: args.mining_timeout_secs,
        warmup_txs_per_worker: args.warmup_txs_per_worker,
        warmup_barrier_timeout_secs: args.warmup_barrier_timeout_secs,
        pending_timeout_secs: args.pending_timeout_secs,
        max_replacements_per_nonce: args.max_replacements_per_nonce,
        replacement_fee_bump_pct: args.replacement_fee_bump_pct,
        replacement_timeout_cap_secs: args.replacement_timeout_cap_secs,
        shutdown_grace_secs: args.shutdown_grace_secs,
        degraded_pause_max_secs: args.degraded_pause_max_secs,
        worker_recovery_timeout_secs: args.worker_recovery_timeout_secs,
        report_secs: args.report_secs.max(1),
        rpc_selection_mode: args.rpc_selection_mode,
        recipient_mode: args.recipient_mode,
        recipient_random_seed: args.recipient_random_seed.unwrap_or(1),
        rpc_random_seed: args.rpc_random_seed.unwrap_or(1),
        el_rpc_urls: el_urls,
        kaspa_rpc_urls: kaspa_urls,
        endpoint_set_sha256,
        parameter_sources,
        contract_base_address,
        contract_end_address,
        preflight_balance_check: args.preflight_balance_check == 1,
        prebuild_horizon_secs: args.prebuild_horizon_secs,
        utxo_refill_lag_secs: args.utxo_refill_lag_secs,
        utxo_safety_factor: args.utxo_safety_factor,
        kaspa_fanout_enabled: args.kaspa_fanout == 1,
        kaspa_fanout_source_private_key: args.kaspa_fanout_source_private_key.clone(),
        kaspa_fanout_source_mnemonic: args.kaspa_fanout_source_mnemonic.clone(),
        kaspa_fanout_source_mnemonic_passphrase: args
            .kaspa_fanout_source_mnemonic_passphrase
            .clone(),
        kaspa_fanout_source_mnemonic_passphrase_as_mnemonic: args
            .kaspa_fanout_source_mnemonic_passphrase_as_mnemonic,
        kaspa_fanout_source_mnemonic_passphrase_empty: args
            .kaspa_fanout_source_mnemonic_passphrase_empty,
        kaspa_fanout_source_mnemonic_index: args.kaspa_fanout_source_mnemonic_index,
        kaspa_fanout_amount_sompi: args.kaspa_fanout_amount_sompi,
        kaspa_fanout_utxos_per_wallet: args.kaspa_fanout_utxos_per_wallet,
        kaspa_fanout_max_outputs_per_tx: args.kaspa_fanout_max_outputs_per_tx,
        kaspa_fee_mode: args.kaspa_fee_mode.trim().to_lowercase(),
        kaspa_fee_bucket: args.kaspa_fee_bucket.trim().to_lowercase(),
        igra_min_fee_floor_gwei_expected: args.igra_min_fee_floor_gwei_expected,
        campaign_output_dir: output_dir,
        no_proxy: args.no_proxy,
        explicit_to,
        explicit_data,
    })
}

fn resolve_workers(
    args: &Args,
    cfg: &ResolvedConfig,
    wallets: &[WalletEntry],
) -> Result<Vec<WorkerPlan>> {
    let evm_keys_cli = parse_list_or_file(args.evm_keys.as_deref().unwrap_or(""))?;
    let mut kaspa_keys_cli = parse_list_or_file(args.kaspa_private_keys.as_deref().unwrap_or(""))?;

    if kaspa_keys_cli.is_empty() {
        if let Some(mnemonic) = args.kaspa_mnemonic.as_deref() {
            let count =
                args.kaspa_mnemonic_count.unwrap_or(cfg.worker_count.try_into().unwrap_or(0));
            if count == 0 {
                return Err(eyre!("kaspa mnemonic derivation requires non-zero count"));
            }
            kaspa_keys_cli = derive_kaspa_private_keys(
                mnemonic,
                args.kaspa_mnemonic_passphrase.as_deref(),
                args.kaspa_mnemonic_passphrase_as_mnemonic,
                args.kaspa_mnemonic_passphrase_empty,
                None,
                args.kaspa_mnemonic_index_start,
                count,
            )?;
        }
    }

    let use_wallets_json = evm_keys_cli.is_empty();

    if use_wallets_json {
        if cfg.wallet_start_index + cfg.worker_count > wallets.len() {
            return Err(eyre!(
                "wallet range overflow: start={} workers={} wallets={}",
                cfg.wallet_start_index,
                cfg.worker_count,
                wallets.len()
            ));
        }
    } else if evm_keys_cli.len() < cfg.worker_count {
        return Err(eyre!(
            "need >= {} EVM keys in --evm-keys, got {}",
            cfg.worker_count,
            evm_keys_cli.len()
        ));
    }

    if !kaspa_keys_cli.is_empty()
        && !args.allow_shared_kaspa_key
        && kaspa_keys_cli.len() < cfg.worker_count
    {
        return Err(eyre!(
            "need >= {} Kaspa keys in --kaspa-private-keys (or set --allow-shared-kaspa-key)",
            cfg.worker_count
        ));
    }

    let per_worker_total_target = match cfg.stop_condition {
        StopCondition::TotalTxs(total) => {
            let base = total / cfg.worker_count as u64;
            let rem = total % cfg.worker_count as u64;
            Some((base, rem))
        }
        StopCondition::Duration(_) => None,
    };

    let mut rng = StdRng::seed_from_u64(cfg.recipient_random_seed);
    let mut plans = Vec::with_capacity(cfg.worker_count);

    for k in 0..cfg.worker_count {
        let wallet_index = if use_wallets_json { cfg.wallet_start_index + k } else { k };

        let (evm_key, kaspa_key, kaspa_address, recipient_source_list) = if use_wallets_json {
            let w = &wallets[wallet_index];
            let kaspa_address =
                kaspa_address_from_private_key_hex(&w.kaspa_private_key, cfg.network.as_str())
                    .wrap_err_with(|| {
                        format!("invalid Kaspa private key for wallet index {}", wallet_index)
                    })?
                    .to_string();
            (w.ethereum_private_key.clone(), w.kaspa_private_key.clone(), kaspa_address, wallets)
        } else {
            let evm_key = evm_keys_cli[k].clone();
            let kaspa_key = if kaspa_keys_cli.is_empty() {
                return Err(eyre!("missing Kaspa key source for worker {}", k + 1));
            } else if kaspa_keys_cli.len() > k {
                kaspa_keys_cli[k].clone()
            } else {
                kaspa_keys_cli[0].clone()
            };
            let kaspa_address =
                kaspa_address_from_private_key_hex(&kaspa_key, cfg.network.as_str())
                    .wrap_err_with(|| format!("invalid Kaspa private key for worker {}", k + 1))?
                    .to_string();
            (evm_key, kaspa_key, kaspa_address, wallets)
        };

        if !kaspa_address.starts_with(cfg.network.kaspa_address_prefix()) {
            return Err(eyre!(
                "Kaspa address prefix mismatch for worker {}: expected {}, got {}",
                k + 1,
                cfg.network.kaspa_address_prefix(),
                kaspa_address
            ));
        }

        let signer = evm_key
            .parse::<PrivateKeySigner>()
            .map_err(|err| eyre!("invalid EVM private key at worker {}: {}", k + 1, err))?;
        let evm_sender = signer.address();

        let contract = add_address_offset(cfg.contract_base_address, wallet_index)
            .ok_or_else(|| eyre!("contract index overflow"))?;

        if contract.as_slice() > cfg.contract_end_address.as_slice() {
            return Err(eyre!(
                "contract address index out of range for wallet index {}",
                wallet_index
            ));
        }

        let recipient = match cfg.recipient_mode {
            RecipientMode::Ring => {
                let idx = if use_wallets_json {
                    (wallet_index + 1) % recipient_source_list.len()
                } else {
                    (k + 1) % cfg.worker_count
                };
                if use_wallets_json {
                    parse_address(&wallets[idx].ethereum_address)?
                } else {
                    let r_signer = evm_keys_cli[idx]
                        .parse::<PrivateKeySigner>()
                        .map_err(|err| eyre!("invalid ring recipient key at {}: {}", idx, err))?;
                    r_signer.address()
                }
            }
            RecipientMode::RandomSeeded => {
                if use_wallets_json {
                    let idx = rng.random_range(0..wallets.len());
                    parse_address(&wallets[idx].ethereum_address)?
                } else {
                    let idx = rng.random_range(0..cfg.worker_count);
                    let r_signer = evm_keys_cli[idx]
                        .parse::<PrivateKeySigner>()
                        .map_err(|err| eyre!("invalid random recipient key at {}: {}", idx, err))?;
                    r_signer.address()
                }
            }
        };

        let call_data = if let Some(explicit) = cfg.explicit_data.as_ref() {
            explicit.clone()
        } else {
            build_transfer_calldata(recipient, 1)
        };

        let per_worker_target = per_worker_total_target
            .map(|(base, rem)| if (k as u64) < rem { base + 1 } else { base });

        plans.push(WorkerPlan {
            worker_id: k + 1,
            wallet_index,
            evm_key,
            kaspa_key,
            kaspa_address,
            evm_sender,
            contract,
            recipient,
            call_data,
            per_worker_total_target: per_worker_target,
        });
    }

    Ok(plans)
}

fn print_addresses(plans: &[WorkerPlan]) {
    let mut rows = Vec::new();
    for p in plans {
        rows.push(json!({
            "worker": p.worker_id,
            "wallet_index": p.wallet_index,
            "evm_sender": format!("{:#x}", p.evm_sender),
            "kaspa_address": p.kaspa_address,
            "contract": format!("{:#x}", p.contract),
            "recipient": format!("{:#x}", p.recipient),
        }));
    }
    println!("{}", serde_json::to_string_pretty(&rows).expect("serialize rows"));
}

async fn preflight(cfg: &ResolvedConfig, plans: &[WorkerPlan]) -> Result<u64> {
    let mut chain_ok = false;
    for el in &cfg.el_rpc_urls {
        let mut transport = build_transport(el, &cfg.kaspa_rpc_urls[0], &plans[0].kaspa_key, cfg)?;
        if eth_chain_id(&mut transport).await.is_ok() {
            chain_ok = true;
            break;
        }
    }
    if !chain_ok {
        return Err(eyre!("preflight failed: unable to query eth_chainId from any EL endpoint"));
    }

    let observed_floor = observe_fee_floor(cfg).await?;
    if let Some(expected) = cfg.igra_min_fee_floor_gwei_expected
        && observed_floor < expected
    {
        return Err(eyre!(
            "preflight failed: observed fee floor {} gwei is below expected {} gwei",
            observed_floor,
            expected
        ));
    }

    verify_contract_code(cfg, plans).await?;

    if cfg.preflight_balance_check {
        verify_evm_balances(cfg, plans).await?;
    }

    verify_kaspa_utxos(cfg, plans).await?;

    Ok(observed_floor)
}

async fn observe_fee_floor(cfg: &ResolvedConfig) -> Result<u64> {
    let mut observed = 0u64;
    for el in &cfg.el_rpc_urls {
        let mut transport = build_runtime_transport(el, cfg.no_proxy)?;
        if let Ok(wei) = eth_gas_price(&mut transport).await {
            let gwei = wei / 1_000_000_000;
            observed = observed.max(gwei);
        }
    }
    if observed == 0 {
        return Err(eyre!("preflight failed: unable to observe gas price floor from EL endpoints"));
    }
    Ok(observed)
}

async fn verify_contract_code(cfg: &ResolvedConfig, plans: &[WorkerPlan]) -> Result<()> {
    if plans.is_empty() {
        return Ok(());
    }

    let sample_count = plans.len().min(10);
    let step = (plans.len() / sample_count).max(1);
    let mut samples = Vec::with_capacity(sample_count);
    let mut idx = 0usize;
    while samples.len() < sample_count && idx < plans.len() {
        samples.push(plans[idx].contract);
        idx = idx.saturating_add(step);
    }

    let mut transport = build_runtime_transport(&cfg.el_rpc_urls[0], cfg.no_proxy)?;
    for addr in &samples {
        let code = eth_get_code(&mut transport, *addr).await?;
        if code == "0x" || code == "0x0" {
            return Err(eyre!("preflight failed: empty contract code at {addr:#x}"));
        }
    }

    Ok(())
}

async fn verify_evm_balances(cfg: &ResolvedConfig, plans: &[WorkerPlan]) -> Result<()> {
    let mut transport = build_runtime_transport(&cfg.el_rpc_urls[0], cfg.no_proxy)?;

    let est_txs_per_worker = match cfg.stop_condition {
        StopCondition::Duration(secs) => {
            let per =
                if cfg.target_tps > 0.0 { cfg.target_tps / cfg.worker_count as f64 } else { 0.0 };
            (per * secs as f64).ceil() as u128
        }
        StopCondition::TotalTxs(total) => ((total as f64) / cfg.worker_count as f64).ceil() as u128,
    };

    let required_wei_per_worker = est_txs_per_worker
        .saturating_add(cfg.warmup_txs_per_worker as u128)
        .saturating_mul(cfg.gas_limit as u128)
        .saturating_mul(cfg.max_fee_per_gas)
        .saturating_mul(11)
        / 10;

    let mut deficits = Vec::new();
    for p in plans {
        let bal = eth_get_balance(&mut transport, p.evm_sender).await?;
        if bal < required_wei_per_worker {
            deficits.push((p.wallet_index, bal, required_wei_per_worker));
        }
    }

    if !deficits.is_empty() {
        return Err(eyre!(
            "preflight failed: {} wallet(s) have insufficient EVM balance (first: index={} have={} need={})",
            deficits.len(),
            deficits[0].0,
            deficits[0].1,
            deficits[0].2
        ));
    }

    Ok(())
}

async fn verify_kaspa_utxos(cfg: &ResolvedConfig, plans: &[WorkerPlan]) -> Result<()> {
    if plans.is_empty() {
        return Ok(());
    }

    let client = GrpcClient::connect(cfg.kaspa_rpc_urls[0].clone())
        .await
        .wrap_err("failed to connect kaspa grpc for preflight")?;

    let mut addrs = Vec::with_capacity(plans.len());
    for p in plans {
        let addr = KaspaAddress::try_from(p.kaspa_address.as_str()).wrap_err_with(|| {
            format!("invalid Kaspa address for wallet index {}", p.wallet_index)
        })?;
        addrs.push(addr);
    }

    let entries = client
        .get_utxos_by_addresses(addrs.clone())
        .await
        .wrap_err("get_utxos_by_addresses failed")?;

    let mut by_addr: HashMap<String, u64> = HashMap::new();
    for e in entries {
        if let Some(addr) = e.address {
            *by_addr.entry(addr.to_string()).or_insert(0) += 1;
        }
    }

    let per_worker_tps =
        if cfg.target_tps > 0.0 { cfg.target_tps / cfg.worker_count as f64 } else { 0.0 };
    let utxos_per_wallet_min = if matches!(cfg.mode, Mode::PrebuildSend) {
        ((per_worker_tps
            * (cfg.prebuild_horizon_secs + cfg.utxo_refill_lag_secs) as f64
            * cfg.utxo_safety_factor)
            .ceil() as u64)
            .max(1)
    } else {
        1
    };

    let mut deficits = Vec::new();
    for (i, addr) in addrs.iter().enumerate() {
        let key = addr.to_string();
        let count = by_addr.get(&key).copied().unwrap_or(0);
        if count < utxos_per_wallet_min {
            deficits.push((plans[i].wallet_index, key, count, utxos_per_wallet_min));
        }
    }

    if !deficits.is_empty() {
        return Err(eyre!(
            "preflight failed: insufficient Kaspa UTXO depth for {} wallet(s) (first: wallet_index={} address={} have={} need={})",
            deficits.len(),
            deficits[0].0,
            deficits[0].1,
            deficits[0].2,
            deficits[0].3
        ));
    }

    Ok(())
}

async fn run_kaspa_fanout(cfg: &ResolvedConfig, plans: &[WorkerPlan]) -> Result<()> {
    if plans.is_empty() {
        return Ok(());
    }
    let source_private_key =
        resolve_kaspa_fanout_source_key(cfg, plans).wrap_err("resolve fan-out source key")?;
    let (network_type, address_prefix) =
        kaspa_network_descriptor(cfg.network.as_str()).wrap_err("fan-out network mapping")?;
    let source_address = kaspa_address_from_private_key(&source_private_key, address_prefix)
        .wrap_err("derive fan-out source address")?;

    let mut targets = Vec::<(KaspaAddress, u64)>::new();
    for plan in plans {
        let addr = KaspaAddress::try_from(plan.kaspa_address.as_str())
            .wrap_err_with(|| format!("invalid worker Kaspa address {}", plan.kaspa_address))?;
        for _ in 0..cfg.kaspa_fanout_utxos_per_wallet {
            targets.push((addr.clone(), cfg.kaspa_fanout_amount_sompi));
        }
    }
    if targets.is_empty() {
        return Ok(());
    }

    let client = GrpcClient::connect(cfg.kaspa_rpc_urls[0].clone())
        .await
        .wrap_err("fan-out: connect kaspa gRPC failed")?;
    let feerate = get_kaspa_feerate_sompi_per_gram(&client, cfg).await?;
    let mut tip = fetch_largest_utxo(&client, &source_address)
        .await
        .wrap_err("fan-out: failed to load source UTXO")?;

    let mut submitted = 0usize;
    let batch_size = cfg.kaspa_fanout_max_outputs_per_tx.max(1);
    let mut queue: VecDeque<Vec<(KaspaAddress, u64)>> =
        targets.chunks(batch_size).map(|chunk| chunk.to_vec()).collect();

    while let Some(batch) = queue.pop_front() {
        let built = build_signed_chained_fanout_tx(
            &source_private_key,
            &source_address,
            network_type,
            &tip,
            &batch,
            feerate,
        );
        let (tx, next_tip, _fee, mass) = match built {
            Ok(v) => v,
            Err(err) => {
                if batch.len() > 1 && fanout_batch_too_large(err.to_string().as_str()) {
                    split_fanout_batch(&batch, &mut queue);
                    continue;
                }
                let hint = fanout_policy_hint(err.to_string().as_str());
                return Err(err.wrap_err(format!(
                    "fan-out build failed after {} outputs submitted{}",
                    submitted, hint
                )));
            }
        };
        if mass > MAX_STANDARD_KASPA_TX_MASS {
            if batch.len() > 1 {
                split_fanout_batch(&batch, &mut queue);
                continue;
            }
            let required = min_fanout_amount_sompi_for_standardness(1);
            return Err(eyre!(
                "fan-out pre-submit standardness validation failed: computed storage mass {} exceeds limit {} for single-output batch (amount_sompi={}); increase --kaspa-fanout-amount-sompi to at least {} (~{:.4} KAS), or use devnet prealloc",
                mass,
                MAX_STANDARD_KASPA_TX_MASS,
                batch[0].1,
                required,
                sompi_to_kaspa(required)
            ));
        }

        let rpc_tx = RpcTransaction::from(&tx);
        match client.submit_transaction(rpc_tx, false).await {
            Ok(_) => {
                tip = next_tip;
                submitted += batch.len();
            }
            Err(err) => {
                let msg = err.to_string();
                if batch.len() > 1 && fanout_batch_too_large(msg.as_str()) {
                    split_fanout_batch(&batch, &mut queue);
                    continue;
                }
                let hint = fanout_policy_hint(msg.as_str());
                return Err(eyre!(
                    "fan-out submit failed after {} outputs: {}{}",
                    submitted,
                    msg,
                    hint
                ));
            }
        }
    }

    println!(
        "{{\"fanout\":\"ok\",\"source\":\"{}\",\"outputs_submitted\":{},\"utxos_per_wallet\":{},\"amount_sompi\":{},\"rpc\":\"{}\"}}",
        source_address,
        submitted,
        cfg.kaspa_fanout_utxos_per_wallet,
        cfg.kaspa_fanout_amount_sompi,
        cfg.kaspa_rpc_urls[0]
    );
    Ok(())
}

fn fanout_policy_hint(err_text: &str) -> &'static str {
    if fanout_batch_too_large(err_text) {
        " (hint: this endpoint enforces strict Kaspa standardness for UTXO-growing fan-out; use devnet prealloc or a policy-relaxed node)"
    } else {
        ""
    }
}

fn fanout_batch_too_large(err_text: &str) -> bool {
    let lower = err_text.to_ascii_lowercase();
    lower.contains("storage mass")
        || lower.contains("mass")
            && (lower.contains("max allowed size")
                || lower.contains("exceeds standard limit")
                || lower.contains("too large"))
}

fn split_fanout_batch(
    batch: &[(KaspaAddress, u64)],
    queue: &mut VecDeque<Vec<(KaspaAddress, u64)>>,
) {
    let mid = batch.len() / 2;
    let left = batch[..mid].to_vec();
    let right = batch[mid..].to_vec();
    if !right.is_empty() {
        queue.push_front(right);
    }
    if !left.is_empty() {
        queue.push_front(left);
    }
}

fn min_fanout_amount_sompi_for_standardness(outputs_per_tx: usize) -> u64 {
    let outputs = outputs_per_tx.max(1) as u128;
    let numerator = outputs.saturating_mul(KASPA_STORAGE_MASS_PARAMETER as u128);
    let denominator = FANOUT_TARGET_STORAGE_MASS as u128;
    let required = numerator.div_ceil(denominator);
    required.min(u64::MAX as u128) as u64
}

fn sompi_to_kaspa(sompi: u64) -> f64 {
    sompi as f64 / 100_000_000.0
}

fn resolve_kaspa_fanout_source_key(cfg: &ResolvedConfig, plans: &[WorkerPlan]) -> Result<[u8; 32]> {
    if let Some(pk) = cfg.kaspa_fanout_source_private_key.as_deref() {
        return parse_private_key_hex(pk);
    }
    if let Some(mnemonic) = cfg.kaspa_fanout_source_mnemonic.as_deref() {
        let keys = derive_kaspa_private_keys(
            mnemonic,
            cfg.kaspa_fanout_source_mnemonic_passphrase.as_deref(),
            cfg.kaspa_fanout_source_mnemonic_passphrase_as_mnemonic,
            cfg.kaspa_fanout_source_mnemonic_passphrase_empty,
            None,
            cfg.kaspa_fanout_source_mnemonic_index,
            1,
        )?;
        return parse_private_key_hex(keys[0].as_str());
    }
    parse_private_key_hex(&plans[0].kaspa_key)
}

async fn get_kaspa_feerate_sompi_per_gram(
    client: &GrpcClient,
    cfg: &ResolvedConfig,
) -> Result<f64> {
    if cfg.kaspa_fee_mode == "fixed" {
        return Ok(1.0);
    }
    let estimate = client.get_fee_estimate().await.wrap_err("fan-out fee estimate failed")?;
    let feerate = match cfg.kaspa_fee_bucket.as_str() {
        "priority" => estimate.priority_bucket.feerate,
        "low" => estimate
            .low_buckets
            .first()
            .map(|b| b.feerate)
            .unwrap_or(estimate.priority_bucket.feerate),
        _ => estimate
            .normal_buckets
            .first()
            .map(|b| b.feerate)
            .unwrap_or(estimate.priority_bucket.feerate),
    };
    Ok(feerate.max(1.0))
}

async fn fetch_largest_utxo(
    client: &GrpcClient,
    source_address: &KaspaAddress,
) -> Result<RpcUtxosByAddressesEntry> {
    let mut utxos = client
        .get_utxos_by_addresses(vec![source_address.clone()])
        .await
        .wrap_err("get_utxos_by_addresses failed")?;
    utxos.sort_by_key(|u| std::cmp::Reverse(u.utxo_entry.amount));
    utxos.into_iter().next().ok_or_else(|| eyre!("source address has no spendable UTXOs"))
}

fn build_signed_chained_fanout_tx(
    private_key: &[u8; 32],
    source_address: &KaspaAddress,
    network_type: KaspaNetworkType,
    tip: &RpcUtxosByAddressesEntry,
    outputs: &[(KaspaAddress, u64)],
    feerate: f64,
) -> Result<(KaspaTransaction, RpcUtxosByAddressesEntry, u64, u64)> {
    let total_out: u64 = outputs.iter().map(|(_, amount)| *amount).sum();
    let input_amount = tip.utxo_entry.amount;
    let input = KaspaTransactionInput::new(tip.outpoint.clone().into(), Vec::new(), 0, 1);
    let entry = KaspaUtxoEntry {
        amount: tip.utxo_entry.amount,
        script_public_key: tip.utxo_entry.script_public_key.clone(),
        block_daa_score: tip.utxo_entry.block_daa_score,
        is_coinbase: tip.utxo_entry.is_coinbase,
    };
    let entries = vec![entry];
    let mass_calculator =
        KaspaMassCalculator::new_with_consensus_params(&KaspaParams::from(network_type));
    let change_script = pay_to_address_script(source_address);

    let mut fee = 1u64;
    for _ in 0..6 {
        let required = total_out.saturating_add(fee).saturating_add(MIN_CHANGE_SOMPI);
        if input_amount < required {
            return Err(eyre!(
                "fan-out insufficient input: have={} need={} (outputs={}, fee={})",
                input_amount,
                required,
                total_out,
                fee
            ));
        }
        let change = input_amount.saturating_sub(total_out).saturating_sub(fee);
        if change < MIN_CHANGE_SOMPI {
            return Err(eyre!("fan-out change below MIN_CHANGE_SOMPI"));
        }

        let mut tx_outputs = Vec::with_capacity(outputs.len() + 1);
        tx_outputs.push(KaspaTransactionOutput::new(change, change_script.clone()));
        for (addr, amount) in outputs {
            tx_outputs.push(KaspaTransactionOutput::new(*amount, pay_to_address_script(addr)));
        }

        let mut tx = KaspaTransaction::new(
            0,
            vec![input.clone()],
            tx_outputs,
            0,
            SubnetworkId::default(),
            0,
            vec![],
        );
        tx.finalize();

        let signable = KaspaSignableTransaction::with_entries(tx, entries.clone());
        let signed = kaspa_sign_with_multiple_v2(signable, std::slice::from_ref(private_key))
            .fully_signed()
            .map_err(|err| eyre!("fan-out sign failed: {err}"))?;
        kaspa_verify(&signed.as_verifiable())
            .map_err(|err| eyre!("fan-out signature verify failed: {err}"))?;

        let non_contextual = mass_calculator.calc_non_contextual_masses(&signed.tx);
        let contextual = mass_calculator
            .calc_contextual_masses(&signed.as_verifiable())
            .ok_or_else(|| eyre!("fan-out mass calculation failed"))?;
        let mass = contextual.max(non_contextual);
        let needed_fee = fee_from_feerate(mass, feerate);

        if needed_fee == fee {
            let tx = signed.tx;
            let tx_id = tx.id();
            // Some deployments report unexpectedly large local mass values for otherwise
            // acceptable standard transactions; rely on node-side standardness checks.
            tx.set_mass(0);
            let next_tip = RpcUtxosByAddressesEntry {
                address: None,
                outpoint: kaspa_rpc_core::RpcTransactionOutpoint {
                    transaction_id: tx_id,
                    index: 0,
                },
                utxo_entry: kaspa_rpc_core::RpcUtxoEntry::new(change, change_script, 0, false),
            };
            return Ok((tx, next_tip, fee, mass));
        }
        fee = needed_fee;
    }

    Err(eyre!("fan-out fee did not converge"))
}

fn fee_from_feerate(mass: u64, feerate_sompi_per_gram: f64) -> u64 {
    let fee = (feerate_sompi_per_gram * (mass as f64)).ceil();
    if fee <= 1.0 {
        1
    } else if fee >= (u64::MAX as f64) {
        u64::MAX
    } else {
        fee as u64
    }
}

async fn run_campaign(
    cfg: &ResolvedConfig,
    plans: &[WorkerPlan],
    _observed_fee_floor_gwei: u64,
) -> Result<RunResult> {
    let worker_states: Arc<Mutex<Vec<WorkerState>>> = Arc::new(Mutex::new(
        plans
            .iter()
            .map(|p| WorkerState {
                worker_id: p.worker_id,
                wallet_index: p.wallet_index,
                counters: WorkerCounters::default(),
                exited: false,
            })
            .collect(),
    ));

    let metrics_path = cfg.campaign_output_dir.join("metrics.ndjson");
    let aggregate_samples: Arc<Mutex<Vec<AggregateSample>>> = Arc::new(Mutex::new(Vec::new()));

    let stop_flag = Arc::new(AtomicBool::new(false));
    let interrupted_exit_code = Arc::new(AtomicI32::new(0));

    install_signal_handlers(stop_flag.clone(), interrupted_exit_code.clone());

    let (event_tx, mut event_rx) = mpsc::channel::<WorkerEvent>(cfg.worker_count * 4);
    let (start_tx, start_rx) = watch::channel::<bool>(false);

    let mut join_set = JoinSet::new();
    for plan in plans {
        let cfg_cloned = cfg.clone();
        let plan_cloned = plan.clone();
        let event_tx_cloned = event_tx.clone();
        let worker_states_cloned = worker_states.clone();
        let stop_flag_cloned = stop_flag.clone();
        let mut start_rx_cloned = start_rx.clone();
        join_set.spawn(async move {
            let res = run_worker(
                &cfg_cloned,
                &plan_cloned,
                &worker_states_cloned,
                &event_tx_cloned,
                &mut start_rx_cloned,
                &stop_flag_cloned,
            )
            .await;

            let _ = event_tx_cloned
                .send(WorkerEvent::WorkerExit {
                    worker_id: plan_cloned.worker_id,
                    error: res.as_ref().err().map(|e| e.to_string()),
                })
                .await;

            res
        });
    }
    drop(event_tx);

    let warmup_deadline = Duration::from_secs(cfg.warmup_barrier_timeout_secs);
    let mut warmup_seen: HashMap<usize, bool> = HashMap::new();
    let warmup_wait = async {
        while warmup_seen.len() < cfg.worker_count {
            let ev = event_rx.recv().await.ok_or_else(|| {
                eyre!("worker event channel closed before warm-up barrier completed")
            })?;
            match ev {
                WorkerEvent::WarmupComplete { worker_id } => {
                    warmup_seen.insert(worker_id, true);
                }
                WorkerEvent::WorkerExit { worker_id, error } => {
                    return Err(eyre!(
                        "worker {} exited before warm-up barrier: {}",
                        worker_id,
                        error.unwrap_or_else(|| "unknown".to_string())
                    ));
                }
            }
        }
        Ok(())
    };

    timeout(warmup_deadline, warmup_wait).await.wrap_err("warm-up barrier timeout")??;

    let warmup_completed_at_utc = utc_now();
    let _ = start_tx.send(true);

    let samples_for_metrics = aggregate_samples.clone();
    let worker_states_for_metrics = worker_states.clone();
    let stop_for_metrics = stop_flag.clone();
    let metrics_cfg = cfg.clone();
    let metrics_task = tokio::spawn(async move {
        metrics_loop(
            &metrics_cfg,
            &metrics_path,
            &worker_states_for_metrics,
            &samples_for_metrics,
            &stop_for_metrics,
        )
        .await
    });

    let run_started = Instant::now();
    let mut worker_error: Option<String> = None;

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        while let Ok(ev) = event_rx.try_recv() {
            if let WorkerEvent::WorkerExit { worker_id, error } = ev
                && let Some(err) = error
            {
                worker_error = Some(format!("worker {} error: {}", worker_id, err));
                stop_flag.store(true, Ordering::Relaxed);
                break;
            }
        }

        if let Some(err) = worker_error.as_ref() {
            eprintln!("[igra-loadgen] fail-fast: {err}");
            break;
        }

        match cfg.stop_condition {
            StopCondition::Duration(secs) => {
                if run_started.elapsed() >= Duration::from_secs(secs) {
                    stop_flag.store(true, Ordering::Relaxed);
                    break;
                }
            }
            StopCondition::TotalTxs(total) => {
                let totals = collect_totals(&worker_states).await;
                let sent = totals.accepted
                    + totals.rejected
                    + totals.timeout
                    + totals.dropped
                    + totals.terminal_error;
                if sent >= total {
                    stop_flag.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    stop_flag.store(true, Ordering::Relaxed);
    let drain_deadline = Instant::now() + Duration::from_secs(cfg.shutdown_grace_secs);
    loop {
        if Instant::now() >= drain_deadline {
            break;
        }
        if join_set.is_empty() {
            break;
        }
        if let Ok(Some(joined)) = timeout(Duration::from_millis(100), join_set.join_next()).await {
            if let Err(err) = joined {
                worker_error.get_or_insert_with(|| format!("worker join error: {err}"));
            }
            continue;
        }
    }

    join_set.shutdown().await;

    let _ = metrics_task.await;

    let totals = collect_totals(&worker_states).await;
    let samples = aggregate_samples.lock().await.clone();
    let (achieved_tps_avg, achieved_tps_p95_1m, max_stalled_ratio, pass_windows) =
        evaluate_windows(cfg, &samples);

    let denominator =
        totals.accepted + totals.rejected + totals.timeout + totals.dropped + totals.terminal_error;
    let failure_ratio = if denominator > 0 {
        (totals.rejected + totals.timeout + totals.dropped + totals.terminal_error) as f64
            / denominator as f64
    } else {
        0.0
    };

    let mut fail_reasons = Vec::new();

    if cfg.calibration_mode {
        if failure_ratio > 0.05 {
            fail_reasons.push("calibration failure_ratio > 0.05".to_string());
        }
    } else if cfg.preflight_sample_mode {
        if totals.terminal_error > 0 {
            fail_reasons.push("preflight-sample encountered terminal errors".to_string());
        }
    } else {
        if failure_ratio > 0.01 {
            fail_reasons.push(format!("failure_ratio {} > 0.01", failure_ratio));
        }
        if !pass_windows {
            fail_reasons.push("throughput windows fail 95%/95% rule".to_string());
        }
        if max_stalled_ratio > 0.02 {
            fail_reasons.push(format!("max stalled ratio {} > 0.02", max_stalled_ratio));
        }
    }
    if let Some(err) = worker_error {
        fail_reasons.push(err);
    }

    Ok(RunResult {
        totals,
        achieved_tps_avg,
        achieved_tps_p95_1m,
        failure_ratio,
        max_stalled_ratio,
        pass: fail_reasons.is_empty(),
        fail_reasons,
        interrupted_exit_code: {
            let code = interrupted_exit_code.load(Ordering::Relaxed);
            if code == 0 { None } else { Some(code) }
        },
        warmup_completed_at_utc: Some(warmup_completed_at_utc),
    })
}

fn install_signal_handlers(stop_flag: Arc<AtomicBool>, exit_code: Arc<AtomicI32>) {
    tokio::spawn(async move {
        tokio::select! {
            _ = signal::ctrl_c() => {
                exit_code.store(130, Ordering::Relaxed);
                stop_flag.store(true, Ordering::Relaxed);
            }
            _ = async {
                #[cfg(unix)]
                {
                    let mut term = signal::unix::signal(signal::unix::SignalKind::terminate())
                        .expect("install SIGTERM handler");
                    term.recv().await;
                }
                #[cfg(not(unix))]
                {
                    std::future::pending::<()>().await;
                }
            } => {
                exit_code.store(143, Ordering::Relaxed);
                stop_flag.store(true, Ordering::Relaxed);
            }
        }
    });
}

async fn metrics_loop(
    cfg: &ResolvedConfig,
    path: &Path,
    worker_states: &Arc<Mutex<Vec<WorkerState>>>,
    aggregate_samples: &Arc<Mutex<Vec<AggregateSample>>>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<()> {
    let file = File::create(path)
        .wrap_err_with(|| format!("failed to create metrics file {}", path.display()))?;
    let mut writer = BufWriter::new(file);

    let mut ticker = interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut prev = AggregatedTotals::default();
    let mut buffered_lines = Vec::<String>::new();
    let mut last_flush = Instant::now();
    let metrics_started = Instant::now();

    loop {
        ticker.tick().await;

        let now = Instant::now();
        let states = worker_states.lock().await.clone();

        let mut totals = AggregatedTotals::default();
        let mut stalled = 0usize;
        let mut active = 0usize;

        for s in &states {
            totals.accepted += s.counters.accepted;
            totals.rejected += s.counters.rejected;
            totals.timeout += s.counters.timeout;
            totals.dropped += s.counters.dropped;
            totals.terminal_error += s.counters.terminal_error;
            if !s.exited {
                active += 1;
                if let Some(last) = s.counters.last_accept_instant {
                    if now.duration_since(last) > Duration::from_secs(60) {
                        stalled += 1;
                    }
                } else if now.duration_since(metrics_started) > Duration::from_secs(60) {
                    stalled += 1;
                }
            }
        }

        let d_accept = totals.accepted.saturating_sub(prev.accepted);
        let d_rejected = totals.rejected.saturating_sub(prev.rejected);
        let d_timeout = totals.timeout.saturating_sub(prev.timeout);
        let d_dropped = totals.dropped.saturating_sub(prev.dropped);
        let d_terminal = totals.terminal_error.saturating_sub(prev.terminal_error);

        let mut el_latencies =
            states.iter().filter_map(|s| s.counters.last_el_rpc_latency_ms).collect::<Vec<_>>();
        let mut kaspa_latencies =
            states.iter().filter_map(|s| s.counters.last_kaspa_rpc_latency_ms).collect::<Vec<_>>();
        let el_rpc_p95_ms = percentile_u64(&mut el_latencies, 95.0);
        let kaspa_rpc_p95_ms = percentile_u64(&mut kaspa_latencies, 95.0);
        let el_pending_count = states
            .iter()
            .map(|s| s.counters.local_next_nonce.saturating_sub(s.counters.rpc_pending_nonce))
            .sum::<u64>();

        let sample = AggregateSample {
            ts_utc: utc_now(),
            tps_1s: d_accept as f64,
            accepted_1s: d_accept,
            rejected_1s: d_rejected,
            timeout_1s: d_timeout,
            dropped_1s: d_dropped,
            terminal_error_1s: d_terminal,
            el_rpc_p95_ms,
            kaspa_rpc_p95_ms,
            el_pending_count,
            kaspa_mempool_mass_estimate: None,
            active_workers: active,
            stalled_workers: stalled,
        };

        aggregate_samples.lock().await.push(sample.clone());

        buffered_lines.push(serde_json::to_string(&json!({
            "ts_utc": sample.ts_utc,
            "tps_1s": sample.tps_1s,
            "accepted_1s": sample.accepted_1s,
            "rejected_1s": sample.rejected_1s,
            "timeout_1s": sample.timeout_1s,
            "dropped_1s": sample.dropped_1s,
            "terminal_error_1s": sample.terminal_error_1s,
            "el_rpc_p95_ms": sample.el_rpc_p95_ms,
            "kaspa_rpc_p95_ms": sample.kaspa_rpc_p95_ms,
            "el_pending_count": sample.el_pending_count,
            "kaspa_mempool_mass_estimate": sample.kaspa_mempool_mass_estimate,
            "active_workers": sample.active_workers,
            "stalled_workers": sample.stalled_workers,
        }))?);

        for s in &states {
            buffered_lines.push(
                serde_json::to_string(&json!({
                    "ts_utc": utc_now(),
                    "worker_id": format!("worker-{:03}", s.worker_id),
                    "wallet_index": s.wallet_index,
                    "tps_1s": Value::Null,
                    "accepted_total": s.counters.accepted,
                    "failed_total": s.counters.rejected + s.counters.timeout + s.counters.dropped + s.counters.terminal_error,
                    "local_next_nonce": s.counters.local_next_nonce,
                    "rpc_pending_nonce": s.counters.rpc_pending_nonce,
                    "replacement_count_total": s.counters.replacement_count_total,
                    "el_rpc_latency_ms": s.counters.last_el_rpc_latency_ms,
                    "kaspa_rpc_latency_ms": s.counters.last_kaspa_rpc_latency_ms,
                    "endpoint": s.counters.active_endpoint,
                    "last_error_code": s.counters.last_error_code,
                }))?,
            );
        }

        let should_flush =
            buffered_lines.len() >= 100 || last_flush.elapsed() >= Duration::from_secs(10);
        if should_flush {
            for line in buffered_lines.drain(..) {
                writer.write_all(line.as_bytes())?;
                writer.write_all(b"\n")?;
            }
            writer.flush()?;
            last_flush = Instant::now();
        }

        prev = totals;

        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
    }

    if !buffered_lines.is_empty() {
        for line in buffered_lines.drain(..) {
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
    }

    if cfg.report_secs == 0 {
        // Keep field used and silence dead-code warnings if report is disabled.
    }

    Ok(())
}

async fn run_worker(
    cfg: &ResolvedConfig,
    plan: &WorkerPlan,
    worker_states: &Arc<Mutex<Vec<WorkerState>>>,
    event_tx: &mpsc::Sender<WorkerEvent>,
    start_rx: &mut watch::Receiver<bool>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<()> {
    let mut rng = StdRng::seed_from_u64(cfg.rpc_random_seed.wrapping_add(plan.worker_id as u64));

    let transport_pool = build_transport_pool(cfg, &plan.kaspa_key)?;
    let mut transport_health = vec![TransportHealth::default(); transport_pool.len()];
    let mut transport_index = rng.random_range(0..transport_pool.len());
    let mut nonce = {
        let mut t = transport_pool[transport_index].transport.clone();
        eth_get_transaction_count_pending(&mut t, plan.evm_sender).await?
    };
    let signer = plan
        .evm_key
        .parse::<PrivateKeySigner>()
        .map_err(|e| eyre!("invalid signer for worker {}: {e}", plan.worker_id))?;
    let chain_id = {
        let mut t = transport_pool[transport_index].transport.clone();
        eth_chain_id(&mut t).await?
    };

    {
        let mut states = worker_states.lock().await;
        if let Some(s) = states.iter_mut().find(|s| s.worker_id == plan.worker_id) {
            s.counters.local_next_nonce = nonce;
            s.counters.rpc_pending_nonce = nonce;
        }
    }

    for _ in 0..cfg.warmup_txs_per_worker {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        send_one(
            cfg,
            plan,
            &signer,
            chain_id,
            &transport_pool,
            &mut transport_health,
            &mut transport_index,
            &mut nonce,
            worker_states,
            false,
            true,
            None,
            &mut rng,
        )
        .await?;
    }

    event_tx.send(WorkerEvent::WarmupComplete { worker_id: plan.worker_id }).await.ok();

    if !*start_rx.borrow() {
        start_rx.changed().await.wrap_err("failed waiting for start barrier")?;
    }

    let per_worker_tps =
        if cfg.target_tps > 0.0 { cfg.target_tps / cfg.worker_count as f64 } else { 0.0 };
    let mut ticker = if per_worker_tps > 0.0 {
        let mut t = interval(Duration::from_secs_f64(1.0 / per_worker_tps));
        t.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Some(t)
    } else {
        None
    };

    if matches!(cfg.mode, Mode::PrebuildSend) {
        run_worker_prebuild_pipeline(
            cfg,
            plan,
            &signer,
            chain_id,
            &transport_pool,
            &mut transport_health,
            &mut transport_index,
            &mut nonce,
            worker_states,
            stop_flag,
            ticker,
            &mut rng,
        )
        .await?;
    } else {
        loop {
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }

            if worker_reached_target(worker_states, plan.worker_id, plan.per_worker_total_target)
                .await
            {
                break;
            }

            if let Some(t) = ticker.as_mut() {
                t.tick().await;
            }

            send_one(
                cfg,
                plan,
                &signer,
                chain_id,
                &transport_pool,
                &mut transport_health,
                &mut transport_index,
                &mut nonce,
                worker_states,
                true,
                false,
                None,
                &mut rng,
            )
            .await?;
        }
    }

    {
        let mut states = worker_states.lock().await;
        if let Some(s) = states.iter_mut().find(|s| s.worker_id == plan.worker_id) {
            s.exited = true;
        }
    }

    Ok(())
}

async fn worker_reached_target(
    worker_states: &Arc<Mutex<Vec<WorkerState>>>,
    worker_id: usize,
    target: Option<u64>,
) -> bool {
    let Some(target) = target else {
        return false;
    };
    let states = worker_states.lock().await;
    let current = states
        .iter()
        .find(|s| s.worker_id == worker_id)
        .map(|s| {
            s.counters.accepted
                + s.counters.rejected
                + s.counters.timeout
                + s.counters.dropped
                + s.counters.terminal_error
        })
        .unwrap_or(0);
    current >= target
}

#[allow(clippy::too_many_arguments)]
async fn run_worker_prebuild_pipeline(
    cfg: &ResolvedConfig,
    plan: &WorkerPlan,
    signer: &PrivateKeySigner,
    chain_id: u64,
    transport_pool: &[TransportSlot],
    transport_health: &mut [TransportHealth],
    transport_index: &mut usize,
    nonce: &mut u64,
    worker_states: &Arc<Mutex<Vec<WorkerState>>>,
    stop_flag: &Arc<AtomicBool>,
    mut ticker: Option<tokio::time::Interval>,
    rng: &mut StdRng,
) -> Result<()> {
    let per_worker_tps =
        if cfg.target_tps > 0.0 { cfg.target_tps / cfg.worker_count as f64 } else { 0.0 };
    let mut queue_capacity =
        ((per_worker_tps * cfg.prebuild_horizon_secs as f64).ceil() as usize).max(1);
    if let Some(target) = plan.per_worker_total_target {
        queue_capacity = queue_capacity.min(target.max(1) as usize);
    }

    let (tx_prebuilt, mut rx_prebuilt) = mpsc::channel::<PrebuiltTx>(queue_capacity);
    let producer_stop = stop_flag.clone();
    let to = cfg.explicit_to.unwrap_or(plan.contract);
    let data =
        if let Some(d) = cfg.explicit_data.as_ref() { d.clone() } else { plan.call_data.clone() };
    let signer_prebuild = signer.clone();
    let cfg_cloned = cfg.clone();
    let mut next_nonce = *nonce;
    let per_worker_target = plan.per_worker_total_target;

    let producer = tokio::spawn(async move {
        let mut produced = 0u64;
        loop {
            if producer_stop.load(Ordering::Relaxed) {
                break;
            }
            if let Some(target) = per_worker_target
                && produced >= target
            {
                break;
            }
            let raw = build_signed_eip1559(
                &signer_prebuild,
                chain_id,
                next_nonce,
                cfg_cloned.gas_limit,
                cfg_cloned.max_fee_per_gas,
                cfg_cloned.max_priority_fee_per_gas,
                to,
                &data,
            )?;
            let tx = PrebuiltTx { nonce: next_nonce, raw };
            if tx_prebuilt.send(tx).await.is_err() {
                break;
            }
            next_nonce = next_nonce.saturating_add(1);
            produced = produced.saturating_add(1);
        }
        Result::<()>::Ok(())
    });

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        if worker_reached_target(worker_states, plan.worker_id, plan.per_worker_total_target).await
        {
            break;
        }
        if let Some(t) = ticker.as_mut() {
            t.tick().await;
        }

        let Some(prebuilt) = rx_prebuilt.recv().await else {
            break;
        };
        if prebuilt.nonce < *nonce {
            let mut states = worker_states.lock().await;
            if let Some(s) = states.iter_mut().find(|s| s.worker_id == plan.worker_id) {
                s.counters.dropped = s.counters.dropped.saturating_add(1);
                s.counters.last_error_code = Some("PREBUILD_STALE_NONCE".to_string());
            }
            continue;
        }

        send_one(
            cfg,
            plan,
            signer,
            chain_id,
            transport_pool,
            transport_health,
            transport_index,
            nonce,
            worker_states,
            true,
            false,
            Some(prebuilt),
            rng,
        )
        .await?;
    }

    drop(rx_prebuilt);
    match producer.await {
        Ok(inner) => inner?,
        Err(err) => return Err(eyre!("prebuild producer task failed: {err}")),
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn send_one(
    cfg: &ResolvedConfig,
    plan: &WorkerPlan,
    signer: &PrivateKeySigner,
    chain_id: u64,
    transport_pool: &[TransportSlot],
    transport_health: &mut [TransportHealth],
    transport_index: &mut usize,
    nonce: &mut u64,
    worker_states: &Arc<Mutex<Vec<WorkerState>>>,
    count_metrics: bool,
    must_succeed: bool,
    prebuilt_tx: Option<PrebuiltTx>,
    rng: &mut StdRng,
) -> Result<()> {
    let to = cfg.explicit_to.unwrap_or(plan.contract);
    let data =
        if let Some(d) = cfg.explicit_data.as_ref() { d.clone() } else { plan.call_data.clone() };

    let mut local_nonce = prebuilt_tx.as_ref().map(|p| p.nonce).unwrap_or(*nonce);
    let prebuilt_raw = prebuilt_tx.as_ref().map(|p| p.raw.clone());
    let mut max_fee_per_gas = cfg.max_fee_per_gas;
    let mut max_priority_fee_per_gas = cfg.max_priority_fee_per_gas;
    let mut replacement_attempts = 0u64;

    loop {
        let selected_idx = select_transport_index(
            cfg,
            transport_pool.len(),
            transport_health,
            transport_index,
            rng,
        )
        .await?;
        *transport_index = selected_idx;
        let endpoint_id = transport_pool[selected_idx].endpoint_id.clone();
        let mut transport = transport_pool[selected_idx].transport.clone();

        let raw = if replacement_attempts == 0 {
            if let Some(raw) = prebuilt_raw.as_ref() {
                raw.clone()
            } else {
                build_signed_eip1559(
                    signer,
                    chain_id,
                    local_nonce,
                    cfg.gas_limit,
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                    to,
                    &data,
                )?
            }
        } else {
            build_signed_eip1559(
                signer,
                chain_id,
                local_nonce,
                cfg.gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                to,
                &data,
            )?
        };

        let send_started = Instant::now();
        let send_result = transport.request(send_raw_packet(&raw)).await;
        let elapsed_ms = send_started.elapsed().as_millis() as u64;

        match send_result {
            Ok(_) => {
                mark_transport_success(transport_health, selected_idx);
                local_nonce = local_nonce.saturating_add(1);
                *nonce = (*nonce).max(local_nonce);
                let mut states = worker_states.lock().await;
                if let Some(s) = states.iter_mut().find(|s| s.worker_id == plan.worker_id) {
                    s.counters.local_next_nonce = local_nonce;
                    s.counters.rpc_pending_nonce = local_nonce;
                    s.counters.last_error_code = None;
                    s.counters.last_kaspa_rpc_latency_ms = Some(elapsed_ms);
                    s.counters.active_endpoint = Some(endpoint_id);
                    if count_metrics {
                        s.counters.accepted = s.counters.accepted.saturating_add(1);
                        s.counters.replacement_count_total =
                            s.counters.replacement_count_total.saturating_add(replacement_attempts);
                        s.counters.last_accept_instant = Some(Instant::now());
                    }
                }
                return Ok(());
            }
            Err(err) => {
                let err_text = err.to_string();
                let failure = classify_error(&err_text);
                if std::env::var_os("IGRA_STRESS_DEBUG_ERRORS").is_some() {
                    eprintln!(
                        "[igra-loadgen] worker-{:03} send error (class={:?}, nonce={}, endpoint={}): {}",
                        plan.worker_id, failure, local_nonce, endpoint_id, err_text
                    );
                }

                let mut refreshed_nonce = None;
                let mut refreshed_nonce_latency_ms = None;
                if err_text.contains("IGRA_NONCE_")
                    || err_text.to_ascii_lowercase().contains("nonce")
                {
                    let t0 = Instant::now();
                    refreshed_nonce =
                        eth_get_transaction_count_pending(&mut transport, plan.evm_sender)
                            .await
                            .ok();
                    refreshed_nonce_latency_ms = Some(t0.elapsed().as_millis() as u64);
                    if let Some(n) = refreshed_nonce {
                        local_nonce = n;
                        *nonce = n;
                    }
                }

                if matches!(failure, FailureClass::TerminalError | FailureClass::Timeout) {
                    mark_transport_failure(
                        transport_health,
                        selected_idx,
                        cfg.degraded_pause_max_secs,
                    );
                } else {
                    mark_transport_success(transport_health, selected_idx);
                }

                let retryable = matches!(failure, FailureClass::Rejected | FailureClass::Timeout);
                if retryable && replacement_attempts < cfg.max_replacements_per_nonce {
                    replacement_attempts = replacement_attempts.saturating_add(1);
                    max_fee_per_gas = bump_fee(max_fee_per_gas, cfg.replacement_fee_bump_pct);
                    max_priority_fee_per_gas =
                        bump_fee(max_priority_fee_per_gas, cfg.replacement_fee_bump_pct)
                            .min(max_fee_per_gas);

                    let backoff = replacement_backoff_secs(cfg, replacement_attempts);
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    continue;
                }

                if count_metrics {
                    let mut states = worker_states.lock().await;
                    if let Some(s) = states.iter_mut().find(|s| s.worker_id == plan.worker_id) {
                        s.counters.replacement_count_total =
                            s.counters.replacement_count_total.saturating_add(replacement_attempts);
                        s.counters.last_error_code = Some(classify_error_code(&err_text));
                        if let Some(el_ms) = refreshed_nonce_latency_ms {
                            s.counters.last_el_rpc_latency_ms = Some(el_ms);
                        }
                        s.counters.last_kaspa_rpc_latency_ms = Some(elapsed_ms);
                        s.counters.active_endpoint = Some(endpoint_id);
                        if let Some(n) = refreshed_nonce {
                            s.counters.rpc_pending_nonce = n;
                            s.counters.local_next_nonce = n;
                        }
                        match failure {
                            FailureClass::Rejected => {
                                s.counters.rejected = s.counters.rejected.saturating_add(1)
                            }
                            FailureClass::Timeout => {
                                s.counters.timeout = s.counters.timeout.saturating_add(1);
                            }
                            FailureClass::TerminalError => {
                                s.counters.terminal_error =
                                    s.counters.terminal_error.saturating_add(1)
                            }
                        }
                    }
                }

                if must_succeed {
                    return Err(eyre!("required send failed: {err_text}"));
                }
                if matches!(failure, FailureClass::TerminalError) {
                    return Err(eyre!("terminal worker error: {err_text}"));
                }
                return Ok(());
            }
        }
    }
}

fn classify_error(err_text: &str) -> FailureClass {
    let lower = err_text.to_ascii_lowercase();
    if err_text.contains(IGRA_MINING_TIMEOUT_ERROR_CODE) || lower.contains("timed out") {
        FailureClass::Timeout
    } else if lower.contains("failed to connect")
        || lower.contains("rpc error")
        || lower.contains("transport")
    {
        FailureClass::TerminalError
    } else {
        FailureClass::Rejected
    }
}

fn classify_error_code(err_text: &str) -> String {
    if err_text.contains("IGRA_NONCE_001") {
        "IGRA_NONCE_001".to_string()
    } else if err_text.contains("IGRA_NONCE_002") {
        "IGRA_NONCE_002".to_string()
    } else if err_text.contains("IGRA_NONCE_003") {
        "IGRA_NONCE_003".to_string()
    } else if err_text.contains("IGRA_NONCE_004") {
        "IGRA_NONCE_004".to_string()
    } else if err_text.contains(IGRA_MINING_TIMEOUT_ERROR_CODE) {
        IGRA_MINING_TIMEOUT_ERROR_CODE.to_string()
    } else {
        "IGRA_ERR".to_string()
    }
}

async fn select_transport_index(
    cfg: &ResolvedConfig,
    pool_len: usize,
    health: &mut [TransportHealth],
    current_index: &usize,
    rng: &mut StdRng,
) -> Result<usize> {
    if pool_len == 0 {
        return Err(eyre!("transport pool is empty"));
    }
    let wait_deadline = Instant::now() + Duration::from_secs(cfg.degraded_pause_max_secs.max(1));
    loop {
        let now = Instant::now();
        let healthy: Vec<usize> = health
            .iter()
            .enumerate()
            .filter_map(|(i, h)| {
                if h.backoff_until.map(|t| now >= t).unwrap_or(true) { Some(i) } else { None }
            })
            .collect();
        if !healthy.is_empty() {
            if matches!(cfg.rpc_selection_mode, RpcSelectionMode::RandomPerStep) {
                let idx = rng.random_range(0..healthy.len());
                return Ok(healthy[idx]);
            }
            if healthy.contains(current_index) {
                return Ok(*current_index);
            }
            return Ok(*healthy.first().expect("checked non-empty"));
        }
        if Instant::now() >= wait_deadline {
            return Err(eyre!(
                "all rpc endpoints remained unhealthy for > {}s",
                cfg.degraded_pause_max_secs.max(1)
            ));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn mark_transport_success(health: &mut [TransportHealth], idx: usize) {
    if let Some(h) = health.get_mut(idx) {
        h.consecutive_failures = 0;
        h.backoff_until = None;
    }
}

fn mark_transport_failure(health: &mut [TransportHealth], idx: usize, max_pause_secs: u64) {
    if let Some(h) = health.get_mut(idx) {
        h.consecutive_failures = h.consecutive_failures.saturating_add(1);
        let exp = h.consecutive_failures.saturating_sub(1).min(6);
        let base = 2u64.saturating_pow(exp);
        let pause_secs = base.min(max_pause_secs.max(1));
        h.backoff_until = Some(Instant::now() + Duration::from_secs(pause_secs));
    }
}

fn replacement_backoff_secs(cfg: &ResolvedConfig, attempt: u64) -> u64 {
    let base = cfg.pending_timeout_secs.max(1);
    let exp = attempt.saturating_sub(1).min(6);
    let backoff = base.saturating_mul(2u64.saturating_pow(exp as u32));
    backoff.min(cfg.replacement_timeout_cap_secs.max(1))
}

fn bump_fee(value: u128, bump_pct: u64) -> u128 {
    if bump_pct == 0 {
        return value;
    }
    let bumped = value.saturating_mul(100u128.saturating_add(bump_pct as u128)) / 100;
    bumped.max(value.saturating_add(1))
}

fn percentile_u64(samples: &mut [u64], percentile: f64) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let idx = ((samples.len() as f64) * (percentile / 100.0)).ceil() as usize;
    let idx = idx.saturating_sub(1).min(samples.len().saturating_sub(1));
    Some(samples[idx])
}

fn percentile_f64(samples: &mut [f64], percentile: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((samples.len() as f64) * (percentile / 100.0)).ceil() as usize;
    let idx = idx.saturating_sub(1).min(samples.len().saturating_sub(1));
    Some(samples[idx])
}

fn evaluate_windows(cfg: &ResolvedConfig, samples: &[AggregateSample]) -> (f64, f64, f64, bool) {
    if samples.is_empty() {
        return (0.0, 0.0, 0.0, false);
    }

    let achieved_tps_avg = samples.iter().map(|s| s.tps_1s).sum::<f64>() / samples.len() as f64;

    let mut one_minute_tps = Vec::new();
    for chunk in samples.chunks(60) {
        let sum_accept = chunk.iter().map(|s| s.accepted_1s).sum::<u64>();
        let secs = chunk.len().max(1) as f64;
        one_minute_tps.push(sum_accept as f64 / secs);
    }

    let mut sorted = one_minute_tps.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((sorted.len() as f64) * 0.95).floor() as usize;
    let idx = idx.min(sorted.len().saturating_sub(1));
    let achieved_tps_p95_1m = sorted[idx];

    let threshold = cfg.target_tps * 0.95;
    let passing = one_minute_tps.iter().filter(|v| **v >= threshold).count();
    let windows_ratio = passing as f64 / one_minute_tps.len().max(1) as f64;
    let pass_windows = windows_ratio >= 0.95;

    let max_stalled_ratio = samples
        .iter()
        .map(|s| {
            if s.active_workers == 0 {
                0.0
            } else {
                s.stalled_workers as f64 / s.active_workers as f64
            }
        })
        .fold(0.0f64, f64::max);

    (achieved_tps_avg, achieved_tps_p95_1m, max_stalled_ratio, pass_windows)
}

async fn collect_totals(worker_states: &Arc<Mutex<Vec<WorkerState>>>) -> AggregatedTotals {
    let states = worker_states.lock().await;
    let mut totals = AggregatedTotals::default();
    for s in states.iter() {
        totals.accepted += s.counters.accepted;
        totals.rejected += s.counters.rejected;
        totals.timeout += s.counters.timeout;
        totals.dropped += s.counters.dropped;
        totals.terminal_error += s.counters.terminal_error;
    }
    totals
}

#[allow(clippy::too_many_arguments)]
fn emit_manifest(
    cfg: &ResolvedConfig,
    plans: &[WorkerPlan],
    campaign_id: &str,
    started_at_utc: &str,
    ended_at_utc: Option<&str>,
    observed_fee_floor_gwei: u64,
    result: Option<&RunResult>,
    warmup_completed_at_utc: Option<&str>,
) -> Result<()> {
    let wallet_start = plans.first().map(|p| p.wallet_index).unwrap_or(0);
    let wallet_end = plans.last().map(|p| p.wallet_index).unwrap_or(0);

    let mut resolved_parameters = HashMap::new();
    resolved_parameters.insert("IGRA_STRESS_NETWORK", cfg.network.as_str().to_string());
    resolved_parameters.insert("IGRA_STRESS_MODE", cfg.mode.as_str().to_string());
    resolved_parameters.insert("IGRA_STRESS_TARGET_TPS", format!("{}", cfg.target_tps));
    resolved_parameters.insert("IGRA_STRESS_WORKER_COUNT", format!("{}", cfg.worker_count));
    resolved_parameters.insert("IGRA_STRESS_EL_RPC_URLS", cfg.el_rpc_urls.join(","));
    resolved_parameters.insert("IGRA_STRESS_KASPA_RPC_URLS", cfg.kaspa_rpc_urls.join(","));

    let mut manifest = json!({
        "schema_version": "1.0.0",
        "campaign_id": campaign_id,
        "started_at_utc": started_at_utc,
        "ended_at_utc": ended_at_utc,
        "mode": cfg.mode.as_str(),
        "network": cfg.network.as_str(),
        "target_tps": cfg.target_tps,
        "worker_count": cfg.worker_count,
        "wallet_start_index": cfg.wallet_start_index,
        "resolved_parameters": resolved_parameters,
        "parameter_sources": cfg.parameter_sources,
        "resolved_wallet_index_range": format!("{}..{}", wallet_start, wallet_end),
        "resolved_contract_index_range": format!("{}..{}", wallet_start, wallet_end),
        "resolved_endpoints": {
            "igra_rpc_urls": cfg.el_rpc_urls,
            "kaspa_rpc_urls": cfg.kaspa_rpc_urls,
            "endpoint_set_sha256": cfg.endpoint_set_sha256,
        },
        "fee_policy": {
            "igra_min_fee_floor_gwei_expected": cfg.igra_min_fee_floor_gwei_expected,
            "igra_min_fee_floor_gwei_observed": observed_fee_floor_gwei,
            "kaspa_fee_mode": cfg.kaspa_fee_mode,
            "kaspa_fee_bucket": cfg.kaspa_fee_bucket,
        },
        "nonce_policy": {
            "pending_timeout_secs": cfg.pending_timeout_secs,
            "max_replacements_per_nonce": cfg.max_replacements_per_nonce,
            "replacement_fee_bump_pct": cfg.replacement_fee_bump_pct,
        },
        "utxo_policy": {
            "prebuild_horizon_secs": cfg.prebuild_horizon_secs,
            "utxo_refill_lag_secs": cfg.utxo_refill_lag_secs,
            "utxo_safety_factor": cfg.utxo_safety_factor,
            "utxos_per_wallet_min": if matches!(cfg.mode, Mode::PrebuildSend) {
                (((cfg.target_tps / cfg.worker_count as f64)
                    * (cfg.prebuild_horizon_secs + cfg.utxo_refill_lag_secs) as f64
                    * cfg.utxo_safety_factor)
                    .ceil() as u64)
                    .max(1)
            } else {
                1
            }
        },
        "build_versions": {
            "foundry": env!("CARGO_PKG_VERSION"),
            "igra": "unknown",
            "reth": "unknown",
            "rusty_kaspa_private": "unknown",
            "kaspaminer": "unknown",
        },
        "runtime": {
            "host_count": 1,
            "runner_version": env!("CARGO_PKG_VERSION"),
            "warmup_completed_at_utc": warmup_completed_at_utc,
        },
    });

    if let Some(r) = result {
        manifest["results"] = json!({
            "accepted": r.totals.accepted,
            "rejected": r.totals.rejected,
            "timeout": r.totals.timeout,
            "dropped": r.totals.dropped,
            "terminal_error": r.totals.terminal_error,
            "failure_ratio": r.failure_ratio,
            "achieved_tps_avg": r.achieved_tps_avg,
            "achieved_tps_p95_1m": r.achieved_tps_p95_1m,
            "pass": r.pass,
            "fail_reasons": r.fail_reasons,
        });
    }

    let path = cfg.campaign_output_dir.join("campaign-manifest.json");
    fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&manifest)?))
        .wrap_err_with(|| format!("failed to write manifest {}", path.display()))?;

    Ok(())
}

fn emit_calibration_report(cfg: &ResolvedConfig, run_result: &RunResult) -> Result<()> {
    let metrics_path = cfg.campaign_output_dir.join("metrics.ndjson");
    if !metrics_path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(&metrics_path)
        .wrap_err_with(|| format!("failed to read metrics file {}", metrics_path.display()))?;

    let mut tps_1s = Vec::<f64>::new();
    let mut el_rpc_p95_ms = Vec::<u64>::new();
    let mut kaspa_rpc_p95_ms = Vec::<u64>::new();

    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(tps) = v.get("tps_1s").and_then(|x| x.as_f64()) {
            tps_1s.push(tps);
        }
        if let Some(el) = v.get("el_rpc_p95_ms").and_then(|x| x.as_u64()) {
            el_rpc_p95_ms.push(el);
        }
        if let Some(k) = v.get("kaspa_rpc_p95_ms").and_then(|x| x.as_u64()) {
            kaspa_rpc_p95_ms.push(k);
        }
    }

    let report = json!({
        "schema_version": "1.0.0",
        "network": cfg.network.as_str(),
        "mode": cfg.mode.as_str(),
        "target_tps": cfg.target_tps,
        "worker_count": cfg.worker_count,
        "totals": {
            "accepted": run_result.totals.accepted,
            "rejected": run_result.totals.rejected,
            "timeout": run_result.totals.timeout,
            "dropped": run_result.totals.dropped,
            "terminal_error": run_result.totals.terminal_error,
        },
        "failure_ratio": run_result.failure_ratio,
        "achieved_tps": {
            "avg": run_result.achieved_tps_avg,
            "p95_1m_windows": run_result.achieved_tps_p95_1m,
            "p50_1s": percentile_f64(&mut tps_1s, 50.0),
            "p95_1s": percentile_f64(&mut tps_1s, 95.0),
        },
        "latency_ms": {
            "el_rpc_p95_observed_p50": percentile_u64(&mut el_rpc_p95_ms, 50.0),
            "el_rpc_p95_observed_p95": percentile_u64(&mut el_rpc_p95_ms, 95.0),
            "kaspa_rpc_p95_observed_p50": percentile_u64(&mut kaspa_rpc_p95_ms, 50.0),
            "kaspa_rpc_p95_observed_p95": percentile_u64(&mut kaspa_rpc_p95_ms, 95.0),
        },
        "pass": run_result.pass,
        "fail_reasons": run_result.fail_reasons,
    });

    let path = cfg.campaign_output_dir.join("calibration-report.json");
    fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&report)?))
        .wrap_err_with(|| format!("failed to write calibration report {}", path.display()))?;
    Ok(())
}

fn build_transport(
    el_rpc_url: &str,
    kaspa_rpc_url: &str,
    kaspa_key: &str,
    cfg: &ResolvedConfig,
) -> Result<IgraTransport<RuntimeTransport>> {
    let inner = build_runtime_transport(el_rpc_url, cfg.no_proxy)?;
    Ok(IgraTransport::new(inner, true).with_transport_config(IgraTransportConfig {
        tx_id_prefix: Some(cfg.tx_id_prefix.clone()),
        mining_timeout_secs: Some(cfg.mining_timeout_secs),
        kaspa_rpc_url: Some(kaspa_rpc_url.to_string()),
        kaspa_network: Some(cfg.network.as_str().to_string()),
        payload_compression: Some("none".to_string()),
        // Prefer chaining across unconfirmed spends to avoid mempool UTXO reuse conflicts
        // during sustained send loops.
        kaspa_utxo_mode: Some("chain".to_string()),
        kaspa_fee_mode: Some(cfg.kaspa_fee_mode.clone()),
        kaspa_fee_bucket: Some(cfg.kaspa_fee_bucket.clone()),
        kaspa_wallet: IgraKaspaWalletConfig {
            private_key: Some(kaspa_key.to_string()),
            ..Default::default()
        },
    }))
}

fn build_transport_pool(cfg: &ResolvedConfig, kaspa_key: &str) -> Result<Vec<TransportSlot>> {
    let mut pool = Vec::new();
    let pair_count = cfg.el_rpc_urls.len().max(cfg.kaspa_rpc_urls.len());
    for i in 0..pair_count {
        let el = &cfg.el_rpc_urls[i % cfg.el_rpc_urls.len()];
        let kaspa = &cfg.kaspa_rpc_urls[i % cfg.kaspa_rpc_urls.len()];
        let endpoint_id = format!("el={} kaspa={}", el, kaspa);
        pool.push(TransportSlot {
            endpoint_id,
            transport: build_transport(el, kaspa, kaspa_key, cfg)?,
        });
    }
    Ok(pool)
}

fn build_runtime_transport(el_rpc_url: &str, no_proxy: bool) -> Result<RuntimeTransport> {
    let url =
        Url::parse(el_rpc_url).wrap_err_with(|| format!("invalid EL RPC URL: {el_rpc_url}"))?;
    Ok(RuntimeTransportBuilder::new(url)
        .with_timeout(Duration::from_secs(30))
        .no_proxy(no_proxy)
        .build())
}

fn build_signed_eip1559(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    to: Address,
    input: &[u8],
) -> Result<Vec<u8>> {
    let mut tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input: Bytes::from(input.to_vec()),
        access_list: Default::default(),
    };
    let sig = signer.sign_transaction_sync(&mut tx).wrap_err("sign eip1559")?;
    let signed = Signed::new_unhashed(tx, sig);
    let mut raw = Vec::with_capacity(signed.eip2718_encoded_length());
    signed.eip2718_encode(&mut raw);
    Ok(raw)
}

fn send_raw_packet(raw_tx: &[u8]) -> RequestPacket {
    let encoded = format!("0x{}", hex::encode(raw_tx));
    let req: Request<Vec<String>> =
        Request::new("eth_sendRawTransaction".to_string(), Id::Number(1), vec![encoded]);
    RequestPacket::Single(req.serialize().expect("serialize request"))
}

async fn eth_chain_id<T>(transport: &mut T) -> Result<u64>
where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError> + Send,
    T::Future: Send,
{
    let req: Request<Vec<()>> = Request::new("eth_chainId".to_string(), Id::Number(1), vec![]);
    let req = req.serialize().map_err(|err| eyre!("rpc request serialize error: {err}"))?;
    let resp = transport.call(RequestPacket::Single(req)).await?;
    parse_hex_u64(single_success_value(resp)?).wrap_err("failed to parse eth_chainId")
}

async fn eth_get_balance<T>(transport: &mut T, addr: Address) -> Result<u128>
where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError> + Send,
    T::Future: Send,
{
    let params = vec![format!("{addr:#x}"), "latest".to_string()];
    let req: Request<Vec<String>> =
        Request::new("eth_getBalance".to_string(), Id::Number(1), params);
    let req = req.serialize().map_err(|err| eyre!("rpc request serialize error: {err}"))?;
    let resp = transport.call(RequestPacket::Single(req)).await?;
    parse_hex_u128(single_success_value(resp)?).wrap_err("failed to parse eth_getBalance")
}

async fn eth_get_code<T>(transport: &mut T, addr: Address) -> Result<String>
where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError> + Send,
    T::Future: Send,
{
    let params = vec![format!("{addr:#x}"), "latest".to_string()];
    let req: Request<Vec<String>> = Request::new("eth_getCode".to_string(), Id::Number(1), params);
    let req = req.serialize().map_err(|err| eyre!("rpc request serialize error: {err}"))?;
    let resp = transport.call(RequestPacket::Single(req)).await?;
    let v = single_success_value(resp)?;
    Ok(v.as_str().unwrap_or("0x").to_string())
}

async fn eth_gas_price<T>(transport: &mut T) -> Result<u64>
where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError> + Send,
    T::Future: Send,
{
    let req: Request<Vec<()>> = Request::new("eth_gasPrice".to_string(), Id::Number(1), vec![]);
    let req = req.serialize().map_err(|err| eyre!("rpc request serialize error: {err}"))?;
    let resp = transport.call(RequestPacket::Single(req)).await?;
    parse_hex_u64(single_success_value(resp)?).wrap_err("failed to parse eth_gasPrice")
}

async fn eth_get_transaction_count_pending<T>(transport: &mut T, addr: Address) -> Result<u64>
where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError> + Send,
    T::Future: Send,
{
    let params = vec![format!("{addr:#x}"), "pending".to_string()];
    let req: Request<Vec<String>> =
        Request::new("eth_getTransactionCount".to_string(), Id::Number(1), params);
    let req = req.serialize().map_err(|err| eyre!("rpc request serialize error: {err}"))?;
    let resp = transport.call(RequestPacket::Single(req)).await?;
    parse_hex_u64(single_success_value(resp)?).wrap_err("failed to parse eth_getTransactionCount")
}

fn single_success_value(packet: ResponsePacket) -> Result<Value> {
    match packet {
        ResponsePacket::Single(resp) => match resp.payload {
            ResponsePayload::Success(raw) => Ok(serde_json::from_str(raw.get())?),
            ResponsePayload::Failure(err) => Err(eyre!("rpc error: {}", err.message)),
        },
        ResponsePacket::Batch(_) => Err(eyre!("unexpected batch response")),
    }
}

fn parse_hex_u64(value: Value) -> Result<u64> {
    let s = value.as_str().ok_or_else(|| eyre!("expected hex string"))?;
    let s = s.strip_prefix("0x").unwrap_or(s);
    Ok(u64::from_str_radix(s, 16)?)
}

fn parse_hex_u128(value: Value) -> Result<u128> {
    let s = value.as_str().ok_or_else(|| eyre!("expected hex string"))?;
    let s = s.strip_prefix("0x").unwrap_or(s);
    Ok(u128::from_str_radix(s, 16)?)
}

fn parse_address(value: &str) -> Result<Address> {
    let value = value.trim();
    let value = value.strip_prefix("0x").unwrap_or(value);
    let bytes = hex::decode(value)?;
    if bytes.len() != 20 {
        return Err(eyre!("expected 20-byte address"));
    }
    Ok(Address::from_slice(&bytes))
}

fn parse_hex_bytes(value: &str) -> Result<Vec<u8>> {
    let value = value.trim();
    let value = value.strip_prefix("0x").unwrap_or(value);
    if value.is_empty() {
        return Ok(vec![]);
    }
    Ok(hex::decode(value)?)
}

fn add_address_offset(base: Address, offset: usize) -> Option<Address> {
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(base.as_slice());
    let mut carry = offset as u64;
    for i in (0..20).rev() {
        let add = (carry & 0xff) as u16;
        let sum = bytes[i] as u16 + add;
        bytes[i] = (sum & 0xff) as u8;
        carry = (carry >> 8) + ((sum >> 8) as u64);
    }
    if carry > 0 {
        return None;
    }
    Some(Address::from_slice(&bytes))
}

fn normalize_kaspa_rpc_url(v: &str) -> String {
    let trimmed = v.trim();
    if trimmed.starts_with("grpc://") { trimmed.to_string() } else { format!("grpc://{trimmed}") }
}

fn normalize_hex_prefix(mut input: String) -> String {
    input = input.trim().to_ascii_lowercase();
    if let Some(stripped) = input.strip_prefix("0x") { stripped.to_string() } else { input }
}

fn endpoint_set_sha256(el: &[String], kaspa: &[String]) -> String {
    let mut all = Vec::with_capacity(el.len() + kaspa.len());
    all.extend(el.iter().cloned());
    all.extend(kaspa.iter().cloned());
    all.sort();
    let payload = all.join("\n");
    let digest = Sha256::digest(payload.as_bytes());
    hex::encode(digest)
}

fn resolve_source(
    cli_args: &[String],
    cli_flags: &[&str],
    env_key: &str,
    from_file: bool,
    fallback: &str,
) -> String {
    if from_file {
        return "file".to_string();
    }
    if cli_flags
        .iter()
        .any(|f| cli_args.iter().any(|arg| arg == f || arg.starts_with(&format!("{f}="))))
    {
        return "cli".to_string();
    }
    if std::env::var_os(env_key).is_some() {
        return "env".to_string();
    }
    fallback.to_string()
}

fn parse_csv_list(v: &str) -> Vec<String> {
    v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| s.to_string()).collect()
}

fn load_wallets(path: &Path) -> Result<Vec<WalletEntry>> {
    let bytes = fs::read(path)
        .wrap_err_with(|| format!("failed to read wallets json {}", path.display()))?;
    let wallets: Vec<WalletEntry> =
        serde_json::from_slice(&bytes).wrap_err("invalid wallets json")?;
    if wallets.is_empty() {
        return Err(eyre!("wallets json is empty"));
    }
    Ok(wallets)
}

fn build_transfer_calldata(to: Address, amount: u128) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 32 + 32);
    out.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);

    let mut to_word = [0u8; 32];
    to_word[12..32].copy_from_slice(to.as_slice());
    out.extend_from_slice(&to_word);

    let mut amount_word = [0u8; 32];
    amount_word[16..32].copy_from_slice(&amount.to_be_bytes());
    out.extend_from_slice(&amount_word);

    out
}

fn utc_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn kaspa_address_from_private_key_hex(private_key: &str, network: &str) -> Result<KaspaAddress> {
    let prefix = match network.trim() {
        "mainnet" => KaspaAddressPrefix::Mainnet,
        "testnet-10" => KaspaAddressPrefix::Testnet,
        "devnet" => KaspaAddressPrefix::Devnet,
        "simnet" => KaspaAddressPrefix::Simnet,
        other => return Err(eyre!("unsupported kaspa network: {other}")),
    };

    let key = private_key.trim();
    let key = key.strip_prefix("0x").unwrap_or(key);
    let bytes = hex::decode(key)?;
    if bytes.len() != 32 {
        return Err(eyre!("expected 32-byte Kaspa private key hex"));
    }
    let mut fixed = [0u8; 32];
    fixed.copy_from_slice(&bytes);

    let secret = KaspaSecretKey::from_slice(&fixed)?;
    let public_key = kaspa_bip32::secp256k1::PublicKey::from_secret_key_global(&secret);
    let payload = public_key.x_only_public_key().0.serialize();
    Ok(KaspaAddress::new(prefix, KaspaAddressVersion::PubKey, &payload))
}

fn parse_private_key_hex(private_key: &str) -> Result<[u8; 32]> {
    let key = private_key.trim().strip_prefix("0x").unwrap_or(private_key.trim());
    let bytes = hex::decode(key).map_err(|err| eyre!("invalid private key hex: {err}"))?;
    if bytes.len() != 32 {
        return Err(eyre!("expected 32-byte private key hex"));
    }
    let mut fixed = [0u8; 32];
    fixed.copy_from_slice(&bytes);
    Ok(fixed)
}

fn kaspa_network_descriptor(network: &str) -> Result<(KaspaNetworkType, KaspaAddressPrefix)> {
    Ok(match network {
        "mainnet" => (KaspaNetworkType::Mainnet, KaspaAddressPrefix::Mainnet),
        "testnet-10" => (KaspaNetworkType::Testnet, KaspaAddressPrefix::Testnet),
        "devnet" => (KaspaNetworkType::Devnet, KaspaAddressPrefix::Devnet),
        "simnet" => (KaspaNetworkType::Simnet, KaspaAddressPrefix::Simnet),
        other => return Err(eyre!("unsupported kaspa network: {other}")),
    })
}

fn kaspa_address_from_private_key(
    private_key: &[u8; 32],
    prefix: KaspaAddressPrefix,
) -> Result<KaspaAddress> {
    let secret = KaspaSecretKey::from_slice(private_key)
        .map_err(|err| eyre!("invalid private key: {err}"))?;
    let public_key = kaspa_bip32::secp256k1::PublicKey::from_secret_key_global(&secret);
    let payload = public_key.x_only_public_key().0.serialize();
    Ok(KaspaAddress::new(prefix, KaspaAddressVersion::PubKey, &payload))
}

fn derive_kaspa_private_keys(
    mnemonic: &str,
    passphrase: Option<&str>,
    passphrase_as_mnemonic: bool,
    passphrase_empty: bool,
    derivation_path: Option<&str>,
    start_index: u32,
    count: u32,
) -> Result<Vec<String>> {
    let phrase = if Path::new(mnemonic).is_file() {
        fs::read_to_string(mnemonic)?
    } else {
        mnemonic.to_string()
    };
    let phrase = phrase.split_whitespace().collect::<Vec<_>>().join(" ");

    let effective_passphrase_owned;
    let effective_passphrase =
        if passphrase_as_mnemonic && passphrase.is_none() && !passphrase_empty {
            effective_passphrase_owned = phrase.clone();
            effective_passphrase_owned.as_str()
        } else if passphrase_empty {
            ""
        } else {
            passphrase.unwrap_or_default()
        };

    let kaspa_mnemonic = KaspaMnemonic::new(phrase, KaspaLanguage::English)
        .map_err(|err| eyre!("invalid Kaspa mnemonic: {err}"))?;
    let seed = kaspa_mnemonic.to_seed(effective_passphrase);
    let xprv = KaspaExtendedPrivateKey::<KaspaSecretKey>::new(seed)
        .map_err(|err| eyre!("failed to derive Kaspa master key: {err}"))?;

    let (path_override, base_xprv) = if let Some(path) = derivation_path {
        let path = path
            .parse::<KaspaDerivationPath>()
            .map_err(|err| eyre!("invalid Kaspa derivation path: {err}"))?;
        (Some(path), None)
    } else {
        let base = "m/44'/111111'/0'/0"
            .parse::<KaspaDerivationPath>()
            .map_err(|err| eyre!("failed to parse default Kaspa derivation path: {err}"))?;
        let base_xprv = xprv
            .clone()
            .derive_path(&base)
            .map_err(|err| eyre!("failed to derive default Kaspa base key: {err}"))?;
        (None, Some(base_xprv))
    };

    let mut keys = Vec::with_capacity(count as usize);
    for idx in start_index..start_index.saturating_add(count) {
        let secret = if let Some(path) = path_override.as_ref() {
            *xprv
                .clone()
                .derive_path(path)
                .map_err(|err| eyre!("failed to derive Kaspa key by path: {err}"))?
                .private_key()
        } else {
            let base = base_xprv.as_ref().expect("base_xprv is set when no path override");
            *base
                .clone()
                .derive_child(
                    KaspaChildNumber::new(idx, false)
                        .map_err(|err| eyre!("invalid Kaspa mnemonic index: {err}"))?,
                )
                .map_err(|err| eyre!("failed to derive Kaspa key by index: {err}"))?
                .private_key()
        };
        keys.push(format!("0x{}", hex::encode(secret.secret_bytes())));
    }
    Ok(keys)
}

fn parse_list_or_file(value: &str) -> Result<Vec<String>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(vec![]);
    }
    if Path::new(value).is_file() {
        let content = fs::read_to_string(value)?;
        let mut out = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            out.push(trimmed.to_string());
        }
        return Ok(out);
    }
    Ok(value
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_hash_is_deterministic() {
        let a = endpoint_set_sha256(
            &["https://b".to_string(), "https://a".to_string()],
            &["grpc://2".to_string(), "grpc://1".to_string()],
        );
        let b = endpoint_set_sha256(
            &["https://a".to_string(), "https://b".to_string()],
            &["grpc://1".to_string(), "grpc://2".to_string()],
        );
        assert_eq!(a, b);
    }

    #[test]
    fn transfer_calldata_shape_is_68_bytes() {
        let to = parse_address("0x0000000000000000000000000000000000000042").expect("parse");
        let data = build_transfer_calldata(to, 1);
        assert_eq!(data.len(), 68);
        assert_eq!(&data[0..4], &[0xa9, 0x05, 0x9c, 0xbb]);
    }

    #[test]
    fn evaluate_windows_works() {
        let cfg = ResolvedConfig {
            network: Network::Devnet,
            mode: Mode::FullCycle,
            target_tps: 100.0,
            worker_count: 10,
            wallet_start_index: 0,
            stop_condition: StopCondition::Duration(60),
            calibration_mode: false,
            preflight_sample_mode: false,
            wallets_json: PathBuf::from(DEFAULT_WALLETS_JSON),
            tx_id_prefix: "01".to_string(),
            gas_limit: 21_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            mining_timeout_secs: 1,
            warmup_txs_per_worker: 1,
            warmup_barrier_timeout_secs: 1,
            pending_timeout_secs: 1,
            max_replacements_per_nonce: 1,
            replacement_fee_bump_pct: 10,
            replacement_timeout_cap_secs: 1,
            shutdown_grace_secs: 1,
            degraded_pause_max_secs: 1,
            worker_recovery_timeout_secs: 1,
            report_secs: 1,
            rpc_selection_mode: RpcSelectionMode::StickyPerInstance,
            recipient_mode: RecipientMode::Ring,
            recipient_random_seed: 1,
            rpc_random_seed: 1,
            el_rpc_urls: vec!["https://x".to_string()],
            kaspa_rpc_urls: vec!["grpc://y".to_string()],
            endpoint_set_sha256: "x".to_string(),
            parameter_sources: HashMap::new(),
            contract_base_address: parse_address(DEFAULT_CONTRACT_BASE).expect("base"),
            contract_end_address: parse_address(DEFAULT_CONTRACT_END).expect("end"),
            preflight_balance_check: false,
            prebuild_horizon_secs: 120,
            utxo_refill_lag_secs: 20,
            utxo_safety_factor: 1.5,
            kaspa_fanout_enabled: false,
            kaspa_fanout_source_private_key: None,
            kaspa_fanout_source_mnemonic: None,
            kaspa_fanout_source_mnemonic_passphrase: None,
            kaspa_fanout_source_mnemonic_passphrase_as_mnemonic: false,
            kaspa_fanout_source_mnemonic_passphrase_empty: false,
            kaspa_fanout_source_mnemonic_index: 0,
            kaspa_fanout_amount_sompi: 100_000_000,
            kaspa_fanout_utxos_per_wallet: 1,
            kaspa_fanout_max_outputs_per_tx: 64,
            kaspa_fee_mode: "estimate".to_string(),
            kaspa_fee_bucket: "normal".to_string(),
            igra_min_fee_floor_gwei_expected: None,
            campaign_output_dir: PathBuf::from("/tmp"),
            no_proxy: false,
            explicit_to: None,
            explicit_data: None,
        };

        let mut samples = Vec::new();
        for _ in 0..120 {
            samples.push(AggregateSample {
                ts_utc: utc_now(),
                tps_1s: 100.0,
                accepted_1s: 100,
                rejected_1s: 0,
                timeout_1s: 0,
                dropped_1s: 0,
                terminal_error_1s: 0,
                el_rpc_p95_ms: Some(10),
                kaspa_rpc_p95_ms: Some(20),
                el_pending_count: 0,
                kaspa_mempool_mass_estimate: None,
                active_workers: 10,
                stalled_workers: 0,
            });
        }
        let (_, _, _, pass) = evaluate_windows(&cfg, &samples);
        assert!(pass);
    }
}
