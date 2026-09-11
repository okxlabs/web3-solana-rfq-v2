//! Rust mirrors of `dex-solana-v3` wire types, derived from
//! `fill-decoder/idls/dex_solana_v3.json`.
//!
//! Only the types needed to walk swap entrypoint args + locate `Dex::SolRfqV2`
//! routes are modeled. The `Dex` enum uses a custom `BorshDeserialize` impl
//! that handles the body-bearing variants and skips the tag-only variants.

use crate::types::Level;
use borsh::io::Read;
use borsh::BorshDeserialize;

/// 8-byte Anchor discriminator for each swap entrypoint. Values copied from
/// `idls/dex_solana_v3.json` (`instructions[*].discriminator`).
pub mod entrypoint {
    pub const SWAP: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
    pub const PROXY_SWAP: [u8; 8] = [19, 44, 130, 148, 72, 56, 44, 238];
    pub const SWAP_TOC: [u8; 8] = [187, 201, 212, 51, 16, 155, 236, 60];
    pub const SWAP_TOC_V2: [u8; 8] = [127, 214, 107, 189, 23, 90, 47, 104];
    pub const SWAP_TOC_V3: [u8; 8] = [86, 222, 68, 49, 225, 9, 201, 235];
    pub const SWAP_TOB: [u8; 8] = [170, 41, 85, 177, 132, 80, 31, 53];
    pub const SWAP_TOB_V2: [u8; 8] = [72, 1, 215, 242, 8, 75, 54, 216];
    pub const SWAP_TOB_V3: [u8; 8] = [14, 191, 44, 246, 142, 225, 224, 157];
    pub const SWAP_TOB_WITH_RECEIVER: [u8; 8] = [223, 170, 216, 234, 204, 6, 241, 25];
    pub const SWAP_TOB_WITH_RECEIVER_V3: [u8; 8] = [26, 190, 234, 223, 241, 5, 177, 189];
    pub const SWAP_TOB_WITH_TOKEN_LEDGER: [u8; 8] = [36, 92, 147, 219, 26, 176, 159, 90];
    pub const SWAP_TOB_WITH_TOKEN_LEDGER_V3: [u8; 8] = [132, 77, 6, 86, 35, 66, 224, 171];
    pub const SWAP_TOB_WITH_RECEIVER_TOKEN_LEDGER: [u8; 8] = [239, 93, 10, 202, 161, 134, 127, 130];
    pub const SWAP_TOB_WITH_RECEIVER_TOKEN_LEDGER_V3: [u8; 8] =
        [119, 172, 209, 16, 91, 44, 63, 224];
    pub const SWAP_TOB_ENHANCED: [u8; 8] = [190, 156, 169, 176, 149, 154, 161, 108];
}

/// `Dex` enum mirror. Only the SolRfqV2 variant carries data we need;
/// all other variants are reduced to their tag byte after their exact body
/// has been consumed. Keeping the body layouts in sync is important: a wrong
/// skip would move the route cursor and could make a later route look like
/// a different RFQ leg.
///
/// `SolRfqV2` body layout (matches upstream `dex-solana-v3` variant):
/// `taker_side: u8 || rfq_id: u64 || expire_at: i64 || levels: Vec<Level>`.
/// `taker_side` is encoded as `sol_rfq_v2::RfqSide` upstream — a single byte
/// discriminant on the wire (0 = Bid, 1 = Ask). The maker's transaction
/// signature commits to this byte, so the on-chain adapter can verify it
/// against the side derived from on-wire source/destination mints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dex {
    SolRfqV2 {
        taker_side: u8,
        rfq_id: u64,
        expire_at: i64,
        levels: Vec<Level>,
    },
    /// Any variant other than `SolRfqV2`. The contained `u8` is the wire-format
    /// variant tag. Bodies of variants with fields are skipped exactly.
    Other(u8),
}

