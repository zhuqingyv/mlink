use super::errors::{MlinkError, Result};
use super::types::{Frame, HEADER_SIZE, MAGIC, PROTOCOL_VERSION};

pub fn encode_frame(frame: &Frame) -> Vec<u8> {
    let length = frame.payload.len() as u16;
    let mut buf = Vec::with_capacity(HEADER_SIZE + frame.payload.len());
    buf.extend_from_slice(&MAGIC.to_be_bytes());
    buf.push(PROTOCOL_VERSION);
    buf.push(frame.flags);
    buf.extend_from_slice(&frame.seq.to_be_bytes());
    buf.extend_from_slice(&length.to_be_bytes());
    buf.extend_from_slice(&frame.payload);
    buf
}

pub fn decode_frame(data: &[u8]) -> Result<Frame> {
    if data.len() < HEADER_SIZE {
        return Err(MlinkError::CodecError(format!(
            "frame too short: {} < {}",
            data.len(),
            HEADER_SIZE
        )));
    }

    let magic = u16::from_be_bytes([data[0], data[1]]);
    if magic != MAGIC {
        return Err(MlinkError::CodecError(format!(
            "invalid magic: 0x{:04X}",
            magic
        )));
    }

    let version = data[2];
    if version != PROTOCOL_VERSION {
        return Err(MlinkError::CodecError(format!(
            "unsupported version: 0x{:02X}",
            version
        )));
    }

    let flags = data[3];
    let seq = u16::from_be_bytes([data[4], data[5]]);
    let length = u16::from_be_bytes([data[6], data[7]]);

    let payload_end = HEADER_SIZE + length as usize;
    if data.len() < payload_end {
        return Err(MlinkError::CodecError(format!(
            "payload truncated: have {} bytes, need {}",
            data.len(),
            payload_end
        )));
    }

    let payload = data[HEADER_SIZE..payload_end].to_vec();

    Ok(Frame {
        magic,
        version,
        flags,
        seq,
        length,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{encode_flags, MessageType};

    fn sample_frame(payload: Vec<u8>) -> Frame {
        let length = payload.len() as u16;
        Frame {
            magic: MAGIC,
            version: PROTOCOL_VERSION,
            flags: encode_flags(true, false, MessageType::Message),
            seq: 0x1234,
            length,
            payload,
        }
    }

    #[test]
    fn encode_layout_is_big_endian() {
        let frame = sample_frame(vec![0xAA, 0xBB, 0xCC]);
        let bytes = encode_frame(&frame);

        assert_eq!(bytes.len(), HEADER_SIZE + 3);
        assert_eq!(&bytes[0..2], &[0x4D, 0x4C]);
        assert_eq!(bytes[2], 0x01);
        assert_eq!(bytes[3], frame.flags);
        assert_eq!(&bytes[4..6], &[0x12, 0x34]);
        assert_eq!(&bytes[6..8], &[0x00, 0x03]);
        assert_eq!(&bytes[8..], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn encode_decode_round_trip() {
        let frame = sample_frame(vec![1, 2, 3, 4, 5]);
        let bytes = encode_frame(&frame);
        let back = decode_frame(&bytes).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn encode_empty_payload() {
        let frame = sample_frame(vec![]);
        let bytes = encode_frame(&frame);
        assert_eq!(bytes.len(), HEADER_SIZE);
        assert_eq!(&bytes[6..8], &[0x00, 0x00]);
        let back = decode_frame(&bytes).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn decode_rejects_too_short() {
        let err = decode_frame(&[0x4D, 0x4C, 0x01]).unwrap_err();
        assert!(matches!(err, MlinkError::CodecError(_)));
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut bytes = encode_frame(&sample_frame(vec![1]));
        bytes[0] = 0xFF;
        let err = decode_frame(&bytes).unwrap_err();
        assert!(matches!(err, MlinkError::CodecError(_)));
    }

    #[test]
    fn decode_rejects_bad_version() {
        let mut bytes = encode_frame(&sample_frame(vec![1]));
        bytes[2] = 0x99;
        let err = decode_frame(&bytes).unwrap_err();
        assert!(matches!(err, MlinkError::CodecError(_)));
    }

    #[test]
    fn decode_rejects_truncated_payload() {
        let frame = sample_frame(vec![1, 2, 3, 4, 5]);
        let bytes = encode_frame(&frame);
        let err = decode_frame(&bytes[..bytes.len() - 2]).unwrap_err();
        assert!(matches!(err, MlinkError::CodecError(_)));
    }

    #[test]
    fn decode_ignores_trailing_bytes() {
        let frame = sample_frame(vec![1, 2, 3]);
        let mut bytes = encode_frame(&frame);
        bytes.extend_from_slice(&[0xDE, 0xAD]);
        let back = decode_frame(&bytes).unwrap();
        assert_eq!(back, frame);
    }
}
