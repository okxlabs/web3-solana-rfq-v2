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
//!    exactly one supported SolRfqV2 leg (zero, multiple, or an unsupported
//!    entrypoint is refuse-to-sign).
//! 2. Looking up its own pre-signed quote via `rfq_id` to recover the
//!    expected `(base_mint, quote_mint, levels)`.
//! 3. Confirming the decoded `levels` matches that quote exactly.
//! 4. Confirming `expire_at` has not passed.
//! 5. Running [`validate_maker_accounts`] on its sensitive accounts.
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
//! - Read on-chain state or ALT contents. Dynamic account indices remain
//!   opaque; maker safety checks operate on static indices.

pub mod error;
pub mod exclusivity;
pub mod idl_types;
pub mod transaction;
pub mod types;
pub mod wire;

mod aggregator;

use base64::Engine;

pub use aggregator::{
    is_dex_solana_v3_program, AGGREGATOR_IDL_JSON, DEX_SOLANA_V3_PROGRAM_ID_BASE58,
    DEX_SOLANA_V3_PROGRAM_ID_BASE58_STAGING, DEX_SOLANA_V3_PROGRAM_ID_BYTES,
    DEX_SOLANA_V3_PROGRAM_ID_BYTES_STAGING, RFQ_V2_PROGRAM_ID_BASE58, RFQ_V2_PROGRAM_ID_BYTES,
};
pub use error::{FillDecoderError, Result};
pub use exclusivity::{
    parse_pubkey_base58, validate_maker_accounts, MakerAccounts, MakerValidationError,
    MakerValidationReport,
};
pub use idl_types::EntrypointKind;
pub use transaction::{DecodedInstruction, DecodedMessage, DecodedTransaction};
pub use types::{DecodedFill, FillCountError, Level, PriceQty, Side};
pub use wire::{AddressTableLookup, MessageVersion};

/// The Anchor IDL for the `solana-rfq-v2` program, embedded at compile time.
pub const IDL_JSON: &str = include_str!("../idls/solana_rfq_v2.json");

/// Decode a full Solana transaction (envelope + message) carried as raw bytes.
///
/// ALT lookup descriptors are parsed, but their on-chain address contents are
/// never fetched or resolved.
pub fn decode_transaction_bytes(bytes: &[u8]) -> Result<DecodedTransaction> {
    let parsed = wire::parse_transaction(bytes)?;
    let mut tx = transaction::decode_transaction(parsed)?;
    extract_fills(&mut tx.message);
    Ok(tx)
}

/// Decode a base64-encoded Solana transaction.
pub fn decode_transaction_base64(b64: &str) -> Result<DecodedTransaction> {
    decode_transaction_bytes(&base64_decode(b64)?)
}

/// Decode a message-only payload (no signature envelope) from raw bytes.
pub fn decode_message_bytes(bytes: &[u8]) -> Result<DecodedMessage> {
    let parsed = wire::parse_message(bytes)?;
    let mut msg = transaction::decode_message(parsed)?;
    extract_fills(&mut msg);
    Ok(msg)
}

/// Decode a base64-encoded message-only payload.
pub fn decode_message_base64(b64: &str) -> Result<DecodedMessage> {
    decode_message_bytes(&base64_decode(b64)?)
}

fn base64_decode(b64: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| FillDecoderError::InvalidEncoding)
}

/// Walk every top-level instruction whose program is `dex-solana-v3` and
/// attach any SolRfqV2 legs found in its `SwapArgs.routes`.
fn extract_fills(msg: &mut DecodedMessage) {
    let static_account_keys = &msg.static_account_keys;
    for ix in msg.instructions.iter_mut() {
        let program_id = static_account_keys.get(ix.program_id_index as usize);
        if program_id.is_some_and(is_dex_solana_v3_program) {
            let (kind, fills) = aggregator::decode_solrfqv2_legs(&ix.data);
            ix.entrypoint = kind;
            ix.fills = fills;
        }
    }
}

