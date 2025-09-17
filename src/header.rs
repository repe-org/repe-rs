use crate::constants::{HEADER_SIZE, REPE_SPEC, REPE_VERSION};
use crate::error::RepeError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub length: u64,
    pub spec: u16,
    pub version: u8,
    pub notify: u8,
    pub reserved: u32,
    pub id: u64,
    pub query_length: u64,
    pub body_length: u64,
    pub query_format: u16,
    pub body_format: u16,
    pub ec: u32,
}

impl Header {
    pub fn new() -> Self {
        Self {
            spec: REPE_SPEC,
            version: REPE_VERSION,
            ..Default::default()
        }
    }

    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        let mut o = 0;

        buf[o..o + 8].copy_from_slice(&self.length.to_le_bytes());
        o += 8;
        buf[o..o + 2].copy_from_slice(&self.spec.to_le_bytes());
        o += 2;
        buf[o] = self.version;
        o += 1;
        buf[o] = self.notify;
        o += 1;
        buf[o..o + 4].copy_from_slice(&self.reserved.to_le_bytes());
        o += 4;
        buf[o..o + 8].copy_from_slice(&self.id.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.query_length.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.body_length.to_le_bytes());
        o += 8;
        buf[o..o + 2].copy_from_slice(&self.query_format.to_le_bytes());
        o += 2;
        buf[o..o + 2].copy_from_slice(&self.body_format.to_le_bytes());
        o += 2;
        buf[o..o + 4].copy_from_slice(&self.ec.to_le_bytes());

        buf
    }

    pub fn decode(input: &[u8]) -> Result<Self, RepeError> {
        if input.len() < HEADER_SIZE {
            return Err(RepeError::InvalidHeaderLength(input.len()));
        }
        let mut o = 0;

        let length = u64::from_le_bytes(input[o..o + 8].try_into().unwrap());
        o += 8;
        let spec = u16::from_le_bytes(input[o..o + 2].try_into().unwrap());
        o += 2;
        let version = input[o];
        o += 1;
        let notify = input[o];
        o += 1;
        let reserved = u32::from_le_bytes(input[o..o + 4].try_into().unwrap());
        o += 4;
        let id = u64::from_le_bytes(input[o..o + 8].try_into().unwrap());
        o += 8;
        let query_length = u64::from_le_bytes(input[o..o + 8].try_into().unwrap());
        o += 8;
        let body_length = u64::from_le_bytes(input[o..o + 8].try_into().unwrap());
        o += 8;
        let query_format = u16::from_le_bytes(input[o..o + 2].try_into().unwrap());
        o += 2;
        let body_format = u16::from_le_bytes(input[o..o + 2].try_into().unwrap());
        o += 2;
        let ec = u32::from_le_bytes(input[o..o + 4].try_into().unwrap());

        if spec != REPE_SPEC {
            return Err(RepeError::InvalidSpec(spec));
        }

        if reserved != 0 {
            return Err(RepeError::ReservedNonZero);
        }

        let expected = HEADER_SIZE as u64 + query_length + body_length;
        if length != expected {
            return Err(RepeError::LengthMismatch {
                expected,
                got: length,
            });
        }

        Ok(Self {
            length,
            spec,
            version,
            notify,
            reserved,
            id,
            query_length,
            body_length,
            query_format,
            body_format,
            ec,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_invalid_spec_and_reserved_non_zero() {
        let mut header = Header::new();
        header.length = HEADER_SIZE as u64;
        let mut bytes = header.encode();

        // Invalid spec magic
        bytes[8..10].copy_from_slice(&0u16.to_le_bytes());
        match Header::decode(&bytes).unwrap_err() {
            RepeError::InvalidSpec(0) => {}
            other => panic!("unexpected error: {other:?}"),
        }

        // Reserved field must be zero
        let mut bytes = header.encode();
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        match Header::decode(&bytes).unwrap_err() {
            RepeError::ReservedNonZero => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn decode_detects_length_mismatch() {
        let mut header = Header::new();
        header.query_length = 4;
        header.body_length = 0;
        header.length = HEADER_SIZE as u64 + header.query_length + header.body_length;
        let mut bytes = header.encode();

        // Corrupt the length field (first 8 bytes)
        bytes[0..8].copy_from_slice(&(HEADER_SIZE as u64).to_le_bytes());
        match Header::decode(&bytes).unwrap_err() {
            RepeError::LengthMismatch { expected, got } => {
                assert_eq!(expected, HEADER_SIZE as u64 + 4);
                assert_eq!(got, HEADER_SIZE as u64);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
