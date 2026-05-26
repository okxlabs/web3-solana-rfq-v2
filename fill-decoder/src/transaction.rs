//! Decoded transaction / message representation. Wraps `ParsedMessage` with
//! ALT-resolved accounts and per-instruction decoded SolRfqV2 legs.

use crate::aggregator::{
    find_sol_rfq_v2_leg_offset, SwapLegLookupError, LEG_BASE_MINT, LEG_DESTINATION_TOKEN_ACCOUNT,
    LEG_FILL_AUTHORITY, LEG_MAKER_BASE_TOKEN_ACCOUNT, LEG_MAKER_QUOTE_TOKEN_ACCOUNT, LEG_PROGRAM_ID,
    LEG_QUOTE_MINT, LEG_SOURCE_TOKEN_ACCOUNT, LEG_SWAP_AUTHORITY,
};
use crate::idl_types::EntrypointKind;
use crate::types::{DecodedFill, FillCountError};
use crate::wire::{
    AddressTableLookup, CompiledInstruction, MessageHeader, MessageVersion, ParsedMessage,
    ParsedTransaction,
};

const PUBKEY_LEN: usize = 32;

/// One account in an instruction's account list, resolved against ALT state
/// when possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAccount {
    /// Resolved pubkey, or `[0u8; 32]` placeholder when `is_resolved == false`.
    pub pubkey: [u8; PUBKEY_LEN],
    /// `true` if the index resolved through static keys or supplied ALT state.
    pub is_resolved: bool,
}

impl ResolvedAccount {
    fn unresolved() -> Self {
        Self {
            pubkey: [0u8; PUBKEY_LEN],
            is_resolved: false,
        }
    }