#[cfg(test)]
mod constructed_transaction_tests {
    use super::*;

    const FILL_AUTHORITY: [u8; 32] = [0x11; 32];
    const MAKER_BASE: [u8; 32] = [0x22; 32];
    const MAKER_QUOTE: [u8; 32] = [0x33; 32];

    #[derive(Clone, Copy)]
    enum Scenario {
        SingleHop,
        MultiHop,
        WrongMakerSlots,
        DynamicRfqProgram,
    }

    fn push_compact_u16(out: &mut Vec<u8>, mut value: usize) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn rfq_route(index: u8) -> Vec<u8> {
        let mut dex = vec![117, 0]; // Dex::SolRfqV2, Side::Bid
        dex.extend_from_slice(&42u64.to_le_bytes());
        dex.extend_from_slice(&2_000_000_000i64.to_le_bytes());
        dex.extend_from_slice(&1u32.to_le_bytes());
        dex.extend_from_slice(&100u64.to_le_bytes());
        dex.extend_from_slice(&85u64.to_le_bytes());
        dex.extend_from_slice(&10_000u16.to_le_bytes());
        dex.push(index);
        dex
    }

    fn other_route(index: u8) -> Vec<u8> {
        let mut route = vec![2]; // a body-less non-RFQ Dex variant
        route.extend_from_slice(&10_000u16.to_le_bytes());
        route.push(index);
        route
    }

    fn swap_data(multi_hop: bool) -> Vec<u8> {
        let routes = if multi_hop {
            vec![other_route(0), rfq_route(1)]
        } else {
            vec![rfq_route(0)]
        };
        let mut data = idl_types::entrypoint::SWAP.to_vec();
        data.extend_from_slice(&7u64.to_le_bytes()); // order_id
        data.extend_from_slice(&85u64.to_le_bytes()); // amount_in
        data.extend_from_slice(&0u64.to_le_bytes()); // expect_amount_out
        data.extend_from_slice(&0u16.to_le_bytes()); // slippage
        data.extend_from_slice(&(routes.len() as u32).to_le_bytes());
        for route in routes {
            data.extend_from_slice(&route);
        }
        data
    }

    /// Build a complete serialized v0 transaction. The instruction references
    /// dynamic account index 6, while no ALT contents are supplied to the
    /// decoder. Only the lookup descriptor is present on the wire.
    fn transaction_bytes(scenario: Scenario) -> Vec<u8> {
        let multi_hop = matches!(scenario, Scenario::MultiHop);
        let mut static_keys = vec![
            FILL_AUTHORITY,
            MAKER_BASE,
            MAKER_QUOTE,
            DEX_SOLANA_V3_PROGRAM_ID_BYTES,
            RFQ_V2_PROGRAM_ID_BYTES,
            [0x55; 32],
        ];
        if matches!(scenario, Scenario::DynamicRfqProgram) {
            static_keys[4] = [0x44; 32];
        }

        // In the multi-hop case, these first positions model the preceding
        // DEX hop and all reference opaque dynamic accounts.
        let leg_offset = if multi_hop { 8 } else { 2 };
        let mut account_indices = vec![6u8; leg_offset + 13];
        if multi_hop {
            for (position, index) in account_indices[..leg_offset].iter_mut().enumerate() {
                *index = 6 + (position % 6) as u8;
            }
        }
        account_indices[leg_offset] = if matches!(scenario, Scenario::DynamicRfqProgram) {
            6
        } else {
            4
        };
        account_indices[leg_offset + 4] = 0;
        account_indices[leg_offset + 5] = 1;
        account_indices[leg_offset + 6] = 2;
        if matches!(scenario, Scenario::WrongMakerSlots) {
            account_indices.swap(leg_offset + 5, leg_offset + 6);
        }

        let data = swap_data(multi_hop);
        let mut message = vec![0x80]; // versioned message, version 0
        message.extend_from_slice(&[1, 0, 3]); // header: 1 signer, 3 readonly unsigned
        push_compact_u16(&mut message, static_keys.len());
        for key in static_keys {
            message.extend_from_slice(&key);
        }
        message.extend_from_slice(&[0x77; 32]); // recent blockhash
        push_compact_u16(&mut message, 1); // instruction count
        message.push(3); // static dex-solana-v3 program index
        push_compact_u16(&mut message, account_indices.len());
        message.extend_from_slice(&account_indices);
        push_compact_u16(&mut message, data.len());
        message.extend_from_slice(&data);
        push_compact_u16(&mut message, 1); // lookup table count
        message.extend_from_slice(&[0x99; 32]); // opaque ALT account key
        let writable_alt_indexes: &[u8] = if multi_hop { &[0, 1, 2, 3] } else { &[0] };
        let readonly_alt_indexes: &[u8] = if multi_hop { &[4, 5] } else { &[1] };
        push_compact_u16(&mut message, writable_alt_indexes.len());
        message.extend_from_slice(writable_alt_indexes);
        push_compact_u16(&mut message, readonly_alt_indexes.len());
        message.extend_from_slice(readonly_alt_indexes);

        let mut tx = Vec::new();
        push_compact_u16(&mut tx, 1); // signature vector length
        tx.extend_from_slice(&[0u8; 64]); // maker signature placeholder
        tx.extend_from_slice(&message);
        tx
    }

