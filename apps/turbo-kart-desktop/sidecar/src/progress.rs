//! E6.5a authoritative race progress: the host's standings snapshot (lane 6,
//! `Latest` track `progress`) and the lap/finish events Rust derives from it
//! (lane 7, `ReliableOrdered` track `progress-events`).
//!
//! The track geometry lives in the game (Three.js), so the host *renderer*
//! derives each racer's progress from host-observed positions. Rust decides
//! who may publish it (the admission's epoch-1 host only, never after host
//! loss), binds it to the race, epoch and schedule, and refuses any state the
//! race rules make impossible: wrong grid, more laps than configured, laps or
//! finishes going backwards, a lap faster than the track's `min_lap_ms`
//! (platform `TrackSpec`: Palm Cove 20 s), standings that are not the
//! deterministic order of the state itself. That is plausibility, not proof
//! of honest driving: a human's position is still that client's own claim.
//!
//! Body (renderer ↔ Rust, big-endian), `BODY_LEN` bytes:
//! ```text
//! 0 version=1 | 1 race status | 2 laps | 3 count=8 | 4..8 seq | 8..12 race_ms
//! then 8 × 20: 0 index | 1 kind (0 human, 1 ai) | 2 laps done | 3 checkpoint
//!              | 4 place | 5 state | 6..8 0 | 8..12 progress (laps × 65536)
//!              | 12..16 last lap ms | 16..20 finish ms (0 = none)
//! ```
//! Event (Rust → renderer), `EVENT_LEN` bytes:
//! ```text
//! 0 version=1 | 1 kind (1 lap, 2 finish, 3 race status) | 2 index | 3 laps done
//! | 4 place | 5 state or race status | 6..8 0 | 8..12 event id | 12..16 race ms
//! | 16..20 snapshot seq
//! ```
//! On the mesh both carry a 32-byte binding first: magic `TKPS`/`TKPE`,
//! race id (16 raw bytes), host epoch (u32), schedule id (u64).

pub const VERSION: u8 = 1;
pub const RACERS: usize = 8;
pub const RACER_LEN: usize = 20;
pub const BODY_LEN: usize = 12 + RACERS * RACER_LEN;
pub const EVENT_LEN: usize = 20;
pub const BINDING_LEN: usize = 32;
/// Progress is laps × this (fixed point).
pub const Q: u32 = 65536;
pub const CHECKPOINTS: u8 = 8;
/// Keep at most this many reliable events per race (8 racers × (5 laps + finish) + status changes).
pub const MAX_EVENTS: u32 = 128;

pub mod race_status { pub const SCHEDULED: u8 = 0; pub const RUNNING: u8 = 1; pub const FINISHING: u8 = 2; pub const COMPLETED: u8 = 3; pub const INTERRUPTED: u8 = 4; }
pub mod racer_state { pub const RACING: u8 = 0; pub const FINISHED: u8 = 1; pub const STALE: u8 = 2; pub const DISCONNECTED: u8 = 3; pub const DNF: u8 = 4; }
pub mod event_kind { pub const LAP: u8 = 1; pub const FINISH: u8 = 2; pub const STATUS: u8 = 3; }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct Racer { pub index: u8, pub kind: u8, pub laps: u8, pub checkpoint: u8, pub place: u8, pub state: u8, pub progress: u32, pub lap_ms: u32, pub finish_ms: u32 }

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Body { pub status: u8, pub laps: u8, pub seq: u32, pub race_ms: u32, pub racers: Vec<Racer> }

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ProgressEvent { pub kind: u8, pub index: u8, pub laps: u8, pub place: u8, pub state: u8, pub id: u32, pub race_ms: u32, pub seq: u32 }

/// What the immutable admission fixes about every snapshot of this race.
#[derive(Debug, Clone, Copy)]
pub struct Rules { pub laps: u8, pub humans: usize, pub min_lap_ms: u32 }

fn be32(b: &[u8]) -> u32 { u32::from_be_bytes(b[..4].try_into().unwrap()) }

