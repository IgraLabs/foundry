use crate::{signer::WalletSigner, utils, wallet_raw::RawWalletOpts};
use alloy_primitives::Address;
use clap::Parser;
use eyre::Result;
use foundry_config::{Config, IgraKaspaWalletConfig};
use serde::Serialize;
use tracing::warn;

/// The wallet options can either be:
/// 1. Raw (via private key / mnemonic file, see `RawWallet`)
/// 2. Keystore (via file path)
/// 3. Ledger
/// 4. Trezor
/// 5. AWS KMS
/// 6. Google Cloud KMS
/// 7. Turnkey
/// 8. Browser wallet
#[derive(Clone, Debug, Default, Serialize, Parser)]
#[command(next_help_heading = "Wallet options", about = None, long_about = None)]
pub struct WalletOpts {
    /// The sender account.
    #[arg(
        long,
        short,
        value_name = "ADDRESS",
        help_heading = "Wallet options - raw",
        env = "ETH_FROM"
    )]
    pub from: Option<Address>,

    #[command(flatten)]
    pub raw: RawWalletOpts,

    #[command(flatten)]
    pub kaspa: KaspaWalletOpts,

    /// Use the keystore in the given folder or file.
    #[arg(
        long = "keystore",
        help_heading = "Wallet options - keystore",
        value_name = "PATH",
        env = "ETH_KEYSTORE"
    )]
    pub keystore_path: Option<String>,

    /// Use a keystore from the default keystores folder (~/.foundry/keystores) by its filename
    #[arg(
        long = "account",
        help_heading = "Wallet options - keystore",
        value_name = "ACCOUNT_NAME",
        env = "ETH_KEYSTORE_ACCOUNT",
        conflicts_with = "keystore_path"
    )]
    pub keystore_account_name: Option<String>,

    /// The keystore password.
    ///
    /// Used with --keystore.
    #[arg(
        long = "password",
        help_heading = "Wallet options - keystore",
        requires = "keystore_path",
        value_name = "PASSWORD"
    )]
    pub keystore_password: Option<String>,

    /// The keystore password file path.
    ///
    /// Used with --keystore.
    #[arg(
        long = "password-file",
        help_heading = "Wallet options - keystore",
        requires = "keystore_path",
        value_name = "PASSWORD_FILE",
        env = "ETH_PASSWORD"
    )]
    pub keystore_password_file: Option<String>,

    /// Use a Ledger hardware wallet.
    #[arg(long, short, help_heading = "Wallet options - hardware wallet")]
    pub ledger: bool,

    /// Use a Trezor hardware wallet.
    #[arg(long, short, help_heading = "Wallet options - hardware wallet")]
    pub trezor: bool,

    /// Use AWS Key Management Service.
    ///
    /// Ensure the AWS_KMS_KEY_ID environment variable is set.
    #[arg(long, help_heading = "Wallet options - remote", hide = !cfg!(feature = "aws-kms"))]
    pub aws: bool,

    /// Use Google Cloud Key Management Service.
    ///
    /// Ensure the following environment variables are set: GCP_PROJECT_ID, GCP_LOCATION,
    /// GCP_KEY_RING, GCP_KEY_NAME, GCP_KEY_VERSION.
    ///
    /// See: <https://cloud.google.com/kms/docs>
    #[arg(long, help_heading = "Wallet options - remote", hide = !cfg!(feature = "gcp-kms"))]
    pub gcp: bool,

    /// Use Turnkey.
    ///
    /// Ensure the following environment variables are set: TURNKEY_API_PRIVATE_KEY,
    /// TURNKEY_ORGANIZATION_ID, TURNKEY_ADDRESS.
    ///
    /// See: <https://docs.turnkey.com/getting-started/quickstart>
    #[arg(long, help_heading = "Wallet options - remote", hide = !cfg!(feature = "turnkey"))]
    pub turnkey: bool,

    /// Use a browser wallet.
    #[arg(long, help_heading = "Wallet options - browser")]
    pub browser: bool,

    /// Port for the browser wallet server.
    #[arg(
        long,
        help_heading = "Wallet options - browser",
        value_name = "PORT",
        default_value = "9545",
        requires = "browser"
    )]
    pub browser_port: u16,

    /// Whether to open the browser for wallet connection.
    #[arg(
        long,
        help_heading = "Wallet options - browser",
        default_value_t = false,
        requires = "browser"
    )]
    pub browser_disable_open: bool,

    /// Enable development mode for the browser wallet.
    /// This relaxes certain security features for local development.
    ///
    /// **WARNING**: This should only be used in a development environment.
    #[arg(long, help_heading = "Wallet options - browser", hide = true)]
    pub browser_development: bool,
}

