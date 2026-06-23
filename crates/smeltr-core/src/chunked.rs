//! Opt-in chunked, indexed session container format. See the design spec.
//!
//! Layout: HEAD_MAGIC | repeated [comp_len u32 LE][zstd frame] | footer | trailer.

use crate::codec::read_frame;
use crate::event::Event;
use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

/// Read a little-endian u64 from a byte slice at `offset`. The slice must be
/// at least `offset + 8` bytes long.
#[inline]
fn read_u64_le(b: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        b[offset],
        b[offset + 1],
        b[offset + 2],
        b[offset + 3],
        b[offset + 4],
        b[offset + 5],
        b[offset + 6],
        b[offset + 7],
    ])
}

/// Read a little-endian u32 from a byte slice at `offset`. The slice must be
/// at least `offset + 4` bytes long.
#[inline]
fn read_u32_le(b: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([b[offset], b[offset + 1], b[offset + 2], b[offset + 3]])
}

pub const HEAD_MAGIC: [u8; 4] = *b"SMz\x01";
pub const FORMAT_VERSION: u8 = 1;
pub const FOOTER_MAGIC: u64 = 0x534D_7A46_4F4F_5400;
pub const ENTRY_SIZE: usize = 40; // 8+4+8+8+8+4

/// Size of the trailer appended after the footer body:
/// footer_offset u64 (8) + crc u32 (4) + magic u64 (8) = 20.
const TRAILER_SIZE: u64 = 20;

/// Minimum valid file length: HEAD_MAGIC (4) + at least one chunk header u32 (4) + trailer (20).
const MIN_VALID_FILE_LEN: u64 = HEAD_MAGIC.len() as u64 + 4 + TRAILER_SIZE; // 28

pub const CHUNK_EVENTS: u32 = 1024;
pub const CHUNK_BYTES: u64 = 256 * 1024;
pub const FLUSH_MIN_BYTES: u64 = 4096;
pub const MAX_CHUNK_BYTES: u64 = 512 * 1024; // scan sanity bound
pub const MAX_CHUNKS: usize = 1_048_576;

/// Seal thresholds (injectable so tests can use tiny values).
#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    pub max_events: u32,
    pub max_bytes: u64,
    pub flush_min_bytes: u64,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            max_events: CHUNK_EVENTS,
            max_bytes: CHUNK_BYTES,
            flush_min_bytes: FLUSH_MIN_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkIndexEntry {
    pub offset: u64,
    pub comp_len: u32,
    pub min_ts: u64,
    pub max_ts: u64,
    pub source_bitmap: u64,
    pub event_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Legacy,
    Chunked,
    Unsupported(u8),
}

#[derive(Debug)]
pub enum SessionFormatError {
    Io(io::Error),
    FooterCorrupt(String),
}

impl std::fmt::Display for SessionFormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionFormatError::Io(e) => write!(f, "io: {e}"),
            SessionFormatError::FooterCorrupt(m) => write!(f, "footer corrupt: {m}"),
        }
    }
}

impl std::error::Error for SessionFormatError {}

impl From<io::Error> for SessionFormatError {
    fn from(e: io::Error) -> Self {
        SessionFormatError::Io(e)
    }
}

/// Detect the session format by reading the first 4 bytes of `file`.
/// The file cursor position is unspecified after this call.
pub fn detect(file: &mut File) -> io::Result<Format> {
    file.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; 4];
    match file.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Format::Legacy),
        Err(e) => return Err(e),
    }
    if head[..3] == HEAD_MAGIC[..3] {
        if head[3] == FORMAT_VERSION {
            Ok(Format::Chunked)
        } else {
            Ok(Format::Unsupported(head[3]))
        }
    } else {
        Ok(Format::Legacy)
    }
}

