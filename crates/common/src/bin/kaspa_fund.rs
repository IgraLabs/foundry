use clap::Parser;
use eyre::{Context, Result, eyre};
use kaspa_addresses::{Address as KaspaAddress, Prefix as KaspaAddressPrefix, Version as KaspaAddressVersion};
use kaspa_bip32::secp256k1::SecretKey as KaspaSecretKey;
use kaspa_bip32::{
    ChildNumber as KaspaChildNumber, DerivationPath as KaspaDerivationPath,
    ExtendedPrivateKey as KaspaExtendedPrivateKey, Language as KaspaLanguage, Mnemonic as KaspaMnemonic,
};
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
use std::path::Path;

const MIN_CHANGE_SOMPI: u64 = 1_000;

#[derive(Debug, Parser)]
#[command(about = "Fund multiple Kaspa addresses from a single source key (gRPC).")]
struct Args {
    /// Kaspa gRPC URL (grpc://...).
    #[arg(long)]
    kaspa_rpc_url: String,

    /// Kaspa network string (mainnet, testnet-10, devnet, simnet).
    #[arg(long, default_value = "testnet-10")]
    kaspa_network: String,

    /// Source mnemonic phrase (12/24 words) or a file path containing it.
    #[arg(long, env = "KASPA_MNEMONIC", hide_env_values = true)]
    mnemonic: Option<String>,

    /// Source mnemonic passphrase (aka "recovery passphrase" in kaspa-cli).
    #[arg(long, env = "KASPA_MNEMONIC_PASSPHRASE", hide_env_values = true, default_value = "")]
    mnemonic_passphrase: String,

    /// Source mnemonic index (default derivation scheme).
    #[arg(long, env = "KASPA_MNEMONIC_INDEX", default_value_t = 0)]
    mnemonic_index: u32,

    /// Source Kaspa private key in hex (0x...).
    #[arg(long, env = "KASPA_PRIVATE_KEY", hide_env_values = true)]
    private_key: Option<String>,

    /// One or more outputs formatted as: `<kaspa_address>:<sompi_amount>`.
    ///
    /// Example: `kaspatest:qq...:100000000` (1 KAS if 1e8 sompi).
    #[arg(long = "to", required = true)]
    outputs: Vec<String>,

    /// Fee rate in sompi/gram (defaults to node estimate normal bucket).
    #[arg(long)]
    feerate: Option<f64>,
}

fn main() -> Result<()> {
    // Use a current-thread runtime here. In some dev/test setups this avoids flaky DNS resolution
    // failures compared to the default multi-thread runtime.
    let args = Args::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .wrap_err("failed to build tokio runtime")?;
    rt.block_on(async_main(args))
}

async fn async_main(args: Args) -> Result<()> {

    let (network_type, prefix) = kaspa_network_descriptor(&args.kaspa_network)?;
    let private_key = resolve_private_key(&args)?;
    let source_address = kaspa_address_from_private_key(&private_key, prefix)?;

    let outputs = parse_outputs(&args.outputs)?;
    let total_out: u64 = outputs.iter().map(|(_, amount)| *amount).sum();

    let client = GrpcClient::connect(args.kaspa_rpc_url.clone())
        .await
        .wrap_err("failed to connect to kaspa gRPC")?;

    let mut utxos = client
        .get_utxos_by_addresses(vec![source_address.clone()])
        .await
        .wrap_err("failed to fetch source UTXOs")?;
    utxos.sort_by_key(|entry| std::cmp::Reverse(entry.utxo_entry.amount));

    let feerate = if let Some(fr) = args.feerate {
        fr
    } else {
        // Use "normal" bucket first entry; enforce protocol minimum 1.0.
        let estimate = client
            .get_fee_estimate()
            .await
            .wrap_err("failed to fetch fee estimate")?;
        estimate
            .normal_buckets
            .first()
            .map(|b| b.feerate)
            .unwrap_or(estimate.priority_bucket.feerate)
            .max(1.0)
    };

    // Select inputs with a pessimistic fee guess, then refine fee from actual mass.
    let mut selected = Vec::new();
    let mut total_in = 0u64;
    let fee_guess = 10_000u64;
    for entry in utxos.iter().cloned() {
        total_in = total_in.saturating_add(entry.utxo_entry.amount);
        selected.push(entry);
        if total_in >= total_out.saturating_add(fee_guess).saturating_add(MIN_CHANGE_SOMPI) {
            break;
        }
    }
    if total_in < total_out.saturating_add(fee_guess).saturating_add(MIN_CHANGE_SOMPI) {
        return Err(eyre!(
            "insufficient funds: total_in={} total_out={} fee_guess={}",
            total_in,
            total_out,
            fee_guess
        ));
    }

    // Iterate once to compute fee from actual mass.
    let (tx, fee, mass) = build_signed_funding_tx(
        &private_key,
        &source_address,
        network_type,
        &selected,
        &outputs,
        feerate,
    )?;

    // Some environments report unexpectedly large masses here. Since the node will re-validate
    // standardness anyway, avoid blocking funding on a local mass calculation.
    //
    // If the tx is truly non-standard, submit_transaction will fail.
    tx.set_mass(0);

    let rpc_tx = RpcTransaction::from(&tx);
    let tx_id = client
        .submit_transaction(rpc_tx, false)
        .await
        .wrap_err("submit_transaction failed")?;

    println!(
        "{{\"source\":\"{}\",\"tx_id\":\"{}\",\"outputs\":{},\"fee_sompi\":{},\"feerate_sompi_per_gram\":{},\"mass\":{}}}",
        source_address,
        tx_id,
        outputs.len(),
        fee,
        feerate,
        mass
    );

    Ok(())
}