/// Kaspa key source options used by IGRA mode.
#[derive(Clone, Debug, Default, Serialize, Parser)]
#[command(next_help_heading = "Wallet options - kaspa", about = None, long_about = None)]
pub struct KaspaWalletOpts {
    /// Use the provided Kaspa private key.
    #[arg(
        long = "private-key-kaspa",
        env = "KASPA_PRIVATE_KEY",
        value_name = "RAW_PRIVATE_KEY",
        hide_env_values = true
    )]
    pub private_key_kaspa: Option<String>,

    /// Use the Kaspa mnemonic phrase.
    #[arg(
        long = "mnemonic-kaspa",
        env = "KASPA_MNEMONIC",
        value_name = "MNEMONIC",
        hide_env_values = true
    )]
    pub mnemonic_kaspa: Option<String>,

    /// Use a BIP39 passphrase for the Kaspa mnemonic.
    #[arg(
        long = "mnemonic-passphrase-kaspa",
        env = "KASPA_MNEMONIC_PASSPHRASE",
        value_name = "PASSPHRASE",
        hide_env_values = true
    )]
    pub mnemonic_passphrase_kaspa: Option<String>,

    /// Kaspa mnemonic derivation path override.
    #[arg(
        long = "mnemonic-derivation-path-kaspa",
        env = "KASPA_MNEMONIC_DERIVATION_PATH",
        value_name = "PATH"
    )]
    pub mnemonic_derivation_path_kaspa: Option<String>,

    /// Kaspa mnemonic index override.
    #[arg(long = "mnemonic-index-kaspa", env = "KASPA_MNEMONIC_INDEX", value_name = "INDEX")]
    pub mnemonic_index_kaspa: Option<u32>,

    /// Use a Kaspa keystore file.
    #[arg(long = "keystore-kaspa", env = "KASPA_KEYSTORE", value_name = "PATH")]
    pub keystore_kaspa: Option<String>,

    /// Use a Kaspa keystore account alias.
    #[arg(
        long = "keystore-account-kaspa",
        env = "KASPA_KEYSTORE_ACCOUNT",
        value_name = "ACCOUNT_NAME"
    )]
    pub keystore_account_kaspa: Option<String>,

    /// Kaspa keystore password.
    #[arg(
        long = "password-kaspa",
        env = "KASPA_PASSWORD",
        value_name = "PASSWORD",
        hide_env_values = true
    )]
    pub password_kaspa: Option<String>,
}

impl KaspaWalletOpts {
    /// Returns true when explicit Kaspa signer source was provided.
    pub fn is_set(&self) -> bool {
        self.private_key_kaspa.is_some() ||
            self.mnemonic_kaspa.is_some() ||
            self.mnemonic_passphrase_kaspa.is_some() ||
            self.mnemonic_derivation_path_kaspa.is_some() ||
            self.mnemonic_index_kaspa.is_some() ||
            self.keystore_kaspa.is_some() ||
            self.keystore_account_kaspa.is_some() ||
            self.password_kaspa.is_some()
    }

    /// Converts CLI/env Kaspa options to config shape.
    pub fn as_config(&self) -> IgraKaspaWalletConfig {
        IgraKaspaWalletConfig {
            private_key: self.private_key_kaspa.clone(),
            mnemonic: self.mnemonic_kaspa.clone(),
            mnemonic_passphrase: self.mnemonic_passphrase_kaspa.clone(),
            mnemonic_derivation_path: self.mnemonic_derivation_path_kaspa.clone(),
            mnemonic_index: self.mnemonic_index_kaspa,
            keystore: self.keystore_kaspa.clone(),
            keystore_account: self.keystore_account_kaspa.clone(),
            password: self.password_kaspa.clone(),
        }
    }
}

