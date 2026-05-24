//! Cross-instruction exclusivity check for the maker's sensitive accounts.
//!
//! A maker about to sign an OKX-aggregator transaction wants to confirm that
//! its sensitive pubkeys — `fill_authority`, `maker_base_token_account`,
//! `maker_quote_token_account` — appear **exactly once** in the whole
//! transaction, and that the single occurrence is inside a `dex-solana-v3`
//! swap instruction that actually carries a SolRfqV2 leg.
//!
//! The single result type is [`ExclusivityReport`] (flat struct, like the
//! reference SDK at `rfq-v2-sdk/fill-decoder`). It carries the queried
//! pubkey, the resolved-reference count, and the per-instruction index lists
//! so callers can both ask "is it safe?" and inspect the details.
//!
//! ## Failure modes (collapsed into `is_exclusive() == false`)
//!
//! - The pubkey is absent (`confirmed_count == 0`).
//! - The pubkey is referenced in a sibling, non-fill-bearing instruction.
//! - The pubkey is referenced more than once across the message (duplicate
//!   use the maker did not pre-sign for).
//! - A non-fill instruction has unresolved ALT entries that could be the
//!   queried pubkey — fail closed.
//!
//! ## Scope
//!
//! - Walks **top-level** instructions only. Inner CPIs are out of scope.
//! - Pubkey-keyed: the caller supplies their own `fill_authority` /
//!   `maker_*_token_account` pubkeys.
//!
//! ## Not covered
//!
//! - Taker token accounts. Those sit at adapter-specific offsets inside the
//!   aggregator's `remaining_accounts`; recovering them would require
//!   modelling every dex-solana-v3 adapter's account width. Out of scope.
//! - Intra-fill misuse via routes the maker did not pre-sign. The maker
//!   must additionally verify every decoded `rfq_id` against its quote
//!   registry.

use crate::error::{FillDecoderError, Result};
use crate::transaction::{DecodedMessage, ResolvedAccount};
use std::fmt;

/// Outcome of checking one pubkey against a decoded message.
///
/// Flat struct rather than an enum so callers can read individual fields
/// without pattern matching, and so [`fmt::Display`] can produce a complete
/// human-readable line on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusivityReport {
    /// The pubkey that was checked. Use [`ExclusivityReport::pubkey_base58`]
    /// to format it for display.
    pub pubkey: [u8; 32],
    /// Number of resolved references found across the whole message
    /// (program_id position counts, each account_meta entry counts once).
    pub confirmed_count: usize,
    /// Indices of top-level instructions that reference the pubkey AND
    /// contain at least one decoded SolRfqV2 leg.
    pub fill_ix_indices: Vec<usize>,
    /// Indices of top-level instructions that reference the pubkey but
    /// contain no decoded SolRfqV2 legs.
    pub non_fill_ix_indices: Vec<usize>,
    /// Indices of top-level instructions that have at least one unresolved
    /// ALT entry (could be this pubkey — fail closed).
    pub ix_with_unresolved: Vec<usize>,
}

impl ExclusivityReport {
    /// `true` iff the pubkey is referenced exactly once, in a fill-bearing
    /// instruction, with no ambiguity from unresolved ALT entries.
    pub fn is_exclusive(&self) -> bool {
        self.confirmed_count == 1
            && self.non_fill_ix_indices.is_empty()
            && self.ix_with_unresolved.is_empty()
    }

    /// Base58 form of the queried pubkey.
    pub fn pubkey_base58(&self) -> String {
        bs58::encode(&self.pubkey).into_string()
    }
}

impl fmt::Display for ExclusivityReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pk = self.pubkey_base58();
        if self.is_exclusive() {
            return write!(
                f,
                "OK {}: exactly one reference, in fill ix {}",
                pk, self.fill_ix_indices[0]
            );
        }
        let mut reasons: Vec<String> = Vec::new();
        if self.confirmed_count == 0 && self.ix_with_unresolved.is_empty() {
            reasons.push("absent (not referenced anywhere)".to_string());
        }
        if !self.non_fill_ix_indices.is_empty() {
            reasons.push(format!(
                "referenced by non-fill ixs {:?}",
                self.non_fill_ix_indices
            ));
        }
        if self.confirmed_count > 1 {
            reasons.push(format!(
                "duplicate references ({} total)",
                self.confirmed_count
            ));
        }
        if !self.ix_with_unresolved.is_empty() {
            reasons.push(format!(
                "unresolved ALT in ixs {:?}",
                self.ix_with_unresolved
            ));
        }
        write!(
            f,
            "UNSAFE {}: {} (fill ixs: {:?})",
            pk,
            reasons.join("; "),
            self.fill_ix_indices,
        )
    }
}

