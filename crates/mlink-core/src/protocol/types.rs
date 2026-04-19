use serde::{Deserialize, Serialize};

pub const MAGIC: u16 = 0x4D4C;
pub const PROTOCOL_VERSION: u8 = 0x01;
pub const HEADER_SIZE: usize = 8;

const FLAG_COMPRESSED: u8 = 0b1000_0000;
const FLAG_ENCRYPTED: u8 = 0b0100_0000;
const TYPE_MASK: u8 = 0b0011_1111;
const TYPE_ERROR_ON_WIRE: u8 = 0x3F;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageType {
    Handshake = 0x01,
    Heartbeat = 0x02,
    Ack = 0x03,
    Ctrl = 0x04,
    Message = 0x10,
    StreamStart = 0x11,
    StreamChunk = 0x12,
    StreamEnd = 0x13,
    StreamAck = 0x14,
    Request = 0x20,
    Response = 0x21,
    ResponseStream = 0x22,
    Subscribe = 0x30,
    Publish = 0x31,
    Unsubscribe = 0x32,
    Error = 0xFF,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidMessageType(pub u8);

impl std::fmt::Display for InvalidMessageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid message type: 0x{:02X}", self.0)
    }
}

impl std::error::Error for InvalidMessageType {}

impl TryFrom<u8> for MessageType {
    type Error = InvalidMessageType;

    fn try_from(value: u8) -> Result<Self, InvalidMessageType> {
        match value {
            0x01 => Ok(MessageType::Handshake),
            0x02 => Ok(MessageType::Heartbeat),
            0x03 => Ok(MessageType::Ack),
            0x04 => Ok(MessageType::Ctrl),
            0x10 => Ok(MessageType::Message),
            0x11 => Ok(MessageType::StreamStart),
            0x12 => Ok(MessageType::StreamChunk),
            0x13 => Ok(MessageType::StreamEnd),
            0x14 => Ok(MessageType::StreamAck),
            0x20 => Ok(MessageType::Request),
            0x21 => Ok(MessageType::Response),
            0x22 => Ok(MessageType::ResponseStream),
            0x30 => Ok(MessageType::Subscribe),
            0x31 => Ok(MessageType::Publish),
            0x32 => Ok(MessageType::Unsubscribe),
            0xFF => Ok(MessageType::Error),
            other => Err(InvalidMessageType(other)),
        }
    }
}