impl Body {
    pub fn decode(b: &[u8]) -> Result<Self, String> {
        if b.len() != BODY_LEN { return Err(format!("progress body is {} bytes, not {BODY_LEN}", b.len())); }
        if b[0] != VERSION || b[3] as usize != RACERS { return Err("progress version/count".into()); }
        let racers = (0..RACERS).map(|i| {
            let r = &b[12 + i * RACER_LEN..12 + (i + 1) * RACER_LEN];
            if r[6] != 0 || r[7] != 0 { return Err("progress reserved bytes".to_string()); }
            Ok(Racer { index: r[0], kind: r[1], laps: r[2], checkpoint: r[3], place: r[4], state: r[5], progress: be32(&r[8..]), lap_ms: be32(&r[12..]), finish_ms: be32(&r[16..]) })
        }).collect::<Result<Vec<_>, _>>()?;
        Ok(Self { status: b[1], laps: b[2], seq: be32(&b[4..]), race_ms: be32(&b[8..]), racers })
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![VERSION, self.status, self.laps, self.racers.len() as u8];
        b.extend_from_slice(&self.seq.to_be_bytes()); b.extend_from_slice(&self.race_ms.to_be_bytes());
        for r in &self.racers {
            b.extend_from_slice(&[r.index, r.kind, r.laps, r.checkpoint, r.place, r.state, 0, 0]);
            for v in [r.progress, r.lap_ms, r.finish_ms] { b.extend_from_slice(&v.to_be_bytes()); }
        }
        b
    }

    /// The deterministic standings of this state: finishers by (finish ms,
    /// grid index), then everyone else by (progress desc, grid index).
    pub fn order(&self) -> Vec<u8> {
        let mut ix: Vec<&Racer> = self.racers.iter().collect();
        ix.sort_by(|a, b| {
            let fa = a.state == racer_state::FINISHED; let fb = b.state == racer_state::FINISHED;
            fb.cmp(&fa).then_with(|| if fa { a.finish_ms.cmp(&b.finish_ms) } else { b.progress.cmp(&a.progress) }).then(a.index.cmp(&b.index))
        });
        ix.iter().map(|r| r.index).collect()
    }

    /// Everything a single snapshot must satisfy under `rules`, and, given the
    /// previous accepted snapshot, everything that may only move forward.
    pub fn validate(&self, rules: &Rules, prev: Option<&Body>) -> Result<(), String> {
        use racer_state::*;
        if self.laps != rules.laps { return Err("laps differ from the admission".into()); }
        if self.status > race_status::INTERRUPTED { return Err("unknown race status".into()); }
        let last = u32::from(self.laps) + 1;
        for (i, r) in self.racers.iter().enumerate() {
            if r.index as usize != i { return Err("racers out of grid order".into()); }
            if r.kind != u8::from(i >= rules.humans) { return Err(format!("racer {i} human/AI kind differs from the grid")); }
            if r.state > DNF || r.checkpoint >= CHECKPOINTS || r.laps > self.laps { return Err(format!("racer {i} state/checkpoint/laps out of range")); }
            if r.progress > last.saturating_mul(Q).saturating_add(Q) { return Err(format!("racer {i} progress out of range")); }
            let finished = r.state == FINISHED;
            if finished != (r.finish_ms != 0) { return Err(format!("racer {i} finish time without a finish (or the reverse)")); }
            if finished && (r.laps != self.laps || r.finish_ms != r.lap_ms) { return Err(format!("racer {i} finished without completing {} laps", self.laps)); }
            if !finished && r.progress >= last * Q { return Err(format!("racer {i} is past the final line but not finished")); }
            if (r.laps == 0) != (r.lap_ms == 0) || r.lap_ms > self.race_ms { return Err(format!("racer {i} lap time inconsistent")); }
            // No lap can be faster than the track allows (platform TrackSpec).
            if r.laps > 0 && u64::from(r.lap_ms) < u64::from(r.laps) * u64::from(rules.min_lap_ms) { return Err(format!("racer {i}: {} laps in {} ms is faster than the track allows", r.laps, r.lap_ms)); }
        }
        let mut places: Vec<u8> = self.racers.iter().map(|r| r.place).collect();
        places.sort_unstable();
        if places != (1..=RACERS as u8).collect::<Vec<_>>() { return Err("places are not 1..8".into()); }
        let order = self.order();
        if order.iter().enumerate().any(|(p, &i)| self.racers[i as usize].place as usize != p + 1) { return Err("standings differ from the deterministic order".into()); }
        let any = |s: u8| self.racers.iter().any(|r| r.state == s);
        let resolved = self.racers.iter().all(|r| matches!(r.state, FINISHED | DISCONNECTED | DNF));
        match self.status {
            race_status::SCHEDULED | race_status::RUNNING if any(FINISHED) || any(DNF) => return Err("finishers before FINISHING".into()),
            race_status::FINISHING if !any(FINISHED) => return Err("FINISHING without a finisher".into()),
            race_status::COMPLETED if !resolved => return Err("COMPLETED with racers unresolved".into()),
            _ => {}
        }
        if let Some(p) = prev {
            if self.seq <= p.seq { return Err("stale or duplicate progress sequence".into()); }
            if self.race_ms < p.race_ms { return Err("race time went backwards".into()); }
            let terminal = |s: u8| matches!(s, race_status::COMPLETED | race_status::INTERRUPTED);
            if terminal(p.status) && self.status != p.status { return Err("race status changed after it ended".into()); }
            if !terminal(self.status) && self.status < p.status { return Err("race status went backwards".into()); }
            let dt = u64::from(self.race_ms - p.race_ms);
            for (a, b) in p.racers.iter().zip(&self.racers) {
                if b.laps < a.laps { return Err(format!("racer {}: laps went backwards", a.index)); }
                if a.state == FINISHED && (b.state != FINISHED || b.finish_ms != a.finish_ms) { return Err(format!("racer {}: a finish cannot change", a.index)); }
                if a.state == DNF && b.state != DNF { return Err(format!("racer {}: DNF is final", a.index)); }
                // Progress rate: at most one lap per min_lap_ms (+ one lap of slack for interpolation).
                let max_gain = (dt * u64::from(Q)) / u64::from(rules.min_lap_ms.max(1)) + u64::from(Q);
                if u64::from(b.progress) > u64::from(a.progress) + max_gain { return Err(format!("racer {}: progress jumped faster than the track allows", a.index)); }
                if b.laps > a.laps && b.lap_ms <= a.lap_ms { return Err(format!("racer {}: new lap without a later lap time", a.index)); }
            }
        }
        Ok(())
    }
}

