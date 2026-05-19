use crate::error::ErrorCode;
use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions::{
    load_current_index_checked, load_instruction_at_checked,
};

/// Reject the fill if any of `protected` appears as an account in any
/// *other* top-level instruction of the same transaction.
pub fn check_fill_exclusivity(
    instructions_sysvar: &AccountInfo,
    protected: &[Pubkey; 3],
) -> Result<()> {
    let current = load_current_index_checked(instructions_sysvar)? as usize;

    let mut idx: usize = 0;
    loop {
        let ix = match load_instruction_at_checked(idx, instructions_sysvar) {
            Ok(ix) => ix,
            Err(_) => break,
        };

        if idx != current {
            for meta in &ix.accounts {
                require!(
                    !protected.contains(&meta.pubkey),
                    ErrorCode::MakerAppearsInOtherInstruction
                );
            }
        }
        idx += 1;
    }
    Ok(())
}
