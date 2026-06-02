use clap::Parser;
use kaspa_addresses::{
    Address as KaspaAddress, Prefix as KaspaAddressPrefix, Version as KaspaAddressVersion,
};
use kaspa_bip32::secp256k1::SecretKey as KaspaSecretKey;
use kaspa_bip32::{
    ChildNumber as KaspaChildNumber, DerivationPath as KaspaDerivationPath,
    ExtendedPrivateKey as KaspaExtendedPrivateKey, Language as KaspaLanguage,
    Mnemonic as KaspaMnemonic,
};

#[derive(Debug, Parser)]
#[command(about = "Derive a Kaspa deposit address from mnemonic + optional BIP39 passphrase.")]
struct Args {
    /// Mnemonic phrase (12/24 words).
    #[arg(long, env = "KASPA_MNEMONIC", hide_env_values = true)]
    mnemonic: String,

    /// BIP39 mnemonic passphrase (aka "recovery passphrase" in kaspa-cli).
    #[arg(long, env = "KASPA_MNEMONIC_PASSPHRASE", hide_env_values = true, default_value = "")]
    mnemonic_passphrase: String,

    /// Derivation path. If omitted, uses m/44'/111111'/0'/0/<index>.
    #[arg(long, env = "KASPA_MNEMONIC_DERIVATION_PATH")]
    derivation_path: Option<String>,

    /// Address index when using default derivation scheme (m/44'/111111'/0'/0/<index>).
    #[arg(long, env = "KASPA_MNEMONIC_INDEX", default_value_t = 0)]
    index: u32,

    /// Kaspa network string (mainnet, testnet-10, devnet, simnet).
    #[arg(long, env = "KASPA_NETWORK", default_value = "testnet-10")]
    network: String,

    /// Optional expected address to compare against.
    #[arg(long)]
    expected: Option<String>,
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();

    let prefix = kaspa_address_prefix(&args.network)?;
    let private_key = derive_private_key(
        &args.mnemonic,
        &args.mnemonic_passphrase,
        args.derivation_path.as_deref(),
        args.index,
    )?;
    let address = kaspa_address_from_private_key(&private_key, prefix)?;

    let mut out = serde_json::json!({
        "kaspa_network": args.network,
        "derivation_path": args.derivation_path.as_deref().unwrap_or("m/44'/111111'/0'/0/<index>"),
        "index": args.index,
        "address": address.to_string(),
    });

    if let Some(expected) = args.expected.as_deref() {
        let matches_expected = expected.trim() == address.to_string();
        out["expected"] = serde_json::Value::String(expected.to_string());
        out["matches_expected"] = serde_json::Value::Bool(matches_expected);
    }

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn kaspa_address_prefix(network: &str) -> eyre::Result<KaspaAddressPrefix> {
    match network {
        "mainnet" => Ok(KaspaAddressPrefix::Mainnet),
        "testnet-10" => Ok(KaspaAddressPrefix::Testnet),
        "devnet" => Ok(KaspaAddressPrefix::Devnet),
        "simnet" => Ok(KaspaAddressPrefix::Simnet),
        other => eyre::bail!("unsupported kaspa network: {other}"),
    }
}

fn derive_private_key(
    mnemonic: &str,
    passphrase: &str,
    derivation_path: Option<&str>,
    index: u32,
) -> eyre::Result<[u8; 32]> {
    let phrase = mnemonic.split_whitespace().collect::<Vec<_>>().join(" ");
    let kaspa_mnemonic = KaspaMnemonic::new(phrase, KaspaLanguage::English)?;
    let seed = kaspa_mnemonic.to_seed(passphrase);
    let xprv = KaspaExtendedPrivateKey::<KaspaSecretKey>::new(seed)?;

    let secret = if let Some(path) = derivation_path {
        let path = path.parse::<KaspaDerivationPath>()?;
        *xprv.derive_path(&path)?.private_key()
    } else {
        let base = "m/44'/111111'/0'/0".parse::<KaspaDerivationPath>()?;
        let base = xprv.derive_path(&base)?;
        *base.derive_child(KaspaChildNumber::new(index, false)?)?.private_key()
    };

    Ok(secret.secret_bytes())
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
