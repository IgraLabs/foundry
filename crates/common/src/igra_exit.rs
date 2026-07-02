use alloy_primitives::hex;
use eyre::{Context, Result, bail, eyre};
use kaspa_addresses::{Address as KaspaAddress, Prefix as KaspaAddressPrefix};
use kaspa_bip32::{
    ChildNumber as KaspaChildNumber, DerivationPath as KaspaDerivationPath,
    ExtendedPublicKey as KaspaExtendedPublicKey, Prefix as KaspaBip32Prefix,
    PublicKey as KaspaBip32PublicKey, secp256k1::PublicKey as KaspaSecpPublicKey,
};
use kaspa_consensus_core::{
    config::params::Params as KaspaParams,
    constants::{
        TX_VERSION as KASPA_TX_VERSION_NATIVE, TX_VERSION_TOCCATA as KASPA_TX_VERSION_TOCCATA,
    },
    hashing::tx as kaspa_tx_hashing,
    mass::{
        ContextualMasses as KaspaContextualMasses, Mass as KaspaMass,
        MassCalculator as KaspaMassCalculator, UtxoCell, calc_storage_mass,
    },
    network::NetworkType as KaspaNetworkType,
    subnets::{SUBNETWORK_ID_SIZE, SubnetworkId},
    tx::{
        ComputeCommit as KaspaComputeCommit, ScriptPublicKey, Transaction as KaspaTransaction,
        TransactionId, TransactionInput as KaspaTransactionInput, TransactionOutpoint,
        TransactionOutput as KaspaTransactionOutput, UtxoEntry,
    },
};
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::{RpcTransaction, api::rpc::RpcApi};
use kaspa_txscript::{
    extract_script_pub_key_address, multisig_redeem_script, multisig_redeem_script_ecdsa,
    pay_to_address_script, pay_to_script_hash_script, script_builder::ScriptBuilder,
};
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{str::FromStr, time::Duration};

pub const IGRA_PROTOCOL_VERSION: u8 = 0x9;
pub const IGRA_EXIT_TX_TYPE_ID: u8 = 0x3;
pub const IGRA_EXIT_PAYLOAD_HEADER: u8 = (IGRA_PROTOCOL_VERSION << 4) | IGRA_EXIT_TX_TYPE_ID;
pub const KAS_LOCKING_SCRIPT_HEX: &str =
    "aa205933185b78c71f0833770ca4aa6b62423af00d0efc2832025a23999543f220f787";
const KASPAWALLET_CANONICAL_COSIGNER_INDEX: u32 = 0;
const KASPAWALLET_EXTERNAL_KEYCHAIN: u32 = 0;
const SOMPI_PER_KAS: u64 = 100_000_000;
const KASPA_MAXIMUM_STANDARD_TRANSACTION_MASS: u64 = 100_000;
const KASPA_MINIMUM_RELAY_TRANSACTION_FEE: u64 = 1_000;
const KASPA_SIGNATURE_SIZE_WITH_HASH_TYPE: usize = 65;
const KASPA_SUBNETWORK_NAMESPACE_LEN: usize = 4;
const KASPA_TOCCATA_COMPUTE_BUDGET_PER_INPUT: u16 = 10;
const SUPPORTED_UNSIGNED_EXIT_SCHEMAS: &[&str] =
    &["igra.exit.unsigned.v1", "igra.exit.unsigned.v2"];
const CURRENT_UNSIGNED_EXIT_SCHEMA: &str = "igra.exit.unsigned.v2";

#[derive(Clone, Debug)]
pub struct BuildExitOptions {
    pub network: String,
    pub tx_id_prefix: String,
    pub lane_id: String,
    pub mining_timeout: Duration,
    pub max_nonce: Option<u32>,
    pub allow_non_igra_lock_script_for_testing: bool,
    pub allow_mass_limit_override_for_testing: bool,
}

