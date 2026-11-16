use borsh::BorshDeserialize;
use fill_decoder::{idl_types::Dex, AGGREGATOR_IDL_JSON};
use serde_json::Value;

fn fixed_size(ty: &Value) -> usize {
    if let Some(name) = ty.as_str() {
        return match name {
            "bool" | "u8" | "i8" => 1,
            "u16" | "i16" => 2,
            "u32" | "i32" | "f32" => 4,
            "u64" | "i64" | "f64" => 8,
            "u128" | "i128" => 16,
            "pubkey" => 32,
            _ => panic!("unsupported fixed-size IDL primitive: {name}"),
        };
    }
    let array = ty
        .get("array")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("unhandled IDL field type; extend the layout test: {ty}"));
    fixed_size(&array[0]) * array[1].as_u64().expect("fixed array length") as usize
}

fn body_size(variant: &Value) -> usize {
    variant["fields"].as_array().map_or(0, |fields| {
        fields
            .iter()
            .map(|field| fixed_size(field.get("type").unwrap_or(field)))
            .sum()
    })
}

fn variants<'a>(idl: &'a Value, name: &str) -> &'a [Value] {
    idl["types"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ty| ty["name"] == name)
        .unwrap()["type"]["variants"]
        .as_array()
        .unwrap()
}

#[test]
fn fixed_dex_bodies_match_the_embedded_idl() {
    let idl: Value = serde_json::from_str(AGGREGATOR_IDL_JSON).unwrap();
    for (tag, variant) in variants(&idl, "Dex").iter().enumerate() {
        // Variable-length bodies have separate route/cursor and transaction tests.
        if matches!(tag, 117 | 119 | 126 | 133) {
            continue;
        }
        let mut bytes = vec![u8::try_from(tag).unwrap()];
        bytes.resize(1 + body_size(variant), 0);
        let expected_len = bytes.len();
        for end in 0..expected_len {
            assert!(
                Dex::deserialize_reader(&mut &bytes[..end]).is_err(),
                "Dex tag {tag} ({}) accepted truncated length {end}",
                variant["name"]
            );
        }
        bytes.push(255); // a following byte must remain unread
        let mut reader = std::io::Cursor::new(&bytes);
        let dex = Dex::deserialize_reader(&mut reader)
            .unwrap_or_else(|error| panic!("Dex tag {tag} ({}): {error}", variant["name"]));
        assert_eq!(dex, Dex::Other(tag as u8));
        assert_eq!(
            reader.position() as usize,
            expected_len,
            "Dex tag {tag} ({})",
            variant["name"]
        );
    }
}

#[test]
fn dynamic_candidate_bodies_match_the_embedded_idl() {
    let idl: Value = serde_json::from_str(AGGREGATOR_IDL_JSON).unwrap();
    for (tag, variant) in variants(&idl, "DynamicCandidate").iter().enumerate() {
        let mut bytes = vec![126]; // Dex::DynamicRouteV1
        bytes.extend_from_slice(&1u32.to_le_bytes()); // one candidate
        bytes.push(u8::try_from(tag).unwrap());
        bytes.resize(bytes.len() + body_size(variant), 0);
        bytes.push(0); // SelectionMode::Auto
        let expected_len = bytes.len();
        for end in 0..expected_len {
            assert!(
                Dex::deserialize_reader(&mut &bytes[..end]).is_err(),
                "DynamicCandidate tag {tag} ({}) accepted truncated length {end}",
                variant["name"]
            );
        }
        bytes.push(255); // the following route must not be consumed
        let mut reader = std::io::Cursor::new(&bytes);
        assert_eq!(
            Dex::deserialize_reader(&mut reader).unwrap_or_else(|error| panic!(
                "DynamicCandidate tag {tag} ({}): {error}",
                variant["name"]
            )),
            Dex::Other(126)
        );
        assert_eq!(
            reader.position() as usize,
            expected_len,
            "DynamicCandidate tag {tag} ({})",
            variant["name"]
        );
    }
}
