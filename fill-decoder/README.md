# fill-decoder

Off-chain decoder for OKX `dex-solana-v3` aggregator transactions that surfaces embedded `solana-rfq-v2` RFQ legs to a maker.

**Use case:** a maker receives a transaction they're asked to co-sign, runs it through this decoder, and gets enough structured data to decide whether the trade matches a quote they pre-committed to — without RPC, without `solana-sdk`, without curve crypto.

## What it does

Given a base64- or bytes-encoded Solana transaction, the decoder:

1. Parses the transaction (legacy + v0 messages, ALT-resolved when state is supplied).
2. Walks top-level instructions; for each `dex-solana-v3` swap entrypoint (mainnet `proVF…X3u` or staging `preX…1bbB`), decodes its `SwapArgs` via the embedded IDL.
3. Tags each swap instruction with its `EntrypointKind` (`Concrete` carries `amount_in` in args; `TokenLedger` derives it from a runtime token-ledger account).
4. Extracts every `Dex::SolRfqV2` leg as a [`DecodedFill`] carrying `taker_side`, `rfq_id`, `expire_at`, and `levels`.
5. Exposes helpers for the safety checks a maker should perform before signing.

## What it does NOT do

- Read on-chain state from the library. Token account mints and ALT contents must be supplied by the caller as `&[AddressLookupTableEntry]`. The CLI fetches ALTs over JSON-RPC and refuses to proceed if any entry remains unresolved.
- Validate the maker's signing policy (mints, fee payer, signer set, etc.) — only surfaces enough data for the caller to do so.
- Treat `rfq_id` as a security primitive. `rfq_id` is event-only — emitted by the on-chain adapter for observability and never gated on. Quote-replay defence lives entirely in the maker's quote registry; see [Security model](#security-model).
- Detect top-level `fill_exact_in` invocations bypassing the aggregator — the caller is assumed to have already confirmed the tx is an OKX aggregator tx.

## Quickstart (library)

```rust
use fill_decoder::{
    all_pubkeys_exclusive, decode_transaction_base64, parse_pubkey_base58, FillCountError,
};

let tx = decode_transaction_base64(b64, &alt_state)?;

// 1. Exactly one SolRfqV2 leg.
let fill = tx.single_fill()?;                            // -> FillCountError on 0 or >1

// 2. Refuse token-ledger entrypoints (amount_in opaque, atomic-arb composable).
if tx.has_token_ledger_fill() { reject; }

// 3. Look up the pre-signed quote intent and match every dimension.
//    `rfq_id` is an event-only field, but the maker can use it as their own
//    registry key by convention. Any maker-controlled identifier works.
let quote = registry.lookup_by_rfq_id(fill.rfq_id)?;
if fill.taker_side != quote.intended_side   { reject; }  // signed-in side
if fill.levels     != quote.expected_levels { reject; }
if fill.expire_at  <  now_wall_clock        { reject; }

// 4. Confirm the leg's on-wire mint pair matches.
let leg = tx.swap_leg_accounts()?;
leg.verify_mint_pair(&quote.base_mint, &quote.quote_mint)?;

// 5. Maker pubkey hygiene: each must appear exactly once, in the fill ix.
let my_keys = [my_fill_authority, my_maker_base, my_maker_quote];
if !all_pubkeys_exclusive(&tx.message, &my_keys) { reject; }

// 6. Mark the quote consumed in the registry *before* releasing the signature.
registry.mark_consumed(quote.id)?;

sign(tx);
```

## Quickstart (CLI)

Build the CLI with the `cli` feature:

```sh
cargo build --release --features cli
```

Decode from a serialized transaction (legacy / v0 without ALTs):

```sh
./target/release/decode-tx --base64 <BASE64>
```

…or fetch and decode from an RPC node:

```sh
./target/release/decode-tx --tx <SIGNATURE> --rpc-url https://api.mainnet-beta.solana.com
```

**ALT resolution is mandatory** for any v0 transaction that references address lookup tables. If the message carries at least one `AddressTableLookup`, the CLI requires `--rpc-url` (or `RPC_URL` env) and exits non-zero if it cannot fully resolve every entry. Partial resolution would silently weaken `swap_leg_accounts`, `verify_mint_pair`, and exclusivity checks into "true for the resolved subset" — fail-closed is the only safe default.

The CLI fetches each referenced ALT account, slices off the 56-byte header, and re-decodes with full account resolution.

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
  signatures: 2
  version:    V0
  accounts:   29 (0 unresolved)
  ixs:        3
  ALTs:
    AKxSPR6vKrfK3ccnZ5Zvg1TXsNDstFGuZExNjdXKx1xS (w=4, r=9)
  [ 0] Comp…1111 (0 accs, 5 bytes, 0 rfq legs)
  [ 1] Comp…1111 (0 accs, 9 bytes, 0 rfq legs)
  [ 2] proV…X3u8 (40 accs, 90 bytes, 1 rfq legs, Concrete)
        rfq leg: side=Bid rfq_id=42 expire_at=2000000000 levels=1
          L0: price=85.100000 qty=100.000000 (base_atoms=100000000000 quote_atoms=8510000000)

Single-fill check
  OK exactly one SolRfqV2 leg (1 total)

Entrypoint check
  OK no token-ledger entrypoint carries a SolRfqV2 leg

