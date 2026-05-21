use crate::error::ErrorCode;
use crate::sweep::{assert_sorted_ask, assert_sorted_bid, sweep_ask, sweep_bid};
use crate::types::{FillExactInEvent, FillExactInParams, Side};
use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar;
use anchor_lang::solana_program::sysvar::instructions::{
    load_current_index_checked, load_instruction_at_checked,
};
use anchor_spl::token_interface::{
    get_mint_extension_data, transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};
use spl_token_2022::extension::transfer_fee::TransferFeeConfig;

/// Disable the Token-2022 TransferFee extension when its current-epoch fee is
/// non-zero: the sweep math assumes the atoms debited from the sender equal
/// the atoms credited to the receiver, and any non-zero transfer fee silently
/// violates that invariant.
///
/// All other Token-2022 extensions pass through. Their failure modes are
/// fail-safe under tx atomicity — a hostile or restrictive extension causes
/// the transfer_checked CPI to fail, which reverts the entire transaction.
///
/// Classic SPL Token (non-2022) mints have no extensions — pass through.
fn check_mint_compatibility(mint_account: &AccountInfo, epoch: u64) -> Result<()> {
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

/// Reject if any `protected` key appears in another top-level ix.
/// Caller program is unconstrained on purpose: the maker signs the tx,
/// so vetting the wrapper is the maker engine's job (program_id check
/// + simulate-and-diff on maker balance changes), not this contract's.
///
/// SVM forces any indirect use of `protected` to surface in some top-level
/// ix's metas, so this loop is exhaustive against siblings. The current
/// ix's inner CPI tree is skipped — that surface is covered off-chain.
fn check_fill_exclusivity(
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

#[event_cpi]
#[derive(Accounts)]
pub struct FillExactIn<'info> {
    pub user: Signer<'info>,

    pub fill_authority: Signer<'info>,

    #[account(
        mut,
        token::mint = base_mint,
        token::authority = user,
        token::token_program = base_token_program,
    )]
    pub user_base_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        token::mint = quote_mint,
        token::authority = user,
        token::token_program = quote_token_program,
    )]
    pub user_quote_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        token::mint = base_mint,
        token::authority = fill_authority,
        token::token_program = base_token_program,
    )]
    pub maker_base_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        token::mint = quote_mint,
        token::authority = fill_authority,
        token::token_program = quote_token_program,
    )]
    pub maker_quote_token_account: InterfaceAccount<'info, TokenAccount>,

    pub base_mint: InterfaceAccount<'info, Mint>,
    pub quote_mint: InterfaceAccount<'info, Mint>,

    pub base_token_program: Interface<'info, TokenInterface>,
    pub quote_token_program: Interface<'info, TokenInterface>,

    /// CHECK: address constraint pins this to the instructions sysvar.
    #[account(address = sysvar::instructions::ID)]
    pub instructions_sysvar: AccountInfo<'info>,
}

