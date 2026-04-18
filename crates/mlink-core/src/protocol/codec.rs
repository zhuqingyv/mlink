use serde::{Serialize, de::DeserializeOwned};

use super::errors::{MlinkError, Result};

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(value).map_err(|e| MlinkError::CodecError(e.to_string()))
}

pub fn decode<T: DeserializeOwned>(data: &[u8]) -> Result<T> {
    rmp_serde::from_slice(data).map_err(|e| MlinkError::CodecError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        name: String,
        value: u32,
    }

    #[test]
    fn roundtrip_preserves_value() {
        let original = Sample { name: "hello".into(), value: 42 };
        let bytes = encode(&original).expect("encode");
        let back: Sample = decode(&bytes).expect("decode");
        assert_eq!(original, back);
    }

    #[test]
    fn encode_uses_named_fields() {
        let sample = Sample { name: "x".into(), value: 1 };
        let bytes = encode(&sample).expect("encode");
        // to_vec_named emits field names as strings in the msgpack map.
        // A raw scan is enough to verify named-map encoding without extra deps.
        assert!(bytes.windows(4).any(|w| w == b"name"));
        assert!(bytes.windows(5).any(|w| w == b"value"));
    }

    #[test]
    fn decode_invalid_returns_codec_error() {
        let err = decode::<Sample>(&[0xff, 0xff, 0xff]).unwrap_err();
        assert!(matches!(err, MlinkError::CodecError(_)));
    }
}
