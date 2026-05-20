use crate::checks::{check_fill_exclusivity, check_mint_compatibility};
use crate::error::ErrorCode;
use crate::sweep::{assert_sorted_ask, assert_sorted_bid, sweep_ask, sweep_bid};
use crate::types::{FillExactInEvent, FillExactInParams, Side};
use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};

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
