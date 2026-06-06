//! Configuration for IGRA mode.

use reqwest::Url;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Kaspa signer source configuration for IGRA write-path submission.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgraKaspaWalletConfig {
    /// Kaspa private key in hex format.
    pub private_key: Option<String>,
    /// Kaspa mnemonic phrase.
    pub mnemonic: Option<String>,
    /// Optional BIP39 passphrase for the mnemonic.
    pub mnemonic_passphrase: Option<String>,
    /// Optional derivation path override.
    pub mnemonic_derivation_path: Option<String>,
    /// Mnemonic index override.
    pub mnemonic_index: Option<u32>,
    /// Optional keystore path.
    pub keystore: Option<String>,
    /// Optional keystore account alias.
    pub keystore_account: Option<String>,
    /// Optional keystore password value.
    pub password: Option<String>,
}

impl IgraKaspaWalletConfig {
    /// Returns true when no explicit Kaspa signer source was configured.
    pub fn is_empty(&self) -> bool {
        self.private_key.is_none()
            && self.mnemonic.is_none()
            && self.mnemonic_passphrase.is_none()
            && self.mnemonic_derivation_path.is_none()
            && self.mnemonic_index.is_none()
            && self.keystore.is_none()
            && self.keystore_account.is_none()
            && self.password.is_none()
    }
}

/// IGRA-specific configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgraConfig {
    /// Enables IGRA mode.
    pub enabled: bool,
    /// Execution-layer RPC endpoint URL.
    pub el_rpc_url: Option<String>,
    /// Kaspa RPC endpoint URL.
    pub kaspa_rpc_url: Option<String>,
    /// Expected execution-layer chain ID.
    pub expected_el_chain_id: Option<u64>,
    /// Expected Kaspa network.
    pub kaspa_network: Option<String>,
    /// Prefix for mined Kaspa transaction IDs, encoded as even-length hex.
    pub tx_id_prefix: Option<String>,
    /// Timeout in seconds while waiting for EL receipts.
    pub el_receipt_timeout_secs: Option<u64>,
    /// Timeout in seconds while mining payload nonces for tx-id prefix.
    pub mining_timeout_secs: Option<u64>,
    /// Optional payload compression mode for L2Data inside the Kaspa payload.
    ///
    /// Note: ZippedPayload is not deployed/accepted on galleon testnet at the moment. Keep v1
    /// deterministic by using "none".
    ///
    /// Supported: "none".
    pub payload_compression: Option<String>,
    /// Target IGRA logic zone for raw transaction submission.
    ///
    /// Supported: "canonical" (default), "falcon-l5".
    pub logic_zone: Option<String>,
    /// Kaspa script public key that receives Entry deposits.
    ///
    /// Required by `cast igra-q-entry`; raw q transaction submission does not use it.
    pub entry_lock_script_pubkey: Option<String>,
    /// Sender lock timeout in seconds.
    pub sender_lock_timeout_secs: Option<u64>,
    /// Retention period for completed IGRA tx-map entries.
    pub completed_retention_hours: Option<u64>,
    /// Retention period for failed IGRA tx-map entries.
    pub failed_retention_hours: Option<u64>,
    /// Max size of IGRA tx-map database in MB.
    pub max_db_size_mb: Option<u64>,
    /// Optional Kaspa signer source configuration for in-process submission.
    #[serde(default)]
    pub kaspa_wallet: IgraKaspaWalletConfig,
}