impl BorshDeserialize for Dex {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let tag = u8::deserialize_reader(reader)?;
        // Variant indices and body layouts are confirmed against the
        // aggregator's `Dex` enum and embedded IDL. Variable-length bodies
        // are skipped field-by-field so malformed lengths fail closed without
        // allocating attacker-controlled buffers.
        match tag {
            23 | 24 | 34 | 35 => skip_exact(reader, 1)?, // cashback bool
            64 => skip_exact(reader, 50)?,               // SolRfq: 6×u64 + 2×bool
            74 | 75 => skip_exact(reader, 2)?,           // SugarMoneyBuy/Sell: 2×u8
            81 => skip_exact(reader, 8)?,                // HumidifiSwap2: u64
            82 => skip_exact(reader, 16)?,               // Scorch: u128
            100 => skip_exact(reader, 8)?,               // SanctumPrefundSwapViaStake: u64
            103 => skip_exact(reader, 16)?,              // WhalestreetV2: 2×u64
            104 | 120 => skip_exact(reader, 99)?,        // *WithSig payload
            117 => {
                let taker_side = u8::deserialize_reader(reader)?;
                let rfq_id = u64::deserialize_reader(reader)?;
                let expire_at = i64::deserialize_reader(reader)?;
                let levels = <Vec<Level>>::deserialize_reader(reader)?;
                return Ok(Dex::SolRfqV2 {
                    taker_side,
                    rfq_id,
                    expire_at,
                    levels,
                });
            }
            118 => skip_exact(reader, 8)?, // FluxMM: u64
            119 => {
                skip_remaining_accounts_info(reader)?;
                skip_exact(reader, 1)?; // bin_array_count
            }
            121 | 125 | 138 => skip_exact(reader, 1)?, // hook/book/oracle count
            126 => skip_dynamic_route_spec(reader)?,
            132 => skip_exact(reader, 40)?, // ZerofiWithSig signature
            133 => skip_borsh_bytes(reader)?, // Native quote_data
            134 => skip_exact(reader, 96)?, // HumidifiSwapRouter
            0..=22
            | 25..=33
            | 36..=63
            | 65..=73
            | 76..=80
            | 83..=99
            | 101..=102
            | 105..=116
            | 122..=124
            | 127..=131
            | 135..=137
            | 139..=140 => {}
            _ => {
                return Err(borsh::io::Error::new(
                    borsh::io::ErrorKind::InvalidData,
                    format!("Dex variant tag {} out of declared range 0..=140", tag),
                ));
            }
        }
        Ok(Dex::Other(tag))
    }
}

fn skip_exact<R: Read>(reader: &mut R, len: usize) -> borsh::io::Result<()> {
    let mut remaining = len;
    let mut chunk = [0u8; 64];
    while remaining != 0 {
        let take = remaining.min(chunk.len());
        reader.read_exact(&mut chunk[..take])?;
        remaining -= take;
    }
    Ok(())
}

fn skip_remaining_accounts_info<R: Read>(reader: &mut R) -> borsh::io::Result<()> {
    let count = u32::deserialize_reader(reader)?;
    for _ in 0..count {
        skip_exact(reader, 2)?; // RemainingAccountsSlice { accounts_type, length }
    }
    Ok(())
}

fn skip_borsh_bytes<R: Read>(reader: &mut R) -> borsh::io::Result<()> {
    let len = u32::deserialize_reader(reader)? as usize;
    skip_exact(reader, len)
}

fn skip_dynamic_route_spec<R: Read>(reader: &mut R) -> borsh::io::Result<()> {
    let count = u32::deserialize_reader(reader)?;
    for _ in 0..count {
        let candidate = u8::deserialize_reader(reader)?;
        match candidate {
            3 | 14 => skip_exact(reader, 8)?,  // HumidifiSwap2 / FluxMM swap_id
            5 | 16 => skip_exact(reader, 16)?, // Scorch / ScorchV2 id
            10 => skip_exact(reader, 99)?,     // BisonFiWithSig
            15 => skip_exact(reader, 96)?,     // HumidifiSwapRouter
            0..=2 | 4 | 6..=9 | 11..=13 | 17 => {}
            _ => {
                return Err(borsh::io::Error::new(
                    borsh::io::ErrorKind::InvalidData,
                    format!(
                        "DynamicCandidate tag {} out of declared range 0..=17",
                        candidate
                    ),
                ));
            }
        }
    }

    let mode = u8::deserialize_reader(reader)?;
    if matches!(mode, 1 | 2) {
        skip_exact(reader, 1)?;
    } else if mode != 0 {
        return Err(borsh::io::Error::new(
            borsh::io::ErrorKind::InvalidData,
            format!("SelectionMode tag {} is invalid", mode),
        ));
    }
    Ok(())
}

/// Route in the aggregator's routing graph. The `weight` and `index` fields
/// drive on-chain share routing; we still parse them because they're part of
/// the wire format, but the maker doesn't need them — pre-signed quote levels
/// bound the trade regardless of how the aggregator routes.
#[derive(Debug, Clone, PartialEq, Eq, BorshDeserialize)]
pub struct Route {
    pub dex: Dex,
    pub weight: u16,
    pub index: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, BorshDeserialize)]