/// Check `pubkey` against `msg`.
pub fn check_pubkey_exclusivity(msg: &DecodedMessage, pubkey: &[u8; 32]) -> ExclusivityReport {
    let mut confirmed_count: usize = 0;
    let mut fill_ix_indices = Vec::new();
    let mut non_fill_ix_indices = Vec::new();
    let mut ix_with_unresolved = Vec::new();

    for ix in &msg.instructions {
        let refs = count_pubkey_refs(&ix.program_id, &ix.accounts, pubkey);
        if refs > 0 {
            confirmed_count += refs;
            if ix.fills.is_empty() {
                non_fill_ix_indices.push(ix.instruction_index);
            } else {
                fill_ix_indices.push(ix.instruction_index);
            }
        }
        if ix_has_unresolved(&ix.program_id, &ix.accounts) {
            ix_with_unresolved.push(ix.instruction_index);
        }
    }

    ExclusivityReport {
        pubkey: *pubkey,
        confirmed_count,
        fill_ix_indices,
        non_fill_ix_indices,
        ix_with_unresolved,
    }
}

/// Convenience: parse `pubkey_base58` and run [`check_pubkey_exclusivity`].
pub fn check_pubkey_exclusivity_base58(
    msg: &DecodedMessage,
    pubkey_base58: &str,
) -> Result<ExclusivityReport> {
    Ok(check_pubkey_exclusivity(msg, &parse_pubkey_base58(pubkey_base58)?))
}

/// Returns `true` iff every pubkey passes [`ExclusivityReport::is_exclusive`].
pub fn all_pubkeys_exclusive(msg: &DecodedMessage, pubkeys: &[[u8; 32]]) -> bool {
    pubkeys
        .iter()
        .all(|pk| check_pubkey_exclusivity(msg, pk).is_exclusive())
}

/// Base58 convenience for [`all_pubkeys_exclusive`].
pub fn all_pubkeys_exclusive_base58(
    msg: &DecodedMessage,
    pubkeys_base58: &[&str],
) -> Result<bool> {
    let parsed: Vec<[u8; 32]> = pubkeys_base58
        .iter()
        .map(|s| parse_pubkey_base58(s))
        .collect::<Result<_>>()?;
    Ok(all_pubkeys_exclusive(msg, &parsed))
}

/// Decode a base58 string to a 32-byte pubkey.
pub fn parse_pubkey_base58(s: &str) -> Result<[u8; 32]> {
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|e| FillDecoderError::Other(format!("invalid base58 pubkey {s:?}: {e}")))?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| {
            FillDecoderError::Other(format!(
                "pubkey {s:?} decoded to {} bytes, expected 32",
                bytes.len()
            ))
        })
}

fn count_pubkey_refs(
    program_id: &ResolvedAccount,
    accounts: &[ResolvedAccount],
    target: &[u8; 32],
) -> usize {
    let prog_hit = (program_id.is_resolved && &program_id.pubkey == target) as usize;
    let acc_hits = accounts
        .iter()
        .filter(|a| a.is_resolved && &a.pubkey == target)
        .count();
    prog_hit + acc_hits
}