impl ProgressEvent {
    pub fn decode(b: &[u8]) -> Result<Self, String> {
        if b.len() != EVENT_LEN || b[0] != VERSION || b[6] != 0 || b[7] != 0 { return Err("malformed progress event".into()); }
        let e = Self { kind: b[1], index: b[2], laps: b[3], place: b[4], state: b[5], id: be32(&b[8..]), race_ms: be32(&b[12..]), seq: be32(&b[16..]) };
        if !(event_kind::LAP..=event_kind::STATUS).contains(&e.kind) || e.index as usize >= RACERS { return Err("progress event kind/racer".into()); }
        Ok(e)
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![VERSION, self.kind, self.index, self.laps, self.place, self.state, 0, 0];
        for v in [self.id, self.race_ms, self.seq] { b.extend_from_slice(&v.to_be_bytes()); }
        b
    }
}

/// Lap, finish and race-status transitions from `prev` to `next` (both
/// validated). Ids continue from `next_id`; the same pair always yields the
/// same events, so every peer's history is the host's.
pub fn derive_events(prev: Option<&Body>, next: &Body, next_id: &mut u32) -> Vec<ProgressEvent> {
    let mut out = Vec::new();
    let mut push = |kind, index, laps, place, state, race_ms| {
        *next_id += 1;
        out.push(ProgressEvent { kind, index, laps, place, state, id: *next_id, race_ms, seq: next.seq });
    };
    for (i, b) in next.racers.iter().enumerate() {
        let a = prev.map(|p| p.racers[i]).unwrap_or_default();
        // One event per lap, even if a lost snapshot hid an intermediate one.
        for lap in a.laps + 1..=b.laps {
            if lap == next.laps && b.state == racer_state::FINISHED { continue; }
            push(event_kind::LAP, b.index, lap, b.place, b.state, if lap == b.laps { b.lap_ms } else { next.race_ms });
        }
        if b.state == racer_state::FINISHED && a.state != racer_state::FINISHED { push(event_kind::FINISH, b.index, b.laps, b.place, b.state, b.finish_ms); }
    }
    if prev.map_or(true, |p| p.status != next.status) { push(event_kind::STATUS, 0, 0, 0, next.status, next.race_ms); }
    out
}