pub fn handler(
    ctx: Context<FillExactIn>,
    taker_side: Side,
    amount_in_atoms: u64,
    min_out_atoms: u64,
    params: FillExactInParams,
) -> Result<()> {
    require!(amount_in_atoms > 0, ErrorCode::ZeroAmountIn);
    require!(!params.levels.is_empty(), ErrorCode::EmptyLevels);

    let clock = Clock::get()?;
    require!(
        clock.unix_timestamp <= params.expire_at,
        ErrorCode::StaleOrderbook
    );

    check_mint_compatibility(&ctx.accounts.base_mint.to_account_info(), clock.epoch)?;
    check_mint_compatibility(&ctx.accounts.quote_mint.to_account_info(), clock.epoch)?;

    let protected = [
        ctx.accounts.fill_authority.key(),
        ctx.accounts.maker_base_token_account.key(),
        ctx.accounts.maker_quote_token_account.key(),
    ];
    check_fill_exclusivity(&ctx.accounts.instructions_sysvar, &protected)?;

    let amount_out_atoms = match taker_side {
        Side::Bid => {
            assert_sorted_bid(&params.levels)?;
            let out = sweep_bid(amount_in_atoms, min_out_atoms, &params.levels)?;
            transfer_checked(
                CpiContext::new(
                    ctx.accounts.quote_token_program.to_account_info(),
                    TransferChecked {
                        from: ctx.accounts.user_quote_token_account.to_account_info(),
                        mint: ctx.accounts.quote_mint.to_account_info(),
                        to: ctx.accounts.maker_quote_token_account.to_account_info(),
                        authority: ctx.accounts.user.to_account_info(),
                    },
                ),
                amount_in_atoms,
                ctx.accounts.quote_mint.decimals,
            )?;
            transfer_checked(
                CpiContext::new(
                    ctx.accounts.base_token_program.to_account_info(),
                    TransferChecked {
                        from: ctx.accounts.maker_base_token_account.to_account_info(),
                        mint: ctx.accounts.base_mint.to_account_info(),
                        to: ctx.accounts.user_base_token_account.to_account_info(),
                        authority: ctx.accounts.fill_authority.to_account_info(),
                    },
                ),
                out,
                ctx.accounts.base_mint.decimals,
            )?;
            out
        }
        Side::Ask => {
            assert_sorted_ask(&params.levels)?;
            let out = sweep_ask(amount_in_atoms, min_out_atoms, &params.levels)?;
            transfer_checked(
                CpiContext::new(
                    ctx.accounts.base_token_program.to_account_info(),
                    TransferChecked {
                        from: ctx.accounts.user_base_token_account.to_account_info(),
                        mint: ctx.accounts.base_mint.to_account_info(),
                        to: ctx.accounts.maker_base_token_account.to_account_info(),
                        authority: ctx.accounts.user.to_account_info(),
                    },
                ),
                amount_in_atoms,
                ctx.accounts.base_mint.decimals,
            )?;
            transfer_checked(
                CpiContext::new(
                    ctx.accounts.quote_token_program.to_account_info(),
                    TransferChecked {
                        from: ctx.accounts.maker_quote_token_account.to_account_info(),
                        mint: ctx.accounts.quote_mint.to_account_info(),
                        to: ctx.accounts.user_quote_token_account.to_account_info(),
                        authority: ctx.accounts.fill_authority.to_account_info(),
                    },
                ),
                out,
                ctx.accounts.quote_mint.decimals,
            )?;
            out
        }
    };

    emit_cpi!(FillExactInEvent {
        rfq_id: params.rfq_id,
        user: ctx.accounts.user.key(),
        fill_authority: ctx.accounts.fill_authority.key(),
        taker_side,
        base_mint: ctx.accounts.base_mint.key(),
        quote_mint: ctx.accounts.quote_mint.key(),
        amount_in_atoms,
        amount_out_atoms,
    });

    Ok(())
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use anchor_lang::solana_program::sysvar::instructions::{
        construct_instructions_data, store_current_index, BorrowedAccountMeta, BorrowedInstruction,
    };

    fn run_exclusivity_check(data: &mut [u8], protected: &[Pubkey]) -> Result<()> {
        let sysvar_key = sysvar::instructions::ID;
        let sysvar_owner = sysvar::ID;
        let mut sysvar_lamports = 0;
        let account_info = AccountInfo::new(
            &sysvar_key,
            false,
            false,
            &mut sysvar_lamports,
            data,
            &sysvar_owner,
            false,
            0,
        );

        check_fill_exclusivity(&account_info, protected)
    }

    #[test]
    fn exclusivity_allows_top_level_rfq() {
        let maker = Pubkey::new_unique();
        let ix_data = [];
        let rfq_ix = BorrowedInstruction {
            program_id: &crate::ID,
            accounts: vec![BorrowedAccountMeta {
                pubkey: &maker,
                is_signer: true,
                is_writable: false,
            }],
            data: &ix_data,
        };
        let mut data = construct_instructions_data(&[rfq_ix]);
        store_current_index(&mut data, 0);

        let result = run_exclusivity_check(&mut data, &[maker]);

        assert!(result.is_ok(), "top-level RFQ instruction should pass");
    }

    #[test]
    fn exclusivity_allows_arbitrary_cpi_wrapper() {
        let wrapper_program = Pubkey::new_unique();
        let maker = Pubkey::new_unique();
        let ix_data = [];
        let wrapper_ix = BorrowedInstruction {
            program_id: &wrapper_program,
            accounts: vec![BorrowedAccountMeta {
                pubkey: &maker,
                is_signer: true,
                is_writable: false,
            }],
            data: &ix_data,
        };
        let mut data = construct_instructions_data(&[wrapper_ix]);
        store_current_index(&mut data, 0);

        let result = run_exclusivity_check(&mut data, &[maker]);

        assert!(
            result.is_ok(),
            "CPI from any wrapper should pass — caller identity is enforced off-chain"
        );
    }

    #[test]
    fn exclusivity_rejects_sibling_referencing_maker_when_rfq_is_top_level() {
        let other_program = Pubkey::new_unique();
        let maker = Pubkey::new_unique();
        let ix_data = [];
        let rfq_ix = BorrowedInstruction {
            program_id: &crate::ID,
            accounts: vec![BorrowedAccountMeta {
                pubkey: &maker,
                is_signer: true,
                is_writable: false,
            }],
            data: &ix_data,
        };
        let sibling_ix = BorrowedInstruction {
            program_id: &other_program,
            accounts: vec![BorrowedAccountMeta {
                pubkey: &maker,
                is_signer: false,
                is_writable: false,
            }],
            data: &ix_data,
        };
        let mut data = construct_instructions_data(&[rfq_ix, sibling_ix]);
        store_current_index(&mut data, 0);

        let result = run_exclusivity_check(&mut data, &[maker]);

        let err = result.expect_err("sibling referencing maker must be rejected");
        assert!(format!("{err:?}").contains("MakerAppearsInOtherInstruction"));
    }

    #[test]
    fn exclusivity_rejects_sibling_referencing_maker_when_cpi_wrapper_is_top_level() {
        let wrapper_program = Pubkey::new_unique();
        let other_program = Pubkey::new_unique();
        let maker = Pubkey::new_unique();
        let ix_data = [];
        let wrapper_ix = BorrowedInstruction {
            program_id: &wrapper_program,
            accounts: vec![BorrowedAccountMeta {
                pubkey: &maker,
                is_signer: true,
                is_writable: false,
            }],
            data: &ix_data,
        };
        let sibling_ix = BorrowedInstruction {
            program_id: &other_program,
            accounts: vec![BorrowedAccountMeta {
                pubkey: &maker,
                is_signer: false,
                is_writable: false,
            }],
            data: &ix_data,
        };
        let mut data = construct_instructions_data(&[wrapper_ix, sibling_ix]);
        store_current_index(&mut data, 0);

        let result = run_exclusivity_check(&mut data, &[maker]);

        let err = result.expect_err("sibling referencing maker must be rejected");
        assert!(format!("{err:?}").contains("MakerAppearsInOtherInstruction"));
    }
}
