use fill_decoder::{
    decode_message_bytes, decode_transaction_base64, decode_transaction_bytes,
    idl_types::{decode_swap_args, entrypoint, AnySwapArgs},
    FillDecoderError, Side, DEX_SOLANA_V3_PROGRAM_ID_BYTES,
};

#[test]
fn original_pumpswap_rfq_jupiter_v1_transaction_decodes() {
    let tx =
        decode_transaction_base64(include_str!("fixtures/pumpswap_rfq_jupiter_v1.b64")).unwrap();
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

fn swap_with_dex(tag: u8) -> Vec<u8> {
    let mut data = entrypoint::SWAP.to_vec();
    data.extend_from_slice(&[0; 8 * 3 + 2]); // order_id, amount_in, expect_amount_out, slippage
    data.extend_from_slice(&1u32.to_le_bytes()); // routes length
    data.push(tag);
    data.extend_from_slice(&10_000u16.to_le_bytes());
    data.push(1); // routing index
    data
}

// A small legacy message with one aggregator instruction. No raw fixture
// offsets or discriminator scanning are needed to test error propagation.
fn message_with_swap(data: &[u8]) -> Vec<u8> {
    assert!(data.len() < 128); // single-byte compact-u16 length in this test
    let mut message = vec![0, 0, 1, 1]; // signer header, static key count
    message.extend_from_slice(&DEX_SOLANA_V3_PROGRAM_ID_BYTES);
    message.extend_from_slice(&[0; 32]); // blockhash
    message.extend_from_slice(&[1, 0, 0, data.len() as u8]); // ix count, program, accounts, data length
    message.extend_from_slice(data);
    message
}

#[test]
fn malformed_swap_preserves_instruction_and_dex_error() {
    let message = message_with_swap(&swap_with_dex(255));
    let error = decode_message_bytes(&message).unwrap_err();
    assert!(matches!(error, FillDecoderError::Borsh(_)));
    let text = error.to_string();
    assert!(text.contains("aggregator instruction 0 decode failed:"));
    assert!(text.contains("Dex variant tag 255"));
    assert!(!text.contains("no SolRfqV2 leg"));
    let mut transaction = vec![0]; // no signatures, matching the message header
    transaction.extend_from_slice(&message);
    assert_eq!(
        decode_transaction_bytes(&transaction)
            .unwrap_err()
            .to_string(),
        text
    );
}

#[test]
fn valid_swap_without_rfq_still_returns_not_found() {
    let mut transaction = vec![0];
    transaction.extend_from_slice(&message_with_swap(&swap_with_dex(146)));
    let tx = decode_transaction_bytes(&transaction).unwrap();
    assert_eq!(
        tx.single_fill().unwrap_err().to_string(),
        "no SolRfqV2 leg found in transaction"
    );
}

#[test]
fn public_option_signature_and_exhaustive_error_match_are_unchanged() {
    let decode: fn(&[u8]) -> Option<AnySwapArgs> = decode_swap_args;
    assert!(decode(&swap_with_dex(255)).is_none());
    assert!(matches!(
        decode(&swap_with_dex(146)),
        Some(AnySwapArgs::Concrete(_))
    ));
    // External-crate match deliberately has no wildcard arm.
    let kind = match FillDecoderError::Borsh("test".into()) {
        FillDecoderError::InvalidEncoding => "encoding",
        FillDecoderError::Truncated { .. } => "truncated",
        FillDecoderError::NonCanonicalCompactU16 => "compact-u16",
        FillDecoderError::Borsh(_) => "borsh",
        FillDecoderError::Other(_) => "other",
    };
    assert_eq!(kind, "borsh");
}