SolRfqV2 leg accounts
  swap_authority (taker)           63Y8…Frf8
  swap_source_token_account        6z4X…nJsd
  swap_destination_token_account   FANw…KphR
  fill_authority (maker)           ih8z…Jse2
  maker_base_token_account         7Qes…Fbho
  maker_quote_token_account        2U9H…8MsG
  base_mint                        So11…1112
  quote_mint                       EPjF…Dt1v

Mint pair check
  OK leg's (base, quote) matches expected

Exclusivity checks
  OK ih8z…Jse2: exactly one reference, in fill ix 2
  OK 7Qes…Fbho: exactly one reference, in fill ix 2
  OK 2U9H…8MsG: exactly one reference, in fill ix 2
```

Pass `--allow-token-ledger` to opt in to token-ledger entrypoints carrying a SolRfqV2 leg (off by default — see [Security model](#security-model)).

### CLI exit codes

| Code | Meaning |
|------|---------|
| 0    | All checks passed |
| 1    | Decode error |
| 2    | Bad CLI args / RPC failure / unresolved ALT entries |
| 3    | Exclusivity check failed |
| 4    | Not exactly one SolRfqV2 leg |
| 5    | Mint pair mismatch |
| 6    | Token-ledger entrypoint carries a SolRfqV2 leg (override with `--allow-token-ledger`) |

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
    pub rfq_id:     u64,     // event-only; emitted by the on-chain adapter, never gated on
    pub expire_at:  i64,
    pub levels:     Vec<Level>,
}
```

Helpers:

- `fill.levels_price_qty(base_decimals, quote_decimals) -> Vec<PriceQty>` — human-readable `(price, qty)` view.

### `DecodedInstruction`

Each top-level ix carries `entrypoint: Option<EntrypointKind>`. It is `Some` only when the program is a recognised `dex-solana-v3` deployment and the discriminator matched a known swap entrypoint:

```rust
pub enum EntrypointKind {
    Concrete,     // SwapArgs carries amount_in in the instruction data
    TokenLedger,  // amount_in derived from a token-ledger account at runtime
}
```

### `DecodedMessage` / `DecodedTransaction`

| Method | Purpose |
|---|---|
| `single_fill()` | Returns the single SolRfqV2 leg; `Err(FillCountError::NotFound \| Multiple)` otherwise. |
| `fills()` | Iterator over every leg in instruction order. |
| `fill_count()` | Total leg count across the message. |
| `has_token_ledger_fill()` | `true` if any SolRfqV2 leg rides inside a token-ledger entrypoint. Refuse-to-sign signal. |
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

`leg.verify_mint_pair(&base_mint, &quote_mint) -> Result<(), MintPairMismatch>` confirms the on-wire pair matches the pair the maker pre-committed to in their quote registry.

### Exclusivity

`check_pubkey_exclusivity(msg, &[u8; 32]) -> ExclusivityReport` returns a flat struct describing how a pubkey appears in the transaction. `is_exclusive()` is true iff the pubkey appears **exactly once** and that occurrence is in a fill-bearing instruction with no ALT ambiguity.

Convenience wrappers:

- `check_pubkey_exclusivity_base58(msg, &str) -> Result<ExclusivityReport>`
- `all_pubkeys_exclusive(msg, &[[u8; 32]]) -> bool`
- `all_pubkeys_exclusive_base58(msg, &[&str]) -> Result<bool>`

## Security model

This decoder is one layer in a maker's signing pipeline. It surfaces facts about a transaction; it does not enforce policy. The maker's quote registry is the source of truth.

### `rfq_id` is not a security primitive

`rfq_id` is emitted on-chain for observability and **never gated on**. The on-chain adapter does not track which `rfq_id`s have been used; the same value can appear in any number of fills. Treat it as an event-log tag, not as an identifier the chain will deduplicate for you.

Quote-replay defence is entirely off-chain. The maker chooses an internal quote identifier (which can be `rfq_id` by convention, or anything else) and ensures each pre-signed quote intent is consumed at most once.

### Refuse-to-sign checklist

1. **Exactly one SolRfqV2 leg.** `single_fill()` returns `Err` for zero or multiple fills.
2. **No token-ledger entrypoint.** `has_token_ledger_fill()` flags the case where a SolRfqV2 leg rides inside `SWAP_TOB_WITH_TOKEN_LEDGER` / `SWAP_TOB_WITH_RECEIVER_TOKEN_LEDGER`. These derive `amount_in` from a runtime account and compose atomically with other swaps in the same tx, creating a clean arbitrage vector for the taker. Default-deny; opt in only if you explicitly support multi-hop composition.
3. **Quote not already consumed.** Look up the maker's registry entry, confirm it's in Pending state, and atomically transition it to Consumed *before* releasing the signature. Never reset Consumed → Pending on a perceived network failure — the safest assumption is that the signed tx landed.
4. **`fill.taker_side == quote.intended_side`.** Catches Bid↔Ask spread exploitation. The on-chain adapter also enforces this against the derived side, but check it off-chain first.
5. **`fill.levels == quote.expected_levels`** exactly (no fuzz match).
6. **`expire_at` not in the past.** Compare against the maker's wall clock; allow a small skew if you want.
7. **On-wire `(base_mint, quote_mint)` matches the quote.**
8. **Maker pubkey hygiene.** `fill_authority`, `maker_base_token_account`, `maker_quote_token_account` each appear *exactly once*, inside the fill-bearing aggregator instruction. Resolves to `false` if any of those accounts shows up in a second instruction or behind an unresolved ALT entry.

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
cargo test                  # lib only
cargo test --features cli   # + CLI build, 47 tests total
```

## License

MIT
