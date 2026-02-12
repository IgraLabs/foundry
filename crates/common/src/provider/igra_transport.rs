//! IGRA-aware transport wrapper.

use crate::igra_store::{
    IGRA_NONCE_GAP_ERROR_CODE, IGRA_NONCE_REPLACEMENT_CANDIDATE_ERROR_CODE, IgraStore,
    IgraStoreConfig, IgraStoreError, NonceOrdering, TxLifecycleState, TxLifecycleUpdate,
};
use alloy_consensus::{Transaction, TxEnvelope, transaction::SignerRecoverable};
use alloy_json_rpc::{
    Id, RequestPacket, Response, ResponsePacket, ResponsePayload, SerializedRequest,
};
use alloy_primitives::{hex, utils::keccak256};
use alloy_provider::network::eip2718::Decodable2718;
use alloy_transport::{TransportError, TransportErrorKind, TransportFut};
use serde_json::Value;
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
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

const DEFAULT_MINING_TIMEOUT_SECS: u64 = 120;
const IGRA_PAYLOAD_VERSION: u8 = 1;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Runtime IGRA settings required by the transport write path.
#[derive(Clone, Debug, Default)]
pub struct IgraTransportConfig {
    pub tx_id_prefix: Option<String>,
    pub mining_timeout_secs: Option<u64>,
    pub kaspa_rpc_url: Option<String>,
    pub kaspa_network: Option<String>,
}

/// Payload submission request forwarded to a Kaspa submitter implementation.
#[derive(Clone, Debug)]
pub struct IgraSubmitRequest {
    pub l2_tx_hash: String,
    pub payload_bytes: Vec<u8>,
    pub payload_nonce: u32,
    pub kaspa_rpc_url: Option<String>,
    pub kaspa_network: Option<String>,
}

/// Abstraction over Kaspa submission to keep IGRA transport testable.
pub trait IgraPayloadSubmitter: Send + Sync + std::fmt::Debug {
    fn submit_payload(&self, request: &IgraSubmitRequest) -> Result<String, String>;
}

/// Default submitter implementation backed by a `kaswallet` command execution path.
#[derive(Clone, Debug)]
pub struct KaswalletPayloadSubmitter {
    binary: String,
}

impl Default for KaswalletPayloadSubmitter {
    fn default() -> Self {
        let binary = std::env::var("KASWALLET_BIN").unwrap_or_else(|_| "kaswallet".to_string());
        Self { binary }
    }
}

