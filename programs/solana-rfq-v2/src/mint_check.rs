use crate::error::ErrorCode;
use anchor_lang::prelude::*;
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
