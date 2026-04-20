use alloy_primitives::hex;
use eyre::{Context, Result, bail, eyre};
use kaspa_addresses::{Address as KaspaAddress, Prefix as KaspaAddressPrefix};
use kaspa_bip32::{
    ChildNumber as KaspaChildNumber, DerivationPath as KaspaDerivationPath,
    ExtendedPublicKey as KaspaExtendedPublicKey, Prefix as KaspaBip32Prefix,
    PublicKey as KaspaBip32PublicKey, secp256k1::PublicKey as KaspaSecpPublicKey,
};
use kaspa_consensus_core::{
    subnets::SubnetworkId,
    tx::{
        ScriptPublicKey, Transaction as KaspaTransaction, TransactionId,
        TransactionInput as KaspaTransactionInput, TransactionOutpoint,
        TransactionOutput as KaspaTransactionOutput,
    },
};
use kaspa_txscript::{
    multisig_redeem_script, multisig_redeem_script_ecdsa, pay_to_address_script,
    pay_to_script_hash_script,
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

#[derive(Clone, Debug)]
pub struct BuildExitOptions {
    pub network: String,
    pub tx_id_prefix: String,
    pub mining_timeout: Duration,
    pub max_nonce: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct VerifyExitOptions {
    pub allow_signatures: bool,
    pub require_fully_signed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BuildExitInput {
    pub locking_utxos: Vec<LockingUtxo>,
    pub exits: Vec<ExitRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeOutput>,
    pub fee_sompi: u64,
    pub multisig: MultisigSpec,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LockingUtxo {
    pub transaction_id: String,
    pub index: u32,
    pub amount_sompi: u64,
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChangeOutput {
    pub derivation_path: String,
    pub amount_sompi: u64,
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
    pub total_input_sompi: u64,
    pub total_output_sompi: u64,
    pub multisig: MultisigSpec,
    pub wallet: WalletArtifactManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExitProtocolManifest {
    pub version: u8,
    pub tx_type_id: u8,
    pub payload_header: String,
    pub tx_id_prefix: String,
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
    validate_build_input(&input)?;
    input.multisig.extended_public_keys.sort();

    let network_prefix = parse_network_prefix(&options.network)?;
    let tx_id_prefix = decode_fixed_hex(&options.tx_id_prefix, "tx-id prefix")?;
    if tx_id_prefix.is_empty() {
        bail!("tx-id prefix cannot be empty");
    }
    if tx_id_prefix.len() > 4 {
        bail!("tx-id prefix cannot exceed 4 bytes; the IGRA payload nonce is only 4 bytes");
    }

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
            Ok(KaspaTransactionInput::new(
                TransactionOutpoint::new(txid, utxo.index),
                Vec::new(),
                0,
                0,
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    let payload = build_payload_with_nonce(IGRA_EXIT_PAYLOAD_HEADER, &payload_l2data, 0);
    let mut tx = KaspaTransaction::new(0, inputs, outputs, 0, SubnetworkId::default(), 0, payload);
    let nonce =
        mine_payload_nonce(&mut tx, &tx_id_prefix, options.mining_timeout, options.max_nonce)?;

    let pst = partially_signed_transaction_proto(&tx, &input, network_prefix)?;
    let wallet_bytes = pst.encode_to_vec();
    let wallet_hex = hex::encode(&wallet_bytes);
    let payload_hex = prefixed_hex(&tx.payload);
    let kaspa_tx_id = tx.id().to_string();

    let manifest = UnsignedExitManifest {
        schema: "igra.exit.unsigned.v1".to_string(),
        network: options.network,
        protocol: ExitProtocolManifest {
            version: IGRA_PROTOCOL_VERSION,
            tx_type_id: IGRA_EXIT_TX_TYPE_ID,
            payload_header: prefixed_hex(&[IGRA_EXIT_PAYLOAD_HEADER]),
            tx_id_prefix: prefixed_hex(&tx_id_prefix),
            nonce,
            kaspa_tx_id,
            payload_hex,
        },
        locking_utxos: input.locking_utxos,
        exits: input.exits,
        change: input.change,
        fee_sompi: input.fee_sompi,
        total_input_sompi,
        total_output_sompi,
        multisig: input.multisig,
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
    if manifest.schema != "igra.exit.unsigned.v1" {
        bail!("unsupported manifest schema: {}", manifest.schema);
    }
    validate_build_input(&BuildExitInput {
        locking_utxos: manifest.locking_utxos.clone(),
        exits: manifest.exits.clone(),
        change: manifest.change.clone(),
        fee_sompi: manifest.fee_sompi,
        multisig: manifest.multisig.clone(),
    })?;

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

    verify_inputs(&tx, &pst, manifest, options.allow_signatures)?;
    verify_outputs(&tx, manifest)?;

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

fn validate_build_input(input: &BuildExitInput) -> Result<()> {
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
        if utxo.script_public_key.version != 0 || script != locking_script {
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
    }

    if let Some(change) = input.change.as_ref() {
        if change.amount_sompi == 0 {
            bail!("change.amount_sompi must be greater than zero");
        }
        validate_canonical_multisig_receive_derivation_path(
            &change.derivation_path,
            "change.derivation_path",
        )?;
    }

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

fn verify_inputs(
    tx: &KaspaTransaction,
    pst: &PartiallySignedTransactionProto,
    manifest: &UnsignedExitManifest,
    allow_signatures: bool,
) -> Result<()> {
    let locking_script = kas_locking_script()?;

    for (index, ((tx_input, partial_input), manifest_utxo)) in
        tx.inputs.iter().zip(&pst.partially_signed_inputs).zip(&manifest.locking_utxos).enumerate()
    {
        if !allow_signatures
            && (!tx_input.signature_script.is_empty() || tx_input.sig_op_count != 0)
        {
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
        if prev_spk.version != manifest_utxo.script_public_key.version as u32
            || prev_spk.script != locking_script
        {
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
        tx: Some(transaction_to_proto(tx)),
        partially_signed_inputs: partial_inputs,
    })
}

fn transaction_to_proto(tx: &KaspaTransaction) -> TransactionMessageProto {
    TransactionMessageProto {
        version: tx.version as u32,
        inputs: tx
            .inputs
            .iter()
            .map(|input| TransactionInputProto {
                previous_outpoint: Some(OutpointProto {
                    transaction_id: Some(TransactionIdProto {
                        bytes: input.previous_outpoint.transaction_id.as_bytes().to_vec(),
                    }),
                    index: input.previous_outpoint.index,
                }),
                signature_script: input.signature_script.clone(),
                sequence: input.sequence,
                sig_op_count: input.sig_op_count as u32,
            })
            .collect(),
        outputs: tx.outputs.iter().map(transaction_output_to_proto).collect(),
        lock_time: tx.lock_time,
        subnetwork_id: Some(SubnetworkIdProto {
            bytes: <SubnetworkId as AsRef<[u8]>>::as_ref(&tx.subnetwork_id).to_vec(),
        }),
        gas: tx.gas,
        payload: tx.payload.clone(),
    }
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
            if input.sig_op_count > u8::MAX as u32 {
                bail!("input {index} sigOpCount is too large");
            }
            Ok(KaspaTransactionInput::new(
                TransactionOutpoint::new(TransactionId::from_slice(&txid.bytes), outpoint.index),
                input.signature_script.clone(),
                input.sequence,
                input.sig_op_count as u8,
            ))
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

    Ok(KaspaTransactionOutput::new(change.amount_sompi, pay_to_script_hash_script(&redeem_script)))
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

fn build_payload_with_nonce(header: u8, l2data: &[u8], nonce: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + l2data.len() + 4);
    payload.push(header);
    payload.extend_from_slice(l2data);
    payload.extend_from_slice(&nonce.to_be_bytes());
    payload
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
            }],
            change: None,
            fee_sompi: 10_000,
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
    fn build_and_verify_unsigned_exit_roundtrip() {
        let output = build_unsigned_exit(
            sample_input(),
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
            },
        )
        .expect("build unsigned exit");

        assert_eq!(output.manifest.protocol.payload_header, "0x93");
        assert!(output.manifest.protocol.kaspa_tx_id.starts_with("00"));

        let report = verify_unsigned_exit(
            &output.manifest,
            &output.wallet_hex,
            VerifyExitOptions { allow_signatures: false, require_fully_signed: false },
        )
        .expect("verify unsigned exit");
        assert_eq!(report.input_count, 1);
        assert_eq!(report.output_count, 1);
        assert!(!report.fully_signed);
    }

    #[test]
    fn verify_allows_signed_wallet_hex_without_requiring_original_hex_hash() {
        let output = build_unsigned_exit(
            sample_input(),
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
            },
        )
        .expect("build unsigned exit");

        let mut pst = PartiallySignedTransactionProto::decode(
            decode_fixed_hex(&output.wallet_hex, "wallet hex").unwrap().as_slice(),
        )
        .expect("decode pst");
        pst.tx.as_mut().unwrap().inputs[0].sig_op_count = 3;
        pst.partially_signed_inputs[0].pub_key_signature_pairs[0].signature = vec![1; 64];
        let signed_hex = hex::encode(pst.encode_to_vec());

        assert!(
            verify_unsigned_exit(
                &output.manifest,
                &signed_hex,
                VerifyExitOptions { allow_signatures: false, require_fully_signed: false },
            )
            .is_err()
        );

        let report = verify_unsigned_exit(
            &output.manifest,
            &signed_hex,
            VerifyExitOptions { allow_signatures: true, require_fully_signed: false },
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
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
            },
        )
        .expect("build unsigned exit");

        let mut expected = output.manifest.multisig.extended_public_keys.clone();
        expected.sort();
        assert_eq!(output.manifest.multisig.extended_public_keys, expected);

        verify_unsigned_exit(
            &output.manifest,
            &output.wallet_hex,
            VerifyExitOptions { allow_signatures: false, require_fully_signed: false },
        )
        .expect("verify unsigned exit");
    }

    #[test]
    fn build_and_verify_exit_with_change_back_to_canonical_multisig() {
        let mut input = sample_input();
        input.locking_utxos[0].amount_sompi = 301_010_000;
        input.change = Some(ChangeOutput {
            derivation_path: canonical_multisig_receive_derivation_path(2),
            amount_sompi: 1_000_000,
        });

        let output = build_unsigned_exit(
            input,
            BuildExitOptions {
                network: "mainnet".to_string(),
                tx_id_prefix: "00".to_string(),
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
            },
        )
        .expect("build unsigned exit");

        assert_eq!(output.manifest.wallet.outputs, 2);
        assert_eq!(output.manifest.total_output_sompi, 301_000_000);
        assert_eq!(output.manifest.change.as_ref().unwrap().amount_sompi, 1_000_000);
        assert_eq!(output.manifest.change.as_ref().unwrap().derivation_path, "m/0/0/2");

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
            VerifyExitOptions { allow_signatures: false, require_fully_signed: false },
        )
        .expect("verify unsigned exit");
        assert_eq!(report.output_count, 2);
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
                mining_timeout: Duration::from_secs(30),
                max_nonce: Some(1_000_000),
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
                    mining_timeout: Duration::from_secs(1),
                    max_nonce: Some(1),
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
                mining_timeout: Duration::from_secs(1),
                max_nonce: Some(1),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("not the IGRA KAS locking script"));
    }
}
