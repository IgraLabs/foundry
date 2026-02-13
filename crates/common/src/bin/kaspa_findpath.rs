use kaspa_addresses::{Address, Prefix, Version};
use kaspa_bip32::{ChildNumber, ExtendedPrivateKey, Language, Mnemonic, secp256k1::SecretKey};

fn addr_from_secret(secret: &SecretKey, prefix: Prefix) -> Address {
    let public = kaspa_bip32::secp256k1::PublicKey::from_secret_key_global(secret);
    let payload = public.x_only_public_key().0.serialize();
    Address::new(prefix, Version::PubKey, &payload)
}

fn seg(n: u32, hardened: bool) -> String {
    if hardened {
        format!("{n}'")
    } else {
        n.to_string()
    }
}

fn main() {
    // Debug helper to identify the derivation path producing a specific address for a mnemonic.
    // This is used to align Foundry IGRA's Kaspa mnemonic derivation behavior to the user's wallet.
    let mnemonic = "test test test test test test test test test test test junk";
    let target = "kaspatest:qzf364tlnl7ja0w65ydu0m5l70pur2hcm3l3ahkmhs660zcyf7cvuf6uznufr";
    let target_addr = Address::try_from(target).expect("target address parse");

    let m = Mnemonic::new(mnemonic, Language::English).expect("mnemonic");
    let seed = m.to_seed("");
    let master = ExtendedPrivateKey::<SecretKey>::new(seed).expect("master xprv");

    // Common BIP44-like shape:
    // m/<purpose>/<coin>/<account>/<change>/<index>
    //
    // We brute-force coin type in a range for a handful of likely hardening patterns.
    #[derive(Clone, Copy)]
    struct Pattern {
        purpose: u32,
        purpose_h: bool,
        coin_h: bool,
        acct_h: bool,
        change_h: bool,
        idx_h: bool,
    }

    let patterns = [
        // Canonical BIP44: purpose'/coin_type'/account'/change/index
        Pattern { purpose: 44, purpose_h: true, coin_h: true, acct_h: true, change_h: false, idx_h: false },
        // Coin not hardened.
        Pattern { purpose: 44, purpose_h: true, coin_h: false, acct_h: true, change_h: false, idx_h: false },
        // Account not hardened.
        Pattern { purpose: 44, purpose_h: true, coin_h: true, acct_h: false, change_h: false, idx_h: false },
        // Change hardened.
        Pattern { purpose: 44, purpose_h: true, coin_h: true, acct_h: true, change_h: true, idx_h: false },
        // Index hardened.
        Pattern { purpose: 44, purpose_h: true, coin_h: true, acct_h: true, change_h: false, idx_h: true },
        // Purpose not hardened (rare).
        Pattern { purpose: 44, purpose_h: false, coin_h: true, acct_h: true, change_h: false, idx_h: false },
        // Multisig-like purpose.
        Pattern { purpose: 45, purpose_h: true, coin_h: true, acct_h: true, change_h: false, idx_h: false },
    ];

    let account = 0u32;
    let change = 0u32;
    let index = 0u32;

    // Search coin types up to this bound for each pattern. Increase if needed.
    let coin_max = 300_000u32;

    for pat in patterns {
        let purpose_key = match master.clone().derive_child(ChildNumber::new(pat.purpose, pat.purpose_h).unwrap()) {
            Ok(k) => k,
            Err(_) => continue,
        };

        for coin in 0..=coin_max {
            let coin_key = match purpose_key.clone().derive_child(ChildNumber::new(coin, pat.coin_h).unwrap()) {
                Ok(k) => k,
                Err(_) => continue,
            };
            let acct_key = match coin_key.clone().derive_child(ChildNumber::new(account, pat.acct_h).unwrap()) {
                Ok(k) => k,
                Err(_) => continue,
            };
            let change_key = match acct_key.clone().derive_child(ChildNumber::new(change, pat.change_h).unwrap()) {
                Ok(k) => k,
                Err(_) => continue,
            };
            let final_key = match change_key.clone().derive_child(ChildNumber::new(index, pat.idx_h).unwrap()) {
                Ok(k) => k,
                Err(_) => continue,
            };

            let addr = addr_from_secret(final_key.private_key(), Prefix::Testnet);
            if addr == target_addr {
                let path = format!(
                    "m/{}/{}/{}/{}/{}",
                    seg(pat.purpose, pat.purpose_h),
                    seg(coin, pat.coin_h),
                    seg(account, pat.acct_h),
                    seg(change, pat.change_h),
                    seg(index, pat.idx_h),
                );
                eprintln!("FOUND path: {path}");
                return;
            }
        }
    }

    eprintln!("NO MATCH found up to coin_max={coin_max} for BIP44-like shape with account=0 change=0 index=0.");
}
