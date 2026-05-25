//! `fill-decoder`: off-chain decoder for OKX `dex-solana-v3` aggregator
//! transactions, specialised for inspecting embedded `solana-rfq-v2` RFQ legs.
//!
//! ## Intended use
//!
//! A maker receives a (presumed) OKX-aggregator transaction. They want to
//! decide whether to sign it. This crate walks the top-level instructions,
//! finds every `dex-solana-v3` swap instruction, decodes its `SwapArgs` via
//! the embedded IDL, and extracts every `Dex::SolRfqV2` leg as a
//! [`DecodedFill`] carrying `rfq_id`, `expire_at`, and `levels`.
//!
//! The maker then validates by:
//! 1. Calling [`DecodedTransaction::single_fill`] to confirm the tx carries
//!    exactly one SolRfqV2 leg (zero or multiple is refuse-to-sign).
//! 2. Looking up its own pre-signed quote via `rfq_id` to recover the
//!    expected `(base_mint, quote_mint, levels)`.
//! 3. Confirming the decoded `levels` matches that quote exactly.
//! 4. Confirming `expire_at` has not passed.
//! 5. Running [`check_pubkey_exclusivity`] on its sensitive accounts.
//!
//! The taker's `amount_in` is intentionally not surfaced — the pre-signed
//! levels bound any acceptable trade, so the maker's commitment holds
//! regardless of the size the aggregator routes through this leg.
//!
//! ## What this crate does NOT do
//!
//! - Validate the maker's signing policy (mints, fee payer, signer set, etc).
//! - Detect top-level `fill_exact_in` invocations bypassing the aggregator —
//!   the caller is assumed to have already confirmed the tx is an OKX
//!   aggregator tx.
//! - Track `rfq_id` replay across transactions.
//! - Read on-chain state (token account mints, ALT contents — ALT state must
//!   be supplied by the caller).

pub mod error;
pub mod exclusivity;
pub mod idl_types;
pub mod transaction;
pub mod types;
pub mod wire;

mod aggregator;

use base64::Engine;

pub use aggregator::{
    SwapLegLookupError, AGGREGATOR_IDL_JSON, DEX_SOLANA_V3_PROGRAM_ID_BASE58,
    DEX_SOLANA_V3_PROGRAM_ID_BYTES, RFQ_V2_PROGRAM_ID_BASE58, RFQ_V2_PROGRAM_ID_BYTES,
};
pub use error::{FillDecoderError, Result};
pub use exclusivity::{
    all_pubkeys_exclusive, all_pubkeys_exclusive_base58, check_pubkey_exclusivity,
    check_pubkey_exclusivity_base58, parse_pubkey_base58, ExclusivityReport,
};
pub use transaction::{
    AddressLookupTableEntry, DecodedInstruction, DecodedMessage, DecodedTransaction,
    MintPairMismatch, ResolvedAccount, SwapLegAccounts, SwapLegError,
};
pub use types::{DecodedFill, FillCountError, Level, PriceQty, Side};
pub use wire::MessageVersion;

/// The Anchor IDL for the `solana-rfq-v2` program, embedded at compile time.
pub const IDL_JSON: &str = include_str!("../idls/solana_rfq_v2.json");

/// Decode a full Solana transaction (envelope + message) carried as raw bytes.
///
/// `alt_state` resolves v0 Address Lookup Table references. Pass an empty
/// slice when no ALT resolution is needed or when the tx is legacy.
pub fn decode_transaction_bytes(
    bytes: &[u8],
    alt_state: &[AddressLookupTableEntry],
) -> Result<DecodedTransaction> {
    let parsed = wire::parse_transaction(bytes)?;
    let mut tx = transaction::from_parsed_tx(parsed, alt_state);
    extract_fills(&mut tx.message);
    Ok(tx)
}

/// Decode a base64-encoded Solana transaction.
pub fn decode_transaction_base64(
    b64: &str,
    alt_state: &[AddressLookupTableEntry],
) -> Result<DecodedTransaction> {
    decode_transaction_bytes(&base64_decode(b64)?, alt_state)
}

/// Decode a message-only payload (no signature envelope) from raw bytes.
pub fn decode_message_bytes(
    bytes: &[u8],
    alt_state: &[AddressLookupTableEntry],
) -> Result<DecodedMessage> {
    let parsed = wire::parse_message(bytes)?;
    let mut msg = transaction::resolve(parsed, alt_state);
    extract_fills(&mut msg);
    Ok(msg)
}

/// Decode a base64-encoded message-only payload.
pub fn decode_message_base64(
    b64: &str,
    alt_state: &[AddressLookupTableEntry],
) -> Result<DecodedMessage> {
    decode_message_bytes(&base64_decode(b64)?, alt_state)
}

fn base64_decode(b64: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| FillDecoderError::InvalidEncoding)
}

/// Walk every top-level instruction whose program is `dex-solana-v3` and
/// attach any SolRfqV2 legs found in its `SwapArgs.routes`.
fn extract_fills(msg: &mut DecodedMessage) {
    for ix in msg.instructions.iter_mut() {
        if ix.program_id.pubkey == DEX_SOLANA_V3_PROGRAM_ID_BYTES {
            ix.fills = aggregator::decode_solrfqv2_legs(&ix.data);
        }
    }
}
