//! Falcon-L5 q-zone transaction helpers for IGRA.

use alloy_primitives::{Address, B256, Bytes, U256, hex, keccak256};
use alloy_rlp::{Encodable, Header};
use falcon_det::det1024::{
    CtSignature, FALCON_DET1024_PRIVKEY_SIZE, FALCON_DET1024_PUBKEY_SIZE,
    FALCON_DET1024_SIG_CT_SIZE, SigningKey, VerifyingKey,
};
use falcon_det_sys as falcon_sys;
use std::mem::MaybeUninit;
use thiserror::Error;

pub const IGRA_FALCON_L5_TX_TYPE: u8 = 0x7c;
pub const FALCON_L5_PUBLIC_KEY_LEN: usize = FALCON_DET1024_PUBKEY_SIZE;
pub const FALCON_L5_PRIVATE_KEY_LEN: usize = FALCON_DET1024_PRIVKEY_SIZE;
pub const FALCON_L5_CT_SIGNATURE_LEN: usize = FALCON_DET1024_SIG_CT_SIZE;
pub const FALCON_L5_AUTH_LEN: usize = FALCON_L5_PUBLIC_KEY_LEN + FALCON_L5_CT_SIGNATURE_LEN;

const ADDRESS_DOMAIN: &[u8] = b"IGRA_FALCON_L5_ADDR_V1";
const EMPTY_ACCESS_LIST_RLP_LEN: usize = 1;

#[derive(Debug, Error)]
pub enum IgraQTxError {
    #[error("invalid Falcon-L5 private key length: {0}")]
    InvalidPrivateKeyLen(usize),
    #[error("invalid Falcon-L5 public key length: {0}")]
    InvalidPublicKeyLen(usize),
    #[error("invalid Falcon-L5 signature length: {0}")]
    InvalidSignatureLen(usize),
    #[error("invalid Falcon-L5 auth length: {0}")]
    InvalidAuthLen(usize),
    #[error("Falcon-L5 signature creation failed")]
    SignFailed,
    #[error("Falcon-L5 signature verification failed")]
    InvalidSignature,
    #[error("Falcon-L5 key generation failed")]
    KeygenFailed,
}

#[derive(Clone)]
pub struct FalconL5PrivateKey {
    inner: SigningKey,
    bytes: [u8; FALCON_L5_PRIVATE_KEY_LEN],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FalconL5PublicKey {
    inner: VerifyingKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FalconL5CtSignature {
    inner: CtSignature,
}

impl FalconL5PrivateKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IgraQTxError> {
        if bytes.len() != FALCON_L5_PRIVATE_KEY_LEN {
            return Err(IgraQTxError::InvalidPrivateKeyLen(bytes.len()));
        }

        let mut key = [0u8; FALCON_L5_PRIVATE_KEY_LEN];
        key.copy_from_slice(bytes);
        Ok(Self::from_array(key))
    }

    pub fn from_array(bytes: [u8; FALCON_L5_PRIVATE_KEY_LEN]) -> Self {
        let inner = SigningKey::from_bytes(bytes);
        Self { inner, bytes }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn to_bytes(&self) -> [u8; FALCON_L5_PRIVATE_KEY_LEN] {
        self.bytes
    }

    pub fn public_key(&self) -> FalconL5PublicKey {
        FalconL5PublicKey { inner: self.inner.verifying_key() }
    }

    pub fn sign_ct(&self, msg: &[u8]) -> Result<FalconL5CtSignature, IgraQTxError> {
        let compressed = self.inner.sign_compressed(msg).map_err(|_| IgraQTxError::SignFailed)?;
        let inner = CtSignature::try_from(compressed).map_err(|_| IgraQTxError::SignFailed)?;
        Ok(FalconL5CtSignature { inner })
    }
}

impl Drop for FalconL5PrivateKey {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

impl core::fmt::Debug for FalconL5PrivateKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("FalconL5PrivateKey").finish_non_exhaustive()
    }
}

impl FalconL5PublicKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IgraQTxError> {
        if bytes.len() != FALCON_L5_PUBLIC_KEY_LEN {
            return Err(IgraQTxError::InvalidPublicKeyLen(bytes.len()));
        }