/// The race identity every progress object carries on the mesh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding { pub race: [u8; 16], pub epoch: u32, pub schedule: u64 }

impl Binding {
    pub fn new(race_hex: &str, epoch: u64, schedule_hex: &str) -> Result<Self, String> {
        let raw = hex_bytes(race_hex).filter(|r| r.len() == 16).ok_or("race id is not 32 hex digits")?;
        let schedule = u64::from_str_radix(schedule_hex, 16).map_err(|_| "schedule id is not hex")?;
        Ok(Self { race: raw.try_into().unwrap(), epoch: u32::try_from(epoch).map_err(|_| "epoch out of range")?, schedule })
    }
    pub fn wrap(&self, magic: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = magic.to_vec();
        b.extend_from_slice(&self.race); b.extend_from_slice(&self.epoch.to_be_bytes()); b.extend_from_slice(&self.schedule.to_be_bytes());
        b.extend_from_slice(body);
        b
    }
    /// The body, if `wire` is bound to exactly this race, epoch and schedule.
    pub fn unwrap<'a>(&self, magic: &[u8; 4], wire: &'a [u8]) -> Result<&'a [u8], Mismatch> {
        if wire.len() < BINDING_LEN || &wire[..4] != magic { return Err(Mismatch::Malformed); }
        if wire[4..20] != self.race { return Err(Mismatch::Race); }
        if be32(&wire[20..]) != self.epoch { return Err(Mismatch::Epoch); }
        if u64::from_be_bytes(wire[24..32].try_into().unwrap()) != self.schedule { return Err(Mismatch::Schedule); }
        Ok(&wire[BINDING_LEN..])
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch { Malformed, Race, Epoch, Schedule }

