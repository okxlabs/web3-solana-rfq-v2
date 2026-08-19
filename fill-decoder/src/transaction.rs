//! Decoded transaction/message representation.
//!
//! The decoder deliberately does not load Address Lookup Table account data.
//! Instructions retain their original account indices; static pubkeys can be
//! read directly from the message and dynamic indices remain opaque.

use crate::error::{FillDecoderError, Result};
use crate::idl_types::EntrypointKind;
use crate::types::{DecodedFill, FillCountError};
use crate::wire::{
    AddressTableLookup, CompiledInstruction, MessageHeader, MessageVersion, ParsedMessage,
    ParsedTransaction,
};

const MAX_ACCOUNT_KEYS: usize = u8::MAX as usize + 1;

#[derive(Debug, Clone)]
pub struct DecodedInstruction {
    pub instruction_index: usize,
    /// Index into `static_account_keys || loaded_writable || loaded_readonly`.
    pub program_id_index: u8,
    /// Indices into `static_account_keys || loaded_writable || loaded_readonly`.
    pub account_indices: Vec<u8>,
    pub data: Vec<u8>,
    /// SolRfqV2 legs decoded from this instruction's args.
    pub fills: Vec<DecodedFill>,
    /// Recognised dex-solana-v3 entrypoint kind.
    pub entrypoint: Option<EntrypointKind>,
}

#[derive(Debug, Clone)]
pub struct DecodedMessage {
    pub version: MessageVersion,
    pub header: MessageHeader,
    pub recent_blockhash: [u8; 32],
    /// Pubkeys encoded directly in the message. Signers are always static.
    pub static_account_keys: Vec<[u8; 32]>,
    pub instructions: Vec<DecodedInstruction>,
    /// Lookup descriptors are retained for inspection and account-count
    /// validation; the referenced ALT account contents are never fetched.
    pub address_table_lookups: Vec<AddressTableLookup>,
    pub loaded_writable_count: usize,
    pub loaded_readonly_count: usize,
}

#[derive(Debug, Clone)]
pub struct DecodedTransaction {
    pub signatures: Vec<[u8; 64]>,
    pub message: DecodedMessage,
}

impl DecodedMessage {
    /// Returns a pubkey only when `index` addresses the static key region.
    pub fn static_account_key(&self, index: u8) -> Option<&[u8; 32]> {
        self.static_account_keys.get(index as usize)
    }

    pub fn total_account_count(&self) -> usize {
        self.static_account_keys.len() + self.loaded_writable_count + self.loaded_readonly_count
    }

    /// Iterator over every SolRfqV2 leg in instruction order.
    pub fn fills(&self) -> impl Iterator<Item = &DecodedFill> + '_ {
        self.instructions.iter().flat_map(|ix| ix.fills.iter())
    }

    pub fn fill_count(&self) -> usize {
        self.instructions.iter().map(|ix| ix.fills.len()).sum()
    }

    pub fn has_token_ledger_fill(&self) -> bool {
        self.instructions
            .iter()
            .any(|ix| !ix.fills.is_empty() && ix.entrypoint == Some(EntrypointKind::TokenLedger))
    }

    pub fn single_fill(&self) -> core::result::Result<&DecodedFill, FillCountError> {
        let mut iter = self.fills();
        let first = iter.next().ok_or(FillCountError::NotFound)?;
        let extra = iter.count();
        if extra == 0 {
            Ok(first)
        } else {
            Err(FillCountError::Multiple(extra + 1))
        }
    }
}

impl DecodedTransaction {
    pub fn fills(&self) -> impl Iterator<Item = &DecodedFill> + '_ {
        self.message.fills()
    }

    pub fn fill_count(&self) -> usize {
        self.message.fill_count()
    }

    pub fn has_token_ledger_fill(&self) -> bool {
        self.message.has_token_ledger_fill()
    }

    pub fn single_fill(&self) -> core::result::Result<&DecodedFill, FillCountError> {
        self.message.single_fill()
    }
}

pub(crate) fn decode_message(parsed: ParsedMessage) -> Result<DecodedMessage> {
    let static_count = parsed.static_account_keys.len();
    let required_signatures = parsed.header.num_required_signatures as usize;
    let readonly_signed = parsed.header.num_readonly_signed_accounts as usize;
    let readonly_unsigned = parsed.header.num_readonly_unsigned_accounts as usize;
    if required_signatures > static_count
        || readonly_signed > required_signatures
        || readonly_unsigned > static_count.saturating_sub(required_signatures)
    {
        return Err(FillDecoderError::Other(format!(
            "invalid message header for {static_count} static account keys"
        )));
    }

    let loaded_writable_count = parsed
        .address_table_lookups
        .iter()
        .map(|lookup| lookup.writable_indexes.len())
        .sum();
    let loaded_readonly_count = parsed
        .address_table_lookups
        .iter()
        .map(|lookup| lookup.readonly_indexes.len())
        .sum();
    let total_account_count =
        parsed.static_account_keys.len() + loaded_writable_count + loaded_readonly_count;

    if total_account_count > MAX_ACCOUNT_KEYS {
        return Err(FillDecoderError::Other(format!(
            "message contains {total_account_count} account keys; maximum is {MAX_ACCOUNT_KEYS}"
        )));
    }

    let instructions = parsed
        .instructions
        .into_iter()
        .enumerate()
        .map(|(index, ix)| build_instruction(index, ix, total_account_count))
        .collect::<Result<Vec<_>>>()?;

    Ok(DecodedMessage {
        version: parsed.version,
        header: parsed.header,
        recent_blockhash: parsed.recent_blockhash,
        static_account_keys: parsed.static_account_keys,
        instructions,
        address_table_lookups: parsed.address_table_lookups,
        loaded_writable_count,
        loaded_readonly_count,
    })
}

