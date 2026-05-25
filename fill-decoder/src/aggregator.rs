//! Decode `Dex::SolRfqV2` legs from an OKX `dex-solana-v3` aggregator swap
//! instruction.
//!
//! Pipeline:
//! 1. Match the 8-byte entrypoint discriminator against the known swap
//!    entrypoints.
//! 2. Borsh-decode the args (either `SwapArgs` or `SwapArgsTokenLedger`) to
//!    walk the `routes` vector.
//! 3. For each route whose `dex == Dex::SolRfqV2`, emit a [`DecodedFill`]
//!    carrying the variant body (`rfq_id`, `expire_at`, `levels`).

use crate::idl_types::{decode_swap_args, Dex};
use crate::transaction::DecodedInstruction;
use crate::types::{DecodedFill, Side};

/// Base-58 form of the canonical `solana-rfq-v2` program id.
pub const RFQ_V2_PROGRAM_ID_BASE58: &str = "RFQ27dg5gSha2cDzQxuGyhfkz5CK2fUSy3Sjw4Rptyj";

/// 32-byte form of `RFQ_V2_PROGRAM_ID_BASE58`. Verified by parity test.
/// Used to locate the SolRfqV2 leg's account slice inside an aggregator
/// instruction's account list (slot 0 of every SolRfqV2 leg is the program id).
pub const RFQ_V2_PROGRAM_ID_BYTES: [u8; 32] = [
    0x06, 0x36, 0x37, 0xd0, 0x81, 0x4e, 0x36, 0x3e, 0x28, 0x17, 0x3c, 0xa6, 0x5d, 0x04, 0xe1, 0x35,
    0xad, 0x9b, 0xba, 0xc1, 0x6d, 0xf0, 0xff, 0x0d, 0x3f, 0xd8, 0x5a, 0x74, 0x6a, 0x14, 0xa2, 0x3e,
];

/// SolRfqV2 leg slice layout, relative to the leg's start in `remaining_accounts`
/// (verified against `programs/dex-solana-v3/src/adapters/sol_rfq_v2.rs::parse_accounts`):
///
/// ```text
/// [0]  dex_program_id (== RFQ_V2_PROGRAM_ID_BYTES)
/// [1]  swap_authority (taker)
/// [2]  swap_source_token_account
/// [3]  swap_destination_token_account
/// [4]  fill_authority (maker)
/// [5]  maker_base_token_account
/// [6]  maker_quote_token_account
/// [7]  base_mint
/// [8]  quote_mint
/// [9]  base_token_program
/// [10] quote_token_program
/// [11] instructions_sysvar
/// [12] event_authority
/// ```
pub(crate) const LEG_PROGRAM_ID: usize = 0;
pub(crate) const LEG_SWAP_AUTHORITY: usize = 1;
pub(crate) const LEG_SOURCE_TOKEN_ACCOUNT: usize = 2;
pub(crate) const LEG_DESTINATION_TOKEN_ACCOUNT: usize = 3;
pub(crate) const LEG_FILL_AUTHORITY: usize = 4;
pub(crate) const LEG_MAKER_BASE_TOKEN_ACCOUNT: usize = 5;
pub(crate) const LEG_MAKER_QUOTE_TOKEN_ACCOUNT: usize = 6;
pub(crate) const LEG_BASE_MINT: usize = 7;
pub(crate) const LEG_QUOTE_MINT: usize = 8;
pub(crate) const SOL_RFQ_V2_LEG_WIDTH: usize = 13;

/// Base-58 form of the canonical `dex-solana-v3` program id.
pub const DEX_SOLANA_V3_PROGRAM_ID_BASE58: &str = "proVF4pMXVaYqmy4NjniPh4pqKNfMmsihgd4wdkCX3u";

/// 32-byte form of `DEX_SOLANA_V3_PROGRAM_ID_BASE58`. Verified by parity test.
pub const DEX_SOLANA_V3_PROGRAM_ID_BYTES: [u8; 32] = [
    0x0c, 0x42, 0x9b, 0xd7, 0xc1, 0x8f, 0x50, 0xf8, 0x15, 0x6d, 0x9a, 0xfc, 0x1c, 0xdd, 0xe7, 0x2d,
    0xf6, 0x68, 0xd9, 0xab, 0x3b, 0xec, 0xaf, 0x6b, 0x57, 0x0d, 0x57, 0x66, 0x64, 0x5a, 0xd9, 0xc8,
];

/// Embedded IDL — the full dex-solana-v3 IDL we Borsh-mirror through
/// `crate::idl_types`. Exposed for tooling that wants to introspect it.
pub const AGGREGATOR_IDL_JSON: &str = include_str!("../idls/dex_solana_v3.json");