pub const SNAPSHOT_MAGIC: &[u8; 4] = b"TKPS";
pub const EVENT_MAGIC: &[u8; 4] = b"TKPE";

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 { return None; }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use racer_state::*;

    const RULES: Rules = Rules { laps: 3, humans: 2, min_lap_ms: 20_000 };

    /// A consistent snapshot: `prog[i]` laps×Q progress, finishers given as (index, ms).
    fn body(seq: u32, race_ms: u32, prog: [u32; 8], fin: &[(usize, u32)]) -> Body {
        let mut racers: Vec<Racer> = (0..8).map(|i| {
            let laps = (prog[i] / Q).saturating_sub(1).min(3) as u8;
            Racer { index: i as u8, kind: u8::from(i >= 2), laps, checkpoint: (((prog[i] % Q) * 8) / Q) as u8, place: 0, state: RACING, progress: prog[i],
                lap_ms: if laps > 0 { 25_000 * u32::from(laps) } else { 0 }, finish_ms: 0 }
        }).collect();
        for &(i, ms) in fin { let r = &mut racers[i]; r.state = FINISHED; r.laps = 3; r.finish_ms = ms; r.lap_ms = ms; r.progress = 4 * Q; r.checkpoint = 0; }
        let mut b = Body { status: if fin.is_empty() { race_status::RUNNING } else { race_status::FINISHING }, laps: 3, seq, race_ms, racers };
        for (p, i) in b.order().into_iter().enumerate() { b.racers[i as usize].place = p as u8 + 1; }
        b
    }
    const START: [u32; 8] = [62_000, 61_000, 60_000, 59_000, 58_000, 57_000, 56_000, 55_000];

    #[test]
    fn codec_round_trips_and_rejects_malformed() {
        let b = body(1, 1000, START, &[]);
        let bytes = b.encode();
        assert_eq!(bytes.len(), BODY_LEN);
        assert_eq!(Body::decode(&bytes).unwrap(), b);
        for n in [0, BODY_LEN - 1, BODY_LEN + 1] { assert!(Body::decode(&vec![1; n]).is_err()); }
        let mut bad = bytes.clone(); bad[0] = 2; assert!(Body::decode(&bad).is_err());
        let mut bad = bytes.clone(); bad[12 + 6] = 1; assert!(Body::decode(&bad).is_err());
        let e = ProgressEvent { kind: event_kind::LAP, index: 3, laps: 1, place: 2, state: 0, id: 9, race_ms: 30_000, seq: 4 };
        assert_eq!(ProgressEvent::decode(&e.encode()).unwrap(), e);
        let mut bad = e.encode(); bad[1] = 9; assert!(ProgressEvent::decode(&bad).is_err());
    }

    /// Same bytes as the game's `tests/progress.test.mjs` golden (src/progress.js codec).
    pub const GOLDEN: &str = "0101030800000007000061a800000000020000000000ea60000000000000000001000001030000000000e678000000000000000002010102010000000002e29000005fb40000000003010003040000000000dea8000000000000000004010004050000000000dac0000000000000000005010005060000000000d6d8000000000000000006010006070000000000d2f0000000000000000007010007080000000000cf080000000000000000";
    #[test]
    fn golden_bytes_match_the_game_codec() {
        let mut racers: Vec<Racer> = (0..8u8).map(|i| Racer { index: i, kind: u8::from(i >= 2), checkpoint: i % 8, progress: 60_000 - u32::from(i) * 1000, ..Default::default() }).collect();
        racers[2].laps = 1; racers[2].lap_ms = 24_500; racers[2].progress += 2 * Q;
        let mut b = Body { status: race_status::RUNNING, laps: 3, seq: 7, race_ms: 25_000, racers };
        for (p, i) in b.order().into_iter().enumerate() { b.racers[i as usize].place = p as u8 + 1; }
        let hex: String = b.encode().iter().map(|x| format!("{x:02x}")).collect();
        assert_eq!(hex, GOLDEN);
        b.validate(&RULES, None).unwrap();
    }

    #[test]
    fn standings_are_deterministic_and_verified() {
        // Equal progress: grid index breaks the tie.
        let mut prog = START; prog[5] = prog[2];
        let b = body(1, 1000, prog, &[]);
        assert_eq!(b.order()[..4], [0, 1, 2, 5]);
        b.validate(&RULES, None).unwrap();
        // A renderer (or forger) claiming a different first place is refused.
        let mut lie = b.clone(); lie.racers[7].place = 1; lie.racers[0].place = 8;
        assert!(lie.validate(&RULES, None).unwrap_err().contains("deterministic order"));
        // Two finishing in the same update: earlier time first; same time → lower index.
        let f = body(9, 100_000, START, &[(4, 90_000), (1, 90_000), (6, 89_000)]);
        assert_eq!(f.order()[..3], [6, 1, 4]);
        f.validate(&RULES, None).unwrap();
    }

    #[test]
    fn identity_kind_follows_the_grid() {
        let mut b = body(1, 1000, START, &[]);
        b.racers[1].kind = 1; // a human claimed as AI
        assert!(b.validate(&RULES, None).unwrap_err().contains("human/AI"));
        let mut b = body(1, 1000, START, &[]);
        b.racers[2].kind = 0; // an AI slot claimed as a human
        assert!(b.validate(&RULES, None).is_err());
        let mut b = body(1, 1000, START, &[]); b.racers.swap(0, 1);
        assert!(b.validate(&RULES, None).is_err());
    }

    #[test]
    fn laps_and_finishes_follow_the_admission_and_the_clock() {
        // Configured lap count only.
        let mut b = body(1, 1000, START, &[]); b.laps = 5;
        assert!(b.validate(&RULES, None).is_err());
        // Early finish: fewer laps than configured, or faster than the track allows.
        let mut early = body(2, 50_000, START, &[(0, 50_000)]);
        assert!(early.validate(&RULES, None).unwrap_err().contains("faster than the track"));
        early.racers[0].laps = 2; assert!(early.validate(&RULES, None).is_err());
        // A legitimate finish time is accepted and must equal the final lap's completion.
        let ok = body(2, 80_000, START, &[(0, 75_000)]);
        ok.validate(&RULES, None).unwrap();
        let mut wrong = ok.clone(); wrong.racers[0].lap_ms = 74_000;
        assert!(wrong.validate(&RULES, None).is_err());
        // Past the final line but not finished, or finished in RUNNING.
        let mut past = body(1, 80_000, START, &[]); past.racers[3].progress = 4 * Q;
        assert!(past.validate(&RULES, None).is_err());
        let mut st = ok.clone(); st.status = race_status::RUNNING;
        assert!(st.validate(&RULES, None).is_err());
        let mut done = ok.clone(); done.status = race_status::COMPLETED;
        assert!(done.validate(&RULES, None).unwrap_err().contains("unresolved"));
    }

    #[test]
    fn only_forward_given_the_previous_snapshot() {
        let a = body(5, 30_000, START, &[]);
        // Stale and duplicate sequences.
        assert!(body(5, 30_100, START, &[]).validate(&RULES, Some(&a)).is_err());
        assert!(body(4, 30_100, START, &[]).validate(&RULES, Some(&a)).is_err());
        // Laps cannot go backwards; progress cannot jump more than the track allows.
        let mut prog = START; prog[0] = 2 * Q + 100;
        let b = body(6, 60_000, prog, &[]);
        b.validate(&RULES, Some(&a)).unwrap();
        assert!(body(7, 60_100, START, &[]).validate(&RULES, Some(&b)).unwrap_err().contains("backwards"));
        let mut jump = START; jump[0] = START[0] + 3 * Q / 2;
        assert!(body(7, 30_500, jump, &[]).validate(&RULES, Some(&a)).unwrap_err().contains("jumped"));
        // A finish is immutable.
        let f1 = body(8, 80_000, START, &[(0, 75_000)]);
        let f2 = body(9, 80_100, START, &[(0, 75_050)]);
        assert!(f2.validate(&RULES, Some(&f1)).unwrap_err().contains("finish cannot change"));
    }

    #[test]
    fn events_are_derived_once_per_transition() {
        let a = body(1, 20_000, START, &[]);
        let mut prog = START; prog[2] = 2 * Q + 10; prog[5] = 3 * Q + 10;
        let mut b = body(2, 60_000, prog, &[]);
        b.racers[5].lap_ms = 50_000; b.racers[2].lap_ms = 30_000;
        let mut id = 0;
        let ev = derive_events(Some(&a), &b, &mut id);
        // racer 2: lap 1; racer 5: laps 1 and 2 (a missed snapshot still yields both); no status change.
        let laps: Vec<_> = ev.iter().map(|e| (e.kind, e.index, e.laps)).collect();
        assert_eq!(laps, vec![(event_kind::LAP, 2, 1), (event_kind::LAP, 5, 1), (event_kind::LAP, 5, 2)]);
        assert_eq!(ev.iter().map(|e| e.id).collect::<Vec<_>>(), vec![1, 2, 3]);
        // The same pair again is no new transition once applied.
        assert!(derive_events(Some(&b), &b, &mut id).is_empty());
        let f = body(3, 80_000, START, &[(0, 75_000)]);
        let ev = derive_events(Some(&a), &f, &mut id);
        assert!(ev.iter().any(|e| e.kind == event_kind::FINISH && e.index == 0 && e.race_ms == 75_000));
        assert!(ev.iter().any(|e| e.kind == event_kind::STATUS && e.state == race_status::FINISHING));
        assert!(!ev.iter().any(|e| e.kind == event_kind::LAP && e.index == 0 && e.laps == 3), "the final lap is the finish, not a lap event");
    }

    #[test]
    fn binding_refuses_other_race_epoch_and_schedule() {
        let bind = Binding::new(&"ab".repeat(16), 1, "00000000000000ff").unwrap();
        let wire = bind.wrap(SNAPSHOT_MAGIC, &[1, 2, 3]);
        assert_eq!(bind.unwrap(SNAPSHOT_MAGIC, &wire).unwrap(), &[1, 2, 3]);
        assert_eq!(bind.unwrap(EVENT_MAGIC, &wire), Err(Mismatch::Malformed));
        let other = Binding::new(&"cd".repeat(16), 1, "ff").unwrap();
        assert_eq!(other.unwrap(SNAPSHOT_MAGIC, &wire), Err(Mismatch::Race));
        assert_eq!(Binding { epoch: 2, ..bind }.unwrap(SNAPSHOT_MAGIC, &wire), Err(Mismatch::Epoch));
        assert_eq!(Binding { schedule: 7, ..bind }.unwrap(SNAPSHOT_MAGIC, &wire), Err(Mismatch::Schedule));
        assert_eq!(bind.unwrap(SNAPSHOT_MAGIC, &wire[..10]), Err(Mismatch::Malformed));
        assert!(Binding::new("xyz", 1, "1").is_err());
    }
}