/// Write the footer body + trailer. `footer_offset` is the absolute byte position
/// at which the footer body starts (i.e. the writer's cursor before this call).
pub fn write_footer_at<W: Write>(
    w: &mut W,
    footer_offset: u64,
    entries: &[ChunkIndexEntry],
) -> io::Result<()> {
    let mut body = Vec::with_capacity(4 + entries.len() * ENTRY_SIZE);
    body.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        body.extend_from_slice(&e.offset.to_le_bytes());
        body.extend_from_slice(&e.comp_len.to_le_bytes());
        body.extend_from_slice(&e.min_ts.to_le_bytes());
        body.extend_from_slice(&e.max_ts.to_le_bytes());
        body.extend_from_slice(&e.source_bitmap.to_le_bytes());
        body.extend_from_slice(&e.event_count.to_le_bytes());
    }
    let crc = crc32fast::hash(&body);
    w.write_all(&body)?;
    // Trailer: footer_offset (u64 LE) + crc (u32 LE) + magic (u64 LE)
    w.write_all(&footer_offset.to_le_bytes())?;
    w.write_all(&crc.to_le_bytes())?;
    w.write_all(&FOOTER_MAGIC.to_le_bytes())?;
    Ok(())
}

/// Read and validate the footer index.
///
/// Returns:
/// - `Ok(None)` if the file has no sealed footer (magic mismatch or file too short).
/// - `Ok(Some(entries))` if the footer is present and valid.
/// - `Err(SessionFormatError::FooterCorrupt(...))` if the magic matches but the
///   data is invalid (range, size equation, or CRC).
pub fn read_footer(file: &mut File) -> Result<Option<Vec<ChunkIndexEntry>>, SessionFormatError> {
    let len = file.seek(SeekFrom::End(0))?;
    if len < MIN_VALID_FILE_LEN {
        return Ok(None);
    }
    let trailer_start = len - TRAILER_SIZE;
    file.seek(SeekFrom::Start(trailer_start))?;
    let mut t = [0u8; TRAILER_SIZE as usize];
    file.read_exact(&mut t)?;

    let footer_offset = read_u64_le(&t, 0);
    let crc = read_u32_le(&t, 8);
    let magic = read_u64_le(&t, 12);

    // Magic mismatch → not a sealed chunked file; treat as legacy/unsealed.
    if magic != FOOTER_MAGIC {
        return Ok(None);
    }

    // From here the file self-identifies as sealed; any structural failure is an error.
    if footer_offset < HEAD_MAGIC.len() as u64 || footer_offset >= trailer_start {
        return Err(SessionFormatError::FooterCorrupt(format!(
            "footer_offset {footer_offset} out of range"
        )));
    }

    let body_len = trailer_start - footer_offset;
    if body_len < 4 {
        return Err(SessionFormatError::FooterCorrupt(
            "footer body too small".into(),
        ));
    }

    // Read the count prefix first to verify size equation before allocating.
    file.seek(SeekFrom::Start(footer_offset))?;
    let mut count_buf = [0u8; 4];
    file.read_exact(&mut count_buf)?;
    let chunk_count = u32::from_le_bytes(count_buf) as u64;

    // Verified u64 arithmetic before any Vec::with_capacity.
    let expected = 4u64
        + chunk_count
            .checked_mul(ENTRY_SIZE as u64)
            .ok_or_else(|| SessionFormatError::FooterCorrupt("count overflow".into()))?;
    if expected != body_len {
        return Err(SessionFormatError::FooterCorrupt(format!(
            "size mismatch: expected {expected}, body {body_len}"
        )));
    }

    // Read the whole footer body and verify CRC.
    file.seek(SeekFrom::Start(footer_offset))?;
    let mut body = vec![0u8; body_len as usize];
    file.read_exact(&mut body)?;
    if crc32fast::hash(&body) != crc {
        return Err(SessionFormatError::FooterCorrupt("crc mismatch".into()));
    }

    let mut entries = Vec::with_capacity(chunk_count as usize);
    let mut p = 4usize;
    for _ in 0..chunk_count {
        let s = &body[p..p + ENTRY_SIZE];
        entries.push(ChunkIndexEntry {
            offset: read_u64_le(s, 0),
            comp_len: read_u32_le(s, 8),
            min_ts: read_u64_le(s, 12),
            max_ts: read_u64_le(s, 20),
            source_bitmap: read_u64_le(s, 28),
            event_count: read_u32_le(s, 36),
        });
        p += ENTRY_SIZE;
    }

    Ok(Some(entries))
}