    fn resolved(pubkey: [u8; PUBKEY_LEN]) -> Self {
        Self {
            pubkey,
            is_resolved: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DecodedInstruction {
    pub instruction_index: usize,
    pub program_id: ResolvedAccount,
    pub accounts: Vec<ResolvedAccount>,
    pub data: Vec<u8>,
    /// SolRfqV2 legs discovered inside this instruction. Empty for non-aggregator
    /// instructions, or aggregator swap instructions that carry no RFQ leg.
    pub fills: Vec<DecodedFill>,
    /// Which dex-solana-v3 swap entrypoint this instruction used, when the
    /// program is a recognised aggregator deployment and the discriminator matched.
    /// `None` for non-aggregator instructions or unrecognised entrypoints.
    pub entrypoint: Option<EntrypointKind>,
}

#[derive(Debug, Clone)]
pub struct DecodedMessage {
    pub version: MessageVersion,
    pub header: MessageHeader,
    pub recent_blockhash: [u8; 32],
    /// Resolved account keys in canonical order:
    /// `static_keys || writable_alt_entries || readonly_alt_entries`. Entries
    /// pointing at unsupplied ALTs are represented by placeholder pubkeys with
    /// `is_resolved = false`.
    pub account_keys: Vec<ResolvedAccount>,
    pub instructions: Vec<DecodedInstruction>,
    pub address_table_lookups: Vec<AddressTableLookup>,
    /// Number of account keys that could not be resolved (ALT supplied as
    /// `None` or partial). Callers should inspect this before treating the
    /// decoded account view as authoritative.
    pub unresolved_count: usize,
}

#[derive(Debug, Clone)]
pub struct DecodedTransaction {
    pub signatures: Vec<[u8; 64]>,
    pub message: DecodedMessage,
}

impl DecodedMessage {
    /// Iterator over every SolRfqV2 leg in the message, in instruction order.
    pub fn fills(&self) -> impl Iterator<Item = &DecodedFill> + '_ {
        self.instructions.iter().flat_map(|ix| ix.fills.iter())
    }

    /// Total number of SolRfqV2 legs across all top-level instructions.
    pub fn fill_count(&self) -> usize {
        self.instructions.iter().map(|ix| ix.fills.len()).sum()
    }

    /// `true` if any SolRfqV2 leg in this message rides inside a token-ledger
    /// entrypoint (`SWAP_TOB_WITH_TOKEN_LEDGER`,
    /// `SWAP_TOB_WITH_RECEIVER_TOKEN_LEDGER`).
    ///
    /// Token-ledger entrypoints derive `amount_in` from an on-chain account
    /// populated by an earlier instruction in the same transaction. That makes
    /// the consumed amount opaque from args alone and lets the taker compose
    /// the RFQ fill atomically with other swaps for arbitrage. The conservative
    /// maker policy is to refuse-to-sign when this returns `true`.
    pub fn has_token_ledger_fill(&self) -> bool {
        self.instructions
            .iter()
            .any(|ix| !ix.fills.is_empty() && ix.entrypoint == Some(EntrypointKind::TokenLedger))
    }

    /// Returns the single SolRfqV2 leg if exactly one is present in the
    /// transaction. Maker signing flow assumes exactly one fill per tx; any
    /// other count is a refuse-to-sign signal.
    pub fn single_fill(&self) -> Result<&DecodedFill, FillCountError> {
        let mut iter = self.fills();
        let first = iter.next().ok_or(FillCountError::NotFound)?;
        let extra = iter.count();
        if extra == 0 {
            Ok(first)
        } else {
            Err(FillCountError::Multiple(extra + 1))
        }
    }

    /// Read every relevant pubkey from the SolRfqV2 leg's 13-account slice
    /// inside the aggregator instruction. The slice's start is located by
    /// scanning for `RFQ_V2_PROGRAM_ID_BYTES` (slot 0 of every leg), making
    /// this robust to varying outer entrypoint shapes and to multi-route
    /// configurations where SolRfqV2 is not at position 0.
    pub fn swap_leg_accounts(&self) -> Result<SwapLegAccounts, SwapLegError> {
        let ix = self
            .instructions
            .iter()
            .find(|ix| !ix.fills.is_empty())
            .ok_or(SwapLegError::FillCount(FillCountError::NotFound))?;
        let offset =
            find_sol_rfq_v2_leg_offset(ix).map_err(SwapLegError::LookupFailed)?;

        // All 9 named positions must be resolved (positions 0..=8 of the slice).
        let read = |slot: usize| -> Result<[u8; 32], SwapLegError> {
            let a = &ix.accounts[offset + slot];
            if !a.is_resolved {
                return Err(SwapLegError::UnresolvedSlot {
                    leg_position: slot,
                    outer_index: offset + slot,
                });
            }
            Ok(a.pubkey)
        };

        Ok(SwapLegAccounts {
            program_id: read(LEG_PROGRAM_ID)?,
            swap_authority: read(LEG_SWAP_AUTHORITY)?,
            swap_source_token_account: read(LEG_SOURCE_TOKEN_ACCOUNT)?,
            swap_destination_token_account: read(LEG_DESTINATION_TOKEN_ACCOUNT)?,
            fill_authority: read(LEG_FILL_AUTHORITY)?,
            maker_base_token_account: read(LEG_MAKER_BASE_TOKEN_ACCOUNT)?,
            maker_quote_token_account: read(LEG_MAKER_QUOTE_TOKEN_ACCOUNT)?,
            base_mint: read(LEG_BASE_MINT)?,
            quote_mint: read(LEG_QUOTE_MINT)?,
        })
    }

}

/// All meaningful pubkeys read from the SolRfqV2 leg slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapLegAccounts {
    /// Slot 0: must equal `RFQ_V2_PROGRAM_ID_BYTES`. Carried for symmetry.
    pub program_id: [u8; 32],
    /// Slot 1: taker's signing authority for the swap.
    pub swap_authority: [u8; 32],
    /// Slot 2: taker's source token account for this leg.
    pub swap_source_token_account: [u8; 32],
    /// Slot 3: taker's destination token account for this leg.
    pub swap_destination_token_account: [u8; 32],
    /// Slot 4: maker's signing authority.
    pub fill_authority: [u8; 32],
    /// Slot 5: maker's base-side token account.
    pub maker_base_token_account: [u8; 32],
    /// Slot 6: maker's quote-side token account.
    pub maker_quote_token_account: [u8; 32],
    /// Slot 7.
    pub base_mint: [u8; 32],
    /// Slot 8.
    pub quote_mint: [u8; 32],
}

impl SwapLegAccounts {
    /// Verify the maker's known `(base_mint, quote_mint)` matches the on-wire
    /// pair. Use this in conjunction with reading `fill.taker_side` from
    /// [`crate::DecodedFill`] — the side itself is now signed in the variant
    /// body, so no offline derivation is needed.
    pub fn verify_mint_pair(
        &self,
        base_mint: &[u8; 32],
        quote_mint: &[u8; 32],
    ) -> Result<(), MintPairMismatch> {
        if &self.base_mint == base_mint && &self.quote_mint == quote_mint {
            Ok(())
        } else {
            Err(MintPairMismatch {
                on_wire_base: self.base_mint,
                on_wire_quote: self.quote_mint,
                expected_base: *base_mint,
                expected_quote: *quote_mint,
            })
        }
    }
}

/// Mismatch between the leg's on-wire `(base_mint, quote_mint)` pair and the
/// maker's expected pair (looked up by `rfq_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MintPairMismatch {
    pub on_wire_base: [u8; 32],
    pub on_wire_quote: [u8; 32],
    pub expected_base: [u8; 32],
    pub expected_quote: [u8; 32],
}

