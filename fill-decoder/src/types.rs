//! Core wire-format types for solana-rfq-v2 maker quote inspection.

use borsh::{BorshDeserialize, BorshSerialize};

/// Which side of the book the taker is on, relative to the maker's
/// `(base_mint, quote_mint)` pair.
///
/// On-wire: 1-byte discriminant (`0x00` = Bid, `0x01` = Ask), matching the
/// `taker_side` field of `Dex::SolRfqV2` and the on-chain
/// `programs/solana-rfq-v2/src/types.rs::Side`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    /// Taker pays quote, receives base.
    Bid = 0,
    /// Taker pays base, receives quote.
    Ask = 1,
}

impl core::fmt::Display for Side {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Side::Bid => write!(f, "Bid"),
            Side::Ask => write!(f, "Ask"),
        }
    }
}

impl Side {
    /// Decode from the 1-byte wire discriminant.
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Side::Bid),
            1 => Some(Side::Ask),
            _ => None,
        }
    }
}

/// One maker-quoted price-level. On-wire: two `u64` LE atoms = 16 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Level {
    pub base_atoms: u64,
    pub quote_atoms: u64,
}

/// Human-readable view of a [`Level`]: quote-per-base price plus base quantity,
/// both in human units (atoms scaled by their respective mint decimals).
///
/// `f64` is used for ergonomics. For exact comparisons (e.g., matching a
/// decoded level against a pre-signed quote), compare `base_atoms` /
/// `quote_atoms` directly — those are the on-wire integers the on-chain
/// handler uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceQty {
    /// Quote per base, in human units. NaN when `base_atoms == 0`.
    pub price: f64,
    /// Base quantity in human units.
    pub qty: f64,
}

impl Level {
    /// Convert this level to a `(price, qty)` view using the maker's
    /// known mint decimals.
    ///
    /// `price = (quote_atoms / 10^quote_decimals) / (base_atoms / 10^base_decimals)`
    /// `qty   = base_atoms / 10^base_decimals`
    pub fn to_price_qty(&self, base_decimals: u8, quote_decimals: u8) -> PriceQty {
        let base = self.base_atoms as f64 / 10f64.powi(base_decimals as i32);
        let quote = self.quote_atoms as f64 / 10f64.powi(quote_decimals as i32);
        PriceQty {
            price: if base == 0.0 { f64::NAN } else { quote / base },
            qty: base,
        }
    }
}

/// One `Dex::SolRfqV2` leg recovered from an OKX aggregator swap instruction.
///
/// The maker uses `rfq_id` to look up its own pre-signed quote, verifies the
/// decoded `taker_side` and `levels` match that quote, and checks `expire_at`
/// against the wall clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFill {
    pub taker_side: Side,
    pub rfq_id: u64,
    pub expire_at: i64,
    pub levels: Vec<Level>,
}

impl DecodedFill {
    /// Convert every level to its `(price, qty)` view using the maker's
    /// known mint decimals. Order matches `self.levels`.
    pub fn levels_price_qty(&self, base_decimals: u8, quote_decimals: u8) -> Vec<PriceQty> {
        self.levels
            .iter()
            .map(|l| l.to_price_qty(base_decimals, quote_decimals))
            .collect()
    }
}

/// Why [`crate::DecodedMessage::single_fill`] could not return a fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillCountError {
    /// No SolRfqV2 leg was found anywhere in the transaction. Either the tx is
    /// not for this maker, or it is not an RFQ-bearing aggregator tx at all.
    NotFound,
    /// More than one SolRfqV2 leg was found. A legitimate maker-signing flow
    /// expects exactly one fill per tx, so refuse to sign.
    Multiple(usize),
}

impl core::fmt::Display for FillCountError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FillCountError::NotFound => write!(f, "no SolRfqV2 leg found in transaction"),
            FillCountError::Multiple(n) => write!(
                f,
                "{n} SolRfqV2 legs found in transaction; expected exactly 1"
            ),
        }
    }
}

impl std::error::Error for FillCountError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn sol_usdc_one_sol_at_85() {
        // 1 SOL (9 decimals) at 85 USDC (6 decimals).
        let l = Level {
            base_atoms: 1_000_000_000,
            quote_atoms: 85_000_000,
        };
        let pq = l.to_price_qty(9, 6);
        assert!(approx(pq.price, 85.0), "got price {}", pq.price);
        assert!(approx(pq.qty, 1.0), "got qty {}", pq.qty);
    }

    #[test]
    fn sol_usdc_hundred_sol_at_8510() {
        // 100 SOL at 85.10 USDC.
        let l = Level {
            base_atoms: 100_000_000_000,
            quote_atoms: 8_510_000_000,
        };
        let pq = l.to_price_qty(9, 6);
        assert!(approx(pq.price, 85.10), "got price {}", pq.price);
        assert!(approx(pq.qty, 100.0), "got qty {}", pq.qty);
    }

    #[test]
    fn same_decimals_yields_atom_ratio() {
        // When base and quote share decimals, price collapses to the atom ratio.
        let l = Level {
            base_atoms: 2_000,
            quote_atoms: 5_000,
        };
        let pq = l.to_price_qty(6, 6);
        assert!(approx(pq.price, 2.5));
        assert!(approx(pq.qty, 0.002));
    }

    #[test]
    fn zero_base_atoms_yields_nan_price() {
        let l = Level {
            base_atoms: 0,
            quote_atoms: 1_000,
        };
        let pq = l.to_price_qty(9, 6);
        assert!(pq.price.is_nan());
        assert_eq!(pq.qty, 0.0);
    }

    #[test]
    fn fill_levels_price_qty_iterates_in_order() {
        let f = DecodedFill {
            taker_side: Side::Bid,
            rfq_id: 1,
            expire_at: 0,
            levels: vec![
                Level {
                    base_atoms: 1_000_000_000,
                    quote_atoms: 85_000_000,
                },
                Level {
                    base_atoms: 2_000_000_000,
                    quote_atoms: 172_000_000,
                },
            ],
        };
        let pqs = f.levels_price_qty(9, 6);
        assert_eq!(pqs.len(), 2);
        assert!(approx(pqs[0].price, 85.0));
        assert!(approx(pqs[1].price, 86.0));
        assert!(approx(pqs[0].qty, 1.0));
        assert!(approx(pqs[1].qty, 2.0));
    }
}