pub struct SwapArgs {
    pub order_id: u64,
    pub amount_in: u64,
    pub expect_amount_out: u64,
    pub slippage: u16,
    pub routes: Vec<Route>,
}

/// Token-ledger variants don't carry `amount_in` on the wire — it's derived
/// from the token-ledger account at execution time.
#[derive(Debug, Clone, PartialEq, Eq, BorshDeserialize)]
pub struct SwapArgsTokenLedger {
    pub order_id: u64,
    pub expect_amount_out: u64,
    pub slippage: u16,
    pub routes: Vec<Route>,
}

/// Unified view: routes are all we care about regardless of which
/// `SwapArgs` variant the entrypoint uses.
pub enum AnySwapArgs {
    Concrete(SwapArgs),
    TokenLedger(SwapArgsTokenLedger),
    /// A recognised aggregator entrypoint that this decoder deliberately
    /// refuses to accept, currently the complete `swap_tob*` family.
    Unsupported,
}

impl AnySwapArgs {
    pub fn routes(&self) -> &[Route] {
        match self {
            AnySwapArgs::Concrete(a) => &a.routes,
            AnySwapArgs::TokenLedger(a) => &a.routes,
            AnySwapArgs::Unsupported => &[],
        }
    }

    pub fn kind(&self) -> EntrypointKind {
        match self {
            AnySwapArgs::Concrete(_) => EntrypointKind::Concrete,
            AnySwapArgs::TokenLedger(_) => EntrypointKind::TokenLedger,
            AnySwapArgs::Unsupported => EntrypointKind::Unsupported,
        }
    }
}

/// Which `SwapArgs` shape an aggregator instruction used.
///
/// `Concrete` entrypoints carry `amount_in` directly in the instruction data.
/// `TokenLedger` entrypoints derive `amount_in` from an on-chain "token ledger"
/// account populated by an earlier instruction in the same transaction, so the
/// maker cannot bound the consumed amount from the args alone and the ix can
/// be composed atomically with other swaps. See [`crate::DecodedInstruction::entrypoint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntrypointKind {
    Concrete,
    TokenLedger,
    /// Recognised but not accepted by maker signing policy. This currently
    /// covers every `swap_tob*` entrypoint, including token-ledger variants.
    Unsupported,
}

impl core::fmt::Display for EntrypointKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EntrypointKind::Concrete => f.write_str("Concrete"),
            EntrypointKind::TokenLedger => f.write_str("TokenLedger"),
            EntrypointKind::Unsupported => f.write_str("Unsupported"),
        }
    }
}

/// Match the first 8 bytes of an instruction's data against the known swap
/// entrypoint discriminators. Returns the decoded args (skipping any
/// per-entrypoint suffix fields like commission_info), an explicit
/// `Unsupported` marker for the rejected `swap_tob*` family, or `None` if no
/// discriminator matched.
pub fn decode_swap_args(data: &[u8]) -> Option<AnySwapArgs> {
    if data.len() < 8 {
        return None;
    }
    let mut disc = [0u8; 8];
    disc.copy_from_slice(&data[..8]);
    let body = &data[8..];

    // Try the simple SwapArgs entrypoints first; suffix fields after the args
    // are ignored by Borsh-from-reader (it stops at end of struct definition).
    let try_swap_args = |body: &[u8]| -> Option<AnySwapArgs> {
        let mut reader = std::io::Cursor::new(body);
        SwapArgs::deserialize_reader(&mut reader)
            .ok()
            .map(AnySwapArgs::Concrete)
    };
    match disc {
        entrypoint::SWAP
        | entrypoint::PROXY_SWAP
        | entrypoint::SWAP_TOC
        | entrypoint::SWAP_TOC_V2
        | entrypoint::SWAP_TOC_V3 => try_swap_args(body),
        entrypoint::SWAP_TOB
        | entrypoint::SWAP_TOB_V2
        | entrypoint::SWAP_TOB_V3
        | entrypoint::SWAP_TOB_WITH_RECEIVER
        | entrypoint::SWAP_TOB_WITH_RECEIVER_V3
        | entrypoint::SWAP_TOB_ENHANCED
        | entrypoint::SWAP_TOB_WITH_TOKEN_LEDGER
        | entrypoint::SWAP_TOB_WITH_TOKEN_LEDGER_V3
        | entrypoint::SWAP_TOB_WITH_RECEIVER_TOKEN_LEDGER
        | entrypoint::SWAP_TOB_WITH_RECEIVER_TOKEN_LEDGER_V3 => Some(AnySwapArgs::Unsupported),
        _ => None,
    }
}