fn build_instruction(
    instruction_index: usize,
    ix: CompiledInstruction,
    total_account_count: usize,
) -> Result<DecodedInstruction> {
    let validate = |index: u8, kind: &str| -> Result<()> {
        if (index as usize) < total_account_count {
            Ok(())
        } else {
            Err(FillDecoderError::Other(format!(
                "instruction {instruction_index} {kind} index {index} is out of range for \
                 {total_account_count} account keys"
            )))
        }
    };

    validate(ix.program_id_index, "program id")?;
    for &index in &ix.account_indices {
        validate(index, "account")?;
    }

    Ok(DecodedInstruction {
        instruction_index,
        program_id_index: ix.program_id_index,
        account_indices: ix.account_indices,
        data: ix.data,
        fills: Vec::new(),
        entrypoint: None,
    })
}

pub(crate) fn decode_transaction(parsed: ParsedTransaction) -> Result<DecodedTransaction> {
    let required_signatures = parsed.message.header.num_required_signatures as usize;
    if parsed.signatures.len() != required_signatures {
        return Err(FillDecoderError::Other(format!(
            "transaction contains {} signatures; message requires {required_signatures}",
            parsed.signatures.len()
        )));
    }
    Ok(DecodedTransaction {
        signatures: parsed.signatures,
        message: decode_message(parsed.message)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(rfq_id: u64) -> DecodedFill {
        DecodedFill {
            taker_side: crate::Side::Bid,
            rfq_id,
            expire_at: 0,
            levels: vec![],
        }
    }

    fn ix(fills: Vec<DecodedFill>, entrypoint: Option<EntrypointKind>) -> DecodedInstruction {
        DecodedInstruction {
            instruction_index: 0,
            program_id_index: 0,
            account_indices: vec![],
            data: vec![],
            fills,
            entrypoint,
        }
    }

    fn msg(instructions: Vec<DecodedInstruction>) -> DecodedMessage {
        DecodedMessage {
            version: MessageVersion::Legacy,
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            recent_blockhash: [0; 32],
            static_account_keys: vec![[0; 32]],
            instructions,
            address_table_lookups: vec![],
            loaded_writable_count: 0,
            loaded_readonly_count: 0,
        }
    }

    #[test]
    fn single_fill_and_count() {
        let message = msg(vec![ix(vec![fill(42)], None)]);
        assert_eq!(message.single_fill().unwrap().rfq_id, 42);
        assert_eq!(message.fill_count(), 1);
    }

    #[test]
    fn single_fill_rejects_zero_and_multiple() {
        assert_eq!(
            msg(vec![ix(vec![], None)]).single_fill().unwrap_err(),
            FillCountError::NotFound
        );
        assert_eq!(
            msg(vec![ix(vec![fill(1), fill(2)], None)])
                .single_fill()
                .unwrap_err(),
            FillCountError::Multiple(2)
        );
    }

    #[test]
    fn token_ledger_fill_is_flagged_only_when_it_has_a_fill() {
        assert!(
            msg(vec![ix(vec![fill(1)], Some(EntrypointKind::TokenLedger))]).has_token_ledger_fill()
        );
        assert!(!msg(vec![ix(vec![], Some(EntrypointKind::TokenLedger))]).has_token_ledger_fill());
    }

    #[test]
    fn counts_dynamic_indices_without_loading_alt_contents() {
        let parsed = ParsedMessage {
            version: MessageVersion::V0,
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            static_account_keys: vec![[1; 32], [2; 32]],
            recent_blockhash: [0; 32],
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                account_indices: vec![0, 2, 4],
                data: vec![],
            }],
            address_table_lookups: vec![AddressTableLookup {
                table_key: [9; 32],
                writable_indexes: vec![3],
                readonly_indexes: vec![4, 5],
            }],
        };
        let decoded = decode_message(parsed).unwrap();
        assert_eq!(decoded.loaded_writable_count, 1);
        assert_eq!(decoded.loaded_readonly_count, 2);
        assert_eq!(decoded.total_account_count(), 5);
    }

    #[test]
    fn rejects_out_of_range_instruction_index() {
        let parsed = ParsedMessage {
            version: MessageVersion::Legacy,
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            static_account_keys: vec![[1; 32]],
            recent_blockhash: [0; 32],
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                account_indices: vec![],
                data: vec![],
            }],
            address_table_lookups: vec![],
        };
        assert!(decode_message(parsed).is_err());
    }

    #[test]
    fn rejects_signature_count_mismatch() {
        let parsed = ParsedTransaction {
            signatures: vec![],
            message: ParsedMessage {
                version: MessageVersion::Legacy,
                header: MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                static_account_keys: vec![[1; 32]],
                recent_blockhash: [0; 32],
                instructions: vec![],
                address_table_lookups: vec![],
            },
        };
        assert!(decode_transaction(parsed).is_err());
    }
}
