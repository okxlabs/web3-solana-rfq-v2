# fill-decoder

Off-chain decoder for OKX `dex-solana-v3` aggregator transactions that surfaces embedded `solana-rfq-v2` RFQ legs to a maker.

**Use case:** a maker receives a transaction they're asked to co-sign, runs it through this decoder, and gets enough structured data to decide whether the trade matches a quote they pre-committed to — without RPC, without `solana-sdk`, without curve crypto.

## What it does

Given a base64- or bytes-encoded Solana transaction, the decoder:

1. Parses the transaction (legacy + v0 messages, ALT-resolved when state is supplied).
2. Walks top-level instructions; for each `dex-solana-v3` swap entrypoint, decodes its `SwapArgs` via the embedded IDL.
3. Extracts every `Dex::SolRfqV2` leg as a [`DecodedFill`] carrying `taker_side`, `rfq_id`, `expire_at`, and `levels`.
4. Exposes helpers for the safety checks a maker should perform before signing.

## What it does NOT do

- Read on-chain state (token account mints, ALT contents — ALT state must be supplied by the caller).
- Validate the maker's signing policy (mints, fee payer, signer set, etc.) — only surfaces enough data for the caller to do so.
- Track `rfq_id` replay. The on-chain handler does not gate on `rfq_id` either — replay protection lives in the maker's quote registry.
- Detect top-level `fill_exact_in` invocations bypassing the aggregator — the caller is assumed to have already confirmed the tx is an OKX aggregator tx.

## Quickstart (library)

```rust
use fill_decoder::{
    all_pubkeys_exclusive, decode_transaction_base64, parse_pubkey_base58, FillCountError,
};

let tx = decode_transaction_base64(b64, &alt_state)?;

// 1. Exactly one SolRfqV2 leg.
let fill = tx.single_fill()?;                            // -> FillCountError on 0 or >1

// 2. Look up the pre-signed quote and match every dimension.
let quote = registry.lookup(fill.rfq_id)?;
if fill.taker_side != quote.intended_side  { reject; }   // signed-in side
if fill.levels     != quote.expected_levels { reject; }
if fill.expire_at  <  now_wall_clock        { reject; }

// 3. Confirm the leg's on-wire mint pair matches.
let leg = tx.swap_leg_accounts()?;
leg.verify_mint_pair(&quote.base_mint, &quote.quote_mint)?;

// 4. Maker pubkey hygiene: each must appear exactly once, in the fill ix.
let my_keys = [my_fill_authority, my_maker_base, my_maker_quote];
if !all_pubkeys_exclusive(&tx.message, &my_keys) { reject; }

sign(tx);
```

## Quickstart (CLI)

Build the CLI with the `cli` feature:

```sh
cargo build --release --features cli
```

Decode from a serialized transaction:

```sh
./target/release/decode-tx --base64 <BASE64>
```

…or fetch and decode from an RPC node:

```sh
./target/release/decode-tx --tx <SIGNATURE> --rpc-url https://api.mainnet-beta.solana.com
```

With the maker's checks wired in:

```sh
./target/release/decode-tx \
  --tx <SIGNATURE> --rpc-url $RPC_URL \
  --base-mint  So11111111111111111111111111111111111111112 \
  --quote-mint EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --base-decimals 9 --quote-decimals 6 \
  --check <FILL_AUTHORITY> \
  --check <MAKER_BASE_TOKEN_ACCOUNT> \
  --check <MAKER_QUOTE_TOKEN_ACCOUNT>
```

Output:

```text
Decoded transaction
  signatures: 1
  version:    V0
  accounts:   42 (0 unresolved)
  ixs:        3
  [ 0] Comp…CXkV (0 accs, 12 bytes, 0 rfq legs)
  [ 1] proV…d9c8 (38 accs, 217 bytes, 1 rfq legs)
        rfq leg: side=Bid rfq_id=42 expire_at=2000000000 levels=2
          L0: price=85.100000 qty=100.000000 (base_atoms=100000000000 quote_atoms=8510000000)
          L1: price=85.200000 qty=200.000000 (base_atoms=200000000000 quote_atoms=17040000000)

Single-fill check
  OK exactly one SolRfqV2 leg (1 total)

Mint pair check
  OK leg's (base, quote) matches expected

Exclusivity checks
  9X7…fT9: OK 9X7…fT9: exactly one reference, in fill ix 1
  FmQ…7B9: OK FmQ…7B9: exactly one reference, in fill ix 1
  FUU…Vw6: OK FUU…Vw6: exactly one reference, in fill ix 1
```

### CLI exit codes

| Code | Meaning |
|------|---------|
| 0    | All checks passed |
| 1    | Decode error |
| 2    | Bad CLI args / RPC failure |
| 3    | Exclusivity check failed |
| 4    | Not exactly one SolRfqV2 leg |
| 5    | Mint pair mismatch |

Higher numbers mean more structural problems; `4` takes precedence over `3` so the structural issue is diagnosed first.

## API reference

### Top-level decode

| Function | Returns |
|---|---|
| `decode_transaction_bytes(bytes, alt_state)` | `Result<DecodedTransaction>` |
| `decode_transaction_base64(b64, alt_state)` | `Result<DecodedTransaction>` |
| `decode_message_bytes(bytes, alt_state)` | `Result<DecodedMessage>` |
| `decode_message_base64(b64, alt_state)` | `Result<DecodedMessage>` |