fn ix_has_unresolved(program_id: &ResolvedAccount, accounts: &[ResolvedAccount]) -> bool {
    !program_id.is_resolved || accounts.iter().any(|a| !a.is_resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::{DecodedInstruction, DecodedMessage};
    use crate::types::DecodedFill;
    use crate::wire::{MessageHeader, MessageVersion};

    fn pk(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn resolved(byte: u8) -> ResolvedAccount {
        ResolvedAccount {
            pubkey: pk(byte),
            is_resolved: true,
        }
    }

    fn unresolved() -> ResolvedAccount {
        ResolvedAccount {
            pubkey: [0u8; 32],
            is_resolved: false,
        }
    }

    fn fake_fill() -> DecodedFill {
        DecodedFill {
            taker_side: crate::Side::Bid,
            rfq_id: 1,
            expire_at: 0,
            levels: vec![],
        }
    }

    fn ix(
        index: usize,
        program: ResolvedAccount,
        accs: Vec<ResolvedAccount>,
        with_fill: bool,
    ) -> DecodedInstruction {
        DecodedInstruction {
            instruction_index: index,
            program_id: program,
            accounts: accs,
            data: vec![],
            fills: if with_fill { vec![fake_fill()] } else { vec![] },
        }
    }

    fn msg(ixs: Vec<DecodedInstruction>) -> DecodedMessage {
        DecodedMessage {
            version: MessageVersion::Legacy,
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            recent_blockhash: [0u8; 32],
            account_keys: vec![],
            instructions: ixs,
            address_table_lookups: vec![],
            unresolved_count: 0,
        }
    }

    #[test]
    fn exactly_one_hit_in_fill_ix_is_exclusive() {
        let target = pk(7);
        let m = msg(vec![
            ix(0, resolved(1), vec![resolved(7)], true),
            ix(1, resolved(2), vec![resolved(8)], false),
        ]);
        let r = check_pubkey_exclusivity(&m, &target);
        assert!(r.is_exclusive());
        assert_eq!(r.confirmed_count, 1);
        assert_eq!(r.fill_ix_indices, vec![0]);
        assert!(r.non_fill_ix_indices.is_empty());
        assert!(r.ix_with_unresolved.is_empty());
    }

    #[test]
    fn absent_pubkey_is_unsafe() {
        let target = pk(99);
        let m = msg(vec![ix(0, resolved(1), vec![resolved(7)], true)]);
        let r = check_pubkey_exclusivity(&m, &target);
        assert!(!r.is_exclusive());
        assert_eq!(r.confirmed_count, 0);
        assert!(r.fill_ix_indices.is_empty());
    }

    #[test]
    fn hit_in_non_fill_ix_is_unsafe() {
        let target = pk(7);
        let m = msg(vec![
            ix(0, resolved(1), vec![resolved(7)], true),
            ix(1, resolved(2), vec![resolved(7)], false),
        ]);
        let r = check_pubkey_exclusivity(&m, &target);
        assert!(!r.is_exclusive());
        assert_eq!(r.confirmed_count, 2);
        assert_eq!(r.fill_ix_indices, vec![0]);
        assert_eq!(r.non_fill_ix_indices, vec![1]);
    }

    #[test]
    fn duplicate_within_one_fill_ix_is_unsafe() {
        let target = pk(7);
        let m = msg(vec![ix(
            0,
            resolved(1),
            vec![resolved(7), resolved(9), resolved(7)],
            true,
        )]);
        let r = check_pubkey_exclusivity(&m, &target);
        assert!(!r.is_exclusive());
        assert_eq!(r.confirmed_count, 2);
        assert_eq!(r.fill_ix_indices, vec![0]);
    }

    #[test]
    fn duplicate_across_two_fill_ixs_is_unsafe() {
        let target = pk(7);
        let m = msg(vec![
            ix(0, resolved(1), vec![resolved(7)], true),
            ix(1, resolved(2), vec![resolved(7)], true),
        ]);
        let r = check_pubkey_exclusivity(&m, &target);
        assert!(!r.is_exclusive());
        assert_eq!(r.confirmed_count, 2);
        assert_eq!(r.fill_ix_indices, vec![0, 1]);
    }

    #[test]
    fn unresolved_alt_anywhere_is_unsafe() {
        let target = pk(7);
        let m = msg(vec![
            ix(0, resolved(1), vec![resolved(7)], true),
            ix(1, resolved(2), vec![unresolved()], false),
        ]);
        let r = check_pubkey_exclusivity(&m, &target);
        assert!(!r.is_exclusive());
        assert_eq!(r.ix_with_unresolved, vec![1]);
    }

    #[test]
    fn program_id_position_counts_as_reference() {
        let target = pk(9);
        let m = msg(vec![
            ix(0, resolved(1), vec![resolved(9)], true),
            ix(1, resolved(9), vec![], false),
        ]);
        let r = check_pubkey_exclusivity(&m, &target);
        assert!(!r.is_exclusive());
        assert_eq!(r.confirmed_count, 2);
        assert_eq!(r.non_fill_ix_indices, vec![1]);
    }

    #[test]
    fn all_pubkeys_helper() {
        let a = pk(7);
        let b = pk(8);
        let m = msg(vec![ix(
            0,
            resolved(1),
            vec![resolved(7), resolved(8)],
            true,
        )]);
        assert!(all_pubkeys_exclusive(&m, &[a, b]));

        let m2 = msg(vec![
            ix(0, resolved(1), vec![resolved(7), resolved(8)], true),
            ix(1, resolved(2), vec![resolved(8)], false),
        ]);
        assert!(!all_pubkeys_exclusive(&m2, &[a, b]));
    }

    #[test]
    fn base58_helpers_parse_then_check() {
        let m = msg(vec![ix(0, resolved(1), vec![resolved(7)], true)]);
        let target_b58 = bs58::encode(pk(7)).into_string();
        let r = check_pubkey_exclusivity_base58(&m, &target_b58).unwrap();
        assert!(r.is_exclusive());
        assert_eq!(r.pubkey, pk(7));
        assert_eq!(r.pubkey_base58(), target_b58);

        assert!(check_pubkey_exclusivity_base58(&m, "not-valid-base58!").is_err());
        assert!(check_pubkey_exclusivity_base58(&m, "1111").is_err());
    }

    #[test]
    fn display_format() {
        let m = msg(vec![ix(0, resolved(1), vec![resolved(7)], true)]);
        let ok = check_pubkey_exclusivity(&m, &pk(7));
        assert!(ok.to_string().starts_with("OK "));
        assert!(ok.to_string().contains("exactly one reference"));

        let absent = check_pubkey_exclusivity(&m, &pk(99));
        let s = absent.to_string();
        assert!(s.starts_with("UNSAFE "));
        assert!(s.contains("absent"));

        let m2 = msg(vec![
            ix(0, resolved(1), vec![resolved(7)], true),
            ix(1, resolved(2), vec![resolved(7)], false),
        ]);
        let unsafe_hit = check_pubkey_exclusivity(&m2, &pk(7));
        let s = unsafe_hit.to_string();
        assert!(s.contains("non-fill"));
        assert!(s.contains("duplicate"));
    }
}