impl core::fmt::Display for MintPairMismatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "leg's (base, quote) does not match maker's expected pair"
        )
    }
}

impl std::error::Error for MintPairMismatch {}

/// Why [`DecodedMessage::swap_leg_accounts`] failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapLegError {
    /// No SolRfqV2-bearing instruction in the message.
    FillCount(FillCountError),
    /// The leg's slot 0 (program id) wasn't located. See [`SwapLegLookupError`].
    LookupFailed(SwapLegLookupError),
    /// A required slot inside the located leg slice resolved to an unsupplied
    /// ALT entry. Supply ALT state and retry.
    UnresolvedSlot {
        leg_position: usize,
        outer_index: usize,
    },
}

impl core::fmt::Display for SwapLegError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SwapLegError::FillCount(e) => write!(f, "{e}"),
            SwapLegError::LookupFailed(e) => write!(f, "{e}"),
            SwapLegError::UnresolvedSlot {
                leg_position,
                outer_index,
            } => write!(
                f,
                "SolRfqV2 leg slot {leg_position} (outer index {outer_index}) is unresolved; \
                 supply ALT state"
            ),
        }
    }
}

impl std::error::Error for SwapLegError {}


impl DecodedTransaction {
    /// See [`DecodedMessage::fills`].
    pub fn fills(&self) -> impl Iterator<Item = &DecodedFill> + '_ {
        self.message.fills()
    }

    /// See [`DecodedMessage::fill_count`].
    pub fn fill_count(&self) -> usize {
        self.message.fill_count()
    }

    /// See [`DecodedMessage::has_token_ledger_fill`].
    pub fn has_token_ledger_fill(&self) -> bool {
        self.message.has_token_ledger_fill()
    }

    /// See [`DecodedMessage::single_fill`].
    pub fn single_fill(&self) -> Result<&DecodedFill, FillCountError> {
        self.message.single_fill()
    }

    /// See [`DecodedMessage::swap_leg_accounts`].
    pub fn swap_leg_accounts(&self) -> Result<SwapLegAccounts, SwapLegError> {
        self.message.swap_leg_accounts()
    }
}

/// One on-chain Address Lookup Table's pubkey + its full address list.
/// Callers fetch this from RPC and supply it here. The decoder treats the
/// contents as trusted input.
#[derive(Debug, Clone)]
pub struct AddressLookupTableEntry {
    pub table_key: [u8; PUBKEY_LEN],
    pub addresses: Vec<[u8; PUBKEY_LEN]>,
}

