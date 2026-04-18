use std::io::{Cursor, Read, Write};

use zstd::stream::{Decoder, Encoder};

use super::errors::{MlinkError, Result};

pub const COMPRESS_THRESHOLD: usize = 256;

const DEFAULT_LEVEL: i32 = 3;

pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    zstd::encode_all(Cursor::new(data), DEFAULT_LEVEL)
        .map_err(|e| MlinkError::CompressError(e.to_string()))
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    zstd::decode_all(Cursor::new(data))
        .map_err(|e| MlinkError::CompressError(e.to_string()))
}

pub fn should_compress(data: &[u8]) -> bool {
    data.len() >= COMPRESS_THRESHOLD
}

pub struct StreamCompressor {
    encoder: Encoder<'static, Vec<u8>>,
}

impl StreamCompressor {
    pub fn new(level: i32) -> Result<Self> {
        let encoder = Encoder::new(Vec::new(), level)
            .map_err(|e| MlinkError::CompressError(e.to_string()))?;
        Ok(Self { encoder })
    }

    pub fn write_chunk(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        self.encoder
            .write_all(data)
            .map_err(|e| MlinkError::CompressError(e.to_string()))?;
        self.encoder
            .flush()
            .map_err(|e| MlinkError::CompressError(e.to_string()))?;
        Ok(std::mem::take(self.encoder.get_mut()))
    }

    pub fn finish(self) -> Result<Vec<u8>> {
        self.encoder
            .finish()
            .map_err(|e| MlinkError::CompressError(e.to_string()))
    }
}

pub struct StreamDecompressor {
    buffer: Vec<u8>,
}

impl StreamDecompressor {
    pub fn new() -> Result<Self> {
        Ok(Self { buffer: Vec::new() })
    }

    pub fn read_chunk(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        self.buffer.extend_from_slice(data);
        let mut decoder = Decoder::new(Cursor::new(&self.buffer[..]))
            .map_err(|e| MlinkError::CompressError(e.to_string()))?;
        let mut out = Vec::new();
        decoder
            .read_to_end(&mut out)
            .map_err(|e| MlinkError::CompressError(e.to_string()))?;
        self.buffer.clear();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_compress_threshold() {
        assert!(!should_compress(&vec![0u8; 255]));
        assert!(should_compress(&vec![0u8; 256]));
        assert!(should_compress(&vec![0u8; 1024]));
    }

    #[test]
    fn compress_decompress_roundtrip() {
        let data = b"hello world, this is a test of zstd compression".repeat(32);
        let compressed = compress(&data).unwrap();
        assert!(compressed.len() < data.len());
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn compress_small_input() {
        let data = b"hi";
        let compressed = compress(data).unwrap();
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn decompress_bad_input() {
        let result = decompress(b"not zstd data");
        assert!(result.is_err());
    }

    #[test]
    fn stream_roundtrip_single_chunk() {
        let data = b"streaming test data ".repeat(64);
        let mut enc = StreamCompressor::new(3).unwrap();
        let mut compressed = enc.write_chunk(&data).unwrap();
        compressed.extend(enc.finish().unwrap());

        let mut dec = StreamDecompressor::new().unwrap();
        let out = dec.read_chunk(&compressed).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn stream_finish_returns_tail() {
        let mut enc = StreamCompressor::new(3).unwrap();
        let head = enc.write_chunk(b"abc").unwrap();
        let tail = enc.finish().unwrap();
        let mut all = head;
        all.extend(tail);

        let decoded = decompress(&all).unwrap();
        assert_eq!(decoded, b"abc");
    }
}