impl WalletOpts {
    pub async fn signer(&self) -> Result<WalletSigner> {
        trace!("start finding signer");

        let get_env = |key: &str| {
            std::env::var(key)
                .map_err(|_| eyre::eyre!("{key} environment variable is required for signer"))
        };

        let signer = if self.ledger {
            utils::create_ledger_signer(self.raw.hd_path.as_deref(), self.raw.mnemonic_index)
                .await?
        } else if self.trezor {
            utils::create_trezor_signer(self.raw.hd_path.as_deref(), self.raw.mnemonic_index)
                .await?
        } else if self.aws {
            let key_id = get_env("AWS_KMS_KEY_ID")?;
            WalletSigner::from_aws(key_id).await?
        } else if self.gcp {
            let project_id = get_env("GCP_PROJECT_ID")?;
            let location = get_env("GCP_LOCATION")?;
            let keyring = get_env("GCP_KEY_RING")?;
            let key_name = get_env("GCP_KEY_NAME")?;
            let key_version = get_env("GCP_KEY_VERSION")?
                .parse()
                .map_err(|_| eyre::eyre!("GCP_KEY_VERSION could not be parsed into u64"))?;
            WalletSigner::from_gcp(project_id, location, keyring, key_name, key_version).await?
        } else if self.turnkey {
            let api_private_key = get_env("TURNKEY_API_PRIVATE_KEY")?;
            let organization_id = get_env("TURNKEY_ORGANIZATION_ID")?;
            let address_str = get_env("TURNKEY_ADDRESS")?;
            let address = address_str.parse().map_err(|_| {
                eyre::eyre!("TURNKEY_ADDRESS could not be parsed as an Ethereum address")
            })?;
            WalletSigner::from_turnkey(api_private_key, organization_id, address)?
        } else if self.browser {
            WalletSigner::from_browser(
                self.browser_port,
                !self.browser_disable_open,
                self.browser_development,
            )
            .await?
        } else if let Some(raw_wallet) = self.raw.signer()? {
            raw_wallet
        } else if let Some(path) = utils::maybe_get_keystore_path(
            self.keystore_path.as_deref(),
            self.keystore_account_name.as_deref(),
        )? {
            let (maybe_signer, maybe_pending) = utils::create_keystore_signer(
                &path,
                self.keystore_password.as_deref(),
                self.keystore_password_file.as_deref(),
            )?;
            if let Some(pending) = maybe_pending {
                pending.unlock()?
            } else if let Some(signer) = maybe_signer {
                signer
            } else {
                unreachable!()
            }
        } else {
            eyre::bail!(
                "\
Error accessing local wallet. Did you pass a keystore, hardware wallet, private key or mnemonic?

Run the command with --help flag for more information or use the corresponding CLI
flag to set your key via:

--keystore
--interactive
--private-key
--mnemonic-path
--aws
--gcp
--turnkey
--trezor
--ledger
--browser

Alternatively, when using the `cast send` or `cast mktx` commands with a local node
or RPC that has unlocked accounts, the --unlocked or --ethsign flags can be used,
respectively. The sender address can be specified by setting the `ETH_FROM` environment
variable to the desired unlocked account address, or by providing the address directly
using the --from flag."
            )
        };

        Ok(signer)
    }

    /// Applies CLI/env Kaspa wallet overrides into IGRA config with highest precedence.
    pub fn apply_igra_kaspa_wallet_overrides(&self, config: &mut Config) {
        if self.kaspa.is_set() {
            config.igra.kaspa_wallet = self.kaspa.as_config();
            return;
        }

        if !config.igra.kaspa_wallet.is_empty() {
            return;
        }

        if let Some(fallback) = self.evm_fallback_kaspa_config() {
            warn!(
                "IGRA Kaspa wallet fallback is reusing EVM signer key material; prefer explicit --private-key-kaspa or --mnemonic-kaspa for key separation"
            );
            config.igra.kaspa_wallet = fallback;
        }
    }

    fn evm_fallback_kaspa_config(&self) -> Option<IgraKaspaWalletConfig> {
        if let Some(private_key) = self.raw.private_key.clone() {
            return Some(IgraKaspaWalletConfig {
                private_key: Some(private_key),
                ..Default::default()
            });
        }

        if let Some(mnemonic) = self.raw.mnemonic.clone() {
            return Some(IgraKaspaWalletConfig {
                mnemonic: Some(mnemonic),
                mnemonic_passphrase: self.raw.mnemonic_passphrase.clone(),
                mnemonic_derivation_path: self.raw.hd_path.clone(),
                mnemonic_index: Some(self.raw.mnemonic_index),
                ..Default::default()
            });
        }

        if self.keystore_path.is_some() || self.keystore_account_name.is_some() {
            return Some(IgraKaspaWalletConfig {
                keystore: self.keystore_path.clone(),
                keystore_account: self.keystore_account_name.clone(),
                password: self.keystore_password.clone(),
                ..Default::default()
            });
        }

        None
    }
}

