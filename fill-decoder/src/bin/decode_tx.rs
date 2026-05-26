//! `decode-tx` CLI. Gated behind the `cli` Cargo feature.
//!
//! ```text
//! decode-tx --base64 <BASE64>                # decode a serialized tx
//! decode-tx --tx <SIGNATURE> --rpc-url <URL> # fetch from RPC, decode
//! decode-tx ... --check <PUBKEY>             # exclusivity check (repeatable)
//! decode-tx ... --allow-token-ledger         # opt in to token-ledger entrypoints
//! decode-tx ... --json                       # machine-readable output
//! ```
//!
//! Exit codes: 0 ok, 1 decode err, 2 cli/rpc err, 3 exclusivity fail,
//! 4 fill count wrong, 5 mint mismatch, 6 token-ledger entrypoint carries
//! a SolRfqV2 leg (override with `--allow-token-ledger`).

#![cfg(feature = "cli")]

use base64::Engine;
use clap::Parser;
use fill_decoder::{
    check_pubkey_exclusivity_base58, decode_transaction_base64, parse_pubkey_base58,
    AddressLookupTableEntry, DecodedFill, DecodedTransaction, ExclusivityReport, FillCountError,
    MintPairMismatch,
};

/// On-chain `AddressLookupTable` account layout: addresses begin at byte 56.
/// See `solana-address-lookup-table-program` -- `LOOKUP_TABLE_META_SIZE = 56`.
const LOOKUP_TABLE_META_SIZE: usize = 56;

#[derive(Parser, Debug)]
#[command(name = "decode-tx", version, about = "Decode OKX dex-solana-v3 transactions and extract SolRfqV2 legs")]
struct Args {
    /// Base64-encoded transaction.
    #[arg(long, conflicts_with = "tx")]
    base64: Option<String>,

    /// Transaction signature to fetch via RPC.
    #[arg(long, conflicts_with = "base64")]
    tx: Option<String>,

    /// Solana RPC endpoint (or set RPC_URL).
    #[arg(long, env = "RPC_URL")]
    rpc_url: Option<String>,

    /// Base58 pubkey to verify is referenced only by fill-bearing instructions.
    /// Repeat for each sensitive account (typically the maker's fill_authority,
    /// maker_base_token_account, and maker_quote_token_account).
    #[arg(long = "check")]
    check_pubkeys: Vec<String>,

    /// Base mint decimals. Required together with `--quote-decimals` to render
    /// levels as `(price, qty)` instead of raw atoms.
    #[arg(long, requires = "quote_decimals")]
    base_decimals: Option<u8>,

    /// Quote mint decimals. See `--base-decimals`.
    #[arg(long, requires = "base_decimals")]
    quote_decimals: Option<u8>,

    /// Base58 base mint. With `--quote-mint`, derives the taker's `Side`
    /// from the on-wire `source_mint` at swap account position 3.
    #[arg(long, requires = "quote_mint")]
    base_mint: Option<String>,

    /// Base58 quote mint. See `--base-mint`.
    #[arg(long, requires = "base_mint")]
    quote_mint: Option<String>,

