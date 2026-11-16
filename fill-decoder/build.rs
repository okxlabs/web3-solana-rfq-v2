//! Check the pinned IDL and the reviewed, checked-in decoder before compiling.
use fill_decoder_codegen::{generate, GENERATED_PATH};
use std::{fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=idls/dex_solana_v3.json");
    println!("cargo:rerun-if-changed=codegen/rustfmt.toml");
    println!("cargo:rerun-if-changed={GENERATED_PATH}");
    let package = Path::new(env!("CARGO_MANIFEST_DIR"));
    // generate() also checks handwritten layouts and explicit entrypoint policy.
    let expected = generate(&fs::read_to_string(package.join("idls/dex_solana_v3.json")).unwrap());
    let actual = fs::read_to_string(package.join(GENERATED_PATH))
        .expect("missing generated decoder; run `cargo run -p fill-decoder-codegen`");
    assert!(actual == expected,
        "generated decoder is stale; run `cargo run -p fill-decoder-codegen`, review and commit its diff");
}
