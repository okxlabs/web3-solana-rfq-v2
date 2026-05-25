//! Solana transaction wire-format parser.
//!
//! Handles legacy and v0 messages with strict (canonical) compact-u16 decoding.
//! Pubkeys exposed as `[u8; 32]`. No dependency on `solana-sdk`.
//!
//! Solana v0 wire format reference:
//! - tx envelope = `compact_u16(num_sigs) || num_sigs × 64-byte sig || message`
//! - legacy message = `header(3) || compact_u16(n_keys) || n_keys × 32 || blockhash(32) || compact_u16(n_ix) || N × ix`
//! - v0 message    = `0x80 || legacy_body || compact_u16(n_alt) || N × ALT_lookup`
//! - ALT lookup    = `table_key(32) || compact_u16(n_writable) || N × u8 || compact_u16(n_readonly) || N × u8`
//! - ix            = `program_id_idx(u8) || compact_u16(n_accs) || N × u8 || compact_u16(data_len) || data`

use crate::error::{FillDecoderError, Result};

const SIGNATURE_LEN: usize = 64;
const PUBKEY_LEN: usize = 32;
const BLOCKHASH_LEN: usize = 32;
const V0_MESSAGE_VERSION_TAG: u8 = 0x80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageVersion {
    Legacy,
    V0,
}

#[derive(Debug, Clone)]
pub struct MessageHeader {
    pub num_required_signatures: u8,
    pub num_readonly_signed_accounts: u8,
    pub num_readonly_unsigned_accounts: u8,
}

