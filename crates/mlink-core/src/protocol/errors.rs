use thiserror::Error;

#[derive(Debug, Error)]
pub enum MlinkError {
    #[error("request timed out")]
    Timeout,

    #[error("peer disconnected: {peer_id}")]
    PeerGone { peer_id: String },

    #[error("handler error: {0}")]
    HandlerError(String),

    #[error("payload too large: {size} > {max}")]
    PayloadTooLarge { size: usize, max: usize },

    #[error("unknown method: {0}")]
    UnknownMethod(String),

    #[error("stream failed: stream_id={stream_id}")]
    StreamFailed { stream_id: u16 },

    #[error("backpressure: receiver overloaded")]
    Backpressure,

    #[error("security error: {0}")]
    SecurityError(String),

    #[error("codec error: {0}")]
    CodecError(String),

    #[error("compress error: {0}")]
    CompressError(String),

    #[error("ble error: {0}")]
    Ble(#[from] btleplug::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, MlinkError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_display() {
        let e = MlinkError::Timeout;
        assert_eq!(e.to_string(), "request timed out");
    }

    #[test]
    fn peer_gone_display() {
        let e = MlinkError::PeerGone {
            peer_id: "abc123".into(),
        };
        assert_eq!(e.to_string(), "peer disconnected: abc123");
    }

    #[test]
    fn payload_too_large_display() {
        let e = MlinkError::PayloadTooLarge {
            size: 2048,
            max: 1024,
        };
        assert_eq!(e.to_string(), "payload too large: 2048 > 1024");
    }

    #[test]
    fn unknown_method_display() {
        let e = MlinkError::UnknownMethod("foo.bar".into());
        assert_eq!(e.to_string(), "unknown method: foo.bar");
    }

    #[test]
    fn stream_failed_display() {
        let e = MlinkError::StreamFailed { stream_id: 7 };
        assert_eq!(e.to_string(), "stream failed: stream_id=7");
    }

    #[test]
    fn backpressure_display() {
        let e = MlinkError::Backpressure;
        assert_eq!(e.to_string(), "backpressure: receiver overloaded");
    }

    #[test]
    fn io_from_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "boom");
        let e: MlinkError = io_err.into();
        assert!(matches!(e, MlinkError::Io(_)));
    }

    #[test]
    fn result_alias_works() {
        fn produces() -> Result<u32> {
            Err(MlinkError::Timeout)
        }
        assert!(produces().is_err());
    }
}