    /// Opt in to accepting SolRfqV2 fills that ride inside a token-ledger
    /// aggregator entrypoint. Off by default: token-ledger entrypoints hide
    /// `amount_in` from the args and enable atomic arbitrage composition, so
    /// the conservative policy is to refuse-to-sign.
    #[arg(long)]
    allow_token_ledger: bool,

    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let b64 = match (args.base64.clone(), args.tx.clone()) {
        (Some(b), None) => b,
        (None, Some(sig)) => {
            let url = match args.rpc_url.as_deref() {
                Some(u) => u,
                None => {
                    eprintln!("error: --rpc-url or RPC_URL required when using --tx");
                    std::process::exit(2);
                }
            };
            match fetch_tx_base64(url, &sig).await {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("error: RPC fetch failed: {e}");
                    std::process::exit(2);
                }
            }
        }
        _ => {
            eprintln!("error: pass exactly one of --base64 or --tx");
            std::process::exit(2);
        }
    };

    // First pass: decode with no ALT state so we can read off the table pubkeys.
    let tx_unresolved = match decode_transaction_base64(&b64, &[] as &[AddressLookupTableEntry]) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: decode failed: {e}");
            std::process::exit(1);
        }
    };

    let alt_keys: Vec<[u8; 32]> = tx_unresolved
        .message
        .address_table_lookups
        .iter()
        .map(|l| l.table_key)
        .collect();

    // Fail-closed ALT resolution: if the tx references any ALTs we must resolve
    // them, otherwise downstream account-level checks (swap_leg_accounts,
    // verify_mint_pair, exclusivity) silently weaken into "true for the
    // resolved subset" without the caller realising.
    let tx = if alt_keys.is_empty() {
        tx_unresolved
    } else {
        let url = args.rpc_url.as_deref().unwrap_or_else(|| {
            eprintln!(
                "error: transaction references {} ALT(s); --rpc-url (or RPC_URL) is required \
                 to resolve them",
                alt_keys.len()
            );
            std::process::exit(2);
        });
        let alt_state = fetch_alt_entries(url, &alt_keys).await.unwrap_or_else(|e| {
            eprintln!("error: ALT fetch failed: {e}");
            std::process::exit(2);
        });
        let resolved = decode_transaction_base64(&b64, &alt_state).unwrap_or_else(|e| {
            eprintln!("error: re-decode with ALT state failed: {e}");
            std::process::exit(1);
        });
        if resolved.message.unresolved_count > 0 {
            eprintln!(
                "error: {} account(s) still unresolved after ALT fetch; \
                 the referenced ALT may be truncated or missing entries",
                resolved.message.unresolved_count
            );
            std::process::exit(2);
        }
        resolved
    };

    let reports: Vec<ExclusivityReport> = args
        .check_pubkeys
        .iter()
        .map(|pk| check_pubkey_exclusivity_base58(&tx.message, pk))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|e| {
            eprintln!("error: --check parse: {e}");
            std::process::exit(2);
        });

    let decimals = args.base_decimals.zip(args.quote_decimals);
    let fill_status = tx.single_fill().map(|_| ());

    let mint_pair = args
        .base_mint
        .as_deref()
        .zip(args.quote_mint.as_deref())
        .map(|(b, q)| {
            let bb = parse_pubkey_base58(b).unwrap_or_else(|e| {
                eprintln!("error: --base-mint: {e}");
                std::process::exit(2);
            });
            let qq = parse_pubkey_base58(q).unwrap_or_else(|e| {
                eprintln!("error: --quote-mint: {e}");
                std::process::exit(2);
            });
            (bb, qq)
        });
    let mint_check: Option<Result<(), MintPairMismatch>> = mint_pair
        .and_then(|(b, q)| tx.swap_leg_accounts().ok().map(|leg| (leg, b, q)))
        .map(|(leg, b, q)| leg.verify_mint_pair(&b, &q));

    if args.json {
        print_json(&tx, &reports, decimals, &fill_status, &mint_check);
    } else {
        print_human(&tx, &reports, decimals, &fill_status, &mint_check);
    }

    if fill_status.is_err() {
        std::process::exit(4);
    }
    if matches!(mint_check, Some(Err(_))) {
        std::process::exit(5);
    }
    if tx.has_token_ledger_fill() && !args.allow_token_ledger {
        std::process::exit(6);
    }
    if reports.iter().any(|r| !r.is_exclusive()) {
        std::process::exit(3);
    }
}


async fn fetch_alt_entries(
    url: &str,
    table_keys: &[[u8; 32]],
) -> Result<Vec<AddressLookupTableEntry>, String> {
    let client = reqwest::Client::new();
    let mut out = Vec::with_capacity(table_keys.len());
    for tk in table_keys {
        let tk_b58 = bs58::encode(tk).into_string();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getAccountInfo",
            "params": [tk_b58, {"encoding": "base64", "commitment": "confirmed"}]
        });
        let resp: serde_json::Value = client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let data_b64 = resp["result"]["value"]["data"][0]
            .as_str()
            .ok_or_else(|| format!("ALT {tk_b58}: unexpected RPC response shape: {resp}"))?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(data_b64.trim())
            .map_err(|e| format!("ALT {tk_b58}: base64 decode: {e}"))?;
        if raw.len() < LOOKUP_TABLE_META_SIZE {
            return Err(format!(
                "ALT {tk_b58}: account data {} bytes, expected at least {LOOKUP_TABLE_META_SIZE}",
                raw.len()
            ));
        }
        let body = &raw[LOOKUP_TABLE_META_SIZE..];
        if body.len() % 32 != 0 {
            return Err(format!(
                "ALT {tk_b58}: address payload {} bytes not a multiple of 32",
                body.len()
            ));
        }
        let addresses: Vec<[u8; 32]> = body
            .chunks_exact(32)
            .map(|c| {
                let mut a = [0u8; 32];
                a.copy_from_slice(c);
                a
            })
            .collect();
        out.push(AddressLookupTableEntry {
            table_key: *tk,
            addresses,
        });
    }
    Ok(out)
}

