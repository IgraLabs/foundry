//! IGRA Kaspa Submission TPS Benchmark
//!
//! Measures the maximum Kaspa L1 submission throughput by generating
//! multiple EVM transactions, wrapping them as IGRA payloads, mining
//! the Kaspa TX ID prefix, and broadcasting via gRPC.

use alloy_consensus::{Signed, TxEip1559};
use alloy_network::TxSignerSync;
use alloy_primitives::{Bytes, TxKind, U256, keccak256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use clap::Parser;
use kaspa_addresses::{
    Address as KaspaAddress, Prefix as KaspaAddressPrefix, Version as KaspaAddressVersion,
};
use kaspa_bip32::secp256k1::SecretKey as KaspaSecretKey;
use kaspa_consensus_core::{
    config::params::Params as KaspaParams,
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
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ── Constants (replicated from igra_transport.rs) ────────────────────────────

const BASE_SUBMIT_FEE_SOMPI: u64 = 200_000;
const FEE_PER_KIB_SOMPI: u64 = 20_000;
const EXTRA_INPUT_FEE_SOMPI: u64 = 10_000;
const MIN_CHANGE_SOMPI: u64 = 1_000;
const MAX_STANDARD_KASPA_TX_MASS: u64 = 100_000;

const IGRA_VERSION: u8 = 0x9;
const TX_TYPE_RAW_UNCOMPRESSED: u8 = 0x4;

/// Default IGRA EVM gas price (1 TKas = 1e12 wei). Overridden at startup via eth_gasPrice.
const DEFAULT_IGRA_GAS_PRICE: u128 = 1_000_000_000_000;

/// Minimum per-sender funding amount (~0.105 KAS) to keep storage mass within limits.
/// Storage mass = C/output ≈ 10^12/10M = 100K exactly at the limit, so we add
/// a 5% margin to avoid borderline rejections.
const MIN_SENDER_FUNDING_SOMPI: u64 = 10_500_000;

/// Maximum fraction of input UTXO that can go to the sender output.
/// Kaspa storage mass = C * (sum(1/output) - sum(1/input)), C=10^12.
/// To stay under 100K mass with a 2-output split, neither output can be
/// too small relative to the input. Keeping each output >= input/3
/// ensures safe ratios for inputs down to ~0.4 KAS.
const MAX_SENDER_FRACTION: u64 = 3;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(about = "IGRA Kaspa submission TPS benchmark")]
struct Args {
    /// Kaspa gRPC endpoint.
    #[arg(long, env = "KASPA_RPC_URL")]
    kaspa_rpc_url: String,

    /// Kaspa network (mainnet, testnet-10).
    #[arg(long, env = "KASPA_NETWORK", default_value = "mainnet")]
    network: String,

    /// Hex private key of the funded master wallet.
    #[arg(long, env = "KASPA_MASTER_PRIVATE_KEY", hide_env_values = true)]
    master_private_key: String,

    /// Required Kaspa TX ID prefix (hex, e.g. "97b5").
    #[arg(long, env = "IGRA_TX_ID_PREFIX")]
    tx_id_prefix: String,

    /// Number of parallel senders.
    #[arg(long, default_value_t = 10)]
    num_senders: usize,

    /// Transactions per sender per round.
    #[arg(long, default_value_t = 10)]
    txs_per_sender: usize,

    /// Nonce mining timeout in seconds.
    #[arg(long, default_value_t = 120)]
    mining_timeout_secs: u64,

    /// EVM chain ID for dummy self-transfer transactions.
    #[arg(long, default_value_t = 38837)]
    chain_id: u64,

    /// EVM RPC for nonce lookup.
    #[arg(long, default_value = "https://galleon.igralabs.com:8545")]
    el_rpc_url: String,

    /// Mine & sign but don't submit to Kaspa.
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Path to save/load sender keypairs (JSON). If the file exists and
    /// contains the right number of senders, they are reused.
    #[arg(long)]
    senders_file: Option<String>,

    /// Sweep-only mode: load senders from --senders-file, sweep all funds
    /// back to master, then exit (no benchmark run).
    #[arg(long, default_value_t = false)]
    sweep_only: bool,

    /// Start from this round number (0-indexed). Use to resume an interrupted
    /// run — senders already have UTXOs from the previous run, and EVM nonces
    /// will start from this value.
    #[arg(long, default_value_t = 0)]
    start_round: usize,

    /// Minimum gas price floor (sompi-wei). The benchmark uses
    /// max(eth_gasPrice, min_gas_price) to avoid silent TX rejection.
    #[arg(long, default_value_t = 2_000_000_000_000)]
    min_gas_price: u128,

    /// Skip EVM verification of submitted TXs.
    #[arg(long, default_value_t = false)]
    no_verify_evm: bool,

    /// Skip L1 and EVM funding phases (assume senders are already funded).
    /// Use when running pre-funded senders on a remote machine.
    #[arg(long, default_value_t = false)]
    skip_funding: bool,

    /// Skip EVM funding & confirmation (phases 2b, 2c). Only fund senders on
    /// L1 and run benchmark rounds without EVM pre-funding. Useful for
    /// measuring pure L1 throughput.
    #[arg(long, default_value_t = false)]
    skip_evm: bool,

    /// Use parallel funding: split master balance to N workers, each funds
    /// a chunk of senders concurrently. ~20x faster than sequential.
    #[arg(long, default_value_t = false)]
    parallel_funding: bool,

    /// Number of parallel funding workers (only with --parallel-funding).
    #[arg(long, default_value_t = 20)]
    funding_workers: usize,
}

// ── Shared helpers ───────────────────────────────────────────────────────────

fn estimated_fee_sompi(payload_len: usize, inputs: usize) -> u64 {
    let payload_len = u64::try_from(payload_len).unwrap_or(u64::MAX);
    let kib = payload_len.div_ceil(1024);
    let input_tail = u64::try_from(inputs.saturating_sub(1)).unwrap_or(u64::MAX);
    BASE_SUBMIT_FEE_SOMPI
        .saturating_add(kib.saturating_mul(FEE_PER_KIB_SOMPI))
        .saturating_add(input_tail.saturating_mul(EXTRA_INPUT_FEE_SOMPI))
}

fn kaspa_address_from_private_key(
    private_key: &[u8; 32],
    prefix: KaspaAddressPrefix,
) -> eyre::Result<KaspaAddress> {
    let secret = KaspaSecretKey::from_slice(private_key)?;
    let public_key = kaspa_bip32::secp256k1::PublicKey::from_secret_key_global(&secret);
    let payload = public_key.x_only_public_key().0.serialize();
    Ok(KaspaAddress::new(prefix, KaspaAddressVersion::PubKey, &payload))
}

fn kaspa_network_descriptor(
    network: &str,
) -> eyre::Result<(KaspaNetworkType, KaspaAddressPrefix)> {
    match network {
        "mainnet" => Ok((KaspaNetworkType::Mainnet, KaspaAddressPrefix::Mainnet)),
        "testnet-10" => Ok((KaspaNetworkType::Testnet, KaspaAddressPrefix::Testnet)),
        "devnet" => Ok((KaspaNetworkType::Devnet, KaspaAddressPrefix::Devnet)),
        "simnet" => Ok((KaspaNetworkType::Simnet, KaspaAddressPrefix::Simnet)),
        other => eyre::bail!("unsupported kaspa network: {other}"),
    }
}

fn parse_private_key_hex(hex_str: &str) -> eyre::Result<[u8; 32]> {
    let hex_str = hex_str.trim().strip_prefix("0x").unwrap_or(hex_str.trim());
    let bytes = hex::decode(hex_str)?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|v: Vec<u8>| eyre::eyre!("private key must be 32 bytes, got {}", v.len()))?;
    Ok(arr)
}

fn normalize_hex_prefix(prefix: &str) -> String {
    prefix.trim().trim_start_matches("0x").to_ascii_lowercase()
}

fn build_payload_with_nonce(header: u8, l2data: &[u8], nonce: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + l2data.len() + 4);
    payload.push(header);
    payload.extend_from_slice(l2data);
    payload.extend_from_slice(&nonce.to_be_bytes());
    payload
}

/// Convert RPC UTXO entries to consensus UTXO entries for signing.
fn rpc_utxos_to_entries(utxos: &[RpcUtxosByAddressesEntry]) -> Vec<KaspaUtxoEntry> {
    utxos
        .iter()
        .map(|entry| KaspaUtxoEntry {
            amount: entry.utxo_entry.amount,
            script_public_key: entry.utxo_entry.script_public_key.clone(),
            block_daa_score: entry.utxo_entry.block_daa_score,
            is_coinbase: entry.utxo_entry.is_coinbase,
        })
        .collect()
}

/// Build Kaspa TX inputs from RPC UTXO entries.
fn utxos_to_inputs(utxos: &[RpcUtxosByAddressesEntry]) -> Vec<KaspaTransactionInput> {
    utxos
        .iter()
        .map(|entry| KaspaTransactionInput::new(entry.outpoint.clone().into(), Vec::new(), 0, 1))
        .collect()
}

// ── Extracted helpers: UTXO selection, signing, waiting ──────────────────────

/// Select UTXOs (largest first) until the total covers `min_amount` plus fee.
/// Returns (selected UTXOs, total input value).
fn select_utxos(
    utxos: &[RpcUtxosByAddressesEntry],
    min_amount: u64,
    payload_len: usize,
) -> eyre::Result<(Vec<RpcUtxosByAddressesEntry>, u64)> {
    let mut sorted = utxos.to_vec();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.utxo_entry.amount));
    let mut selected = Vec::new();
    let mut total_input = 0u64;
    for entry in sorted {
        total_input = total_input.saturating_add(entry.utxo_entry.amount);
        selected.push(entry);
        let fee = estimated_fee_sompi(payload_len, selected.len());
        if total_input >= min_amount.saturating_add(fee).saturating_add(MIN_CHANGE_SOMPI) {
            break;
        }
    }
    let fee = estimated_fee_sompi(payload_len, selected.len());
    if total_input < min_amount.saturating_add(fee).saturating_add(MIN_CHANGE_SOMPI) {
        eyre::bail!(
            "insufficient UTXOs: have {} sompi, need {} + {} fee + {} min change",
            total_input,
            min_amount,
            fee,
            MIN_CHANGE_SOMPI,
        );
    }
    Ok((selected, total_input))
}

/// Sign a Kaspa TX, verify signatures, compute mass, validate against the
/// standard mass limit, and set the mass on the transaction.
fn sign_and_validate_kaspa_tx(
    tx: KaspaTransaction,
    utxo_entries: Vec<KaspaUtxoEntry>,
    private_key: &[u8; 32],
    network_type: KaspaNetworkType,
) -> eyre::Result<KaspaTransaction> {
    let signable = KaspaSignableTransaction::with_entries(tx, utxo_entries);
    let signed = kaspa_sign_with_multiple_v2(signable, std::slice::from_ref(private_key))
        .fully_signed()
        .map_err(|err| eyre::eyre!("failed to sign Kaspa tx: {err}"))?;
    kaspa_verify(&signed.as_verifiable())
        .map_err(|err| eyre::eyre!("invalid Kaspa signature: {err}"))?;

    let mass_calculator =
        KaspaMassCalculator::new_with_consensus_params(&KaspaParams::from(network_type));
    let non_contextual = mass_calculator.calc_non_contextual_masses(&signed.tx);
    let contextual = mass_calculator
        .calc_contextual_masses(&signed.as_verifiable())
        .ok_or_else(|| eyre::eyre!("failed to calculate Kaspa tx storage mass"))?;
    let mass = contextual.max(non_contextual);
    if mass > MAX_STANDARD_KASPA_TX_MASS {
        eyre::bail!("Kaspa transaction mass {mass} exceeds limit {MAX_STANDARD_KASPA_TX_MASS}");
    }

    let tx = signed.tx;
    tx.set_mass(mass);
    Ok(tx)
}

