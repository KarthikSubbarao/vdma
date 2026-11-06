//! The transport reference a client attaches to each `DMA.SET` and `DMA.GET` so the server can RMA
//! against its exposed buffer. Fabric address, the buffer's remote key, and, on
//! `FI_MR_VIRT_ADDR` providers like efa, its virtual address. Shared by the client and the module so
//! both agree on the encoding. Carried as each command's leading three arguments.

/// Where a client's exposed RMA buffer lives, encoded for the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertisement {
    /// The remote network address for RDMA operation
    pub address: Vec<u8>,
    /// The rkey for the remote memory region
    pub remote_key: u64,
    /// The exposed buffer's virtual address on `FI_MR_VIRT_ADDR` providers like efa, or 0 where
    /// addressing is by offset, like tcp.
    pub remote_address: u64,
}

impl Advertisement {
    /// Command arguments an advertisement occupies.
    pub const ARG_COUNT: usize = 3;

    /// Parse from the three advertisement fields.
    pub fn from_fields(fields: &[&[u8]]) -> Result<Self, AdvertisementError> {
        let [address, remote_key, remote_address] = fields else {
            return Err(AdvertisementError::Arity);
        };
        Ok(Self {
            address: decode_hex(address)?,
            remote_key: parse_ascii(remote_key).ok_or(AdvertisementError::Integer)?,
            remote_address: parse_ascii(remote_address).ok_or(AdvertisementError::Integer)?,
        })
    }
}

/// Errors decoding a transport advertisement.
#[derive(Debug, thiserror::Error)]
pub enum AdvertisementError {
    /// Wrong argument count
    #[error("expected: address rkey remote-address")]
    Arity,
    /// Invalid hex
    #[error("invalid hex in address")]
    Hex,
    /// Invalid integer
    #[error("invalid integer in advertisement")]
    Integer,
}

/// Decode a decimal RESP argument. The single implementation for every numeric field on the wire —
/// advertisement, length, checksum — each caller supplying its own error.
pub fn parse_ascii<T: std::str::FromStr>(raw: &[u8]) -> Option<T> {
    std::str::from_utf8(raw)
        .ok()
        .and_then(|text| text.parse().ok())
}

/// Hex-encode opaque bytes for a RESP argument or reply. Shared, so the advertisement and the
/// `dma.hello` address exchange use one encoding.
pub fn encode_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        output.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    output
}

/// Decode an [`encode_hex`] string back to bytes.
pub fn decode_hex(text: &[u8]) -> Result<Vec<u8>, AdvertisementError> {
    if !text.len().is_multiple_of(2) {
        return Err(AdvertisementError::Hex);
    }
    text.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = (pair[0] as char)
                .to_digit(16)
                .ok_or(AdvertisementError::Hex)?;
            let low = (pair[1] as char)
                .to_digit(16)
                .ok_or(AdvertisementError::Hex)?;
            Ok((high << 4 | low) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{Advertisement, AdvertisementError};

    /// The exact bytes a client sends, parsed the way `DMA.SET`/`DMA.GET` parse them.
    #[test]
    fn parses_the_wire_fields() {
        let decoded = Advertisement::from_fields(&[b"01007fffab", b"42", b"139638282147448"])
            .expect("decodes");
        assert_eq!(
            decoded,
            Advertisement {
                address: vec![0x01, 0x00, 0x7f, 0xff, 0xab],
                remote_key: 42,
                remote_address: 0x7f00_1234_5678,
            }
        );
    }

    #[test]
    fn rejects_malformed_fields() {
        assert!(matches!(
            Advertisement::from_fields(&[b"0100", b"1"]),
            Err(AdvertisementError::Arity)
        ));
        assert!(matches!(
            Advertisement::from_fields(&[b"abc", b"1", b"0"]),
            Err(AdvertisementError::Hex)
        ));
        assert!(matches!(
            Advertisement::from_fields(&[b"0100", b"nope", b"0"]),
            Err(AdvertisementError::Integer)
        ));
    }
}
