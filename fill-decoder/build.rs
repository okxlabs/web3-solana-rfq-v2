//! Generate Borsh field readers from the pinned aggregator IDL. No network access.
use serde_json::{json, Value};
use std::{env, fs, path::PathBuf};

fn fields(fields: &Value, types: &[Value], stack: &mut Vec<String>) -> String {
    fields.as_array().map_or_else(String::new, |fields| {
        fields
            .iter()
            .map(|field| {
                // Anchor enum fields can be named fields or tuple fields.
                emit(field.get("type").unwrap_or(field), types, stack)
            })
            .collect()
    })
}

fn emit(ty: &Value, types: &[Value], stack: &mut Vec<String>) -> String {
    if let Some(name) = ty.as_str() {
        return match name {
            "bool" => "let _ = bool::deserialize_reader(reader)?;".into(),
            "u8" | "i8" => "skip_exact(reader, 1)?;".into(),
            "u16" | "i16" => "skip_exact(reader, 2)?;".into(),
            "u32" | "i32" | "f32" => "skip_exact(reader, 4)?;".into(),
            "u64" | "i64" | "f64" => "skip_exact(reader, 8)?;".into(),
            "u128" | "i128" => "skip_exact(reader, 16)?;".into(),
            "pubkey" => "skip_exact(reader, 32)?;".into(),
            "bytes" | "string" => {
                "let len = u32::deserialize_reader(reader)? as usize; skip_exact(reader, len)?;"
                    .into()
            }
            _ => panic!("unsupported IDL primitive: {name}"),
        };
    }
    if let Some(defined) = ty.get("defined") {
        let name = defined["name"].as_str().expect("defined type name");
        assert!(
            !stack.iter().any(|n| n == name),
            "recursive IDL type: {name}"
        );
        let definition = &types
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("missing IDL type: {name}"))["type"];
        stack.push(name.into());
        let code = match definition["kind"].as_str().expect("IDL type kind") {
            "struct" => fields(&definition["fields"], types, stack),
            "enum" => {
                let variants = definition["variants"].as_array().expect("enum variants");
                assert!(variants.len() <= 256, "IDL enum exceeds u8 tag: {name}");
                let mut code = "match u8::deserialize_reader(reader)? {".to_owned();
                for (tag, variant) in variants.iter().enumerate() {
                    code.push_str(&format!(
                        "{tag} => {{ {} }},",
                        fields(&variant["fields"], types, stack)
                    ));
                }
                code.push_str(&format!("tag => return Err(borsh::io::Error::new(borsh::io::ErrorKind::InvalidData, format!(\"unknown {name} variant tag {{tag}} in embedded aggregator IDL\"))), }}"));
                code
            }
            kind => panic!("unsupported IDL type kind {kind}: {name}"),
        };
        stack.pop();
        return code;
    }
    if let Some(element) = ty.get("vec") {
        let body = emit(element, types, stack);
        return format!(
            "{{ let count = u32::deserialize_reader(reader)?; for _ in 0..count {{ {body} }} }}"
        );
    }
    if let Some(array) = ty.get("array").and_then(Value::as_array) {
        let body = emit(&array[0], types, stack);
        let count = array[1].as_u64().expect("fixed array length");
        return format!("for _ in 0..{count} {{ {body} }}");
    }
    if let Some(element) = ty.get("option") {
        let body = emit(element, types, stack);
        return format!("if bool::deserialize_reader(reader)? {{ {body} }}");
    }
    panic!("unsupported IDL type: {ty}");
}

fn main() {
    println!("cargo:rerun-if-changed=idls/dex_solana_v3.json");
    let idl: Value =
        serde_json::from_str(&fs::read_to_string("idls/dex_solana_v3.json").unwrap()).unwrap();
    let types = idl["types"].as_array().expect("IDL types");
    let variants = types.iter().find(|t| t["name"] == "Dex").expect("Dex type")["type"]["variants"]
        .as_array()
        .expect("Dex variants");
    assert!(variants.len() <= 256, "Dex exceeds u8 tag");
    let mut code = "fn decode_idl_dex<R: Read>(reader: &mut R) -> borsh::io::Result<Dex> { let tag = u8::deserialize_reader(reader)?; match tag {".to_owned();
    let mut found_rfq = false;
    for (tag, variant) in variants.iter().enumerate() {
        let body = if variant["name"] == "SolRfqV2" {
            // RFQ values are exposed through a stable public Rust type. Fail the
            // build if upstream changes that contract rather than misreading it.
            assert_eq!(
                variant["fields"],
                json!([
                    {"name":"taker_side","type":{"defined":{"name":"RfqSide"}}},
                    {"name":"rfq_id","type":"u64"},
                    {"name":"expire_at","type":"i64"},
                    {"name":"levels","type":{"vec":{"defined":{"name":"RfqLevel"}}}}
                ]),
                "SolRfqV2 layout changed; update the public decoder types"
            );
            let side = &types
                .iter()
                .find(|t| t["name"] == "RfqSide")
                .expect("RfqSide")["type"];
            assert_eq!(
                side,
                &json!({"kind":"enum","variants":[{"name":"Bid"},{"name":"Ask"}]})
            );
            let level = &types
                .iter()
                .find(|t| t["name"] == "RfqLevel")
                .expect("RfqLevel")["type"];
            assert_eq!(
                level,
                &json!({"kind":"struct","fields":[{"name":"base_atoms","type":"u64"},{"name":"quote_atoms","type":"u64"}]})
            );
            found_rfq = true;
            "let taker_side = u8::deserialize_reader(reader)?; if taker_side > 1 { return Err(borsh::io::Error::new(borsh::io::ErrorKind::InvalidData, format!(\"invalid RfqSide tag {taker_side}\"))); } return Ok(Dex::SolRfqV2 { taker_side, rfq_id: u64::deserialize_reader(reader)?, expire_at: i64::deserialize_reader(reader)?, levels: Vec::<Level>::deserialize_reader(reader)? });".into()
        } else {
            fields(&variant["fields"], types, &mut vec!["Dex".into()])
        };
        code.push_str(&format!("{tag} => {{ {body} }},"));
    }
    assert!(found_rfq, "SolRfqV2 missing from IDL");
    code.push_str("_ => return Err(borsh::io::Error::new(borsh::io::ErrorKind::InvalidData, format!(\"unknown Dex variant tag {tag} in embedded aggregator IDL\"))), } Ok(Dex::Other(tag)) }");
    // Keep the existing public discriminator constants, but source their values
    // from the same IDL as the route layouts.
    code.push_str("pub mod entrypoint {");
    for instruction in idl["instructions"].as_array().expect("IDL instructions") {
        let name = instruction["name"]
            .as_str()
            .expect("instruction name")
            .to_ascii_uppercase();
        let bytes: Vec<u8> = serde_json::from_value(instruction["discriminator"].clone())
            .expect("discriminator bytes");
        assert_eq!(bytes.len(), 8);
        code.push_str(&format!("pub const {name}: [u8; 8] = {bytes:?};"));
    }
    code.push('}');
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("aggregator_idl.rs"),
        code,
    )
    .unwrap();
}