#[derive(Debug, Clone)]
pub struct CompiledInstruction {
    pub program_id_index: u8,
    pub account_indices: Vec<u8>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct AddressTableLookup {
    pub table_key: [u8; PUBKEY_LEN],
    pub writable_indexes: Vec<u8>,
    pub readonly_indexes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ParsedMessage {
    pub version: MessageVersion,
    pub header: MessageHeader,
    pub static_account_keys: Vec<[u8; PUBKEY_LEN]>,
    pub recent_blockhash: [u8; BLOCKHASH_LEN],
    pub instructions: Vec<CompiledInstruction>,
    /// Empty for legacy messages.
    pub address_table_lookups: Vec<AddressTableLookup>,
}

#[derive(Debug, Clone)]
pub struct ParsedTransaction {
    pub signatures: Vec<[u8; SIGNATURE_LEN]>,
    pub message: ParsedMessage,
}

/// Parse a full transaction (envelope + message).
pub fn parse_transaction(bytes: &[u8]) -> Result<ParsedTransaction> {
    let mut c = Cursor::new(bytes);
    let num_sigs = c.read_compact_u16()? as usize;
    let mut signatures = Vec::with_capacity(num_sigs);
    for _ in 0..num_sigs {
        let mut sig = [0u8; SIGNATURE_LEN];
        c.read_into(&mut sig)?;
        signatures.push(sig);
    }
    let message = parse_message_from_cursor(&mut c)?;
    Ok(ParsedTransaction {
        signatures,
        message,
    })
}

/// Parse a message-only payload (no signature envelope).
pub fn parse_message(bytes: &[u8]) -> Result<ParsedMessage> {
    let mut c = Cursor::new(bytes);
    parse_message_from_cursor(&mut c)
}

fn parse_message_from_cursor(c: &mut Cursor) -> Result<ParsedMessage> {
    // Detect version: first byte's high bit set → v0 (versioned); otherwise legacy.
    let first = c.peek_byte()?;
    let version = if first & V0_MESSAGE_VERSION_TAG != 0 {
        c.advance(1)?; // consume the version tag
        let tag = first & !V0_MESSAGE_VERSION_TAG;
        if tag != 0 {
            return Err(FillDecoderError::Other(format!(
                "unsupported message version {tag}"
            )));
        }
        MessageVersion::V0
    } else {
        MessageVersion::Legacy
    };

    // header (3 bytes)
    let header = MessageHeader {
        num_required_signatures: c.read_byte()?,
        num_readonly_signed_accounts: c.read_byte()?,
        num_readonly_unsigned_accounts: c.read_byte()?,
    };

    // static account keys
    let num_keys = c.read_compact_u16()? as usize;
    let mut static_account_keys = Vec::with_capacity(num_keys);
    for _ in 0..num_keys {
        let mut k = [0u8; PUBKEY_LEN];
        c.read_into(&mut k)?;
        static_account_keys.push(k);
    }

    // recent blockhash
    let mut recent_blockhash = [0u8; BLOCKHASH_LEN];
    c.read_into(&mut recent_blockhash)?;

    // instructions
    let num_ix = c.read_compact_u16()? as usize;
    let mut instructions = Vec::with_capacity(num_ix);
    for _ in 0..num_ix {
        let program_id_index = c.read_byte()?;
        let num_accs = c.read_compact_u16()? as usize;
        let mut account_indices = vec![0u8; num_accs];
        c.read_into(&mut account_indices)?;
        let data_len = c.read_compact_u16()? as usize;
        let mut data = vec![0u8; data_len];
        c.read_into(&mut data)?;
        instructions.push(CompiledInstruction {
            program_id_index,
            account_indices,
            data,
        });
    }

    // v0: trailing address table lookups
    let address_table_lookups = if version == MessageVersion::V0 {
        let num_alt = c.read_compact_u16()? as usize;
        let mut lookups = Vec::with_capacity(num_alt);
        for _ in 0..num_alt {
            let mut table_key = [0u8; PUBKEY_LEN];
            c.read_into(&mut table_key)?;
            let n_writable = c.read_compact_u16()? as usize;
            let mut writable_indexes = vec![0u8; n_writable];
            c.read_into(&mut writable_indexes)?;
            let n_readonly = c.read_compact_u16()? as usize;
            let mut readonly_indexes = vec![0u8; n_readonly];
            c.read_into(&mut readonly_indexes)?;
            lookups.push(AddressTableLookup {
                table_key,
                writable_indexes,
                readonly_indexes,
            });
        }
        lookups
    } else {
        Vec::new()
    };

    Ok(ParsedMessage {
        version,
        header,
        static_account_keys,
        recent_blockhash,
        instructions,
        address_table_lookups,
    })
}

// ─── cursor helpers ────────────────────────────────────────────────────────

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn require(&self, n: usize) -> Result<()> {
        if self.pos + n > self.bytes.len() {
            return Err(FillDecoderError::Truncated {
                expected: self.pos + n,
                actual: self.bytes.len(),
            });
        }
        Ok(())
    }

    fn peek_byte(&self) -> Result<u8> {
        self.require(1)?;
        Ok(self.bytes[self.pos])
    }

    fn read_byte(&mut self) -> Result<u8> {
        self.require(1)?;
        let b = self.bytes[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn advance(&mut self, n: usize) -> Result<()> {
        self.require(n)?;
        self.pos += n;
        Ok(())
    }

    fn read_into(&mut self, dst: &mut [u8]) -> Result<()> {
        let n = dst.len();
        self.require(n)?;
        dst.copy_from_slice(&self.bytes[self.pos..self.pos + n]);
        self.pos += n;
        Ok(())
    }

    /// Strict compact-u16 decode. Rejects non-canonical encodings.
    fn read_compact_u16(&mut self) -> Result<u16> {
        let mut result: u32 = 0;
        for i in 0..3 {
            let b = self.read_byte()? as u32;
            let value = b & 0x7f;
            result |= value << (i * 7);
            if b & 0x80 == 0 {
                // canonical-check: this is the terminating byte
                if i > 0 && value == 0 {
                    // last byte being zero with previous continuation means overlong encoding
                    return Err(FillDecoderError::NonCanonicalCompactU16);
                }
                if result > u16::MAX as u32 {
                    return Err(FillDecoderError::Other(
                        "compact-u16 value exceeds u16::MAX".into(),
                    ));
                }
                return Ok(result as u16);
            }
        }
        Err(FillDecoderError::NonCanonicalCompactU16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_u16_round_trip_basic() {
        let cases: &[(&[u8], u16)] = &[(&[0x00], 0), (&[0x05], 5), (&[0x80, 0x01], 128)];
        for (bytes, expected) in cases {
            let mut c = Cursor::new(bytes);
            assert_eq!(c.read_compact_u16().unwrap(), *expected);
        }
    }

    #[test]
    fn compact_u16_rejects_overlong_zero_tail() {
        // 5 encoded as 0x85 0x00 → overlong; valid encoding is 0x05.
        let mut c = Cursor::new(&[0x85u8, 0x00]);
        assert!(matches!(
            c.read_compact_u16(),
            Err(FillDecoderError::NonCanonicalCompactU16)
        ));
    }

    #[test]
    fn compact_u16_rejects_three_byte_overrun() {
        // Three continuation bytes is malformed (value would exceed u16 range).
        let mut c = Cursor::new(&[0xff, 0xff, 0xff]);
        assert!(c.read_compact_u16().is_err());
    }

    #[test]
    fn truncated_bytes_error() {
        let mut c = Cursor::new(&[]);
        assert!(matches!(
            c.read_byte(),
            Err(FillDecoderError::Truncated { .. })
        ));
    }
}
