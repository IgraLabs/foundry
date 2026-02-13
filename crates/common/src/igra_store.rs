//! IGRA persistence primitives backed by SQLite.

use alloy_primitives::{hex, utils::keccak256};
use eyre::{Context, Result};
use foundry_config::Config;
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

/// Default sender lock timeout in seconds.
pub const DEFAULT_SENDER_LOCK_TIMEOUT_SECS: u64 = 60;
/// Error code returned for nonce-gap blocking.
pub const IGRA_NONCE_GAP_ERROR_CODE: &str = "IGRA_NONCE_001";
/// Error code returned for sender-lock timeout blocking.
pub const IGRA_NONCE_LOCK_TIMEOUT_ERROR_CODE: &str = "IGRA_NONCE_002";
/// Error code returned for nonce advancement overflow.
pub const IGRA_NONCE_OVERFLOW_ERROR_CODE: &str = "IGRA_NONCE_003";
/// Error code returned when a stale nonce is classified as a replacement candidate.
pub const IGRA_NONCE_REPLACEMENT_CANDIDATE_ERROR_CODE: &str = "IGRA_NONCE_004";

const SCHEMA_VERSION_KEY: &str = "tx_map_schema_version";
const SCHEMA_VERSION_VALUE: &str = "1";
const PROFILE_CHAIN_ID_KEY: &str = "profile.expected_el_chain_id";
const PROFILE_KASPA_NETWORK_KEY: &str = "profile.kaspa_network";
const PROFILE_EL_RPC_FINGERPRINT_KEY: &str = "profile.el_rpc_fingerprint";
const PROFILE_KASPA_RPC_FINGERPRINT_KEY: &str = "profile.kaspa_rpc_fingerprint";
const DEFAULT_COMPLETED_RETENTION_HOURS: u64 = 168;
const DEFAULT_FAILED_RETENTION_HOURS: u64 = 720;
const DEFAULT_MAX_DB_SIZE_MB: u64 = 512;
const SQLITE_BUSY_TIMEOUT_MS: u64 = 5_000;
const LOCK_POLL_INTERVAL_MS: u64 = 25;

/// IGRA lifecycle state persisted in `tx_map`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxLifecycleState {
    ReceivedRawL2,
    KaspaUnsignedCreated,
    KaspaPrefixMined,
    KaspaSigned,
    KaspaBroadcasted,
    FailedRecoverable,
    BlockedNonceGap,
    BlockedLockTimeout,
    StaleReplacementCandidate,
}

impl TxLifecycleState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReceivedRawL2 => "RECEIVED_RAW_L2",
            Self::KaspaUnsignedCreated => "KASPA_UNSIGNED_CREATED",
            Self::KaspaPrefixMined => "KASPA_PREFIX_MINED",
            Self::KaspaSigned => "KASPA_SIGNED",
            Self::KaspaBroadcasted => "KASPA_BROADCASTED",
            Self::FailedRecoverable => "FAILED_RECOVERABLE",
            Self::BlockedNonceGap => "BLOCKED_NONCE_GAP",
            Self::BlockedLockTimeout => "BLOCKED_LOCK_TIMEOUT",
            Self::StaleReplacementCandidate => "STALE_REPLACEMENT_CANDIDATE",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "RECEIVED_RAW_L2" => Some(Self::ReceivedRawL2),
            "KASPA_UNSIGNED_CREATED" => Some(Self::KaspaUnsignedCreated),
            "KASPA_PREFIX_MINED" => Some(Self::KaspaPrefixMined),
            "KASPA_SIGNED" => Some(Self::KaspaSigned),
            "KASPA_BROADCASTED" => Some(Self::KaspaBroadcasted),
            "FAILED_RECOVERABLE" => Some(Self::FailedRecoverable),
            "BLOCKED_NONCE_GAP" => Some(Self::BlockedNonceGap),
            "BLOCKED_LOCK_TIMEOUT" => Some(Self::BlockedLockTimeout),
            "STALE_REPLACEMENT_CANDIDATE" => Some(Self::StaleReplacementCandidate),
            _ => None,
        }
    }
}

/// IGRA store configuration.
#[derive(Clone, Debug, Default)]
pub struct IgraStoreConfig {
    pub db_path: Option<PathBuf>,
    pub kaspa_network: Option<String>,
    pub expected_el_chain_id: Option<u64>,
    pub el_rpc_url: Option<String>,
    pub kaspa_rpc_url: Option<String>,
    pub sender_lock_timeout_secs: Option<u64>,
    pub completed_retention_hours: Option<u64>,
    pub failed_retention_hours: Option<u64>,
    pub max_db_size_mb: Option<u64>,
}

/// Nonce ordering classification for an incoming tx.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonceOrdering {
    InOrder { expected: u64 },
    Stale { expected: u64, replacement_candidate: bool },
}

