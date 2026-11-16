//! Deterministic, offline code generation for the pinned aggregator IDL.
use heck::{ToShoutySnakeCase, ToSnakeCase};
use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

pub const GENERATED_PATH: &str = "src/generated/aggregator_idl.rs";

#[cfg(test)]
mod tests;

// Signing policy is reviewed here, not inferred from an instruction's name.
// The argument type is checked too: a supported instruction must start with it.
const SUPPORTED: &[(&str, &str)] = &[
    ("swap", "SwapArgs"),
    ("proxy_swap", "SwapArgs"),
    ("swap_toc", "SwapArgs"),
    ("swap_toc_v2", "SwapArgs"),
    ("swap_toc_v3", "SwapArgs"),
];
const REJECTED: &[(&str, &str)] = &[
    ("swap_tob", "SwapArgs"),
    ("swap_tob_v2", "SwapArgs"),
    ("swap_tob_v3", "SwapArgs"),
    ("swap_tob_with_receiver", "SwapArgs"),
    ("swap_tob_with_receiver_v3", "SwapArgs"),
    ("swap_tob_enhanced", "SwapArgs"),
    ("swap_tob_with_token_ledger", "SwapArgsTokenLedger"),
    ("swap_tob_with_token_ledger_v3", "SwapArgsTokenLedger"),
    ("swap_tob_with_receiver_token_ledger", "SwapArgsTokenLedger"),
    (
        "swap_tob_with_receiver_token_ledger_v3",
        "SwapArgsTokenLedger",
    ),
];

fn strip_docs(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("docs");
            for child in object.values_mut() {
                strip_docs(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                strip_docs(child);
            }
        }
        _ => {}
    }
}

fn definition<'a>(types: &'a [Value], name: &str) -> &'a Value {
    &types
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("missing IDL type: {name}"))["type"]
}

fn check_layouts(types: &[Value]) {
    for (name, expected) in [
        (
            "SwapArgs",
            json!({"kind":"struct","fields":[
                {"name":"order_id","type":"u64"},
                {"name":"amount_in","type":"u64"},
                {"name":"expect_amount_out","type":"u64"},
                {"name":"slippage","type":"u16"},
                {"name":"routes","type":{"vec":{"defined":{"name":"Route"}}}}
            ]}),
        ),
        (
            "SwapArgsTokenLedger",
            json!({"kind":"struct","fields":[
                {"name":"order_id","type":"u64"},
                {"name":"expect_amount_out","type":"u64"},
                {"name":"slippage","type":"u16"},
                {"name":"routes","type":{"vec":{"defined":{"name":"Route"}}}}
            ]}),
        ),
        (
            "Route",
            json!({"kind":"struct","fields":[
                {"name":"dex","type":{"defined":{"name":"Dex"}}},
                {"name":"weight","type":"u16"},
                {"name":"index","type":"u8"}
            ]}),
        ),
        (
            "RfqSide",
            json!({"kind":"enum","variants":[{"name":"Bid"},{"name":"Ask"}]}),
        ),
        (
            "RfqLevel",
            json!({"kind":"struct","fields":[
                {"name":"base_atoms","type":"u64"},
                {"name":"quote_atoms","type":"u64"}
            ]}),
        ),
    ] {
        assert_eq!(
            definition(types, name),
            &expected,
            "{name} layout changed; update idl_types.rs before rebuilding"
        );
    }
    let variants = definition(types, "Dex")["variants"]
        .as_array()
        .expect("Dex variants");
    let rfq = variants
        .iter()
        .find(|v| v["name"] == "SolRfqV2")
        .expect("SolRfqV2 missing from IDL");
    assert_eq!(
        rfq["fields"],
        json!([
            {"name":"taker_side","type":{"defined":{"name":"RfqSide"}}},
            {"name":"rfq_id","type":"u64"},
            {"name":"expire_at","type":"i64"},
            {"name":"levels","type":{"vec":{"defined":{"name":"RfqLevel"}}}}
        ]),
        "SolRfqV2 layout changed; update the public decoder types"
    );
}

fn skip_name(name: &str) -> Ident {
    format_ident!("skip_{}", name.to_snake_case())
}

struct Readers<'a> {
    types: &'a [Value],
    // Each referenced definition gets one function, even when used more than once.
    required: BTreeSet<String>,
}

