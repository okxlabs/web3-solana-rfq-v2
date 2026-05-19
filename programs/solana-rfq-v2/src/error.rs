use anchor_lang::prelude::*;

#[error_code]
pub enum ErrorCode {
    #[msg("u128 intermediate value overflowed")]
    Overflow,                            // 6000
    #[msg("insufficient liquidity to fully consume amount_in")]
    InsufficientLiquidity,               // 6001
    #[msg("orderbook expired (now > expire_at)")]
    StaleOrderbook,                      // 6002
    #[msg("levels are not strictly monotonic in implied price")]
    InvalidLevelOrdering,                // 6003
    #[msg("levels array is empty")]
    EmptyLevels,                         // 6004
    #[msg("maker account appears in another top-level instruction")]
    MakerAppearsInOtherInstruction,      // 6005
    #[msg("amount_in_atoms must be > 0")]
    ZeroAmountIn,                        // 6006
    #[msg("a level has base_atoms == 0 or quote_atoms == 0")]
    DegenerateLevelEntry,                // 6007
    #[msg("mint has an extension incompatible with sweep math")]
    UnsupportedMintExtension,            // 6008
    #[msg("sweep result is below taker's min_out_atoms")]
    SlippageExceeded,                    // 6009
}
