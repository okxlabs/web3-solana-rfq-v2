use thiserror::Error;

#[derive(Debug, Error)]
pub enum FillDecoderError {
    #[error("input bytes are not valid base64")]
    InvalidEncoding,

    #[error("transaction wire format truncated: expected {expected} bytes, got {actual}")]
    Truncated { expected: usize, actual: usize },

    #[error("non-canonical compact-u16 encoding rejected (over-long form)")]
    NonCanonicalCompactU16,

    #[error("Borsh deserialization failed: {0}")]
    Borsh(String),

    #[error("internal: {0}")]
    Other(String),
}

impl From<std::io::Error> for FillDecoderError {
    fn from(e: std::io::Error) -> Self {
        FillDecoderError::Borsh(e.to_string())
    }
}

pub type Result<T> = core::result::Result<T, FillDecoderError>;
