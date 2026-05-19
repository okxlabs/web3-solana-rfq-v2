use crate::error::ErrorCode;
use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions::{
    load_current_index_checked, load_instruction_at_checked,
};
use spl_token_2022::extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions};
use spl_token_2022::state::Mint as Token2022Mint;

/// Reject mints whose Token-2022 extensions break amount-preserving transfer semantics.
pub fn check_mint_compatibility(mint_account: &AccountInfo) -> Result<()> {
    if mint_account.owner == &anchor_spl::token::ID {
        return Ok(());
    }

    let data = mint_account.data.borrow();
    let mint = StateWithExtensions::<Token2022Mint>::unpack(&data)
        .map_err(|_| ErrorCode::UnsupportedMintExtension)?;

    let extensions = mint
        .get_extension_types()
        .map_err(|_| ErrorCode::UnsupportedMintExtension)?;

    for ext in extensions {
        match ext {
            ExtensionType::TransferFeeConfig
            | ExtensionType::TransferHook
            | ExtensionType::NonTransferable
            | ExtensionType::DefaultAccountState
            | ExtensionType::PermanentDelegate
            | ExtensionType::ConfidentialTransferMint
            | ExtensionType::ConfidentialTransferFeeConfig => {
                return Err(ErrorCode::UnsupportedMintExtension.into());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Reject the fill if any of `protected` appears as an account in any
/// *other* top-level instruction of the same transaction.
pub fn check_fill_exclusivity(
    instructions_sysvar: &AccountInfo,
    protected: &[Pubkey],
) -> Result<()> {
    let current = load_current_index_checked(instructions_sysvar)? as usize;

    let mut idx: usize = 0;
    while let Ok(ix) = load_instruction_at_checked(idx, instructions_sysvar) {
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
