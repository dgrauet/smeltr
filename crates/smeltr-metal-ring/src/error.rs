use thiserror::Error;

#[derive(Debug, Error)]
pub enum RingError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad magic 0x{0:08x}, expected 0x{1:08x}")]
    BadMagic(u32, u32),
    #[error("bad version {0}, expected {1}")]
    BadVersion(u32, u32),
    #[error("capacity {0} is not a power of two")]
    BadCapacity(u64),
    #[error("frame truncated at offset {0}")]
    Truncated(u64),
    #[error("unknown frame kind {0}")]
    UnknownKind(u32),
    #[error("invalid utf-8 in label: {0}")]
    BadUtf8(#[from] std::str::Utf8Error),
}