impl Readers<'_> {
    fn fields(&mut self, fields: &Value) -> TokenStream {
        fields.as_array().map_or_else(TokenStream::new, |fields| {
            fields
                .iter()
                .map(|field| {
                    // Anchor enum fields can be named or tuple fields.
                    self.ty(field.get("type").unwrap_or(field))
                })
                .collect()
        })
    }

    fn ty(&mut self, ty: &Value) -> TokenStream {
        if let Some(name) = ty.as_str() {
            return match name {
                "bool" => quote! { let _ = bool::deserialize_reader(reader)?; },
                "u8" | "i8" => quote! { skip_exact(reader, 1)?; },
                "u16" | "i16" => quote! { skip_exact(reader, 2)?; },
                "u32" | "i32" | "f32" => quote! { skip_exact(reader, 4)?; },
                "u64" | "i64" | "f64" => quote! { skip_exact(reader, 8)?; },
                "u128" | "i128" => quote! { skip_exact(reader, 16)?; },
                "pubkey" => quote! { skip_exact(reader, 32)?; },
                "bytes" | "string" => quote! {
                    let len = u32::deserialize_reader(reader)? as usize;
                    skip_exact(reader, len)?;
                },
                _ => panic!("unsupported IDL primitive: {name}"),
            };
        }
        if let Some(defined) = ty.get("defined") {
            let name = defined["name"].as_str().expect("defined type name");
            self.required.insert(name.into());
            let function = skip_name(name);
            return quote! { #function(reader)?; };
        }
        if let Some(element) = ty.get("vec") {
            let body = self.ty(element);
            return quote! {{
                let count = u32::deserialize_reader(reader)?;
                for _ in 0..count { #body }
            }};
        }
        if let Some(array) = ty.get("array").and_then(Value::as_array) {
            let body = self.ty(&array[0]);
            let count = array[1].as_u64().expect("fixed array length");
            if matches!(array[0].as_str(), Some("u8" | "i8")) {
                let count = usize::try_from(count).expect("array length fits usize");
                return quote! { skip_exact(reader, #count)?; };
            }
            return quote! { for _ in 0..#count { #body } };
        }
        if let Some(element) = ty.get("option") {
            let body = self.ty(element);
            return quote! { if bool::deserialize_reader(reader)? { #body } };
        }
        panic!("unsupported IDL type: {ty}");
    }

    fn body(&mut self, name: &str) -> TokenStream {
        let definition = definition(self.types, name);
        match definition["kind"].as_str().expect("IDL type kind") {
            "struct" => self.fields(&definition["fields"]),
            "enum" => {
                let variants = definition["variants"].as_array().expect("enum variants");
                assert!(variants.len() <= 256, "IDL enum exceeds u8 tag: {name}");
                let arms: Vec<_> = variants
                    .iter()
                    .enumerate()
                    .map(|(tag, variant)| {
                        let tag = tag as u8;
                        let body = self.fields(&variant["fields"]);
                        quote! { #tag => { #body } }
                    })
                    .collect();
                quote! {
                    match u8::deserialize_reader(reader)? {
                        #(#arms,)*
                        tag => return Err(borsh::io::Error::new(
                            borsh::io::ErrorKind::InvalidData,
                            format!("unknown {} variant tag {tag} in embedded aggregator IDL", #name),
                        )),
                    }
                }
            }
            kind => panic!("unsupported IDL type kind {kind}: {name}"),
        }
    }

    fn functions(&mut self) -> TokenStream {
        let mut functions = BTreeMap::new();
        while let Some(name) = self
            .required
            .iter()
            .find(|n| !functions.contains_key(*n))
            .cloned()
        {
            let body = self.body(&name);
            let function = skip_name(&name);
            functions.insert(
                name,
                quote! {
                    fn #function<R: Read>(reader: &mut R) -> borsh::io::Result<()> {
                        #body
                        Ok(())
                    }
                },
            );
        }
        functions.into_values().collect()
    }
}

// Follow argument types and their named definitions. A wrapper around SwapArgs
// still needs an explicit policy, even though current supported entries use it directly.
fn has_swap_args(value: &Value, types: &[Value]) -> bool {
    fn visit(value: &Value, types: &[Value], seen: &mut BTreeSet<String>) -> bool {
        if let Some(name) = value
            .get("defined")
            .and_then(|d| d.get("name"))
            .and_then(Value::as_str)
        {
            if matches!(name, "SwapArgs" | "SwapArgsTokenLedger") {
                return true;
            }
            if !seen.insert(name.into()) {
                return false;
            }
            return visit(definition(types, name), types, seen);
        }
        match value {
            Value::Object(object) => object.values().any(|v| visit(v, types, seen)),
            Value::Array(array) => array.iter().any(|v| visit(v, types, seen)),
            _ => false,
        }
    }
    visit(value, types, &mut BTreeSet::new())
}

fn entrypoints(instructions: &[Value], types: &[Value]) -> TokenStream {
    // Check the policy in both directions so a renamed/removed instruction
    // cannot silently leave a stale policy entry behind.
    for &(name, _) in SUPPORTED.iter().chain(REJECTED) {
        assert!(
            instructions.iter().any(|i| i["name"] == name),
            "entrypoint policy references missing instruction: {name}"
        );
    }
    let mut constants = TokenStream::new();
    let mut arms = TokenStream::new();
    let mut discriminators = BTreeSet::new();
    for instruction in instructions {
        let name = instruction["name"].as_str().expect("instruction name");
        let constant = format_ident!("{}", name.to_shouty_snake_case());
        let bytes: Vec<u8> = serde_json::from_value(instruction["discriminator"].clone())
            .expect("discriminator bytes");
        assert_eq!(bytes.len(), 8, "invalid discriminator for {name}");
        assert!(
            discriminators.insert(bytes.clone()),
            "duplicate discriminator: {name}"
        );
        constants.extend(quote! { pub const #constant: [u8; 8] = [#(#bytes),*]; });
        let supported = SUPPORTED.iter().find(|&&(n, _)| n == name);
        let rejected = REJECTED.iter().find(|&&(n, _)| n == name);
        assert!(
            !(supported.is_some() && rejected.is_some()),
            "entrypoint has conflicting support/reject policies: {name}"
        );
        let policy = supported.or(rejected);
        if has_swap_args(&instruction["args"], types) {
            assert!(
                policy.is_some(),
                "unclassified swap entrypoint: {name}; add it to SUPPORTED or REJECTED"
            );
        }
        if let Some(&(_, args)) = policy {
            let parameters = instruction["args"].as_array().expect("instruction args");
            assert_eq!(
                parameters.first().map(|a| &a["type"]),
                Some(&json!({"defined":{"name":args}})),
                "{name} must start with {args}; review entrypoint policy"
            );
            assert_eq!(
                parameters
                    .iter()
                    .filter(|a| has_swap_args(&a["type"], types))
                    .count(),
                1,
                "{name} must contain exactly one swap argument"
            );
            let body = if supported.is_some() {
                let ty = format_ident!("{args}");
                let variant = match args {
                    "SwapArgs" => format_ident!("Concrete"),
                    "SwapArgsTokenLedger" => format_ident!("TokenLedger"),
                    _ => unreachable!("unknown swap argument in policy"),
                };
                quote! {
                    let mut reader = std::io::Cursor::new(&data[8..]);
                    #ty::deserialize_reader(&mut reader)
                        .map(|args| Some(AnySwapArgs::#variant(args)))
                }
            } else {
                quote! { Ok(Some(AnySwapArgs::Unsupported)) }
            };
            arms.extend(quote! { entrypoint::#constant => { #body }, });
        }
    }
    quote! {
        pub mod entrypoint { #constants }
        fn decode_idl_swap_args(data: &[u8]) -> borsh::io::Result<Option<AnySwapArgs>> {
            if data.len() < 8 { return Ok(None); }
            let mut disc = [0u8; 8];
            disc.copy_from_slice(&data[..8]);
            match disc {
                #arms
                _ => Ok(None),
            }
        }
    }
}

fn format_rust(source: &str) -> String {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("rustfmt.toml");
    let mut child = Command::new("rustfmt")
        .args(["--emit", "stdout", "--config-path"])
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rustfmt is required; install it with `rustup component add rustfmt`");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .expect("write generated source to rustfmt");
    let output = child.wait_with_output().expect("wait for rustfmt");
    assert!(
        output.status.success(),
        "rustfmt failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("rustfmt output must be UTF-8")
}

/// Generate the checked-in source. Layout or entrypoint policy drift is an
/// error at generation time and at build time. Documentation is ignored.
pub fn generate(idl: &str) -> String {
    let mut idl: Value = serde_json::from_str(idl).expect("valid aggregator IDL JSON");
    strip_docs(&mut idl);
    let types = idl["types"].as_array().expect("IDL types");
    check_layouts(types);
    let entrypoints = entrypoints(
        idl["instructions"].as_array().expect("IDL instructions"),
        types,
    );
    let variants = definition(types, "Dex")["variants"]
        .as_array()
        .expect("Dex variants");
    assert!(variants.len() <= 256, "Dex exceeds u8 tag");
    let mut readers = Readers {
        types,
        required: BTreeSet::new(),
    };
    let arms: Vec<_> = variants
        .iter()
        .enumerate()
        .map(|(tag, variant)| {
            let tag = tag as u8;
            let body = if variant["name"] == "SolRfqV2" {
                quote! {
                    let taker_side = u8::deserialize_reader(reader)?;
                    if taker_side > 1 {
                        return Err(borsh::io::Error::new(
                            borsh::io::ErrorKind::InvalidData,
                            format!("invalid RfqSide tag {taker_side}"),
                        ));
                    }
                    return Ok(Dex::SolRfqV2 {
                        taker_side,
                        rfq_id: u64::deserialize_reader(reader)?,
                        expire_at: i64::deserialize_reader(reader)?,
                        levels: Vec::<Level>::deserialize_reader(reader)?,
                    });
                }
            } else {
                readers.fields(&variant["fields"])
            };
            quote! { #tag => { #body } }
        })
        .collect();
    let helpers = readers.functions();
    let tokens = quote! {
        fn decode_idl_dex<R: Read>(reader: &mut R) -> borsh::io::Result<Dex> {
            let tag = u8::deserialize_reader(reader)?;
            match tag {
                #(#arms,)*
                _ => return Err(borsh::io::Error::new(
                    borsh::io::ErrorKind::InvalidData,
                    format!("unknown Dex variant tag {tag} in embedded aggregator IDL"),
                )),
            }
            Ok(Dex::Other(tag))
        }
        #helpers
        #entrypoints
    };
    syn::parse2::<syn::File>(tokens.clone()).expect("generated aggregator Rust must be valid");
    let mut source = String::from(
        "// @generated by `cargo run -p fill-decoder-codegen`. Do not edit by hand.\n",
    );
    source.push_str(&tokens.to_string());
    format_rust(&source)
}