impl From<MessageType> for u8 {
    fn from(t: MessageType) -> u8 {
        t as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

impl MessageType {
    pub fn priority(self) -> Priority {
        match self {
            MessageType::Handshake
            | MessageType::Heartbeat
            | MessageType::Ack
            | MessageType::Ctrl => Priority::P0,
            MessageType::StreamAck
            | MessageType::Request
            | MessageType::Response
            | MessageType::ResponseStream
            | MessageType::Error => Priority::P1,
            MessageType::Message
            | MessageType::StreamStart
            | MessageType::StreamEnd
            | MessageType::Subscribe
            | MessageType::Publish
            | MessageType::Unsubscribe => Priority::P2,
            MessageType::StreamChunk => Priority::P3,
        }
    }
}

pub fn encode_flags(compressed: bool, encrypted: bool, msg_type: MessageType) -> u8 {
    let type_bits = match msg_type {
        MessageType::Error => TYPE_ERROR_ON_WIRE,
        other => u8::from(other) & TYPE_MASK,
    };
    let mut flags = type_bits;
    if compressed {
        flags |= FLAG_COMPRESSED;
    }
    if encrypted {
        flags |= FLAG_ENCRYPTED;
    }
    flags
}

pub fn decode_flags(flags: u8) -> (bool, bool, MessageType) {
    let compressed = flags & FLAG_COMPRESSED != 0;
    let encrypted = flags & FLAG_ENCRYPTED != 0;
    let type_bits = flags & TYPE_MASK;
    let msg_type = if type_bits == TYPE_ERROR_ON_WIRE {
        MessageType::Error
    } else {
        MessageType::try_from(type_bits).unwrap_or(MessageType::Error)
    };
    (compressed, encrypted, msg_type)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CtrlCommand {
    Pause,
    Resume,
    MtuUpdate(u16),
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    Timeout = 0x01,
    PeerGone = 0x02,
    HandlerError = 0x03,
    PayloadTooLarge = 0x04,
    UnknownMethod = 0x05,
    StreamFailed = 0x06,
    Backpressure = 0x07,
}

impl TryFrom<u8> for ErrorCode {
    type Error = InvalidMessageType;

    fn try_from(value: u8) -> Result<Self, InvalidMessageType> {
        match value {
            0x01 => Ok(ErrorCode::Timeout),
            0x02 => Ok(ErrorCode::PeerGone),
            0x03 => Ok(ErrorCode::HandlerError),
            0x04 => Ok(ErrorCode::PayloadTooLarge),
            0x05 => Ok(ErrorCode::UnknownMethod),
            0x06 => Ok(ErrorCode::StreamFailed),
            0x07 => Ok(ErrorCode::Backpressure),
            other => Err(InvalidMessageType(other)),
        }
    }
}

impl From<ErrorCode> for u8 {
    fn from(c: ErrorCode) -> u8 {
        c as u8
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub magic: u16,
    pub version: u8,
    pub flags: u8,
    pub seq: u16,
    pub length: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamResumeInfo {
    pub stream_id: u16,
    pub received_bitmap: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake {
    pub app_uuid: String,
    pub version: u8,
    pub mtu: u16,
    pub compress: bool,
    pub encrypt: bool,
    pub last_seq: u16,
    pub resume_streams: Vec<StreamResumeInfo>,
    /// Room hash the local side claims membership in; peers must match this
    /// to keep the connection. `None` means "any room" (legacy / listen mode).
    #[serde(default)]
    pub room_hash: Option<[u8; 8]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub stream_id: u16,
    pub total_chunks: u32,
    pub total_size: u64,
    pub checksum_algo: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_correct() {
        assert_eq!(MAGIC, 0x4D4C);
        assert_eq!(PROTOCOL_VERSION, 0x01);
        assert_eq!(HEADER_SIZE, 8);
    }

    #[test]
    fn message_type_round_trip() {
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
            assert_eq!(MessageType::try_from(byte).unwrap(), t);
        }
    }

    #[test]
    fn message_type_rejects_unknown() {
        assert!(MessageType::try_from(0x00).is_err());
        assert!(MessageType::try_from(0x99).is_err());
    }

    #[test]
    fn priority_mapping() {
        assert_eq!(MessageType::Handshake.priority(), Priority::P0);
        assert_eq!(MessageType::Heartbeat.priority(), Priority::P0);
        assert_eq!(MessageType::Ack.priority(), Priority::P0);
        assert_eq!(MessageType::Ctrl.priority(), Priority::P0);
        assert_eq!(MessageType::StreamAck.priority(), Priority::P1);
        assert_eq!(MessageType::Request.priority(), Priority::P1);
        assert_eq!(MessageType::Response.priority(), Priority::P1);
        assert_eq!(MessageType::ResponseStream.priority(), Priority::P1);
        assert_eq!(MessageType::Error.priority(), Priority::P1);
        assert_eq!(MessageType::Message.priority(), Priority::P2);
        assert_eq!(MessageType::StreamStart.priority(), Priority::P2);
        assert_eq!(MessageType::StreamEnd.priority(), Priority::P2);
        assert_eq!(MessageType::Subscribe.priority(), Priority::P2);
        assert_eq!(MessageType::Publish.priority(), Priority::P2);
        assert_eq!(MessageType::Unsubscribe.priority(), Priority::P2);
        assert_eq!(MessageType::StreamChunk.priority(), Priority::P3);
    }

    #[test]
    fn flags_round_trip_all_combinations() {
        let types = [
            MessageType::Handshake,
            MessageType::Message,
            MessageType::StreamChunk,
            MessageType::Error,
        ];
        for t in types {
            for compressed in [false, true] {
                for encrypted in [false, true] {
                    let flags = encode_flags(compressed, encrypted, t);
                    let (c, e, mt) = decode_flags(flags);
                    assert_eq!(c, compressed);
                    assert_eq!(e, encrypted);
                    assert_eq!(mt, t);
                }
            }
        }
    }

    #[test]
    fn flags_bit_layout() {
        let flags = encode_flags(true, false, MessageType::Handshake);
        assert_eq!(flags & 0b1000_0000, 0b1000_0000);
        assert_eq!(flags & 0b0100_0000, 0);

        let flags = encode_flags(false, true, MessageType::Handshake);
        assert_eq!(flags & 0b1000_0000, 0);
        assert_eq!(flags & 0b0100_0000, 0b0100_0000);
    }

    #[test]
    fn error_code_round_trip() {
        let all = [
            ErrorCode::Timeout,
            ErrorCode::PeerGone,
            ErrorCode::HandlerError,
            ErrorCode::PayloadTooLarge,
            ErrorCode::UnknownMethod,
            ErrorCode::StreamFailed,
            ErrorCode::Backpressure,
        ];
        for c in all {
            let byte: u8 = c.into();
            assert_eq!(ErrorCode::try_from(byte).unwrap(), c);
        }
    }

    #[test]
    fn ctrl_command_serde() {
        let cases = [
            CtrlCommand::Pause,
            CtrlCommand::Resume,
            CtrlCommand::MtuUpdate(512),
        ];
        for c in cases {
            let bytes = rmp_serde::to_vec(&c).unwrap();
            let back: CtrlCommand = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(c, back);
        }
    }

    #[test]
    fn handshake_serde() {
        let h = Handshake {
            app_uuid: "abc".into(),
            version: 1,
            mtu: 512,
            compress: true,
            encrypt: true,
            last_seq: 42,
            resume_streams: vec![StreamResumeInfo {
                stream_id: 7,
                received_bitmap: vec![0xFF, 0x3F],
            }],
            room_hash: Some([0xAB; 8]),
        };
        let bytes = rmp_serde::to_vec(&h).unwrap();
        let back: Handshake = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn handshake_serde_backwards_compat_no_room_hash() {
        let h = Handshake {
            app_uuid: "abc".into(),
            version: 1,
            mtu: 512,
            compress: true,
            encrypt: true,
            last_seq: 42,
            resume_streams: vec![],
            room_hash: None,
        };
        let bytes = rmp_serde::to_vec(&h).unwrap();
        let back: Handshake = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(h, back);
        assert!(back.room_hash.is_none());
    }

    #[test]
    fn frame_serde() {
        let f = Frame {
            magic: MAGIC,
            version: PROTOCOL_VERSION,
            flags: encode_flags(true, false, MessageType::Message),
            seq: 1,
            length: 3,
            payload: vec![1, 2, 3],
        };
        let bytes = rmp_serde::to_vec(&f).unwrap();
        let back: Frame = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn stream_info_serde() {
        let s = StreamInfo {
            stream_id: 9,
            total_chunks: 100,
            total_size: 1024 * 1024,
            checksum_algo: "sha256".into(),
        };
        let bytes = rmp_serde::to_vec(&s).unwrap();
        let back: StreamInfo = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(s, back);
    }
}