async fn fetch_tx_base64(url: &str, sig: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getTransaction",
        "params": [
            sig,
            {"encoding": "base64", "maxSupportedTransactionVersion": 0, "commitment": "confirmed"}
        ]
    });
    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let tx_b64 = resp["result"]["transaction"][0]
        .as_str()
        .ok_or_else(|| format!("unexpected RPC response shape: {resp}"))?
        .to_string();
    Ok(tx_b64)
}

fn print_human(
    tx: &DecodedTransaction,
    reports: &[ExclusivityReport],
    decimals: Option<(u8, u8)>,
    fill_status: &Result<(), FillCountError>,
    mint_check: &Option<Result<(), MintPairMismatch>>,
) {
    println!("Decoded transaction");
    println!("  signatures: {}", tx.signatures.len());
    println!("  version:    {:?}", tx.message.version);
    println!(
        "  accounts:   {} ({} unresolved)",
        tx.message.account_keys.len(),
        tx.message.unresolved_count
    );
    println!("  ixs:        {}", tx.message.instructions.len());

    if !tx.message.address_table_lookups.is_empty() {
        println!("  ALTs:");
        for l in &tx.message.address_table_lookups {
            println!(
                "    {} (w={}, r={})",
                bs58::encode(&l.table_key).into_string(),
                l.writable_indexes.len(),
                l.readonly_indexes.len(),
            );
        }
    }

    for ix in &tx.message.instructions {
        let pid = bs58::encode(&ix.program_id.pubkey).into_string();
        let ep = match ix.entrypoint {
            Some(k) => format!(", {k}"),
            None => String::new(),
        };
        println!(
            "  [{:>2}] {} ({} accs, {} bytes, {} rfq legs{ep})",
            ix.instruction_index,
            short(&pid),
            ix.accounts.len(),
            ix.data.len(),
            ix.fills.len(),
        );
        for fill in &ix.fills {
            print_fill(fill, decimals);
        }
    }

    println!();
    println!("Single-fill check");
    match fill_status {
        Ok(()) => println!("  OK exactly one SolRfqV2 leg ({} total)", tx.fill_count()),
        Err(e) => println!("  UNSAFE {e}"),
    }

    println!();
    println!("Entrypoint check");
    if tx.has_token_ledger_fill() {
        println!("  UNSAFE SolRfqV2 leg rides inside a token-ledger entrypoint");
    } else {
        println!("  OK no token-ledger entrypoint carries a SolRfqV2 leg");
    }

    if let Ok(leg) = tx.swap_leg_accounts() {
        println!();
        println!("SolRfqV2 leg accounts");
        let p = |label: &str, k: &[u8; 32]| {
            println!("  {label:<32} {}", bs58::encode(k).into_string());
        };
        p("swap_authority (taker)", &leg.swap_authority);
        p("swap_source_token_account", &leg.swap_source_token_account);
        p("swap_destination_token_account", &leg.swap_destination_token_account);
        p("fill_authority (maker)", &leg.fill_authority);
        p("maker_base_token_account", &leg.maker_base_token_account);
        p("maker_quote_token_account", &leg.maker_quote_token_account);
        p("base_mint", &leg.base_mint);
        p("quote_mint", &leg.quote_mint);
    }

    if let Some(r) = mint_check {
        println!();
        println!("Mint pair check");
        match r {
            Ok(()) => println!("  OK leg's (base, quote) matches expected"),
            Err(e) => println!("  UNSAFE {e}"),
        }
    }

    if !reports.is_empty() {
        println!();
        println!("Exclusivity checks");
        for r in reports {
            println!("  {r}");
        }
    }
}

