use anchor_lang::prelude::*;

declare_id!("2NWeqNzPxecVfGsA2fVSVa6ZQKiCWWUAJ7w8fDPej3n8");

#[program]
pub mod solana_rfq_v2 {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        msg!("Greetings from: {:?}", ctx.program_id);
        Ok(())
    }
}

#[derive(Accounts)]
pub struct Initialize {}
