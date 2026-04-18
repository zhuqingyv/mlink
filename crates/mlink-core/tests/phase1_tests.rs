use mlink_core::protocol::codec::{decode, encode};
use mlink_core::protocol::compress::{
    COMPRESS_THRESHOLD, StreamCompressor, StreamDecompressor, compress, decompress, should_compress,
};
use mlink_core::protocol::errors::{MlinkError, Result};
use mlink_core::protocol::types::{
    MessageType, Priority, decode_flags, encode_flags,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// types module
// ============================================================================

#[test]
fn test_message_type_roundtrip() {
    let all = [
        MessageType::Handshake,
        MessageType::Heartbeat,
        MessageType::Ack,
        MessageType::Ctrl,
        MessageType::Message,
        MessageType::StreamStart,
        MessageType::StreamChunk,
        MessageType::StreamEnd,
        MessageType::StreamAck,
        MessageType::Request,
        MessageType::Response,
        MessageType::ResponseStream,
        MessageType::Subscribe,
        MessageType::Publish,
        MessageType::Unsubscribe,
        MessageType::Error,
    ];
    for t in all {
        let byte: u8 = t.into();
        let back = MessageType::try_from(byte).expect("valid byte");
        assert_eq!(back, t, "roundtrip failed for {:?} (byte=0x{:02X})", t, byte);
    }
}

#[test]
fn test_invalid_message_type() {
    for bad in [0x00u8, 0x05, 0x15, 0x23, 0x33, 0x40, 0x99, 0xFE] {
        let res = MessageType::try_from(bad);
        assert!(res.is_err(), "byte 0x{:02X} should be invalid", bad);
    }
}

#[test]
fn test_priority_mapping() {
    // P0: control-plane messages
    assert_eq!(MessageType::Handshake.priority(), Priority::P0);
    assert_eq!(MessageType::Heartbeat.priority(), Priority::P0);
    assert_eq!(MessageType::Ack.priority(), Priority::P0);
    assert_eq!(MessageType::Ctrl.priority(), Priority::P0);

    // P1: interactive
    assert_eq!(MessageType::StreamAck.priority(), Priority::P1);
    assert_eq!(MessageType::Request.priority(), Priority::P1);
    assert_eq!(MessageType::Response.priority(), Priority::P1);
    assert_eq!(MessageType::ResponseStream.priority(), Priority::P1);
    assert_eq!(MessageType::Error.priority(), Priority::P1);

    // P2: normal
    assert_eq!(MessageType::Message.priority(), Priority::P2);
    assert_eq!(MessageType::StreamStart.priority(), Priority::P2);
    assert_eq!(MessageType::StreamEnd.priority(), Priority::P2);
    assert_eq!(MessageType::Subscribe.priority(), Priority::P2);
    assert_eq!(MessageType::Publish.priority(), Priority::P2);
    assert_eq!(MessageType::Unsubscribe.priority(), Priority::P2);

    // P3: bulk
    assert_eq!(MessageType::StreamChunk.priority(), Priority::P3);
}

#[test]
fn test_flags_encode_decode() {
    let types = [
        MessageType::Handshake,
        MessageType::Heartbeat,
        MessageType::Ack,
        MessageType::Ctrl,
        MessageType::Message,
        MessageType::StreamStart,
        MessageType::StreamChunk,
        MessageType::StreamEnd,
        MessageType::StreamAck,
        MessageType::Request,
        MessageType::Response,
        MessageType::ResponseStream,
        MessageType::Subscribe,
        MessageType::Publish,
        MessageType::Unsubscribe,
        MessageType::Error,
    ];
    for t in types {
        for compressed in [false, true] {
            for encrypted in [false, true] {
                let flags = encode_flags(compressed, encrypted, t);
                let (c, e, mt) = decode_flags(flags);
                assert_eq!(c, compressed, "compress mismatch for {:?}", t);
                assert_eq!(e, encrypted, "encrypt mismatch for {:?}", t);
                assert_eq!(mt, t, "type mismatch for {:?}", t);
            }
        }
    }
}

#[test]
fn test_flags_bit_layout() {
    // bit7 = compress
    let f = encode_flags(true, false, MessageType::Message);
    assert_eq!(f & 0b1000_0000, 0b1000_0000);
    assert_eq!(f & 0b0100_0000, 0);

    // bit6 = encrypt
    let f = encode_flags(false, true, MessageType::Message);
    assert_eq!(f & 0b1000_0000, 0);
    assert_eq!(f & 0b0100_0000, 0b0100_0000);

    // both
    let f = encode_flags(true, true, MessageType::Heartbeat);
    assert_eq!(f & 0b1100_0000, 0b1100_0000);
}

// ============================================================================
// errors module
// ============================================================================

#[test]
fn test_error_display() {
    assert_eq!(MlinkError::Timeout.to_string(), "request timed out");

    assert_eq!(
        MlinkError::PeerGone { peer_id: "peer-1".into() }.to_string(),
        "peer disconnected: peer-1"
    );

    assert_eq!(
        MlinkError::HandlerError("boom".into()).to_string(),
        "handler error: boom"
    );

    assert_eq!(
        MlinkError::PayloadTooLarge { size: 5000, max: 1024 }.to_string(),
        "payload too large: 5000 > 1024"
    );

    assert_eq!(
        MlinkError::UnknownMethod("foo".into()).to_string(),
        "unknown method: foo"
    );

    assert_eq!(
        MlinkError::StreamFailed { stream_id: 3 }.to_string(),
        "stream failed: stream_id=3"
    );

    assert_eq!(
        MlinkError::Backpressure.to_string(),
        "backpressure: receiver overloaded"
    );

    // Each variant includes its key information in Display output.
    let codec = MlinkError::CodecError("oops".into()).to_string();
    assert!(codec.contains("oops"), "codec display missing info: {}", codec);

    let comp = MlinkError::CompressError("bad".into()).to_string();
    assert!(comp.contains("bad"), "compress display missing info: {}", comp);
}

#[test]
fn test_io_error_from() {
    let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no access");
    let mlink_err: MlinkError = io_err.into();
    match mlink_err {
        MlinkError::Io(e) => {
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
            assert!(e.to_string().contains("no access"));
        }
        other => panic!("expected Io variant, got {:?}", other),
    }
}

#[test]
fn test_result_alias() {
    fn returns_err() -> Result<u32> {
        Err(MlinkError::Timeout)
    }
    fn returns_ok() -> Result<u32> {
        Ok(7)
    }
    assert!(returns_err().is_err());
    assert_eq!(returns_ok().unwrap(), 7);
}

// ============================================================================
// codec module
// ============================================================================

#[test]
fn test_encode_decode_string() {
    let s = "hello".to_string();
    let bytes = encode(&s).expect("encode");
    let back: String = decode(&bytes).expect("decode");
    assert_eq!(back, "hello");
}

#[test]
fn test_encode_decode_struct() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Payload {
        id: u64,
        name: String,
        tags: Vec<String>,
        active: bool,
    }

    let original = Payload {
        id: 12345,
        name: "test-payload".into(),
        tags: vec!["alpha".into(), "beta".into(), "gamma".into()],
        active: true,
    };
    let bytes = encode(&original).expect("encode");
    let back: Payload = decode(&bytes).expect("decode");
    assert_eq!(back, original);
}