#[derive(Clone, Debug)]
pub struct VerifyExitOptions {
    pub allow_signatures: bool,
    pub require_fully_signed: bool,
    pub allow_non_igra_lock_script_for_testing: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BuildExitInput {
    pub locking_utxos: Vec<LockingUtxo>,
    pub exits: Vec<ExitRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeOutput>,
    pub fee_sompi: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_kas_amount",
        skip_serializing_if = "Option::is_none"
    )]
    pub fee_kas: Option<String>,
    pub multisig: MultisigSpec,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LockingUtxo {
    pub transaction_id: String,
    pub index: u32,
    pub amount_sompi: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_kas_amount",
        skip_serializing_if = "Option::is_none"
    )]
    pub amount_kas: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    pub script_public_key: ScriptPublicKeyJson,
    pub derivation_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScriptPublicKeyJson {
    pub version: u16,
    pub script: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExitRequest {
    pub message_id: String,
    pub recipient: String,
    pub amount_sompi: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_kas_amount",
        skip_serializing_if = "Option::is_none"
    )]
    pub amount_kas: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChangeOutput {
    pub derivation_path: String,
    pub amount_sompi: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_kas_amount",
        skip_serializing_if = "Option::is_none"
    )]
    pub amount_kas: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MultisigSpec {
    pub minimum_signatures: u32,
    pub extended_public_keys: Vec<String>,
    #[serde(default)]
    pub ecdsa: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UnsignedExitManifest {
    pub schema: String,
    pub network: String,
    pub protocol: ExitProtocolManifest,
    pub locking_utxos: Vec<LockingUtxo>,
    pub exits: Vec<ExitRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeOutput>,
    pub fee_sompi: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_kas_amount",
        skip_serializing_if = "Option::is_none"
    )]
    pub fee_kas: Option<String>,
    pub total_input_sompi: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_kas_amount",
        skip_serializing_if = "Option::is_none"
    )]
    pub total_input_kas: Option<String>,
    pub total_output_sompi: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_kas_amount",
        skip_serializing_if = "Option::is_none"
    )]
    pub total_output_kas: Option<String>,
    pub multisig: MultisigSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mass: Option<KaspaMassManifest>,
    pub wallet: WalletArtifactManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExitProtocolManifest {
    pub version: u8,
    pub tx_type_id: u8,
    pub payload_header: String,
    pub tx_id_prefix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnetwork_id: Option<String>,
    #[serde(
        serialize_with = "serialize_u32_hex",
        deserialize_with = "deserialize_u32_hex_or_decimal"
    )]
    pub nonce: u32,
    pub kaspa_tx_id: String,
    pub payload_hex: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WalletArtifactManifest {
    pub format: String,
    pub hex_sha256: String,
    pub transaction_version: u16,
    pub inputs: usize,
    pub outputs: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct KaspaMassManifest {
    pub estimated_signed_compute_mass: u64,
    pub transient_mass: u64,
    pub storage_mass: u64,
    pub effective_mass: u64,
    pub fee_sompi: u64,
    pub fee_kas: String,
    pub minimum_relay_fee_sompi: u64,
    pub minimum_relay_fee_kas: String,
    pub standard_transaction_mass_limit: u64,
    pub block_mass_limit: u64,
    pub standard_limit_exceeded: bool,
    pub block_limit_exceeded: bool,
    pub fee_below_minimum_relay: bool,
}

#[derive(Clone, Debug)]
pub struct BuildExitOutput {
    pub manifest: UnsignedExitManifest,
    pub wallet_hex: String,
}

#[derive(Clone, Debug)]
pub struct VerifyExitReport {
    pub kaspa_tx_id: String,
    pub payload_nonce: u32,
    pub input_count: usize,
    pub output_count: usize,
    pub signed_inputs: usize,
    pub fully_signed: bool,
}

#[derive(Clone, Debug)]
pub struct MultisigAddressInput {
    pub network: String,
    pub derivation_path: String,
    pub minimum_signatures: u32,
    pub extended_public_keys: Vec<String>,
    pub ecdsa: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MultisigDerivationPathReport {
    pub path: String,
    pub canonical: bool,
    pub cosigner_index: u32,
    pub keychain: u32,
    pub address_index: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MultisigAddressReport {
    pub network: String,
    pub derivation_path: MultisigDerivationPathReport,
    pub minimum_signatures: u32,
    pub ecdsa: bool,
    pub address: String,
    pub script_public_key: ScriptPublicKeyJson,
    pub redeem_script_hex: String,
    pub sorted_extended_public_keys: Vec<String>,
    pub derived_extended_public_keys: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MultisigAddressVerificationReport {
    pub expected_address: String,
    pub actual_address: String,
    pub matches: bool,
    pub derivation: MultisigAddressReport,
}

#[derive(Clone, PartialEq, Message)]
struct PartiallySignedTransactionProto {
    #[prost(message, optional, tag = "1")]
    tx: Option<TransactionMessageProto>,
    #[prost(message, repeated, tag = "2")]
    partially_signed_inputs: Vec<PartiallySignedInputProto>,
}

#[derive(Clone, PartialEq, Message)]
struct PartiallySignedInputProto {
    #[prost(bytes = "vec", tag = "1")]
    redeem_script: Vec<u8>,
    #[prost(message, optional, tag = "2")]
    prev_output: Option<TransactionOutputProto>,
    #[prost(uint32, tag = "3")]
    minimum_signatures: u32,
    #[prost(message, repeated, tag = "4")]
    pub_key_signature_pairs: Vec<PubKeySignaturePairProto>,
    #[prost(string, tag = "5")]
    derivation_path: String,
}

#[derive(Clone, PartialEq, Message)]
struct PubKeySignaturePairProto {
    #[prost(string, tag = "1")]
    extended_pub_key: String,
    #[prost(bytes = "vec", tag = "2")]
    signature: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct SubnetworkIdProto {
    #[prost(bytes = "vec", tag = "1")]
    bytes: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct TransactionMessageProto {
    #[prost(uint32, tag = "1")]
    version: u32,
    #[prost(message, repeated, tag = "2")]
    inputs: Vec<TransactionInputProto>,
    #[prost(message, repeated, tag = "3")]
    outputs: Vec<TransactionOutputProto>,
    #[prost(uint64, tag = "4")]
    lock_time: u64,
    #[prost(message, optional, tag = "5")]
    subnetwork_id: Option<SubnetworkIdProto>,
    #[prost(uint64, tag = "6")]
    gas: u64,
    #[prost(bytes = "vec", tag = "8")]
    payload: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct TransactionInputProto {
    #[prost(message, optional, tag = "1")]
    previous_outpoint: Option<OutpointProto>,
    #[prost(bytes = "vec", tag = "2")]
    signature_script: Vec<u8>,
    #[prost(uint64, tag = "3")]
    sequence: u64,
    #[prost(uint32, tag = "4")]
    sig_op_count: u32,
    #[prost(uint32, tag = "6")]
    compute_budget: u32,
}

#[derive(Clone, PartialEq, Message)]
struct OutpointProto {
    #[prost(message, optional, tag = "1")]
    transaction_id: Option<TransactionIdProto>,
    #[prost(uint32, tag = "2")]
    index: u32,
}

#[derive(Clone, PartialEq, Message)]
struct TransactionIdProto {
    #[prost(bytes = "vec", tag = "1")]
    bytes: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct ScriptPublicKeyProto {
    #[prost(bytes = "vec", tag = "1")]
    script: Vec<u8>,
    #[prost(uint32, tag = "2")]
    version: u32,
}

#[derive(Clone, PartialEq, Message)]
struct TransactionOutputProto {
    #[prost(uint64, tag = "1")]
    value: u64,
    #[prost(message, optional, tag = "2")]
    script_public_key: Option<ScriptPublicKeyProto>,
}

pub fn build_unsigned_exit(
    input: BuildExitInput,
    options: BuildExitOptions,
) -> Result<BuildExitOutput> {
    let mut input = input;
    validate_build_input(&input, options.allow_non_igra_lock_script_for_testing)?;
    input.multisig.extended_public_keys.sort();

    let network_prefix = parse_network_prefix(&options.network)?;
    validate_human_readable_addresses(&input, network_prefix)?;
    enrich_human_readable_fields(&mut input, network_prefix)?;
    let tx_id_prefix = decode_fixed_hex(&options.tx_id_prefix, "tx-id prefix")?;
    if tx_id_prefix.is_empty() {
        bail!("tx-id prefix cannot be empty");
    }
    if tx_id_prefix.len() > 4 {
        bail!("tx-id prefix cannot exceed 4 bytes; the IGRA payload nonce is only 4 bytes");
    }
    let subnetwork_id = parse_igra_lane_id(&options.lane_id)?;
    let lane_id = normalize_lane_id(&options.lane_id)?;
    let subnetwork_id_hex = prefixed_hex(subnetwork_id.as_ref());
    let tx_version = kaspa_tx_version_for_subnetwork_id(&subnetwork_id);

    let payload_l2data = exit_l2data(&input.exits)?;
    let outputs =
        transaction_outputs(&input.exits, input.change.as_ref(), &input.multisig, network_prefix)?;
    let total_output_sompi = outputs.iter().map(|output| output.value).sum::<u64>();
    let total_input_sompi = input.locking_utxos.iter().map(|utxo| utxo.amount_sompi).sum::<u64>();
    let required = total_output_sompi
        .checked_add(input.fee_sompi)
        .ok_or_else(|| eyre!("output total plus fee overflows u64"))?;
    if total_input_sompi != required {
        bail!(
            "input amount mismatch: inputs={total_input_sompi}, outputs={total_output_sompi}, fee={}",
            input.fee_sompi
        );
    }

    let inputs = input
        .locking_utxos
        .iter()
        .map(|utxo| {
            let txid = parse_transaction_id(&utxo.transaction_id)?;
            Ok(kaspa_transaction_input_for_version(
                tx_version,
                TransactionOutpoint::new(txid, utxo.index),
                Vec::new(),
                0,
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    let payload = build_payload_with_nonce(IGRA_EXIT_PAYLOAD_HEADER, &payload_l2data, 0);
    let mut tx = KaspaTransaction::new(tx_version, inputs, outputs, 0, subnetwork_id, 0, payload);
    let mass = calculate_mass_preflight(&tx, &input, &options.network)?;
    validate_mass_preflight(
        &mass,
        &input,
        network_prefix,
        &options.network,
        options.allow_mass_limit_override_for_testing,
    )?;
    let nonce =
        mine_payload_nonce(&mut tx, &tx_id_prefix, options.mining_timeout, options.max_nonce)?;

    let pst = partially_signed_transaction_proto(&tx, &input, network_prefix)?;
    let wallet_bytes = pst.encode_to_vec();
    let wallet_hex = hex::encode(&wallet_bytes);
    let payload_hex = prefixed_hex(&tx.payload);
    let kaspa_tx_id = tx.id().to_string();

    let manifest = UnsignedExitManifest {
        schema: CURRENT_UNSIGNED_EXIT_SCHEMA.to_string(),
        network: options.network,
        protocol: ExitProtocolManifest {
            version: IGRA_PROTOCOL_VERSION,
            tx_type_id: IGRA_EXIT_TX_TYPE_ID,
            payload_header: prefixed_hex(&[IGRA_EXIT_PAYLOAD_HEADER]),
            tx_id_prefix: prefixed_hex(&tx_id_prefix),
            lane_id: Some(lane_id),
            subnetwork_id: Some(subnetwork_id_hex),
            nonce,
            kaspa_tx_id,
            payload_hex,
        },
        locking_utxos: input.locking_utxos,
        exits: input.exits,
        change: input.change,
        fee_sompi: input.fee_sompi,
        fee_kas: input.fee_kas,
        total_input_sompi,
        total_input_kas: Some(sompi_to_kas_string(total_input_sompi)),
        total_output_sompi,
        total_output_kas: Some(sompi_to_kas_string(total_output_sompi)),
        multisig: input.multisig,
        mass: Some(mass),
        wallet: WalletArtifactManifest {
            format: "kaspawallet.PartiallySignedTransaction.hex".to_string(),
            hex_sha256: prefixed_hex(&Sha256::digest(wallet_hex.as_bytes())),
            transaction_version: tx.version,
            inputs: tx.inputs.len(),
            outputs: tx.outputs.len(),
        },
    };

    Ok(BuildExitOutput { manifest, wallet_hex })
}

pub fn verify_unsigned_exit(
    manifest: &UnsignedExitManifest,
    wallet_hex: &str,
    options: VerifyExitOptions,
) -> Result<VerifyExitReport> {
    if !SUPPORTED_UNSIGNED_EXIT_SCHEMAS.contains(&manifest.schema.as_str()) {
        bail!("unsupported manifest schema: {}", manifest.schema);
    }
    validate_build_input(
        &BuildExitInput {
            locking_utxos: manifest.locking_utxos.clone(),
            exits: manifest.exits.clone(),
            change: manifest.change.clone(),
            fee_sompi: manifest.fee_sompi,
            fee_kas: manifest.fee_kas.clone(),
            multisig: manifest.multisig.clone(),
        },
        options.allow_non_igra_lock_script_for_testing,
    )?;
    validate_human_readable_addresses(
        &BuildExitInput {
            locking_utxos: manifest.locking_utxos.clone(),
            exits: manifest.exits.clone(),
            change: manifest.change.clone(),
            fee_sompi: manifest.fee_sompi,
            fee_kas: manifest.fee_kas.clone(),
            multisig: manifest.multisig.clone(),
        },
        parse_network_prefix(&manifest.network)?,
    )?;
    validate_amount_kas_field(
        manifest.total_input_kas.as_deref(),
        manifest.total_input_sompi,
        "total_input_kas",
    )?;
    validate_amount_kas_field(
        manifest.total_output_kas.as_deref(),
        manifest.total_output_sompi,
        "total_output_kas",
    )?;

    let wallet_bytes = decode_wallet_hex(wallet_hex)?;
    let actual_sha256 = prefixed_hex(&Sha256::digest(wallet_hex.trim().as_bytes()));
    if !options.allow_signatures && actual_sha256 != manifest.wallet.hex_sha256 {
        bail!(
            "wallet hex sha256 mismatch: manifest={}, actual={actual_sha256}",
            manifest.wallet.hex_sha256
        );
    }

    let pst = PartiallySignedTransactionProto::decode(wallet_bytes.as_slice())
        .wrap_err("failed to decode kaspawallet PartiallySignedTransaction protobuf")?;
    let proto_tx = pst.tx.as_ref().ok_or_else(|| eyre!("wallet protobuf is missing tx"))?;
    let tx = transaction_from_proto(proto_tx)?;

    if tx.version != manifest.wallet.transaction_version {
        bail!("transaction version mismatch");
    }
    let expected_subnetwork_id =
        expected_manifest_subnetwork_id(&manifest.protocol)?.unwrap_or_default();
    let expected_tx_version = kaspa_tx_version_for_subnetwork_id(&expected_subnetwork_id);
    if tx.version != expected_tx_version {
        bail!(
            "transaction version mismatch for subnetwork: expected={}, actual={}",
            expected_tx_version,
            tx.version
        );
    }
    if tx.subnetwork_id != expected_subnetwork_id {
        bail!(
            "transaction subnetwork id mismatch: manifest={}, actual=0x{}",
            prefixed_hex(expected_subnetwork_id.as_ref()),
            tx.subnetwork_id
        );
    }
    if tx.inputs.len() != manifest.locking_utxos.len() {
        bail!("input count mismatch");
    }
    let expected_output_count = manifest.exits.len() + usize::from(manifest.change.is_some());
    if tx.outputs.len() != expected_output_count {
        bail!("output count mismatch");
    }
    if pst.partially_signed_inputs.len() != tx.inputs.len() {
        bail!("partially signed input count must match transaction input count");
    }

    let tx_id = tx.id();
    let tx_id_string = tx_id.to_string();
    if tx_id_string != manifest.protocol.kaspa_tx_id {
        let legacy_tx_id = kaspa_tx_hashing::id_v0(&tx).to_string();
        if legacy_tx_id == manifest.protocol.kaspa_tx_id {
            bail!(
                "kaspa txid mismatch: manifest={} matches legacy v0-style txid for a version {} transaction; current Kaspa v1 txid is {tx_id_string}",
                manifest.protocol.kaspa_tx_id,
                tx.version
            );
        }
        bail!(
            "kaspa txid mismatch: manifest={}, actual={tx_id_string}",
            manifest.protocol.kaspa_tx_id
        );
    }

    let expected_prefix =
        decode_fixed_hex(&manifest.protocol.tx_id_prefix, "manifest tx-id prefix")?;
    if !tx_id.as_bytes().starts_with(&expected_prefix) {
        bail!("kaspa txid does not match required IGRA prefix {}", manifest.protocol.tx_id_prefix);
    }

    let nonce = verify_payload(&tx.payload, &manifest.exits)?;
    if nonce != manifest.protocol.nonce {
        bail!("payload nonce mismatch: manifest={}, actual={nonce}", manifest.protocol.nonce);
    }
    if prefixed_hex(&tx.payload) != manifest.protocol.payload_hex {
        bail!("payload hex mismatch");
    }

    verify_inputs(
        &tx,
        &pst,
        manifest,
        options.allow_signatures,
        options.allow_non_igra_lock_script_for_testing,
    )?;
    verify_outputs(&tx, manifest)?;
    verify_mass_manifest(&tx, manifest)?;

    let signed_inputs = pst
        .partially_signed_inputs
        .iter()
        .filter(|input| {
            input.pub_key_signature_pairs.iter().filter(|pair| !pair.signature.is_empty()).count()
                > 0
        })
        .count();
    let fully_signed = pst.partially_signed_inputs.iter().all(|input| {
        input.pub_key_signature_pairs.iter().filter(|pair| !pair.signature.is_empty()).count()
            >= input.minimum_signatures as usize
    });

    if options.require_fully_signed && !fully_signed {
        bail!("transaction is not fully signed according to minimum_signatures");
    }

    Ok(VerifyExitReport {
        kaspa_tx_id: tx_id_string,
        payload_nonce: nonce,
        input_count: tx.inputs.len(),
        output_count: tx.outputs.len(),
        signed_inputs,
        fully_signed,
    })
}

pub fn decode_wallet_transaction(wallet_hex: &str) -> Result<KaspaTransaction> {
    let wallet_bytes = decode_wallet_hex(wallet_hex)?;
    let pst = PartiallySignedTransactionProto::decode(wallet_bytes.as_slice())
        .wrap_err("failed to decode kaspawallet PartiallySignedTransaction protobuf")?;
    let proto_tx = pst.tx.as_ref().ok_or_else(|| eyre!("wallet protobuf is missing tx"))?;
    transaction_from_proto(proto_tx)
}

pub fn materialize_signed_wallet_transaction(
    manifest: &UnsignedExitManifest,
    wallet_hex: &str,
) -> Result<KaspaTransaction> {
    let wallet_bytes = decode_wallet_hex(wallet_hex)?;
    let pst = PartiallySignedTransactionProto::decode(wallet_bytes.as_slice())
        .wrap_err("failed to decode kaspawallet PartiallySignedTransaction protobuf")?;
    let proto_tx = pst.tx.as_ref().ok_or_else(|| eyre!("wallet protobuf is missing tx"))?;
    let mut tx = transaction_from_proto(proto_tx)?;

    if tx.inputs.len() != pst.partially_signed_inputs.len() {
        bail!("partially signed input count must match transaction input count");
    }
    if tx.inputs.len() != manifest.locking_utxos.len() {
        bail!("manifest locking_utxos count must match transaction input count");
    }

    let sig_op_count = u8::try_from(manifest.multisig.extended_public_keys.len())
        .wrap_err("multisig key count exceeds Kaspa sigOpCount range")?;
    let tx_version = tx.version;

    for (index, ((tx_input, partial_input), manifest_utxo)) in tx
        .inputs
        .iter_mut()
        .zip(&pst.partially_signed_inputs)
        .zip(&manifest.locking_utxos)
        .enumerate()
    {
        let signatures = partial_input
            .pub_key_signature_pairs
            .iter()
            .filter_map(|pair| (!pair.signature.is_empty()).then_some(pair.signature.as_slice()))
            .collect::<Vec<_>>();
        if signatures.len() < partial_input.minimum_signatures as usize {
            bail!(
                "input {index} has only {} signatures, below minimum_signatures {}",
                signatures.len(),
                partial_input.minimum_signatures
            );
        }

        let mut script_builder = ScriptBuilder::new();
        if manifest.multisig.extended_public_keys.len() > 1 {
            for signature in signatures {
                script_builder.add_data(signature)?;
            }
            let redeem_script = multisig_redeem_script_for_path(
                &manifest.multisig,
                &manifest_utxo.derivation_path,
            )?;
            script_builder.add_data(&redeem_script)?;
        } else {
            let signature = signatures
                .first()
                .copied()
                .ok_or_else(|| eyre!("input {index} missing single-sig signature"))?;
            script_builder.add_data(signature)?;
        }

        apply_kaspa_input_signature_mass(tx_input, tx_version, sig_op_count);
        tx_input.signature_script = script_builder.drain();
    }

    tx.finalize();
    Ok(tx)
}

pub async fn broadcast_wallet_transaction(
    manifest: &UnsignedExitManifest,
    wallet_hex: &str,
    kaspa_rpc_url: &str,
) -> Result<String> {
    let tx = materialize_signed_wallet_transaction(manifest, wallet_hex)?;
    let expected_tx_id = tx.id().to_string();
    let client = GrpcClient::connect(kaspa_rpc_url.to_string())
        .await
        .wrap_err_with(|| format!("failed to connect to Kaspa RPC `{kaspa_rpc_url}`"))?;
    let submitted_tx_id = client
        .submit_transaction(RpcTransaction::from(&tx), false)
        .await
        .wrap_err_with(|| format!("failed to submit transaction to Kaspa RPC `{kaspa_rpc_url}`"))?
        .to_string();
    if submitted_tx_id != expected_tx_id {
        bail!(
            "Kaspa RPC returned unexpected txid: expected {expected_tx_id}, actual {submitted_tx_id}"
        );
    }
    Ok(submitted_tx_id)
}

pub fn check_multisig_derivation_path(path: &str) -> Result<MultisigDerivationPathReport> {
    validate_canonical_multisig_receive_derivation_path(path, "derivation_path")?;
    let parsed = path
        .parse::<KaspaDerivationPath>()
        .wrap_err("derivation_path is not a valid Kaspa derivation path")?;
    let children = parsed.as_ref();
    Ok(MultisigDerivationPathReport {
        path: path.to_string(),
        canonical: true,
        cosigner_index: children[0].index(),
        keychain: children[1].index(),
        address_index: children[2].index(),
    })
}

pub fn derive_multisig_address(input: MultisigAddressInput) -> Result<MultisigAddressReport> {
    validate_multisig_address_input(&input)?;
    let network_prefix = parse_network_prefix(&input.network)?;
    let derivation_path = check_multisig_derivation_path(&input.derivation_path)?;
    let mut sorted_extended_public_keys = input.extended_public_keys;
    sorted_extended_public_keys.sort();

    let multisig = MultisigSpec {
        minimum_signatures: input.minimum_signatures,
        extended_public_keys: sorted_extended_public_keys.clone(),
        ecdsa: input.ecdsa,
    };
    let redeem_script = multisig_redeem_script_for_path(&multisig, &input.derivation_path)?;
    let script_public_key = pay_to_script_hash_script(&redeem_script);
    let address = extract_script_pub_key_address(&script_public_key, network_prefix)
        .wrap_err("failed to derive multisig address from script_public_key")?
        .to_string();
    let derived_extended_public_keys = derived_extended_public_keys(
        &sorted_extended_public_keys,
        &input.derivation_path,
        network_prefix,
    )?;

    Ok(MultisigAddressReport {
        network: input.network,
        derivation_path,
        minimum_signatures: input.minimum_signatures,
        ecdsa: input.ecdsa,
        address,
        script_public_key: ScriptPublicKeyJson {
            version: script_public_key.version(),
            script: hex::encode(script_public_key.script()),
        },
        redeem_script_hex: prefixed_hex(&redeem_script),
        sorted_extended_public_keys,
        derived_extended_public_keys,
    })
}

pub fn verify_multisig_address(
    input: MultisigAddressInput,
    expected_address: &str,
) -> Result<MultisigAddressVerificationReport> {
    let derivation = derive_multisig_address(input)?;
    let expected = KaspaAddress::try_from(expected_address)
        .wrap_err("failed to parse expected multisig address")?;
    if expected.prefix != parse_network_prefix(&derivation.network)? {
        bail!(
            "expected address prefix {} does not match network {}",
            expected.prefix,
            derivation.network
        );
    }

    Ok(MultisigAddressVerificationReport {
        expected_address: expected_address.to_string(),
        actual_address: derivation.address.clone(),
        matches: derivation.address == expected_address,
        derivation,
    })
}

fn validate_multisig_address_input(input: &MultisigAddressInput) -> Result<()> {
    if input.minimum_signatures == 0 {
        bail!("minimum_signatures must be greater than zero");
    }
    if input.extended_public_keys.is_empty() {
        bail!("at least one extended public key is required");
    }
    if input.minimum_signatures as usize > input.extended_public_keys.len() {
        bail!("minimum_signatures cannot exceed extended_public_keys length");
    }
    if has_duplicates(&input.extended_public_keys) {
        bail!("extended_public_keys contains duplicates");
    }
    for key in &input.extended_public_keys {
        key.parse::<KaspaExtendedPublicKey<KaspaSecpPublicKey>>()
            .wrap_err_with(|| format!("invalid Kaspa extended public key `{key}`"))?;
    }
    Ok(())
}

fn validate_build_input(
    input: &BuildExitInput,
    allow_non_igra_lock_script_for_testing: bool,
) -> Result<()> {
    if input.locking_utxos.is_empty() {
        bail!("at least one KAS locking UTXO is required");
    }
    if input.exits.is_empty() {
        bail!("at least one exit is required");
    }
    if input.multisig.minimum_signatures == 0 {
        bail!("minimum_signatures must be greater than zero");
    }
    if input.multisig.extended_public_keys.is_empty() {
        bail!("at least one extended public key is required");
    }
    if input.multisig.minimum_signatures as usize > input.multisig.extended_public_keys.len() {
        bail!("minimum_signatures cannot exceed extended_public_keys length");
    }
    if has_duplicates(&input.multisig.extended_public_keys) {
        bail!("extended_public_keys contains duplicates");
    }

    let locking_script = kas_locking_script()?;
    for (index, utxo) in input.locking_utxos.iter().enumerate() {
        if utxo.amount_sompi == 0 {
            bail!("locking_utxos[{index}].amount_sompi must be greater than zero");
        }
        validate_amount_kas_field(
            utxo.amount_kas.as_deref(),
            utxo.amount_sompi,
            &format!("locking_utxos[{index}].amount_kas"),
        )?;
        if utxo.derivation_path.trim().is_empty() {
            bail!("locking_utxos[{index}].derivation_path cannot be empty");
        }
        if input.multisig.extended_public_keys.len() > 1 {
            validate_canonical_multisig_receive_derivation_path(
                &utxo.derivation_path,
                &format!("locking_utxos[{index}].derivation_path"),
            )?;
        }
        let script = decode_fixed_hex(&utxo.script_public_key.script, "locking UTXO script")?;
        if !allow_non_igra_lock_script_for_testing
            && (utxo.script_public_key.version != 0 || script != locking_script)
        {
            bail!(
                "locking_utxos[{index}] is not the IGRA KAS locking script from the transaction protocol spec"
            );
        }
        parse_transaction_id(&utxo.transaction_id)?;
    }

    for (index, exit) in input.exits.iter().enumerate() {
        let message_id = decode_fixed_hex(&exit.message_id, "message id")?;
        if message_id.len() != 32 {
            bail!("exits[{index}].message_id must be exactly 32 bytes");
        }
        if exit.amount_sompi == 0 {
            bail!("exits[{index}].amount_sompi must be greater than zero");
        }
        validate_amount_kas_field(
            exit.amount_kas.as_deref(),
            exit.amount_sompi,
            &format!("exits[{index}].amount_kas"),
        )?;
    }

    if let Some(change) = input.change.as_ref() {
        if change.amount_sompi == 0 {
            bail!("change.amount_sompi must be greater than zero");
        }
        validate_amount_kas_field(
            change.amount_kas.as_deref(),
            change.amount_sompi,
            "change.amount_kas",
        )?;
        validate_canonical_multisig_receive_derivation_path(
            &change.derivation_path,
            "change.derivation_path",
        )?;
    }
    validate_amount_kas_field(input.fee_kas.as_deref(), input.fee_sompi, "fee_kas")?;

    Ok(())
}

fn validate_canonical_multisig_receive_derivation_path(path: &str, field: &str) -> Result<()> {
    let path = path
        .parse::<KaspaDerivationPath>()
        .wrap_err_with(|| format!("{field} is not a valid Kaspa derivation path"))?;
    let children = path.as_ref();
    if children.len() != 3 {
        bail!(
            "{field} must use the official kaspawallet canonical multisig receive path m/0/0/<index>"
        );
    }

    let cosigner_index = children[0];
    let keychain = children[1];
    let address_index = children[2];
    if is_hardened(cosigner_index) || is_hardened(keychain) || is_hardened(address_index) {
        bail!("{field} must use non-hardened official kaspawallet child indexes: m/0/0/<index>");
    }
    if cosigner_index.index() != KASPAWALLET_CANONICAL_COSIGNER_INDEX {
        bail!("{field} must use canonical sorted-signer cosigner index 0: m/0/0/<index>");
    }
    if keychain.index() != KASPAWALLET_EXTERNAL_KEYCHAIN {
        bail!("{field} must use kaspawallet external receive keychain 0: m/0/0/<index>");
    }

    Ok(())
}

fn is_hardened(child: KaspaChildNumber) -> bool {
    child.is_hardened()
}

fn validate_amount_kas_field(
    amount_kas: Option<&str>,
    amount_sompi: u64,
    field: &str,
) -> Result<()> {
    if let Some(amount_kas) = amount_kas {
        let parsed = kas_string_to_sompi(amount_kas)
            .wrap_err_with(|| format!("{field} does not match amount_sompi"))?;
        if parsed != amount_sompi {
            bail!(
                "{field} does not match amount_sompi: {amount_kas} KAS = {parsed} sompi, expected {amount_sompi}"
            );
        }
    }
    Ok(())
}

fn validate_human_readable_addresses(
    input: &BuildExitInput,
    network_prefix: KaspaAddressPrefix,
) -> Result<()> {
    for (index, utxo) in input.locking_utxos.iter().enumerate() {
        if let Some(address) = utxo.address.as_deref() {
            let expected = locking_utxo_address(utxo, network_prefix)?;
            if address != expected {
                bail!(
                    "locking_utxos[{index}].address does not match script_public_key: {address} != {expected}"
                );
            }
        }
    }

    if let Some(change) = input.change.as_ref() {
        if let Some(address) = change.address.as_deref() {
            let expected = multisig_change_address(change, &input.multisig, network_prefix)?;
            if address != expected {
                bail!(
                    "change.address does not match derivation_path/multisig: {address} != {expected}"
                );
            }
        }
    }

    Ok(())
}

fn enrich_human_readable_fields(
    input: &mut BuildExitInput,
    network_prefix: KaspaAddressPrefix,
) -> Result<()> {
    for utxo in &mut input.locking_utxos {
        utxo.amount_kas = Some(sompi_to_kas_string(utxo.amount_sompi));
        utxo.address = Some(locking_utxo_address(utxo, network_prefix)?);
    }

    for exit in &mut input.exits {
        exit.amount_kas = Some(sompi_to_kas_string(exit.amount_sompi));
    }

    if let Some(change) = input.change.as_mut() {
        change.amount_kas = Some(sompi_to_kas_string(change.amount_sompi));
        change.address = Some(multisig_change_address(change, &input.multisig, network_prefix)?);
    }

    input.fee_kas = Some(sompi_to_kas_string(input.fee_sompi));
    Ok(())
}

fn locking_utxo_address(utxo: &LockingUtxo, network_prefix: KaspaAddressPrefix) -> Result<String> {
    let script = decode_fixed_hex(&utxo.script_public_key.script, "locking UTXO script")?;
    let script_public_key = ScriptPublicKey::from_vec(utxo.script_public_key.version, script);
    Ok(extract_script_pub_key_address(&script_public_key, network_prefix)
        .wrap_err("failed to derive locking UTXO address from script_public_key")?
        .to_string())
}

fn verify_inputs(
    tx: &KaspaTransaction,
    pst: &PartiallySignedTransactionProto,
    manifest: &UnsignedExitManifest,
    allow_signatures: bool,
    allow_non_igra_lock_script_for_testing: bool,
) -> Result<()> {
    let locking_script = kas_locking_script()?;

    for (index, ((tx_input, partial_input), manifest_utxo)) in
        tx.inputs.iter().zip(&pst.partially_signed_inputs).zip(&manifest.locking_utxos).enumerate()
    {
        if !allow_signatures && tx_input_contains_signature_material(tx.version, tx_input) {
            bail!(
                "input {index} contains signatures; pass --allow-signatures to verify signed artifacts"
            );
        }
        if tx_input.previous_outpoint.transaction_id
            != parse_transaction_id(&manifest_utxo.transaction_id)?
        {
            bail!("input {index} previous transaction id mismatch");
        }
        if tx_input.previous_outpoint.index != manifest_utxo.index {
            bail!("input {index} previous outpoint index mismatch");
        }

        let prev_output = partial_input
            .prev_output
            .as_ref()
            .ok_or_else(|| eyre!("partial input {index} missing prevOutput"))?;
        let prev_spk = prev_output
            .script_public_key
            .as_ref()
            .ok_or_else(|| eyre!("partial input {index} missing prevOutput scriptPublicKey"))?;
        if prev_output.value != manifest_utxo.amount_sompi {
            bail!("partial input {index} prevOutput amount mismatch");
        }
        if prev_spk.version != manifest_utxo.script_public_key.version as u32 {
            bail!("partial input {index} prevOutput scriptPublicKey version mismatch");
        }
        if prev_spk.script
            != decode_fixed_hex(
                &manifest_utxo.script_public_key.script,
                "manifest locking UTXO script",
            )?
        {
            bail!("partial input {index} prevOutput script mismatch");
        }
        if !allow_non_igra_lock_script_for_testing && prev_spk.script != locking_script {
            bail!("partial input {index} prevOutput is not the IGRA KAS locking script");
        }
        if partial_input.minimum_signatures != manifest.multisig.minimum_signatures {
            bail!("partial input {index} minimumSignatures mismatch");
        }
        if partial_input.derivation_path != manifest_utxo.derivation_path {
            bail!("partial input {index} derivationPath mismatch");
        }
        let expected_pairs = derived_extended_public_keys(
            &manifest.multisig.extended_public_keys,
            &manifest_utxo.derivation_path,
            parse_network_prefix(&manifest.network)?,
        )?;
        if partial_input.pub_key_signature_pairs.len() != expected_pairs.len() {
            bail!("partial input {index} pubKeySignaturePairs length mismatch");
        }
        for (pair, expected) in partial_input.pub_key_signature_pairs.iter().zip(expected_pairs) {
            if pair.extended_pub_key != expected {
                bail!("partial input {index} extended public key order mismatch");
            }
            if !allow_signatures && !pair.signature.is_empty() {
                bail!(
                    "partial input {index} contains signatures; pass --allow-signatures to verify signed artifacts"
                );
            }
        }
    }

    Ok(())
}

fn verify_outputs(tx: &KaspaTransaction, manifest: &UnsignedExitManifest) -> Result<()> {
    let network_prefix = parse_network_prefix(&manifest.network)?;
    let expected_outputs = transaction_outputs(
        &manifest.exits,
        manifest.change.as_ref(),
        &manifest.multisig,
        network_prefix,
    )?;
    if tx.outputs != expected_outputs {
        bail!("transaction outputs do not match manifest exits");
    }

    let input_total = manifest.locking_utxos.iter().map(|utxo| utxo.amount_sompi).sum::<u64>();
    let output_total = tx.outputs.iter().map(|output| output.value).sum::<u64>();
    if input_total != output_total.saturating_add(manifest.fee_sompi) {
        bail!("fee arithmetic mismatch");
    }
    if output_total != manifest.total_output_sompi || input_total != manifest.total_input_sompi {
        bail!("manifest totals do not match transaction");
    }

    Ok(())
}

fn verify_mass_manifest(tx: &KaspaTransaction, manifest: &UnsignedExitManifest) -> Result<()> {
    let Some(expected) = manifest.mass.as_ref() else {
        return Ok(());
    };

    let actual = calculate_mass_preflight(
        tx,
        &BuildExitInput {
            locking_utxos: manifest.locking_utxos.clone(),
            exits: manifest.exits.clone(),
            change: manifest.change.clone(),
            fee_sompi: manifest.fee_sompi,
            fee_kas: manifest.fee_kas.clone(),
            multisig: manifest.multisig.clone(),
        },
        &manifest.network,
    )?;
    if &actual != expected {
        bail!("manifest mass preflight does not match transaction data");
    }

    Ok(())
}

fn calculate_mass_preflight(
    tx: &KaspaTransaction,
    input: &BuildExitInput,
    network: &str,
) -> Result<KaspaMassManifest> {
    let params = KaspaParams::from(parse_kaspa_network_type(network)?);
    let calculator = KaspaMassCalculator::new_with_consensus_params(&params);
    let mass_cofactors = params.mempool_block_mass_cofactors().after();
    let storage_mass = calculate_storage_mass(tx, input, &params)?;

    let mut signed_tx_for_mass = tx.clone();
    apply_kaspawallet_signature_placeholders(&mut signed_tx_for_mass, input)?;
    let non_contextual = calculator.calc_non_contextual_masses(&signed_tx_for_mass);

    let estimated_signed_compute_mass = non_contextual.compute_mass;
    let transient_mass = non_contextual.transient_mass;
    let effective_mass = KaspaMass::new(non_contextual, KaspaContextualMasses::new(storage_mass))
        .normalized_max(&mass_cofactors);
    let minimum_relay_fee_sompi = minimum_required_transaction_relay_fee(effective_mass);

    Ok(KaspaMassManifest {
        estimated_signed_compute_mass,
        transient_mass,
        storage_mass,
        effective_mass,
        fee_sompi: input.fee_sompi,
        fee_kas: sompi_to_kas_string(input.fee_sompi),
        minimum_relay_fee_sompi,
        minimum_relay_fee_kas: sompi_to_kas_string(minimum_relay_fee_sompi),
        standard_transaction_mass_limit: KASPA_MAXIMUM_STANDARD_TRANSACTION_MASS,
        block_mass_limit: mass_cofactors.reference,
        standard_limit_exceeded: estimated_signed_compute_mass
            > KASPA_MAXIMUM_STANDARD_TRANSACTION_MASS
            || transient_mass > KASPA_MAXIMUM_STANDARD_TRANSACTION_MASS
            || storage_mass > KASPA_MAXIMUM_STANDARD_TRANSACTION_MASS,
        block_limit_exceeded: effective_mass > mass_cofactors.reference,
        fee_below_minimum_relay: input.fee_sompi < minimum_relay_fee_sompi,
    })
}

fn calculate_storage_mass(
    tx: &KaspaTransaction,
    input: &BuildExitInput,
    params: &KaspaParams,
) -> Result<u64> {
    let input_cells = input
        .locking_utxos
        .iter()
        .map(|utxo| {
            let script = decode_fixed_hex(&utxo.script_public_key.script, "locking UTXO script")?;
            let entry = UtxoEntry::new(
                utxo.amount_sompi,
                ScriptPublicKey::from_vec(utxo.script_public_key.version, script),
                0,
                false,
                None,
            );
            Ok(UtxoCell::from(&entry))
        })
        .collect::<Result<Vec<_>>>()?;
    let output_cells = tx.outputs.iter().map(UtxoCell::from).collect::<Vec<_>>();
    calc_storage_mass(
        tx.is_coinbase(),
        input_cells.into_iter(),
        output_cells.into_iter(),
        params.storage_mass_parameter,
    )
    .ok_or_else(|| eyre!("failed to calculate Kaspa storage mass"))
}

fn apply_kaspawallet_signature_placeholders(
    tx: &mut KaspaTransaction,
    input: &BuildExitInput,
) -> Result<()> {
    let sig_op_count = u8::try_from(input.multisig.extended_public_keys.len())
        .wrap_err("multisig key count exceeds Kaspa sigOpCount range")?;
    let tx_version = tx.version;

    for (tx_input, utxo) in tx.inputs.iter_mut().zip(&input.locking_utxos) {
        apply_kaspa_input_signature_mass(tx_input, tx_version, sig_op_count);
        tx_input.signature_script =
            signature_script_placeholder_for_mass(&input.multisig, &utxo.derivation_path)?;
    }
    tx.finalize();
    Ok(())
}

fn signature_script_placeholder_for_mass(
    multisig: &MultisigSpec,
    derivation_path: &str,
) -> Result<Vec<u8>> {
    let mut script_builder = ScriptBuilder::new();
    let signature = vec![0_u8; KASPA_SIGNATURE_SIZE_WITH_HASH_TYPE];
    let signature_count = multisig.minimum_signatures.max(1) as usize;

    if multisig.extended_public_keys.len() > 1 {
        for _ in 0..signature_count {
            script_builder.add_data(&signature)?;
        }
        let redeem_script = multisig_redeem_script_for_path(multisig, derivation_path)?;
        script_builder.add_data(&redeem_script)?;
    } else {
        script_builder.add_data(&signature)?;
    }

    Ok(script_builder.drain())
}

fn multisig_redeem_script_for_path(
    multisig: &MultisigSpec,
    derivation_path: &str,
) -> Result<Vec<u8>> {
    let path = derivation_path
        .parse::<KaspaDerivationPath>()
        .wrap_err_with(|| format!("invalid Kaspa derivation path `{derivation_path}`"))?;
    let derived = multisig
        .extended_public_keys
        .iter()
        .map(|key| {
            let xpub = key
                .parse::<KaspaExtendedPublicKey<KaspaSecpPublicKey>>()
                .wrap_err_with(|| format!("invalid Kaspa extended public key `{key}`"))?;
            Ok(xpub.derive_path(&path)?)
        })
        .collect::<Result<Vec<_>>>()?;

    if multisig.ecdsa {
        let public_keys = derived.iter().map(|xpub| xpub.public_key().to_bytes());
        multisig_redeem_script_ecdsa(public_keys, multisig.minimum_signatures as usize)
            .wrap_err("failed to build ECDSA multisig redeem script")
    } else {
        let public_keys =
            derived.iter().map(|xpub| xpub.public_key().x_only_public_key().0.serialize());
        multisig_redeem_script(public_keys, multisig.minimum_signatures as usize)
            .wrap_err("failed to build multisig redeem script")
    }
}

fn validate_mass_preflight(
    mass: &KaspaMassManifest,
    input: &BuildExitInput,
    network_prefix: KaspaAddressPrefix,
    network: &str,
    allow_mass_limit_override_for_testing: bool,
) -> Result<()> {
    if allow_mass_limit_override_for_testing {
        return Ok(());
    }

    let mut failures = Vec::new();
    if mass.estimated_signed_compute_mass > mass.standard_transaction_mass_limit {
        failures.push(format!(
            "estimated signed compute mass {} exceeds standard limit {}",
            mass.estimated_signed_compute_mass, mass.standard_transaction_mass_limit
        ));
    }
    if mass.transient_mass > mass.standard_transaction_mass_limit {
        failures.push(format!(
            "transient mass {} exceeds standard limit {}",
            mass.transient_mass, mass.standard_transaction_mass_limit
        ));
    }
    if mass.storage_mass > mass.standard_transaction_mass_limit {
        failures.push(format!(
            "storage mass {} exceeds standard limit {}",
            mass.storage_mass, mass.standard_transaction_mass_limit
        ));
    }
    if mass.effective_mass > mass.block_mass_limit {
        failures.push(format!(
            "effective mass {} exceeds block mass limit {}",
            mass.effective_mass, mass.block_mass_limit
        ));
    }
    if mass.fee_below_minimum_relay {
        failures.push(format!(
            "fee {} sompi is below minimum relay fee {} sompi for effective mass {}",
            mass.fee_sompi, mass.minimum_relay_fee_sompi, mass.effective_mass
        ));
    }

    if failures.is_empty() {
        return Ok(());
    }

    let suggestion = mass_rejection_suggestion(input, network_prefix, network)
        .map(|suggestion| format!(" {suggestion}"))
        .unwrap_or_else(|err| format!(" Unable to estimate a smaller batch suggestion: {err}"));

    bail!(
        "Kaspa mass preflight failed: {}.{} This transaction is expected to be rejected by standard kaspad/kaspawallet broadcast; reduce the number of small outputs, increase output amounts, add more aggregate input value, or raise the fee as applicable. Pass --allow-mass-limit-override-for-testing only for non-broadcast signing rehearsals",
        failures.join("; "),
        suggestion
    )
}

fn mass_rejection_suggestion(
    input: &BuildExitInput,
    network_prefix: KaspaAddressPrefix,
    network: &str,
) -> Result<String> {
    let total_input_sompi = input
        .locking_utxos
        .iter()
        .try_fold(0_u64, |sum, utxo| sum.checked_add(utxo.amount_sompi))
        .ok_or_else(|| eyre!("input total overflows u64"))?;

    let mut best: Option<(usize, u64, KaspaMassManifest)> = None;
    let mut first_candidate: Option<KaspaMassManifest> = None;

    for exit_count in 1..=input.exits.len() {
        let selected_exit_sompi = input.exits[..exit_count]
            .iter()
            .try_fold(0_u64, |sum, exit| sum.checked_add(exit.amount_sompi))
            .ok_or_else(|| eyre!("exit total overflows u64"))?;
        let required_without_change = selected_exit_sompi
            .checked_add(input.fee_sompi)
            .ok_or_else(|| eyre!("exit total plus fee overflows u64"))?;
        if required_without_change > total_input_sompi {
            break;
        }

        let change_sompi = total_input_sompi - required_without_change;
        let Some(candidate_input) =
            input_with_exit_prefix_and_recalculated_change(input, exit_count, change_sompi)
        else {
            continue;
        };
        let mass = calculate_input_mass_for_suggestion(&candidate_input, network_prefix, network)?;
        if exit_count == 1 {
            first_candidate = Some(mass.clone());
        }
        if mass_preflight_passes(&mass) {
            best = Some((exit_count, change_sompi, mass));
        }
    }

    if let Some((exit_count, change_sompi, mass)) = best {
        return Ok(format!(
            "Estimated smaller-batch suggestion: at most {exit_count} exit(s) from this input set should fit standard policy if change is recalculated to {} sompi ({} KAS); estimated effective mass {}, storage mass {}, minimum relay fee {} sompi ({} KAS). Re-run build and verify before signing.",
            change_sompi,
            sompi_to_kas_string(change_sompi),
            mass.effective_mass,
            mass.storage_mass,
            mass.minimum_relay_fee_sompi,
            mass.minimum_relay_fee_kas,
        ));
    }

    if let Some(mass) = first_candidate {
        return Ok(format!(
            "Estimated smaller-batch suggestion: 0 positive exits from this input set appear to fit standard policy; even 1 exit estimates effective mass {}, storage mass {}, minimum relay fee {} sompi ({} KAS).",
            mass.effective_mass,
            mass.storage_mass,
            mass.minimum_relay_fee_sompi,
            mass.minimum_relay_fee_kas,
        ));
    }

    Ok("Estimated smaller-batch suggestion: no valid smaller exit batch could be estimated from this input; provide a change output path or reduce requested spend.".to_string())
}

fn input_with_exit_prefix_and_recalculated_change(
    input: &BuildExitInput,
    exit_count: usize,
    change_sompi: u64,
) -> Option<BuildExitInput> {
    let mut candidate = input.clone();
    candidate.exits.truncate(exit_count);
    candidate.change = if change_sompi == 0 {
        None
    } else {
        let mut change = input.change.clone()?;
        change.amount_sompi = change_sompi;
        change.amount_kas = Some(sompi_to_kas_string(change_sompi));
        Some(change)
    };
    Some(candidate)
}

fn calculate_input_mass_for_suggestion(
    input: &BuildExitInput,
    network_prefix: KaspaAddressPrefix,
    network: &str,
) -> Result<KaspaMassManifest> {
    let inputs = input
        .locking_utxos
        .iter()
        .map(|utxo| {
            let txid = parse_transaction_id(&utxo.transaction_id)?;
            Ok(KaspaTransactionInput::new(
                TransactionOutpoint::new(txid, utxo.index),
                Vec::new(),
                0,
                0,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let outputs =
        transaction_outputs(&input.exits, input.change.as_ref(), &input.multisig, network_prefix)?;
    let payload =
        build_payload_with_nonce(IGRA_EXIT_PAYLOAD_HEADER, &exit_l2data(&input.exits)?, 0);
    let tx = KaspaTransaction::new(0, inputs, outputs, 0, SubnetworkId::default(), 0, payload);
    calculate_mass_preflight(&tx, input, network)
}

fn mass_preflight_passes(mass: &KaspaMassManifest) -> bool {
    !mass.standard_limit_exceeded && !mass.block_limit_exceeded && !mass.fee_below_minimum_relay
}

fn minimum_required_transaction_relay_fee(mass: u64) -> u64 {
    let fee = mass.saturating_mul(KASPA_MINIMUM_RELAY_TRANSACTION_FEE) / 1_000;
    fee.max(KASPA_MINIMUM_RELAY_TRANSACTION_FEE)
}

fn verify_payload(payload: &[u8], exits: &[ExitRequest]) -> Result<u32> {
    let expected_len = 1usize
        .checked_add(
            exits.len().checked_mul(32).ok_or_else(|| eyre!("exit payload length overflow"))?,
        )
        .and_then(|len| len.checked_add(4))
        .ok_or_else(|| eyre!("exit payload length overflow"))?;
    if payload.len() != expected_len {
        bail!("exit payload length mismatch: expected {expected_len}, actual {}", payload.len());
    }
    if payload.first().copied() != Some(IGRA_EXIT_PAYLOAD_HEADER) {
        bail!("exit payload header must be 0x93");
    }
    let mut offset = 1;
    for (index, exit) in exits.iter().enumerate() {
        let expected = decode_fixed_hex(&exit.message_id, "message id")?;
        let actual = &payload[offset..offset + 32];
        if actual != expected.as_slice() {
            bail!("payload message id {index} mismatch");
        }
        offset += 32;
    }
    Ok(u32::from_be_bytes(payload[offset..offset + 4].try_into()?))
}

fn partially_signed_transaction_proto(
    tx: &KaspaTransaction,
    input: &BuildExitInput,
    network_prefix: KaspaAddressPrefix,
) -> Result<PartiallySignedTransactionProto> {
    let partial_inputs = input
        .locking_utxos
        .iter()
        .map(|utxo| {
            let prev_script =
                decode_fixed_hex(&utxo.script_public_key.script, "locking UTXO script")?;
            Ok(PartiallySignedInputProto {
                redeem_script: Vec::new(),
                prev_output: Some(TransactionOutputProto {
                    value: utxo.amount_sompi,
                    script_public_key: Some(ScriptPublicKeyProto {
                        script: prev_script,
                        version: utxo.script_public_key.version as u32,
                    }),
                }),
                minimum_signatures: input.multisig.minimum_signatures,
                pub_key_signature_pairs: derived_extended_public_keys(
                    &input.multisig.extended_public_keys,
                    &utxo.derivation_path,
                    network_prefix,
                )?
                .into_iter()
                .map(|extended_pub_key| PubKeySignaturePairProto {
                    extended_pub_key,
                    signature: Vec::new(),
                })
                .collect(),
                derivation_path: utxo.derivation_path.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(PartiallySignedTransactionProto {
        tx: Some(transaction_to_proto(tx)?),
        partially_signed_inputs: partial_inputs,
    })
}

fn transaction_to_proto(tx: &KaspaTransaction) -> Result<TransactionMessageProto> {
    Ok(TransactionMessageProto {
        version: tx.version as u32,
        inputs: tx
            .inputs
            .iter()
            .map(|input| {
                let (sig_op_count, compute_budget) =
                    kaspa_input_proto_mass_fields(tx.version, input)?;
                Ok(TransactionInputProto {
                    previous_outpoint: Some(OutpointProto {
                        transaction_id: Some(TransactionIdProto {
                            bytes: input.previous_outpoint.transaction_id.as_bytes().to_vec(),
                        }),
                        index: input.previous_outpoint.index,
                    }),
                    signature_script: input.signature_script.clone(),
                    sequence: input.sequence,
                    sig_op_count,
                    compute_budget,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        outputs: tx.outputs.iter().map(transaction_output_to_proto).collect(),
        lock_time: tx.lock_time,
        subnetwork_id: Some(SubnetworkIdProto {
            bytes: <SubnetworkId as AsRef<[u8]>>::as_ref(&tx.subnetwork_id).to_vec(),
        }),
        gas: tx.gas,
        payload: tx.payload.clone(),
    })
}

fn transaction_output_to_proto(output: &KaspaTransactionOutput) -> TransactionOutputProto {
    TransactionOutputProto {
        value: output.value,
        script_public_key: Some(ScriptPublicKeyProto {
            script: output.script_public_key.script().to_vec(),
            version: output.script_public_key.version() as u32,
        }),
    }
}

fn transaction_from_proto(proto: &TransactionMessageProto) -> Result<KaspaTransaction> {
    if proto.version > u16::MAX as u32 {
        bail!("transaction version is too large");
    }
    let inputs = proto
        .inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let outpoint = input
                .previous_outpoint
                .as_ref()
                .ok_or_else(|| eyre!("input {index} missing previousOutpoint"))?;
            let txid = outpoint
                .transaction_id
                .as_ref()
                .ok_or_else(|| eyre!("input {index} missing transactionId"))?;
            if txid.bytes.len() != 32 {
                bail!("input {index} transactionId must be 32 bytes");
            }
            kaspa_transaction_input_from_proto(
                proto.version as u16,
                TransactionOutpoint::new(TransactionId::from_slice(&txid.bytes), outpoint.index),
                input.signature_script.clone(),
                input.sequence,
                input.sig_op_count,
                input.compute_budget,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let outputs = proto
        .outputs
        .iter()
        .enumerate()
        .map(|(index, output)| {
            let spk = output
                .script_public_key
                .as_ref()
                .ok_or_else(|| eyre!("output {index} missing scriptPublicKey"))?;
            if spk.version > u16::MAX as u32 {
                bail!("output {index} scriptPublicKey version is too large");
            }
            Ok(KaspaTransactionOutput::new(
                output.value,
                ScriptPublicKey::from_vec(spk.version as u16, spk.script.clone()),
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    let subnetwork_id = match &proto.subnetwork_id {
        Some(id) => {
            if id.bytes.len() != 20 {
                bail!("subnetworkId must be exactly 20 bytes");
            }
            SubnetworkId::from_bytes(id.bytes.as_slice().try_into()?)
        }
        None => SubnetworkId::default(),
    };

    Ok(KaspaTransaction::new(
        proto.version as u16,
        inputs,
        outputs,
        proto.lock_time,
        subnetwork_id,
        proto.gas,
        proto.payload.clone(),
    ))
}

fn mine_payload_nonce(
    tx: &mut KaspaTransaction,
    tx_id_prefix: &[u8],
    timeout: Duration,
    max_nonce: Option<u32>,
) -> Result<u32> {
    let nonce_offset = tx.payload.len().checked_sub(4).ok_or_else(|| eyre!("payload too short"))?;
    let started = std::time::Instant::now();
    let max_nonce = max_nonce.unwrap_or(u32::MAX);
    let mut nonce = 0_u32;

    loop {
        tx.payload[nonce_offset..].copy_from_slice(&nonce.to_be_bytes());
        tx.finalize();
        if tx.id().as_bytes().starts_with(tx_id_prefix) {
            return Ok(nonce);
        }
        if nonce == max_nonce {
            bail!(
                "unable to mine IGRA txid prefix {} before max nonce {max_nonce}",
                prefixed_hex(tx_id_prefix)
            );
        }
        if !timeout.is_zero() && started.elapsed() >= timeout {
            bail!(
                "timed out mining IGRA txid prefix {} after {} seconds",
                prefixed_hex(tx_id_prefix),
                timeout.as_secs()
            );
        }
        nonce = nonce.saturating_add(1);
    }
}

fn exit_l2data(exits: &[ExitRequest]) -> Result<Vec<u8>> {
    let mut l2data = Vec::with_capacity(exits.len() * 32);
    for exit in exits {
        let message_id = decode_fixed_hex(&exit.message_id, "message id")?;
        l2data.extend_from_slice(&message_id);
    }
    Ok(l2data)
}

fn exit_outputs(
    exits: &[ExitRequest],
    network_prefix: KaspaAddressPrefix,
) -> Result<Vec<KaspaTransactionOutput>> {
    exits
        .iter()
        .enumerate()
        .map(|(index, exit)| {
            let address = KaspaAddress::try_from(exit.recipient.as_str())
                .wrap_err_with(|| format!("failed to parse exits[{index}].recipient"))?;
            if address.prefix != network_prefix {
                bail!(
                    "exits[{index}].recipient prefix {} does not match network prefix {network_prefix}",
                    address.prefix
                );
            }
            Ok(KaspaTransactionOutput::new(exit.amount_sompi, pay_to_address_script(&address)))
        })
        .collect()
}

fn transaction_outputs(
    exits: &[ExitRequest],
    change: Option<&ChangeOutput>,
    multisig: &MultisigSpec,
    network_prefix: KaspaAddressPrefix,
) -> Result<Vec<KaspaTransactionOutput>> {
    let mut outputs = exit_outputs(exits, network_prefix)?;
    if let Some(change) = change {
        outputs.push(multisig_change_output(change, multisig)?);
    }
    Ok(outputs)
}

fn multisig_change_output(
    change: &ChangeOutput,
    multisig: &MultisigSpec,
) -> Result<KaspaTransactionOutput> {
    Ok(KaspaTransactionOutput::new(
        change.amount_sompi,
        multisig_change_script_public_key(change, multisig)?,
    ))
}

fn multisig_change_script_public_key(
    change: &ChangeOutput,
    multisig: &MultisigSpec,
) -> Result<ScriptPublicKey> {
    validate_canonical_multisig_receive_derivation_path(
        &change.derivation_path,
        "change.derivation_path",
    )?;

    let path = change.derivation_path.parse::<KaspaDerivationPath>().wrap_err_with(|| {
        format!("invalid Kaspa change derivation path `{}`", change.derivation_path)
    })?;
    let derived = multisig
        .extended_public_keys
        .iter()
        .map(|key| {
            let xpub = key
                .parse::<KaspaExtendedPublicKey<KaspaSecpPublicKey>>()
                .wrap_err_with(|| format!("invalid Kaspa extended public key `{key}`"))?;
            Ok(xpub.derive_path(&path)?)
        })
        .collect::<Result<Vec<_>>>()?;

    let redeem_script = if multisig.ecdsa {
        let public_keys = derived.iter().map(|xpub| xpub.public_key().to_bytes());
        multisig_redeem_script_ecdsa(public_keys, multisig.minimum_signatures as usize)?
    } else {
        let public_keys =
            derived.iter().map(|xpub| xpub.public_key().x_only_public_key().0.serialize());
        multisig_redeem_script(public_keys, multisig.minimum_signatures as usize)?
    };

    Ok(pay_to_script_hash_script(&redeem_script))
}

fn multisig_change_address(
    change: &ChangeOutput,
    multisig: &MultisigSpec,
    network_prefix: KaspaAddressPrefix,
) -> Result<String> {
    let script_public_key = multisig_change_script_public_key(change, multisig)?;
    Ok(extract_script_pub_key_address(&script_public_key, network_prefix)
        .wrap_err("failed to derive multisig change address from derivation_path")?
        .to_string())
}

fn derived_extended_public_keys(
    extended_public_keys: &[String],
    derivation_path: &str,
    network_prefix: KaspaAddressPrefix,
) -> Result<Vec<String>> {
    let path = derivation_path
        .parse::<KaspaDerivationPath>()
        .wrap_err_with(|| format!("invalid Kaspa derivation path `{derivation_path}`"))?;
    let output_prefix = bip32_public_prefix(network_prefix);

    extended_public_keys
        .iter()
        .map(|key| {
            let xpub = key
                .parse::<KaspaExtendedPublicKey<KaspaSecpPublicKey>>()
                .wrap_err_with(|| format!("invalid Kaspa extended public key `{key}`"))?;
            Ok(xpub.derive_path(&path)?.to_string(Some(output_prefix)))
        })
        .collect()
}

fn bip32_public_prefix(network_prefix: KaspaAddressPrefix) -> KaspaBip32Prefix {
    match network_prefix {
        KaspaAddressPrefix::Mainnet => KaspaBip32Prefix::KPUB,
        KaspaAddressPrefix::Testnet | KaspaAddressPrefix::Devnet | KaspaAddressPrefix::Simnet => {
            KaspaBip32Prefix::KTUB
        }
    }
}

fn parse_network_prefix(network: &str) -> Result<KaspaAddressPrefix> {
    match network {
        "mainnet" => Ok(KaspaAddressPrefix::Mainnet),
        "testnet-10" | "testnet" => Ok(KaspaAddressPrefix::Testnet),
        "devnet" => Ok(KaspaAddressPrefix::Devnet),
        "simnet" => Ok(KaspaAddressPrefix::Simnet),
        _ => bail!(
            "unsupported Kaspa network `{network}`; use mainnet, testnet-10, devnet, or simnet"
        ),
    }
}

fn parse_kaspa_network_type(network: &str) -> Result<KaspaNetworkType> {
    match network {
        "mainnet" => Ok(KaspaNetworkType::Mainnet),
        "testnet-10" | "testnet" => Ok(KaspaNetworkType::Testnet),
        "devnet" => Ok(KaspaNetworkType::Devnet),
        "simnet" => Ok(KaspaNetworkType::Simnet),
        _ => bail!(
            "unsupported Kaspa network `{network}`; use mainnet, testnet-10, devnet, or simnet"
        ),
    }
}

fn parse_transaction_id(value: &str) -> Result<TransactionId> {
    let normalized = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    if normalized.len() != 64 {
        bail!("transaction id must be exactly 32 bytes");
    }
    TransactionId::from_str(normalized).wrap_err("failed to parse transaction id")
}

fn decode_wallet_hex(wallet_hex: &str) -> Result<Vec<u8>> {
    let trimmed = wallet_hex.trim();
    if trimmed.contains('_') {
        bail!(
            "wallet hex must contain exactly one transaction; `_`-joined multi-transaction files are not supported"
        );
    }
    decode_fixed_hex(trimmed, "wallet hex")
}

fn decode_fixed_hex(value: &str, field: &str) -> Result<Vec<u8>> {
    let normalized = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    if normalized.len() % 2 != 0 {
        bail!("{field} must have an even number of hex characters");
    }
    hex::decode(normalized).wrap_err_with(|| format!("failed to decode {field} hex"))
}

fn deserialize_optional_kas_amount<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value)),
        Some(serde_json::Value::Number(value)) => Ok(Some(value.to_string())),
        Some(_) => Err(serde::de::Error::custom("KAS amount must be a string or number")),
    }
}

fn serialize_u32_hex<S>(value: &u32, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&format!("0x{value:08x}"))
}

fn deserialize_u32_hex_or_decimal<'de, D>(deserializer: D) -> std::result::Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(number) => {
            let value = number
                .as_u64()
                .ok_or_else(|| serde::de::Error::custom("nonce must be an unsigned integer"))?;
            u32::try_from(value).map_err(|_| serde::de::Error::custom("nonce exceeds u32 range"))
        }
        serde_json::Value::String(value) => parse_u32_hex_or_decimal(&value)
            .map_err(|err| serde::de::Error::custom(format!("invalid nonce: {err}"))),
        _ => Err(serde::de::Error::custom("nonce must be a hex string or unsigned integer")),
    }
}

fn parse_u32_hex_or_decimal(value: &str) -> Result<u32> {
    let value = value.trim();
    if value.is_empty() {
        bail!("nonce cannot be empty");
    }
    if let Some(hex) = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        if hex.is_empty() {
            bail!("hex nonce cannot be empty");
        }
        if hex.len() > 8 {
            bail!("hex nonce exceeds 4 bytes");
        }
        return u32::from_str_radix(hex, 16).wrap_err("failed to parse hex nonce");
    }
    value.parse::<u32>().wrap_err("failed to parse decimal nonce")
}

fn sompi_to_kas_string(sompi: u64) -> String {
    format!("{}.{:08}", sompi / SOMPI_PER_KAS, sompi % SOMPI_PER_KAS)
}

fn kas_string_to_sompi(amount_kas: &str) -> Result<u64> {
    let value = amount_kas.trim();
    if value.is_empty() {
        bail!("KAS amount cannot be empty");
    }
    if value.starts_with('-') {
        bail!("KAS amount cannot be negative");
    }

    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fractional = parts.next();
    if parts.next().is_some() {
        bail!("KAS amount has more than one decimal point");
    }
    if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("KAS amount whole part must contain only digits");
    }

    let whole_sompi = whole
        .parse::<u64>()?
        .checked_mul(SOMPI_PER_KAS)
        .ok_or_else(|| eyre!("KAS amount overflows u64 sompi"))?;
    let fractional_sompi = if let Some(fractional) = fractional {
        if fractional.len() > 8 {
            bail!("KAS amount cannot have more than 8 decimal places");
        }
        if !fractional.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("KAS amount fractional part must contain only digits");
        }
        let padded = format!("{fractional:0<8}");
        padded.parse::<u64>()?
    } else {
        0
    };

    whole_sompi.checked_add(fractional_sompi).ok_or_else(|| eyre!("KAS amount overflows u64 sompi"))
}

fn build_payload_with_nonce(header: u8, l2data: &[u8], nonce: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + l2data.len() + 4);
    payload.push(header);
    payload.extend_from_slice(l2data);
    payload.extend_from_slice(&nonce.to_be_bytes());
    payload
}

fn kaspa_tx_version_for_subnetwork_id(subnetwork_id: &SubnetworkId) -> u16 {
    if *subnetwork_id == SubnetworkId::default() {
        KASPA_TX_VERSION_NATIVE
    } else {
        KASPA_TX_VERSION_TOCCATA
    }
}

fn kaspa_tx_version_uses_compute_budget(version: u16) -> bool {
    KaspaComputeCommit::version_expects_compute_budget_field(version)
}

fn kaspa_transaction_input_for_version(
    version: u16,
    previous_outpoint: TransactionOutpoint,
    signature_script: Vec<u8>,
    sequence: u64,
) -> KaspaTransactionInput {
    if kaspa_tx_version_uses_compute_budget(version) {
        KaspaTransactionInput::new_with_compute_budget(
            previous_outpoint,
            signature_script,
            sequence,
            KASPA_TOCCATA_COMPUTE_BUDGET_PER_INPUT,
        )
    } else {
        KaspaTransactionInput::new(previous_outpoint, signature_script, sequence, 0)
    }
}

fn kaspa_transaction_input_from_proto(
    version: u16,
    previous_outpoint: TransactionOutpoint,
    signature_script: Vec<u8>,
    sequence: u64,
    sig_op_count: u32,
    compute_budget: u32,
) -> Result<KaspaTransactionInput> {
    if kaspa_tx_version_uses_compute_budget(version) {
        let compute_field = if compute_budget == 0 {
            // Older kaspawallet PSTs had no computeBudget field. Accept the
            // legacy overloaded sigOpCount slot so previously built artifacts
            // can still be decoded, then normalize before broadcast.
            sig_op_count
        } else {
            compute_budget
        };
        let compute_budget = u16::try_from(compute_field)
            .wrap_err("input computeBudget is too large for Kaspa v1 transaction")?;
        Ok(KaspaTransactionInput::new_with_compute_budget(
            previous_outpoint,
            signature_script,
            sequence,
            compute_budget,
        ))
    } else {
        if compute_budget != 0 {
            bail!("input computeBudget is invalid for Kaspa v0 transaction");
        }
        let sig_op_count = u8::try_from(sig_op_count)
            .wrap_err("input sigOpCount is too large for Kaspa v0 transaction")?;
        Ok(KaspaTransactionInput::new(previous_outpoint, signature_script, sequence, sig_op_count))
    }
}

fn kaspa_input_proto_mass_fields(
    version: u16,
    input: &KaspaTransactionInput,
) -> Result<(u32, u32)> {
    if kaspa_tx_version_uses_compute_budget(version) {
        let compute_budget = input
            .compute_commit
            .compute_budget()
            .ok_or_else(|| eyre!("Kaspa v1 input is missing compute budget"))?;
        Ok((0, compute_budget as u32))
    } else {
        let sig_op_count = input
            .compute_commit
            .sig_op_count()
            .ok_or_else(|| eyre!("Kaspa v0 input is missing sigOpCount"))?;
        Ok((sig_op_count as u32, 0))
    }
}

fn apply_kaspa_input_signature_mass(
    input: &mut KaspaTransactionInput,
    version: u16,
    sig_op_count: u8,
) {
    if kaspa_tx_version_uses_compute_budget(version) {
        let compute_budget = input
            .compute_commit
            .compute_budget()
            .unwrap_or_default()
            .max(KASPA_TOCCATA_COMPUTE_BUDGET_PER_INPUT);
        input.compute_commit = KaspaComputeCommit::ComputeBudget(compute_budget.into());
    } else {
        input.compute_commit = KaspaComputeCommit::SigopCount(sig_op_count.into());
    }
}

fn tx_input_contains_signature_material(version: u16, input: &KaspaTransactionInput) -> bool {
    !input.signature_script.is_empty()
        || (!kaspa_tx_version_uses_compute_budget(version)
            && input.compute_commit.sig_op_count().unwrap_or_default() != 0)
}

fn parse_igra_lane_id(value: &str) -> Result<SubnetworkId> {
    let value = value.trim().trim_start_matches("0x").trim_start_matches("0X");
    if value.is_empty() {
        bail!("lane-id cannot be empty");
    }
    if !value.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        bail!("lane-id must be hex-encoded");
    }

    let subnetwork_id = match value.len() {
        8 => {
            let mut namespace = [0u8; KASPA_SUBNETWORK_NAMESPACE_LEN];
            hex::decode_to_slice(value, &mut namespace)
                .wrap_err("failed to decode lane-id namespace hex")?;
            let mut bytes = [0u8; SUBNETWORK_ID_SIZE];
            bytes[..KASPA_SUBNETWORK_NAMESPACE_LEN].copy_from_slice(&namespace);
            SubnetworkId::from_bytes(bytes)
        }
        40 => {
            let mut bytes = [0u8; SUBNETWORK_ID_SIZE];
            hex::decode_to_slice(value, &mut bytes)
                .wrap_err("failed to decode lane-id subnetwork hex")?;
            SubnetworkId::from_bytes(bytes)
        }
        len => {
            bail!(
                "lane-id expected 8 hex chars (4-byte namespace) or 40 hex chars (20-byte subnetwork id), got {len}"
            );
        }
    };

    let bytes: &[u8; SUBNETWORK_ID_SIZE] = subnetwork_id.as_ref();
    if bytes[1..].iter().all(|byte| *byte == 0) {
        bail!("lane-id reserved system lane shape is not allowed");
    }
    if bytes[KASPA_SUBNETWORK_NAMESPACE_LEN..].iter().any(|byte| *byte != 0) {
        bail!("lane-id full lane id must use user-lane shape [namespace(4), zero_tail(16)]");
    }

    Ok(subnetwork_id)
}

fn normalize_lane_id(value: &str) -> Result<String> {
    let value = value.trim().trim_start_matches("0x").trim_start_matches("0X");
    parse_igra_lane_id(value)?;
    Ok(format!("0x{}", value.to_ascii_lowercase()))
}

fn expected_manifest_subnetwork_id(
    protocol: &ExitProtocolManifest,
) -> Result<Option<SubnetworkId>> {
    if let Some(subnetwork_id) = protocol.subnetwork_id.as_deref() {
        let parsed = parse_igra_lane_id(subnetwork_id)
            .wrap_err("manifest protocol.subnetwork_id is invalid")?;
        if let Some(lane_id) = protocol.lane_id.as_deref() {
            let lane =
                parse_igra_lane_id(lane_id).wrap_err("manifest protocol.lane_id is invalid")?;
            if lane != parsed {
                bail!("manifest protocol.lane_id does not match protocol.subnetwork_id");
            }
        }
        return Ok(Some(parsed));
    }

    protocol
        .lane_id
        .as_deref()
        .map(|lane_id| parse_igra_lane_id(lane_id).wrap_err("manifest protocol.lane_id is invalid"))
        .transpose()
}

fn kas_locking_script() -> Result<Vec<u8>> {
    decode_fixed_hex(KAS_LOCKING_SCRIPT_HEX, "KAS locking script")
}

fn has_duplicates(values: &[String]) -> bool {
    let mut sorted = values.iter().map(String::as_str).collect::<Vec<_>>();
    sorted.sort_unstable();
    sorted.windows(2).any(|window| window[0] == window[1])
}

fn prefixed_hex(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_multisig_receive_derivation_path(address_index: u32) -> String {
        format!(
            "m/{}/{}/{}",
            KASPAWALLET_CANONICAL_COSIGNER_INDEX, KASPAWALLET_EXTERNAL_KEYCHAIN, address_index
        )
    }

    fn sample_input() -> BuildExitInput {
        BuildExitInput {
            locking_utxos: vec![LockingUtxo {
                transaction_id: "1111111111111111111111111111111111111111111111111111111111111111"
                    .to_string(),
                index: 0,
                amount_sompi: 300_010_000,
                amount_kas: None,
                address: None,
                script_public_key: ScriptPublicKeyJson {
                    version: 0,
                    script: KAS_LOCKING_SCRIPT_HEX.to_string(),
                },
                derivation_path: canonical_multisig_receive_derivation_path(1),
            }],
            exits: vec![ExitRequest {
                message_id: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_string(),
                recipient: "kaspa:qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqkx9awp4e"
                    .to_string(),
                amount_sompi: 300_000_000,
                amount_kas: None,
            }],
            change: None,
            fee_sompi: 10_000,
            fee_kas: None,
            multisig: MultisigSpec {
                minimum_signatures: 2,
                extended_public_keys: vec![
                    "kpub2C2CKMtB3F5r4LEGRnS3o73omeQB3KJ5QfAzC5R3t9bpChBEZNitvn92JYeCTMtnR7oE1im7DhsxGqV72JErXFG9G3YnTHRnZPkGZLFE6PZ".to_string(),
                    "kpub2EHcK5Be8WCqCwMydYJgg99v6TxXRPn66GbtAAoArLo6ZyUQycFz3vVS5pCuCfoKRL5nsxJXxLx3FETEyKyEb8isTgM3NbL15KsprxXRXYP".to_string(),
                    "kpub2GTjWrjXXD5u3PQRMoCZGt3a9qwdRRWP2bGikSZynybJoWyYhQgJ1VPfVtfUccWfP3hqfNke4wSWqYC4Sf98GnYoktBtrELGi4Qc9xmGTUP".to_string(),
                ],
                ecdsa: false,
            },
        }
    }

    #[test]
    fn igra_lane_id_parser_accepts_canonical_namespace_and_full_id() {
        let shorthand = parse_igra_lane_id("97b10000").expect("canonical lane parses");
        let full = parse_igra_lane_id("0x97b1000000000000000000000000000000000000")
            .expect("full canonical lane parses");

        assert_eq!(shorthand, full);
        assert_eq!(prefixed_hex(shorthand.as_ref()), "0x97b1000000000000000000000000000000000000");
        assert_eq!(normalize_lane_id("0X97B10000").unwrap(), "0x97b10000");
    }

    #[test]
    fn igra_lane_id_parser_rejects_reserved_and_non_user_shapes() {
        assert!(parse_igra_lane_id("").unwrap_err().to_string().contains("cannot be empty"));
        assert!(parse_igra_lane_id("zzzzzzzz").unwrap_err().to_string().contains("hex"));
        assert!(parse_igra_lane_id("01000000").unwrap_err().to_string().contains("reserved"));
        assert!(
            parse_igra_lane_id("97b1000000000000000000000000000000000001")
                .unwrap_err()
                .to_string()
                .contains("user-lane shape")
        );
    }

    #[test]
    fn build_and_verify_unsigned_exit_roundtrip() {
        let output = build_unsigned_exit(
            sample_input(),
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .expect("build unsigned exit");

        assert_eq!(output.manifest.protocol.payload_header, "0x93");
        assert!(output.manifest.protocol.kaspa_tx_id.starts_with("00"));
        assert_eq!(
            serde_json::to_value(&output.manifest).unwrap()["protocol"]["nonce"],
            serde_json::Value::String(format!("0x{:08x}", output.manifest.protocol.nonce))
        );
        assert_eq!(output.manifest.protocol.lane_id.as_deref(), Some("0x97b10000"));
        assert_eq!(
            output.manifest.protocol.subnetwork_id.as_deref(),
            Some("0x97b1000000000000000000000000000000000000")
        );
        let decoded =
            decode_wallet_transaction(&output.wallet_hex).expect("decode wallet transaction");
        assert_eq!(decoded.version, KASPA_TX_VERSION_TOCCATA);
        assert_eq!(
            prefixed_hex(decoded.subnetwork_id.as_ref()),
            "0x97b1000000000000000000000000000000000000"
        );
        assert!(decoded.inputs.iter().all(|input| {
            input.compute_commit.compute_budget() == Some(KASPA_TOCCATA_COMPUTE_BUDGET_PER_INPUT)
        }));
        let pst = PartiallySignedTransactionProto::decode(
            decode_fixed_hex(&output.wallet_hex, "wallet hex").unwrap().as_slice(),
        )
        .expect("decode pst");
        for input in &pst.tx.as_ref().expect("pst tx").inputs {
            assert_eq!(input.sig_op_count, 0);
            assert_eq!(input.compute_budget, KASPA_TOCCATA_COMPUTE_BUDGET_PER_INPUT as u32);
        }
        assert_eq!(output.manifest.locking_utxos[0].amount_kas.as_deref(), Some("3.00010000"));
        assert_eq!(output.manifest.exits[0].amount_kas.as_deref(), Some("3.00000000"));
        assert_eq!(output.manifest.fee_kas.as_deref(), Some("0.00010000"));
        assert_eq!(output.manifest.total_input_kas.as_deref(), Some("3.00010000"));
        assert_eq!(output.manifest.total_output_kas.as_deref(), Some("3.00000000"));
        let mass = output.manifest.mass.as_ref().expect("mass preflight manifest");
        assert!(!mass.standard_limit_exceeded);
        assert!(!mass.block_limit_exceeded);
        assert!(!mass.fee_below_minimum_relay);
        assert!(mass.storage_mass <= mass.standard_transaction_mass_limit);
        assert!(
            output.manifest.locking_utxos[0]
                .address
                .as_deref()
                .is_some_and(|addr| addr.starts_with("kaspa:p"))
        );

        let report = verify_unsigned_exit(
            &output.manifest,
            &output.wallet_hex,
            VerifyExitOptions {
                allow_signatures: false,
                require_fully_signed: false,
                allow_non_igra_lock_script_for_testing: false,
            },
        )
        .expect("verify unsigned exit");
        assert_eq!(report.input_count, 1);
        assert_eq!(report.output_count, 1);
        assert!(!report.fully_signed);

        let mut tampered = output.manifest.clone();
        tampered.mass.as_mut().unwrap().storage_mass += 1;
        assert!(
            verify_unsigned_exit(
                &tampered,
                &output.wallet_hex,
                VerifyExitOptions {
                    allow_signatures: false,
                    require_fully_signed: false,
                    allow_non_igra_lock_script_for_testing: false,
                },
            )
            .unwrap_err()
            .to_string()
            .contains("manifest mass preflight does not match transaction data")
        );

        let mut tampered = output.manifest.clone();
        tampered.protocol.lane_id = Some("97b20000".to_string());
        tampered.protocol.subnetwork_id =
            Some("0x97b2000000000000000000000000000000000000".to_string());
        assert!(
            verify_unsigned_exit(
                &tampered,
                &output.wallet_hex,
                VerifyExitOptions {
                    allow_signatures: false,
                    require_fully_signed: false,
                    allow_non_igra_lock_script_for_testing: false,
                },
            )
            .unwrap_err()
            .to_string()
            .contains("transaction subnetwork id mismatch")
        );
    }

    #[test]
    fn verify_allows_signed_wallet_hex_without_requiring_original_hex_hash() {
        let output = build_unsigned_exit(
            sample_input(),
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .expect("build unsigned exit");

        let mut pst = PartiallySignedTransactionProto::decode(
            decode_fixed_hex(&output.wallet_hex, "wallet hex").unwrap().as_slice(),
        )
        .expect("decode pst");
        pst.partially_signed_inputs[0].pub_key_signature_pairs[0].signature = vec![1; 64];
        let signed_hex = hex::encode(pst.encode_to_vec());

        assert!(
            verify_unsigned_exit(
                &output.manifest,
                &signed_hex,
                VerifyExitOptions {
                    allow_signatures: false,
                    require_fully_signed: false,
                    allow_non_igra_lock_script_for_testing: false
                },
            )
            .is_err()
        );

        let report = verify_unsigned_exit(
            &output.manifest,
            &signed_hex,
            VerifyExitOptions {
                allow_signatures: true,
                require_fully_signed: false,
                allow_non_igra_lock_script_for_testing: false,
            },
        )
        .expect("verify signed exit");
        assert_eq!(report.signed_inputs, 1);
        assert!(!report.fully_signed);
    }

    #[test]
    fn build_canonicalizes_multisig_public_key_order() {
        let mut input = sample_input();
        input.multisig.extended_public_keys.reverse();

        let output = build_unsigned_exit(
            input,
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .expect("build unsigned exit");

        let mut expected = output.manifest.multisig.extended_public_keys.clone();
        expected.sort();
        assert_eq!(output.manifest.multisig.extended_public_keys, expected);

        verify_unsigned_exit(
            &output.manifest,
            &output.wallet_hex,
            VerifyExitOptions {
                allow_signatures: false,
                require_fully_signed: false,
                allow_non_igra_lock_script_for_testing: false,
            },
        )
        .expect("verify unsigned exit");
    }

    #[test]
    fn build_and_verify_exit_with_change_back_to_canonical_multisig() {
        let mut input = sample_input();
        input.locking_utxos[0].amount_sompi = 600_010_000;
        input.change = Some(ChangeOutput {
            derivation_path: canonical_multisig_receive_derivation_path(2),
            amount_sompi: 300_000_000,
            amount_kas: None,
            address: None,
        });

        let output = build_unsigned_exit(
            input,
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .expect("build unsigned exit");

        assert_eq!(output.manifest.wallet.outputs, 2);
        assert_eq!(output.manifest.total_output_sompi, 600_000_000);
        assert_eq!(output.manifest.change.as_ref().unwrap().amount_sompi, 300_000_000);
        assert_eq!(output.manifest.change.as_ref().unwrap().derivation_path, "m/0/0/2");
        assert_eq!(
            output.manifest.change.as_ref().unwrap().amount_kas.as_deref(),
            Some("3.00000000")
        );
        assert!(
            output
                .manifest
                .change
                .as_ref()
                .unwrap()
                .address
                .as_deref()
                .is_some_and(|addr| addr.starts_with("kaspa:p"))
        );

        let pst = PartiallySignedTransactionProto::decode(
            decode_fixed_hex(&output.wallet_hex, "wallet hex").unwrap().as_slice(),
        )
        .expect("decode pst");
        assert_eq!(pst.tx.as_ref().unwrap().outputs.len(), 2);
        assert_ne!(
            pst.tx.as_ref().unwrap().outputs[0].script_public_key,
            pst.tx.as_ref().unwrap().outputs[1].script_public_key
        );

        let report = verify_unsigned_exit(
            &output.manifest,
            &output.wallet_hex,
            VerifyExitOptions {
                allow_signatures: false,
                require_fully_signed: false,
                allow_non_igra_lock_script_for_testing: false,
            },
        )
        .expect("verify unsigned exit");
        assert_eq!(report.output_count, 2);
    }

    #[test]
    fn rejects_non_standard_kaspa_mass_without_testing_override() {
        let mut input = sample_input();
        input.locking_utxos = (0..3)
            .map(|index| {
                let mut utxo = sample_input().locking_utxos[0].clone();
                utxo.transaction_id = format!("{:064x}", index + 1);
                utxo.amount_sompi = 100_000_000;
                utxo
            })
            .collect();
        input.exits = (0..20)
            .map(|index| ExitRequest {
                message_id: format!("0x{:064x}", index + 1),
                recipient: input.exits[0].recipient.clone(),
                amount_sompi: 5_000_000,
                amount_kas: None,
            })
            .collect();
        input.change = Some(ChangeOutput {
            derivation_path: canonical_multisig_receive_derivation_path(1),
            amount_sompi: 199_000_000,
            amount_kas: None,
            address: None,
        });
        input.fee_sompi = 1_000_000;

        let err = build_unsigned_exit(
            input.clone(),
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(1),
                max_nonce: Some(1),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .unwrap_err();
        let err = err.to_string();
        assert!(err.contains("Kaspa mass preflight failed"));
        assert!(err.contains("storage mass 3975025 exceeds standard limit 100000"));
        assert!(err.contains("0 positive exits from this input set appear to fit"));

        let mut input_with_larger_exits = input.clone();
        for exit in &mut input_with_larger_exits.exits {
            exit.amount_sompi = 10_000_000;
            exit.amount_kas = None;
        }
        input_with_larger_exits.change.as_mut().unwrap().amount_sompi = 99_000_000;
        input_with_larger_exits.change.as_mut().unwrap().amount_kas = None;
        let err = build_unsigned_exit(
            input_with_larger_exits,
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(1),
                max_nonce: Some(1),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("at most 1 exit(s) from this input set should fit"));
        assert!(err.contains("change is recalculated to 289000000 sompi"));

        let output = build_unsigned_exit(
            input,
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: true,
            },
        )
        .expect("testing override should emit rehearsal artifact");
        let mass = output.manifest.mass.as_ref().expect("mass preflight manifest");
        assert_eq!(mass.storage_mass, 3_975_025);
        assert!(mass.standard_limit_exceeded);
        assert!(mass.block_limit_exceeded);
    }

    #[test]
    fn derives_official_kaspawallet_canonical_receive_path_public_keys() {
        let mut input = sample_input();
        input.multisig.extended_public_keys.reverse();
        input.locking_utxos[0].derivation_path = canonical_multisig_receive_derivation_path(1);

        let output = build_unsigned_exit(
            input,
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .expect("build unsigned exit");
        let pst = PartiallySignedTransactionProto::decode(
            decode_fixed_hex(&output.wallet_hex, "wallet hex").unwrap().as_slice(),
        )
        .expect("decode pst");
        let partial_input = &pst.partially_signed_inputs[0];

        assert_eq!(partial_input.derivation_path, "m/0/0/1");
        assert_eq!(
            partial_input
                .pub_key_signature_pairs
                .iter()
                .map(|pair| pair.extended_pub_key.as_str())
                .collect::<Vec<_>>(),
            vec![
                // These values were cross-checked against official kaspawallet's
                // libkaspawallet/bip32 DeriveFromPath("m/0/0/1").
                "kpub2HmC6PuGMkqBB1rjvaRTVrKDgBxBFCKXrzHq5GbfHi796kwBAnorViyPyeuqX7SqrRNzPQBteWKpMGi7hyyDSS24bsJtTQyb1YJbeRbnxGy",
                "kpub2LRq4jzk6NcvqgKzZkgFTsVLqzbFdRe6XtUSEGevhJQKBw9gD8Viq2mh84TqHQvec3N8ZavLrBRE6fGTR4bRPLJgyx9nivnLJSkKBoHhXNq",
                "kpub2NHxXk4U63VdJZtmiUHxowkG9m7EGA4ERTsHY4Pt51sERVHfVcRr8hWCh76kHnAkUUsNgFyqhbJuFKEsu5yQd9BJkMPzHTY2J1TmteQxWxu",
            ]
        );
    }

    #[test]
    fn decodes_kaspawallet_wallet_hex_into_expected_transaction() {
        let output = build_unsigned_exit(
            sample_input(),
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .expect("build unsigned exit");

        let tx = decode_wallet_transaction(&output.wallet_hex).expect("decode wallet tx");

        assert_eq!(tx.version, KASPA_TX_VERSION_TOCCATA);
        assert_eq!(tx.id().to_string(), output.manifest.protocol.kaspa_tx_id);
        assert_eq!(tx.inputs.len(), output.manifest.locking_utxos.len());
        assert!(tx.inputs.iter().all(|input| {
            input.compute_commit.compute_budget() == Some(KASPA_TOCCATA_COMPUTE_BUDGET_PER_INPUT)
        }));
        assert_eq!(
            tx.outputs.len(),
            output.manifest.exits.len() + usize::from(output.manifest.change.is_some())
        );
        assert_eq!(prefixed_hex(&tx.payload), output.manifest.protocol.payload_hex);
    }

    #[test]
    fn materializes_signature_scripts_from_partial_signatures() {
        let output = build_unsigned_exit(
            sample_input(),
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .expect("build unsigned exit");

        let mut pst = PartiallySignedTransactionProto::decode(
            decode_fixed_hex(&output.wallet_hex, "wallet hex").unwrap().as_slice(),
        )
        .expect("decode pst");
        for partial_input in &mut pst.partially_signed_inputs {
            partial_input.pub_key_signature_pairs[0].signature =
                vec![1_u8; KASPA_SIGNATURE_SIZE_WITH_HASH_TYPE];
            partial_input.pub_key_signature_pairs[1].signature =
                vec![2_u8; KASPA_SIGNATURE_SIZE_WITH_HASH_TYPE];
        }
        for input in &mut pst.tx.as_mut().expect("pst tx").inputs {
            input.sig_op_count = 3;
            input.compute_budget = 0;
        }
        let wallet_hex = hex::encode(pst.encode_to_vec());

        let tx = materialize_signed_wallet_transaction(&output.manifest, &wallet_hex)
            .expect("materialize signed tx");

        assert_eq!(tx.id().to_string(), output.manifest.protocol.kaspa_tx_id);
        assert!(tx.inputs.iter().all(|input| !input.signature_script.is_empty()));
        assert!(tx.inputs.iter().all(|input| {
            input.compute_commit.compute_budget() == Some(KASPA_TOCCATA_COMPUTE_BUDGET_PER_INPUT)
        }));
    }

    #[test]
    fn derives_and_verifies_canonical_multisig_address_helpers() {
        let input = sample_input();
        let report = derive_multisig_address(MultisigAddressInput {
            network: "mainnet".to_string(),
            derivation_path: canonical_multisig_receive_derivation_path(1),
            minimum_signatures: input.multisig.minimum_signatures,
            extended_public_keys: input.multisig.extended_public_keys.clone(),
            ecdsa: false,
        })
        .expect("derive multisig address");

        assert_eq!(report.derivation_path.path, "m/0/0/1");
        assert!(report.derivation_path.canonical);
        assert_eq!(report.derivation_path.cosigner_index, 0);
        assert_eq!(report.derivation_path.keychain, 0);
        assert_eq!(report.derivation_path.address_index, 1);
        assert_eq!(report.derived_extended_public_keys.len(), 3);
        assert_eq!(report.script_public_key.version, 0);
        assert!(report.script_public_key.script.starts_with("aa20"));
        assert!(report.address.starts_with("kaspa:p"));

        let verification = verify_multisig_address(
            MultisigAddressInput {
                network: "mainnet".to_string(),
                derivation_path: canonical_multisig_receive_derivation_path(1),
                minimum_signatures: input.multisig.minimum_signatures,
                extended_public_keys: input.multisig.extended_public_keys,
                ecdsa: false,
            },
            &report.address,
        )
        .expect("verify multisig address");
        assert!(verification.matches);

        let path_report =
            check_multisig_derivation_path(&canonical_multisig_receive_derivation_path(1))
                .expect("check canonical path");
        assert_eq!(path_report.address_index, 1);
        assert!(check_multisig_derivation_path("m/1/0/1").is_err());
    }

    #[test]
    fn rejects_non_canonical_multisig_receive_derivation_path() {
        let cases = [
            ("m/0/0", "canonical multisig receive path"),
            ("m/1/0/1", "cosigner index 0"),
            ("m/0/1/1", "external receive keychain 0"),
            ("m/0/0/1'", "non-hardened"),
        ];

        for (path, expected) in cases {
            let mut input = sample_input();
            input.locking_utxos[0].derivation_path = path.to_string();
            let err = build_unsigned_exit(
                input,
                BuildExitOptions {
                    network: "mainnet".to_string(),
                    tx_id_prefix: "00".to_string(),
                    lane_id: "97b10000".to_string(),
                    mining_timeout: Duration::from_secs(1),
                    max_nonce: Some(1),
                    allow_non_igra_lock_script_for_testing: false,
                    allow_mass_limit_override_for_testing: false,
                },
            )
            .unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "path {path} error did not contain {expected}: {err}"
            );
        }
    }

    #[test]
    fn rejects_non_locking_script() {
        let mut input = sample_input();
        input.locking_utxos[0].script_public_key.script = "00".to_string();
        let err = build_unsigned_exit(
            input,
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(1),
                max_nonce: Some(1),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("not the IGRA KAS locking script"));
    }

    #[test]
    fn testing_override_allows_non_igra_locking_script() {
        let mut input = sample_input();
        input.locking_utxos[0].script_public_key.script =
            "aa201f3dfb8e24afa4cee432d456b4b6dd8a16b9e3149aa8949cdd8bf8ba5edb736687".to_string();

        let output = build_unsigned_exit(
            input,
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(1),
                max_nonce: Some(1_000_000),
                allow_non_igra_lock_script_for_testing: true,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .expect("build test non-IGRA lock script");

        assert!(
            verify_unsigned_exit(
                &output.manifest,
                &output.wallet_hex,
                VerifyExitOptions {
                    allow_signatures: false,
                    require_fully_signed: false,
                    allow_non_igra_lock_script_for_testing: false,
                },
            )
            .is_err()
        );

        verify_unsigned_exit(
            &output.manifest,
            &output.wallet_hex,
            VerifyExitOptions {
                allow_signatures: false,
                require_fully_signed: false,
                allow_non_igra_lock_script_for_testing: true,
            },
        )
        .expect("verify test non-IGRA lock script");
    }

    #[test]
    fn rejects_mismatched_human_readable_amounts_and_addresses() {
        let mut input = sample_input();
        input.exits[0].amount_kas = Some("2.99999999".to_string());
        let err = build_unsigned_exit(
            input,
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(1),
                max_nonce: Some(1),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("exits[0].amount_kas does not match amount_sompi"));

        let mut input = sample_input();
        input.locking_utxos[0].address =
            Some("kaspa:pq0nm7uwyjh6fnhyxt29dd9kmk9pdw0rzjd239yumk9l3wj7mdekvwzglcu9r".to_string());
        let err = build_unsigned_exit(
            input,
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                lane_id: "97b10000".to_string(),
                mining_timeout: Duration::from_secs(1),
                max_nonce: Some(1),
                allow_non_igra_lock_script_for_testing: false,
                allow_mass_limit_override_for_testing: false,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("locking_utxos[0].address does not match script_public_key")
        );
    }
}
