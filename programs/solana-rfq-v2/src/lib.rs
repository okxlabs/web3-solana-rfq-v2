use anchor_lang::prelude::*;

pub mod checks;
pub mod error;
pub mod handler;
pub mod sweep;
pub mod types;

use crate::handler::*;
use crate::types::{FillExactInParams, Side};

declare_id!("RFQ27dg5gSha2cDzQxuGyhfkz5CK2fUSy3Sjw4Rptyj");

#[program]
pub mod solana_rfq_v2 {
    use super::*;

    pub fn fill_exact_in(
        ctx: Context<FillExactIn>,
        taker_side: Side,
        amount_in_atoms: u64,
        min_out_atoms: u64,
        params: FillExactInParams,
    ) -> Result<()> {
        handler::handler(ctx, taker_side, amount_in_atoms, min_out_atoms, params)
    }
}
