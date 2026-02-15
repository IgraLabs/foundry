use alloy_consensus::{Signed, TxEip1559};
use alloy_json_rpc::{Id, Request, RequestPacket, ResponsePacket, ResponsePayload};
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, Bytes, TxKind, U256, hex};
use alloy_signer_local::PrivateKeySigner;
use alloy_transport::TransportError;
use clap::Parser;
use eyre::{Context, Result, eyre};
use foundry_common::provider::{
    igra_transport::{IgraTransport, IgraTransportConfig},
    runtime_transport::RuntimeTransportBuilder,
};
use foundry_config::IgraKaspaWalletConfig;
use kaspa_addresses::{Address as KaspaAddress, Prefix as KaspaAddressPrefix, Version as KaspaAddressVersion};
use kaspa_bip32::secp256k1::SecretKey as KaspaSecretKey;
use kaspa_bip32::{
    ChildNumber as KaspaChildNumber, DerivationPath as KaspaDerivationPath,
    ExtendedPrivateKey as KaspaExtendedPrivateKey, Language as KaspaLanguage, Mnemonic as KaspaMnemonic,
};
use reqwest::Url;
use serde_json::Value;
use std::{fs, path::Path, time::Duration};
use tokio::time::{Instant, interval_at};
use tower::Service;

#[derive(Debug, Parser)]
#[command(about = "High-throughput IGRA load generator (eth_sendRawTransaction -> Kaspa payload).")]
struct Args {
    /// EL RPC URL (http(s):// or ws(s):// depending on your endpoint).
    #[arg(long, env = "IGRA_EL_RPC_URL")]
    el_rpc_url: String,

    /// Kaspa RPC URL (grpc://...).
    #[arg(long, env = "IGRA_KASPA_RPC_URL")]
    kaspa_rpc_url: String,

    /// Kaspa network string (mainnet, testnet-10, devnet, simnet).
    #[arg(long, env = "IGRA_KASPA_NETWORK", default_value = "testnet-10")]
    kaspa_network: String,

    /// Kaspa TXID prefix to mine (even-length hex without 0x).
    #[arg(long, env = "IGRA_TX_ID_PREFIX", default_value = "97b4")]
    tx_id_prefix: String,

    /// Mining timeout per tx (seconds).
    #[arg(long, env = "IGRA_MINING_TIMEOUT_SECS", default_value_t = 120)]
    mining_timeout_secs: u64,

    /// Total target TPS (aggregate across workers). If omitted or 0, send as fast as possible.
    #[arg(long, env = "IGRA_STRESS_TPS", default_value_t = 0.0)]
    tps: f64,

    /// Run duration in seconds (0 means unlimited; requires --total-txs to stop).
    #[arg(long, env = "IGRA_STRESS_DURATION_SECS", default_value_t = 0)]
    duration_secs: u64,

    /// Total txs per worker (0 means unlimited; requires --duration-secs to stop).
    #[arg(long, env = "IGRA_STRESS_TOTAL_TXS_PER_WORKER", default_value_t = 0)]
    total_txs_per_worker: u64,

    /// Report stats every N seconds.
    #[arg(long, env = "IGRA_STRESS_REPORT_SECS", default_value_t = 5)]
    report_secs: u64,

    /// Disable automatic proxy detection for HTTP(S) RPC connections.
    ///
    /// Useful in sandboxed environments where system proxy/DNS integration is flaky.
    #[arg(long, env = "IGRA_NO_PROXY", default_value_t = false)]
    no_proxy: bool,

    /// Comma-separated EVM private keys (0x...) or a file path containing one key per line.
    #[arg(long, env = "IGRA_STRESS_EVM_KEYS", hide_env_values = true)]
    evm_keys: String,

    /// Comma-separated Kaspa private keys (0x...) or a file path containing one key per line.
    ///
    /// Recommended: one Kaspa key per worker.
    #[arg(long, env = "IGRA_STRESS_KASPA_PRIVATE_KEYS", hide_env_values = true, default_value = "")]
    kaspa_private_keys: String,

    /// Kaspa mnemonic (12/24 words) or a file path containing the phrase.
    ///
    /// If set, Kaspa keys will be derived using the same scheme as IGRA:
    /// - BIP39 seed from mnemonic (+ optional passphrase)
    /// - BIP32 master key
    /// - Default path: m/44'/111111'/0'/0/<index>
    #[arg(long, env = "IGRA_KASPA_MNEMONIC", hide_env_values = true)]
    kaspa_mnemonic: Option<String>,