/// Record for `tx_map`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxMapRecord {
    pub l2_tx_hash: String,
    pub sender: String,
    pub l2_nonce: u64,
    pub payload_nonce: Option<u64>,
    pub kaspa_tx_id: Option<String>,
    pub state: TxLifecycleState,
    pub chain_id: u64,
    pub kaspa_network: String,
    pub el_rpc_fingerprint: Option<String>,
    pub kaspa_rpc_fingerprint: Option<String>,
    pub correlation_id: String,
    pub attempts: u32,
    pub last_error_code: Option<String>,
    pub last_error_message: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Payload for updating tx lifecycle state.
#[derive(Clone, Debug)]
pub struct TxLifecycleUpdate {
    pub l2_tx_hash: String,
    pub sender: String,
    pub l2_nonce: u64,
    pub payload_nonce: Option<u64>,
    pub kaspa_tx_id: Option<String>,
    pub state: TxLifecycleState,
    pub correlation_id: String,
    pub last_error_code: Option<String>,
    pub last_error_message: Option<String>,
    pub increment_attempts: bool,
}

/// Errors emitted by the IGRA store.
#[derive(Debug, Error)]
pub enum IgraStoreError {
    #[error("{IGRA_NONCE_GAP_ERROR_CODE}: BLOCKED_NONCE_GAP (expected {expected}, got {got})")]
    BlockedNonceGap { expected: u64, got: u64 },
    #[error("{IGRA_NONCE_LOCK_TIMEOUT_ERROR_CODE}: sender lock timed out for {sender}")]
    LockTimeout { sender: String },
    #[error(
        "{IGRA_NONCE_OVERFLOW_ERROR_CODE}: next expected nonce overflow for sender {sender} at nonce {nonce}"
    )]
    NonceOverflow { sender: String, nonce: u64 },
    #[error("IGRA store SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("IGRA store I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl IgraStoreError {
    pub const fn code(&self) -> Option<&'static str> {
        match self {
            Self::BlockedNonceGap { .. } => Some(IGRA_NONCE_GAP_ERROR_CODE),
            Self::LockTimeout { .. } => Some(IGRA_NONCE_LOCK_TIMEOUT_ERROR_CODE),
            Self::NonceOverflow { .. } => Some(IGRA_NONCE_OVERFLOW_ERROR_CODE),
            Self::Sqlite(_) | Self::Io(_) => None,
        }
    }
}

/// SQLite-backed store for IGRA tx lifecycle and sender nonce/lock state.
#[derive(Clone, Debug)]
pub struct IgraStore {
    db_path: PathBuf,
    kaspa_network: String,
    expected_el_chain_id: u64,
    sender_lock_timeout_secs: u64,
    el_rpc_fingerprint: Option<String>,
    kaspa_rpc_fingerprint: Option<String>,
    completed_retention_hours: u64,
    failed_retention_hours: u64,
    max_db_size_mb: u64,
}

impl IgraStore {
    pub fn new(config: IgraStoreConfig) -> Result<Self> {
        Self::from_config(config, true)
    }

    pub fn open_read_only(config: IgraStoreConfig) -> Result<Self> {
        Self::from_config(config, false)
    }

