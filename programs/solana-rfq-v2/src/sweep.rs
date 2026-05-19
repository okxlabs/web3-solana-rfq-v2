use crate::error::ErrorCode;
use crate::types::Level;
use anchor_lang::prelude::*;
use core::cmp::Ordering;

fn assert_sorted(levels: &[Level], expected: Ordering) -> Result<()> {
    for w in levels.windows(2) {
        let lhs = (w[0].quote_atoms as u128) * (w[1].base_atoms as u128);
        let rhs = (w[1].quote_atoms as u128) * (w[0].base_atoms as u128);
        require!(lhs.cmp(&rhs) == expected, ErrorCode::InvalidLevelOrdering);
    }
    Ok(())
}

pub fn assert_sorted_bid(levels: &[Level]) -> Result<()> {
    assert_sorted(levels, Ordering::Less)
}

pub fn assert_sorted_ask(levels: &[Level]) -> Result<()> {
    assert_sorted(levels, Ordering::Greater)
}

pub fn sweep_bid(amount_in: u64, min_out_atoms: u64, levels: &[Level]) -> Result<u64> {
    let mut remaining: u64 = amount_in;
    let mut out: u64 = 0;

    for level in levels {
        if remaining == 0 {
            break;
        }
        require!(
            level.base_atoms > 0 && level.quote_atoms > 0,
            ErrorCode::DegenerateLevelEntry
        );

        if remaining >= level.quote_atoms {
            remaining = remaining
                .checked_sub(level.quote_atoms)
                .ok_or(ErrorCode::Overflow)?;
            out = out
                .checked_add(level.base_atoms)
                .ok_or(ErrorCode::Overflow)?;
        } else {
            let partial = (remaining as u128)
                .checked_mul(level.base_atoms as u128)
                .ok_or(ErrorCode::Overflow)?
                .checked_div(level.quote_atoms as u128)
                .ok_or(ErrorCode::Overflow)?;
            require!(partial <= u64::MAX as u128, ErrorCode::Overflow);
            require!(partial > 0, ErrorCode::InsufficientLiquidity);
            out = out.checked_add(partial as u64).ok_or(ErrorCode::Overflow)?;
            remaining = 0;
            break;
        }
    }

    require!(remaining == 0, ErrorCode::InsufficientLiquidity);
    require!(out >= min_out_atoms, ErrorCode::SlippageExceeded);
    Ok(out)
}