        let inner = VerifyingKey::from_slice(bytes)
            .map_err(|_| IgraQTxError::InvalidPublicKeyLen(bytes.len()))?;
        Ok(Self { inner })
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.inner.as_ref()
    }

    pub fn to_bytes(self) -> [u8; FALCON_L5_PUBLIC_KEY_LEN] {
        let mut out = [0u8; FALCON_L5_PUBLIC_KEY_LEN];
        out.copy_from_slice(self.as_bytes());
        out
    }

    pub fn verify_ct(&self, msg: &[u8], sig: &FalconL5CtSignature) -> bool {
        self.inner.verify_ct(msg, &sig.inner).is_ok()
    }
}

impl FalconL5CtSignature {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IgraQTxError> {
        if bytes.len() != FALCON_L5_CT_SIGNATURE_LEN {
            return Err(IgraQTxError::InvalidSignatureLen(bytes.len()));
        }

        let inner = CtSignature::from_slice(bytes)
            .map_err(|_| IgraQTxError::InvalidSignatureLen(bytes.len()))?;
        Ok(Self { inner })
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.inner.as_ref()
    }

    pub fn to_bytes(self) -> [u8; FALCON_L5_CT_SIGNATURE_LEN] {
        let mut out = [0u8; FALCON_L5_CT_SIGNATURE_LEN];
        out.copy_from_slice(self.as_bytes());
        out
    }
}

pub fn generate_falcon_l5_keypair() -> Result<(FalconL5PrivateKey, FalconL5PublicKey), IgraQTxError>
{
    let mut rng = MaybeUninit::<falcon_sys::shake256_context>::uninit();
    let result = unsafe { falcon_sys::shake256_init_prng_from_system(rng.as_mut_ptr()) };
    if result != 0 {
        return Err(IgraQTxError::KeygenFailed);
    }
    let mut rng = unsafe { rng.assume_init() };
    generate_falcon_l5_keypair_from_rng(&mut rng)
}

pub fn generate_falcon_l5_keypair_from_seed(
    seed: &[u8],
) -> Result<(FalconL5PrivateKey, FalconL5PublicKey), IgraQTxError> {
    let mut rng = MaybeUninit::<falcon_sys::shake256_context>::uninit();
    unsafe {
        falcon_sys::shake256_init_prng_from_seed(
            rng.as_mut_ptr(),
            seed.as_ptr().cast(),
            seed.len(),
        );
    }
    let mut rng = unsafe { rng.assume_init() };
    generate_falcon_l5_keypair_from_rng(&mut rng)
}

fn generate_falcon_l5_keypair_from_rng(
    rng: &mut falcon_sys::shake256_context,
) -> Result<(FalconL5PrivateKey, FalconL5PublicKey), IgraQTxError> {
    let mut secret = [0u8; FALCON_L5_PRIVATE_KEY_LEN];
    let mut public = [0u8; FALCON_L5_PUBLIC_KEY_LEN];
    let result = unsafe {
        falcon_sys::falcon_det1024_keygen(
            rng,
            secret.as_mut_ptr().cast(),
            public.as_mut_ptr().cast(),
        )
    };
    if result != 0 {
        return Err(IgraQTxError::KeygenFailed);
    }

    let private_key = FalconL5PrivateKey::from_array(secret);
    let public_key = FalconL5PublicKey::from_bytes(&public)?;
    if private_key.public_key().as_bytes() != public_key.as_bytes() {
        return Err(IgraQTxError::KeygenFailed);
    }
    Ok((private_key, public_key))
}

pub fn falcon_l5_pubkey_to_address(pk: &FalconL5PublicKey) -> Address {
    let mut input = Vec::with_capacity(ADDRESS_DOMAIN.len() + FALCON_L5_PUBLIC_KEY_LEN);
    input.extend_from_slice(ADDRESS_DOMAIN);
    input.extend_from_slice(pk.as_bytes());
    let hash = keccak256(input);
    Address::from_slice(&hash[12..])
}