/// Hand-encode a `Dex::SolRfqV2` variant body for tests (the variant has
/// no `BorshSerialize` impl by design — we only ever decode from upstream
/// bytes).
#[cfg(test)]
pub(crate) fn encode_sol_rfq_v2_variant(
    taker_side: u8,
    rfq_id: u64,
    expire_at: i64,
    levels: &[Level],
) -> Vec<u8> {
    let mut bytes = vec![117u8]; // variant tag
    bytes.push(taker_side);
    bytes.extend_from_slice(&rfq_id.to_le_bytes());
    bytes.extend_from_slice(&expire_at.to_le_bytes());
    bytes.extend_from_slice(&(levels.len() as u32).to_le_bytes());
    for l in levels {
        bytes.extend_from_slice(&l.base_atoms.to_le_bytes());
        bytes.extend_from_slice(&l.quote_atoms.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_sol_rfq_v2_variant() {
        let levels = vec![Level {
            base_atoms: 100,
            quote_atoms: 85,
        }];
        let bytes = encode_sol_rfq_v2_variant(1, 42, 2_000_000_000, &levels);
        let mut reader = std::io::Cursor::new(&bytes[..]);
        let dex = Dex::deserialize_reader(&mut reader).unwrap();
        match dex {
            Dex::SolRfqV2 {
                taker_side,
                rfq_id,
                expire_at,
                levels,
            } => {
                assert_eq!(taker_side, 1);
                assert_eq!(rfq_id, 42);
                assert_eq!(expire_at, 2_000_000_000);
                assert_eq!(levels.len(), 1);
            }
            _ => panic!("expected SolRfqV2"),
        }
    }

    #[test]
    fn skips_other_unit_variants() {
        // Tag 0 (SplTokenSwap) is unit-bodied; should decode to Other(0) with no body consumed.
        let mut reader = std::io::Cursor::new(&[0u8, 0xff, 0xff][..]);
        let dex = Dex::deserialize_reader(&mut reader).unwrap();
        assert_eq!(dex, Dex::Other(0));
        // Remaining bytes still in the reader: position should be 1.
        assert_eq!(reader.position(), 1);
    }

    #[test]
    fn skips_sol_rfq_v1_body() {
        // Tag 64 (SolRfq) has 50 bytes of body.
        let mut bytes = vec![64u8];
        bytes.extend_from_slice(&[0xab; 50]);
        bytes.extend_from_slice(&[0xcd, 0xef]); // trailing — must NOT be consumed
        let mut reader = std::io::Cursor::new(&bytes[..]);
        let dex = Dex::deserialize_reader(&mut reader).unwrap();
        assert_eq!(dex, Dex::Other(64));
        assert_eq!(reader.position(), 51); // 1 tag + 50 body
    }

    #[test]
    fn rejects_out_of_range_tag() {
        let mut reader = std::io::Cursor::new(&[200u8][..]);
        assert!(Dex::deserialize_reader(&mut reader).is_err());
    }

    #[test]
    fn swap_args_round_trip() {
        // We build a SwapArgs by hand (no Serialize on Dex) and deserialize.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&7u64.to_le_bytes()); // order_id
        bytes.extend_from_slice(&1_000u64.to_le_bytes()); // amount_in
        bytes.extend_from_slice(&0u64.to_le_bytes()); // expect_amount_out
        bytes.extend_from_slice(&100u16.to_le_bytes()); // slippage
        bytes.extend_from_slice(&1u32.to_le_bytes()); // routes len
        bytes.extend_from_slice(&encode_sol_rfq_v2_variant(0, 42, 2_000_000_000, &[]));
        // Borsh empty Vec<Level> = 4-byte zero len; already encoded above.
        bytes.extend_from_slice(&5_000u16.to_le_bytes()); // weight
        bytes.push(0x01); // index: input=0, output=1
        let mut reader = std::io::Cursor::new(&bytes[..]);
        let args = SwapArgs::deserialize_reader(&mut reader).unwrap();
        assert_eq!(args.order_id, 7);
        assert_eq!(args.amount_in, 1_000);
        assert_eq!(args.routes.len(), 1);
        match &args.routes[0].dex {
            Dex::SolRfqV2 { rfq_id, .. } => assert_eq!(*rfq_id, 42),
            _ => panic!("expected SolRfqV2"),
        }
    }
}
