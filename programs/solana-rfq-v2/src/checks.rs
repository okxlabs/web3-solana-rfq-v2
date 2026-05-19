use crate::error::ErrorCode;
use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions::{
    load_current_index_checked, load_instruction_at_checked,
};
use anchor_spl::token_interface::get_mint_extension_data;
use spl_token_2022::extension::transfer_fee::TransferFeeConfig;

/// Reject only Token-2022 mints whose current-epoch transfer fee is non-zero
/// (would break amount-preserving sweep math). All other extensions are
/// accepted; their failure modes are fail-safe under tx atomicity (a hostile
/// or restrictive extension causes the CPI to fail, which reverts the entire
/// transaction).
///
/// Classic SPL Token (non-2022) mints have no extensions — pass through.
pub fn check_mint_compatibility(mint_account: &AccountInfo, epoch: u64) -> Result<()> {
    if mint_account.owner == &anchor_spl::token::ID {
        return Ok(());
    }

    if let Ok(cfg) = get_mint_extension_data::<TransferFeeConfig>(mint_account) {
        let fee = cfg.get_epoch_fee(epoch);
        require!(
            u16::from(fee.transfer_fee_basis_points) == 0,
            ErrorCode::UnsupportedMintExtension
        );
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
