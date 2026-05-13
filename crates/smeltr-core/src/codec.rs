//! Length-prefixed CBOR framing.
//!
//! Each frame on disk is `u32_le(length) || cbor_bytes`. Sized for sequential
//! append-only writes and tolerant of partial reads.

use serde::{de::DeserializeOwned, Serialize};
use std::io::{self, Read, Write};

#[derive(thiserror::Error, Debug)]
pub enum CodecError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("cbor encode: {0}")]
    CborEncode(#[from] ciborium::ser::Error<io::Error>),
    #[error("cbor decode: {0}")]
    CborDecode(#[from] ciborium::de::Error<io::Error>),
    #[error("frame too large: {0} bytes (max {1})")]
    FrameTooLarge(u32, u32),
    #[error("truncated frame")]
    Truncated,
}

pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> Result<usize, CodecError> {
    let mut buf = Vec::with_capacity(256);
    ciborium::into_writer(value, &mut buf)?;
    if buf.len() as u64 > MAX_FRAME_BYTES as u64 {
        return Err(CodecError::FrameTooLarge(buf.len() as u32, MAX_FRAME_BYTES));
    }
    let len = (buf.len() as u32).to_le_bytes();
    w.write_all(&len)?;
    w.write_all(&buf)?;
    Ok(4 + buf.len())
}

/// Reads a single frame. Returns `Ok(None)` on clean EOF before any byte was
/// read (i.e. end of file at a frame boundary). Returns `Err(Truncated)` if
/// EOF arrives mid-frame.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>, CodecError> {
    let mut len_buf = [0u8; 4];
    match r.read(&mut len_buf)? {
        0 => return Ok(None),
        4 => {}
        n => {
            // partial header
            let mut total = n;
            while total < 4 {
                match r.read(&mut len_buf[total..])? {
                    0 => return Err(CodecError::Truncated),
                    k => total += k,
                }
            }
        }
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge(len, MAX_FRAME_BYTES));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            CodecError::Truncated
        } else {
            e.into()
        }
    })?;
    let value = ciborium::from_reader(&payload[..])?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Payload, Source};
    use uuid::Uuid;

    fn ev(seq: u64, label: &str) -> Event {
        Event {
            ts_mono_ns: seq * 1000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq,
            payload: Payload::Mark {
                label: label.into(),
            },
        }
    }

    #[test]
    fn round_trip_multiple_frames() {
        let mut buf = Vec::<u8>::new();
        write_frame(&mut buf, &ev(1, "a")).unwrap();
        write_frame(&mut buf, &ev(2, "bb")).unwrap();
        write_frame(&mut buf, &ev(3, "ccc")).unwrap();

        let mut r = std::io::Cursor::new(buf);
        let e1: Event = read_frame(&mut r).unwrap().unwrap();
        let e2: Event = read_frame(&mut r).unwrap().unwrap();
        let e3: Event = read_frame(&mut r).unwrap().unwrap();
        let end: Option<Event> = read_frame(&mut r).unwrap();

        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        assert_eq!(e3.seq, 3);
        assert!(end.is_none());
    }

    #[test]
    fn truncated_mid_frame_is_error() {
        let mut buf = Vec::<u8>::new();
        write_frame(&mut buf, &ev(1, "label")).unwrap();
        buf.truncate(buf.len() - 3); // chop last 3 bytes
        let mut r = std::io::Cursor::new(buf);
        let res = read_frame::<_, Event>(&mut r);
        assert!(matches!(res, Err(CodecError::Truncated)));
    }
}
