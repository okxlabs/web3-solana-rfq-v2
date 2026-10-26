//! Solana transaction wire-format parser.
//!
//! Handles legacy, v0, and SIMD-0385 v1 messages. Legacy/v0 lengths use
//! strict (canonical) compact-u16 decoding; v1 uses fixed-width lengths.
//! Pubkeys exposed as `[u8; 32]`. No dependency on `solana-sdk`.
//!
//! Solana v0 wire format reference:
//! - tx envelope = `compact_u16(num_sigs) || num_sigs × 64-byte sig || message`
//! - legacy message = `header(3) || compact_u16(n_keys) || n_keys × 32 || blockhash(32) || compact_u16(n_ix) || N × ix`
//! - v0 message    = `0x80 || legacy_body || compact_u16(n_alt) || N × ALT_lookup`
//! - ALT lookup    = `table_key(32) || compact_u16(n_writable) || N × u8 || compact_u16(n_readonly) || N × u8`
//! - ix            = `program_id_idx(u8) || compact_u16(n_accs) || N × u8 || compact_u16(data_len) || data`
//!
//! SIMD-0385 v1 uses `0x81 || header || config_mask || lifetime || counts ||
//! addresses || config_values || instruction_headers || instruction_payloads || signatures`.
//! Message-only input excludes the trailing signatures.

use crate::error::{FillDecoderError, Result};

const SIGNATURE_LEN: usize = 64;
const PUBKEY_LEN: usize = 32;
const BLOCKHASH_LEN: usize = 32;
const V0_MESSAGE_VERSION_TAG: u8 = 0x80;
const V1_MESSAGE_VERSION_TAG: u8 = 0x81;
const V1_MAX_TRANSACTION_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageVersion {
    Legacy,
    V0,
    V1,
}

/// SIMD-0385 configuration words, in ascending mask-bit order.
/// Only the defined configuration bits 0–4 are accepted by the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V1TransactionConfig {
    pub mask: u32,
    pub values: Vec<u32>,
}

impl V1TransactionConfig {
    fn value(&self, bit: u32) -> Option<u32> {
        if self.mask & (1 << bit) == 0 {
            return None;
        }
        let index = (self.mask & ((1 << bit) - 1)).count_ones() as usize;
        self.values.get(index).copied()
    }

    pub fn priority_fee_lamports(&self) -> u64 {
        self.value(0).unwrap_or(0) as u64 | ((self.value(1).unwrap_or(0) as u64) << 32)
    }

    pub fn compute_unit_limit(&self) -> u32 {
        self.value(2).unwrap_or(0)
    }

    pub fn loaded_accounts_data_size_limit(&self) -> u32 {
        self.value(3).unwrap_or(0)
    }

    pub fn heap_size(&self) -> u32 {
        self.value(4).unwrap_or(32 * 1024)
    }
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
    /// Empty for legacy and v1 messages.
    pub address_table_lookups: Vec<AddressTableLookup>,
    /// Present only for v1, including when its mask is zero.
    pub transaction_config: Option<V1TransactionConfig>,
}

#[derive(Debug, Clone)]
pub struct ParsedTransaction {
    pub signatures: Vec<[u8; SIGNATURE_LEN]>,
    pub message: ParsedMessage,
}

/// Parse a full transaction. Legacy/v0 use a leading signature envelope;
/// v1 starts with its version byte and ends with fixed-count signatures.
pub fn parse_transaction(bytes: &[u8]) -> Result<ParsedTransaction> {
    let mut c = Cursor::new(bytes);
    if c.peek_byte()? == V1_MESSAGE_VERSION_TAG {
        validate_v1_size(bytes.len())?;
        let message = parse_v1_message(&mut c)?;
        let mut signatures = Vec::with_capacity(message.header.num_required_signatures as usize);
        for _ in 0..message.header.num_required_signatures {
            let mut signature = [0; SIGNATURE_LEN];
            c.read_into(&mut signature)?;
            signatures.push(signature);
        }
        c.finish()?;
        return Ok(ParsedTransaction {
            signatures,
            message,
        });
    }
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
    if c.peek_byte()? == V1_MESSAGE_VERSION_TAG {
        let message = parse_v1_message(&mut c)?;
        validate_v1_size(
            bytes.len() + message.header.num_required_signatures as usize * SIGNATURE_LEN,
        )?;
        c.finish()?;
        return Ok(message);
    }
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
        transaction_config: None,
    })
}

fn validate_v1_size(size: usize) -> Result<()> {
    if size > V1_MAX_TRANSACTION_SIZE {
        return Err(FillDecoderError::Other(
            "v1 transaction exceeds 4096 bytes".into(),
        ));
    }
    Ok(())
}