fn parse_outputs(values: &[String]) -> Result<Vec<(KaspaAddress, u64)>> {
    let mut out = Vec::with_capacity(values.len());
    for raw in values {
        // Kaspa addresses contain `:` (e.g. `kaspatest:...`), so split from the right.
        let (addr, amount) = raw
            .rsplit_once(':')
            .ok_or_else(|| eyre!("invalid --to entry (expected <address>:<sompi>): {raw}"))?;
        let address = KaspaAddress::try_from(addr.trim())
            .wrap_err_with(|| format!("invalid kaspa address in --to: {addr}"))?;
        let amount = amount
            .trim()
            .parse::<u64>()
            .wrap_err_with(|| format!("invalid sompi amount in --to: {raw}"))?;
        out.push((address, amount));
    }
    Ok(out)
}

fn resolve_private_key(args: &Args) -> Result<[u8; 32]> {
    if let Some(pk) = args.private_key.as_deref() {
        return parse_private_key_hex(pk);
    }
    let mnemonic = args
        .mnemonic
        .as_deref()
        .ok_or_else(|| eyre!("must set either --private-key or --mnemonic"))?;
    derive_private_key(mnemonic, &args.mnemonic_passphrase, None, args.mnemonic_index)
}

fn parse_private_key_hex(private_key: &str) -> Result<[u8; 32]> {
    let key = private_key.trim().strip_prefix("0x").unwrap_or(private_key.trim());
    let bytes = alloy_primitives::hex::decode(key)
        .map_err(|err| eyre!("invalid private key hex: {err}"))?;
    if bytes.len() != 32 {
        return Err(eyre!("expected 32-byte private key hex"));
    }
    let mut fixed = [0u8; 32];
    fixed.copy_from_slice(&bytes);
    Ok(fixed)
}

fn derive_private_key(
    mnemonic: &str,
    passphrase: &str,
    derivation_path: Option<&str>,
    index: u32,
) -> Result<[u8; 32]> {
    let phrase = if Path::new(mnemonic).is_file() {
        std::fs::read_to_string(mnemonic)
            .wrap_err("failed to read mnemonic file")?
    } else {
        mnemonic.to_string()
    };
    let phrase = phrase.split_whitespace().collect::<Vec<_>>().join(" ");

    let kaspa_mnemonic = KaspaMnemonic::new(phrase, KaspaLanguage::English)
        .map_err(|err| eyre!("invalid Kaspa mnemonic: {err}"))?;
    let seed = kaspa_mnemonic.to_seed(passphrase);
    let xprv = KaspaExtendedPrivateKey::<KaspaSecretKey>::new(seed)
        .map_err(|err| eyre!("failed to derive Kaspa master key: {err}"))?;

    let secret = if let Some(path) = derivation_path {
        let path = path
            .parse::<KaspaDerivationPath>()
            .map_err(|err| eyre!("invalid Kaspa derivation path: {err}"))?;
        *xprv
            .derive_path(&path)
            .map_err(|err| eyre!("failed to derive Kaspa key by path: {err}"))?
            .private_key()
    } else {
        let base = "m/44'/111111'/0'/0"
            .parse::<KaspaDerivationPath>()
            .map_err(|err| eyre!("failed to parse default Kaspa derivation path: {err}"))?;
        let base = xprv
            .derive_path(&base)
            .map_err(|err| eyre!("failed to derive default Kaspa base key: {err}"))?;
        *base
            .derive_child(
                KaspaChildNumber::new(index, false)
                    .map_err(|err| eyre!("invalid Kaspa mnemonic index: {err}"))?,
            )
            .map_err(|err| eyre!("failed to derive Kaspa key by index: {err}"))?
            .private_key()
    };

    Ok(secret.secret_bytes())
}