    fn from_config(config: IgraStoreConfig, initialize_cache: bool) -> Result<Self> {
        let kaspa_network =
            sanitize_component(config.kaspa_network.as_deref().unwrap_or("unknown"), "unknown");
        let expected_el_chain_id = config.expected_el_chain_id.unwrap_or(0);
        let sender_lock_timeout_secs =
            config.sender_lock_timeout_secs.unwrap_or(DEFAULT_SENDER_LOCK_TIMEOUT_SECS);
        let completed_retention_hours =
            config.completed_retention_hours.unwrap_or(DEFAULT_COMPLETED_RETENTION_HOURS);
        let failed_retention_hours =
            config.failed_retention_hours.unwrap_or(DEFAULT_FAILED_RETENTION_HOURS);
        let max_db_size_mb = config.max_db_size_mb.unwrap_or(DEFAULT_MAX_DB_SIZE_MB);
        let db_path =
            config.db_path.unwrap_or_else(|| default_db_path(&kaspa_network, expected_el_chain_id));

        if initialize_cache && let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create IGRA DB directory `{}`", parent.display())
            })?;
        }

        let store = Self {
            db_path,
            kaspa_network,
            expected_el_chain_id,
            sender_lock_timeout_secs,
            el_rpc_fingerprint: fingerprint(
                config.el_rpc_url.as_deref(),
                expected_el_chain_id,
                config.kaspa_network.as_deref(),
            ),
            kaspa_rpc_fingerprint: fingerprint(
                config.kaspa_rpc_url.as_deref(),
                expected_el_chain_id,
                config.kaspa_network.as_deref(),
            ),
            completed_retention_hours,
            failed_retention_hours,
            max_db_size_mb,
        };
        if initialize_cache {
            store.migrate()?;
            store.reconcile_profile_cache()?;
            store.prune_cache()?;
        }
        Ok(store)
    }

    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }

    pub const fn expected_el_chain_id(&self) -> u64 {
        self.expected_el_chain_id
    }

    pub fn kaspa_network(&self) -> &str {
        &self.kaspa_network
    }

    pub const fn sender_lock_timeout_secs(&self) -> u64 {
        self.sender_lock_timeout_secs
    }

    pub fn acquire_sender_lock(&self, sender: &str, owner_id: &str) -> Result<(), IgraStoreError> {
        let mut conn = self.open_connection()?;
        let timeout_ms = self.sender_lock_timeout_secs.saturating_mul(1000);
        let deadline_ms = now_ms().saturating_add(timeout_ms);
        // The lease should outlive the acquisition timeout. Otherwise, a contending sender could
        // "wait out" the lease and succeed within the same call, which breaks determinism.
        let lease_ms = timeout_ms.saturating_mul(2);

        loop {
            let now = now_ms();
            if now >= deadline_ms {
                return Err(IgraStoreError::LockTimeout { sender: sender.to_string() });
            }

            match self.try_acquire_sender_lock_once(&mut conn, sender, owner_id, now, lease_ms) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(err) if is_retryable_busy_error(&err) => {}
                Err(err) => return Err(err),
            }

            thread::sleep(Duration::from_millis(LOCK_POLL_INTERVAL_MS));
        }
    }

    pub fn release_sender_lock(&self, sender: &str, owner_id: &str) -> Result<(), IgraStoreError> {
        let conn = self.open_connection()?;
        conn.execute(
            "DELETE FROM sender_locks WHERE sender = ?1 AND owner_id = ?2",
            params![sender, owner_id],
        )?;
        Ok(())
    }

    pub fn acquire_sender_lock_and_classify_nonce(
        &self,
        sender: &str,
        owner_id: &str,
        nonce: u64,
        l2_tx_hash: &str,
    ) -> Result<NonceOrdering, IgraStoreError> {
        let mut conn = self.open_connection()?;
        let timeout_ms = self.sender_lock_timeout_secs.saturating_mul(1000);
        let deadline_ms = now_ms().saturating_add(timeout_ms);
        // Keep lease > timeout; see `acquire_sender_lock`.
        let lease_ms = timeout_ms.saturating_mul(2);

        loop {
            let now = now_ms();
            if now >= deadline_ms {
                return Err(IgraStoreError::LockTimeout { sender: sender.to_string() });
            }

            match self.try_acquire_sender_lock_and_classify_once(
                &mut conn, sender, owner_id, nonce, l2_tx_hash, now, lease_ms,
            ) {
                Ok(Some(ordering)) => return Ok(ordering),
                Ok(None) => {}
                Err(err) if is_retryable_busy_error(&err) => {}
                Err(err) => return Err(err),
            }

            thread::sleep(Duration::from_millis(LOCK_POLL_INTERVAL_MS));
        }
    }

    pub fn classify_nonce(
        &self,
        sender: &str,
        nonce: u64,
    ) -> Result<NonceOrdering, IgraStoreError> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ordering = self.classify_nonce_in_tx(&tx, sender, nonce, None)?;
        tx.commit()?;
        Ok(ordering)
    }

    pub fn mark_submitted_in_order_nonce(
        &self,
        sender: &str,
        nonce: u64,
    ) -> Result<(), IgraStoreError> {
        let next_nonce = nonce
            .checked_add(1)
            .ok_or_else(|| IgraStoreError::NonceOverflow { sender: sender.to_string(), nonce })?;
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();

        let expected: Option<i64> = tx
            .query_row(
                "SELECT next_expected_nonce FROM sender_nonce_state WHERE sender = ?1",
                params![sender],
                |row| row.get(0),
            )
            .optional()?;

        match expected {
            Some(current) if current as u64 == nonce => {
                tx.execute(
                    "UPDATE sender_nonce_state
                     SET next_expected_nonce = ?2, updated_at_ms = ?3
                     WHERE sender = ?1",
                    params![sender, next_nonce as i64, now as i64],
                )?;
            }
            None => {
                tx.execute(
                    "INSERT INTO sender_nonce_state (sender, next_expected_nonce, updated_at_ms)
                     VALUES (?1, ?2, ?3)",
                    params![sender, next_nonce as i64, now as i64],
                )?;
            }
            Some(_) => {}
        }

        tx.commit()?;
        Ok(())
    }

    pub fn persist_transition(&self, update: &TxLifecycleUpdate) -> Result<(), IgraStoreError> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let existing: Option<(u32, u64)> = tx
            .query_row(
                "SELECT attempts, created_at_ms FROM tx_map WHERE l2_tx_hash = ?1",
                params![update.l2_tx_hash],
                |row| Ok((row.get::<_, i64>(0)? as u32, row.get::<_, i64>(1)? as u64)),
            )
            .optional()?;
        let now = now_ms();
        let (attempts, created_at_ms) = match existing {
            Some((existing_attempts, created_at)) => {
                (existing_attempts.saturating_add(u32::from(update.increment_attempts)), created_at)
            }
            None => (u32::from(update.increment_attempts), now),
        };

        tx.execute(
            "INSERT INTO tx_map (
                l2_tx_hash,
                sender,
                l2_nonce,
                payload_nonce,
                kaspa_tx_id,
                state,
                chain_id,
                kaspa_network,
                el_rpc_fingerprint,
                kaspa_rpc_fingerprint,
                correlation_id,
                attempts,
                last_error_code,
                last_error_message,
                created_at_ms,
                updated_at_ms
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16
            )
            ON CONFLICT(l2_tx_hash) DO UPDATE SET
                sender = excluded.sender,
                l2_nonce = excluded.l2_nonce,
                payload_nonce = excluded.payload_nonce,
                kaspa_tx_id = excluded.kaspa_tx_id,
                state = excluded.state,
                chain_id = excluded.chain_id,
                kaspa_network = excluded.kaspa_network,
                el_rpc_fingerprint = excluded.el_rpc_fingerprint,
                kaspa_rpc_fingerprint = excluded.kaspa_rpc_fingerprint,
                correlation_id = excluded.correlation_id,
                attempts = excluded.attempts,
                last_error_code = excluded.last_error_code,
                last_error_message = excluded.last_error_message,
                updated_at_ms = excluded.updated_at_ms",
            params![
                update.l2_tx_hash,
                update.sender,
                update.l2_nonce as i64,
                update.payload_nonce.map(|value| value as i64),
                update.kaspa_tx_id,
                update.state.as_str(),
                self.expected_el_chain_id as i64,
                self.kaspa_network,
                self.el_rpc_fingerprint,
                self.kaspa_rpc_fingerprint,
                update.correlation_id,
                attempts as i64,
                update.last_error_code,
                update.last_error_message,
                created_at_ms as i64,
                now as i64,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_tx(&self, l2_tx_hash: &str) -> Result<Option<TxMapRecord>, IgraStoreError> {
        if !self.db_path.exists() {
            return Ok(None);
        }
        let conn = self.open_connection()?;
        let row = conn
            .query_row(
                "SELECT
                    l2_tx_hash,
                    sender,
                    l2_nonce,
                    payload_nonce,
                    kaspa_tx_id,
                    state,
                    chain_id,
                    kaspa_network,
                    el_rpc_fingerprint,
                    kaspa_rpc_fingerprint,
                    correlation_id,
                    attempts,
                    last_error_code,
                    last_error_message,
                    created_at_ms,
                    updated_at_ms
                 FROM tx_map WHERE l2_tx_hash = ?1",
                params![l2_tx_hash],
                |row| {
                    let state_raw: String = row.get(5)?;
                    let state = TxLifecycleState::parse(&state_raw).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            format!("unknown state `{state_raw}`").into(),
                        )
                    })?;
                    Ok(TxMapRecord {
                        l2_tx_hash: row.get(0)?,
                        sender: row.get(1)?,
                        l2_nonce: row.get::<_, i64>(2)? as u64,
                        payload_nonce: row.get::<_, Option<i64>>(3)?.map(|value| value as u64),
                        kaspa_tx_id: row.get(4)?,
                        state,
                        chain_id: row.get::<_, i64>(6)? as u64,
                        kaspa_network: row.get(7)?,
                        el_rpc_fingerprint: row.get(8)?,
                        kaspa_rpc_fingerprint: row.get(9)?,
                        correlation_id: row.get(10)?,
                        attempts: row.get::<_, i64>(11)? as u32,
                        last_error_code: row.get(12)?,
                        last_error_message: row.get(13)?,
                        created_at_ms: row.get::<_, i64>(14)? as u64,
                        updated_at_ms: row.get::<_, i64>(15)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    fn classify_nonce_in_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        sender: &str,
        nonce: u64,
        l2_tx_hash: Option<&str>,
    ) -> Result<NonceOrdering, IgraStoreError> {
        let now = now_ms();
        let expected: Option<i64> = tx
            .query_row(
                "SELECT next_expected_nonce FROM sender_nonce_state WHERE sender = ?1",
                params![sender],
                |row| row.get(0),
            )
            .optional()?;

        let expected_u64 = match expected {
            Some(value) => value as u64,
            None => {
                tx.execute(
                    "INSERT INTO sender_nonce_state (sender, next_expected_nonce, updated_at_ms)
                     VALUES (?1, ?2, ?3)",
                    params![sender, nonce as i64, now as i64],
                )?;
                nonce
            }
        };

        if nonce > expected_u64 {
            return Err(IgraStoreError::BlockedNonceGap { expected: expected_u64, got: nonce });
        }

        if nonce == expected_u64 {
            return Ok(NonceOrdering::InOrder { expected: expected_u64 });
        }

        let replacement_candidate = if let Some(incoming_hash) = l2_tx_hash {
            self.is_stale_replacement_candidate(tx, sender, nonce, incoming_hash)?
        } else {
            false
        };
        Ok(NonceOrdering::Stale { expected: expected_u64, replacement_candidate })
    }

    fn try_acquire_sender_lock_once(
        &self,
        conn: &mut Connection,
        sender: &str,
        owner_id: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<bool, IgraStoreError> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT owner_id, expires_at_ms FROM sender_locks WHERE sender = ?1",
                params![sender],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        let can_acquire = match existing {
            None => true,
            Some((existing_owner, expires_at_ms)) => {
                existing_owner == owner_id || (expires_at_ms as u64) < now_ms
            }
        };

        if can_acquire {
            let expires_at_ms = now_ms.saturating_add(lease_ms);
            tx.execute(
                "INSERT INTO sender_locks (sender, owner_id, expires_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(sender) DO UPDATE SET
                    owner_id = excluded.owner_id,
                    expires_at_ms = excluded.expires_at_ms,
                    updated_at_ms = excluded.updated_at_ms",
                params![sender, owner_id, expires_at_ms as i64, now_ms as i64],
            )?;
        }

        tx.commit()?;
        Ok(can_acquire)
    }

    fn try_acquire_sender_lock_and_classify_once(
        &self,
        conn: &mut Connection,
        sender: &str,
        owner_id: &str,
        nonce: u64,
        l2_tx_hash: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<NonceOrdering>, IgraStoreError> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT owner_id, expires_at_ms FROM sender_locks WHERE sender = ?1",
                params![sender],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        let can_acquire = match existing {
            None => true,
            Some((existing_owner, expires_at_ms)) => {
                existing_owner == owner_id || (expires_at_ms as u64) < now_ms
            }
        };

        if !can_acquire {
            tx.commit()?;
            return Ok(None);
        }

        let expires_at_ms = now_ms.saturating_add(lease_ms);
        tx.execute(
            "INSERT INTO sender_locks (sender, owner_id, expires_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(sender) DO UPDATE SET
                owner_id = excluded.owner_id,
                expires_at_ms = excluded.expires_at_ms,
                updated_at_ms = excluded.updated_at_ms",
            params![sender, owner_id, expires_at_ms as i64, now_ms as i64],
        )?;

        let ordering = self.classify_nonce_in_tx(&tx, sender, nonce, Some(l2_tx_hash))?;
        tx.commit()?;
        Ok(Some(ordering))
    }

    fn is_stale_replacement_candidate(
        &self,
        tx: &rusqlite::Transaction<'_>,
        sender: &str,
        nonce: u64,
        incoming_l2_tx_hash: &str,
    ) -> Result<bool, IgraStoreError> {
        let has_other_hash: Option<i64> = tx
            .query_row(
                "SELECT 1
                 FROM tx_map
                 WHERE sender = ?1 AND l2_nonce = ?2 AND l2_tx_hash != ?3
                 LIMIT 1",
                params![sender, nonce as i64, incoming_l2_tx_hash],
                |row| row.get(0),
            )
            .optional()?;
        Ok(has_other_hash.is_some())
    }

    fn reconcile_profile_cache(&self) -> Result<(), IgraStoreError> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction()?;

        let expected_chain_id = self.expected_el_chain_id.to_string();
        let expected_network = self.kaspa_network.as_str();
        let existing_chain_id = Self::metadata_value(&tx, PROFILE_CHAIN_ID_KEY)?;
        let existing_network = Self::metadata_value(&tx, PROFILE_KASPA_NETWORK_KEY)?;
        let existing_el_rpc_fingerprint =
            Self::metadata_value(&tx, PROFILE_EL_RPC_FINGERPRINT_KEY)?;
        let existing_kaspa_rpc_fingerprint =
            Self::metadata_value(&tx, PROFILE_KASPA_RPC_FINGERPRINT_KEY)?;

        let profile_changed =
            existing_chain_id.as_deref().is_some_and(|value| value != expected_chain_id)
                || existing_network.as_deref().is_some_and(|value| value != expected_network)
                || Self::metadata_changed(
                    existing_el_rpc_fingerprint.as_deref(),
                    self.el_rpc_fingerprint.as_deref(),
                )
                || Self::metadata_changed(
                    existing_kaspa_rpc_fingerprint.as_deref(),
                    self.kaspa_rpc_fingerprint.as_deref(),
                );

        if profile_changed {
            tx.execute("DELETE FROM tx_map", [])?;
            tx.execute("DELETE FROM sender_nonce_state", [])?;
            tx.execute("DELETE FROM sender_locks", [])?;
        }

        Self::set_metadata_value(&tx, PROFILE_CHAIN_ID_KEY, Some(&expected_chain_id))?;
        Self::set_metadata_value(&tx, PROFILE_KASPA_NETWORK_KEY, Some(expected_network))?;
        Self::set_metadata_value(
            &tx,
            PROFILE_EL_RPC_FINGERPRINT_KEY,
            self.el_rpc_fingerprint.as_deref(),
        )?;
        Self::set_metadata_value(
            &tx,
            PROFILE_KASPA_RPC_FINGERPRINT_KEY,
            self.kaspa_rpc_fingerprint.as_deref(),
        )?;

        tx.commit()?;
        Ok(())
    }

    fn prune_cache(&self) -> Result<(), IgraStoreError> {
        let conn = self.open_connection()?;
        let now = now_ms() as i64;
        let completed_cutoff =
            now.saturating_sub((self.completed_retention_hours as i64).saturating_mul(3_600_000));
        let failed_cutoff =
            now.saturating_sub((self.failed_retention_hours as i64).saturating_mul(3_600_000));

        conn.execute(
            "DELETE FROM tx_map
             WHERE state IN ('KASPA_BROADCASTED')
               AND updated_at_ms < ?1",
            params![completed_cutoff],
        )?;
        conn.execute(
            "DELETE FROM tx_map
             WHERE state IN ('FAILED_RECOVERABLE', 'BLOCKED_NONCE_GAP', 'BLOCKED_LOCK_TIMEOUT')
               AND updated_at_ms < ?1",
            params![failed_cutoff],
        )?;

        self.enforce_size_budget(&conn)?;
        Ok(())
    }

    fn enforce_size_budget(&self, conn: &Connection) -> Result<(), IgraStoreError> {
        if self.max_db_size_mb == 0 {
            return Ok(());
        }

        let max_bytes = self.max_db_size_mb.saturating_mul(1024 * 1024);
        let current_size = fs::metadata(&self.db_path).map(|meta| meta.len()).unwrap_or_default();
        if current_size <= max_bytes {
            return Ok(());
        }

        loop {
            let current_size =
                fs::metadata(&self.db_path).map(|meta| meta.len()).unwrap_or_default();
            if current_size <= max_bytes {
                break;
            }

            let deleted = conn.execute(
                "DELETE FROM tx_map
                 WHERE l2_tx_hash IN (
                    SELECT l2_tx_hash FROM tx_map
                    WHERE state IN (
                        'FAILED_RECOVERABLE',
                        'BLOCKED_NONCE_GAP',
                        'BLOCKED_LOCK_TIMEOUT',
                        'STALE_REPLACEMENT_CANDIDATE',
                        'KASPA_BROADCASTED'
                    )
                    ORDER BY updated_at_ms ASC
                    LIMIT 1024
                 )",
                [],
            )?;
            if deleted == 0 {
                break;
            }
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        }
        Ok(())
    }

    fn metadata_value(
        tx: &rusqlite::Transaction<'_>,
        key: &str,
    ) -> Result<Option<String>, rusqlite::Error> {
        tx.query_row("SELECT value FROM schema_meta WHERE key = ?1", params![key], |row| row.get(0))
            .optional()
    }

    fn set_metadata_value(
        tx: &rusqlite::Transaction<'_>,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        if let Some(value) = value {
            tx.execute(
                "INSERT INTO schema_meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
        } else {
            tx.execute("DELETE FROM schema_meta WHERE key = ?1", params![key])?;
        }
        Ok(())
    }

    fn metadata_changed(previous: Option<&str>, current: Option<&str>) -> bool {
        previous != current
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.open_connection()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tx_map (
                l2_tx_hash TEXT PRIMARY KEY,
                sender TEXT NOT NULL,
                l2_nonce INTEGER NOT NULL,
                payload_nonce INTEGER,
                kaspa_tx_id TEXT,
                state TEXT NOT NULL,
                chain_id INTEGER NOT NULL,
                kaspa_network TEXT NOT NULL,
                el_rpc_fingerprint TEXT,
                kaspa_rpc_fingerprint TEXT,
                correlation_id TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error_code TEXT,
                last_error_message TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_tx_map_sender_nonce ON tx_map(sender, l2_nonce);
            CREATE INDEX IF NOT EXISTS idx_tx_map_prune ON tx_map(state, updated_at_ms);

            CREATE TABLE IF NOT EXISTS sender_nonce_state (
                sender TEXT PRIMARY KEY,
                next_expected_nonce INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sender_locks (
                sender TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL,
                expires_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );",
        )?;
        conn.execute(
            "INSERT INTO schema_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SCHEMA_VERSION_KEY, SCHEMA_VERSION_VALUE],
        )?;
        Ok(())
    }

    fn open_connection(&self) -> Result<Connection, rusqlite::Error> {
        let conn = Connection::open(&self.db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))?;
        Ok(conn)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn is_retryable_busy_error(err: &IgraStoreError) -> bool {
    match err {
        IgraStoreError::Sqlite(rusqlite::Error::SqliteFailure(sqlite_err, _)) => {
            matches!(sqlite_err.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        }
        _ => false,
    }
}

fn fingerprint(url: Option<&str>, chain_id: u64, network: Option<&str>) -> Option<String> {
    let url = url?.trim();
    if url.is_empty() {
        return None;
    }
    let network = network.unwrap_or("unknown");
    let payload = format!("{url}|{chain_id}|{network}");
    Some(format!("0x{}", hex::encode(keccak256(payload.as_bytes()))))
}

fn sanitize_component(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    let mut out = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() { fallback.to_string() } else { out }
}

fn default_db_path(kaspa_network: &str, expected_el_chain_id: u64) -> PathBuf {
    let cache_dir = Config::foundry_cache_dir().unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".foundry").join("cache"))
            .unwrap_or_else(|| std::env::temp_dir().join("foundry").join("cache"))
    });
    cache_dir.join("igra").join(format!("tx-map-v1-{kaspa_network}-{expected_el_chain_id}.sqlite"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store(name: &str) -> IgraStore {
        let path = std::env::temp_dir().join(format!(
            "foundry-igra-store-tests-{name}-{}-{}.sqlite",
            std::process::id(),
            now_ms()
        ));
        IgraStore::new(IgraStoreConfig {
            db_path: Some(path),
            kaspa_network: Some("testnet-10".to_string()),
            expected_el_chain_id: Some(1337),
            sender_lock_timeout_secs: Some(1),
            ..Default::default()
        })
        .expect("store should initialize")
    }

    #[test]
    fn igra_store_migration_initializes_schema() {
        let store = test_store("migration");
        let conn = Connection::open(store.db_path()).expect("db should open");
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN ('schema_meta', 'tx_map', 'sender_nonce_state', 'sender_locks')",
                [],
                |row| row.get(0),
            )
            .expect("schema tables query should succeed");
        assert_eq!(table_count, 4);

        let version: String = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = ?1",
                params![SCHEMA_VERSION_KEY],
                |row| row.get(0),
            )
            .expect("schema version should exist");
        assert_eq!(version, SCHEMA_VERSION_VALUE);
    }

    #[test]
    fn igra_store_profile_change_invalidates_cached_rows() {
        let path = std::env::temp_dir().join(format!(
            "foundry-igra-store-tests-profile-invalidation-{}-{}.sqlite",
            std::process::id(),
            now_ms()
        ));

        let first = IgraStore::new(IgraStoreConfig {
            db_path: Some(path.clone()),
            kaspa_network: Some("testnet-10".to_string()),
            expected_el_chain_id: Some(1337),
            el_rpc_url: Some("http://127.0.0.1:8545".to_string()),
            kaspa_rpc_url: Some("grpc://127.0.0.1:16110".to_string()),
            ..Default::default()
        })
        .expect("initial store should initialize");

        first
            .persist_transition(&TxLifecycleUpdate {
                l2_tx_hash: "0xdeadbeef".to_string(),
                sender: "0x3333333333333333333333333333333333333333".to_string(),
                l2_nonce: 3,
                payload_nonce: Some(3),
                kaspa_tx_id: Some("kaspa-a".to_string()),
                state: TxLifecycleState::KaspaBroadcasted,
                correlation_id: "corr-a".to_string(),
                last_error_code: None,
                last_error_message: None,
                increment_attempts: false,
            })
            .expect("initial tx should persist");

        let second = IgraStore::new(IgraStoreConfig {
            db_path: Some(path),
            kaspa_network: Some("mainnet".to_string()),
            expected_el_chain_id: Some(1),
            el_rpc_url: Some("https://mainnet.example".to_string()),
            kaspa_rpc_url: Some("grpc://mainnet.example".to_string()),
            ..Default::default()
        })
        .expect("reopened store should initialize");

        let record = second.load_tx("0xdeadbeef").expect("query should succeed");
        assert!(record.is_none(), "profile change should invalidate prior cached tx rows");
    }

    #[test]
    fn igra_store_sender_lock_timeout_and_expiry() {
        let store = test_store("lock-timeout");
        let sender = "0x1111111111111111111111111111111111111111";

        store.acquire_sender_lock(sender, "owner-a").expect("owner-a should acquire lock");
        let timeout_err = store
            .acquire_sender_lock(sender, "owner-b")
            .expect_err("owner-b should hit lock timeout while owner-a lease is active");
        assert!(matches!(timeout_err, IgraStoreError::LockTimeout { .. }));
        assert_eq!(timeout_err.code(), Some(IGRA_NONCE_LOCK_TIMEOUT_ERROR_CODE));

        // Lease is 2x the configured timeout (see `acquire_sender_lock`).
        thread::sleep(Duration::from_millis(2_100));
        store
            .acquire_sender_lock(sender, "owner-b")
            .expect("owner-b should acquire lock after lease expiry");
    }

    #[test]
    fn igra_store_same_owner_lock_reacquire_extends_lease() {
        let path = std::env::temp_dir().join(format!(
            "foundry-igra-store-tests-same-owner-reacquire-{}-{}.sqlite",
            std::process::id(),
            now_ms()
        ));
        let store = IgraStore::new(IgraStoreConfig {
            db_path: Some(path),
            kaspa_network: Some("testnet-10".to_string()),
            expected_el_chain_id: Some(1337),
            sender_lock_timeout_secs: Some(2),
            ..Default::default()
        })
        .expect("store should initialize");
        let sender = "0x1212121212121212121212121212121212121212";

        store.acquire_sender_lock(sender, "owner-a").expect("owner-a should acquire lock");
        thread::sleep(Duration::from_millis(500));
        store
            .acquire_sender_lock(sender, "owner-a")
            .expect("same owner should be able to reacquire and renew lease");

        let timeout_err = store
            .acquire_sender_lock(sender, "owner-b")
            .expect_err("owner-b should still time out while renewed lease is active");
        assert!(matches!(timeout_err, IgraStoreError::LockTimeout { .. }));

        // Lease is 2x the configured timeout, so with a 2s timeout we wait >4s.
        thread::sleep(Duration::from_millis(4_100));
        store
            .acquire_sender_lock(sender, "owner-b")
            .expect("owner-b should acquire lock after renewed lease expires");
    }

    #[test]
    fn igra_store_atomic_lock_and_classify_requires_lock_ownership() {
        let store = test_store("atomic-lock-classify");
        let sender = "0x1313131313131313131313131313131313131313";
        let nonce = 11_u64;
        let first_hash = "0xaaa111";

        let first = store
            .acquire_sender_lock_and_classify_nonce(sender, "owner-a", nonce, first_hash)
            .expect("owner-a should lock and classify");
        assert!(matches!(first, NonceOrdering::InOrder { expected: 11 }));

        let timeout_err = store
            .acquire_sender_lock_and_classify_nonce(sender, "owner-b", nonce, "0xbbb222")
            .expect_err("owner-b should not classify while owner-a lock is active");
        assert!(matches!(timeout_err, IgraStoreError::LockTimeout { .. }));

        store.release_sender_lock(sender, "owner-a").expect("owner-a lock release should succeed");
        let in_order = store
            .acquire_sender_lock_and_classify_nonce(sender, "owner-b", nonce, "0xbbb222")
            .expect("owner-b should lock and classify after release");
        assert!(matches!(in_order, NonceOrdering::InOrder { expected: 11 }));
    }

    #[test]
    fn igra_store_mark_submitted_nonce_overflow_is_explicit_error() {
        let store = test_store("nonce-overflow");
        let sender = "0x1414141414141414141414141414141414141414";
        let err = store
            .mark_submitted_in_order_nonce(sender, u64::MAX)
            .expect_err("u64::MAX should overflow next_expected nonce increment");
        assert!(matches!(err, IgraStoreError::NonceOverflow { .. }));
        assert_eq!(err.code(), Some(IGRA_NONCE_OVERFLOW_ERROR_CODE));
    }

    #[test]
    fn igra_store_nonce_gap_classification() {
        let store = test_store("nonce-gap");
        let sender = "0x2222222222222222222222222222222222222222";

        let order = store.classify_nonce(sender, 7).expect("first nonce should be accepted");
        assert!(matches!(order, NonceOrdering::InOrder { expected: 7 }));
        store.mark_submitted_in_order_nonce(sender, 7).expect("next expected nonce should advance");

        let err = store.classify_nonce(sender, 9).expect_err("nonce gap should be blocked");
        assert!(matches!(err, IgraStoreError::BlockedNonceGap { expected: 8, got: 9 }));
        assert_eq!(err.code(), Some(IGRA_NONCE_GAP_ERROR_CODE));
    }

    #[test]
    fn igra_store_happy_path_transition_sequence() {
        let store = test_store("happy-path");
        let tx_hash = "0xabc123";
        let sender = "0x3333333333333333333333333333333333333333";
        let correlation_id = "corr-1";

        for state in [
            TxLifecycleState::ReceivedRawL2,
            TxLifecycleState::KaspaUnsignedCreated,
            TxLifecycleState::KaspaPrefixMined,
            TxLifecycleState::KaspaSigned,
            TxLifecycleState::KaspaBroadcasted,
        ] {
            store
                .persist_transition(&TxLifecycleUpdate {
                    l2_tx_hash: tx_hash.to_string(),
                    sender: sender.to_string(),
                    l2_nonce: 3,
                    payload_nonce: Some(3),
                    kaspa_tx_id: Some("kaspa-tx-1".to_string()),
                    state,
                    correlation_id: correlation_id.to_string(),
                    last_error_code: None,
                    last_error_message: None,
                    increment_attempts: false,
                })
                .expect("transition should persist");
        }

        let record =
            store.load_tx(tx_hash).expect("tx query should succeed").expect("tx should exist");
        assert_eq!(record.state, TxLifecycleState::KaspaBroadcasted);
        assert_eq!(record.sender, sender);
        assert_eq!(record.l2_nonce, 3);
        assert_eq!(record.payload_nonce, Some(3));
        assert_eq!(record.chain_id, 1337);
        assert_eq!(record.kaspa_network, "testnet-10");
        assert_eq!(record.attempts, 0);
        assert_eq!(record.correlation_id, correlation_id);
    }
}