fn print_fill(fill: &DecodedFill, decimals: Option<(u8, u8)>) {
    println!(
        "        rfq leg: side={} rfq_id={} expire_at={} levels={}",
        fill.taker_side,
        fill.rfq_id,
        fill.expire_at,
        fill.levels.len(),
    );
    for (i, l) in fill.levels.iter().enumerate() {
        match decimals {
            Some((bd, qd)) => {
                let pq = l.to_price_qty(bd, qd);
                println!(
                    "          L{i}: price={:.6} qty={:.6} (base_atoms={} quote_atoms={})",
                    pq.price, pq.qty, l.base_atoms, l.quote_atoms
                );
            }
            None => println!(
                "          L{i}: base_atoms={} quote_atoms={}",
                l.base_atoms, l.quote_atoms
            ),
        }
    }
}

fn short(pk: &str) -> String {
    if pk.len() <= 12 {
        pk.to_string()
    } else {
        format!("{}…{}", &pk[..4], &pk[pk.len() - 4..])
    }
}

fn print_json(
    tx: &DecodedTransaction,
    reports: &[ExclusivityReport],
    decimals: Option<(u8, u8)>,
    fill_status: &Result<(), FillCountError>,
    mint_check: &Option<Result<(), MintPairMismatch>>,
) {
    let ixs: Vec<serde_json::Value> = tx
        .message
        .instructions
        .iter()
        .map(|ix| {
            serde_json::json!({
                "index": ix.instruction_index,
                "program_id": bs58::encode(&ix.program_id.pubkey).into_string(),
                "entrypoint": ix.entrypoint.map(|k| k.to_string()),
                "account_count": ix.accounts.len(),
                "data_bytes": ix.data.len(),
                "rfq_legs": ix.fills.iter().map(|f| fill_json(f, decimals)).collect::<Vec<_>>(),
            })
        })
        .collect();

    let exclusivity: Vec<serde_json::Value> = reports
        .iter()
        .map(|r| {
            serde_json::json!({
                "pubkey": r.pubkey_base58(),
                "safe": r.is_exclusive(),
                "confirmed_count": r.confirmed_count,
                "fill_ix_indices": r.fill_ix_indices,
                "non_fill_ix_indices": r.non_fill_ix_indices,
                "ix_with_unresolved": r.ix_with_unresolved,
            })
        })
        .collect();

    let single_fill = serde_json::json!({
        "ok": fill_status.is_ok(),
        "fill_count": tx.fill_count(),
        "error": fill_status.as_ref().err().map(|e| e.to_string()),
    });

    let entrypoint_check = serde_json::json!({
        "ok": !tx.has_token_ledger_fill(),
        "has_token_ledger_fill": tx.has_token_ledger_fill(),
    });

    let mint_check_json = mint_check.as_ref().map(|r| match r {
        Ok(()) => serde_json::json!({ "ok": true }),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    });

    let out = serde_json::json!({
        "version": format!("{:?}", tx.message.version),
        "signature_count": tx.signatures.len(),
        "unresolved_count": tx.message.unresolved_count,
        "instructions": ixs,
        "single_fill": single_fill,
        "entrypoint_check": entrypoint_check,
        "mint_check": mint_check_json,
        "exclusivity_checks": exclusivity,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}

fn fill_json(fill: &DecodedFill, decimals: Option<(u8, u8)>) -> serde_json::Value {
    let levels: Vec<serde_json::Value> = fill
        .levels
        .iter()
        .map(|l| {
            let mut obj = serde_json::json!({
                "base_atoms": l.base_atoms,
                "quote_atoms": l.quote_atoms,
            });
            if let Some((bd, qd)) = decimals {
                let pq = l.to_price_qty(bd, qd);
                obj["price"] = serde_json::json!(pq.price);
                obj["qty"] = serde_json::json!(pq.qty);
            }
            obj
        })
        .collect();
    serde_json::json!({
        "taker_side": fill.taker_side.to_string(),
        "rfq_id": fill.rfq_id,
        "expire_at": fill.expire_at,
        "levels": levels,
    })
}