impl From<RawWalletOpts> for WalletOpts {
    fn from(options: RawWalletOpts) -> Self {
        Self { raw: options, ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_signer::Signer;
    use std::{path::Path, str::FromStr};

    #[tokio::test]
    async fn find_keystore() {
        let keystore =
            Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../cast/tests/fixtures/keystore"));
        let keystore_file = keystore
            .join("UTC--2022-12-20T10-30-43.591916000Z--ec554aeafe75601aaab43bd4621a22284db566c2");
        let password_file = keystore.join("password-ec554");
        let wallet: WalletOpts = WalletOpts::parse_from([
            "foundry-cli",
            "--from",
            "560d246fcddc9ea98a8b032c9a2f474efb493c28",
            "--keystore",
            keystore_file.to_str().unwrap(),
            "--password-file",
            password_file.to_str().unwrap(),
        ]);
        let signer = wallet.signer().await.unwrap();
        assert_eq!(
            signer.address(),
            Address::from_str("ec554aeafe75601aaab43bd4621a22284db566c2").unwrap()
        );
    }

    #[tokio::test]
    async fn illformed_private_key_generates_user_friendly_error() {
        let wallet = WalletOpts {
            raw: RawWalletOpts {
                interactive: false,
                private_key: Some("123".to_string()),
                mnemonic: None,
                mnemonic_passphrase: None,
                hd_path: None,
                mnemonic_index: 0,
            },
            from: None,
            keystore_path: None,
            keystore_account_name: None,
            keystore_password: None,
            keystore_password_file: None,
            ledger: false,
            trezor: false,
            aws: false,
            gcp: false,
            turnkey: false,
            browser: false,
            browser_port: 9545,
            browser_development: false,
            browser_disable_open: false,
            kaspa: KaspaWalletOpts::default(),
        };
        match wallet.signer().await {
            Ok(_) => {
                panic!("illformed private key shouldn't decode")
            }
            Err(x) => {
                assert!(
                    x.to_string().contains("Failed to decode private key"),
                    "Error message is not user-friendly"
                );
            }
        }
    }

    #[test]
    fn parses_kaspa_wallet_opts_from_cli() {
        let wallet: WalletOpts = WalletOpts::parse_from([
            "foundry-cli",
            "--private-key-kaspa",
            "0x1234",
            "--mnemonic-index-kaspa",
            "7",
        ]);

        assert_eq!(wallet.kaspa.private_key_kaspa.as_deref(), Some("0x1234"));
        assert_eq!(wallet.kaspa.mnemonic_index_kaspa, Some(7));
        assert!(wallet.kaspa.is_set());
    }

    #[test]
    fn applies_kaspa_wallet_overrides_to_igra_config() {
        let wallet: WalletOpts = WalletOpts::parse_from([
            "foundry-cli",
            "--private-key-kaspa",
            "0xabcd",
            "--mnemonic-kaspa",
            "test test test test test test test test test test test junk",
            "--mnemonic-derivation-path-kaspa",
            "m/44'/111111'/0'/0/0",
        ]);

        let mut config = Config::default();
        wallet.apply_igra_kaspa_wallet_overrides(&mut config);

        assert_eq!(config.igra.kaspa_wallet.private_key.as_deref(), Some("0xabcd"));
        assert_eq!(
            config.igra.kaspa_wallet.mnemonic.as_deref(),
            Some("test test test test test test test test test test test junk")
        );
        assert_eq!(
            config.igra.kaspa_wallet.mnemonic_derivation_path.as_deref(),
            Some("m/44'/111111'/0'/0/0")
        );
    }

    #[test]
    fn falls_back_to_evm_private_key_when_kaspa_key_missing() {
        let wallet: WalletOpts =
            WalletOpts::parse_from(["foundry-cli", "--private-key", "0x1111111111111111111111111111111111111111111111111111111111111111"]);

        let mut config = Config::default();
        wallet.apply_igra_kaspa_wallet_overrides(&mut config);

        assert_eq!(
            config.igra.kaspa_wallet.private_key.as_deref(),
            Some("0x1111111111111111111111111111111111111111111111111111111111111111")
        );
        assert!(config.igra.kaspa_wallet.mnemonic.is_none());
    }

    #[test]
    fn explicit_kaspa_key_overrides_evm_fallback() {
        let wallet: WalletOpts = WalletOpts::parse_from([
            "foundry-cli",
            "--private-key",
            "0x1111111111111111111111111111111111111111111111111111111111111111",
            "--private-key-kaspa",
            "0x2222222222222222222222222222222222222222222222222222222222222222",
        ]);

        let mut config = Config::default();
        wallet.apply_igra_kaspa_wallet_overrides(&mut config);

        assert_eq!(
            config.igra.kaspa_wallet.private_key.as_deref(),
            Some("0x2222222222222222222222222222222222222222222222222222222222222222")
        );
    }
}
