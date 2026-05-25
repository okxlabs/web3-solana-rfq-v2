# solana-rfq-v2

Solana on-chain Request-for-Quote settlement program, plus the off-chain tooling needed to use it safely.

The maker pre-quotes a price ladder, the taker submits the trade, and the on-chain program walks the ladder, transfers the right atom counts, and emits a fill event — all in one instruction, with the maker as an active transaction co-signer.

**Program ID:** `RFQ27dg5gSha2cDzQxuGyhfkz5CK2fUSy3Sjw4Rptyj`

## Layout

```
programs/solana-rfq-v2/   — the on-chain Anchor program (single ix: fill_exact_in)
scripts/cli.ts            — TypeScript CLI for taker submission
tests/                    — Anchor mocha-based integration tests
```

## On-chain program

One instruction:

```rust
fill_exact_in(
    ctx,
    taker_side: Side,           // Bid (taker pays quote) or Ask (taker pays base)
    amount_in_atoms: u64,
    min_out_atoms: u64,
    params: FillExactInParams { rfq_id, expire_at, levels: Vec<Level> },
) -> Result<()>
```

The maker's `fill_authority` is a `Signer<'info>` — every fill requires an active maker signature on the assembled transaction. The handler walks `levels` cheapest-first (sweep math in `sweep.rs`, with strict-monotonic ordering enforced per `taker_side`), moves atoms between the four token accounts, and emits a `FillExactInEvent`.

Designed to be invoked as an inner CPI from OKX `dex-solana-v3`, where the aggregator's `Dex::SolRfqV2` route carries the same `(taker_side, rfq_id, expire_at, levels)` payload that the maker signs.

### Build & test

```sh
anchor build
anchor test                       # full localnet integration tests
cargo test -p solana-rfq-v2       # unit tests on sweep math + handler
```

## TypeScript CLI

`scripts/cli.ts` is a taker helper for end-to-end flows on localnet or devnet:

```sh
yarn rfq fill --rfq-id 42 --amount-in 8_510_000_000 ...
```

(Run `yarn rfq fill --help` for the full flag set.)