/// Parse the v1 signed portion; signatures, if present, follow this portion.
fn parse_v1_message(c: &mut Cursor) -> Result<ParsedMessage> {
    c.advance(1)?; // 0x81, checked by the caller
    let header = MessageHeader {
        num_required_signatures: c.read_byte()?,
        num_readonly_signed_accounts: c.read_byte()?,
        num_readonly_unsigned_accounts: c.read_byte()?,
    };
    let mask = c.read_u32()?;
    if mask & !0x1f != 0 {
        return Err(FillDecoderError::Other(format!(
            "unsupported v1 configuration mask bits: {:#010x}",
            mask & !0x1f
        )));
    }
    let mut recent_blockhash = [0; BLOCKHASH_LEN];
    c.read_into(&mut recent_blockhash)?;
    let num_ix = c.read_byte()? as usize;
    let num_keys = c.read_byte()? as usize;
    if header.num_required_signatures > 12
        || header.num_readonly_signed_accounts >= header.num_required_signatures
        || num_keys
            < header.num_required_signatures as usize
                + header.num_readonly_unsigned_accounts as usize
        || num_keys > 64
        || num_ix > 64
    {
        return Err(FillDecoderError::Other(
            "invalid v1 header or counts".into(),
        ));
    }
    let mut static_account_keys = Vec::with_capacity(num_keys);
    for _ in 0..num_keys {
        let mut key = [0; PUBKEY_LEN];
        c.read_into(&mut key)?;
        if static_account_keys.contains(&key) {
            return Err(FillDecoderError::Other(
                "duplicate v1 account address".into(),
            ));
        }
        static_account_keys.push(key);
    }
    if mask & 3 != 0 && mask & 3 != 3 {
        return Err(FillDecoderError::Other(
            "v1 priority fee requires both mask bits".into(),
        ));
    }
    let mut values = Vec::with_capacity(mask.count_ones() as usize);
    for _ in 0..mask.count_ones() {
        values.push(c.read_u32()?);
    }
    let config = V1TransactionConfig { mask, values };
    let heap = config.heap_size();
    if !(32 * 1024..=256 * 1024).contains(&heap) || !heap.is_multiple_of(1024) {
        return Err(FillDecoderError::Other("invalid v1 heap size".into()));
    }
    // All headers precede all payloads. Data lengths are little-endian u16.
    let mut headers = Vec::with_capacity(num_ix);
    for _ in 0..num_ix {
        let program = c.read_byte()?;
        let accounts = c.read_byte()? as usize;
        let mut data_len = [0; 2];
        c.read_into(&mut data_len)?;
        headers.push((program, accounts, u16::from_le_bytes(data_len) as usize));
    }
    let mut instructions = Vec::with_capacity(num_ix);
    for (program_id_index, num_accounts, data_len) in headers {
        let mut account_indices = vec![0; num_accounts];
        c.read_into(&mut account_indices)?;
        if program_id_index as usize >= num_keys
            || account_indices
                .iter()
                .any(|&index| index as usize >= num_keys)
        {
            return Err(FillDecoderError::Other(
                "v1 instruction index out of range".into(),
            ));
        }
        let mut data = vec![0; data_len];
        c.read_into(&mut data)?;
        instructions.push(CompiledInstruction {
            program_id_index,
            account_indices,
            data,
        });
    }
    Ok(ParsedMessage {
        version: MessageVersion::V1,
        header,
        static_account_keys,
        recent_blockhash,
        instructions,
        address_table_lookups: Vec::new(),
        transaction_config: Some(config),
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

    fn read_u32(&mut self) -> Result<u32> {
        let mut bytes = [0; 4];
        self.read_into(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn finish(&self) -> Result<()> {
        if self.pos != self.bytes.len() {
            return Err(FillDecoderError::Other(
                "trailing data after v1 payload".into(),
            ));
        }
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
    use crate::{decode_message_bytes, decode_transaction_bytes};

    // Independent wire fixture: two headers followed by two payloads. The second
    // payload has a >255-byte data length to exercise little-endian u16 lengths.
    fn message(mask: u32, values: &[u32]) -> Vec<u8> {
        let mut bytes = vec![0x81, 1, 0, 1];
        bytes.extend_from_slice(&mask.to_le_bytes());
        bytes.extend_from_slice(&[0x77; 32]);
        bytes.extend_from_slice(&[2, 3]); // instruction count, address count
        for key in [1, 2, 3] {
            bytes.extend_from_slice(&[key; 32]);
        }
        for value in values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&[2, 2, 3, 0, 2, 1, 0, 1]); // headers
        bytes.extend_from_slice(&[0, 1, 9, 8, 7]); // first accounts and data
        bytes.push(1); // second account
        bytes.extend_from_slice(&[0xab; 256]);
        bytes
    }

    fn transaction(mut message: Vec<u8>) -> Vec<u8> {
        message.extend_from_slice(&[0x55; 64]);
        message
    }

    #[test]
    fn parses_v1_layout_and_preserves_configuration() {
        let words = [0x11223344, 0x55667788, 200_000, 123_456, 64 * 1024];
        let bytes = message(31, &words);
        let parsed = parse_transaction(&transaction(bytes.clone())).unwrap();
        assert_eq!(parsed.signatures, vec![[0x55; 64]]);
        assert_eq!(parsed.message.version, MessageVersion::V1);
        assert_eq!(parsed.message.recent_blockhash, [0x77; 32]);
        assert_eq!(
            parsed.message.static_account_keys,
            vec![[1; 32], [2; 32], [3; 32]]
        );
        assert!(parsed.message.address_table_lookups.is_empty());
        let config = parsed.message.transaction_config.unwrap();
        assert_eq!(config.mask, 31);
        assert_eq!(config.values, words);
        assert_eq!(config.priority_fee_lamports(), 0x5566778811223344);
        assert_eq!(config.compute_unit_limit(), 200_000);
        assert_eq!(config.loaded_accounts_data_size_limit(), 123_456);
        assert_eq!(config.heap_size(), 64 * 1024);
        let decoded = decode_message_bytes(&bytes).unwrap();
        assert_eq!(decoded.transaction_config.unwrap(), config);
        assert_eq!(decoded.instructions[0].account_indices, [0, 1]);
        assert_eq!(decoded.instructions[0].data, [9, 8, 7]);
        assert_eq!(decoded.instructions[1].account_indices, [1]);
        assert_eq!(decoded.instructions[1].data, vec![0xab; 256]);
    }

    #[test]
    fn defaults_and_sparse_config_bits() {
        let parsed = parse_message(&message(0, &[])).unwrap();
        let config = parsed.transaction_config.unwrap();
        assert_eq!(config.priority_fee_lamports(), 0);
        assert_eq!(config.compute_unit_limit(), 0);
        assert_eq!(config.loaded_accounts_data_size_limit(), 0);
        assert_eq!(config.heap_size(), 32 * 1024);
        let config = parse_message(&message(1 << 2, &[99]))
            .unwrap()
            .transaction_config
            .unwrap();
        assert_eq!(config.compute_unit_limit(), 99);
        assert_eq!(config.values, [99]);
    }

    #[test]
    fn rejects_unsupported_config_mask_bits() {
        for bit in 5..32 {
            for supported_mask in [0, 1 << 2] {
                let values: &[u32] = if supported_mask == 0 {
                    &[42]
                } else {
                    &[99, 42]
                };
                let bytes = message(supported_mask | (1 << bit), values);
                for result in [
                    parse_message(&bytes),
                    parse_transaction(&transaction(bytes.clone())).map(|tx| tx.message),
                ] {
                    assert!(
                        matches!(
                            result,
                            Err(FillDecoderError::Other(reason))
                                if reason.contains("unsupported v1 configuration mask bits")
                        ),
                        "accepted unsupported bit {bit} with mask {supported_mask}"
                    );
                }
            }
        }
    }

    #[test]
    fn rejects_every_truncated_transaction_prefix() {
        let bytes = transaction(message(31, &[1, 2, 3, 4, 32 * 1024]));
        for end in 0..bytes.len() {
            assert!(
                parse_transaction(&bytes[..end]).is_err(),
                "accepted prefix {end}"
            );
        }
    }

    #[test]
    fn rejects_trailing_bytes_and_signatures_in_message_only_input() {
        let msg = message(0, &[]);
        assert!(parse_message(&transaction(msg.clone())).is_err());
        let mut trailing_msg = msg.clone();
        trailing_msg.push(0);
        assert!(parse_message(&trailing_msg).is_err());
        let mut tx = transaction(msg);
        tx.push(0);
        assert!(parse_transaction(&tx).is_err());
    }

    #[test]
    fn rejects_invalid_config() {
        for mask in [1, 2, 5, 6] {
            assert!(parse_message(&message(mask, &[0, 0])).is_err());
        }
        for heap in [0, 31 * 1024, 32 * 1024 + 1, 257 * 1024] {
            assert!(parse_message(&message(16, &[heap])).is_err());
        }
        for heap in [32 * 1024, 256 * 1024] {
            assert!(parse_message(&message(16, &[heap])).is_ok());
        }
    }

    #[test]
    fn rejects_invalid_headers_duplicate_keys_and_indices() {
        for (offset, value) in [
            (1, 0),
            (1, 13),
            (1, 4),
            (2, 1),
            (3, 3),
            (40, 65),
            (41, 65),
            (138, 3), // first program index
            (146, 3), // first instruction account index
        ] {
            let mut bytes = message(0, &[]);
            bytes[offset] = value;
            assert!(
                decode_message_bytes(&bytes).is_err(),
                "offset {offset}, value {value}"
            );
        }
        let mut duplicate = message(0, &[]);
        duplicate[74..106].copy_from_slice(&[1; 32]);
        assert!(parse_message(&duplicate).is_err());
    }

    #[test]
    fn enforces_size_limit_including_signatures() {
        // One signer, two addresses, one instruction with no account indices.
        let mut bytes = vec![0x81, 1, 0, 1];
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 32]);
        bytes.extend_from_slice(&[1, 2]);
        bytes.extend_from_slice(&[1; 32]);
        bytes.extend_from_slice(&[2; 32]);
        let data_len = 4096 - bytes.len() - 4 - 64;
        bytes.extend_from_slice(&[1, 0]);
        bytes.extend_from_slice(&(data_len as u16).to_le_bytes());
        bytes.resize(bytes.len() + data_len, 0);
        assert!(decode_message_bytes(&bytes).is_ok());
        assert!(decode_transaction_bytes(&transaction(bytes.clone())).is_ok());
        bytes[108..110].copy_from_slice(&((data_len + 1) as u16).to_le_bytes());
        bytes.push(0);
        assert!(parse_message(&bytes).is_err());
        assert!(parse_transaction(&transaction(bytes)).is_err());
    }

    #[test]
    fn supports_multiple_trailing_signatures() {
        let mut bytes = message(0, &[]);
        bytes[1] = 2;
        bytes[2] = 1;
        let mut tx = transaction(bytes);
        tx.extend_from_slice(&[0x66; 64]);
        let parsed = decode_transaction_bytes(&tx).unwrap();
        assert_eq!(parsed.signatures, vec![[0x55; 64], [0x66; 64]]);
    }

    #[test]
    fn legacy_and_v0_envelopes_still_decode() {
        for version in [MessageVersion::Legacy, MessageVersion::V0] {
            let mut msg = Vec::new();
            if version == MessageVersion::V0 {
                msg.push(0x80);
            }
            msg.extend_from_slice(&[1, 0, 1, 2]); // header and compact key count
            msg.extend_from_slice(&[1; 32]);
            msg.extend_from_slice(&[2; 32]);
            msg.extend_from_slice(&[0x77; 32]);
            msg.extend_from_slice(&[1, 1, 1, 0, 2, 9, 8]); // one instruction
            if version == MessageVersion::V0 {
                msg.push(0); // no ALTs
            }
            let mut tx = vec![1]; // compact signature count
            tx.extend_from_slice(&[0x55; 64]);
            tx.extend_from_slice(&msg);
            let decoded = decode_transaction_bytes(&tx).unwrap();
            assert_eq!(decoded.message.version, version);
            assert!(decoded.message.transaction_config.is_none());
            assert_eq!(decoded.signatures, vec![[0x55; 64]]);
            assert_eq!(decoded.message.instructions[0].data, [9, 8]);
            assert_eq!(decode_message_bytes(&msg).unwrap().version, version);
        }
    }

    /// User-provided sample, with a missing leading Base64 `g` restored.
    /// This is a decoding fixture, not proof of mainnet inclusion or valid signatures.
    #[test]
    fn user_v1_transaction_decodes_one_rfq_fill() {
        let b64 = concat!(
            "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",
        );
        let tx = crate::decode_transaction_base64(b64).unwrap();
        assert_eq!(tx.message.version, MessageVersion::V1);
        assert_eq!(tx.signatures.len(), 2);
        assert_eq!(tx.message.static_account_keys.len(), 26);
        assert_eq!(tx.message.instructions.len(), 10);
        assert!(tx.message.address_table_lookups.is_empty());
        let config = tx.message.transaction_config.as_ref().unwrap();
        assert_eq!(config.mask, 15);
        assert_eq!(config.priority_fee_lamports(), 237_260);
        assert_eq!(config.compute_unit_limit(), 202_000);
        assert_eq!(config.loaded_accounts_data_size_limit(), 67_108_864);
        assert_eq!(config.heap_size(), 32_768);
        let fill = tx.single_fill().unwrap();
        assert_eq!(fill.taker_side, crate::Side::Bid);
        assert_eq!(fill.rfq_id, 4_668_517_833_210_568_064);
        assert_eq!(fill.expire_at, 1_793_001_383);
        assert_eq!(
            fill.levels,
            vec![crate::Level {
                base_atoms: 30_201_019,
                quote_atoms: 300_000_000,
            }]
        );
        assert_eq!(tx.message.instructions[7].fills.len(), 1);
    }

    #[test]
    fn rejects_unknown_versions() {
        for version in [0x82, 0xff] {
            let mut bytes = message(0, &[]);
            bytes[0] = version;
            assert!(parse_message(&bytes).is_err());
        }
    }

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
