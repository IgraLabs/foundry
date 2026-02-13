use clap::Parser;
use eyre::{Context, Result};
use kaspa_addresses::Address as KaspaAddress;
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::api::rpc::RpcApi;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "List UTXOs for a Kaspa address (gRPC).")]
struct Args {
    /// Kaspa gRPC URL (must start with grpc://)
    #[arg(long)]
    kaspa_rpc_url: String,

    /// Kaspa address (e.g. kaspatest:...)
    #[arg(long)]
    address: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UtxoOut {
    tx_id: String,
    index: u32,
    amount: u64,
    is_coinbase: bool,
    block_daa_score: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    address: String,
    utxos: Vec<UtxoOut>,
    error: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let address: KaspaAddress = KaspaAddress::try_from(args.address.as_str())
        .wrap_err("invalid --address (expected a kaspa address string)")?;

    let client = match GrpcClient::connect(args.kaspa_rpc_url.clone()).await {
        Ok(c) => c,
        Err(err) => {
            let out = Output { address: args.address, utxos: vec![], error: Some(err.to_string()) };
            println!("{}", serde_json::to_string(&out)?);
            return Ok(());
        }
    };

    let entries = match client.get_utxos_by_addresses(vec![address.clone()]).await {
        Ok(e) => e,
        Err(err) => {
            let out = Output { address: args.address, utxos: vec![], error: Some(err.to_string()) };
            println!("{}", serde_json::to_string(&out)?);
            return Ok(());
        }
    };

    let mut utxos = Vec::new();
    for entry in entries {
        utxos.push(UtxoOut {
            tx_id: entry.outpoint.transaction_id.to_string(),
            index: entry.outpoint.index,
            amount: entry.utxo_entry.amount,
            is_coinbase: entry.utxo_entry.is_coinbase,
            block_daa_score: entry.utxo_entry.block_daa_score,
        });
    }

    let out = Output { address: address.to_string(), utxos, error: None };
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
