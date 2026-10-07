//! Clock messages, version 1. Big-endian, fixed length, no allocation.
//!
//! ```text
//! ClockPing (22 bytes)                    ClockPong (38 bytes)
//! off size                                off size
//!  0   1  version = 1                      0   1  version = 1
//!  1   1  type    = 1                      1   1  type    = 2
//!  2   8  nonce   u64 (sampler session)    2   8  nonce   u64 (echoed)
//! 10   4  seq     u32                     10   4  seq     u32 (echoed)
//! 14   8  t0      u64 us, sender's clock  14   8  t0      u64 (echoed)
//!                                         22   8  t1      u64 us, responder's clock at receive
//!                                         30   8  t2      u64 us, responder's clock at send
//! ```
//!
//! `nonce` identifies one sampler (random per [`crate::ClockSampler`]); all
//! pongs share the host's clock track, so a sampler ignores pongs with
//! another nonce. Timestamps are microseconds on the named peer's
//! [`crate::LocalClock`], never wall-clock time.

use std::fmt;

pub const VERSION: u8 = 1;
pub const TYPE_PING: u8 = 1;
pub const TYPE_PONG: u8 = 2;
pub const PING_LEN: usize = 22;
pub const PONG_LEN: usize = 38;
/// Largest valid timestamp (~146 000 years of microseconds); keeps all
/// arithmetic far from overflow.
pub const MAX_TIME_US: u64 = 1 << 62;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockPing {
    pub nonce: u64,
    pub seq: u32,
    pub t0: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockPong {
    pub nonce: u64,
    pub seq: u32,
    pub t0: u64,
    pub t1: u64,
    pub t2: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockMessage {
    Ping(ClockPing),
    Pong(ClockPong),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    Empty,
    UnknownVersion(u8),
    UnknownType(u8),
    WrongLength {
        msg_type: u8,
        expected: usize,
        got: usize,
    },
    /// A timestamp above [`MAX_TIME_US`].
    TimestampOutOfRange,
    /// A pong whose responder sent before it received (`t2 < t1`).
    TimestampsOutOfOrder,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for WireError {}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

fn check_t(t: u64) -> Result<u64, WireError> {
    if t > MAX_TIME_US {
        return Err(WireError::TimestampOutOfRange);
    }
    Ok(t)
}

impl ClockPing {
    pub fn encode(&self) -> [u8; PING_LEN] {
        let mut b = [0u8; PING_LEN];
        b[0] = VERSION;
        b[1] = TYPE_PING;
        b[2..10].copy_from_slice(&self.nonce.to_be_bytes());
        b[10..14].copy_from_slice(&self.seq.to_be_bytes());
        b[14..22].copy_from_slice(&self.t0.to_be_bytes());
        b
    }
}

impl ClockPong {
    pub fn encode(&self) -> [u8; PONG_LEN] {
        let mut b = [0u8; PONG_LEN];
        b[0] = VERSION;
        b[1] = TYPE_PONG;
        b[2..10].copy_from_slice(&self.nonce.to_be_bytes());
        b[10..14].copy_from_slice(&self.seq.to_be_bytes());
        b[14..22].copy_from_slice(&self.t0.to_be_bytes());
        b[22..30].copy_from_slice(&self.t1.to_be_bytes());
        b[30..38].copy_from_slice(&self.t2.to_be_bytes());
        b
    }
}

impl ClockMessage {
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        let (&version, rest) = b.split_first().ok_or(WireError::Empty)?;
        if version != VERSION {
            return Err(WireError::UnknownVersion(version));
        }
        let &msg_type = rest.first().ok_or(WireError::WrongLength {
            msg_type: 0,
            expected: 2,
            got: b.len(),
        })?;
        let expected = match msg_type {
            TYPE_PING => PING_LEN,
            TYPE_PONG => PONG_LEN,
            other => return Err(WireError::UnknownType(other)),
        };
        if b.len() != expected {
            return Err(WireError::WrongLength {
                msg_type,
                expected,
                got: b.len(),
            });
        }
        let nonce = u64_at(b, 2);
        let seq = u32::from_be_bytes(b[10..14].try_into().unwrap());
        let t0 = check_t(u64_at(b, 14))?;
        if msg_type == TYPE_PING {
            return Ok(Self::Ping(ClockPing { nonce, seq, t0 }));
        }
        let t1 = check_t(u64_at(b, 22))?;
        let t2 = check_t(u64_at(b, 30))?;
        if t2 < t1 {
            return Err(WireError::TimestampsOutOfOrder);
        }
        Ok(Self::Pong(ClockPong {
            nonce,
            seq,
            t0,
            t1,
            t2,
        }))
    }
}