/// Fetch UTXOs with retry on transient gRPC errors.
async fn get_utxos_with_retry(
    client: &GrpcClient,
    addresses: Vec<KaspaAddress>,
    max_retries: usize,
) -> eyre::Result<Vec<RpcUtxosByAddressesEntry>> {
    let mut last_err = String::new();
    for attempt in 0..=max_retries {
        match client.get_utxos_by_addresses(addresses.clone()).await {
            Ok(utxos) => return Ok(utxos),
            Err(e) => {
                last_err = e.to_string();
                if attempt < max_retries {
                    let delay = Duration::from_secs(1 << attempt.min(3)); // 1s, 2s, 4s, 8s
                    eprintln!(
                        "    gRPC error (attempt {}/{}): {e}, retrying in {delay:?}...",
                        attempt + 1,
                        max_retries + 1,
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    eyre::bail!("failed to fetch UTXOs after {} attempts: {last_err}", max_retries + 1)
}

/// Submit a transaction with retry on transient gRPC errors.
async fn submit_tx_with_retry(
    client: &GrpcClient,
    rpc_tx: RpcTransaction,
    max_retries: usize,
) -> eyre::Result<()> {
    let mut last_err = String::new();
    for attempt in 0..=max_retries {
        match client.submit_transaction(rpc_tx.clone(), false).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = e.to_string();
                if attempt < max_retries {
                    let delay = Duration::from_secs(1 << attempt.min(3));
                    eprintln!(
                        "    gRPC submit error (attempt {}/{}): {e}, retrying in {delay:?}...",
                        attempt + 1,
                        max_retries + 1,
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    eyre::bail!("failed to submit TX after {} attempts: {last_err}", max_retries + 1)
}

/// Poll until a UTXO from a specific Kaspa TX ID appears for the given address.
async fn wait_for_utxo(
    client: &GrpcClient,
    address: &KaspaAddress,
    tx_id: &str,
    timeout: Duration,
) -> eyre::Result<()> {
    let start = Instant::now();
    loop {
        let utxos = get_utxos_with_retry(client, vec![address.clone()], 3).await?;
        if utxos
            .iter()
            .any(|u| u.outpoint.transaction_id.to_string() == tx_id)
        {
            return Ok(());
        }
        if start.elapsed() > timeout {
            eyre::bail!("timed out waiting for UTXO from tx {tx_id}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ── EVM TX construction (unified) ───────────────────────────────────────────

/// Build and sign an EVM TX. Returns (raw_tx_bytes, evm_tx_hash_hex).
fn build_signed_evm_tx(
    private_key: &[u8; 32],
    to: alloy_primitives::Address,
    chain_id: u64,
    nonce: u64,
    value: U256,
    gas_price: u128,
) -> eyre::Result<(Vec<u8>, String)> {
    let signer_hex = hex::encode(private_key);
    let signer: PrivateKeySigner = signer_hex
        .parse()
        .map_err(|e| eyre::eyre!("failed to parse EVM signer: {e}"))?;

    let mut tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: gas_price,
        max_priority_fee_per_gas: gas_price,
        to: TxKind::Call(to),
        value,
        input: Bytes::default(),
        access_list: Default::default(),
    };

    let sig = signer
        .sign_transaction_sync(&mut tx)
        .map_err(|e| eyre::eyre!("failed to sign EVM tx: {e}"))?;
    let signed = Signed::new_unhashed(tx, sig);
    let mut raw_tx = Vec::with_capacity(signed.eip2718_encoded_length());
    signed.eip2718_encode(&mut raw_tx);
    let evm_tx_hash = format!("0x{}", hex::encode(keccak256(&raw_tx)));
    Ok((raw_tx, evm_tx_hash))
}

// ── Kaspa TX with IGRA payload (prefix mining) ──────────────────────────────

/// Build and sign a Kaspa TX with IGRA payload, mining the TX ID prefix.
fn mine_and_build_signed_payload_transaction(
    private_key: &[u8; 32],
    source_address: &KaspaAddress,
    network_type: KaspaNetworkType,
    l2data: &[u8],
    tx_id_prefix: &[u8],
    timeout: Duration,
    utxos: &[RpcUtxosByAddressesEntry],
) -> eyre::Result<(u64, KaspaTransaction)> {
    if utxos.is_empty() {
        eyre::bail!("no UTXOs available for {source_address}");
    }
    if tx_id_prefix.is_empty() {
        eyre::bail!("tx_id_prefix cannot be empty");
    }

    let payload_header: u8 = (IGRA_VERSION << 4) | TX_TYPE_RAW_UNCOMPRESSED;
    let payload_len = 1usize + l2data.len() + 4;

    let (selected, total_input) = select_utxos(utxos, 0, payload_len)?;
    let fee = estimated_fee_sompi(payload_len, selected.len());
    let output_value = total_input.saturating_sub(fee);
    let script_public_key = pay_to_address_script(source_address);
    let inputs = utxos_to_inputs(&selected);
    let outputs = vec![KaspaTransactionOutput::new(output_value, script_public_key)];

    let payload = build_payload_with_nonce(payload_header, l2data, 0);
    let nonce_offset = payload.len() - 4;
    let mut tx = KaspaTransaction::new(
        0,
        inputs,
        outputs,
        0,
        SubnetworkId::default(),
        0,
        payload,
    );

    // Mine prefix
    let start = Instant::now();
    let mut nonce = 0u32;
    loop {
        if start.elapsed() > timeout {
            eyre::bail!(
                "timed out mining kaspa txid prefix after {}s (tried {} nonces)",
                timeout.as_secs(),
                nonce
            );
        }

        tx.payload[nonce_offset..].copy_from_slice(&nonce.to_be_bytes());
        tx.finalize();
        let tx_id = tx.id();
        if tx_id.as_bytes().starts_with(tx_id_prefix) {
            break;
        }

        nonce = nonce.wrapping_add(1);
        if nonce == 0 {
            if let Some(first) = tx.outputs.first_mut() {
                first.value = first.value.saturating_sub(1);
            }
            tx.finalize();
        }
    }

    // Sign and validate
    let entries = rpc_utxos_to_entries(&selected);
    let tx = sign_and_validate_kaspa_tx(tx, entries, private_key, network_type)?;

    if !tx.id().as_bytes().starts_with(tx_id_prefix) {
        eyre::bail!("mined Kaspa txid prefix changed after signing");
    }

    Ok((nonce as u64, tx))
}

// ── Sender keypair ───────────────────────────────────────────────────────────

struct SenderKeypair {
    private_key: [u8; 32],
    kaspa_address: KaspaAddress,
    evm_address: alloy_primitives::Address,
}

/// Serializable form of a sender keypair for persistence.
#[derive(Serialize, Deserialize)]
struct SenderKeypairJson {
    private_key_hex: String,
    kaspa_address: String,
    evm_address: String,
}

fn generate_sender_keypairs(
    n: usize,
    prefix: KaspaAddressPrefix,
) -> eyre::Result<Vec<SenderKeypair>> {
    let mut keypairs = Vec::with_capacity(n);
    for _ in 0..n {
        let signer = PrivateKeySigner::random();
        let evm_address = signer.address();
        let private_key: [u8; 32] = signer.credential().to_bytes().into();
        let kaspa_address = kaspa_address_from_private_key(&private_key, prefix)?;
        keypairs.push(SenderKeypair {
            private_key,
            kaspa_address,
            evm_address,
        });
    }
    Ok(keypairs)
}

fn save_senders(senders: &[SenderKeypair], path: &str) -> eyre::Result<()> {
    let json_senders: Vec<SenderKeypairJson> = senders
        .iter()
        .map(|s| SenderKeypairJson {
            private_key_hex: hex::encode(s.private_key),
            kaspa_address: s.kaspa_address.to_string(),
            evm_address: format!("{}", s.evm_address),
        })
        .collect();
    let json = serde_json::to_string_pretty(&json_senders)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn load_senders(path: &str, prefix: KaspaAddressPrefix) -> eyre::Result<Vec<SenderKeypair>> {
    let json = std::fs::read_to_string(path)?;
    let json_senders: Vec<SenderKeypairJson> = serde_json::from_str(&json)?;
    let mut keypairs = Vec::with_capacity(json_senders.len());
    for js in &json_senders {
        let private_key = parse_private_key_hex(&js.private_key_hex)?;
        let kaspa_address = kaspa_address_from_private_key(&private_key, prefix)?;
        let evm_address: alloy_primitives::Address = js
            .evm_address
            .parse()
            .map_err(|e| eyre::eyre!("invalid EVM address '{}': {e}", js.evm_address))?;
        keypairs.push(SenderKeypair {
            private_key,
            kaspa_address,
            evm_address,
        });
    }
    Ok(keypairs)
}

fn get_or_create_senders(
    n: usize,
    prefix: KaspaAddressPrefix,
    senders_file: Option<&str>,
) -> eyre::Result<Vec<SenderKeypair>> {
    if let Some(path) = senders_file {
        if std::path::Path::new(path).exists() {
            let loaded = load_senders(path, prefix)?;
            if loaded.len() == n {
                eprintln!("  loaded {n} sender keypairs from {path}");
                return Ok(loaded);
            }
            // Count mismatch — back up the old file to prevent key loss,
            // then generate fresh keys into a new file.
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let backup = format!("{path}.bak-{ts}");
            std::fs::rename(path, &backup)?;
            eprintln!(
                "  senders file has {} keypairs but need {n}, backed up to {backup}",
                loaded.len()
            );
        }
    }

    let senders = generate_sender_keypairs(n, prefix)?;

    if let Some(path) = senders_file {
        save_senders(&senders, path)?;
        eprintln!("  saved {n} sender keypairs to {path}");
    }

    Ok(senders)
}

// ── Worker keypairs (ephemeral, for parallel funding) ────────────────────────

struct WorkerKeypair {
    private_key: [u8; 32],
    kaspa_address: KaspaAddress,
}

fn generate_worker_keypairs(
    n: usize,
    prefix: KaspaAddressPrefix,
) -> eyre::Result<Vec<WorkerKeypair>> {
    let mut keypairs = Vec::with_capacity(n);
    for _ in 0..n {
        let signer = PrivateKeySigner::random();
        let private_key: [u8; 32] = signer.credential().to_bytes().into();
        let kaspa_address = kaspa_address_from_private_key(&private_key, prefix)?;
        keypairs.push(WorkerKeypair {
            private_key,
            kaspa_address,
        });
    }
    Ok(keypairs)
}

// ── UTXO consolidation ──────────────────────────────────────────────────────

/// Maximum inputs per consolidation TX to stay within mass limits.
/// With no payload and 1 output, ~84 inputs fit under 100K mass.
const CONSOLIDATION_MAX_INPUTS: usize = 80;

/// Consolidate master UTXOs into fewer, larger UTXOs.
/// Repeatedly merges batches of small UTXOs until the master has at most
/// `target_utxo_count` UTXOs. Each batch TX merges up to 80 inputs into 1 output.
async fn consolidate_master_utxos(
    client: &GrpcClient,
    master_private_key: &[u8; 32],
    master_address: &KaspaAddress,
    network_type: KaspaNetworkType,
    target_utxo_count: usize,
) -> eyre::Result<()> {
    loop {
        let utxos =
            get_utxos_with_retry(client, vec![master_address.clone()], 3).await?;

        if utxos.len() <= target_utxo_count {
            eprintln!(
                "  master has {} UTXOs (<= {target_utxo_count}), no consolidation needed",
                utxos.len()
            );
            return Ok(());
        }

        // Take up to CONSOLIDATION_MAX_INPUTS smallest UTXOs
        let mut sorted = utxos.to_vec();
        sorted.sort_by_key(|entry| entry.utxo_entry.amount);
        let batch: Vec<_> = sorted
            .into_iter()
            .take(CONSOLIDATION_MAX_INPUTS)
            .collect();
        let batch_total: u64 = batch.iter().map(|u| u.utxo_entry.amount).sum();
        let fee = estimated_fee_sompi(0, batch.len());
        if batch_total <= fee + MIN_CHANGE_SOMPI {
            eprintln!("  consolidation batch too small (dust), stopping");
            return Ok(());
        }

        let output_value = batch_total - fee;
        let outputs = vec![KaspaTransactionOutput::new(
            output_value,
            pay_to_address_script(master_address),
        )];
        let inputs = utxos_to_inputs(&batch);
        let mut tx = KaspaTransaction::new(
            0,
            inputs,
            outputs,
            0,
            SubnetworkId::default(),
            0,
            Vec::new(),
        );
        tx.finalize();

        let entries = rpc_utxos_to_entries(&batch);
        let tx = sign_and_validate_kaspa_tx(tx, entries, master_private_key, network_type)?;
        let tx_id = tx.id().to_string();
        let rpc_tx = RpcTransaction::from(&tx);
        submit_tx_with_retry(client, rpc_tx, 3).await?;

        eprintln!(
            "  consolidated {} UTXOs ({} sompi) into 1: {tx_id}",
            batch.len(),
            batch_total,
        );

        // Wait for the consolidated UTXO to appear
        wait_for_utxo(client, master_address, &tx_id, Duration::from_secs(30)).await?;
    }
}

// ── Parallel funding: split master → workers ─────────────────────────────────

/// Split master balance into worker outputs, batching across multiple TXs if
/// needed to stay within Kaspa's storage mass limit.
///
/// Storage mass ≈ C × sum(1/output_i). For N equal outputs of `amount`:
/// mass ≈ C × N / amount. With C=10^12 and limit=100K, max outputs per TX ≈
/// amount / 10^7. We use a conservative estimate and batch accordingly.
async fn split_to_workers(
    client: &GrpcClient,
    master_private_key: &[u8; 32],
    master_address: &KaspaAddress,
    workers: &[WorkerKeypair],
    amount_per_worker: u64,
    network_type: KaspaNetworkType,
) -> eyre::Result<String> {
    let n = workers.len();
    if n == 0 {
        eyre::bail!("split_to_workers: no workers");
    }
    if amount_per_worker < MIN_SENDER_FUNDING_SOMPI {
        eyre::bail!(
            "amount_per_worker {} < MIN_SENDER_FUNDING_SOMPI {}",
            amount_per_worker,
            MIN_SENDER_FUNDING_SOMPI,
        );
    }

    // Max worker outputs per TX: C/amount × max_outputs < 100K mass.
    // Leave headroom for change output and non-contextual mass.
    // mass_per_output = C / amount, so max_outputs = 80K / mass_per_output
    let mass_per_output = 1_000_000_000_000u64 / amount_per_worker;
    let max_per_tx = if mass_per_output == 0 {
        n // amount is so large that mass is negligible
    } else {
        // Use 80K budget (leaving 20K for change + non-contextual mass)
        (80_000u64 / mass_per_output) as usize
    }
    .max(1); // at least 1 per TX

    let batches: Vec<&[WorkerKeypair]> = workers.chunks(max_per_tx).collect();
    eprintln!(
        "  split_to_workers: {} workers in {} batch(es) (max {} per TX, mass/output={})",
        n,
        batches.len(),
        max_per_tx,
        mass_per_output,
    );

    let mut last_tx_id = String::new();
    for (batch_idx, batch) in batches.iter().enumerate() {
        let batch_total = amount_per_worker * (batch.len() as u64);

        let utxos =
            get_utxos_with_retry(client, vec![master_address.clone()], 3).await?;
        let (selected, total_input) = select_utxos(&utxos, batch_total, 0)?;
        let fee = estimated_fee_sompi(0, selected.len());
        let change = total_input
            .checked_sub(batch_total + fee)
            .ok_or_else(|| {
                eyre::eyre!(
                    "insufficient master balance for split batch {}: have {}, need {} + {} fee",
                    batch_idx,
                    total_input,
                    batch_total,
                    fee,
                )
            })?;

        let mut outputs: Vec<KaspaTransactionOutput> = batch
            .iter()
            .map(|w| {
                KaspaTransactionOutput::new(
                    amount_per_worker,
                    pay_to_address_script(&w.kaspa_address),
                )
            })
            .collect();
        if change > MIN_CHANGE_SOMPI {
            outputs.push(KaspaTransactionOutput::new(
                change,
                pay_to_address_script(master_address),
            ));
        }

        let inputs = utxos_to_inputs(&selected);
        let mut tx = KaspaTransaction::new(
            0,
            inputs,
            outputs,
            0,
            SubnetworkId::default(),
            0,
            Vec::new(),
        );
        tx.finalize();

        let entries = rpc_utxos_to_entries(&selected);
        let tx = sign_and_validate_kaspa_tx(tx, entries, master_private_key, network_type)?;
        let tx_id = tx.id().to_string();
        let rpc_tx = RpcTransaction::from(&tx);
        submit_tx_with_retry(client, rpc_tx, 3).await?;

        if batches.len() > 1 {
            eprintln!(
                "  split batch {}/{}: {} workers, tx={}",
                batch_idx + 1,
                batches.len(),
                batch.len(),
                tx_id,
            );
        }

        // Wait for a worker UTXO from this batch (and implicitly the change UTXO)
        wait_for_utxo(
            client,
            &batch[0].kaspa_address,
            &tx_id,
            Duration::from_secs(30),
        )
        .await?;

        last_tx_id = tx_id;
    }

    Ok(last_tx_id)
}

/// Sweep all worker remaining balances back to master in parallel.
async fn sweep_workers_to_master(
    client: &GrpcClient,
    workers: &[WorkerKeypair],
    master_address: &KaspaAddress,
    network_type: KaspaNetworkType,
) -> eyre::Result<u64> {
    let mut handles = Vec::with_capacity(workers.len());
    for (i, worker) in workers.iter().enumerate() {
        let client = client.clone();
        let master_address = master_address.clone();
        let worker_private_key = worker.private_key;
        let worker_address = worker.kaspa_address.clone();
        handles.push(tokio::spawn(async move {
            let utxos =
                get_utxos_with_retry(&client, vec![worker_address.clone()], 3).await?;
            if utxos.is_empty() {
                return Ok(0u64);
            }
            let total_input: u64 = utxos.iter().map(|u| u.utxo_entry.amount).sum();
            let fee = estimated_fee_sompi(0, utxos.len());
            if total_input <= fee + MIN_CHANGE_SOMPI {
                return Ok(0u64);
            }
            let sweep_amount = total_input - fee;
            let outputs = vec![KaspaTransactionOutput::new(
                sweep_amount,
                pay_to_address_script(&master_address),
            )];
            let inputs = utxos_to_inputs(&utxos);
            let mut tx = KaspaTransaction::new(
                0,
                inputs,
                outputs,
                0,
                SubnetworkId::default(),
                0,
                Vec::new(),
            );
            tx.finalize();
            let entries = rpc_utxos_to_entries(&utxos);
            let tx =
                sign_and_validate_kaspa_tx(tx, entries, &worker_private_key, network_type)?;
            let rpc_tx = RpcTransaction::from(&tx);
            submit_tx_with_retry(&client, rpc_tx, 3).await?;
            eprintln!("  worker[{i}] swept {sweep_amount} sompi");
            Ok::<u64, eyre::Report>(sweep_amount)
        }));
    }

    let mut total = 0u64;
    for handle in handles {
        match handle.await {
            Ok(Ok(amount)) => total += amount,
            Ok(Err(e)) => eprintln!("  worker sweep error: {e}"),
            Err(e) => eprintln!("  worker sweep task error: {e}"),
        }
    }
    Ok(total)
}

// ── Funding: Kaspa L1 ───────────────────────────────────────────────────────

/// Send one plain funding TX from master to a single sender (2 outputs: sender + change).
/// Returns the Kaspa TX ID.
async fn fund_one_sender(
    client: &GrpcClient,
    master_private_key: &[u8; 32],
    master_address: &KaspaAddress,
    sender_address: &KaspaAddress,
    amount: u64,
    network_type: KaspaNetworkType,
) -> eyre::Result<String> {
    let utxos =
        get_utxos_with_retry(client, vec![master_address.clone()], 3).await?;

    if utxos.is_empty() {
        eyre::bail!("master wallet has no UTXOs ({master_address})");
    }

    // Select UTXOs with storage-mass-aware accumulation
    let mut sorted = utxos.to_vec();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.utxo_entry.amount));
    let mut selected = Vec::new();
    let mut total_input = 0u64;
    for entry in sorted {
        total_input += entry.utxo_entry.amount;
        selected.push(entry);
        let fee = estimated_fee_sompi(0, selected.len());
        let max_safe = (total_input.saturating_sub(fee)) / MAX_SENDER_FRACTION;
        if max_safe >= amount.min(MIN_SENDER_FUNDING_SOMPI)
            && total_input >= amount + fee + MIN_CHANGE_SOMPI
        {
            break;
        }
    }

    let fee = estimated_fee_sompi(0, selected.len());
    let max_safe_amount = (total_input.saturating_sub(fee)) / MAX_SENDER_FRACTION;
    let actual_amount = amount.min(max_safe_amount);
    if actual_amount < MIN_SENDER_FUNDING_SOMPI {
        eyre::bail!(
            "master UTXOs too small for safe split: have {} sompi across {} UTXOs, max safe amount {} < minimum {}",
            total_input,
            selected.len(),
            max_safe_amount,
            MIN_SENDER_FUNDING_SOMPI,
        );
    }
    if total_input < actual_amount + fee + MIN_CHANGE_SOMPI {
        eyre::bail!(
            "master UTXOs too small: have {} sompi, need {} + {} fee",
            total_input,
            actual_amount,
            fee,
        );
    }

    let change = total_input - actual_amount - fee;
    let outputs = vec![
        KaspaTransactionOutput::new(actual_amount, pay_to_address_script(sender_address)),
        KaspaTransactionOutput::new(change, pay_to_address_script(master_address)),
    ];

    let inputs = utxos_to_inputs(&selected);
    let mut tx = KaspaTransaction::new(
        0,
        inputs,
        outputs,
        0,
        SubnetworkId::default(),
        0,
        Vec::new(),
    );
    tx.finalize();

    let entries = rpc_utxos_to_entries(&selected);
    let tx = sign_and_validate_kaspa_tx(tx, entries, master_private_key, network_type)?;

    let tx_id_hash = tx.id();
    let rpc_tx = RpcTransaction::from(&tx);
    submit_tx_with_retry(client, rpc_tx, 3).await?;

    Ok(tx_id_hash.to_string())
}

/// Fund all senders one at a time, waiting for master change UTXO between each.
///
/// Each sender receives a fixed amount (`MIN_SENDER_FUNDING_SOMPI`) — just enough
/// to cover storage mass limits and IGRA TX fees. This keeps the master balance
/// requirement low (~12 KAS for 1000 senders instead of ~104 KAS).
async fn fund_senders(
    client: &GrpcClient,
    master_private_key: &[u8; 32],
    master_address: &KaspaAddress,
    senders: &[SenderKeypair],
    network_type: KaspaNetworkType,
    extra_master_reserve: u64,
    txs_per_sender: usize,
) -> eyre::Result<()> {
    let per_sender_fee_reserve = estimated_fee_sompi(0, 1);

    // Each sender needs: MIN_SENDER_FUNDING_SOMPI (storage mass floor)
    // + fees for all rounds (each IGRA TX burns ~220K sompi).
    let per_round_fee = estimated_fee_sompi(120, 1); // ~220K for payload TX
    let amount_per_sender = MIN_SENDER_FUNDING_SOMPI
        + (txs_per_sender as u64) * per_round_fee;

    // Check which senders already have sufficient L1 balance (for resume)
    eprintln!("  checking existing sender L1 balances...");
    let mut already_funded_l1 = 0usize;
    for sender in senders.iter() {
        let utxos = get_utxos_with_retry(client, vec![sender.kaspa_address.clone()], 3).await?;
        let bal: u64 = utxos.iter().map(|u| u.utxo_entry.amount).sum();
        if bal >= MIN_SENDER_FUNDING_SOMPI {
            already_funded_l1 += 1;
        }
    }
    let unfunded = senders.len() - already_funded_l1;
    if already_funded_l1 > 0 {
        eprintln!("  {already_funded_l1}/{} senders already have L1 balance, {unfunded} need funding", senders.len());
    }

    if unfunded > 0 {
        let utxos =
            get_utxos_with_retry(client, vec![master_address.clone()], 3).await?;
        let total_balance: u64 = utxos.iter().map(|u| u.utxo_entry.amount).sum();
        let num = unfunded as u64;
        let total_needed = num * amount_per_sender + num * per_sender_fee_reserve + extra_master_reserve;
        if total_balance < total_needed {
            eyre::bail!(
                "insufficient master balance for {} unfunded senders × {} rounds: have {} sompi ({:.4} KAS), need {} sompi ({:.4} KAS)",
                unfunded,
                txs_per_sender,
                total_balance,
                total_balance as f64 / 100_000_000.0,
                total_needed,
                total_needed as f64 / 100_000_000.0,
            );
        }
    } else {
        eprintln!("  all senders already funded on L1, skipping.");
    }

    eprintln!(
        "  distributing {} sompi ({:.3} KAS) per sender",
        amount_per_sender,
        amount_per_sender as f64 / 100_000_000.0
    );

    for (i, sender) in senders.iter().enumerate() {
        // Skip if already funded
        let utxos_check = get_utxos_with_retry(client, vec![sender.kaspa_address.clone()], 3).await?;
        let existing_bal: u64 = utxos_check.iter().map(|u| u.utxo_entry.amount).sum();
        if existing_bal >= MIN_SENDER_FUNDING_SOMPI {
            continue;
        }

        eprintln!("  funding sender[{i}] ({})...", sender.kaspa_address);

        let tx_id = fund_one_sender(
            client,
            master_private_key,
            master_address,
            &sender.kaspa_address,
            amount_per_sender,
            network_type,
        )
        .await?;

        eprintln!("  sender[{i}] funded: {tx_id}");

        // Wait for change UTXO before funding next sender
        if i + 1 < senders.len() {
            wait_for_utxo(client, master_address, &tx_id, Duration::from_secs(30)).await?;
        }
    }
    Ok(())
}

/// Poll UTXOs until all senders have at least one UTXO.
async fn wait_for_funding(
    client: &GrpcClient,
    senders: &[SenderKeypair],
    timeout: Duration,
) -> eyre::Result<()> {
    let start = Instant::now();
    loop {
        let addresses: Vec<KaspaAddress> =
            senders.iter().map(|s| s.kaspa_address.clone()).collect();
        let utxos =
            get_utxos_with_retry(client, addresses, 3).await?;

        let funded_count = senders
            .iter()
            .filter(|s| {
                utxos
                    .iter()
                    .any(|u| u.address.as_ref() == Some(&s.kaspa_address))
            })
            .count();

        if funded_count == senders.len() {
            return Ok(());
        }

        if start.elapsed() > timeout {
            eyre::bail!(
                "timed out waiting for sender funding: {funded_count}/{} funded",
                senders.len()
            );
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ── Parallel L1 funding ──────────────────────────────────────────────────────

/// Fund senders in parallel using ephemeral worker wallets.
///
/// 1. Check which senders already have L1 balance (resume-safe)
/// 2. Generate N worker keypairs, split master balance to them in one TX
/// 3. Each worker funds its chunk of senders sequentially using its own UTXO chain
/// 4. Sweep worker remainders back to master
async fn fund_senders_parallel(
    client: &GrpcClient,
    master_private_key: &[u8; 32],
    master_address: &KaspaAddress,
    senders: &[SenderKeypair],
    network_type: KaspaNetworkType,
    address_prefix: KaspaAddressPrefix,
    num_workers: usize,
    txs_per_sender: usize,
) -> eyre::Result<()> {
    // Per-sender amount (same as sequential)
    let per_round_fee = estimated_fee_sompi(120, 1);
    let amount_per_sender =
        MIN_SENDER_FUNDING_SOMPI + (txs_per_sender as u64) * per_round_fee;

    // Check which senders already have L1 balance
    eprintln!("  checking existing sender L1 balances...");
    let mut unfunded_indices = Vec::new();
    for (i, sender) in senders.iter().enumerate() {
        let utxos =
            get_utxos_with_retry(client, vec![sender.kaspa_address.clone()], 3).await?;
        let bal: u64 = utxos.iter().map(|u| u.utxo_entry.amount).sum();
        if bal < MIN_SENDER_FUNDING_SOMPI {
            unfunded_indices.push(i);
        }
    }
    let already_funded = senders.len() - unfunded_indices.len();
    if already_funded > 0 {
        eprintln!(
            "  {already_funded}/{} senders already have L1 balance, {} need funding",
            senders.len(),
            unfunded_indices.len()
        );
    }
    if unfunded_indices.is_empty() {
        eprintln!("  all senders already funded on L1, skipping.");
        return Ok(());
    }

    // Adjust worker count
    let actual_workers = num_workers.min(unfunded_indices.len());
    eprintln!(
        "  parallel L1 funding: {} unfunded senders, {} workers",
        unfunded_indices.len(),
        actual_workers,
    );

    // Calculate per-worker needs: each worker funds ceil(unfunded/workers) senders.
    //
    // Storage mass constraint: for a 2-output split (sender + change), the worker
    // UTXO must be >= MAX_SENDER_FRACTION (3) × amount_per_sender so both outputs
    // stay within limits (same logic as fund_one_sender).
    //
    // After funding k senders, worker balance = W - k*(amount+fee).
    // For the last sender (k = chunk-1): need W - (chunk-1)*(amount+fee) >= 3*amount.
    // => W >= (chunk + 2) * amount + (chunk - 1) * fee.
    let chunk_size = unfunded_indices.len().div_ceil(actual_workers);
    let per_funding_tx_fee = estimated_fee_sompi(0, 1);
    let per_worker_amount =
        (chunk_size as u64 + 2) * amount_per_sender
            + (chunk_size as u64) * per_funding_tx_fee;

    // Check master balance
    let utxos = get_utxos_with_retry(client, vec![master_address.clone()], 3).await?;
    let master_balance: u64 = utxos.iter().map(|u| u.utxo_entry.amount).sum();
    let total_needed = per_worker_amount * (actual_workers as u64);
    if master_balance < total_needed + estimated_fee_sompi(0, utxos.len()) {
        eyre::bail!(
            "insufficient master balance for parallel funding: have {} sompi ({:.4} KAS), \
             need {} sompi ({:.4} KAS) for {} workers × {} senders",
            master_balance,
            master_balance as f64 / 100_000_000.0,
            total_needed,
            total_needed as f64 / 100_000_000.0,
            actual_workers,
            unfunded_indices.len(),
        );
    }

    // Generate workers and split
    eprintln!(
        "  generating {} worker keypairs and splitting {} sompi ({:.4} KAS) per worker...",
        actual_workers,
        per_worker_amount,
        per_worker_amount as f64 / 100_000_000.0,
    );
    let workers = generate_worker_keypairs(actual_workers, address_prefix)?;
    let split_tx_id = split_to_workers(
        client,
        master_private_key,
        master_address,
        &workers,
        per_worker_amount,
        network_type,
    )
    .await?;
    eprintln!("  split TX confirmed: {split_tx_id}");

    // Chunk unfunded senders across workers
    let chunks: Vec<Vec<usize>> = unfunded_indices
        .chunks(chunk_size)
        .map(|c| c.to_vec())
        .collect();

    // Spawn worker tasks
    let cancel = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(actual_workers);

    for (worker_idx, chunk) in chunks.into_iter().enumerate() {
        let client = client.clone();
        let cancel = cancel.clone();
        let worker_private_key = workers[worker_idx].private_key;
        let worker_address = workers[worker_idx].kaspa_address.clone();
        // Collect sender addresses for this chunk
        let chunk_senders: Vec<(usize, KaspaAddress)> = chunk
            .iter()
            .map(|&i| (i, senders[i].kaspa_address.clone()))
            .collect();

        handles.push(tokio::spawn(async move {
            let mut funded = 0usize;
            // Track expected UTXO TX ID to avoid stale UTXOs
            let mut expected_utxo_tx_id: Option<String> = None;
            for (sender_idx, sender_address) in &chunk_senders {
                if cancel.load(Ordering::Relaxed) {
                    eprintln!("  worker[{worker_idx}] cancelled after {funded}/{} senders", chunk_senders.len());
                    break;
                }

                let all_utxos = get_utxos_with_retry(
                    &client,
                    vec![worker_address.clone()],
                    3,
                )
                .await?;
                // Filter to only the expected UTXO (avoids stale spent UTXOs)
                let utxos: Vec<_> = if let Some(ref expected_tx) = expected_utxo_tx_id {
                    all_utxos
                        .into_iter()
                        .filter(|u| u.outpoint.transaction_id.to_string() == *expected_tx)
                        .collect()
                } else {
                    all_utxos
                };
                if utxos.is_empty() {
                    eyre::bail!(
                        "worker[{worker_idx}] has no UTXOs after funding {funded} senders"
                    );
                }
                let total_input: u64 = utxos.iter().map(|u| u.utxo_entry.amount).sum();
                let fee = estimated_fee_sompi(0, utxos.len());
                // Cap sender output to keep storage mass safe (same as fund_one_sender)
                let max_safe = (total_input.saturating_sub(fee)) / MAX_SENDER_FRACTION;
                let actual_amount = amount_per_sender.min(max_safe);
                if actual_amount < MIN_SENDER_FUNDING_SOMPI {
                    eyre::bail!(
                        "worker[{worker_idx}] UTXO too small for safe split: \
                         balance={} max_safe={} < MIN={}",
                        total_input,
                        max_safe,
                        MIN_SENDER_FUNDING_SOMPI,
                    );
                }
                if total_input < actual_amount + fee + MIN_CHANGE_SOMPI {
                    eyre::bail!(
                        "worker[{worker_idx}] insufficient balance: {} < {} + {} + {}",
                        total_input,
                        actual_amount,
                        fee,
                        MIN_CHANGE_SOMPI,
                    );
                }
                let change = total_input - actual_amount - fee;

                let mut outputs = vec![KaspaTransactionOutput::new(
                    actual_amount,
                    pay_to_address_script(sender_address),
                )];
                if change > MIN_CHANGE_SOMPI {
                    outputs.push(KaspaTransactionOutput::new(
                        change,
                        pay_to_address_script(&worker_address),
                    ));
                }

                let inputs = utxos_to_inputs(&utxos);
                let mut tx = KaspaTransaction::new(
                    0,
                    inputs,
                    outputs,
                    0,
                    SubnetworkId::default(),
                    0,
                    Vec::new(),
                );
                tx.finalize();

                let entries = rpc_utxos_to_entries(&utxos);
                let tx = sign_and_validate_kaspa_tx(
                    tx,
                    entries,
                    &worker_private_key,
                    network_type,
                )?;
                let tx_id = tx.id().to_string();
                let rpc_tx = RpcTransaction::from(&tx);
                submit_tx_with_retry(&client, rpc_tx, 3).await?;

                funded += 1;
                if funded % 50 == 0 || funded == chunk_senders.len() {
                    eprintln!(
                        "  worker[{worker_idx}] funded {funded}/{} (sender[{sender_idx}])",
                        chunk_senders.len()
                    );
                }

                // Wait for change UTXO before next sender
                if funded < chunk_senders.len() {
                    wait_for_utxo(
                        &client,
                        &worker_address,
                        &tx_id,
                        Duration::from_secs(30),
                    )
                    .await?;
                }
                expected_utxo_tx_id = Some(tx_id);
            }
            Ok::<usize, eyre::Report>(funded)
        }));
    }

    // Collect results
    let mut total_funded = 0usize;
    let mut had_error = false;
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.await {
            Ok(Ok(count)) => {
                total_funded += count;
            }
            Ok(Err(e)) => {
                eprintln!("  worker[{i}] error: {e}");
                cancel.store(true, Ordering::Relaxed);
                had_error = true;
            }
            Err(e) => {
                eprintln!("  worker[{i}] task panic: {e}");
                cancel.store(true, Ordering::Relaxed);
                had_error = true;
            }
        }
    }

    eprintln!(
        "  parallel L1 funding complete: {total_funded}/{} senders funded",
        unfunded_indices.len()
    );

    // Wait for last worker TXs to propagate before sweeping
    eprintln!("  waiting for worker TXs to confirm before sweep...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Sweep worker remainders
    eprintln!("  sweeping worker balances back to master...");
    let swept = sweep_workers_to_master(client, &workers, master_address, network_type).await?;
    eprintln!(
        "  swept {} sompi ({:.4} KAS) from workers",
        swept,
        swept as f64 / 100_000_000.0,
    );

    if had_error {
        eyre::bail!("parallel L1 funding had errors (funded {total_funded}/{} senders). Re-run to resume.", unfunded_indices.len());
    }

    Ok(())
}

// ── Funding: EVM side via IGRA TXs ──────────────────────────────────────────

/// Fund each sender with iKAS on the EVM side by sending IGRA transfer TXs
/// from master. Each sender gets enough gas for their benchmark rounds.
async fn fund_senders_evm(
    client: &GrpcClient,
    master_private_key: &[u8; 32],
    master_address: &KaspaAddress,
    senders: &[SenderKeypair],
    chain_id: u64,
    txs_per_sender: usize,
    prefix_bytes: &[u8],
    mining_timeout: Duration,
    network_type: KaspaNetworkType,
    el_rpc_url: &str,
    gas_price: u128,
) -> eyre::Result<()> {
    let gas_per_tx = U256::from(21_000u64) * U256::from(gas_price);
    let ether_per_sender = gas_per_tx * U256::from(txs_per_sender + 1); // +1 buffer
    eprintln!(
        "[2b/5] Funding {} senders with iKAS on EVM ({} wei each)...",
        senders.len(),
        ether_per_sender
    );

    let master_evm_address = {
        let signer_hex = hex::encode(master_private_key);
        let signer: PrivateKeySigner = signer_hex
            .parse()
            .map_err(|e| eyre::eyre!("failed to parse master EVM signer: {e}"))?;
        signer.address()
    };
    eprintln!("  master EVM address: {master_evm_address}");

    let provider = ProviderBuilder::new().connect_http(el_rpc_url.parse()?);
    let master_balance = provider.get_balance(master_evm_address).await?;
    eprintln!("  master EVM balance: {master_balance} wei");

    let master_evm_nonce = provider.get_transaction_count(master_evm_address).await?;
    eprintln!("  master EVM nonce: {master_evm_nonce}");

    // Check which senders already have EVM balance (for resume after partial funding)
    let mut already_funded = 0usize;
    for sender in senders.iter() {
        let bal = provider.get_balance(sender.evm_address).await?;
        if !bal.is_zero() {
            already_funded += 1;
        }
    }
    if already_funded > 0 {
        eprintln!("  {already_funded}/{} senders already have EVM balance, skipping them", senders.len());
    }

    let mut nonce_offset = 0u64;
    for (i, sender) in senders.iter().enumerate() {
        // Skip senders that already have EVM balance
        let bal = provider.get_balance(sender.evm_address).await?;
        if !bal.is_zero() {
            continue;
        }
        let evm_nonce = master_evm_nonce + nonce_offset;
        let (raw_tx, evm_hash) = build_signed_evm_tx(
            master_private_key,
            sender.evm_address,
            chain_id,
            evm_nonce,
            ether_per_sender,
            gas_price,
        )?;

        let utxos =
            get_utxos_with_retry(client, vec![master_address.clone()], 3).await?;

        let (_, kaspa_tx) = mine_and_build_signed_payload_transaction(
            master_private_key,
            master_address,
            network_type,
            &raw_tx,
            prefix_bytes,
            mining_timeout,
            &utxos,
        )?;

        let kaspa_tx_id = kaspa_tx.id().to_string();
        let rpc_tx = RpcTransaction::from(&kaspa_tx);
        submit_tx_with_retry(client, rpc_tx, 3).await?;

        eprintln!("  sender[{i}] EVM funded: kaspa={kaspa_tx_id} evm={evm_hash}");
        nonce_offset += 1;

        if i + 1 < senders.len() {
            wait_for_utxo(client, master_address, &kaspa_tx_id, Duration::from_secs(30)).await?;
        }
    }
    eprintln!("  all senders EVM-funded.");
    Ok(())
}

// ── Parallel EVM funding ─────────────────────────────────────────────────────

/// Fund senders with iKAS on the EVM side in parallel using worker wallets.
///
/// Key insight: EVM nonces must be sequential on the master's EVM address, but the
/// Kaspa signer (worker key) is independent from the EVM signer (master key).
/// Any Kaspa address can carry any EVM TX as IGRA payload.
///
/// Strategy:
/// 1. Pre-compute ALL EVM TXs upfront (signed by master, sequential nonces)
/// 2. Split master Kaspa balance to workers
/// 3. Each worker: take pre-built EVM TX → mine Kaspa TX wrapping it → submit
/// 4. Cancel-on-first-failure to prevent nonce gaps
async fn fund_senders_evm_parallel(
    client: &GrpcClient,
    master_private_key: &[u8; 32],
    master_address: &KaspaAddress,
    senders: &[SenderKeypair],
    chain_id: u64,
    txs_per_sender: usize,
    prefix_bytes: &[u8],
    mining_timeout: Duration,
    network_type: KaspaNetworkType,
    address_prefix: KaspaAddressPrefix,
    el_rpc_url: &str,
    gas_price: u128,
    num_workers: usize,
) -> eyre::Result<()> {
    let gas_per_tx = U256::from(21_000u64) * U256::from(gas_price);
    let ether_per_sender = gas_per_tx * U256::from(txs_per_sender + 1);
    eprintln!(
        "[2b/5] Parallel EVM funding: {} senders with {} wei each, {} workers...",
        senders.len(),
        ether_per_sender,
        num_workers,
    );

    let master_evm_address = {
        let signer_hex = hex::encode(master_private_key);
        let signer: PrivateKeySigner = signer_hex
            .parse()
            .map_err(|e| eyre::eyre!("failed to parse master EVM signer: {e}"))?;
        signer.address()
    };
    eprintln!("  master EVM address: {master_evm_address}");

    let provider = ProviderBuilder::new().connect_http(el_rpc_url.parse()?);
    let master_evm_nonce = provider.get_transaction_count(master_evm_address).await?;
    eprintln!("  master EVM nonce: {master_evm_nonce}");

    // Check which senders already have EVM balance
    let mut unfunded_indices = Vec::new();
    for (i, sender) in senders.iter().enumerate() {
        let bal = provider.get_balance(sender.evm_address).await?;
        if bal.is_zero() {
            unfunded_indices.push(i);
        }
    }
    let already_funded = senders.len() - unfunded_indices.len();
    if already_funded > 0 {
        eprintln!(
            "  {already_funded}/{} senders already have EVM balance, {} need funding",
            senders.len(),
            unfunded_indices.len(),
        );
    }
    if unfunded_indices.is_empty() {
        eprintln!("  all senders already EVM-funded, skipping.");
        return Ok(());
    }

    // Pre-build ALL EVM TXs (signed by master, sequential nonces)
    eprintln!("  pre-building {} EVM TXs...", unfunded_indices.len());
    let mut evm_txs: Vec<(usize, Vec<u8>, String)> = Vec::with_capacity(unfunded_indices.len());
    for (offset, &sender_idx) in unfunded_indices.iter().enumerate() {
        let evm_nonce = master_evm_nonce + (offset as u64);
        let (raw_tx, evm_hash) = build_signed_evm_tx(
            master_private_key,
            senders[sender_idx].evm_address,
            chain_id,
            evm_nonce,
            ether_per_sender,
            gas_price,
        )?;
        evm_txs.push((sender_idx, raw_tx, evm_hash));
    }
    eprintln!("  pre-built {} EVM TXs (nonces {}..{})", evm_txs.len(), master_evm_nonce, master_evm_nonce + evm_txs.len() as u64 - 1);

    // Adjust worker count and chunk
    let actual_workers = num_workers.min(evm_txs.len());
    let chunk_size = evm_txs.len().div_ceil(actual_workers);

    // Each worker needs enough KAS for IGRA fees only (one per EVM TX)
    let igra_fee_per_tx = estimated_fee_sompi(120, 1); // ~220K sompi
    let per_worker_amount =
        (igra_fee_per_tx + MIN_CHANGE_SOMPI) * (chunk_size as u64) + MIN_SENDER_FUNDING_SOMPI;

    // Generate workers, split master balance
    eprintln!(
        "  splitting master balance to {} workers ({} sompi each)...",
        actual_workers, per_worker_amount,
    );
    let workers = generate_worker_keypairs(actual_workers, address_prefix)?;
    let split_tx_id = split_to_workers(
        client,
        master_private_key,
        master_address,
        &workers,
        per_worker_amount,
        network_type,
    )
    .await?;
    eprintln!("  split TX confirmed: {split_tx_id}");

    // Chunk pre-built EVM TXs across workers (contiguous nonce ranges)
    let chunks: Vec<Vec<(usize, Vec<u8>, String)>> = evm_txs
        .into_iter()
        .collect::<Vec<_>>()
        .chunks(chunk_size)
        .map(|c| c.to_vec())
        .collect();

    // Spawn worker tasks
    let cancel = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(actual_workers);

    for (worker_idx, chunk) in chunks.into_iter().enumerate() {
        let client = client.clone();
        let cancel = cancel.clone();
        let worker_private_key = workers[worker_idx].private_key;
        let worker_address = workers[worker_idx].kaspa_address.clone();
        let prefix_bytes = prefix_bytes.to_vec();

        handles.push(tokio::spawn(async move {
            let mut submitted = 0usize;
            // Track the expected UTXO TX ID so we only use the correct (unspent) UTXO
            let mut expected_utxo_tx_id: Option<String> = None;
            for (sender_idx, raw_evm_tx, evm_hash) in &chunk {
                if cancel.load(Ordering::Relaxed) {
                    eprintln!(
                        "  evm-worker[{worker_idx}] cancelled after {submitted}/{} TXs",
                        chunk.len()
                    );
                    break;
                }

                let all_utxos = get_utxos_with_retry(
                    &client,
                    vec![worker_address.clone()],
                    3,
                )
                .await?;

                // Filter to only the expected UTXO (avoids stale spent UTXOs)
                let utxos: Vec<_> = if let Some(ref expected_tx) = expected_utxo_tx_id {
                    all_utxos
                        .into_iter()
                        .filter(|u| u.outpoint.transaction_id.to_string() == *expected_tx)
                        .collect()
                } else {
                    all_utxos
                };

                let (_, kaspa_tx) = mine_and_build_signed_payload_transaction(
                    &worker_private_key,
                    &worker_address,
                    network_type,
                    raw_evm_tx,
                    &prefix_bytes,
                    mining_timeout,
                    &utxos,
                )?;

                let kaspa_tx_id = kaspa_tx.id().to_string();
                let rpc_tx = RpcTransaction::from(&kaspa_tx);
                submit_tx_with_retry(&client, rpc_tx, 3).await?;

                submitted += 1;
                if submitted % 50 == 0 || submitted == chunk.len() {
                    eprintln!(
                        "  evm-worker[{worker_idx}] submitted {submitted}/{} (sender[{sender_idx}] evm={evm_hash})",
                        chunk.len()
                    );
                }

                // Wait for change UTXO before next TX
                if submitted < chunk.len() {
                    wait_for_utxo(
                        &client,
                        &worker_address,
                        &kaspa_tx_id,
                        Duration::from_secs(30),
                    )
                    .await?;
                }
                expected_utxo_tx_id = Some(kaspa_tx_id);
            }
            Ok::<usize, eyre::Report>(submitted)
        }));
    }

    // Collect results
    let mut total_submitted = 0usize;
    let mut had_error = false;
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.await {
            Ok(Ok(count)) => total_submitted += count,
            Ok(Err(e)) => {
                eprintln!("  evm-worker[{i}] error: {e}");
                cancel.store(true, Ordering::Relaxed);
                had_error = true;
            }
            Err(e) => {
                eprintln!("  evm-worker[{i}] task panic: {e}");
                cancel.store(true, Ordering::Relaxed);
                had_error = true;
            }
        }
    }

    eprintln!(
        "  parallel EVM funding complete: {total_submitted}/{} TXs submitted",
        unfunded_indices.len(),
    );

    // Wait for last worker TXs to propagate before sweeping
    eprintln!("  waiting for worker TXs to confirm before sweep...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Sweep worker remainders
    eprintln!("  sweeping evm-worker balances back to master...");
    let swept = sweep_workers_to_master(client, &workers, master_address, network_type).await?;
    eprintln!(
        "  swept {} sompi ({:.4} KAS) from evm-workers",
        swept,
        swept as f64 / 100_000_000.0,
    );

    if had_error {
        eyre::bail!(
            "parallel EVM funding had errors ({total_submitted}/{} submitted). \
             Re-run to resume (nonce will be re-queried).",
            unfunded_indices.len(),
        );
    }

    Ok(())
}

// ── Benchmark rounds ────────────────────────────────────────────────────────

/// Wait for change UTXOs from submitted IGRA TXs to appear for each sender.
async fn wait_for_change_utxos(
    client: &GrpcClient,
    senders: &[SenderKeypair],
    submitted_tx_ids: &std::collections::HashMap<usize, String>,
    timeout: Duration,
) -> eyre::Result<()> {
    if submitted_tx_ids.is_empty() {
        return Ok(());
    }

    let start = Instant::now();
    loop {
        let addresses: Vec<KaspaAddress> = submitted_tx_ids
            .keys()
            .map(|&i| senders[i].kaspa_address.clone())
            .collect();
        let utxos =
            get_utxos_with_retry(client, addresses, 3).await?;

        let ready_count = submitted_tx_ids
            .iter()
            .filter(|(sender_idx, expected_tx_id)| {
                utxos.iter().any(|u| {
                    u.address.as_ref() == Some(&senders[**sender_idx].kaspa_address)
                        && u.outpoint.transaction_id.to_string() == **expected_tx_id
                })
            })
            .count();

        if ready_count == submitted_tx_ids.len() {
            return Ok(());
        }

        if start.elapsed() > timeout {
            eyre::bail!(
                "timed out waiting for change UTXOs: {ready_count}/{} ready",
                submitted_tx_ids.len()
            );
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Run all benchmark rounds: for each round, mine TXs in parallel then submit.
/// Returns all TX results.
async fn run_benchmark_rounds(
    client: &GrpcClient,
    senders: &[SenderKeypair],
    num_senders: usize,
    txs_per_sender: usize,
    chain_id: u64,
    prefix_bytes: &[u8],
    mining_timeout: Duration,
    network_type: KaspaNetworkType,
    dry_run: bool,
    start_round: usize,
    evm_verify_tx: Option<&tokio::sync::mpsc::UnboundedSender<EvmVerifyRequest>>,
    gas_price: u128,
) -> eyre::Result<Vec<TxResult>> {
    let total_txs = num_senders * (txs_per_sender - start_round);
    let mut all_results: Vec<TxResult> = Vec::with_capacity(total_txs);

    for round in start_round..txs_per_sender {
        eprintln!(
            "[3/5] Round {}/{}: building & mining {} TXs...",
            round + 1,
            txs_per_sender,
            num_senders
        );

        // Fetch all sender UTXOs via a single call
        let all_sender_addresses: Vec<KaspaAddress> =
            senders.iter().map(|s| s.kaspa_address.clone()).collect();
        let all_utxos =
            get_utxos_with_retry(client, all_sender_addresses, 3).await?;

        // Build EVM TXs and mine in parallel (CPU-bound)
        let mut mine_handles = Vec::with_capacity(num_senders);

        for (sender_idx, sender) in senders.iter().enumerate() {
            let private_key = sender.private_key;
            let evm_address = sender.evm_address;
            let kaspa_address = sender.kaspa_address.clone();
            let chain_id = chain_id;
            let evm_nonce = round as u64;
            let prefix_bytes = prefix_bytes.to_vec();
            let mining_timeout = mining_timeout;
            let gas_price = gas_price;

            let sender_utxos: Vec<RpcUtxosByAddressesEntry> = all_utxos
                .iter()
                .filter(|u| u.address.as_ref() == Some(&kaspa_address))
                .cloned()
                .collect();

            let handle = tokio::task::spawn_blocking(move || {
                let (raw_tx, evm_tx_hash) =
                    build_signed_evm_tx(&private_key, evm_address, chain_id, evm_nonce, U256::ZERO, gas_price)?;

                let mine_start = Instant::now();
                let (nonce_iterations, tx) = mine_and_build_signed_payload_transaction(
                    &private_key,
                    &kaspa_address,
                    network_type,
                    &raw_tx,
                    &prefix_bytes,
                    mining_timeout,
                    &sender_utxos,
                )?;
                let mining_time = mine_start.elapsed();

                Ok::<_, eyre::Report>((sender_idx, (nonce_iterations, tx), mining_time, evm_tx_hash))
            });

            mine_handles.push(handle);
        }

        // Collect mined TXs
        let mut mined_txs: Vec<(usize, KaspaTransaction, Duration, u64, String)> = Vec::new();
        for handle in mine_handles {
            match handle.await {
                Ok(Ok((sender_idx, (nonce_iterations, tx), mining_time, evm_tx_hash))) => {
                    mined_txs.push((sender_idx, tx, mining_time, nonce_iterations, evm_tx_hash));
                }
                Ok(Err(e)) => {
                    eprintln!("    mining error: {e}");
                    all_results.push(TxResult {
                        sender_index: 0,
                        round,
                        mining_time: Duration::ZERO,
                        submit_time: None,
                        kaspa_tx_id: None,
                        evm_tx_hash: None,
                        error: Some(e.to_string()),
                        nonce_iterations: 0,
                    });
                }
                Err(e) => {
                    eprintln!("    task join error: {e}");
                    all_results.push(TxResult {
                        sender_index: 0,
                        round,
                        mining_time: Duration::ZERO,
                        submit_time: None,
                        kaspa_tx_id: None,
                        evm_tx_hash: None,
                        error: Some(e.to_string()),
                        nonce_iterations: 0,
                    });
                }
            }
        }

        eprintln!("    mined {}/{} TXs", mined_txs.len(), num_senders);

        // Submit
        if dry_run {
            eprintln!("[4/5] Dry run — skipping submission");
            for (sender_idx, _tx, mining_time, nonce_iterations, evm_tx_hash) in mined_txs {
                all_results.push(TxResult {
                    sender_index: sender_idx,
                    round,
                    mining_time,
                    submit_time: None,
                    kaspa_tx_id: None,
                    evm_tx_hash: Some(evm_tx_hash),
                    error: None,
                    nonce_iterations,
                });
            }
        } else {
            eprintln!("[4/5] Submitting {} TXs in parallel...", mined_txs.len());

            // Submit all TXs in parallel for maximum throughput
            let submit_futures: Vec<_> = mined_txs
                .into_iter()
                .map(|(sender_idx, tx, mining_time, nonce_iterations, evm_tx_hash)| {
                    let tx_id = tx.id().to_string();
                    let rpc_tx = RpcTransaction::from(&tx);
                    async move {
                        let submit_start = Instant::now();
                        match submit_tx_with_retry(client, rpc_tx, 3).await {
                            Ok(_) => {
                                let submit_time = submit_start.elapsed();
                                eprintln!(
                                    "    sender[{sender_idx}] submitted: {tx_id} evm={evm_tx_hash} (mine={mining_time:.2?} submit={submit_time:.2?})"
                                );
                                TxResult {
                                    sender_index: sender_idx,
                                    round,
                                    mining_time,
                                    submit_time: Some(submit_time),
                                    kaspa_tx_id: Some(tx_id),
                                    evm_tx_hash: Some(evm_tx_hash),
                                    error: None,
                                    nonce_iterations,
                                }
                            }
                            Err(e) => {
                                eprintln!("    sender[{sender_idx}] submit error: {e}");
                                TxResult {
                                    sender_index: sender_idx,
                                    round,
                                    mining_time,
                                    submit_time: None,
                                    kaspa_tx_id: None,
                                    evm_tx_hash: Some(evm_tx_hash),
                                    error: Some(e.to_string()),
                                    nonce_iterations,
                                }
                            }
                        }
                    }
                })
                .collect();
            let results = futures::future::join_all(submit_futures).await;

            // Send successful TX hashes to EVM verifier
            if let Some(verify_tx) = evm_verify_tx {
                let now = Instant::now();
                for r in &results {
                    if let Some(ref hash) = r.evm_tx_hash {
                        if r.error.is_none() {
                            let _ = verify_tx.send(EvmVerifyRequest {
                                evm_tx_hash: hash.clone(),
                                kaspa_submit_time: now,
                            });
                        }
                    }
                }
            }

            all_results.extend(results);
        }

        // Wait for change UTXOs before next round
        if round + 1 < txs_per_sender && !dry_run {
            let round_submitted: std::collections::HashMap<usize, String> = all_results
                .iter()
                .filter(|r| r.round == round && r.kaspa_tx_id.is_some())
                .map(|r| (r.sender_index, r.kaspa_tx_id.clone().unwrap()))
                .collect();
            if !round_submitted.is_empty() {
                eprintln!(
                    "    waiting for {} change UTXOs before next round...",
                    round_submitted.len()
                );
                wait_for_change_utxos(client, senders, &round_submitted, Duration::from_secs(30))
                    .await?;
                eprintln!("    change UTXOs confirmed.");
            }
        }

        eprintln!();
    }

    Ok(all_results)
}

// ── Sweep ────────────────────────────────────────────────────────────────────

/// Sweep all remaining funds from sender wallets back to the master address.
async fn sweep_senders_to_master(
    client: &GrpcClient,
    senders: &[SenderKeypair],
    master_address: &KaspaAddress,
    network_type: KaspaNetworkType,
) -> eyre::Result<u64> {
    let mut total_swept = 0u64;

    for (i, sender) in senders.iter().enumerate() {
        let utxos =
            get_utxos_with_retry(client, vec![sender.kaspa_address.clone()], 3).await?;

        if utxos.is_empty() {
            continue;
        }

        let total_input: u64 = utxos.iter().map(|u| u.utxo_entry.amount).sum();
        let fee = estimated_fee_sompi(0, utxos.len());
        if total_input <= fee + MIN_CHANGE_SOMPI {
            continue; // dust, not worth sweeping
        }

        let sweep_amount = total_input - fee;
        let outputs = vec![KaspaTransactionOutput::new(
            sweep_amount,
            pay_to_address_script(master_address),
        )];

        let inputs = utxos_to_inputs(&utxos);
        let mut tx = KaspaTransaction::new(
            0,
            inputs,
            outputs,
            0,
            SubnetworkId::default(),
            0,
            Vec::new(),
        );
        tx.finalize();

        let entries = rpc_utxos_to_entries(&utxos);
        let tx = sign_and_validate_kaspa_tx(tx, entries, &sender.private_key, network_type)?;

        let rpc_tx = RpcTransaction::from(&tx);
        match submit_tx_with_retry(client, rpc_tx, 3).await {
            Ok(_) => {
                eprintln!("  sender[{i}] swept {} sompi", sweep_amount);
                total_swept += sweep_amount;
            }
            Err(e) => {
                eprintln!("  sender[{i}] sweep failed: {e}");
            }
        }
    }

    Ok(total_swept)
}

// ── Result types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct TxResult {
    sender_index: usize,
    round: usize,
    mining_time: Duration,
    submit_time: Option<Duration>,
    kaspa_tx_id: Option<String>,
    evm_tx_hash: Option<String>,
    error: Option<String>,
    nonce_iterations: u64,
}

#[derive(Debug, Serialize)]
struct TxDetail {
    sender_index: usize,
    round: usize,
    kaspa_tx_id: Option<String>,
    evm_tx_hash: Option<String>,
    mining_time_ms: f64,
    submit_time_ms: Option<f64>,
    nonce_iterations: u64,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    total_txs: usize,
    successful_txs: usize,
    failed_txs: usize,
    num_senders: usize,
    txs_per_sender: usize,
    total_time_secs: f64,
    effective_tps: f64,
    dry_run: bool,
    mining_stats: DurationStats,
    submit_stats: Option<DurationStats>,
    transactions: Vec<TxDetail>,
    failures: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DurationStats {
    min_ms: f64,
    max_ms: f64,
    avg_ms: f64,
    p50_ms: f64,
    p99_ms: f64,
}

fn compute_duration_stats(durations: &mut Vec<Duration>) -> DurationStats {
    durations.sort();
    let n = durations.len();
    if n == 0 {
        return DurationStats {
            min_ms: 0.0,
            max_ms: 0.0,
            avg_ms: 0.0,
            p50_ms: 0.0,
            p99_ms: 0.0,
        };
    }
    let min_ms = durations[0].as_secs_f64() * 1000.0;
    let max_ms = durations[n - 1].as_secs_f64() * 1000.0;
    let avg_ms = durations.iter().map(|d| d.as_secs_f64()).sum::<f64>() / n as f64 * 1000.0;
    let p50_ms = durations[n / 2].as_secs_f64() * 1000.0;
    let p99_idx = ((n as f64) * 0.99).ceil() as usize;
    let p99_ms = durations[p99_idx.min(n - 1)].as_secs_f64() * 1000.0;
    DurationStats {
        min_ms,
        max_ms,
        avg_ms,
        p50_ms,
        p99_ms,
    }
}

fn build_report(all_results: &[TxResult], args: &Args, total_time: Duration) -> BenchmarkReport {
    let successful = all_results.iter().filter(|r| r.error.is_none()).count();
    let failed = all_results.iter().filter(|r| r.error.is_some()).count();

    let mut mining_times: Vec<Duration> = all_results
        .iter()
        .filter(|r| r.error.is_none())
        .map(|r| r.mining_time)
        .collect();
    let mining_stats = compute_duration_stats(&mut mining_times);

    let submit_stats = if !args.dry_run {
        let mut submit_times: Vec<Duration> = all_results
            .iter()
            .filter_map(|r| r.submit_time)
            .collect();
        Some(compute_duration_stats(&mut submit_times))
    } else {
        None
    };

    let failures: Vec<String> = all_results
        .iter()
        .filter_map(|r| r.error.clone())
        .collect();

    let effective_tps = if total_time.as_secs_f64() > 0.0 {
        successful as f64 / total_time.as_secs_f64()
    } else {
        0.0
    };

    let transactions: Vec<TxDetail> = all_results
        .iter()
        .map(|r| TxDetail {
            sender_index: r.sender_index,
            round: r.round,
            kaspa_tx_id: r.kaspa_tx_id.clone(),
            evm_tx_hash: r.evm_tx_hash.clone(),
            mining_time_ms: r.mining_time.as_secs_f64() * 1000.0,
            submit_time_ms: r.submit_time.map(|d| d.as_secs_f64() * 1000.0),
            nonce_iterations: r.nonce_iterations,
            error: r.error.clone(),
        })
        .collect();

    BenchmarkReport {
        total_txs: all_results.len(),
        successful_txs: successful,
        failed_txs: failed,
        num_senders: args.num_senders,
        txs_per_sender: args.txs_per_sender,
        total_time_secs: total_time.as_secs_f64(),
        effective_tps,
        dry_run: args.dry_run,
        mining_stats,
        submit_stats,
        transactions,
        failures,
    }
}

// ── EVM Verification ─────────────────────────────────────────────────────────

/// A submitted TX hash to verify on EVM, with the timestamp it was submitted to Kaspa.
struct EvmVerifyRequest {
    evm_tx_hash: String,
    kaspa_submit_time: Instant,
}

#[derive(Debug, Serialize)]
struct EvmVerifyStats {
    total: usize,
    confirmed: usize,
    not_found: usize,
    /// TXs confirmed but exceeding the 3s latency threshold.
    slow_count: usize,
    latency_stats: Option<DurationStats>,
}

/// Background task that polls EVM for transaction receipts.
/// Receives TX hashes via channel, polls until confirmed or timeout.
async fn evm_verifier_task(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<EvmVerifyRequest>,
    el_rpc_url: String,
    poll_interval: Duration,
    timeout: Duration,
) -> EvmVerifyStats {
    let provider = ProviderBuilder::new().connect_http(el_rpc_url.parse().expect("invalid EVM RPC URL"));

    const SLOW_THRESHOLD: Duration = Duration::from_secs(3);

    let mut pending: Vec<(String, Instant)> = Vec::new();
    let mut confirmed = 0usize;
    let mut not_found = 0usize;
    let mut slow_count = 0usize;
    let mut latencies: Vec<Duration> = Vec::new();
    let mut total = 0usize;
    let mut channel_closed = false;

    loop {
        // Drain new requests from channel
        loop {
            match rx.try_recv() {
                Ok(req) => {
                    total += 1;
                    pending.push((req.evm_tx_hash, req.kaspa_submit_time));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    channel_closed = true;
                    break;
                }
            }
        }

        if pending.is_empty() && channel_closed {
            break;
        }

        if pending.is_empty() {
            tokio::time::sleep(poll_interval).await;
            continue;
        }

        // Check pending TXs in batches
        let mut still_pending = Vec::new();
        for (hash_hex, submit_time) in pending.drain(..) {
            let hash: alloy_primitives::B256 = hash_hex
                .parse()
                .unwrap_or_default();
            match provider.get_transaction_receipt(hash).await {
                Ok(Some(_receipt)) => {
                    let latency = submit_time.elapsed();
                    latencies.push(latency);
                    confirmed += 1;
                    if latency > SLOW_THRESHOLD {
                        slow_count += 1;
                        eprintln!(
                            "  [EVM verify] SLOW TX {hash_hex} latency={latency:.2?} (>{:.0}s threshold)",
                            SLOW_THRESHOLD.as_secs_f64()
                        );
                    }
                    if confirmed % 100 == 0 {
                        eprintln!(
                            "  [EVM verify] {confirmed}/{total} confirmed (latency={latency:.2?})"
                        );
                    }
                }
                Ok(None) => {
                    if submit_time.elapsed() > timeout {
                        not_found += 1;
                        eprintln!(
                            "  [EVM verify] TX {hash_hex} not found after {:.0}s",
                            timeout.as_secs_f64()
                        );
                    } else {
                        still_pending.push((hash_hex, submit_time));
                    }
                }
                Err(e) => {
                    // Transient RPC error — keep in pending
                    if submit_time.elapsed() > timeout {
                        not_found += 1;
                        eprintln!("  [EVM verify] TX {hash_hex} error after timeout: {e}");
                    } else {
                        still_pending.push((hash_hex, submit_time));
                    }
                }
            }
        }
        pending = still_pending;

        if !pending.is_empty() {
            tokio::time::sleep(poll_interval).await;
        }
    }

    let latency_stats = if !latencies.is_empty() {
        Some(compute_duration_stats(&mut latencies))
    } else {
        None
    };

    EvmVerifyStats {
        total,
        confirmed,
        not_found,
        slow_count,
        latency_stats,
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();

    let prefix = normalize_hex_prefix(&args.tx_id_prefix);
    if prefix.is_empty() {
        eyre::bail!("--tx-id-prefix cannot be empty");
    }
    let prefix_bytes =
        hex::decode(&prefix).map_err(|e| eyre::eyre!("--tx-id-prefix invalid hex: {e}"))?;

    let (network_type, address_prefix) = kaspa_network_descriptor(&args.network)?;
    let master_private_key = parse_private_key_hex(&args.master_private_key)?;
    let master_address = kaspa_address_from_private_key(&master_private_key, address_prefix)?;
    let mining_timeout = Duration::from_secs(args.mining_timeout_secs);

    let total_txs = args.num_senders * args.txs_per_sender;

    eprintln!("=== IGRA TPS Benchmark ===");
    eprintln!("  network:       {}", args.network);
    eprintln!("  kaspa_rpc_url: {}", args.kaspa_rpc_url);
    eprintln!("  master_addr:   {master_address}");
    eprintln!("  prefix:        {prefix}");
    eprintln!("  num_senders:   {}", args.num_senders);
    eprintln!("  txs_per_sender:{}", args.txs_per_sender);
    eprintln!("  total_txs:     {total_txs}");
    eprintln!("  chain_id:      {}", args.chain_id);
    eprintln!("  dry_run:       {}", args.dry_run);
    if args.parallel_funding {
        eprintln!("  parallel_fund: true ({} workers)", args.funding_workers);
    }
    if args.skip_evm {
        eprintln!("  skip_evm:      true (L1 only, no EVM funding/verify)");
    }

    // Query gas price from EVM RPC, enforce --min-gas-price floor
    let evm_gas_price = {
        let provider =
            ProviderBuilder::new().connect_http(args.el_rpc_url.parse().expect("invalid EVM RPC"));
        let rpc_price = provider.get_gas_price().await.unwrap_or(DEFAULT_IGRA_GAS_PRICE);
        let price = rpc_price.max(args.min_gas_price);
        if price != rpc_price {
            eprintln!("  gas_price:     {} (floor applied, RPC returned {})", price, rpc_price);
        } else if price > DEFAULT_IGRA_GAS_PRICE {
            eprintln!("  gas_price:     {} (from RPC, higher than default {})", price, DEFAULT_IGRA_GAS_PRICE);
        } else {
            eprintln!("  gas_price:     {}", price);
        }
        price
    };
    eprintln!();

    // Phase 1: Setup
    eprintln!("[1/5] Generating {} sender keypairs...", args.num_senders);
    let senders = get_or_create_senders(
        args.num_senders,
        address_prefix,
        args.senders_file.as_deref(),
    )?;
    for (i, s) in senders.iter().enumerate() {
        eprintln!(
            "  sender[{i}]: kaspa={} evm={}",
            s.kaspa_address, s.evm_address
        );
    }
    eprintln!();

    let client = GrpcClient::connect(args.kaspa_rpc_url.clone())
        .await
        .map_err(|e| eyre::eyre!("failed to connect to Kaspa RPC: {e}"))?;

    // Sweep-only mode
    if args.sweep_only {
        eprintln!("[sweep-only] Sweeping sender funds back to master...");
        let swept =
            sweep_senders_to_master(&client, &senders, &master_address, network_type).await;
        match swept {
            Ok(total) => eprintln!(
                "  swept {} sompi ({:.4} KAS) back to master",
                total,
                total as f64 / 100_000_000.0
            ),
            Err(e) => eprintln!("  sweep error: {e}"),
        }
        return Ok(());
    }

    // Pre-flight: check master balance is sufficient before doing any work
    if !args.skip_funding && args.start_round == 0 {
        let utxos = get_utxos_with_retry(&client, vec![master_address.clone()], 3).await?;
        let master_bal: u64 = utxos.iter().map(|u| u.utxo_entry.amount).sum();
        let per_round_fee = estimated_fee_sompi(120, 1);
        let amount_per_sender =
            MIN_SENDER_FUNDING_SOMPI + (args.txs_per_sender as u64) * per_round_fee;
        let l1_funding_needed = (args.num_senders as u64) * amount_per_sender;
        let evm_funding_needed = if args.skip_evm {
            0u64
        } else {
            (args.num_senders as u64) * estimated_fee_sompi(120, 1)
        };
        let total_needed = l1_funding_needed + evm_funding_needed;
        eprintln!("[pre-flight] Master balance: {} sompi ({:.4} KAS)", master_bal, master_bal as f64 / 100_000_000.0);
        eprintln!("[pre-flight] Estimated need: {} sompi ({:.4} KAS) (L1={:.4} + EVM={:.4})",
            total_needed, total_needed as f64 / 100_000_000.0,
            l1_funding_needed as f64 / 100_000_000.0,
            evm_funding_needed as f64 / 100_000_000.0);
        if master_bal < total_needed {
            let deficit = total_needed - master_bal;
            eyre::bail!(
                "insufficient master balance: have {:.4} KAS, need {:.4} KAS (short {:.4} KAS). \
                 Send more tKAS to {}",
                master_bal as f64 / 100_000_000.0,
                total_needed as f64 / 100_000_000.0,
                deficit as f64 / 100_000_000.0,
                master_address,
            );
        }
        eprintln!("[pre-flight] Balance OK ({:.4} KAS surplus)", (master_bal - total_needed) as f64 / 100_000_000.0);
        eprintln!();
    }

    // Phase 1b: Consolidate master UTXOs if fragmented
    eprintln!("[1b] Checking master UTXO fragmentation...");
    consolidate_master_utxos(
        &client,
        &master_private_key,
        &master_address,
        network_type,
        5, // target: at most 5 UTXOs
    )
    .await?;
    eprintln!();

    if args.start_round > 0 || args.skip_funding {
        eprintln!("[2/5] Skipping funding phases (start_round={}, skip_funding={}).", args.start_round, args.skip_funding);
    } else if args.parallel_funding {
        // ── Parallel funding path ──
        let funding_start = Instant::now();
        eprintln!(
            "[2/5] Parallel funding: {} senders, {} workers...",
            args.num_senders, args.funding_workers,
        );

        // Phase 2a: Parallel L1 funding
        fund_senders_parallel(
            &client,
            &master_private_key,
            &master_address,
            &senders,
            network_type,
            address_prefix,
            args.funding_workers,
            args.txs_per_sender,
        )
        .await?;

        eprintln!("  waiting for all sender UTXOs to confirm...");
        wait_for_funding(&client, &senders, Duration::from_secs(60)).await?;
        eprintln!("  all senders funded on L1.");
        eprintln!();

        if !args.skip_evm {
            // Re-consolidate master UTXOs after worker sweeps
            eprintln!("  re-consolidating master UTXOs after L1 funding...");
            consolidate_master_utxos(
                &client,
                &master_private_key,
                &master_address,
                network_type,
                5,
            )
            .await?;

            // Phase 2b: Parallel EVM funding
            fund_senders_evm_parallel(
                &client,
                &master_private_key,
                &master_address,
                &senders,
                args.chain_id,
                args.txs_per_sender,
                &prefix_bytes,
                mining_timeout,
                network_type,
                address_prefix,
                &args.el_rpc_url,
                evm_gas_price,
                args.funding_workers,
            )
            .await?;

            // Phase 2c: EVM funding TXs are submitted to L1 — they'll land on L2
            // in the background. No need to block here; if a sender's EVM TX
            // hasn't landed yet, that round's EVM TX will simply fail on L2
            // (but L1 TX still succeeds, which is what we measure).
            eprintln!("[2c/5] Skipping EVM balance confirmation (TXs submitted to L1, will land on L2 in background).");
        } else {
            eprintln!("  [2b/2c] Skipping EVM funding & confirmation (--skip-evm).");
        }
        eprintln!(
            "  parallel funding complete in {:.1}s",
            funding_start.elapsed().as_secs_f64()
        );
        eprintln!();
    } else {
        // ── Sequential funding path (original) ──
        // Phase 2: Fund senders on Kaspa L1
        eprintln!(
            "[2/5] Funding senders ({} individual TXs)...",
            args.num_senders
        );
        let evm_funding_reserve = if args.skip_evm {
            0
        } else {
            args.num_senders as u64 * estimated_fee_sompi(120, 1)
        };
        fund_senders(
            &client,
            &master_private_key,
            &master_address,
            &senders,
            network_type,
            evm_funding_reserve,
            args.txs_per_sender,
        )
        .await?;

        eprintln!("  waiting for all sender UTXOs to confirm...");
        wait_for_funding(&client, &senders, Duration::from_secs(60)).await?;
        eprintln!("  all senders funded.");
        eprintln!();

        if !args.skip_evm {
            // Phase 2b: Fund senders with iKAS on EVM
            fund_senders_evm(
                &client,
                &master_private_key,
                &master_address,
                &senders,
                args.chain_id,
                args.txs_per_sender,
                &prefix_bytes,
                mining_timeout,
                network_type,
                &args.el_rpc_url,
                evm_gas_price,
            )
            .await?;

            eprintln!("[2c/5] Skipping EVM balance confirmation (TXs submitted to L1, will land on L2 in background).");
        } else {
            eprintln!("  [2b/2c] Skipping EVM funding & confirmation (--skip-evm).");
        }
        eprintln!();
    }

    // Phase 3+4: Benchmark rounds
    let verify_evm = !args.no_verify_evm && !args.skip_evm;
    let (evm_verify_tx, evm_verifier_handle) = if verify_evm && !args.dry_run {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<EvmVerifyRequest>();
        let el_rpc_url = args.el_rpc_url.clone();
        let handle = tokio::spawn(evm_verifier_task(
            rx,
            el_rpc_url,
            Duration::from_millis(500),
            Duration::from_secs(120),
        ));
        eprintln!("  [EVM verify] background verifier started (timeout=120s)");
        (Some(tx), Some(handle))
    } else {
        (None, None)
    };

    let overall_start = Instant::now();
    let all_results = run_benchmark_rounds(
        &client,
        &senders,
        args.num_senders,
        args.txs_per_sender,
        args.chain_id,
        &prefix_bytes,
        mining_timeout,
        network_type,
        args.dry_run,
        args.start_round,
        evm_verify_tx.as_ref(),
        evm_gas_price,
    )
    .await?;
    let total_time = overall_start.elapsed();

    // Drop the sender to signal the verifier that no more TXs are coming
    drop(evm_verify_tx);

    // Phase 5: Report
    eprintln!("[5/5] Generating report...");
    let report = build_report(&all_results, &args, total_time);

    // Wait for EVM verifier to finish
    if let Some(handle) = evm_verifier_handle {
        eprintln!("[EVM verify] waiting for all receipts...");
        let evm_stats = handle.await.unwrap_or(EvmVerifyStats {
            total: 0,
            confirmed: 0,
            not_found: 0,
            slow_count: 0,
            latency_stats: None,
        });
        eprintln!("[EVM verify] results:");
        eprintln!("  total:     {}", evm_stats.total);
        eprintln!("  confirmed: {}", evm_stats.confirmed);
        eprintln!("  not_found: {}", evm_stats.not_found);
        eprintln!("  slow (>3s): {}", evm_stats.slow_count);
        if let Some(ref lat) = evm_stats.latency_stats {
            eprintln!("  latency:   avg={:.0}ms p50={:.0}ms p99={:.0}ms min={:.0}ms max={:.0}ms",
                lat.avg_ms, lat.p50_ms, lat.p99_ms, lat.min_ms, lat.max_ms);
        }
        // Print EVM stats as JSON too
        let evm_json = serde_json::to_string_pretty(&evm_stats).unwrap_or_default();
        eprintln!("{evm_json}");
    }

    // Phase 6: Sweep remaining sender funds back to master
    if !args.dry_run {
        let last_round = args.txs_per_sender.saturating_sub(1);
        let last_submitted: std::collections::HashMap<usize, String> = all_results
            .iter()
            .filter(|r| r.round == last_round && r.kaspa_tx_id.is_some())
            .map(|r| (r.sender_index, r.kaspa_tx_id.clone().unwrap()))
            .collect();
        if !last_submitted.is_empty() {
            eprintln!("[6] Waiting for last round TXs to confirm before sweep...");
            if let Err(e) =
                wait_for_change_utxos(&client, &senders, &last_submitted, Duration::from_secs(30))
                    .await
            {
                eprintln!("  warning: {e}");
            }
        }

        eprintln!("[6] Sweeping sender funds back to master...");
        let swept =
            sweep_senders_to_master(&client, &senders, &master_address, network_type).await;
        match swept {
            Ok(total) => eprintln!(
                "  swept {} sompi ({:.4} KAS) back to master",
                total,
                total as f64 / 100_000_000.0
            ),
            Err(e) => eprintln!("  sweep warning: {e}"),
        }
    }

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
