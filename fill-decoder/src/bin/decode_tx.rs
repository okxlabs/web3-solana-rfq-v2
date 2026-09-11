//! `decode-tx` CLI. Gated behind the `cli` Cargo feature.
//!
//! ```text
//! decode-tx --base64 <BASE64>                # decode a serialized tx
//! decode-tx --tx <SIGNATURE> --rpc-url <URL> # fetch from RPC, decode
//! decode-tx ... --fill-authority <PUBKEY>     # static maker-account validation
//! decode-tx ... --allow-token-ledger         # opt in to token-ledger entrypoints
//! decode-tx ... --json                       # machine-readable output
//! ```
//!
//! Exit codes: 0 ok, 1 decode err, 2 cli/rpc err, 3 maker validation fail,
//! 4 fill count wrong, 6 unsupported/token-ledger entrypoint policy failure.

#![cfg(feature = "cli")]

use clap::Parser;
use fill_decoder::{
    decode_transaction_base64, parse_pubkey_base58, validate_maker_accounts, DecodedFill,
    DecodedTransaction, FillCountError, MakerAccounts, MakerValidationError, MakerValidationReport,
};

#[derive(Parser, Debug)]
#[command(
    name = "decode-tx",
    version,
    about = "Decode OKX dex-solana-v3 transactions and extract SolRfqV2 legs"
)]
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

    /// Expected static fill authority. Must be supplied with both maker token accounts.
    #[arg(long, requires_all = ["maker_base_token_account", "maker_quote_token_account"])]
    fill_authority: Option<String>,

    /// Expected static maker base token account.
    #[arg(long, requires_all = ["fill_authority", "maker_quote_token_account"])]
    maker_base_token_account: Option<String>,

    /// Expected static maker quote token account.
    #[arg(long, requires_all = ["fill_authority", "maker_base_token_account"])]
    maker_quote_token_account: Option<String>,

    /// Base mint decimals. Required together with `--quote-decimals` to render
    /// levels as `(price, qty)` instead of raw atoms.
    #[arg(long, requires = "quote_decimals")]
    base_decimals: Option<u8>,

    /// Quote mint decimals. See `--base-decimals`.
    #[arg(long, requires = "base_decimals")]
    quote_decimals: Option<u8>,

    /// Opt in to accepting SolRfqV2 fills that ride inside a token-ledger
    /// aggregator entrypoint. Off by default: token-ledger entrypoints hide
    /// `amount_in` from the args and enable atomic arbitrage composition, so
    /// the conservative policy is to refuse-to-sign. This does not override
    /// the unconditional rejection of `swap_tob*` entrypoints.
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

    let tx = match decode_transaction_base64(&b64) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: decode failed: {e}");
            std::process::exit(1);
        }
    };

    let decimals = args.base_decimals.zip(args.quote_decimals);
    let fill_status = tx.single_fill().map(|_| ());
    let maker_check = args.fill_authority.as_deref().map(|fill_authority| {
        let parse = |label: &str, value: &str| {
            parse_pubkey_base58(value).unwrap_or_else(|e| {
                eprintln!("error: --{label}: {e}");
                std::process::exit(2);
            })
        };
        let maker = MakerAccounts {
            fill_authority: parse("fill-authority", fill_authority),
            maker_base_token_account: parse(
                "maker-base-token-account",
                args.maker_base_token_account.as_deref().unwrap(),
            ),
            maker_quote_token_account: parse(
                "maker-quote-token-account",
                args.maker_quote_token_account.as_deref().unwrap(),
            ),
        };
        validate_maker_accounts(&tx.message, &maker)
    });

    if args.json {
        print_json(&tx, decimals, &fill_status, &maker_check);
    } else {
        print_human(&tx, decimals, &fill_status, &maker_check);
    }

    if tx.has_unsupported_entrypoint() {
        std::process::exit(6);
    }
    if fill_status.is_err() {
        std::process::exit(4);
    }
    if tx.has_token_ledger_fill() && !args.allow_token_ledger {
        std::process::exit(6);
    }
    if matches!(maker_check, Some(Err(_))) {
        std::process::exit(3);
    }
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
    decimals: Option<(u8, u8)>,
    fill_status: &Result<(), FillCountError>,
    maker_check: &Option<Result<MakerValidationReport, MakerValidationError>>,
) {
    println!("Decoded transaction");
    println!("  signatures: {}", tx.signatures.len());
    println!("  version:    {:?}", tx.message.version);
    println!("  static:     {}", tx.message.static_account_keys.len());
    println!(
        "  dynamic:    {} writable + {} readonly (not resolved)",
        tx.message.loaded_writable_count, tx.message.loaded_readonly_count
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
        let pid = tx
            .message
            .static_account_key(ix.program_id_index)
            .map(|key| short(&bs58::encode(key).into_string()))
            .unwrap_or_else(|| format!("dynamic[{}]", ix.program_id_index));
        let ep = match ix.entrypoint {
            Some(k) => format!(", {k}"),
            None => String::new(),
        };
        println!(
            "  [{:>2}] {} ({} accs, {} bytes, {} rfq legs{ep})",
            ix.instruction_index,
            pid,
            ix.account_indices.len(),
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
    if tx.has_unsupported_entrypoint() {
        println!("  UNSAFE swap_tob* entrypoint is not supported");
    } else if tx.has_token_ledger_fill() {
        println!("  UNSAFE SolRfqV2 leg rides inside a token-ledger entrypoint");
    } else {
        println!("  OK no token-ledger entrypoint carries a SolRfqV2 leg");
    }

    if let Some(result) = maker_check {
        println!();
        println!("Maker account check");
        match result {
            Ok(report) => println!("  {report}"),
            Err(e) => println!("  UNSAFE {e}"),
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
    decimals: Option<(u8, u8)>,
    fill_status: &Result<(), FillCountError>,
    maker_check: &Option<Result<MakerValidationReport, MakerValidationError>>,
) {
    let ixs: Vec<serde_json::Value> = tx
        .message
        .instructions
        .iter()
        .map(|ix| {
            let program_id = tx
                .message
                .static_account_key(ix.program_id_index)
                .map(|key| bs58::encode(key).into_string());
            serde_json::json!({
                "index": ix.instruction_index,
                "program_id_index": ix.program_id_index,
                "static_program_id": program_id,
                "entrypoint": ix.entrypoint.map(|k| k.to_string()),
                "account_count": ix.account_indices.len(),
                "data_bytes": ix.data.len(),
                "rfq_legs": ix.fills.iter().map(|f| fill_json(f, decimals)).collect::<Vec<_>>(),
            })
        })
        .collect();

    let single_fill = serde_json::json!({
        "ok": fill_status.is_ok(),
        "fill_count": tx.fill_count(),
        "error": fill_status.as_ref().err().map(|e| e.to_string()),
    });

    let entrypoint_check = serde_json::json!({
        "ok": !tx.has_unsupported_entrypoint() && !tx.has_token_ledger_fill(),
        "has_unsupported_entrypoint": tx.has_unsupported_entrypoint(),
        "has_token_ledger_fill": tx.has_token_ledger_fill(),
    });

    let maker_check_json = maker_check.as_ref().map(|r| match r {
        Ok(report) => serde_json::json!({
            "ok": true,
            "instruction_index": report.instruction_index,
            "leg_offset": report.leg_offset,
            "fill_authority_index": report.fill_authority_index,
            "maker_base_token_account_index": report.maker_base_token_account_index,
            "maker_quote_token_account_index": report.maker_quote_token_account_index,
            "rfq_program_index": report.rfq_program_index,
        }),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    });

    let out = serde_json::json!({
        "version": format!("{:?}", tx.message.version),
        "signature_count": tx.signatures.len(),
        "static_account_count": tx.message.static_account_keys.len(),
        "loaded_writable_count": tx.message.loaded_writable_count,
        "loaded_readonly_count": tx.message.loaded_readonly_count,
        "instructions": ixs,
        "single_fill": single_fill,
        "entrypoint_check": entrypoint_check,
        "maker_account_check": maker_check_json,
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