#[test]
fn test_decode_invalid_data() {
    // Garbage bytes cannot be decoded as a String.
    let garbage = [0xFFu8, 0xFE, 0xFD, 0xFC, 0xFB];
    let res: Result<String> = decode(&garbage);
    assert!(res.is_err(), "decoding garbage should error");
    match res.unwrap_err() {
        MlinkError::CodecError(_) => {}
        other => panic!("expected CodecError, got {:?}", other),
    }
}

#[test]
fn test_encode_large_data() {
    // 1 MB of pseudo-random bytes (deterministic pattern for repeatability).
    let size = 1024 * 1024;
    let mut data = Vec::with_capacity(size);
    for i in 0..size {
        data.push((i as u8).wrapping_mul(37).wrapping_add(13));
    }

    let bytes = encode(&data).expect("encode 1MB");
    assert!(bytes.len() >= size, "encoded should be at least raw size");

    let back: Vec<u8> = decode(&bytes).expect("decode 1MB");
    assert_eq!(back.len(), data.len());
    assert_eq!(back, data);
}

// ============================================================================
// compress module
// ============================================================================

#[test]
fn test_compress_decompress() {
    let data = b"The quick brown fox jumps over the lazy dog. ".repeat(32);
    let compressed = compress(&data).expect("compress");
    let back = decompress(&compressed).expect("decompress");
    assert_eq!(back, data);
}

#[test]
fn test_should_compress_threshold() {
    // Boundary values around the 256-byte threshold.
    assert_eq!(COMPRESS_THRESHOLD, 256);

    let small = vec![0u8; 255];
    assert!(!should_compress(&small), "255 bytes should not compress");

    let at_threshold = vec![0u8; 256];
    assert!(should_compress(&at_threshold), "256 bytes should compress");

    let large = vec![0u8; 1024];
    assert!(should_compress(&large), "1024 bytes should compress");

    let empty: Vec<u8> = Vec::new();
    assert!(!should_compress(&empty), "empty should not compress");
}

#[test]
fn test_compress_ratio() {
    // Repeated text compresses very well; target: < 50% of original.
    let original = "abcdefghij".repeat(1024); // 10 KB
    let original_bytes = original.as_bytes();
    let compressed = compress(original_bytes).expect("compress");

    let ratio = compressed.len() as f64 / original_bytes.len() as f64;
    assert!(
        compressed.len() < original_bytes.len() / 2,
        "compression ratio {:.2}% not better than 50% (orig={}, comp={})",
        ratio * 100.0,
        original_bytes.len(),
        compressed.len()
    );

    let decomp = decompress(&compressed).expect("decompress");
    assert_eq!(decomp, original_bytes);
}

#[test]
fn test_empty_data() {
    let empty: &[u8] = &[];
    let compressed = compress(empty).expect("compress empty");
    let back = decompress(&compressed).expect("decompress empty");
    assert_eq!(back, empty);
}

#[test]
fn test_stream_compressor() {
    let chunks: Vec<&[u8]> = vec![
        b"chunk-one-alpha ".as_slice(),
        b"chunk-two-beta ".as_slice(),
        b"chunk-three-gamma ".as_slice(),
        b"final tail.".as_slice(),
    ];
    let mut original = Vec::new();
    for c in &chunks {
        original.extend_from_slice(c);
    }

    let mut comp = StreamCompressor::new(3).expect("compressor");
    let mut wire = Vec::new();
    for c in &chunks {
        let part = comp.write_chunk(c).expect("write_chunk");
        wire.extend_from_slice(&part);
    }
    let tail = comp.finish().expect("finish");
    wire.extend_from_slice(&tail);

    let mut dec = StreamDecompressor::new().expect("decompressor");
    let out = dec.read_chunk(&wire).expect("read_chunk");

    assert_eq!(out, original);
}
