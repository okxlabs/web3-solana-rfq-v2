use base64::Engine;
use borsh::BorshDeserialize;
use fill_decoder::{
    decode_message_bytes, decode_transaction_base64, decode_transaction_bytes,
    idl_types::{entrypoint, Dex},
    is_dex_solana_v3_program,
    wire::parse_transaction,
    FillDecoderError, MessageVersion, Side, AGGREGATOR_IDL_JSON,
};
use serde_json::Value;

const TX: &str = include_str!("fixtures/pumpswap_rfq_jupiter_v1.b64");

// Fixture offsets within swap instruction data:
// discriminator + three u64 fields + slippage u16 + routes Vec length u32 = 38.
const ROUTE_0_DEX: usize = 8 + 8 * 3 + 2 + 4;
// Route 0: unit Dex tag u8 + weight u16 + routing index u8 = 4 bytes; offset 42.
const ROUTE_1_DEX: usize = ROUTE_0_DEX + 1 + 2 + 1;
// Route 1 RFQ tag + taker_side = offset 43.
const ROUTE_1_SIDE: usize = ROUTE_1_DEX + 1;
// RFQ Dex: tag + side + rfq_id + expire_at + levels length + two levels = 54 bytes.
const RFQ_DEX_BYTES: usize = 1 + 1 + 8 + 8 + 4 + 2 * 16;
// Route 1 Dex + weight u16 + routing index u8 = 57 bytes; route 2 starts at 99.
const ROUTE_2_DEX: usize = ROUTE_1_DEX + RFQ_DEX_BYTES + 2 + 1;

fn raw_tx() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(TX.trim())
        .unwrap()
}

fn swap_offset(bytes: &[u8]) -> usize {
    let message = parse_transaction(bytes).unwrap().message;
    assert_eq!(message.version, MessageVersion::V1);
    let (index, swap) = message
        .instructions
        .iter()
        .enumerate()
        .find(|(_, ix)| {
            is_dex_solana_v3_program(&message.static_account_keys[ix.program_id_index as usize])
                && ix.data.starts_with(&entrypoint::SWAP_TOC)
        })
        .unwrap();

    // V1: version + signer header + config mask + lifetime + ix/key counts,
    // then static keys, config u32 values, and all four-byte instruction headers.
    let payload_start = 1
        + 3
        + 4
        + 32
        + 1
        + 1
        + message.static_account_keys.len() * 32
        + message.transaction_config.as_ref().unwrap().values.len() * 4
        + message.instructions.len() * 4;
    let previous_payloads: usize = message.instructions[..index]
        .iter()
        .map(|ix| ix.account_indices.len() + ix.data.len())
        .sum();
    let start = payload_start + previous_payloads + swap.account_indices.len();
    assert_eq!(&bytes[start..start + swap.data.len()], swap.data.as_slice());
    start
}

#[test]
fn swap_offset_ignores_discriminator_bytes_in_a_pubkey() {
    let mut bytes = raw_tx();
    let expected = swap_offset(&bytes);
    // V1 static keys begin after the fixed 42-byte header.
    bytes[42..50].copy_from_slice(&entrypoint::SWAP_TOC);
    assert_eq!(swap_offset(&bytes), expected);
}

fn assert_decode_error(bytes: &[u8], expected: &str) {
    let err = decode_transaction_bytes(bytes).unwrap_err();
    assert!(matches!(err, FillDecoderError::Borsh(_)));
    let message = err.to_string();
    assert!(
        message.contains("aggregator instruction 2 decode failed:"),
        "{message}"
    );
    assert!(message.contains(expected), "{message}");
    assert!(!message.contains("no SolRfqV2 leg"));
    // v1 message-only input excludes the two trailing signatures.
    let message_err = decode_message_bytes(&bytes[..bytes.len() - 128]).unwrap_err();
    assert_eq!(message_err.to_string(), message);
}

#[test]
fn error_enum_remains_exhaustive_for_existing_callers() {
    // This is an external-crate match: a new variant or #[non_exhaustive]
    // would break compilation just as it would for existing SDK callers.
    fn legacy_error_kind(error: &FillDecoderError) -> &'static str {
        match error {
            FillDecoderError::InvalidEncoding => "encoding",
            FillDecoderError::Truncated { .. } => "truncated",
            FillDecoderError::NonCanonicalCompactU16 => "compact-u16",
            FillDecoderError::Borsh(_) => "borsh",
            FillDecoderError::Other(_) => "other",
        }
    }
    assert_eq!(
        legacy_error_kind(&FillDecoderError::Borsh("test".into())),
        "borsh"
    );
}

#[test]
fn original_v1_transaction_decodes_without_modification() {
    let tx = decode_transaction_base64(TX).unwrap();
    let fill = tx.single_fill().unwrap();
    assert_eq!(fill.rfq_id, 4692700956412293824);
    assert_eq!(fill.taker_side, Side::Bid);
    assert_eq!(fill.expire_at, 1794442809);
    assert_eq!(fill.levels.len(), 2);
    assert_eq!(fill.levels[0].base_atoms, 40723998);
    assert_eq!(fill.levels[0].quote_atoms, 487450270);
    assert_eq!(fill.levels[1].base_atoms, 99999999);
    assert_eq!(fill.levels[1].quote_atoms, 2393921479);
}