pub fn falcon_l5_auth_bytes(
    pk: &FalconL5PublicKey,
    sig: &FalconL5CtSignature,
) -> [u8; FALCON_L5_AUTH_LEN] {
    let mut out = [0u8; FALCON_L5_AUTH_LEN];
    out[..FALCON_L5_PUBLIC_KEY_LEN].copy_from_slice(pk.as_bytes());
    out[FALCON_L5_PUBLIC_KEY_LEN..].copy_from_slice(sig.as_bytes());
    out
}

pub fn falcon_l5_recover_address(msg: &[u8], auth: &[u8]) -> Result<Address, IgraQTxError> {
    if auth.len() != FALCON_L5_AUTH_LEN {
        return Err(IgraQTxError::InvalidAuthLen(auth.len()));
    }

    let pk = FalconL5PublicKey::from_bytes(&auth[..FALCON_L5_PUBLIC_KEY_LEN])?;
    let sig = FalconL5CtSignature::from_bytes(&auth[FALCON_L5_PUBLIC_KEY_LEN..])?;
    if !pk.verify_ct(msg, &sig) {
        return Err(IgraQTxError::InvalidSignature);
    }

    Ok(falcon_l5_pubkey_to_address(&pk))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IgraFalconL5TransactionRequest {
    pub chain_id: u64,
    pub nonce: u64,
    pub max_priority_fee_per_gas: u64,
    pub max_fee_per_gas: u64,
    pub gas_limit: u64,
    pub to: Option<Address>,
    pub value: U256,
    pub data: Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IgraFalconL5SignedTransaction {
    pub raw_tx: Vec<u8>,
    pub signing_hash: B256,
    pub sender: Address,
    pub auth: Vec<u8>,
}

impl IgraFalconL5TransactionRequest {
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.push(IGRA_FALCON_L5_TX_TYPE);
        encode_q_tx_payload(self, None, &mut payload);
        payload
    }

    pub fn signing_hash(&self) -> B256 {
        keccak256(self.signing_payload())
    }

    pub fn sign(
        &self,
        private_key: &FalconL5PrivateKey,
    ) -> Result<IgraFalconL5SignedTransaction, IgraQTxError> {
        let pk = private_key.public_key();
        let signing_hash = self.signing_hash();
        let sig = private_key.sign_ct(signing_hash.as_slice())?;
        let auth = falcon_l5_auth_bytes(&pk, &sig);

        let mut raw_tx = Vec::new();
        raw_tx.push(IGRA_FALCON_L5_TX_TYPE);
        encode_q_tx_payload(self, Some(&auth), &mut raw_tx);

        Ok(IgraFalconL5SignedTransaction {
            raw_tx,
            signing_hash,
            sender: falcon_l5_pubkey_to_address(&pk),
            auth: auth.to_vec(),
        })
    }
}

fn encode_q_tx_payload(
    tx: &IgraFalconL5TransactionRequest,
    auth: Option<&[u8]>,
    out: &mut Vec<u8>,
) {
    let payload_len = q_tx_payload_len(tx, auth);
    Header { list: true, payload_length: payload_len }.encode(out);
    tx.chain_id.encode(out);
    tx.nonce.encode(out);
    tx.max_priority_fee_per_gas.encode(out);
    tx.max_fee_per_gas.encode(out);
    tx.gas_limit.encode(out);
    encode_to(&tx.to, out);
    tx.value.encode(out);
    tx.data.as_ref().encode(out);
    // M1 q-zone transactions keep access lists empty. This is the standard RLP encoding
    // for an empty list, matching the access_list field q-ethrex currently decodes.
    encode_empty_access_list(out);
    if let Some(auth) = auth {
        auth.encode(out);
    }
}

fn q_tx_payload_len(tx: &IgraFalconL5TransactionRequest, auth: Option<&[u8]>) -> usize {
    tx.chain_id.length()
        + tx.nonce.length()
        + tx.max_priority_fee_per_gas.length()
        + tx.max_fee_per_gas.length()
        + tx.gas_limit.length()
        + to_len(&tx.to)
        + tx.value.length()
        + tx.data.as_ref().length()
        + EMPTY_ACCESS_LIST_RLP_LEN
        + auth.map_or(0, |auth| auth.length())
}

fn encode_to(to: &Option<Address>, out: &mut Vec<u8>) {
    match to {
        Some(address) => address.as_slice().encode(out),
        None => (&[] as &[u8]).encode(out),
    }
}

fn to_len(to: &Option<Address>) -> usize {
    match to {
        Some(address) => address.as_slice().length(),
        None => (&[] as &[u8]).length(),
    }
}

fn encode_empty_access_list(out: &mut Vec<u8>) {
    Header { list: true, payload_length: 0 }.encode(out);
}

pub fn encode_q_entry_payload(address: Address, amount_sompi: u64) -> [u8; 28] {
    let mut out = [0u8; 28];
    out[..20].copy_from_slice(address.as_slice());
    out[20..].copy_from_slice(&amount_sompi.to_le_bytes());
    out
}

pub fn parse_private_key_hex(value: &str) -> Result<FalconL5PrivateKey, IgraQTxError> {
    let value = value.trim().trim_start_matches("0x");
    let bytes =
        hex::decode(value).map_err(|_| IgraQTxError::InvalidPrivateKeyLen(value.len() / 2))?;
    FalconL5PrivateKey::from_bytes(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn deterministic_keypair() -> (FalconL5PrivateKey, FalconL5PublicKey) {
        generate_falcon_l5_keypair_from_seed(b"igra-q-logic-zone-falcon-l5-test-seed")
            .expect("deterministic Falcon keygen succeeds")
    }

    fn q_tx_request() -> IgraFalconL5TransactionRequest {
        IgraFalconL5TransactionRequest {
            chain_id: 2026,
            nonce: 7,
            max_priority_fee_per_gas: 3,
            max_fee_per_gas: 10,
            gas_limit: 55_000,
            to: Some(address!("0000000000000000000000000000000000001234")),
            value: U256::from(42u64),
            data: Bytes::from_static(b"q-call"),
        }
    }

    #[test]
    fn sizes_match_falcon_l5_auth_format() {
        assert_eq!(FALCON_L5_PUBLIC_KEY_LEN, 1793);
        assert_eq!(FALCON_L5_CT_SIGNATURE_LEN, 1538);
        assert_eq!(FALCON_L5_AUTH_LEN, 3331);
    }

    #[test]
    fn q_tx_signs_and_recovers_sender() {
        let (sk, pk) = deterministic_keypair();
        let tx = q_tx_request();
        let signed = tx.sign(&sk).expect("q tx signs");

        assert_eq!(signed.raw_tx[0], IGRA_FALCON_L5_TX_TYPE);
        assert_eq!(signed.auth.len(), FALCON_L5_AUTH_LEN);
        assert_eq!(signed.sender, falcon_l5_pubkey_to_address(&pk));
        assert_eq!(
            falcon_l5_recover_address(signed.signing_hash.as_slice(), &signed.auth)
                .expect("auth recovers address"),
            signed.sender
        );
    }

    #[test]
    fn generated_private_key_bytes_roundtrip() {
        let (sk, pk) = deterministic_keypair();
        let private_key_bytes = sk.to_bytes();
        let restored =
            FalconL5PrivateKey::from_bytes(&private_key_bytes).expect("private key restores");

        assert_eq!(private_key_bytes.len(), FALCON_L5_PRIVATE_KEY_LEN);
        assert_eq!(restored.public_key(), pk);
    }

    #[test]
    fn q_tx_signing_payload_excludes_auth() {
        let (sk, _) = deterministic_keypair();
        let tx = q_tx_request();
        let signing_payload = tx.signing_payload();
        let signed = tx.sign(&sk).expect("q tx signs");

        assert!(signing_payload.len() < signed.raw_tx.len());
        assert_eq!(keccak256(&signing_payload), signed.signing_hash);
        assert!(
            !signing_payload
                .windows(FALCON_L5_PUBLIC_KEY_LEN)
                .any(|window| { window == &signed.auth[..FALCON_L5_PUBLIC_KEY_LEN] })
        );
    }

    #[test]
    fn q_entry_payload_is_address_plus_le_amount() {
        let address = address!("1111111111111111111111111111111111111111");
        let entry = encode_q_entry_payload(address, 123_456);

        assert_eq!(&entry[..20], address.as_slice());
        assert_eq!(&entry[20..], &123_456u64.to_le_bytes());
    }
}
