//! Public swap types with Dex field readers generated from the embedded
//! aggregator IDL at build time. Update the IDL and rebuild for new variants.

use crate::types::Level;
use borsh::io::Read;
use borsh::BorshDeserialize;

include!(concat!(env!("OUT_DIR"), "/aggregator_idl.rs"));

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
        decode_idl_dex(reader)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapArgs {
    pub order_id: u64,
    pub amount_in: u64,
    pub expect_amount_out: u64,
    pub slippage: u16,
    pub routes: Vec<Route>,
}

impl BorshDeserialize for SwapArgs {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let order_id = u64::deserialize_reader(reader)?;
        let amount_in = u64::deserialize_reader(reader)?;
        let expect_amount_out = u64::deserialize_reader(reader)?;
        let slippage = u16::deserialize_reader(reader)?;
        let count = u32::deserialize_reader(reader)?;
        let mut routes = Vec::new();
        for index in 0..count {
            let route = Route::deserialize_reader(reader).map_err(|err| {
                borsh::io::Error::new(err.kind(), format!("route {index}: {err}"))
            })?;
            routes.push(route);
        }
        Ok(Self {
            order_id,
            amount_in,
            expect_amount_out,
            slippage,
            routes,
        })
    }
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
/// discriminator matched. Malformed args in a supported entrypoint return an
/// error with the route index; they are never treated as an absent RFQ.
pub fn decode_swap_args(data: &[u8]) -> borsh::io::Result<Option<AnySwapArgs>> {
    if data.len() < 8 {
        return Ok(None);
    }
    let mut disc = [0u8; 8];
    disc.copy_from_slice(&data[..8]);
    let body = &data[8..];

    // Try the simple SwapArgs entrypoints first; suffix fields after the args
    // are ignored by Borsh-from-reader (it stops at end of struct definition).
    let try_swap_args = |body: &[u8]| {
        let mut reader = std::io::Cursor::new(body);
        SwapArgs::deserialize_reader(&mut reader).map(|args| Some(AnySwapArgs::Concrete(args)))
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
        | entrypoint::SWAP_TOB_WITH_RECEIVER_TOKEN_LEDGER_V3 => Ok(Some(AnySwapArgs::Unsupported)),
        _ => Ok(None),
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
        bytes.extend_from_slice(&[0xab; 48]);
        bytes.extend_from_slice(&[0, 1]); // valid Borsh bools
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