ALT state is supplied as a `&[AddressLookupTableEntry]` — the caller fetches each referenced ALT account and passes its `(table_key, addresses)` pair. Empty slice is fine for legacy txs.

### `DecodedFill`

```rust
pub struct DecodedFill {
    pub taker_side: Side,    // 0=Bid, 1=Ask — read directly from the variant body
    pub rfq_id:     u64,
    pub expire_at:  i64,
    pub levels:     Vec<Level>,
}
```

Helpers:

- `fill.levels_price_qty(base_decimals, quote_decimals) -> Vec<PriceQty>` — human-readable `(price, qty)` view.

### `DecodedMessage` / `DecodedTransaction`

| Method | Purpose |
|---|---|
| `single_fill()` | Returns the single SolRfqV2 leg; `Err(FillCountError::NotFound \| Multiple)` otherwise. |
| `fills()` | Iterator over every leg in instruction order. |
| `fill_count()` | Total leg count across the message. |
| `swap_leg_accounts()` | Reads the leg's 13-account slice; returns `SwapLegAccounts`. |

### `SwapLegAccounts`

All meaningful pubkeys read from the leg slice:

```rust
pub struct SwapLegAccounts {
    pub program_id:                     [u8; 32],
    pub swap_authority:                 [u8; 32],
    pub swap_source_token_account:      [u8; 32],
    pub swap_destination_token_account: [u8; 32],
    pub fill_authority:                 [u8; 32],
    pub maker_base_token_account:       [u8; 32],
    pub maker_quote_token_account:      [u8; 32],
    pub base_mint:                      [u8; 32],
    pub quote_mint:                     [u8; 32],
}
```

`leg.verify_mint_pair(&base_mint, &quote_mint) -> Result<(), MintPairMismatch>` confirms the on-wire pair matches the maker's expected pair (looked up via `rfq_id`).

### Exclusivity

`check_pubkey_exclusivity(msg, &[u8; 32]) -> ExclusivityReport` returns a flat struct describing how a pubkey appears in the transaction. `is_exclusive()` is true iff the pubkey appears **exactly once** and that occurrence is in a fill-bearing instruction with no ALT ambiguity.

Convenience wrappers:

- `check_pubkey_exclusivity_base58(msg, &str) -> Result<ExclusivityReport>`
- `all_pubkeys_exclusive(msg, &[[u8; 32]]) -> bool`
- `all_pubkeys_exclusive_base58(msg, &[&str]) -> Result<bool>`

## Security model

This decoder is one layer in a maker's signing pipeline. It surfaces facts about a transaction; it does not enforce policy. The maker's quote registry is the source of truth for:

- Which `rfq_id`s are pending / consumed (the on-chain handler does **not** track this).
- The expected `(base_mint, quote_mint, taker_side, levels, expire_at)` per `rfq_id`.

The maker MUST:

1. Mark `rfq_id` consumed *before* releasing the signature. Never reset Consumed → Pending after a perceived network failure.
2. Verify `fill.taker_side == quote.intended_side` (catches Bid↔Ask spread exploitation).
3. Verify `fill.levels == quote.expected_levels` exactly (no fuzz match).
4. Verify `expire_at` against the wall clock.
5. Verify the on-wire `(base_mint, quote_mint)` matches the quote.
6. Verify the maker's `fill_authority`, `maker_base_token_account`, `maker_quote_token_account` each appear *exactly once* — and only inside the fill-bearing aggregator instruction.

If your maker stack cannot guarantee (1), consider adding an on-chain replay-marker PDA — the `solana-rfq-v2` audit docs cover that mitigation.

## Wire format notes

`Dex::SolRfqV2` body layout (matches upstream `dex-solana-v3` exactly):

```text
tag(117) | taker_side: u8 (RfqSide) | rfq_id: u64 | expire_at: i64 | levels_len: u32 | levels: [Level; n]
```

`taker_side` is **signed-in** by the maker via the transaction signature. The on-chain adapter additionally verifies that `taker_side == derived_side` (from `swap_source.mint` vs `base_mint`/`quote_mint`) and reverts with `TakerSideMismatch` otherwise. This closes the Bid↔Ask spread-exploitation attack for single-level and multi-level quotes alike.

## Crate layout

```
src/
├── lib.rs           # public entry points
├── wire.rs          # hand-rolled Solana tx parser (no solana-sdk dep)
├── transaction.rs   # DecodedTransaction / DecodedMessage / single_fill / swap_leg_accounts
├── idl_types.rs     # Borsh mirrors of dex-solana-v3 SwapArgs / Dex / Route
├── aggregator.rs    # SolRfqV2 leg location + extraction
├── types.rs         # Side, Level, DecodedFill, PriceQty, FillCountError
├── exclusivity.rs   # cross-instruction pubkey exclusivity checks
├── error.rs
└── bin/
    └── decode_tx.rs # CLI (--features cli)

idls/
├── solana_rfq_v2.json
└── dex_solana_v3.json
```

No `solana-sdk`, no curve25519, no async at the library boundary. The CLI feature pulls in `clap` / `reqwest` / `tokio` / `serde_json` for ergonomics only.

## Tests

```sh
cargo test                  # lib only, ~30 tests
cargo test --features cli   # + CLI build, 43 tests total
```

## License

Apache-2.0