    fn maker_accounts() -> MakerAccounts {
        MakerAccounts {
            fill_authority: FILL_AUTHORITY,
            maker_base_token_account: MAKER_BASE,
            maker_quote_token_account: MAKER_QUOTE,
        }
    }

    #[test]
    fn constructed_v0_transaction_validates_without_alt_contents() {
        let tx = decode_transaction_bytes(&transaction_bytes(Scenario::SingleHop)).unwrap();
        assert_eq!(tx.message.loaded_writable_count, 1);
        assert_eq!(tx.message.loaded_readonly_count, 1);
        assert!(tx.message.static_account_key(6).is_none());

        let fill = tx.single_fill().unwrap();
        assert_eq!(fill.rfq_id, 42);
        assert_eq!(
            fill.levels,
            vec![Level {
                base_atoms: 100,
                quote_atoms: 85
            }]
        );

        let report = validate_maker_accounts(&tx.message, &maker_accounts()).unwrap();
        assert_eq!(report.instruction_index, 0);
        assert_eq!(report.leg_offset, 2);
    }

    #[test]
    fn constructed_multi_hop_v0_transaction_validates_rfq_after_opaque_alt_hop() {
        let tx = decode_transaction_bytes(&transaction_bytes(Scenario::MultiHop)).unwrap();
        assert_eq!(tx.message.loaded_writable_count, 4);
        assert_eq!(tx.message.loaded_readonly_count, 2);
        assert_eq!(tx.fill_count(), 1); // the non-RFQ hop is ignored
        assert_eq!(tx.single_fill().unwrap().rfq_id, 42);

        let report = validate_maker_accounts(&tx.message, &maker_accounts()).unwrap();
        assert_eq!(report.instruction_index, 0);
        assert_eq!(report.leg_offset, 8);
    }

    #[test]
    fn constructed_v0_transaction_rejects_wrong_maker_slots() {
        let tx = decode_transaction_bytes(&transaction_bytes(Scenario::WrongMakerSlots)).unwrap();
        assert_eq!(
            validate_maker_accounts(&tx.message, &maker_accounts()).unwrap_err(),
            MakerValidationError::LegNotFound
        );
    }

    #[test]
    fn constructed_v0_transaction_rejects_dynamic_rfq_program() {
        let tx = decode_transaction_bytes(&transaction_bytes(Scenario::DynamicRfqProgram)).unwrap();
        assert_eq!(
            validate_maker_accounts(&tx.message, &maker_accounts()).unwrap_err(),
            MakerValidationError::RfqProgramNotStatic
        );
    }
}