#[test]
fn unknown_dex_before_rfq_returns_instruction_route_and_tag() {
    let mut bytes = raw_tx();
    let start = swap_offset(&bytes);
    bytes[start + ROUTE_0_DEX] = 255;
    assert_decode_error(&bytes, "route 0: unknown Dex variant tag 255");
}

#[test]
fn unknown_dex_after_rfq_does_not_return_partial_fill() {
    let mut bytes = raw_tx();
    let start = swap_offset(&bytes);
    bytes[start + ROUTE_2_DEX] = 255;
    assert_decode_error(&bytes, "route 2: unknown Dex variant tag 255");
}

#[test]
fn truncated_variant_body_returns_error_instead_of_no_fill() {
    let mut bytes = raw_tx();
    let start = swap_offset(&bytes);
    // HumidifiSwapRouter needs 96 body bytes, more than remain in this instruction.
    bytes[start + ROUTE_0_DEX] = 134;
    assert_decode_error(&bytes, "route 0:");
}

#[test]
fn invalid_rfq_side_returns_error_instead_of_no_fill() {
    let mut bytes = raw_tx();
    let start = swap_offset(&bytes);
    bytes[start + ROUTE_1_SIDE] = 2;
    assert_decode_error(&bytes, "route 1: invalid RfqSide tag 2");
}

// IDL-based test encoder: use non-empty vectors and the last nested enum
// variant to exercise cursor lengths. This shares the generator's IDL model,
// so it is not an independent wire-format oracle or full nested-enum coverage.
// The real transaction fixture above provides a separate regression check.
fn encode_type(ty: &Value, types: &[Value], out: &mut Vec<u8>) {
    if let Some(name) = ty.as_str() {
        let len = match name {
            "bool" | "u8" | "i8" => 1,
            "u16" | "i16" => 2,
            "u32" | "i32" | "f32" => 4,
            "u64" | "i64" | "f64" => 8,
            "u128" | "i128" => 16,
            "pubkey" => 32,
            "bytes" | "string" => {
                out.extend_from_slice(&3u32.to_le_bytes());
                out.extend_from_slice(b"abc");
                return;
            }
            _ => panic!("unknown primitive {name}"),
        };
        out.resize(out.len() + len, 0);
    } else if let Some(name) = ty.get("defined") {
        let definition = &types.iter().find(|t| t["name"] == name["name"]).unwrap()["type"];
        match definition["kind"].as_str().unwrap() {
            "struct" => encode_fields(&definition["fields"], types, out),
            "enum" => {
                let variants = definition["variants"].as_array().unwrap();
                let tag = variants.len() - 1;
                out.push(tag as u8);
                encode_fields(&variants[tag]["fields"], types, out);
            }
            _ => panic!("unknown definition {definition}"),
        }
    } else if let Some(element) = ty.get("vec") {
        out.extend_from_slice(&2u32.to_le_bytes());
        for _ in 0..2 {
            encode_type(element, types, out);
        }
    } else if let Some(array) = ty.get("array") {
        for _ in 0..array[1].as_u64().unwrap() {
            encode_type(&array[0], types, out);
        }
    } else if let Some(element) = ty.get("option") {
        out.push(1);
        encode_type(element, types, out);
    } else {
        panic!("unknown type {ty}");
    }
}

fn encode_fields(fields: &Value, types: &[Value], out: &mut Vec<u8>) {
    if let Some(fields) = fields.as_array() {
        for field in fields {
            encode_type(field.get("type").unwrap_or(field), types, out);
        }
    }
}

#[test]
fn every_idl_dex_consumes_exactly_its_body_and_rejects_truncation() {
    let idl: Value = serde_json::from_str(AGGREGATOR_IDL_JSON).unwrap();
    let types = idl["types"].as_array().unwrap();
    let variants = types.iter().find(|t| t["name"] == "Dex").unwrap()["type"]["variants"]
        .as_array()
        .unwrap();
    for (tag, variant) in variants.iter().enumerate() {
        let mut bytes = vec![tag as u8];
        encode_fields(&variant["fields"], types, &mut bytes);
        let length = bytes.len();
        bytes.extend_from_slice(&[255; 4]);
        let mut reader = std::io::Cursor::new(&bytes);
        let dex = Dex::deserialize_reader(&mut reader)
            .unwrap_or_else(|e| panic!("{}: {e}", variant["name"]));
        assert_eq!(reader.position() as usize, length, "{}", variant["name"]);
        if variant["name"] != "SolRfqV2" {
            assert_eq!(dex, Dex::Other(tag as u8));
        }
        for end in 0..length {
            assert!(
                Dex::deserialize_reader(&mut &bytes[..end]).is_err(),
                "{}: accepted truncated length {end}",
                variant["name"]
            );
        }
    }
}

#[test]
fn nested_unknown_enum_tag_is_not_skipped() {
    // DynamicRouteV1 { candidates: [unknown], mode: Auto }
    let bytes = [126, 1, 0, 0, 0, 255, 0];
    let err = Dex::deserialize_reader(&mut &bytes[..]).unwrap_err();
    assert!(err
        .to_string()
        .contains("unknown DynamicCandidate variant tag 255"));
}
