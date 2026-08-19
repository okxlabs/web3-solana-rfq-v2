# fill-decoder

Offline decoder for OKX `dex-solana-v3` transactions carrying
`solana-rfq-v2` RFQ legs. It is designed for a maker deciding whether to
co-sign an untrusted transaction.

The library has no Solana RPC or `solana-sdk` dependency. It parses legacy and
v0 messages, decodes RFQ route data, and validates the maker's protected
accounts from static account indices.

## ALT model

The decoder parses v0 `addressTableLookups` from the message but never fetches
or resolves the referenced ALT account contents. Dynamic account indices remain
opaque.

This is safe for the protected maker accounts under execute-or-fail semantics:

- `fill_authority`, `maker_base_token_account`, and
  `maker_quote_token_account` must be in `staticAccountKeys`.
- `RFQ_V2_PROGRAM_ID` must also be static so it can anchor the RFQ leg's fixed
  13-account slice.
- If an ALT dynamically loads a pubkey that is already static, Solana rejects
  the transaction with `AccountLoadedTwice` before execution.

The recognised top-level `dex-solana-v3` program id must be static. A swap whose
top-level program id is dynamic is not decoded and therefore fails the
exactly-one-fill policy.

This model proves that any transaction which executes successfully uses the
expected maker accounts exactly once in the expected RFQ slots. It does not try
to prove that every opaque dynamic account is otherwise valid.

## Decode and validate

```rust
use fill_decoder::{
    decode_transaction_base64, validate_maker_accounts, MakerAccounts,
};

let tx = decode_transaction_base64(b64)?;

// Exactly one SolRfqV2 leg must be present.
let fill = tx.single_fill()?;

// Token-ledger entrypoints are refused by the conservative signing policy.
if tx.has_token_ledger_fill() {
    reject();
}

// Match signed RFQ data against the maker's quote registry.
let quote = registry.lookup_by_rfq_id(fill.rfq_id)?;
if fill.taker_side != quote.intended_side { reject(); }
if fill.levels != quote.expected_levels { reject(); }
if fill.expire_at < now_wall_clock { reject(); }

// The registry binds these known token accounts to the quote's mint pair.
let report = validate_maker_accounts(
    &tx.message,
    &MakerAccounts {
        fill_authority: quote.fill_authority,
        maker_base_token_account: quote.maker_base_token_account,
        maker_quote_token_account: quote.maker_quote_token_account,
    },
)?;

registry.mark_consumed(quote.id)?;
sign(tx);
```

`validate_maker_accounts` requires:

1. All three maker pubkeys are distinct and occur exactly once in the static
   key list.
2. `fill_authority` is a transaction signer.
3. Both maker token accounts are writable.
4. The transaction contains exactly one decoded SolRfqV2 leg.
5. `RFQ_V2_PROGRAM_ID` is static and referenced exactly once.
6. Each maker index is referenced exactly once across all top-level
   instructions, including program-id positions.
7. The RFQ program and maker indices occupy slots `0/4/5/6` of the same fixed
   SolRfqV2 leg slice.

The on-chain program additionally constrains the maker token accounts' mint,
authority, and token program. The maker registry must bind the expected token
accounts to the quoted base/quote pair.

## CLI

Build:

```sh
cargo build --release -p fill-decoder --features cli
```

Decode serialized transaction bytes without RPC or ALT loading:

```sh
./target/release/decode-tx --base64 <BASE64>
```

Fetch an already-submitted transaction over RPC, then decode it locally:

```sh
./target/release/decode-tx \
  --tx <SIGNATURE> \
  --rpc-url $RPC_URL
```

Run the static maker-account check:

```sh
./target/release/decode-tx \
  --base64 <BASE64> \
  --fill-authority <FILL_AUTHORITY> \
  --maker-base-token-account <MAKER_BASE_TOKEN_ACCOUNT> \
  --maker-quote-token-account <MAKER_QUOTE_TOKEN_ACCOUNT>
```

Optional flags:

- `--base-decimals` and `--quote-decimals` render human-readable price/qty.
- `--allow-token-ledger` opts into token-ledger entrypoints.
- `--json` emits machine-readable output.

CLI exit codes:

| Code | Meaning |
|---:|---|
| 0 | Checks passed |
| 1 | Decode error |
| 2 | Invalid CLI arguments or transaction RPC failure |
| 3 | Static maker-account validation failed |
| 4 | Transaction does not contain exactly one SolRfqV2 leg |
| 6 | SolRfqV2 leg uses a token-ledger entrypoint |

## Public API

Decode functions no longer accept ALT state:

```rust
decode_transaction_bytes(bytes)
decode_transaction_base64(b64)
decode_message_bytes(bytes)
decode_message_base64(b64)
```

Relevant message fields:

```rust
pub struct DecodedMessage {
    pub static_account_keys: Vec<[u8; 32]>,
    pub instructions: Vec<DecodedInstruction>,
    pub address_table_lookups: Vec<AddressTableLookup>,
    pub loaded_writable_count: usize,
    pub loaded_readonly_count: usize,
    // ...
}

pub struct DecodedInstruction {
    pub program_id_index: u8,
    pub account_indices: Vec<u8>,
    pub data: Vec<u8>,
    pub fills: Vec<DecodedFill>,
    // ...
}
```

`static_account_key(index)` returns a pubkey only for the static key region.
`total_account_count()` includes the number of selected writable and readonly
ALT indices without resolving their addresses.

## Signing checklist

1. Require exactly one SolRfqV2 leg.
2. Refuse token-ledger entrypoints unless explicitly supported.
3. Match `rfq_id`, side, expiry, and levels against the pending quote.
4. Validate the exact expected maker accounts with
   `validate_maker_accounts`.
5. Ensure the quote registry binds those token accounts to the expected mint
   pair.
6. Atomically mark the quote consumed before releasing the signature.

## Tests

```sh
cargo test -p fill-decoder
cargo test -p fill-decoder --all-features
```
