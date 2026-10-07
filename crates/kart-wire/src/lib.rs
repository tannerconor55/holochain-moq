//! Turbo Kart Rally realtime wire format, version 1 (PROTOCOL.md "Game wire
//! format (WP3)").
//!
//! * **Big-endian** throughout (network order; JavaScript `DataView` default).
//! * Floats are IEEE-754 binary32; NaN and infinities are rejected.
//! * Fixed field order and sizes; nothing depends on Rust memory layout.
//! * Every message starts with a version byte. An unknown version is
//!   rejected, never guessed at.
//! * Strict lengths: a kart state is exactly [`KART_STATE_LEN`] bytes; an
//!   event's `len` must match its type's payload size exactly (variable-size
//!   payloads are self-delimiting and must consume exactly `len`).
//! * Decoding never panics and never allocates more than the input it was given.
//!
//! The publisher's identity is **not** in these messages: it is the track
//! they arrive on (`race/<race>/<agent|ai-N>/<track>`), authenticated by the
//! transport. Who may send what is checked by the game layer (later WPs).

use std::fmt;

/// The wire version this crate speaks.
pub const VERSION: u8 = 1;

// ------------------------------------------------------------------ errors

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than the message needs.
    Truncated { needed: usize, got: usize },
    /// More bytes than the message is defined to have.
    TrailingBytes { expected: usize, got: usize },
    /// The first byte is not a version this decoder knows.
    UnknownVersion(u8),
    /// An event's `len` exceeds [`MAX_EVENT_PAYLOAD`].
    PayloadTooLarge(usize),
    /// An event's `len` does not match its type's payload size.
    WrongPayloadLength {
        event_type: u8,
        expected: usize,
        got: usize,
    },
    /// A float field is NaN or infinite.
    NonFinite(&'static str),
    /// A field holds a value outside its defined range.
    OutOfRange(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for DecodeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    OutOfRange(&'static str),
    NonFinite(&'static str),
    PayloadTooLarge(usize),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for EncodeError {}

// ----------------------------------------------------------------- helpers

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|e| *e <= self.b.len())
            .ok_or(DecodeError::Truncated {
                needed: self.at + n,
                got: self.b.len(),
            })?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f32(&mut self, field: &'static str) -> Result<f32, DecodeError> {
        let v = f32::from_bits(self.u32()?);
        if !v.is_finite() {
            return Err(DecodeError::NonFinite(field));
        }
        Ok(v)
    }
    fn vec3(&mut self, field: &'static str) -> Result<[f32; 3], DecodeError> {
        Ok([self.f32(field)?, self.f32(field)?, self.f32(field)?])
    }
    fn agent(&mut self) -> Result<AgentKey, DecodeError> {
        Ok(AgentKey(self.take(AGENT_KEY_LEN)?.try_into().unwrap()))
    }
    fn done(&self) -> Result<(), DecodeError> {
        if self.at != self.b.len() {
            return Err(DecodeError::TrailingBytes {
                expected: self.at,
                got: self.b.len(),
            });
        }
        Ok(())
    }
}

fn put_f32(out: &mut Vec<u8>, v: f32, field: &'static str) -> Result<(), EncodeError> {
    if !v.is_finite() {
        return Err(EncodeError::NonFinite(field));
    }
    out.extend_from_slice(&v.to_bits().to_be_bytes());
    Ok(())
}

fn put_vec3(out: &mut Vec<u8>, v: [f32; 3], field: &'static str) -> Result<(), EncodeError> {
    for c in v {
        put_f32(out, c, field)?;
    }
    Ok(())
}

// ------------------------------------------------------------- kart state

/// Bytes in a version-1 kart state.
pub const KART_STATE_LEN: usize = 42;

/// `flags` bits.
pub mod flags {
    pub const AIRBORNE: u8 = 1 << 0;
    pub const DRIFTING: u8 = 1 << 1;
    /// Set: drifting right; clear: left. Meaningful only with `DRIFTING`.
    pub const DRIFT_RIGHT: u8 = 1 << 2;
    pub const BOOSTING: u8 = 1 << 3;
    pub const STAR: u8 = 1 << 4;
    pub const SHRUNK: u8 = 1 << 5;
    pub const SPINNING: u8 = 1 << 6;
    pub const FINISHED: u8 = 1 << 7;
}

/// Held item ids (`config.js` ITEMS order; 0 = nothing).
pub mod item {
    pub const NONE: u8 = 0;
    pub const MUSHROOM: u8 = 1;
    pub const TRIPLE_MUSHROOM: u8 = 2;
    pub const BANANA: u8 = 3;
    pub const GREEN_SHELL: u8 = 4;
    pub const RED_SHELL: u8 = 5;
    pub const STAR: u8 = 6;
    pub const LIGHTNING: u8 = 7;
    pub const BLUE_SHELL: u8 = 8;
    pub const MAX: u8 = BLUE_SHELL;
}

/// Highest mini-turbo drift level (blue, orange, purple).
pub const MAX_DRIFT_LEVEL: u8 = 3;
/// Most items a kart can hold at once (triple mushroom).
pub const MAX_ITEM_COUNT: u8 = 3;

/// One kart's state at one simulation tick (`Latest` track `state`).
///
/// ```text
/// off size field
///  0   1   version   = 1
///  1   1   flags     (see `flags`)
///  2   4   tick      u32, simulation tick (60 Hz) of this snapshot
///  6  12   pos       3 x f32, metres, Y up
/// 18  12   vel       3 x f32, metres/s
/// 30   4   heading   f32, radians (object3D.rotation.y)
/// 34   1   drift     u8, 0..=3 mini-turbo level
/// 35   1   item      u8, 0..=8 (see `item`)
/// 36   1   itemCount u8, 0..=3
/// 37   1   lap       u8
/// 38   4   trackT    f32, [0, 1) progress along the centreline
/// 42       end
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KartState {
    pub flags: u8,
    pub tick: u32,
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    pub heading: f32,
    pub drift: u8,
    pub item: u8,
    pub item_count: u8,
    pub lap: u8,
    pub track_t: f32,
}

impl KartState {
    pub fn encode(&self) -> Result<[u8; KART_STATE_LEN], EncodeError> {
        self.check().map_err(|e| match e {
            DecodeError::NonFinite(f) => EncodeError::NonFinite(f),
            DecodeError::OutOfRange(f) => EncodeError::OutOfRange(f),
            _ => unreachable!("check only reports ranges"),
        })?;
        let mut out = Vec::with_capacity(KART_STATE_LEN);
        out.push(VERSION);
        out.push(self.flags);
        out.extend_from_slice(&self.tick.to_be_bytes());
        put_vec3(&mut out, self.pos, "pos")?;
        put_vec3(&mut out, self.vel, "vel")?;
        put_f32(&mut out, self.heading, "heading")?;
        out.extend_from_slice(&[self.drift, self.item, self.item_count, self.lap]);
        put_f32(&mut out, self.track_t, "trackT")?;
        Ok(out.try_into().expect("exactly KART_STATE_LEN bytes"))
    }

    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(b);
        let version = r.u8()?;
        if version != VERSION {
            return Err(DecodeError::UnknownVersion(version));
        }
        if b.len() != KART_STATE_LEN {
            return Err(if b.len() < KART_STATE_LEN {
                DecodeError::Truncated {
                    needed: KART_STATE_LEN,
                    got: b.len(),
                }
            } else {
                DecodeError::TrailingBytes {
                    expected: KART_STATE_LEN,
                    got: b.len(),
                }
            });
        }
        let s = Self {
            flags: r.u8()?,
            tick: r.u32()?,
            pos: r.vec3("pos")?,
            vel: r.vec3("vel")?,
            heading: r.f32("heading")?,
            drift: r.u8()?,
            item: r.u8()?,
            item_count: r.u8()?,
            lap: r.u8()?,
            track_t: r.f32("trackT")?,
        };
        r.done()?;
        s.check()?;
        Ok(s)
    }

    fn check(&self) -> Result<(), DecodeError> {
        for (v, f) in [(self.heading, "heading"), (self.track_t, "trackT")]
            .into_iter()
            .chain(self.pos.iter().map(|v| (*v, "pos")))
            .chain(self.vel.iter().map(|v| (*v, "vel")))
        {
            if !v.is_finite() {
                return Err(DecodeError::NonFinite(f));
            }
        }
        if self.drift > MAX_DRIFT_LEVEL {
            return Err(DecodeError::OutOfRange("drift"));
        }
        if self.item > item::MAX {
            return Err(DecodeError::OutOfRange("item"));
        }
        if self.item_count > MAX_ITEM_COUNT {
            return Err(DecodeError::OutOfRange("itemCount"));
        }
        if !(0.0..1.0).contains(&self.track_t) {
            return Err(DecodeError::OutOfRange("trackT"));
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ events

/// Bytes before an event's payload.
pub const EVENT_HEADER_LEN: usize = 12;
/// Largest event payload (`len`). The whole event stays far below the mesh's
/// 4 KiB `ReliableOrdered` payload limit.
pub const MAX_EVENT_PAYLOAD: usize = 1024;
/// Raw Holochain `AgentPubKey` length (3-byte prefix + 32-byte key + 4-byte location).
pub const AGENT_KEY_LEN: usize = 39;
/// Longest `RaceStart.track_id`.
pub const MAX_TRACK_ID_LEN: usize = 32;
/// Most grid slots in `RaceStart`.
pub const MAX_GRID: usize = 8;

/// Raw Holochain agent key bytes, opaque here.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentKey(pub [u8; AGENT_KEY_LEN]);

impl fmt::Debug for AgentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AgentKey(")?;
        for b in &self.0[..6] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "..)")
    }
}

/// Event type codes. 0 is reserved (never valid on the wire).
pub mod event_type {
    pub const RACE_START: u8 = 1;
    pub const HOST_CHANGED: u8 = 2;
    pub const ITEM_PICKUP: u8 = 3;
    pub const ITEM_USE: u8 = 4;
    pub const PROJECTILE_SPAWN: u8 = 5;
    pub const HIT: u8 = 6;
    pub const LAP: u8 = 7;
    pub const FINISH: u8 = 8;
    pub const LEAVE: u8 = 9;
}

/// `NO_TARGET` in `ProjectileSpawn.target_slot`.
pub const NO_TARGET: u8 = 0xff;
/// `Finish.best_lap_ms` when no lap was completed.
pub const NO_TIME: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridSlot {
    Player(AgentKey),
    Ai(u8),
}

/// Event payloads, version 1. Minimal: full game semantics come later.
#[derive(Debug, Clone, PartialEq)]
pub enum EventBody {
    /// Host only. `start_at_ms` is the host's clock (WP4 converts it).
    RaceStart {
        start_at_ms: u64,
        seed: u64,
        laps: u8,
        class: u8,
        track_id: String,
        grid: Vec<GridSlot>,
    },
    /// Host only; the envelope `tick` is the tick it takes effect.
    HostChanged {
        new_host: AgentKey,
    },
    ItemPickup {
        box_index: u16,
        pickup_index: u16,
        item: u8,
    },
    ItemUse {
        item: u8,
    },
    ProjectileSpawn {
        projectile_id: u32,
        kind: u8,
        pos: [f32; 3],
        vel: [f32; 3],
        target_slot: u8,
    },
    /// Sent by the victim (victim-authoritative hits).
    Hit {
        kind: u8,
        by_slot: u8,
        projectile_id: u32,
    },
    Lap {
        lap: u8,
        lap_ms: u32,
    },
    Finish {
        finish_ms: u32,
        best_lap_ms: u32,
    },
    Leave {
        reason: u8,
    },
    /// A type this decoder does not know, kept verbatim so a receiver can
    /// skip (and count) it. Types are only ever added, never re-used; a
    /// change to an existing type's payload needs a new [`VERSION`].
    Unknown {
        event_type: u8,
        payload: Vec<u8>,
    },
}

/// One race event (`ReliableOrdered` track `events`).
///
/// ```text
/// off size field
///  0   1   version  = 1
///  1   1   type     (see `event_type`)
///  2   4   seq      u32, 0, 1, 2, ... per (race, publisher); dedupe key
///  6   4   tick     u32, simulation tick the event belongs to
/// 10   2   len      u16, payload bytes, <= MAX_EVENT_PAYLOAD
/// 12 len   payload
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub seq: u32,
    pub tick: u32,
    pub body: EventBody,
}

impl EventBody {
    pub fn event_type(&self) -> u8 {
        use event_type::*;
        match self {
            Self::RaceStart { .. } => RACE_START,
            Self::HostChanged { .. } => HOST_CHANGED,
            Self::ItemPickup { .. } => ITEM_PICKUP,
            Self::ItemUse { .. } => ITEM_USE,
            Self::ProjectileSpawn { .. } => PROJECTILE_SPAWN,
            Self::Hit { .. } => HIT,
            Self::Lap { .. } => LAP,
            Self::Finish { .. } => FINISH,
            Self::Leave { .. } => LEAVE,
            Self::Unknown { event_type, .. } => *event_type,
        }
    }

    /// Payload size for fixed-size types.
    fn fixed_len(event_type: u8) -> Option<usize> {
        use event_type::*;
        Some(match event_type {
            HOST_CHANGED => AGENT_KEY_LEN,
            ITEM_PICKUP => 5,
            ITEM_USE => 1,
            PROJECTILE_SPAWN => 30,
            HIT => 6,
            LAP => 5,
            FINISH => 8,
            LEAVE => 1,
            _ => return None,
        })
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        match self {
            Self::RaceStart {
                start_at_ms,
                seed,
                laps,
                class,
                track_id,
                grid,
            } => {
                if track_id.len() > MAX_TRACK_ID_LEN || !track_id.is_ascii() {
                    return Err(EncodeError::OutOfRange("track_id"));
                }
                if grid.len() > MAX_GRID {
                    return Err(EncodeError::OutOfRange("grid"));
                }
                out.extend_from_slice(&start_at_ms.to_be_bytes());
                out.extend_from_slice(&seed.to_be_bytes());
                out.extend_from_slice(&[*laps, *class, track_id.len() as u8]);
                out.extend_from_slice(track_id.as_bytes());
                out.push(grid.len() as u8);
                for slot in grid {
                    match slot {
                        GridSlot::Player(k) => {
                            out.push(0);
                            out.extend_from_slice(&k.0);
                        }
                        GridSlot::Ai(n) => out.extend_from_slice(&[1, *n]),
                    }
                }
            }
            Self::HostChanged { new_host } => out.extend_from_slice(&new_host.0),
            Self::ItemPickup {
                box_index,
                pickup_index,
                item,
            } => {
                out.extend_from_slice(&box_index.to_be_bytes());
                out.extend_from_slice(&pickup_index.to_be_bytes());
                out.push(*item);
            }
            Self::ItemUse { item } => out.push(*item),
            Self::ProjectileSpawn {
                projectile_id,
                kind,
                pos,
                vel,
                target_slot,
            } => {
                out.extend_from_slice(&projectile_id.to_be_bytes());
                out.push(*kind);
                put_vec3(out, *pos, "pos")?;
                put_vec3(out, *vel, "vel")?;
                out.push(*target_slot);
            }
            Self::Hit {
                kind,
                by_slot,
                projectile_id,
            } => {
                out.extend_from_slice(&[*kind, *by_slot]);
                out.extend_from_slice(&projectile_id.to_be_bytes());
            }
            Self::Lap { lap, lap_ms } => {
                out.push(*lap);
                out.extend_from_slice(&lap_ms.to_be_bytes());
            }
            Self::Finish {
                finish_ms,
                best_lap_ms,
            } => {
                out.extend_from_slice(&finish_ms.to_be_bytes());
                out.extend_from_slice(&best_lap_ms.to_be_bytes());
            }
            Self::Leave { reason } => out.push(*reason),
            Self::Unknown {
                event_type,
                payload,
            } => {
                if *event_type == 0
                    || Self::fixed_len(*event_type).is_some()
                    || *event_type == event_type::RACE_START
                {
                    return Err(EncodeError::OutOfRange("event_type"));
                }
                out.extend_from_slice(payload);
            }
        }
        Ok(())
    }

    fn decode_payload(event_type: u8, p: &[u8]) -> Result<Self, DecodeError> {
        use event_type::*;
        if let Some(n) = Self::fixed_len(event_type) {
            if p.len() != n {
                return Err(DecodeError::WrongPayloadLength {
                    event_type,
                    expected: n,
                    got: p.len(),
                });
            }
        }
        let mut r = Reader::new(p);
        let body = match event_type {
            0 => return Err(DecodeError::OutOfRange("type")),
            RACE_START => {
                let start_at_ms = r.u64()?;
                let seed = r.u64()?;
                let laps = r.u8()?;
                let class = r.u8()?;
                let n = r.u8()? as usize;
                if n > MAX_TRACK_ID_LEN {
                    return Err(DecodeError::OutOfRange("track_id"));
                }
                let id = r.take(n)?;
                if !id.is_ascii() {
                    return Err(DecodeError::OutOfRange("track_id"));
                }
                let track_id = String::from_utf8(id.to_vec()).expect("ascii");
                let g = r.u8()? as usize;
                if g > MAX_GRID {
                    return Err(DecodeError::OutOfRange("grid"));
                }
                let mut grid = Vec::with_capacity(g);
                for _ in 0..g {
                    grid.push(match r.u8()? {
                        0 => GridSlot::Player(r.agent()?),
                        1 => GridSlot::Ai(r.u8()?),
                        _ => return Err(DecodeError::OutOfRange("grid slot kind")),
                    });
                }
                EventBody::RaceStart {
                    start_at_ms,
                    seed,
                    laps,
                    class,
                    track_id,
                    grid,
                }
            }
            HOST_CHANGED => EventBody::HostChanged {
                new_host: r.agent()?,
            },
            ITEM_PICKUP => {
                let box_index = r.u16()?;
                let pickup_index = r.u16()?;
                let item = r.u8()?;
                if item > item::MAX {
                    return Err(DecodeError::OutOfRange("item"));
                }
                EventBody::ItemPickup {
                    box_index,
                    pickup_index,
                    item,
                }
            }
            ITEM_USE => {
                let item = r.u8()?;
                if item == item::NONE || item > item::MAX {
                    return Err(DecodeError::OutOfRange("item"));
                }
                EventBody::ItemUse { item }
            }
            PROJECTILE_SPAWN => EventBody::ProjectileSpawn {
                projectile_id: r.u32()?,
                kind: r.u8()?,
                pos: r.vec3("pos")?,
                vel: r.vec3("vel")?,
                target_slot: r.u8()?,
            },
            HIT => EventBody::Hit {
                kind: r.u8()?,
                by_slot: r.u8()?,
                projectile_id: r.u32()?,
            },
            LAP => EventBody::Lap {
                lap: r.u8()?,
                lap_ms: r.u32()?,
            },
            FINISH => EventBody::Finish {
                finish_ms: r.u32()?,
                best_lap_ms: r.u32()?,
            },
            LEAVE => EventBody::Leave { reason: r.u8()? },
            other => {
                return Ok(EventBody::Unknown {
                    event_type: other,
                    payload: p.to_vec(),
                })
            }
        };
        r.done()?;
        Ok(body)
    }
}

impl Event {
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let mut payload = Vec::new();
        self.body.encode_payload(&mut payload)?;
        if payload.len() > MAX_EVENT_PAYLOAD {
            return Err(EncodeError::PayloadTooLarge(payload.len()));
        }
        let mut out = Vec::with_capacity(EVENT_HEADER_LEN + payload.len());
        out.push(VERSION);
        out.push(self.body.event_type());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.tick.to_be_bytes());
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }

    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(b);
        let version = r.u8()?;
        if version != VERSION {
            return Err(DecodeError::UnknownVersion(version));
        }
        let event_type = r.u8()?;
        let seq = r.u32()?;
        let tick = r.u32()?;
        let len = r.u16()? as usize;
        // Checked before anything is allocated or sliced for the payload.
        if len > MAX_EVENT_PAYLOAD {
            return Err(DecodeError::PayloadTooLarge(len));
        }
        let payload = r.take(len)?;
        r.done()?;
        Ok(Self {
            seq,
            tick,
            body: EventBody::decode_payload(event_type, payload)?,
        })
    }
}

/// Receiver-side ordering check for one (race, publisher) event stream:
/// accepts `0, 1, 2, ...`, reports duplicates and gaps. The mesh already
/// guarantees order; this is the game layer's own dedupe (brief S4).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EventSequence {
    next: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqCheck {
    /// The next event: apply it.
    Next,
    /// Already applied: ignore it.
    Duplicate,
    /// Events between were never seen.
    Gap { expected: u32 },
}

impl EventSequence {
    pub fn check(&mut self, seq: u32) -> SeqCheck {
        if seq < self.next {
            SeqCheck::Duplicate
        } else if seq == self.next {
            self.next = self.next.wrapping_add(1);
            SeqCheck::Next
        } else {
            SeqCheck::Gap {
                expected: self.next,
            }
        }
    }
}
