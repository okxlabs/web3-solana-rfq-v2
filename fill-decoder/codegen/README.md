# Aggregator decoder code generation

Generation, consistency tests, and SDK builds require `rustfmt`:

```sh
rustup component add rustfmt
```

Use the same Rust toolchain version locally and in CI to keep formatting
consistent. The generator uses `codegen/rustfmt.toml`, not a configuration
inherited from the caller's working directory.

Run from the repository root after updating the pinned
`fill-decoder/idls/dex_solana_v3.json`:

```sh
cargo run -p fill-decoder-codegen
cargo run -p fill-decoder-codegen -- --check
cargo test -p fill-decoder-codegen
cargo test -p fill-decoder --all-features
```

Review and commit both the IDL and
`fill-decoder/src/generated/aggregator_idl.rs`. Do not edit the generated file
by hand. Code generation is offline and deterministic. Builds check that this
file matches the IDL and generator; they do not write into the source tree.

The generator uses `quote!` for Rust tokens, `syn` for syntax checking, and
`rustfmt` for formatting. Each referenced named IDL type gets one reader
function. Non-RFQ Dex variants consume their bodies without retaining values.

`SUPPORTED` and `REJECTED` in `src/lib.rs` define the reviewed entrypoint
policy. Every instruction carrying `SwapArgs` or `SwapArgsTokenLedger`,
including through a wrapper type, must have a policy entry. Missing policy
entries, removed instructions, and changes to the expected leading argument
fail both generation and builds. The runtime discriminator dispatch is generated
from the same policy; there is no second hand-maintained match list.

The handwritten `SwapArgs`, `SwapArgsTokenLedger`, `Route`, `SolRfqV2`,
`RfqSide`, and `RfqLevel` layouts are checked before generation. Field order,
field types, and enum order must match. Documentation fields are ignored.
Unknown IDL type forms fail generation rather than assuming a body length.

Unknown runtime Dex tags still fail decoding. Updating the checked-in source
requires a new decoder build and release; old binaries cannot learn new wire
layouts automatically. None of these checks verify a deployed Router version,
transaction signatures, account state, or execution results.
