use crate::error::ErrorCode;
use crate::exclusivity::check_fill_exclusivity;
use crate::mint_check::check_mint_compatibility;
use crate::sort::{assert_sorted_ask, assert_sorted_bid};
use crate::state::{FillExactInParams, Side};
use crate::sweep::{sweep_ask, sweep_bid};
use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};

#[derive(Accounts)]
pub struct FillExactIn<'info> {
    pub user: Signer<'info>,

    pub fill_authority: Signer<'info>,

    #[account(mut)]
    pub user_base_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub user_quote_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(mut)]
    pub maker_base_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
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
    params: FillExactInParams,
) -> Result<()> {
    require!(amount_in_atoms > 0, ErrorCode::ZeroAmountIn);
    require!(!params.levels.is_empty(), ErrorCode::EmptyLevels);

    let now = Clock::get()?.unix_timestamp;
    require!(now <= params.expire_at, ErrorCode::StaleOrderbook);

    check_mint_compatibility(&ctx.accounts.base_mint.to_account_info())?;
    check_mint_compatibility(&ctx.accounts.quote_mint.to_account_info())?;

    let protected = [
        ctx.accounts.fill_authority.key(),
        ctx.accounts.maker_base_token_account.key(),
        ctx.accounts.maker_quote_token_account.key(),
    ];
    check_fill_exclusivity(&ctx.accounts.instructions_sysvar, &protected)?;

    match taker_side {
        Side::Bid => {
            assert_sorted_bid(&params.levels)?;
            let out = sweep_bid(amount_in_atoms, params.min_out_atoms, &params.levels)?;
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
        }
        Side::Ask => {
            assert_sorted_ask(&params.levels)?;
            let out = sweep_ask(amount_in_atoms, params.min_out_atoms, &params.levels)?;
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
        }
    }

    Ok(())
}