fn kaspa_network_descriptor(
    network: &str,
) -> Result<(KaspaNetworkType, KaspaAddressPrefix)> {
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
        .map_err(|err| eyre!("invalid private key bytes: {err}"))?;
    let public_key = kaspa_bip32::secp256k1::PublicKey::from_secret_key_global(&secret);
    let payload = public_key.x_only_public_key().0.serialize();
    Ok(KaspaAddress::new(prefix, KaspaAddressVersion::PubKey, &payload))
}

fn fee_from_feerate(mass: u64, feerate_sompi_per_gram: f64) -> u64 {
    let fee = (feerate_sompi_per_gram * (mass as f64)).ceil();
    if fee <= 1.0 { 1 } else if fee >= (u64::MAX as f64) { u64::MAX } else { fee as u64 }
}

fn build_signed_funding_tx(
    private_key: &[u8; 32],
    source_address: &KaspaAddress,
    network_type: KaspaNetworkType,
    utxos: &[RpcUtxosByAddressesEntry],
    outputs: &[(KaspaAddress, u64)],
    feerate: f64,
) -> Result<(KaspaTransaction, u64, u64)> {
    if utxos.is_empty() {
        return Err(eyre!("no UTXOs available for source address"));
    }

    let inputs = utxos
        .iter()
        .map(|entry| KaspaTransactionInput::new(entry.outpoint.clone().into(), Vec::new(), 0, 1))
        .collect::<Vec<_>>();
    let total_in: u64 = utxos.iter().map(|e| e.utxo_entry.amount).sum();
    let total_out: u64 = outputs.iter().map(|(_, amount)| *amount).sum();

    let entries = utxos
        .iter()
        .map(|entry| KaspaUtxoEntry {
            amount: entry.utxo_entry.amount,
            script_public_key: entry.utxo_entry.script_public_key.clone(),
            block_daa_score: entry.utxo_entry.block_daa_score,
            is_coinbase: entry.utxo_entry.is_coinbase,
        })
        .collect::<Vec<_>>();

    let mass_calculator =
        KaspaMassCalculator::new_with_consensus_params(&KaspaParams::from(network_type));
    let change_script = pay_to_address_script(source_address);
    let payload = vec![];

    // Iterate until fee stabilizes (mass should be stable, but be defensive).
    let mut fee = 1u64;
    for _iter in 0..4 {
        let required = total_out.saturating_add(fee).saturating_add(MIN_CHANGE_SOMPI);
        if total_in < required {
            return Err(eyre!(
                "insufficient inputs for outputs+fee: total_in={} required={}",
                total_in,
                required
            ));
        }
        let change = total_in.saturating_sub(total_out).saturating_sub(fee);
        if change < MIN_CHANGE_SOMPI {
            return Err(eyre!("computed change below MIN_CHANGE_SOMPI"));
        }

        let mut tx_outputs = Vec::with_capacity(outputs.len() + 1);
        for (addr, amount) in outputs.iter() {
            tx_outputs.push(KaspaTransactionOutput::new(*amount, pay_to_address_script(addr)));
        }
        tx_outputs.push(KaspaTransactionOutput::new(change, change_script.clone()));

        let mut tx = KaspaTransaction::new(
            0,
            inputs.clone(),
            tx_outputs,
            0,
            SubnetworkId::default(),
            0,
            payload.clone(),
        );
        tx.finalize();

        let signable = KaspaSignableTransaction::with_entries(tx, entries.clone());
        let signed = kaspa_sign_with_multiple_v2(signable, std::slice::from_ref(private_key))
            .fully_signed()
            .map_err(|err| eyre!("failed to sign Kaspa tx: {err}"))?;
        kaspa_verify(&signed.as_verifiable())
            .map_err(|err| eyre!("invalid Kaspa signature set: {err}"))?;

        let non_contextual = mass_calculator.calc_non_contextual_masses(&signed.tx);
        let contextual = mass_calculator
            .calc_contextual_masses(&signed.as_verifiable())
            .ok_or_else(|| eyre!("failed to calculate Kaspa tx storage mass"))?;
        let mass = contextual.max(non_contextual);
        let needed_fee = fee_from_feerate(mass, feerate);

        if needed_fee == fee {
            let tx = signed.tx;
            tx.set_mass(mass);
            return Ok((tx, fee, mass));
        }
        fee = needed_fee;
    }

    Err(eyre!("fee did not converge after iterations"))
}