    /// BIP39 passphrase for the Kaspa mnemonic ("recovery passphrase" in kaspa-cli).
    ///
    /// If omitted and no other passphrase flags are set, defaults to empty string (standard BIP39).
    #[arg(long, env = "IGRA_KASPA_MNEMONIC_PASSPHRASE", hide_env_values = true)]
    kaspa_mnemonic_passphrase: Option<String>,

    /// Use the mnemonic phrase itself as the BIP39 passphrase (non-standard; opt-in).
    #[arg(
        long,
        env = "IGRA_KASPA_MNEMONIC_PASSPHRASE_AS_MNEMONIC",
        default_value_t = false,
        conflicts_with = "kaspa_mnemonic_passphrase",
        conflicts_with = "kaspa_mnemonic_passphrase_empty"
    )]
    kaspa_mnemonic_passphrase_as_mnemonic: bool,

    /// Explicitly set the Kaspa mnemonic passphrase to the empty string.
    #[arg(
        long,
        env = "IGRA_KASPA_MNEMONIC_PASSPHRASE_EMPTY",
        default_value_t = false,
        conflicts_with = "kaspa_mnemonic_passphrase",
        conflicts_with = "kaspa_mnemonic_passphrase_as_mnemonic"
    )]
    kaspa_mnemonic_passphrase_empty: bool,

    /// Start index when deriving Kaspa keys from mnemonic.
    #[arg(long, default_value_t = 0)]
    kaspa_mnemonic_index_start: u32,

    /// Number of Kaspa keys to derive from mnemonic (defaults to worker count).
    #[arg(long)]
    kaspa_mnemonic_count: Option<u32>,

    /// Allow using a single Kaspa key for all workers (not recommended).
    #[arg(long, env = "ALLOW_SHARED_KASPA_KEY", default_value_t = false)]
    allow_shared_kaspa_key: bool,

    /// Gas limit for the EVM transaction.
    #[arg(long, env = "IGRA_STRESS_GAS_LIMIT", default_value_t = 21_000)]
    gas_limit: u64,

    /// Max fee per gas for EIP-1559 txs (wei).
    #[arg(long, env = "IGRA_STRESS_MAX_FEE_PER_GAS", default_value_t = 2_000_000_000)]
    max_fee_per_gas: u128,

    /// Max priority fee per gas for EIP-1559 txs (wei).
    #[arg(long, env = "IGRA_STRESS_MAX_PRIORITY_FEE_PER_GAS", default_value_t = 1_000_000_000)]
    max_priority_fee_per_gas: u128,

    /// Optional destination address. Defaults to sender (self-call transfer with 0 value).
    #[arg(long, env = "IGRA_STRESS_TO")]
    to: Option<String>,

    /// Optional hex calldata (0x...). Defaults to empty.
    #[arg(long, env = "IGRA_STRESS_DATA", default_value = "0x")]
    data: String,

    /// Print derived worker addresses (EVM + Kaspa) and exit.
    #[arg(long, default_value_t = false)]
    print_addresses: bool,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();

    let evm_keys = parse_list_or_file(&args.evm_keys)?;
    if evm_keys.is_empty() {
        return Err(eyre!("no EVM keys provided"));
    }
    let mut kaspa_keys = parse_list_or_file(&args.kaspa_private_keys)?;
    if kaspa_keys.is_empty() {
        if let Some(mnemonic) = args.kaspa_mnemonic.as_deref() {
            let count = args.kaspa_mnemonic_count.unwrap_or(evm_keys.len().try_into().unwrap_or(0));
            if count == 0 {
                return Err(eyre!("kaspa mnemonic derivation requires non-zero --kaspa-mnemonic-count"));
            }
            kaspa_keys = derive_kaspa_private_keys(
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
    if kaspa_keys.is_empty() {
        return Err(eyre!(
            "no Kaspa keys provided (set --kaspa-private-keys or --kaspa-mnemonic)"
        ));
    }
    if !args.allow_shared_kaspa_key && kaspa_keys.len() < evm_keys.len() {
        return Err(eyre!(
            "need >= as many Kaspa keys as EVM keys (workers); set --allow-shared-kaspa-key to override"
        ));
    }

    if args.print_addresses {
        let mut out = Vec::new();
        for (idx, evm_key) in evm_keys.iter().enumerate() {
            let kaspa_key = if kaspa_keys.len() > idx {
                kaspa_keys[idx].clone()
            } else {
                kaspa_keys[0].clone()
            };
            let evm_signer = evm_key
                .parse::<PrivateKeySigner>()
                .map_err(|err| eyre!("invalid EVM private key at index {}: {}", idx, err))?;
            let evm_sender = evm_signer.address();
            let kaspa_address = kaspa_address_from_private_key_hex(&kaspa_key, &args.kaspa_network)
                .wrap_err_with(|| format!("invalid Kaspa private key at index {idx}"))?;
            out.push(serde_json::json!({
                "worker": idx + 1,
                "evm_sender": format!("{evm_sender:#x}"),
                "kaspa_address": kaspa_address.to_string(),
            }));
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let el_url = Url::parse(&args.el_rpc_url).wrap_err("invalid --el-rpc-url")?;

    // Shared EL transport (read-path calls are small).
    let inner = RuntimeTransportBuilder::new(el_url)
        .with_timeout(Duration::from_secs(30))
        .no_proxy(args.no_proxy)
        .build();

    let (chain_id, report_nonce) = {
        let mut t = IgraTransport::new(inner.clone(), true).with_transport_config(IgraTransportConfig {
            tx_id_prefix: Some(args.tx_id_prefix.clone()),
            mining_timeout_secs: Some(args.mining_timeout_secs),
            kaspa_rpc_url: Some(args.kaspa_rpc_url.clone()),
            kaspa_network: Some(args.kaspa_network.clone()),
            payload_compression: Some("none".to_string()),
            kaspa_utxo_mode: Some("chain".to_string()),
            kaspa_fee_mode: Some("estimate".to_string()),
            kaspa_fee_bucket: Some("normal".to_string()),
            kaspa_wallet: IgraKaspaWalletConfig::default(),
        });
        let chain_id = eth_chain_id(&mut t).await?;
        let report_nonce = 0u64;
        (chain_id, report_nonce)
    };
    if report_nonce != 0 {
        // This is intentionally not a structured log; it helps disambiguate multiple loadgens.
        eprintln!("[igra-loadgen] rpc-seed={report_nonce}");
    }

    let workers = evm_keys.len();
    let per_worker_tps = if args.tps > 0.0 { args.tps / (workers as f64) } else { 0.0 };

    let to_addr = if let Some(to) = args.to.as_deref() {
        parse_address(to).wrap_err("invalid --to")?
    } else {
        Address::ZERO // per-worker default is sender; we patch it below
    };
    let calldata = parse_hex_bytes(&args.data).wrap_err("invalid --data")?;

    let start = Instant::now();
    let stop_at = if args.duration_secs > 0 {
        Some(start + Duration::from_secs(args.duration_secs))
    } else {
        None
    };

    let mut handles = Vec::with_capacity(workers);
    for (idx, evm_key) in evm_keys.iter().enumerate() {
        let kaspa_key = if kaspa_keys.len() > idx {
            kaspa_keys[idx].clone()
        } else {
            kaspa_keys[0].clone()
        };

        let signer = evm_key
            .parse::<PrivateKeySigner>()
            .map_err(|err| eyre!("invalid EVM private key at index {}: {}", idx, err))?;
        let sender = signer.address();

        let mut transport = IgraTransport::new(inner.clone(), true).with_transport_config(IgraTransportConfig {
            tx_id_prefix: Some(args.tx_id_prefix.clone()),
            mining_timeout_secs: Some(args.mining_timeout_secs),
            kaspa_rpc_url: Some(args.kaspa_rpc_url.clone()),
            kaspa_network: Some(args.kaspa_network.clone()),
            payload_compression: Some("none".to_string()),
            kaspa_utxo_mode: Some("chain".to_string()),
            kaspa_fee_mode: Some("estimate".to_string()),
            kaspa_fee_bucket: Some("normal".to_string()),
            kaspa_wallet: IgraKaspaWalletConfig { private_key: Some(kaspa_key), ..Default::default() },
        });

        let nonce = eth_get_transaction_count_pending(&mut transport, sender).await?;
        let worker_to = if to_addr == Address::ZERO { sender } else { to_addr };
        let gas_limit = args.gas_limit;
        let max_fee_per_gas = args.max_fee_per_gas;
        let max_priority_fee_per_gas = args.max_priority_fee_per_gas;
        let total_txs = args.total_txs_per_worker;
        let report_every = args.report_secs.max(1);
        let payload = calldata.clone();

        let handle = tokio::spawn(async move {
            run_worker(
                idx + 1,
                transport,
                signer,
                chain_id,
                nonce,
                worker_to,
                payload,
                gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                per_worker_tps,
                stop_at,
                total_txs,
                report_every,
            )
            .await
        });
        handles.push(handle);
    }

    let mut ok_total = 0u64;
    let mut fail_total = 0u64;
    for h in handles {
        let (ok, fail) = h.await.wrap_err("worker join failed")??;
        ok_total = ok_total.saturating_add(ok);
        fail_total = fail_total.saturating_add(fail);
    }

    let elapsed = start.elapsed();
    let ok_tps = if elapsed.as_secs_f64() > 0.0 { (ok_total as f64) / elapsed.as_secs_f64() } else { 0.0 };
    let out = serde_json::json!({
        "workers": workers,
        "results": { "ok": ok_total, "fail": fail_total, "elapsed_secs": elapsed.as_secs_f64(), "ok_tps": ok_tps },
        "config": {
            "el_rpc_url": args.el_rpc_url,
            "kaspa_rpc_url": args.kaspa_rpc_url,
            "kaspa_network": args.kaspa_network,
            "tx_id_prefix": args.tx_id_prefix,
            "mining_timeout_secs": args.mining_timeout_secs,
            "tps": args.tps,
            "duration_secs": args.duration_secs,
            "total_txs_per_worker": args.total_txs_per_worker,
        }
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

async fn run_worker<T>(
    worker_id: usize,
    transport: IgraTransport<T>,
    signer: PrivateKeySigner,
    chain_id: u64,
    mut nonce: u64,
    to: Address,
    data: Vec<u8>,
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    per_worker_tps: f64,
    stop_at: Option<Instant>,
    total_txs: u64,
    report_every_secs: u64,
) -> Result<(u64, u64)>
where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Clone
        + Send
        + 'static,
    T::Future: Send + 'static,
{
    let start = Instant::now();
    let mut ok = 0u64;
    let fail = 0u64;

    let period = if per_worker_tps > 0.0 { Some(Duration::from_secs_f64(1.0 / per_worker_tps)) } else { None };
    let mut tick = period.map(|p| interval_at(Instant::now(), p));
    let mut next_report = Instant::now() + Duration::from_secs(report_every_secs);

    loop {
        if let Some(stop) = stop_at {
            if Instant::now() >= stop {
                break;
            }
        }
        if total_txs > 0 && ok.saturating_add(fail) >= total_txs {
            break;
        }
        if let Some(t) = tick.as_mut() {
            t.tick().await;
        }

        let raw_tx = build_signed_eip1559(
            &signer,
            chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to,
            &data,
        )?;

        let result = transport
            .request(send_raw_packet(&raw_tx))
            .await;

        match result {
            Ok(_) => {
                ok = ok.saturating_add(1);
                nonce = nonce.saturating_add(1);
            }
            Err(err) => {
                return Err(eyre!("[w{}] send failed: {}", worker_id, err));
            }
        }

        if Instant::now() >= next_report {
            let elapsed = start.elapsed().as_secs_f64().max(1e-9);
            eprintln!(
                "[igra-loadgen][w{}] ok={} fail={} ok_tps={}",
                worker_id,
                ok,
                fail,
                (ok as f64) / elapsed
            );
            next_report = Instant::now() + Duration::from_secs(report_every_secs);
        }
    }

    Ok((ok, fail))
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

fn kaspa_address_from_private_key_hex(private_key: &str, network: &str) -> Result<KaspaAddress> {
    let network = network.trim();
    let prefix = match network {
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
    let effective_passphrase = if passphrase_as_mnemonic && passphrase.is_none() && !passphrase_empty {
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
            // Note: this yields the same key for each iteration (path doesn't include index).
            *xprv
                .clone()
                .derive_path(path)
                .map_err(|err| eyre!("failed to derive Kaspa key by path: {err}"))?
                .private_key()
        } else {
            let base = base_xprv
                .as_ref()
                .expect("base_xprv is set when no path override");
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
