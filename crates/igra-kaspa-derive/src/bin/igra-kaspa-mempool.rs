use std::str::FromStr;

use clap::Parser;
use eyre::{Context, Result};
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::{RpcTransactionId, api::rpc::RpcApi};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Query Kaspa mempool for a transaction id (gRPC).")]
struct Args {
    /// Kaspa gRPC URL (must start with grpc://)
    #[arg(long)]
    kaspa_rpc_url: String,

    /// Transaction id (hex string)
    #[arg(long)]
    tx_id: String,

    /// Include orphan pool
    #[arg(long, default_value_t = false)]
    include_orphan_pool: bool,

    /// Filter out transactions not in the transaction pool (node-specific)
    #[arg(long, default_value_t = false)]
    filter_transaction_pool: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    tx_id: String,
    found: bool,
    mempool_entry: Option<serde_json::Value>,
    error: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let tx_id = RpcTransactionId::from_str(args.tx_id.trim())
        .wrap_err("invalid --tx-id (expected a Kaspa transaction id hex string)")?;

    let client = match GrpcClient::connect(args.kaspa_rpc_url.clone()).await {
        Ok(c) => c,
        Err(err) => {
            let out = Output {
                tx_id: args.tx_id,
                found: false,
                mempool_entry: None,
                error: Some(format!("failed to connect to kaspa rpc: {err}")),
            };
            println!("{}", serde_json::to_string(&out)?);
            return Ok(());
        }
    };

    match client
        .get_mempool_entry(tx_id, args.include_orphan_pool, args.filter_transaction_pool)
        .await
    {
        Ok(entry) => {
            let out = Output {
                tx_id: args.tx_id,
                found: true,
                mempool_entry: Some(serde_json::to_value(entry)?),
                error: None,
            };
            println!("{}", serde_json::to_string(&out)?);
        }
        Err(err) => {
            // Most commonly: not found (or RPC error). We intentionally treat it as "not found"
            // and surface the RPC error string for debugging.
            let out = Output {
                tx_id: args.tx_id,
                found: false,
                mempool_entry: None,
                error: Some(err.to_string()),
            };
            println!("{}", serde_json::to_string(&out)?);
        }
    }

    Ok(())
}