/// Decompress a sealed chunk's compressed bytes and decode all events within.
///
/// Uses `zstd::stream::read::Decoder` over a `Cursor` — never `zstd::bulk`.
pub fn decode_chunk(bytes: &[u8]) -> io::Result<Vec<Event>> {
    let mut dec = zstd::stream::read::Decoder::new(Cursor::new(bytes))?;
    let mut out = Vec::new();
    loop {
        match read_frame::<_, Event>(&mut dec) {
            Ok(Some(ev)) => out.push(ev),
            Ok(None) => break, // clean EOF at frame boundary
            Err(crate::codec::CodecError::Truncated) => break,
            Err(crate::codec::CodecError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("chunk decode: {e}"),
                ))
            }
        }
    }
    Ok(out)
}

/// Read the compressed bytes of one chunk from `file` at `offset` (pointing to the
/// comp_len u32 header). Returns `None` at clean truncation: EOF, zero comp_len, or
/// comp_len exceeding `MAX_CHUNK_BYTES` or extending past end-of-file.
fn read_chunk_at(
    file: &mut File,
    offset: u64,
    file_len: u64,
) -> io::Result<Option<(Vec<u8>, u64)>> {
    if offset + 4 > file_len {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut lb = [0u8; 4];
    match file.read_exact(&mut lb) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let comp_len = u32::from_le_bytes(lb) as u64;
    if comp_len == 0 || comp_len > MAX_CHUNK_BYTES || offset + 4 + comp_len > file_len {
        return Ok(None);
    }
    let mut buf = vec![0u8; comp_len as usize];
    match file.read_exact(&mut buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    Ok(Some((buf, offset + 4 + comp_len)))
}

/// Decode all sealed chunks from a chunked session file starting at offset 4
/// (immediately after HEAD_MAGIC). EOF-tolerant: a truncated or undecodable
/// chunk stops the scan and returns whatever was recovered.
pub fn scan_chunks(file: &mut File) -> io::Result<Vec<Event>> {
    let file_len = file.seek(SeekFrom::End(0))?;
    let mut out = Vec::new();
    let mut cursor = HEAD_MAGIC.len() as u64;
    while let Some((bytes, next)) = read_chunk_at(file, cursor, file_len)? {
        match decode_chunk(&bytes) {
            Ok(mut evs) => out.append(&mut evs),
            Err(_) => break, // terminal torn chunk → clean truncation
        }
        cursor = next;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::write_frame;
    use crate::event::{Event, Payload, Source};
    use std::io::Write;
    use uuid::Uuid;

    fn ev(ts: u64, src: Source) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: src,
            pid: None,
            seq: ts,
            payload: Payload::Mark {
                label: "m".into(),
                fields: Default::default(),
            },
        }
    }

    // Build one in-memory chunk frame from events; returns compressed bytes.
    fn make_chunk(events: &[Event]) -> Vec<u8> {
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
        for e in events {
            write_frame(&mut enc, e).unwrap();
        }
        enc.finish().unwrap()
    }

    #[test]
    fn decode_chunk_roundtrips() {
        let evs = vec![ev(1, Source::Mark), ev(2, Source::MetalHook)];
        let comp = make_chunk(&evs);
        let got = decode_chunk(&comp).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ts_mono_ns, 1);
        assert_eq!(got[1].source, Source::MetalHook);
    }

    #[test]
    fn footer_write_read_roundtrip() {
        let entries = vec![
            ChunkIndexEntry {
                offset: 4,
                comp_len: 10,
                min_ts: 1,
                max_ts: 5,
                source_bitmap: 0b1,
                event_count: 3,
            },
            ChunkIndexEntry {
                offset: 18,
                comp_len: 20,
                min_ts: 6,
                max_ts: 9,
                source_bitmap: 0b10,
                event_count: 2,
            },
        ];
        // Simulate a file: HEAD_MAGIC (4 bytes) + 34 stand-in chunk bytes = footer_offset=38
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.zst");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&HEAD_MAGIC).unwrap();
            f.write_all(&[0u8; 34]).unwrap(); // stand-in chunk bytes up to footer_offset=38
            write_footer_at(&mut f, 38, &entries).unwrap();
        }
        let mut f = std::fs::File::open(&path).unwrap();
        let got = read_footer(&mut f).unwrap().expect("footer present");
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].max_ts, 9);
        assert_eq!(got[0].source_bitmap, 0b1);
    }

    #[test]
    fn read_footer_none_when_no_trailer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.zst");
        std::fs::write(&path, b"SMz\x01some chunk bytes no trailer here..").unwrap();
        let mut f = std::fs::File::open(&path).unwrap();
        assert!(read_footer(&mut f).unwrap().is_none());
    }

    #[test]
    fn read_footer_none_when_too_short() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.zst");
        std::fs::write(&path, b"SMz\x01").unwrap();
        let mut f = std::fs::File::open(&path).unwrap();
        assert!(read_footer(&mut f).unwrap().is_none());
    }

    #[test]
    fn read_footer_corrupt_crc_is_err() {
        let entries = vec![ChunkIndexEntry {
            offset: 4,
            comp_len: 1,
            min_ts: 0,
            max_ts: 0,
            source_bitmap: 1,
            event_count: 1,
        }];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.zst");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&HEAD_MAGIC).unwrap();
            f.write_all(&[0u8; 1]).unwrap();
            // footer_offset = 4 (HEAD) + 1 (stand-in) = 5
            write_footer_at(&mut f, 5, &entries).unwrap();
        }
        // Corrupt one footer byte (flip a byte inside the footer region, offset 5).
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[5] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        let mut f = std::fs::File::open(&path).unwrap();
        assert!(matches!(
            read_footer(&mut f),
            Err(SessionFormatError::FooterCorrupt(_))
        ));
    }

    #[test]
    fn detect_distinguishes_formats() {
        let dir = tempfile::tempdir().unwrap();
        let chunked = dir.path().join("c.zst");
        std::fs::write(&chunked, b"SMz\x01....").unwrap();
        let mut f = std::fs::File::open(&chunked).unwrap();
        assert!(matches!(detect(&mut f).unwrap(), Format::Chunked));

        let legacy = dir.path().join("l.zst");
        std::fs::write(&legacy, [0x28, 0xB5, 0x2F, 0xFD, 0, 0]).unwrap();
        let mut f = std::fs::File::open(&legacy).unwrap();
        assert!(matches!(detect(&mut f).unwrap(), Format::Legacy));

        let bad = dir.path().join("v.zst");
        std::fs::write(&bad, b"SMz\x09..").unwrap();
        let mut f = std::fs::File::open(&bad).unwrap();
        assert!(matches!(detect(&mut f).unwrap(), Format::Unsupported(9)));
    }

    #[test]
    fn scan_chunks_recovers_sealed_and_tolerates_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.zst");
        let c1 = make_chunk(&[ev(1, Source::Mark), ev(2, Source::Mark)]);
        let c2 = make_chunk(&[ev(3, Source::MetalHook)]);
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&HEAD_MAGIC).unwrap();
            f.write_all(&(c1.len() as u32).to_le_bytes()).unwrap();
            f.write_all(&c1).unwrap();
            f.write_all(&(c2.len() as u32).to_le_bytes()).unwrap();
            f.write_all(&c2).unwrap();
            // a torn third chunk: comp_len says 50 but only 3 bytes follow
            f.write_all(&50u32.to_le_bytes()).unwrap();
            f.write_all(&[1, 2, 3]).unwrap();
        }
        let mut f = std::fs::File::open(&path).unwrap();
        let got = scan_chunks(&mut f).unwrap();
        assert_eq!(
            got.len(),
            3,
            "two sealed chunks (2+1 events), torn chunk ignored"
        );
        assert_eq!(got[2].ts_mono_ns, 3);
    }
}