impl IgraConfig {
    /// Validates the IGRA configuration when enabled.
    pub fn validate(&self) -> Result<(), IgraConfigError> {
        if !self.enabled {
            return Ok(());
        }

        let el_rpc_url = required_str("el_rpc_url", self.el_rpc_url.as_deref())?;
        validate_url("el_rpc_url", el_rpc_url)?;
        validate_scheme("el_rpc_url", el_rpc_url, &["http", "https", "ws", "wss"])?;

        let kaspa_rpc_url = required_str("kaspa_rpc_url", self.kaspa_rpc_url.as_deref())?;
        validate_url("kaspa_rpc_url", kaspa_rpc_url)?;
        validate_scheme("kaspa_rpc_url", kaspa_rpc_url, &["grpc", "grpcs", "http", "https"])?;

        let chain_id = self
            .expected_el_chain_id
            .ok_or(IgraConfigError::Missing { field: "expected_el_chain_id" })?;
        if chain_id == 0 {
            return Err(IgraConfigError::Invalid {
                field: "expected_el_chain_id",
                reason: "must be greater than zero".to_string(),
            });
        }

        let kaspa_network = required_str("kaspa_network", self.kaspa_network.as_deref())?;
        if !matches!(kaspa_network, "mainnet" | "testnet-10" | "devnet" | "simnet" | "custom") {
            return Err(IgraConfigError::Invalid {
                field: "kaspa_network",
                reason: format!(
                    "unsupported network `{kaspa_network}` (expected one of mainnet, testnet-10, \
                     devnet, simnet, custom)"
                ),
            });
        }

        let tx_id_prefix = required_str("tx_id_prefix", self.tx_id_prefix.as_deref())?;
        if tx_id_prefix.len() % 2 != 0 {
            return Err(IgraConfigError::Invalid {
                field: "tx_id_prefix",
                reason: "must have an even number of hex characters".to_string(),
            });
        }
        if !tx_id_prefix.as_bytes().iter().all(u8::is_ascii_hexdigit) {
            return Err(IgraConfigError::Invalid {
                field: "tx_id_prefix",
                reason: "must be hex-encoded".to_string(),
            });
        }

        let timeout = self
            .el_receipt_timeout_secs
            .ok_or(IgraConfigError::Missing { field: "el_receipt_timeout_secs" })?;
        if timeout == 0 {
            return Err(IgraConfigError::Invalid {
                field: "el_receipt_timeout_secs",
                reason: "must be greater than zero".to_string(),
            });
        }

        validate_positive_opt("sender_lock_timeout_secs", self.sender_lock_timeout_secs)?;
        validate_positive_opt("mining_timeout_secs", self.mining_timeout_secs)?;
        validate_positive_opt("completed_retention_hours", self.completed_retention_hours)?;
        validate_positive_opt("failed_retention_hours", self.failed_retention_hours)?;
        validate_positive_opt("max_db_size_mb", self.max_db_size_mb)?;

        if let Some(mode) = self.payload_compression.as_deref() {
            let mode = mode.trim().to_ascii_lowercase();
            if matches!(mode.as_str(), "zlib") {
                return Err(IgraConfigError::Invalid {
                    field: "payload_compression",
                    reason: "zlib is not implemented; use `none`".to_string(),
                });
            }
            if !matches!(mode.as_str(), "" | "none") {
                return Err(IgraConfigError::Invalid {
                    field: "payload_compression",
                    reason: "supported values: none".to_string(),
                });
            }
        }

        if let Some(logic_zone) = self.logic_zone.as_deref() {
            let logic_zone = logic_zone.trim().to_ascii_lowercase();
            if !matches!(logic_zone.as_str(), "" | "canonical" | "falcon-l5") {
                return Err(IgraConfigError::Invalid {
                    field: "logic_zone",
                    reason: "supported values: canonical, falcon-l5".to_string(),
                });
            }
        }

        if let Some(script) = self.entry_lock_script_pubkey.as_deref() {
            validate_hex_string("entry_lock_script_pubkey", script)?;
        }

        Ok(())
    }
}

fn required_str<'a>(
    field: &'static str,
    value: Option<&'a str>,
) -> Result<&'a str, IgraConfigError> {
    let value = value.ok_or(IgraConfigError::Missing { field })?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(IgraConfigError::Missing { field });
    }
    Ok(trimmed)
}

fn validate_url(field: &'static str, value: &str) -> Result<(), IgraConfigError> {
    Url::parse(value)
        .map_err(|err| IgraConfigError::Invalid { field, reason: format!("invalid URL: {err}") })?;
    Ok(())
}

fn validate_scheme(
    field: &'static str,
    value: &str,
    allowed_schemes: &[&str],
) -> Result<(), IgraConfigError> {
    let parsed = Url::parse(value)
        .map_err(|err| IgraConfigError::Invalid { field, reason: format!("invalid URL: {err}") })?;
    if !allowed_schemes.contains(&parsed.scheme()) {
        return Err(IgraConfigError::Invalid {
            field,
            reason: format!(
                "unsupported URL scheme `{}` (expected one of {})",
                parsed.scheme(),
                allowed_schemes.join(", ")
            ),
        });
    }
    Ok(())
}

fn validate_positive_opt(field: &'static str, value: Option<u64>) -> Result<(), IgraConfigError> {
    if value == Some(0) {
        return Err(IgraConfigError::Invalid {
            field,
            reason: "must be greater than zero".to_string(),
        });
    }
    Ok(())
}

fn validate_hex_string(field: &'static str, value: &str) -> Result<(), IgraConfigError> {
    let value = value.trim().trim_start_matches("0x");
    if value.is_empty() {
        return Err(IgraConfigError::Invalid { field, reason: "must not be empty".to_string() });
    }
    if value.len() % 2 != 0 {
        return Err(IgraConfigError::Invalid {
            field,
            reason: "must have an even number of hex characters".to_string(),
        });
    }
    if !value.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return Err(IgraConfigError::Invalid { field, reason: "must be hex-encoded".to_string() });
    }
    Ok(())
}