/// Locate the SolRfqV2 leg's 13-account slice inside a dex-solana-v3 swap
/// instruction's account list.
///
/// Scans `ix.accounts` for the position where the resolved pubkey equals
/// `RFQ_V2_PROGRAM_ID_BYTES`. That position is the leg slice's position 0
/// (the adapter's `dex_program_id` slot); the remaining 12 slots follow it
/// in fixed order.
///
/// Returns the offset of the leg slice's start within `ix.accounts`. Returns
/// an error if no SolRfqV2 leg is present, if the resolved program id at slot
/// 0 doesn't have room for the full 13-slot slice, or if it appears more than
/// once (multiple SolRfqV2 legs are out of scope for the maker's
/// exactly-one-fill invariant — see [`crate::FillCountError`]).
pub(crate) fn find_sol_rfq_v2_leg_offset(
    ix: &DecodedInstruction,
) -> Result<usize, SwapLegLookupError> {
    let mut matches = ix
        .accounts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.is_resolved && a.pubkey == RFQ_V2_PROGRAM_ID_BYTES)
        .map(|(i, _)| i);
    let first = matches.next().ok_or(SwapLegLookupError::ProgramIdNotFound)?;
    if matches.next().is_some() {
        return Err(SwapLegLookupError::MultipleProgramIdSlots);
    }
    if ix.accounts.len() < first + SOL_RFQ_V2_LEG_WIDTH {
        return Err(SwapLegLookupError::TruncatedLegSlice {
            offset: first,
            available: ix.accounts.len() - first,
        });
    }
    Ok(first)
}

/// Why locating a SolRfqV2 leg slice failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapLegLookupError {
    /// `RFQ_V2_PROGRAM_ID_BYTES` was not present in the instruction's
    /// account list. Either the ix has no SolRfqV2 leg, or the program id at
    /// slot 0 of the leg slice is an unresolved ALT entry.
    ProgramIdNotFound,
    /// The program id appears in more than one slot — possibly multiple
    /// SolRfqV2 legs. The maker's signing flow assumes exactly one fill;
    /// see [`crate::FillCountError`].
    MultipleProgramIdSlots,
    /// The program id is present but the account list ends before the leg's
    /// 13-slot layout completes.
    TruncatedLegSlice { offset: usize, available: usize },
}

impl core::fmt::Display for SwapLegLookupError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SwapLegLookupError::ProgramIdNotFound => write!(
                f,
                "RFQ_V2_PROGRAM_ID_BYTES not found among resolved accounts in this instruction"
            ),
            SwapLegLookupError::MultipleProgramIdSlots => write!(
                f,
                "RFQ_V2_PROGRAM_ID_BYTES appears in multiple account slots; \
                 exactly one SolRfqV2 leg is expected"
            ),
            SwapLegLookupError::TruncatedLegSlice { offset, available } => write!(
                f,
                "SolRfqV2 leg starts at index {offset} but only {available} accounts follow; \
                 expected {SOL_RFQ_V2_LEG_WIDTH}"
            ),
        }
    }
}

impl std::error::Error for SwapLegLookupError {}