pub(crate) fn resolve(
    parsed: ParsedMessage,
    alt_state: &[AddressLookupTableEntry],
) -> DecodedMessage {
    let mut account_keys: Vec<ResolvedAccount> = parsed
        .static_account_keys
        .iter()
        .map(|k| ResolvedAccount::resolved(*k))
        .collect();

    let mut unresolved = 0usize;

    let alt_addresses = |table_key: &[u8; 32]| -> Option<&[[u8; 32]]> {
        alt_state
            .iter()
            .find(|e| &e.table_key == table_key)
            .map(|e| e.addresses.as_slice())
    };

    let mut push_alt = |lookups: &[AddressTableLookup], pick: fn(&AddressTableLookup) -> &[u8]| {
        for lookup in lookups {
            let addrs = alt_addresses(&lookup.table_key);
            for &idx in pick(lookup) {
                let resolved = addrs.and_then(|a| a.get(idx as usize).copied());
                account_keys.push(match resolved {
                    Some(pk) => ResolvedAccount::resolved(pk),
                    None => {
                        unresolved += 1;
                        ResolvedAccount::unresolved()
                    }
                });
            }
        }
    };
    push_alt(&parsed.address_table_lookups, |l| &l.writable_indexes);
    push_alt(&parsed.address_table_lookups, |l| &l.readonly_indexes);

    let instructions = parsed
        .instructions
        .into_iter()
        .enumerate()
        .map(|(i, ix)| build_instruction(i, ix, &account_keys))
        .collect();

    DecodedMessage {
        version: parsed.version,
        header: parsed.header,
        recent_blockhash: parsed.recent_blockhash,
        account_keys,
        instructions,
        address_table_lookups: parsed.address_table_lookups,
        unresolved_count: unresolved,
    }
}

fn build_instruction(
    index: usize,
    ix: CompiledInstruction,
    account_keys: &[ResolvedAccount],
) -> DecodedInstruction {
    let lookup = |idx: u8| -> ResolvedAccount {
        account_keys
            .get(idx as usize)
            .cloned()
            .unwrap_or_else(ResolvedAccount::unresolved)
    };

    let program_id = lookup(ix.program_id_index);
    let accounts = ix.account_indices.iter().map(|&i| lookup(i)).collect();

    DecodedInstruction {
        instruction_index: index,
        program_id,
        accounts,
        data: ix.data,
        fills: Vec::new(),
        entrypoint: None,
    }
}