/// IGRA config validation error.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum IgraConfigError {
    /// A required field is missing while IGRA mode is enabled.
    #[error("IGRA config error: `{field}` is required when `igra.enabled=true`")]
    Missing { field: &'static str },
    /// A field is present but invalid.
    #[error("IGRA config error: `{field}` is invalid: {reason}")]
    Invalid { field: &'static str, reason: String },
}

#[cfg(test)]
mod tests {
    use super::{IgraConfig, IgraKaspaWalletConfig};

    fn valid_config() -> IgraConfig {
        IgraConfig {
            enabled: true,
            el_rpc_url: Some("http://127.0.0.1:8545".to_string()),
            kaspa_rpc_url: Some("grpc://127.0.0.1:16110".to_string()),
            expected_el_chain_id: Some(1337),
            kaspa_network: Some("testnet-10".to_string()),
            tx_id_prefix: Some("97b1".to_string()),
            el_receipt_timeout_secs: Some(300),
            mining_timeout_secs: Some(120),
            payload_compression: None,
            logic_zone: None,
            entry_lock_script_pubkey: None,
            sender_lock_timeout_secs: Some(60),
            completed_retention_hours: Some(168),
            failed_retention_hours: Some(720),
            max_db_size_mb: Some(512),
            kaspa_wallet: IgraKaspaWalletConfig::default(),
        }
    }

    #[test]
    fn validate_accepts_valid_config() {
        let config = valid_config();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_invalid_prefix() {
        let mut config = valid_config();
        config.tx_id_prefix = Some("zz".to_string());

        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("IGRA config error"));
        assert!(err.contains("tx_id_prefix"));
    }

    #[test]
    fn validate_rejects_odd_length_prefix() {
        let mut config = valid_config();
        config.tx_id_prefix = Some("abc".to_string());

        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("IGRA config error"));
        assert!(err.contains("even number of hex"));
    }

    #[test]
    fn validate_rejects_unsupported_kaspa_scheme() {
        let mut config = valid_config();
        config.kaspa_rpc_url = Some("ws://127.0.0.1:16110".to_string());

        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("IGRA config error"));
        assert!(err.contains("unsupported URL scheme"));
    }

    #[test]
    fn validate_rejects_missing_required_field() {
        let mut config = valid_config();
        config.el_rpc_url = None;

        let err = config.validate().unwrap_err().to_string();
        assert_eq!(err, "IGRA config error: `el_rpc_url` is required when `igra.enabled=true`");
    }

    #[test]
    fn validate_rejects_zero_chain_id() {
        let mut config = valid_config();
        config.expected_el_chain_id = Some(0);

        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("expected_el_chain_id"));
        assert!(err.contains("must be greater than zero"));
    }

    #[test]
    fn validate_rejects_zero_receipt_timeout() {
        let mut config = valid_config();
        config.el_receipt_timeout_secs = Some(0);

        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("el_receipt_timeout_secs"));
        assert!(err.contains("must be greater than zero"));
    }

    #[test]
    fn validate_rejects_whitespace_strings_as_missing() {
        let mut config = valid_config();
        config.kaspa_network = Some("   ".to_string());

        let err = config.validate().unwrap_err().to_string();
        assert_eq!(err, "IGRA config error: `kaspa_network` is required when `igra.enabled=true`");
    }

    #[test]
    fn validate_accepts_all_supported_networks() {
        for network in ["mainnet", "testnet-10", "devnet", "simnet", "custom"] {
            let mut config = valid_config();
            config.kaspa_network = Some(network.to_string());
            assert!(config.validate().is_ok(), "network {network} should be accepted");
        }
    }

    #[test]
    fn validate_rejects_zero_cache_constraints() {
        for field in [
            "sender_lock_timeout_secs",
            "mining_timeout_secs",
            "completed_retention_hours",
            "failed_retention_hours",
            "max_db_size_mb",
        ] {
            let mut config = valid_config();
            match field {
                "sender_lock_timeout_secs" => config.sender_lock_timeout_secs = Some(0),
                "mining_timeout_secs" => config.mining_timeout_secs = Some(0),
                "completed_retention_hours" => config.completed_retention_hours = Some(0),
                "failed_retention_hours" => config.failed_retention_hours = Some(0),
                "max_db_size_mb" => config.max_db_size_mb = Some(0),
                _ => unreachable!(),
            }

            let err = config.validate().unwrap_err().to_string();
            assert!(err.contains(field), "expected field `{field}` in error: {err}");
        }
    }

    #[test]
    fn validate_rejects_invalid_payload_compression() {
        let mut config = valid_config();
        config.payload_compression = Some("brotli".to_string());

        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("payload_compression"));
        assert!(err.contains("supported values"));
    }

    #[test]
    fn validate_accepts_supported_logic_zones() {
        for logic_zone in [None, Some("canonical"), Some("falcon-l5")] {
            let mut config = valid_config();
            config.logic_zone = logic_zone.map(str::to_string);
            assert!(config.validate().is_ok(), "logic_zone {logic_zone:?} should be accepted");
        }
    }

    #[test]
    fn validate_rejects_invalid_logic_zone() {
        let mut config = valid_config();
        config.logic_zone = Some("ml-dsa".to_string());

        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("logic_zone"));
        assert!(err.contains("supported values"));
    }
}