/// Returns one [`DecodedFill`] per `Dex::SolRfqV2` leg in the entrypoint's
/// `SwapArgs.routes`. Returns an empty Vec when the instruction is not a known
/// swap entrypoint or carries no SolRfqV2 routes. Legs whose `taker_side`
/// byte is neither 0 nor 1 are silently skipped (they would revert on chain).
pub(crate) fn decode_solrfqv2_legs(data: &[u8]) -> Vec<DecodedFill> {
    let Some(args) = decode_swap_args(data) else {
        return Vec::new();
    };
    args.routes()
        .iter()
        .filter_map(|r| match &r.dex {
            Dex::SolRfqV2 {
                taker_side,
                rfq_id,
                expire_at,
                levels,
            } => Some(DecodedFill {
                taker_side: Side::from_u8(*taker_side)?,
                rfq_id: *rfq_id,
                expire_at: *expire_at,
                levels: levels.clone(),
            }),
            Dex::Other(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::idl_types::{encode_sol_rfq_v2_variant, entrypoint};
    use crate::types::Level;

    fn encode_other_variant(tag: u8, body_len: usize) -> Vec<u8> {
        let mut bytes = vec![tag];
        bytes.extend_from_slice(&vec![0u8; body_len]);
        bytes
    }

    fn encode_route(dex_bytes: &[u8], weight: u16, index: u8) -> Vec<u8> {
        let mut bytes = dex_bytes.to_vec();
        bytes.extend_from_slice(&weight.to_le_bytes());
        bytes.push(index);
        bytes
    }

    fn encode_swap_args_ix(
        disc: [u8; 8],
        order_id: u64,
        amount_in: u64,
        routes: Vec<Vec<u8>>,
    ) -> Vec<u8> {
        let mut bytes = disc.to_vec();
        bytes.extend_from_slice(&order_id.to_le_bytes());
        bytes.extend_from_slice(&amount_in.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes()); // expect_amount_out
        bytes.extend_from_slice(&0u16.to_le_bytes()); // slippage
        bytes.extend_from_slice(&(routes.len() as u32).to_le_bytes());
        for r in routes {
            bytes.extend_from_slice(&r);
        }
        bytes
    }

    #[test]
    fn program_id_bytes_match_base58() {
        let decoded = bs58::decode(DEX_SOLANA_V3_PROGRAM_ID_BASE58)
            .into_vec()
            .expect("base58 decode of dex-solana-v3 program id");
        assert_eq!(decoded.as_slice(), DEX_SOLANA_V3_PROGRAM_ID_BYTES);
    }

    #[test]
    fn single_solrfqv2_leg_bid() {
        let level = Level {
            base_atoms: 100_000_000_000,
            quote_atoms: 8_510_000_000,
        };
        let dex = encode_sol_rfq_v2_variant(0, 42, 2_000_000_000, &[level]);
        let route = encode_route(&dex, 10_000, 0x01);
        let ix_data = encode_swap_args_ix(entrypoint::SWAP, 7, 8_510_000_000, vec![route]);

        let fills = decode_solrfqv2_legs(&ix_data);
        assert_eq!(fills.len(), 1);
        let f = &fills[0];
        assert_eq!(f.taker_side, Side::Bid);
        assert_eq!(f.rfq_id, 42);
        assert_eq!(f.expire_at, 2_000_000_000);
        assert_eq!(f.levels, vec![level]);
    }

    #[test]
    fn single_solrfqv2_leg_ask() {
        let dex = encode_sol_rfq_v2_variant(
            1,
            7,
            2_000_000_000,
            &[Level { base_atoms: 100, quote_atoms: 85 }],
        );
        let route = encode_route(&dex, 10_000, 0x01);
        let ix_data = encode_swap_args_ix(entrypoint::SWAP, 7, 100, vec![route]);
        let fills = decode_solrfqv2_legs(&ix_data);
        assert_eq!(fills[0].taker_side, Side::Ask);
    }

    #[test]
    fn invalid_taker_side_byte_skips_leg() {
        // taker_side = 2 is neither Bid (0) nor Ask (1); leg is silently dropped.
        let dex = encode_sol_rfq_v2_variant(2, 42, 2_000_000_000, &[]);
        let route = encode_route(&dex, 10_000, 0x01);
        let ix_data = encode_swap_args_ix(entrypoint::SWAP, 7, 100, vec![route]);
        assert!(decode_solrfqv2_legs(&ix_data).is_empty());
    }

    #[test]
    fn two_solrfqv2_legs_both_decoded() {
        let dex1 = encode_sol_rfq_v2_variant(0, 1, 2_000_000_000, &[]);
        let dex2 = encode_sol_rfq_v2_variant(1, 2, 2_000_000_000, &[]);
        let r1 = encode_route(&dex1, 3_000, 0x01);
        let r2 = encode_route(&dex2, 7_000, 0x02);
        let ix_data = encode_swap_args_ix(entrypoint::PROXY_SWAP, 9, 10_000, vec![r1, r2]);
        let fills = decode_solrfqv2_legs(&ix_data);
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].taker_side, Side::Bid);
        assert_eq!(fills[0].rfq_id, 1);
        assert_eq!(fills[1].taker_side, Side::Ask);
        assert_eq!(fills[1].rfq_id, 2);
    }

    #[test]
    fn mixed_with_non_rfq_dex_variant_isolates_solrfqv2() {
        let r1 = encode_route(&encode_other_variant(2, 0), 5_000, 0x01);
        let dex_v2 = encode_sol_rfq_v2_variant(
            0,
            42,
            2_000_000_000,
            &[Level { base_atoms: 100, quote_atoms: 85 }],
        );
        let r2 = encode_route(&dex_v2, 5_000, 0x02);
        let ix_data = encode_swap_args_ix(entrypoint::SWAP, 0, 10_000, vec![r1, r2]);

        let fills = decode_solrfqv2_legs(&ix_data);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].rfq_id, 42);
    }

    #[test]
    fn token_ledger_entrypoint_decoded() {
        let dex = encode_sol_rfq_v2_variant(
            0,
            42,
            2_000_000_000,
            &[Level { base_atoms: 100, quote_atoms: 85 }],
        );
        let route = encode_route(&dex, 10_000, 0x01);

        // SwapArgsTokenLedger: order_id, expect_amount_out, slippage, routes.
        let mut bytes = entrypoint::SWAP_TOB_WITH_TOKEN_LEDGER.to_vec();
        bytes.extend_from_slice(&7u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&route);

        let fills = decode_solrfqv2_legs(&bytes);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].rfq_id, 42);
    }

    #[test]
    fn unknown_entrypoint_returns_empty() {
        let ix_data = encode_swap_args_ix([0xde, 0xad, 0xbe, 0xef, 0, 0, 0, 0], 0, 0, vec![]);
        assert!(decode_solrfqv2_legs(&ix_data).is_empty());
    }
}
