use super::*;

const IDL: &str = include_str!("../../idls/dex_solana_v3.json");
const CHECKED_IN: &str = include_str!("../../src/generated/aggregator_idl.rs");

fn idl() -> Value {
    serde_json::from_str(IDL).unwrap()
}

fn generate_value(idl: &Value) -> String {
    generate(&idl.to_string())
}

fn assert_rejected(idl: &Value, expected: &str) {
    let error = std::panic::catch_unwind(|| generate_value(idl)).unwrap_err();
    let message = error
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| error.downcast_ref::<&str>().copied())
        .unwrap();
    assert!(message.contains(expected), "{message}");
}

#[test]
fn checked_in_source_matches_regeneration() {
    assert_eq!(
        generate(IDL),
        CHECKED_IN,
        "run `cargo run -p fill-decoder-codegen` and commit the generated diff"
    );
}

#[test]
fn documentation_changes_do_not_change_generated_source() {
    fn document(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for child in object.values_mut() {
                    document(child);
                }
                object.insert("docs".into(), json!(["Documentation-only change"]));
            }
            Value::Array(array) => {
                for child in array {
                    document(child);
                }
            }
            _ => {}
        }
    }
    let mut idl = idl();
    document(&mut idl);
    assert_eq!(generate_value(&idl), CHECKED_IN);
}

#[test]
fn new_swap_entrypoints_require_an_explicit_policy() {
    for args in ["SwapArgs", "SwapArgsTokenLedger"] {
        let mut idl = idl();
        idl["instructions"].as_array_mut().unwrap().push(json!({
            "name":"swap_toc_v4", "discriminator":[255,254,253,252,251,250,249,248],
            "args":[{"name":"args","type":{"defined":{"name":args}}}]
        }));
        assert_rejected(&idl, "unclassified swap entrypoint: swap_toc_v4");
    }
}

#[test]
fn new_entrypoint_with_a_named_swap_wrapper_requires_policy() {
    let mut idl = idl();
    idl["types"].as_array_mut().unwrap().push(json!({
        "name":"SwapWrapper", "type":{"kind":"struct","fields":[
            {"name":"args","type":{"defined":{"name":"SwapArgs"}}}
        ]}
    }));
    idl["instructions"].as_array_mut().unwrap().push(json!({
        "name":"swap_toc_v4", "discriminator":[255,254,253,252,251,250,249,248],
        "args":[{"name":"wrapper","type":{"defined":{"name":"SwapWrapper"}}}]
    }));
    assert_rejected(&idl, "unclassified swap entrypoint: swap_toc_v4");
}

#[test]
fn moving_the_supported_swap_argument_behind_a_prefix_is_rejected() {
    let mut idl = idl();
    let swap = idl["instructions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|i| i["name"] == "swap")
        .unwrap();
    swap["args"]
        .as_array_mut()
        .unwrap()
        .insert(0, json!({"name":"prefix","type":"u64"}));
    assert_rejected(&idl, "swap must start with SwapArgs");
}

#[test]
fn removed_policy_entrypoints_are_rejected() {
    let mut idl = idl();
    idl["instructions"]
        .as_array_mut()
        .unwrap()
        .retain(|i| i["name"] != "swap_toc_v3");
    assert_rejected(
        &idl,
        "entrypoint policy references missing instruction: swap_toc_v3",
    );
}

#[test]
fn handwritten_layout_changes_are_rejected() {
    for name in [
        "SwapArgs",
        "SwapArgsTokenLedger",
        "Route",
        "RfqSide",
        "RfqLevel",
        "SolRfqV2",
    ] {
        let mut idl = idl();
        let types = idl["types"].as_array_mut().unwrap();
        if name == "SolRfqV2" {
            let dex = types.iter_mut().find(|t| t["name"] == "Dex").unwrap();
            let rfq = dex["type"]["variants"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|v| v["name"] == name)
                .unwrap();
            rfq["fields"][1]["type"] = json!("u32");
        } else {
            let ty = &mut types.iter_mut().find(|t| t["name"] == name).unwrap()["type"];
            if name == "RfqSide" {
                ty["variants"].as_array_mut().unwrap().reverse();
            } else {
                ty["fields"][0]["type"] = json!("u128");
            }
        }
        assert_rejected(&idl, &format!("{name} layout changed"));
    }
}

#[test]
fn referenced_types_get_one_reader_function_each() {
    let mut idl = idl();
    let dex = idl["types"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|t| t["name"] == "Dex")
        .unwrap();
    dex["type"]["variants"].as_array_mut().unwrap().push(json!({
        "name":"TestRepeatedType", "fields":[
            {"defined":{"name":"DynamicRouteSpec"}},
            {"defined":{"name":"DynamicRouteSpec"}}
        ]
    }));
    let source = generate_value(&idl);
    assert_eq!(source.matches("fn skip_dynamic_route_spec<").count(), 1);
    assert_eq!(source.matches("fn skip_dynamic_candidate<").count(), 1);
    assert_eq!(
        source.matches("skip_dynamic_route_spec(reader)?;").count(),
        3
    );
    assert!(source.contains("153u8 =>"));
}
