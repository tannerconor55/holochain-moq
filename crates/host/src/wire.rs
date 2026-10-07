//! Host-authority messages, version 1. Big-endian, bounded, no game payload.
//!
//! ```text
//! Heartbeat  (Latest track `host-hb`, from the host)        19 + agent bytes
//!   0 version=1 | 1 type=1 | 2 epoch u32 | 6 seq u32 | 10 host_time_us u64 | 18 len u8 | 19 host agent
//!
//! HostChanged (ReliableOrdered track `host-ctl`, from the new host)   11 + both agents
//!   0 version=1 | 1 type=2 | 2 epoch u32 | 6 len u8 | 7 new host agent
//!   | prev_epoch u32 | len u8 | previous host agent
//!
//! AiFrame (Latest track `race/<r>/ai-<slot>/state`, from the host)    9 + payload
//!   0 version=1 | 1 type=3 | 2 epoch u32 | 6 slot u8 | 7 len u16 | 9 payload (opaque, game-defined)
//! ```
//!
//! Agent keys are the mesh's `AgentPubKey` text (ASCII letters, digits, `-`,
//! `_`; 1–64 bytes). The sender of every message is also authenticated by the
//! track it arrives on; receivers check the two agree.

use bytes::Bytes;

pub const VERSION: u8 = 1;
pub const TYPE_HEARTBEAT: u8 = 1;
pub const TYPE_HOST_CHANGED: u8 = 2;
pub const TYPE_AI_FRAME: u8 = 3;
pub const MAX_AGENT_LEN: usize = 64;
/// Fits one `Latest` datagram (1024 B) with the 9-byte header.
pub const MAX_AI_PAYLOAD: usize = 1015;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    pub epoch: u32,
    pub seq: u32,
    /// The host's monotonic clock (WP4 `LocalClock`), microseconds. Diagnostics only.
    pub host_time_us: u64,
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostChanged {
    pub epoch: u32,
    pub new_host: String,
    pub prev_epoch: u32,
    pub prev_host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiFrame {
    pub epoch: u32,
    pub slot: u8,
    pub payload: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMessage {
    Heartbeat(Heartbeat),
    HostChanged(HostChanged),
    AiFrame(AiFrame),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    Truncated,
    TrailingBytes,
    UnknownVersion(u8),
    UnknownType(u8),
    /// Epoch 0 (never valid).
    ZeroEpoch,
    /// An agent key that is empty, too long, or not key-text.
    BadAgent,
    PayloadTooLarge,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for WireError {}

fn valid_agent(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= MAX_AGENT_LEN
        && a.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn put_agent(out: &mut Vec<u8>, a: &str) -> Result<(), WireError> {
    if !valid_agent(a) {
        return Err(WireError::BadAgent);
    }
    out.push(a.len() as u8);
    out.extend_from_slice(a.as_bytes());
    Ok(())
}

struct R<'a>(&'a [u8], usize);

impl R<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], WireError> {
        let end = self
            .1
            .checked_add(n)
            .filter(|e| *e <= self.0.len())
            .ok_or(WireError::Truncated)?;
        let s = &self.0[self.1..end];
        self.1 = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn epoch(&mut self) -> Result<u32, WireError> {
        match self.u32()? {
            0 => Err(WireError::ZeroEpoch),
            e => Ok(e),
        }
    }
    fn agent(&mut self) -> Result<String, WireError> {
        let n = self.u8()? as usize;
        if n == 0 || n > MAX_AGENT_LEN {
            return Err(WireError::BadAgent);
        }
        let s = std::str::from_utf8(self.take(n)?).map_err(|_| WireError::BadAgent)?;
        if !valid_agent(s) {
            return Err(WireError::BadAgent);
        }
        Ok(s.to_string())
    }
}

impl HostMessage {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut out = vec![VERSION];
        match self {
            Self::Heartbeat(h) => {
                if h.epoch == 0 {
                    return Err(WireError::ZeroEpoch);
                }
                out.push(TYPE_HEARTBEAT);
                out.extend_from_slice(&h.epoch.to_be_bytes());
                out.extend_from_slice(&h.seq.to_be_bytes());
                out.extend_from_slice(&h.host_time_us.to_be_bytes());
                put_agent(&mut out, &h.host)?;
            }
            Self::HostChanged(c) => {
                if c.epoch == 0 || c.prev_epoch == 0 {
                    return Err(WireError::ZeroEpoch);
                }
                out.push(TYPE_HOST_CHANGED);
                out.extend_from_slice(&c.epoch.to_be_bytes());
                put_agent(&mut out, &c.new_host)?;
                out.extend_from_slice(&c.prev_epoch.to_be_bytes());
                put_agent(&mut out, &c.prev_host)?;
            }
            Self::AiFrame(f) => {
                if f.epoch == 0 {
                    return Err(WireError::ZeroEpoch);
                }
                if f.payload.len() > MAX_AI_PAYLOAD {
                    return Err(WireError::PayloadTooLarge);
                }
                out.push(TYPE_AI_FRAME);
                out.extend_from_slice(&f.epoch.to_be_bytes());
                out.push(f.slot);
                out.extend_from_slice(&(f.payload.len() as u16).to_be_bytes());
                out.extend_from_slice(&f.payload);
            }
        }
        Ok(out)
    }

    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        let mut r = R(b, 0);
        let v = r.u8()?;
        if v != VERSION {
            return Err(WireError::UnknownVersion(v));
        }
        let m = match r.u8()? {
            TYPE_HEARTBEAT => Self::Heartbeat(Heartbeat {
                epoch: r.epoch()?,
                seq: r.u32()?,
                host_time_us: r.u64()?,
                host: r.agent()?,
            }),
            TYPE_HOST_CHANGED => Self::HostChanged(HostChanged {
                epoch: r.epoch()?,
                new_host: r.agent()?,
                prev_epoch: r.epoch()?,
                prev_host: r.agent()?,
            }),
            TYPE_AI_FRAME => {
                let (epoch, slot, len) = (r.epoch()?, r.u8()?, r.u16()? as usize);
                if len > MAX_AI_PAYLOAD {
                    return Err(WireError::PayloadTooLarge);
                }
                Self::AiFrame(AiFrame {
                    epoch,
                    slot,
                    payload: Bytes::copy_from_slice(r.take(len)?),
                })
            }
            t => return Err(WireError::UnknownType(t)),
        };
        if r.1 != b.len() {
            return Err(WireError::TrailingBytes);
        }
        Ok(m)
    }
}
