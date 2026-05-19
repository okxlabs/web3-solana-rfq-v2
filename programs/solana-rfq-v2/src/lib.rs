use anchor_lang::prelude::*;

pub mod checks;
pub mod error;
pub mod handler;
pub mod state;
pub mod sweep;

use crate::handler::*;
use crate::state::{FillExactInParams, Side};

declare_id!("2NWeqNzPxecVfGsA2fVSVa6ZQKiCWWUAJ7w8fDPej3n8");

#[program]
pub mod solana_rfq_v2 {
    use super::*;

    pub fn fill_exact_in(
        ctx: Context<FillExactIn>,
        taker_side: Side,
        amount_in_atoms: u64,
        params: FillExactInParams,
    ) -> Result<()> {
        handler::handler(ctx, taker_side, amount_in_atoms, params)
    }
}