pub(crate) fn from_parsed_tx(
    parsed: ParsedTransaction,
    alt_state: &[AddressLookupTableEntry],
) -> DecodedTransaction {
    DecodedTransaction {
        signatures: parsed.signatures,
        message: resolve(parsed.message, alt_state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::MessageHeader;

    fn fill(rfq_id: u64) -> DecodedFill {
        DecodedFill {
            taker_side: crate::Side::Bid,
            rfq_id,
            expire_at: 0,
            levels: vec![],
        }
    }

    fn ix_with(fills: Vec<DecodedFill>) -> DecodedInstruction {
        ix_with_ep(fills, None)
    }

    fn ix_with_ep(
        fills: Vec<DecodedFill>,
        entrypoint: Option<EntrypointKind>,
    ) -> DecodedInstruction {
        DecodedInstruction {
            instruction_index: 0,
            program_id: ResolvedAccount {
                pubkey: [0u8; 32],
                is_resolved: true,
            },
            accounts: vec![],
            data: vec![],
            fills,
            entrypoint,
        }
    }

    fn msg_with(instructions: Vec<DecodedInstruction>) -> DecodedMessage {
        DecodedMessage {
            version: MessageVersion::Legacy,
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            recent_blockhash: [0u8; 32],
            account_keys: vec![],
            instructions,
            address_table_lookups: vec![],
            unresolved_count: 0,
        }
    }

    #[test]
    fn single_fill_returns_the_one_fill() {
        let m = msg_with(vec![ix_with(vec![fill(42)])]);
        let f = m.single_fill().expect("one fill");
        assert_eq!(f.rfq_id, 42);
        assert_eq!(m.fill_count(), 1);
    }

    #[test]
    fn token_ledger_fill_is_flagged() {
        let m = msg_with(vec![ix_with_ep(vec![fill(1)], Some(EntrypointKind::TokenLedger))]);
        assert!(m.has_token_ledger_fill());
    }

    #[test]
    fn concrete_entrypoint_does_not_flag() {
        let m = msg_with(vec![ix_with_ep(vec![fill(1)], Some(EntrypointKind::Concrete))]);
        assert!(!m.has_token_ledger_fill());
    }

    #[test]
    fn token_ledger_ix_without_fills_does_not_flag() {
        // A token-ledger ix that carries no SolRfqV2 leg doesn't put the maker
        // at risk; the gate is specifically about RFQ legs being consumed.
        let m = msg_with(vec![ix_with_ep(vec![], Some(EntrypointKind::TokenLedger))]);
        assert!(!m.has_token_ledger_fill());
    }

    #[test]
    fn single_fill_rejects_zero_fills() {
        let m = msg_with(vec![ix_with(vec![])]);
        assert_eq!(m.single_fill().unwrap_err(), FillCountError::NotFound);
        assert_eq!(m.fill_count(), 0);
    }

    #[test]
    fn single_fill_rejects_two_in_one_ix() {
        let m = msg_with(vec![ix_with(vec![fill(1), fill(2)])]);
        assert_eq!(m.single_fill().unwrap_err(), FillCountError::Multiple(2));
        assert_eq!(m.fill_count(), 2);
    }

    #[test]
    fn single_fill_rejects_one_per_ix_across_two_ixs() {
        let m = msg_with(vec![ix_with(vec![fill(1)]), ix_with(vec![fill(2)])]);
        assert_eq!(m.single_fill().unwrap_err(), FillCountError::Multiple(2));
    }

    #[test]
    fn fills_iterator_preserves_order() {
        let m = msg_with(vec![
            ix_with(vec![fill(1), fill(2)]),
            ix_with(vec![fill(3)]),
        ]);
        let ids: Vec<u64> = m.fills().map(|f| f.rfq_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn fill_count_error_display() {
        assert_eq!(
            FillCountError::NotFound.to_string(),
            "no SolRfqV2 leg found in transaction"
        );
        assert_eq!(
            FillCountError::Multiple(3).to_string(),
            "3 SolRfqV2 legs found in transaction; expected exactly 1"
        );
    }

    fn resolved(byte: u8) -> ResolvedAccount {
        ResolvedAccount {
            pubkey: [byte; 32],
            is_resolved: true,
        }
    }

    fn unresolved() -> ResolvedAccount {
        ResolvedAccount {
            pubkey: [0u8; 32],
            is_resolved: false,
        }
    }

    fn swap_ix(accounts: Vec<ResolvedAccount>, fills: Vec<DecodedFill>) -> DecodedInstruction {
        DecodedInstruction {
            instruction_index: 0,
            program_id: resolved(0xff),
            accounts,
            data: vec![],
            fills,
            entrypoint: None,
        }
    }

    fn pk_resolved(pubkey: [u8; 32]) -> ResolvedAccount {
        ResolvedAccount {
            pubkey,
            is_resolved: true,
        }
    }

    /// Builds an outer ix where the SolRfqV2 leg slice begins at `leg_offset`.
    /// `outer_src_token` lets the test control whether the outer named
    /// source_token_account matches the leg's swap_source_token_account.
    fn build_swap_ix(
        leg_offset: usize,
        outer_src_token: [u8; 32],
        outer_src_mint: [u8; 32],
        leg_src_token: [u8; 32],
        leg_dst_token: [u8; 32],
        base_mint: [u8; 32],
        quote_mint: [u8; 32],
    ) -> DecodedInstruction {
        // Outer named accounts: pad with arbitrary pubkeys, but position 1 =
        // source_token_account and position 3 = source_mint must be specific.
        let mut accs: Vec<ResolvedAccount> = (0..leg_offset)
            .map(|i| match i {
                1 => pk_resolved(outer_src_token),
                3 => pk_resolved(outer_src_mint),
                _ => resolved((0x80 + i) as u8),
            })
            .collect();
        // Leg slice (13 accounts).
        let leg = [
            crate::aggregator::RFQ_V2_PROGRAM_ID_BYTES, // [0] program_id
            [0xa1; 32],                                  // [1] swap_authority
            leg_src_token,                               // [2]
            leg_dst_token,                               // [3]
            [0xa4; 32],                                  // [4] fill_authority
            [0xa5; 32],                                  // [5] maker_base_token_account
            [0xa6; 32],                                  // [6] maker_quote_token_account
            base_mint,                                   // [7]
            quote_mint,                                  // [8]
            [0xa9; 32], [0xaa; 32], [0xab; 32], [0xac; 32], // [9..=12]
        ];
        for pk in &leg {
            accs.push(pk_resolved(*pk));
        }
        swap_ix(accs, vec![fill(42)])
    }

    #[test]
    fn swap_leg_accounts_reads_slice() {
        let base = [0xb0; 32];
        let quote = [0xc0; 32];
        let src = [0xd0; 32];
        let dst = [0xd1; 32];
        let m = msg_with(vec![build_swap_ix(7, src, quote, src, dst, base, quote)]);
        let leg = m.swap_leg_accounts().expect("ok");
        assert_eq!(leg.swap_source_token_account, src);
        assert_eq!(leg.swap_destination_token_account, dst);
        assert_eq!(leg.base_mint, base);
        assert_eq!(leg.quote_mint, quote);
        assert_eq!(leg.fill_authority, [0xa4; 32]);
        assert_eq!(leg.maker_base_token_account, [0xa5; 32]);
        assert_eq!(leg.maker_quote_token_account, [0xa6; 32]);
    }

    #[test]
    fn swap_leg_accounts_errors_when_no_fill() {
        let m = msg_with(vec![swap_ix(vec![resolved(0x01); 13], vec![])]);
        assert_eq!(
            m.swap_leg_accounts(),
            Err(SwapLegError::FillCount(FillCountError::NotFound))
        );
    }

    #[test]
    fn swap_leg_accounts_errors_when_program_id_absent() {
        // Fill is present but no slot contains RFQ_V2_PROGRAM_ID_BYTES.
        let accs = vec![resolved(0x01); 13];
        let m = msg_with(vec![swap_ix(accs, vec![fill(1)])]);
        assert_eq!(
            m.swap_leg_accounts(),
            Err(SwapLegError::LookupFailed(
                SwapLegLookupError::ProgramIdNotFound
            ))
        );
    }

    #[test]
    fn verify_mint_pair_matches() {
        let base = [0xb0; 32];
        let quote = [0xc0; 32];
        let m = msg_with(vec![build_swap_ix(7, [0xd0; 32], quote, [0xd0; 32], [0xd1; 32], base, quote)]);
        let leg = m.swap_leg_accounts().unwrap();
        assert!(leg.verify_mint_pair(&base, &quote).is_ok());
    }

    #[test]
    fn verify_mint_pair_rejects_mismatched_pair() {
        let leg_base = [0x11; 32];
        let leg_quote = [0x22; 32];
        let m = msg_with(vec![build_swap_ix(
            7, [0xd0; 32], leg_quote, [0xd0; 32], [0xd1; 32], leg_base, leg_quote,
        )]);
        let leg = m.swap_leg_accounts().unwrap();
        let expected_base = [0xb0; 32];
        let expected_quote = [0xc0; 32];
        let err = leg.verify_mint_pair(&expected_base, &expected_quote).unwrap_err();
        assert_eq!(err.on_wire_base, leg_base);
        assert_eq!(err.expected_base, expected_base);
    }
}
