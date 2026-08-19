//! Static-index validation for the maker accounts protected by SolRfqV2.
//!
//! ALT contents are intentionally not loaded. For an executable v0 message,
//! a pubkey cannot occur in both the static and dynamically loaded regions:
//! Solana rejects the duplicate with `AccountLoadedTwice`. We therefore prove
//! maker-account exclusivity from their static indices and require the RFQ
//! program id to be static as a trustworthy leg-slice anchor.

use crate::aggregator::{
    LEG_FILL_AUTHORITY, LEG_MAKER_BASE_TOKEN_ACCOUNT, LEG_MAKER_QUOTE_TOKEN_ACCOUNT,
    LEG_PROGRAM_ID, SOL_RFQ_V2_LEG_WIDTH,
};
use crate::error::{FillDecoderError, Result};
use crate::transaction::DecodedMessage;
use crate::types::FillCountError;
use crate::RFQ_V2_PROGRAM_ID_BYTES;
use std::fmt;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MakerAccounts {
    pub fill_authority: [u8; 32],
    pub maker_base_token_account: [u8; 32],
    pub maker_quote_token_account: [u8; 32],
}

impl MakerAccounts {
    fn pubkeys(&self) -> [[u8; 32]; 3] {
        [
            self.fill_authority,
            self.maker_base_token_account,
            self.maker_quote_token_account,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MakerValidationReport {
    pub instruction_index: usize,
    pub leg_offset: usize,
    pub fill_authority_index: u8,
    pub maker_base_token_account_index: u8,
    pub maker_quote_token_account_index: u8,
    pub rfq_program_index: u8,
}

impl fmt::Display for MakerValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OK maker accounts are static, exclusive, and occupy RFQ leg slots 4/5/6 \
             in instruction {} (leg offset {})",
            self.instruction_index, self.leg_offset
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MakerValidationError {
    #[error("maker account pubkeys must be distinct")]
    KeysNotDistinct,

    #[error("{role} is not present in staticAccountKeys")]
    NotStatic { role: &'static str },

    #[error("{role} appears more than once in staticAccountKeys")]
    DuplicateStaticKey { role: &'static str },

    #[error("fill_authority static index {index} is outside the signer range")]
    FillAuthorityNotSigner { index: u8 },

    #[error("{role} static index {index} is not writable")]
    TokenAccountNotWritable { role: &'static str, index: u8 },

    #[error("RFQ_V2_PROGRAM_ID is not present in staticAccountKeys")]
    RfqProgramNotStatic,

    #[error("RFQ_V2_PROGRAM_ID appears more than once in staticAccountKeys")]
    DuplicateRfqProgramStaticKey,

    #[error(transparent)]
    FillCount(#[from] FillCountError),

    #[error("{role} is referenced {count} times; expected exactly once")]
    ReferenceCount { role: &'static str, count: usize },

    #[error("RFQ_V2_PROGRAM_ID is referenced {count} times; expected exactly once")]
    RfqProgramReferenceCount { count: usize },

    #[error("the unique SolRfqV2 fill instruction does not contain the expected static RFQ leg")]
    LegNotFound,

    #[error("the fill instruction contains multiple matching RFQ leg slices")]
    MultipleLegMatches,
}

const MAKER_ROLES: [&str; 3] = [
    "fill_authority",
    "maker_base_token_account",
    "maker_quote_token_account",
];

/// Validate the maker's three protected accounts without resolving ALT data.
///
/// This proves safety under execute-or-fail semantics: if an ALT loads one of
/// these already-static pubkeys, the runtime rejects the transaction before
/// execution with `AccountLoadedTwice`.
pub fn validate_maker_accounts(
    msg: &DecodedMessage,
    maker: &MakerAccounts,
) -> core::result::Result<MakerValidationReport, MakerValidationError> {
    let maker_pubkeys = maker.pubkeys();
    if maker_pubkeys[0] == maker_pubkeys[1]
        || maker_pubkeys[0] == maker_pubkeys[2]
        || maker_pubkeys[1] == maker_pubkeys[2]
    {
        return Err(MakerValidationError::KeysNotDistinct);
    }

    let maker_indices = [
        unique_static_index(msg, &maker_pubkeys[0], MAKER_ROLES[0])?,
        unique_static_index(msg, &maker_pubkeys[1], MAKER_ROLES[1])?,
        unique_static_index(msg, &maker_pubkeys[2], MAKER_ROLES[2])?,
    ];

    if maker_indices[0] >= msg.header.num_required_signatures {
        return Err(MakerValidationError::FillAuthorityNotSigner {
            index: maker_indices[0],
        });
    }
    for (role, index) in MAKER_ROLES[1..].iter().zip(maker_indices[1..].iter()) {
        if !static_key_is_writable(msg, *index) {
            return Err(MakerValidationError::TokenAccountNotWritable {
                role,
                index: *index,
            });
        }
    }

    let rfq_program_index = unique_rfq_program_index(msg)?;
    msg.single_fill().map_err(MakerValidationError::from)?;
    let fill_ix = msg
        .instructions
        .iter()
        .find(|ix| !ix.fills.is_empty())
        .ok_or(MakerValidationError::FillCount(FillCountError::NotFound))?;

    let tracked = [
        maker_indices[0],
        maker_indices[1],
        maker_indices[2],
        rfq_program_index,
    ];
    let mut counts = [0usize; 4];
    for ix in &msg.instructions {
        for index in std::iter::once(&ix.program_id_index).chain(ix.account_indices.iter()) {
            for (position, tracked_index) in tracked.iter().enumerate() {
                if index == tracked_index {
                    counts[position] += 1;
                }
            }
        }
    }
    for i in 0..3 {
        if counts[i] != 1 {
            return Err(MakerValidationError::ReferenceCount {
                role: MAKER_ROLES[i],
                count: counts[i],
            });
        }
    }
    if counts[3] != 1 {
        return Err(MakerValidationError::RfqProgramReferenceCount { count: counts[3] });
    }

    let expected_slots = [
        (LEG_PROGRAM_ID, rfq_program_index),
        (LEG_FILL_AUTHORITY, maker_indices[0]),
        (LEG_MAKER_BASE_TOKEN_ACCOUNT, maker_indices[1]),
        (LEG_MAKER_QUOTE_TOKEN_ACCOUNT, maker_indices[2]),
    ];
    let mut matches = (0..=fill_ix
        .account_indices
        .len()
        .saturating_sub(SOL_RFQ_V2_LEG_WIDTH))
        .filter(|&offset| {
            fill_ix.account_indices.len() >= offset + SOL_RFQ_V2_LEG_WIDTH
                && expected_slots
                    .iter()
                    .all(|&(slot, index)| fill_ix.account_indices[offset + slot] == index)
        });
    let leg_offset = matches.next().ok_or(MakerValidationError::LegNotFound)?;
    if matches.next().is_some() {
        return Err(MakerValidationError::MultipleLegMatches);
    }

    Ok(MakerValidationReport {
        instruction_index: fill_ix.instruction_index,
        leg_offset,
        fill_authority_index: maker_indices[0],
        maker_base_token_account_index: maker_indices[1],
        maker_quote_token_account_index: maker_indices[2],
        rfq_program_index,
    })
}

fn unique_static_index(
    msg: &DecodedMessage,
    pubkey: &[u8; 32],
    role: &'static str,
) -> core::result::Result<u8, MakerValidationError> {
    let mut matches = msg
        .static_account_keys
        .iter()
        .enumerate()
        .filter(|(_, key)| *key == pubkey)
        .map(|(index, _)| index as u8);
    let index = matches
        .next()
        .ok_or(MakerValidationError::NotStatic { role })?;
    if matches.next().is_some() {
        return Err(MakerValidationError::DuplicateStaticKey { role });
    }
    Ok(index)
}

fn unique_rfq_program_index(
    msg: &DecodedMessage,
) -> core::result::Result<u8, MakerValidationError> {
    let mut matches = msg
        .static_account_keys
        .iter()
        .enumerate()
        .filter(|(_, key)| **key == RFQ_V2_PROGRAM_ID_BYTES)
        .map(|(index, _)| index as u8);
    let index = matches
        .next()
        .ok_or(MakerValidationError::RfqProgramNotStatic)?;
    if matches.next().is_some() {
        return Err(MakerValidationError::DuplicateRfqProgramStaticKey);
    }
    Ok(index)
}

fn static_key_is_writable(msg: &DecodedMessage, index: u8) -> bool {
    let index = index as usize;
    let required_signatures = msg.header.num_required_signatures as usize;
    let readonly_signed = msg.header.num_readonly_signed_accounts as usize;
    let readonly_unsigned = msg.header.num_readonly_unsigned_accounts as usize;
    let static_len = msg.static_account_keys.len();

    if index < required_signatures {
        index < required_signatures.saturating_sub(readonly_signed)
    } else {
        index < static_len.saturating_sub(readonly_unsigned)
    }
}

/// Decode a base58 string to a 32-byte pubkey.
pub fn parse_pubkey_base58(s: &str) -> Result<[u8; 32]> {
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|e| FillDecoderError::Other(format!("invalid base58 pubkey {s:?}: {e}")))?;
    bytes.as_slice().try_into().map_err(|_| {
        FillDecoderError::Other(format!(
            "pubkey {s:?} decoded to {} bytes, expected 32",
            bytes.len()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::DecodedInstruction;
    use crate::types::{DecodedFill, Side};
    use crate::wire::{MessageHeader, MessageVersion};

    const FILL: [u8; 32] = [1; 32];
    const BASE: [u8; 32] = [2; 32];
    const QUOTE: [u8; 32] = [3; 32];
    const AGG: [u8; 32] = [4; 32];

    fn maker() -> MakerAccounts {
        MakerAccounts {
            fill_authority: FILL,
            maker_base_token_account: BASE,
            maker_quote_token_account: QUOTE,
        }
    }

    fn fake_fill() -> DecodedFill {
        DecodedFill {
            taker_side: Side::Bid,
            rfq_id: 1,
            expire_at: 0,
            levels: vec![],
        }
    }

    fn valid_message() -> DecodedMessage {
        let mut accounts = vec![5u8; 16];
        let offset = 2;
        accounts[offset + LEG_PROGRAM_ID] = 4;
        accounts[offset + LEG_FILL_AUTHORITY] = 0;
        accounts[offset + LEG_MAKER_BASE_TOKEN_ACCOUNT] = 1;
        accounts[offset + LEG_MAKER_QUOTE_TOKEN_ACCOUNT] = 2;
        DecodedMessage {
            version: MessageVersion::V0,
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 2,
            },
            recent_blockhash: [0; 32],
            static_account_keys: vec![FILL, BASE, QUOTE, AGG, RFQ_V2_PROGRAM_ID_BYTES, [5; 32]],
            instructions: vec![DecodedInstruction {
                instruction_index: 0,
                program_id_index: 3,
                account_indices: accounts,
                data: vec![],
                fills: vec![fake_fill()],
                entrypoint: None,
            }],
            address_table_lookups: vec![],
            loaded_writable_count: 2,
            loaded_readonly_count: 2,
        }
    }

    #[test]
    fn accepts_exact_static_rfq_slots_without_alt_contents() {
        let report = validate_maker_accounts(&valid_message(), &maker()).unwrap();
        assert_eq!(report.leg_offset, 2);
        assert_eq!(report.instruction_index, 0);
    }

    #[test]
    fn rejects_duplicate_reference_in_same_instruction() {
        let mut msg = valid_message();
        msg.instructions[0].account_indices.push(1);
        assert_eq!(
            validate_maker_accounts(&msg, &maker()).unwrap_err(),
            MakerValidationError::ReferenceCount {
                role: "maker_base_token_account",
                count: 2,
            }
        );
    }

    #[test]
    fn rejects_reference_in_sibling_instruction() {
        let mut msg = valid_message();
        msg.instructions.push(DecodedInstruction {
            instruction_index: 1,
            program_id_index: 3,
            account_indices: vec![2],
            data: vec![],
            fills: vec![],
            entrypoint: None,
        });
        assert_eq!(
            validate_maker_accounts(&msg, &maker()).unwrap_err(),
            MakerValidationError::ReferenceCount {
                role: "maker_quote_token_account",
                count: 2,
            }
        );
    }

    #[test]
    fn rejects_wrong_leg_slot() {
        let mut msg = valid_message();
        msg.instructions[0].account_indices.swap(6, 7);
        assert_eq!(
            validate_maker_accounts(&msg, &maker()).unwrap_err(),
            MakerValidationError::LegNotFound
        );
    }

    #[test]
    fn rejects_dynamic_or_missing_rfq_program_anchor() {
        let mut msg = valid_message();
        msg.static_account_keys[4] = [9; 32];
        assert_eq!(
            validate_maker_accounts(&msg, &maker()).unwrap_err(),
            MakerValidationError::RfqProgramNotStatic
        );
    }

    #[test]
    fn rejects_non_signing_fill_authority() {
        let mut msg = valid_message();
        msg.header.num_required_signatures = 0;
        assert_eq!(
            validate_maker_accounts(&msg, &maker()).unwrap_err(),
            MakerValidationError::FillAuthorityNotSigner { index: 0 }
        );
    }

    #[test]
    fn base58_parser() {
        let encoded = bs58::encode(FILL).into_string();
        assert_eq!(parse_pubkey_base58(&encoded).unwrap(), FILL);
        assert!(parse_pubkey_base58("not-valid-base58!").is_err());
    }
}
