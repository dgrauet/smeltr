use crate::error::RingError;
use crate::wire::kind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedOpSample {
    pub name: String,
    pub symbol: Option<String>,
    pub gpu_ns: u64,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedFrame {
    CbCommitted {
        cb_id: u64,
        queue_id: u64,
        queue_depth: u32,
        label: Option<String>,
    },
    CbScheduled {
        cb_id: u64,
        queue_id: u64,
    },
    CbCompleted {
        cb_id: u64,
        queue_id: u64,
        status: u32,
        error_code: Option<i64>,
        error_domain: Option<String>,
        in_flight_ns: u64,
    },
    CbWarning {
        cb_id: u64,
        queue_id: u64,
        elapsed_ns: u64,
    },
    CbOps {
        cb_id: u64,
        ops: Vec<DecodedOpSample>,
    },
    HeapAlloc {
        heap_id: u64,
        size_bytes: u64,
        label: Option<String>,
    },
    HeapFree {
        heap_id: u64,
    },
    BufferAlloc {
        buffer_id: u64,
        heap_id: Option<u64>,
        size_bytes: u64,
        label: Option<String>,
    },
    BufferFree {
        buffer_id: u64,
    },
    TextureAlloc {
        texture_id: u64,
        heap_id: Option<u64>,
        size_bytes: u64,
        label: Option<String>,
    },
    TextureFree {
        texture_id: u64,
    },
    Dropped {
        count: u64,
    },
    Skipped {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedEvent {
    pub ts_mono_ns: u64,
    pub frame: DecodedFrame,
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn read_u32(&mut self) -> Result<u32, RingError> {
        if self.pos + 4 > self.buf.len() {
            return Err(RingError::Truncated(self.pos as u64));
        }
        let v = u32::from_le_bytes(self.buf[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Ok(v)
    }
    fn read_i32(&mut self) -> Result<i32, RingError> {
        Ok(self.read_u32()? as i32)
    }
    fn read_u64(&mut self) -> Result<u64, RingError> {
        if self.pos + 8 > self.buf.len() {
            return Err(RingError::Truncated(self.pos as u64));
        }
        let v = u64::from_le_bytes(self.buf[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        Ok(v)
    }
    fn read_i64(&mut self) -> Result<i64, RingError> {
        Ok(self.read_u64()? as i64)
    }
    fn read_label(&mut self) -> Result<Option<String>, RingError> {
        let len = self.read_u32()? as usize;
        if len == 0 {
            return Ok(None);
        }
        if self.pos + len > self.buf.len() {
            return Err(RingError::Truncated(self.pos as u64));
        }
        let s = std::str::from_utf8(&self.buf[self.pos..self.pos + len])?.to_string();
        self.pos += len;
        Ok(Some(s))
    }
    fn read_opt_u64(&mut self) -> Result<Option<u64>, RingError> {
        let present = self.read_u32()? != 0;
        let v = self.read_u64()?;
        Ok(if present { Some(v) } else { None })
    }
    fn read_opt_i64(&mut self) -> Result<Option<i64>, RingError> {
        let present = self.read_i32()? != 0;
        let v = self.read_i64()?;
        Ok(if present { Some(v) } else { None })
    }
}

pub fn decode_frame(kind_val: u32, payload: &[u8]) -> Result<DecodedFrame, RingError> {
    let mut c = Cursor {
        buf: payload,
        pos: 0,
    };
    let f = match kind_val {
        k if k == kind::CB_COMMITTED => DecodedFrame::CbCommitted {
            cb_id: c.read_u64()?,
            queue_id: c.read_u64()?,
            queue_depth: c.read_u32()?,
            label: c.read_label()?,
        },
        k if k == kind::CB_SCHEDULED => DecodedFrame::CbScheduled {
            cb_id: c.read_u64()?,
            queue_id: c.read_u64()?,
        },
        k if k == kind::CB_COMPLETED => DecodedFrame::CbCompleted {
            cb_id: c.read_u64()?,
            queue_id: c.read_u64()?,
            status: c.read_u32()?,
            error_code: c.read_opt_i64()?,
            error_domain: c.read_label()?,
            in_flight_ns: c.read_u64()?,
        },
        k if k == kind::CB_WARNING => DecodedFrame::CbWarning {
            cb_id: c.read_u64()?,
            queue_id: c.read_u64()?,
            elapsed_ns: c.read_u64()?,
        },
        k if k == kind::HEAP_ALLOC => DecodedFrame::HeapAlloc {
            heap_id: c.read_u64()?,
            size_bytes: c.read_u64()?,
            label: c.read_label()?,
        },
        k if k == kind::HEAP_FREE => DecodedFrame::HeapFree {
            heap_id: c.read_u64()?,
        },
        k if k == kind::BUFFER_ALLOC => DecodedFrame::BufferAlloc {
            buffer_id: c.read_u64()?,
            heap_id: c.read_opt_u64()?,
            size_bytes: c.read_u64()?,
            label: c.read_label()?,
        },
        k if k == kind::BUFFER_FREE => DecodedFrame::BufferFree {
            buffer_id: c.read_u64()?,
        },
        k if k == kind::TEXTURE_ALLOC => DecodedFrame::TextureAlloc {
            texture_id: c.read_u64()?,
            heap_id: c.read_opt_u64()?,
            size_bytes: c.read_u64()?,
            label: c.read_label()?,
        },
        k if k == kind::TEXTURE_FREE => DecodedFrame::TextureFree {
            texture_id: c.read_u64()?,
        },
        k if k == kind::CB_OPS => {
            let cb_id = c.read_u64()?;
            let op_count = c.read_u32()? as usize;
            let mut ops = Vec::with_capacity(op_count);
            for _ in 0..op_count {
                let name_len = c.read_u32()? as usize;
                if c.pos + name_len > c.buf.len() {
                    return Err(RingError::Truncated(c.pos as u64));
                }
                let name = std::str::from_utf8(&c.buf[c.pos..c.pos + name_len])?.to_string();
                c.pos += name_len;

                let symbol_len_raw = c.read_u32()?;
                let symbol = if symbol_len_raw == crate::wire::CB_OPS_SYMBOL_LEN_NONE {
                    None
                } else {
                    let symbol_len = symbol_len_raw as usize;
                    if c.pos + symbol_len > c.buf.len() {
                        return Err(RingError::Truncated(c.pos as u64));
                    }
                    let s = std::str::from_utf8(&c.buf[c.pos..c.pos + symbol_len])?.to_string();
                    c.pos += symbol_len;
                    Some(s)
                };

                let gpu_ns = c.read_u64()?;
                let count = c.read_u32()?;
                ops.push(DecodedOpSample {
                    name,
                    symbol,
                    gpu_ns,
                    count,
                });
            }
            DecodedFrame::CbOps { cb_id, ops }
        }
        k if k == kind::DROPPED => DecodedFrame::Dropped {
            count: c.read_u64()?,
        },
        k if k == kind::SKIPPED => DecodedFrame::Skipped {
            reason: c.read_label()?.unwrap_or_default(),
        },
        other => return Err(RingError::UnknownKind(other)),
    };
    Ok(f)
}