pub fn sweep_ask(amount_in: u64, min_out_atoms: u64, levels: &[Level]) -> Result<u64> {
    let mut remaining: u64 = amount_in;
    let mut out: u64 = 0;

    for level in levels {
        if remaining == 0 {
            break;
        }
        require!(
            level.base_atoms > 0 && level.quote_atoms > 0,
            ErrorCode::DegenerateLevelEntry
        );

        if remaining >= level.base_atoms {
            remaining = remaining
                .checked_sub(level.base_atoms)
                .ok_or(ErrorCode::Overflow)?;
            out = out
                .checked_add(level.quote_atoms)
                .ok_or(ErrorCode::Overflow)?;
        } else {
            let partial = (remaining as u128)
                .checked_mul(level.quote_atoms as u128)
                .ok_or(ErrorCode::Overflow)?
                .checked_div(level.base_atoms as u128)
                .ok_or(ErrorCode::Overflow)?;
            require!(partial <= u64::MAX as u128, ErrorCode::Overflow);
            require!(partial > 0, ErrorCode::InsufficientLiquidity);
            out = out.checked_add(partial as u64).ok_or(ErrorCode::Overflow)?;
            remaining = 0;
            break;
        }
    }

    require!(remaining == 0, ErrorCode::InsufficientLiquidity);
    require!(out >= min_out_atoms, ErrorCode::SlippageExceeded);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lvl(b: u64, q: u64) -> Level {
        Level {
            base_atoms: b,
            quote_atoms: q,
        }
    }

    // §6 — sort validation
    #[test]
    fn bid_strictly_ascending_passes() {
        let v = vec![
            lvl(100_000_000_000, 8_510_000_000),
            lvl(200_000_000_000, 17_040_000_000),
            lvl(300_000_000_000, 25_590_000_000),
        ];
        assert!(assert_sorted_bid(&v).is_ok());
    }

    #[test]
    fn bid_equal_adjacent_levels_fail() {
        let v = vec![lvl(100, 851), lvl(200, 1702)];
        let err = assert_sorted_bid(&v).unwrap_err();
        assert!(format!("{err:?}").contains("InvalidLevelOrdering"));
    }

    #[test]
    fn bid_descending_fails() {
        let v = vec![lvl(100, 853), lvl(100, 851)];
        assert!(assert_sorted_bid(&v).is_err());
    }

    #[test]
    fn ask_strictly_descending_passes() {
        let v = vec![
            lvl(100_000_000_000, 8_490_000_000),
            lvl(200_000_000_000, 16_960_000_000),
            lvl(300_000_000_000, 25_410_000_000),
        ];
        assert!(assert_sorted_ask(&v).is_ok());
    }

    #[test]
    fn ask_equal_adjacent_levels_fail() {
        let v = vec![lvl(100, 849), lvl(200, 1698)];
        assert!(assert_sorted_ask(&v).is_err());
    }

    #[test]
    fn sort_single_level_always_ok() {
        let v = vec![lvl(1, 1)];
        assert!(assert_sorted_bid(&v).is_ok());
        assert!(assert_sorted_ask(&v).is_ok());
    }

    #[test]
    fn sort_u128_intermediate_does_not_overflow_at_u64_max() {
        let v = vec![lvl(u64::MAX, 1), lvl(1, u64::MAX)];
        assert!(assert_sorted_bid(&v).is_ok());
    }

    // §5.3
    #[test]
    fn bid_full_consume_two_levels() {
        let levels = vec![
            lvl(100_000_000_000, 8_510_000_000),
            lvl(200_000_000_000, 17_040_000_000),
            lvl(300_000_000_000, 25_590_000_000),
        ];
        let out = sweep_bid(25_550_000_000, 0, &levels).unwrap();
        assert_eq!(out, 300_000_000_000);
    }

    // §5.4
    #[test]
    fn ask_partial_last_level_no_dust() {
        let levels = vec![
            lvl(100_000_000_000, 8_490_000_000),
            lvl(200_000_000_000, 16_960_000_000),
            lvl(300_000_000_000, 25_410_000_000),
        ];
        let out = sweep_ask(150_000_000_000, 0, &levels).unwrap();
        assert_eq!(out, 12_730_000_000);
    }

    #[test]
    fn ask_partial_last_level_with_dust() {
        let levels = vec![
            lvl(100_000_000_000, 8_490_000_000),
            lvl(200_000_000_000, 16_960_000_001),
            lvl(300_000_000_000, 25_410_000_000),
        ];
        let out = sweep_ask(150_000_000_000, 0, &levels).unwrap();
        assert_eq!(out, 8_490_000_000 + 4_240_000_000);
    }

    // §5.5
    #[test]
    fn bid_insufficient_liquidity_reverts() {
        let levels = vec![
            lvl(100_000_000_000, 8_510_000_000),
            lvl(200_000_000_000, 17_040_000_000),
            lvl(300_000_000_000, 25_590_000_000),
        ];
        let err = sweep_bid(60_000_000_000, 0, &levels).unwrap_err();
        assert!(format!("{err:?}").contains("InsufficientLiquidity"));
    }

    // §5.6
    #[test]
    fn bid_tiny_input_partial_positive() {
        let levels = vec![lvl(100_000_000_000, 8_510_000_000)];
        let out = sweep_bid(10, 0, &levels).unwrap();
        assert_eq!(out, 117);
    }

    #[test]
    fn bid_malicious_levels_rejected_by_partial_guard() {
        let levels = vec![lvl(1, 1), lvl(1, 1_000_000_000_000)];
        let err = sweep_bid(1_000_000_000_000, 0, &levels).unwrap_err();
        assert!(format!("{err:?}").contains("InsufficientLiquidity"));
    }

    // §7
    #[test]
    fn bid_zero_base_atoms_reverts() {
        let levels = vec![lvl(0, 100)];
        let err = sweep_bid(10, 0, &levels).unwrap_err();
        assert!(format!("{err:?}").contains("DegenerateLevelEntry"));
    }

    #[test]
    fn bid_zero_quote_atoms_reverts() {
        let levels = vec![lvl(100, 0)];
        let err = sweep_bid(10, 0, &levels).unwrap_err();
        assert!(format!("{err:?}").contains("DegenerateLevelEntry"));
    }

    #[test]
    fn bid_slippage_protection() {
        let levels = vec![lvl(100_000_000_000, 8_510_000_000)];
        let err = sweep_bid(10, 200, &levels).unwrap_err();
        assert!(format!("{err:?}").contains("SlippageExceeded"));
    }

    #[test]
    fn ask_full_consume_one_level() {
        let levels = vec![lvl(100, 8500), lvl(100, 8400)];
        let out = sweep_ask(100, 0, &levels).unwrap();
        assert_eq!(out, 8500);
    }

    #[test]
    fn ask_malicious_partial_zero_rejected() {
        // descending quote/base: (1,1) -> 1.0,  (1e12,1) -> 1e-12
        let levels = vec![lvl(1, 1), lvl(1_000_000_000_000, 1)];
        let err = sweep_ask(1_000_000_000_000, 0, &levels).unwrap_err();
        assert!(format!("{err:?}").contains("InsufficientLiquidity"));
    }
}
