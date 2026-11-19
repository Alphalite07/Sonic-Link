use reedsolomon::ReedSolomon;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FecError {
    #[error("Reed-Solomon encoding failed: {0}")]
    Encode(String),
    #[error("Too much acoustic noise — packet uncorrectable: {0}")]
    Uncorrectable(String),
}

/// Wraps encrypted bytes in CCSDS RS(255, 223) framed error correction.
/// Can repair up to 16 completely corrupted bytes per 255-byte block.
pub fn fec_encode(encrypted_payload: &[u8]) -> Result<Vec<u8>, FecError> {
    let rs = ReedSolomon::default();
    rs.encode_framed(encrypted_payload)
        .map_err(|e| FecError::Encode(format!("{e:?}")))
}

/// Repairs acoustic bit/byte errors and strips FEC framing.
pub fn fec_decode(received_bytes: &[u8]) -> Result<Vec<u8>, FecError> {
    let rs = ReedSolomon::default();
    rs.decode_framed(received_bytes)
        .map_err(|e| FecError::Uncorrectable(format!("{e:?}")))
}