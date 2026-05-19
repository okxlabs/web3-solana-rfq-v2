use anchor_lang::prelude::*;

#[error_code]
pub enum ErrorCode {
    #[msg("u128 intermediate value overflowed")]
    Overflow,
    #[msg("insufficient liquidity to fully consume amount_in")]
    InsufficientLiquidity,
    #[msg("orderbook expired (now > expire_at)")]
    StaleOrderbook,
    #[msg("levels are not strictly monotonic in implied price")]
    InvalidLevelOrdering,
    #[msg("levels array is empty")]
    EmptyLevels,
    #[msg("maker account appears in another top-level instruction")]
    MakerAppearsInOtherInstruction,
    #[msg("amount_in_atoms must be > 0")]
    ZeroAmountIn,
    #[msg("a level has base_atoms == 0 or quote_atoms == 0")]
    DegenerateLevelEntry,
    #[msg("mint has an extension incompatible with sweep math")]
    UnsupportedMintExtension,
    #[msg("sweep result is below taker's min_out_atoms")]
    SlippageExceeded,
}
