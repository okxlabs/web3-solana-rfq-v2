use anchor_lang::prelude::*;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Bid = 0,
    Ask = 1,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    pub base_atoms: u64,
    pub quote_atoms: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct FillExactInParams {
    pub rfq_id: u64,
    pub expire_at: i64,
    pub min_out_atoms: u64,
    pub levels: Vec<Level>,
}

#[event]
pub struct FillExactInEvent {
    pub rfq_id: u64,
    pub user: Pubkey,
    pub fill_authority: Pubkey,

    pub taker_side: Side,

    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,

    pub amount_in_atoms: u64,
    pub amount_out_atoms: u64,
}
