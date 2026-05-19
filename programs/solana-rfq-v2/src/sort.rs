use crate::error::ErrorCode;
use crate::state::Level;
use anchor_lang::prelude::*;

pub fn assert_sorted_bid(levels: &[Level]) -> Result<()> {
    for w in levels.windows(2) {
        let lhs = (w[0].quote_atoms as u128) * (w[1].base_atoms as u128);
        let rhs = (w[1].quote_atoms as u128) * (w[0].base_atoms as u128);
        require!(lhs < rhs, ErrorCode::InvalidLevelOrdering);
    }
    Ok(())
}

pub fn assert_sorted_ask(levels: &[Level]) -> Result<()> {
    for w in levels.windows(2) {
        let lhs = (w[0].quote_atoms as u128) * (w[1].base_atoms as u128);
        let rhs = (w[1].quote_atoms as u128) * (w[0].base_atoms as u128);
        require!(lhs > rhs, ErrorCode::InvalidLevelOrdering);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lvl(b: u64, q: u64) -> Level {
        Level { base_atoms: b, quote_atoms: q }
    }

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
    fn single_level_always_ok() {
        let v = vec![lvl(1, 1)];
        assert!(assert_sorted_bid(&v).is_ok());
        assert!(assert_sorted_ask(&v).is_ok());
    }

    #[test]
    fn u128_intermediate_does_not_overflow_at_u64_max() {
        let v = vec![lvl(u64::MAX, 1), lvl(1, u64::MAX)];
        assert!(assert_sorted_bid(&v).is_ok());
    }
}
