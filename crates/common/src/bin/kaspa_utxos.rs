use kaspa_addresses::Address;
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::api::rpc::RpcApi;
use std::collections::HashMap;

fn main() {
    // Minimal debug helper.
    // Usage:
    //   cargo run -p foundry-common --bin kaspa_utxos -- <grpc_url> <addr1> [addr2...]
    let mut args = std::env::args().skip(1);
    let rpc_url =
        args.next().unwrap_or_else(|| "grpc://stage-roman.igralabs.com:16210".to_string());
    let addrs: Vec<String> = args.collect();
    if addrs.is_empty() {
        eprintln!("usage: kaspa_utxos <grpc_url> <addr1> [addr2...]");
        std::process::exit(2);
    }

    let rt =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");

    rt.block_on(async move {
        let client = GrpcClient::connect(rpc_url.clone())
            .await
            .unwrap_or_else(|err| panic!("failed to connect to {rpc_url}: {err}"));

        let parsed: Vec<Address> = addrs
            .iter()
            .map(|a| {
                Address::try_from(a.as_str()).unwrap_or_else(|err| panic!("bad address {a}: {err}"))
            })
            .collect();

        let utxos = client
            .get_utxos_by_addresses(parsed.clone())
            .await
            .unwrap_or_else(|err| panic!("get_utxos_by_addresses failed: {err}"));

        let mut by_addr: HashMap<String, (u64, u64)> = HashMap::new(); // addr -> (count, total)
        for entry in utxos {
            let Some(address) = entry.address else {
                continue;
            };
            let key = address.to_string();
            let v = by_addr.entry(key).or_insert((0, 0));
            v.0 += 1;
            v.1 += entry.utxo_entry.amount;
        }

        for addr in parsed {
            let key = addr.to_string();
            let (count, total) = by_addr.get(&key).copied().unwrap_or((0, 0));
            println!("{key}: utxos={count} total_sompi={total}");
        }
    });
}
