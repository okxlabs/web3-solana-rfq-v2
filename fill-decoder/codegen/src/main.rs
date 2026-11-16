use fill_decoder_codegen::{generate, GENERATED_PATH};
use std::{env, fs, path::Path};

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    assert!(
        args.is_empty() || args == ["--check"],
        "usage: cargo run -p fill-decoder-codegen -- [--check]"
    );
    let package = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let source = generate(&fs::read_to_string(package.join("idls/dex_solana_v3.json")).unwrap());
    let output = package.join(GENERATED_PATH);
    if args.is_empty() {
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::write(&output, source).unwrap();
        println!("Generated {}", output.display());
    } else {
        assert_eq!(
            fs::read_to_string(&output).unwrap(),
            source,
            "generated source is stale; run `cargo run -p fill-decoder-codegen`"
        );
        println!("{} is up to date", output.display());
    }
}
