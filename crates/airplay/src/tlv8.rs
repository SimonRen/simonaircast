//! TLV8 encoding used by HomeKit pairing messages.
//!
//! Each item is `tag (1 byte) | length (1 byte) | value`. Values longer than
//! 255 bytes are split into consecutive items with the same tag.

use std::collections::BTreeMap;

use crate::{Error, Result};

pub mod tag {
    pub const METHOD: u8 = 0x00;
    pub const SALT: u8 = 0x02;
    pub const PUBLIC_KEY: u8 = 0x03;
    pub const PROOF: u8 = 0x04;
    pub const SEQ_NO: u8 = 0x06;
    pub const ERROR: u8 = 0x07;
    pub const FLAGS: u8 = 0x13;
}

const MAX_TLV_BYTES: usize = 64 * 1024;

pub fn encode(items: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(tag, value) in items {
        if value.is_empty() {
            out.extend([tag, 0]);
            continue;
        }
        for chunk in value.chunks(255) {
            out.push(tag);
            out.push(chunk.len() as u8);
            out.extend_from_slice(chunk);
        }
    }
    out
}

/// Decodes into tag -> value, concatenating split values.
pub fn decode(mut data: &[u8]) -> Result<BTreeMap<u8, Vec<u8>>> {
    if data.len() > MAX_TLV_BYTES {
        return Err(Error::Protocol("TLV8 message too large".into()));
    }
    let mut items: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
    while !data.is_empty() {
        let [tag, len, rest @ ..] = data else {
            return Err(Error::Protocol("truncated TLV8 header".into()));
        };
        let len = *len as usize;
        if rest.len() < len {
            return Err(Error::Protocol("truncated TLV8 value".into()));
        }
        items
            .entry(*tag)
            .or_default()
            .extend_from_slice(&rest[..len]);
        data = &rest[len..];
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_pair_setup_m1_like_pyatv() {
        // Bytes pyatv sent to the HomePod for transient pair-setup M1.
        let m1 = encode(&[
            (tag::METHOD, &[0]),
            (tag::SEQ_NO, &[1]),
            (tag::FLAGS, &[0x10]),
        ]);
        assert_eq!(m1, [0x00, 0x01, 0x00, 0x06, 0x01, 0x01, 0x13, 0x01, 0x10]);
    }

    #[test]
    fn long_values_round_trip_through_split_items() {
        let key = vec![0xAB; 384];
        let encoded = encode(&[(tag::SEQ_NO, &[3]), (tag::PUBLIC_KEY, &key)]);
        assert_eq!(encoded.len(), 3 + 2 + 255 + 2 + 129);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded[&tag::PUBLIC_KEY], key);
        assert_eq!(decoded[&tag::SEQ_NO], [3]);
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(decode(&[0x06, 0x02, 0x01]).is_err());
        assert!(decode(&[0x06]).is_err());
    }

    #[test]
    fn rejects_unreasonably_large_pairing_messages() {
        let mut data = vec![0x03, 255];
        data.extend(std::iter::repeat_n(0xAB, 255));
        data.extend(std::iter::repeat_n(data.clone(), 300).flatten());
        assert!(
            matches!(decode(&data), Err(Error::Protocol(message)) if message.contains("too large"))
        );
    }
}
