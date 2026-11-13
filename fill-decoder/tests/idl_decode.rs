use base64::Engine;
use borsh::BorshDeserialize;
use fill_decoder::{
    decode_message_bytes, decode_transaction_base64, decode_transaction_bytes,
    idl_types::{entrypoint, Dex},
    FillDecoderError, Side, AGGREGATOR_IDL_JSON,
};
use serde_json::Value;

const TX: &str = include_str!("fixtures/pumpswap_rfq_jupiter_v1.b64");

fn raw_tx() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(TX.trim())
        .unwrap()
}

fn swap_offset(bytes: &[u8]) -> usize {
    bytes
        .windows(8)
        .position(|w| w == entrypoint::SWAP_TOC)
        .unwrap()
}

fn assert_decode_error(bytes: &[u8], expected: &str) {
    let err = decode_transaction_bytes(bytes).unwrap_err();
    assert!(matches!(
        err,
        FillDecoderError::AggregatorInstruction {
            instruction_index: 2,
            ..
        }
    ));
    let message = err.to_string();
    assert!(message.contains(expected), "{message}");
    assert!(!message.contains("no SolRfqV2 leg"));
    // v1 message-only input excludes the two trailing signatures.
    let message_err = decode_message_bytes(&bytes[..bytes.len() - 128]).unwrap_err();
    assert_eq!(message_err.to_string(), message);
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
    bytes[start + 38] = 255;
    assert_decode_error(&bytes, "route 0: unknown Dex variant tag 255");
}

#[test]
fn unknown_dex_after_rfq_does_not_return_partial_fill() {
    let mut bytes = raw_tx();
    let start = swap_offset(&bytes);
    bytes[start + 99] = 255;
    assert_decode_error(&bytes, "route 2: unknown Dex variant tag 255");
}

#[test]
fn truncated_variant_body_returns_error_instead_of_no_fill() {
    let mut bytes = raw_tx();
    let start = swap_offset(&bytes);
    // HumidifiSwapRouter needs 96 body bytes, more than remain in this instruction.
    bytes[start + 38] = 134;
    assert_decode_error(&bytes, "route 0:");
}

#[test]
fn invalid_rfq_side_returns_error_instead_of_no_fill() {
    let mut bytes = raw_tx();
    let start = swap_offset(&bytes);
    bytes[start + 43] = 2;
    assert_decode_error(&bytes, "route 1: invalid RfqSide tag 2");
}

// Independent test encoder: use non-empty vectors and the last nested enum
// variant to exercise lengths and newly added DynamicCandidate layouts.
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