impl IgraPayloadSubmitter for KaswalletPayloadSubmitter {
    fn submit_payload(&self, request: &IgraSubmitRequest) -> Result<String, String> {
        let payload_hex = format!("0x{}", hex::encode(&request.payload_bytes));
        let mut command = Command::new(&self.binary);
        command
            .arg("submit-payload")
            .arg("--payload")
            .arg(payload_hex)
            .arg("--l2-tx-hash")
            .arg(&request.l2_tx_hash);

        if let Some(network) = request.kaspa_network.as_deref() {
            command.arg("--network").arg(network);
        }
        if let Some(rpc_url) = request.kaspa_rpc_url.as_deref() {
            command.arg("--rpc-url").arg(rpc_url);
        }

        let output =
            command.output().map_err(|err| format!("kaswallet execution failed: {err}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let details = if !stderr.is_empty() { stderr } else { stdout };
            return Err(format!(
                "kaswallet submit-payload failed with status {}: {}",
                output.status, details
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_kaspa_tx_id(&stdout).ok_or_else(|| {
            "kaswallet submit-payload succeeded but did not return kaspa_tx_id".to_string()
        })
    }
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
            submitter: Arc::new(KaswalletPayloadSubmitter::default()),
        }
    }

    /// Sets runtime IGRA settings used by the raw-submit interception path.
    pub fn with_transport_config(mut self, config: IgraTransportConfig) -> Self {
        self.tx_id_prefix = config.tx_id_prefix.map(normalize_hex_prefix);
        self.mining_timeout =
            Duration::from_secs(config.mining_timeout_secs.unwrap_or(DEFAULT_MINING_TIMEOUT_SECS));
        self.kaspa_rpc_url = config.kaspa_rpc_url;
        self.kaspa_network = config.kaspa_network;
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

        request.requests().iter().find_map(Self::request_rejection_reason)
    }

    fn request_rejection_reason(request: &SerializedRequest) -> Option<String> {
        if Self::is_unsupported_method(request.method()) {
            return Some(IGRA_SEND_TRANSACTION_UNSUPPORTED_ERROR.to_string());
        }

        if request.method() != "eth_sendRawTransaction" {
            return None;
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
        let tx_type_nibble = Self::raw_tx_type(request)?
            .tx_type_nibble()
            .ok_or_else(|| "unsupported tx type for IGRA payload header".to_string())?;
        let metadata = Self::raw_tx_metadata_from_raw_tx(&raw_tx);

        Ok(Some(RawSendRequest { id: request.id().clone(), raw_tx, tx_type_nibble, metadata }))
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
        Ok(Self::raw_tx_metadata_from_raw_tx(&raw_tx))
    }

    fn raw_tx_metadata_from_raw_tx(raw_tx: &[u8]) -> RawTxMetadata {
        let l2_tx_hash = format!("0x{}", hex::encode(keccak256(&raw_tx)));

        let mut raw_tx_slice = raw_tx;
        let decoded: Option<TxEnvelope> = Decodable2718::decode_2718(&mut raw_tx_slice).ok();
        let sender = decoded
            .as_ref()
            .and_then(|envelope| envelope.recover_signer().ok())
            .map(|address| format!("{address:#x}"));
        let nonce = decoded.as_ref().map(Transaction::nonce);

        RawTxMetadata { l2_tx_hash, sender, nonce }
    }

    fn persist_transition_safe(
        tracked: &TrackedSend,
        state: TxLifecycleState,
        payload_nonce: Option<u32>,
        kaspa_tx_id: Option<String>,
        last_error_code: Option<String>,
        last_error_message: Option<String>,
        increment_attempts: bool,
    ) {
        let update = TxLifecycleUpdate {
            l2_tx_hash: tracked.l2_tx_hash.clone(),
            sender: tracked.sender.clone(),
            l2_nonce: tracked.l2_nonce,
            payload_nonce: payload_nonce.map(u64::from),
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

                        match tracked.store.acquire_sender_lock_and_classify_nonce(
                            &tracked.sender,
                            &tracked.lock_owner_id,
                            tracked.l2_nonce,
                            &tracked.l2_tx_hash,
                        ) {
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

                    let mine_raw_tx = raw_send.raw_tx.clone();
                    let mine_prefix = tx_id_prefix.clone();
                    let mine_timeout = mining_timeout;
                    let mined = tokio::task::spawn_blocking(move || {
                        mine_payload_nonce(
                            &mine_raw_tx,
                            raw_send.tx_type_nibble,
                            &mine_prefix,
                            mine_timeout,
                        )
                    })
                    .await
                    .map_err(|err| {
                        Self::reject_error(&format!("IGRA mining task join failure: {err}"))
                    })?
                    .map_err(|err| Self::reject_error(&err))?;

                    if let Some(tracked) = tracked.as_ref() {
                        Self::persist_transition_safe(
                            tracked,
                            TxLifecycleState::KaspaPrefixMined,
                            Some(mined.payload_nonce),
                            None,
                            replacement_code.clone(),
                            replacement_message.clone(),
                            false,
                        );
                    }

                    let submit_request = IgraSubmitRequest {
                        l2_tx_hash: raw_send.metadata.l2_tx_hash.clone(),
                        payload_bytes: mined.payload_bytes.clone(),
                        payload_nonce: mined.payload_nonce,
                        kaspa_rpc_url,
                        kaspa_network,
                    };
                    let submitter = submitter.clone();
                    let kaspa_tx_id = tokio::task::spawn_blocking(move || {
                        submitter.submit_payload(&submit_request)
                    })
                    .await
                    .map_err(|err| {
                        Self::reject_error(&format!("IGRA submit task join failure: {err}"))
                    })?
                    .map_err(|err| Self::reject_error(&err))?;

                    if let Some(tracked) = tracked.as_ref() {
                        Self::persist_transition_safe(
                            tracked,
                            TxLifecycleState::KaspaSigned,
                            Some(mined.payload_nonce),
                            None,
                            replacement_code.clone(),
                            replacement_message.clone(),
                            false,
                        );
                        Self::persist_transition_safe(
                            tracked,
                            TxLifecycleState::KaspaBroadcasted,
                            Some(mined.payload_nonce),
                            Some(kaspa_tx_id.clone()),
                            replacement_code,
                            replacement_message,
                            false,
                        );
                        if in_order_submit
                            && let Err(err) = tracked
                                .store
                                .mark_submitted_in_order_nonce(&tracked.sender, tracked.l2_nonce)
                        {
                            warn!("failed to advance IGRA next-expected nonce: {err}");
                        }
                    }

                    Self::success_l2_tx_hash_response(raw_send.id.clone(), &raw_send.metadata.l2_tx_hash)
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

                    if lock_acquired
                        && let Err(err) = tracked
                            .store
                            .release_sender_lock(&tracked.sender, &tracked.lock_owner_id)
                    {
                        warn!("failed to release IGRA sender lock: {err}");
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
    tx_type_nibble: u8,
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

#[derive(Clone, Debug)]
struct MinedPayload {
    payload_nonce: u32,
    payload_bytes: Vec<u8>,
}

fn normalize_hex_prefix(prefix: String) -> String {
    prefix.trim().trim_start_matches("0x").to_ascii_lowercase()
}

fn raw_json_string(value: &str) -> Result<Box<serde_json::value::RawValue>, serde_json::Error> {
    serde_json::value::RawValue::from_string(serde_json::to_string(value)?)
}

fn build_igra_payload(raw_tx: &[u8], tx_type_nibble: u8, payload_nonce: u32) -> Vec<u8> {
    let header = ((IGRA_PAYLOAD_VERSION & 0x0f) << 4) | (tx_type_nibble & 0x0f);
    let mut payload = Vec::with_capacity(1 + raw_tx.len() + 4);
    payload.push(header);
    payload.extend_from_slice(raw_tx);
    payload.extend_from_slice(&payload_nonce.to_be_bytes());
    payload
}

fn mine_payload_nonce(
    raw_tx: &[u8],
    tx_type_nibble: u8,
    tx_id_prefix: &str,
    timeout: Duration,
) -> Result<MinedPayload, String> {
    let prefix = normalize_hex_prefix(tx_id_prefix.to_string());
    if prefix.is_empty() {
        return Err("IGRA config error: `tx_id_prefix` cannot be empty".to_string());
    }

    let mut payload = build_igra_payload(raw_tx, tx_type_nibble, 0);

    let nonce_offset = payload.len() - 4;
    let start = Instant::now();
    let mut nonce = 0_u32;
    loop {
        if start.elapsed() > timeout {
            return Err(format!(
                "{IGRA_MINING_TIMEOUT_ERROR_CODE}: timed out mining payload prefix `{prefix}` after {}ms",
                timeout.as_millis()
            ));
        }

        payload[nonce_offset..].copy_from_slice(&nonce.to_be_bytes());
        let hash_hex = hex::encode(keccak256(&payload));
        if hash_hex.starts_with(&prefix) {
            return Ok(MinedPayload { payload_nonce: nonce, payload_bytes: payload });
        }

        if nonce == u32::MAX {
            return Err(format!(
                "{IGRA_MINING_TIMEOUT_ERROR_CODE}: exhausted payload nonce space while mining prefix `{prefix}`"
            ));
        }
        nonce = nonce.wrapping_add(1);
    }
}

fn parse_kaspa_tx_id(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        for key in ["kaspa_tx_id", "tx_id", "transaction_id", "id"] {
            if let Some(id) = value.get(key).and_then(Value::as_str).map(str::trim)
                && !id.is_empty()
            {
                return Some(id.to_string());
            }
        }
    }

    if let Some(candidate) = trimmed
        .split_whitespace()
        .find(|part| looks_like_kaspa_tx_id(part.trim_matches(|c| c == '"' || c == '\'')))
    {
        return Some(candidate.trim_matches(|c| c == '"' || c == '\'').to_string());
    }

    trimmed.lines().rev().find_map(|line| {
        let line = line.trim();
        if line.is_empty() { None } else { Some(line.to_string()) }
    })
}

fn looks_like_kaspa_tx_id(candidate: &str) -> bool {
    let value = candidate.strip_prefix("0x").unwrap_or(candidate);
    value.len() == 64 && value.as_bytes().iter().all(u8::is_ascii_hexdigit)
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
        IgraTransportConfig,
    };
    use crate::igra_store::{
        IGRA_NONCE_REPLACEMENT_CANDIDATE_ERROR_CODE, IgraStore, IgraStoreConfig, TxLifecycleState,
        TxLifecycleUpdate,
    };
    use alloy_json_rpc::{Id, Request, RequestPacket, Response, ResponsePacket, ResponsePayload};
    use alloy_primitives::{hex, utils::keccak256};
    use alloy_transport::{TransportError, TransportFut};
    use serde_json::value::RawValue;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tower::Service;

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
        result: Result<String, String>,
    }

    impl RecordingSubmitter {
        fn success() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: Arc::new(AtomicUsize::new(0)),
                delay: Duration::from_millis(0),
                result: Ok("kaspa-tx-id-1".to_string()),
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

    impl IgraPayloadSubmitter for RecordingSubmitter {
        fn submit_payload(&self, _request: &super::IgraSubmitRequest) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(current, Ordering::SeqCst);
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
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
        }
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
        let raw_tx = [0xc0];
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

        transport
            .request(send_raw_packet(&[0x01, 0x00]))
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

        transport
            .request(send_raw_packet(&[0x02, 0x00]))
            .await
            .expect("EIP-1559 raw tx should be intercepted in IGRA mode");

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
    fn igra_payload_format_includes_header_raw_and_nonce() {
        let raw_tx = [0x01, 0x02, 0x03];
        let payload = super::build_igra_payload(&raw_tx, 2, 0x01020304);
        assert_eq!(payload[0], 0x12, "header must encode version=1 and tx_type=2");
        assert_eq!(&payload[1..4], &raw_tx);
        assert_eq!(&payload[4..8], &[0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn igra_payload_nonce_mining_finds_matching_prefix() {
        let raw_tx = [0xc0];
        let payload0 = super::build_igra_payload(&raw_tx, 0, 0);
        let digest = hex::encode(keccak256(payload0));
        let prefix = &digest[..2];
        let mined = super::mine_payload_nonce(&raw_tx, 0, prefix, Duration::from_secs(1))
            .expect("mining should find a nonce for matching prefix");
        assert_eq!(mined.payload_nonce, 0, "nonce=0 should satisfy its own digest prefix");
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
