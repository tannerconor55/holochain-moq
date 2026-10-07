//! Wire-format tests: hand-computed golden bytes (not produced by the
//! encoder), round trips, every rejection path, and a deterministic
//! garbage sweep. `fixtures/golden.json` holds the golden vectors in a form a
//! JavaScript decoder can consume later; regenerate it with
//! `KART_WIRE_BLESS=1 cargo test -p holochain-moq-kart-wire`.

use holochain_moq_kart_wire::*;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn golden_state() -> KartState {
    KartState {
        flags: flags::AIRBORNE | flags::DRIFTING,
        tick: 0x0102_0304,
        pos: [1.0, -2.0, 0.5],
        vel: [0.0, 0.25, -0.5],
        heading: 3.0,
        drift: 2,
        item: item::GREEN_SHELL,
        item_count: 1,
        lap: 2,
        track_t: 0.75,
    }
}

/// Written out by hand from the layout in PROTOCOL.md.
const GOLDEN_STATE_HEX: &str = "01 03 01020304 3f800000 c0000000 3f000000 00000000 3e800000 bf000000 40400000 02 04 01 02 3f400000";

fn golden_lap() -> Event {
    Event {
        seq: 7,
        tick: 3600,
        body: EventBody::Lap {
            lap: 2,
            lap_ms: 50_000,
        },
    }
}
const GOLDEN_LAP_HEX: &str = "01 07 00000007 00000e10 0005 02 0000c350";

fn golden_race_start() -> Event {
    Event {
        seq: 0,
        tick: 0,
        body: EventBody::RaceStart {
            start_at_ms: 1_700_000_000_000,
            seed: 0x0102_0304_0506_0708,
            laps: 3,
            class: 2,
            track_id: "palm-cove".into(),
            grid: vec![
                GridSlot::Ai(1),
                GridSlot::Player(AgentKey([0xaa; AGENT_KEY_LEN])),
            ],
        },
    }
}
fn golden_race_start_hex() -> String {
    format!(
        "01 01 00000000 00000000 0047 0000018bcfe56800 0102030405060708 03 02 09 70616c6d2d636f7665 02 0101 00{}",
        "aa".repeat(AGENT_KEY_LEN)
    )
}

fn golden_hit() -> Event {
    Event {
        seq: 0xfffffffe,
        tick: 0xffffffff,
        body: EventBody::Hit {
            kind: 1,
            by_slot: 5,
            projectile_id: 0xdeadbeef,
        },
    }
}
const GOLDEN_HIT_HEX: &str = "01 06 fffffffe ffffffff 0006 01 05 deadbeef";

#[test]
fn kart_state_golden_bytes() {
    let b = golden_state().encode().unwrap();
    assert_eq!(b.len(), KART_STATE_LEN);
    assert_eq!(hex(&b), hex(&unhex(GOLDEN_STATE_HEX)));
    assert_eq!(KartState::decode(&b).unwrap(), golden_state());
}

#[test]
fn event_golden_bytes() {
    for (ev, h) in [
        (golden_lap(), GOLDEN_LAP_HEX.to_string()),
        (golden_race_start(), golden_race_start_hex()),
        (golden_hit(), GOLDEN_HIT_HEX.to_string()),
    ] {
        let b = ev.encode().unwrap();
        assert_eq!(hex(&b), hex(&unhex(&h)), "{ev:?}");
        assert_eq!(Event::decode(&b).unwrap(), ev);
    }
}

#[test]
fn every_event_type_round_trips_with_seq_and_tick() {
    let key = AgentKey([7; AGENT_KEY_LEN]);
    let bodies = vec![
        golden_race_start().body,
        EventBody::RaceStart {
            start_at_ms: 0,
            seed: 0,
            laps: 1,
            class: 0,
            track_id: String::new(),
            grid: vec![],
        },
        EventBody::HostChanged { new_host: key },
        EventBody::ItemPickup {
            box_index: 29,
            pickup_index: 3,
            item: item::NONE,
        },
        EventBody::ItemUse {
            item: item::BLUE_SHELL,
        },
        EventBody::ProjectileSpawn {
            projectile_id: 9,
            kind: 2,
            pos: [1.5, 0.0, -3.25],
            vel: [-0.0, 1e-3, 123.5],
            target_slot: NO_TARGET,
        },
        EventBody::Hit {
            kind: 0,
            by_slot: 7,
            projectile_id: 0,
        },
        EventBody::Lap {
            lap: 255,
            lap_ms: u32::MAX,
        },
        EventBody::Finish {
            finish_ms: 180_000,
            best_lap_ms: NO_TIME,
        },
        EventBody::Leave { reason: 3 },
    ];
    for (i, body) in bodies.into_iter().enumerate() {
        let ev = Event {
            seq: i as u32 * 1000 + 1,
            tick: u32::MAX - i as u32,
            body,
        };
        let back = Event::decode(&ev.encode().unwrap()).unwrap();
        assert_eq!(back, ev);
        assert_eq!((back.seq, back.tick), (ev.seq, ev.tick));
    }
}

#[test]
fn floats_survive_bit_exactly() {
    for v in [
        0.0f32,
        -0.0,
        f32::MIN_POSITIVE,
        1e-38,
        3.4e38,
        -123.456,
        0.999_999_94,
    ] {
        let mut s = golden_state();
        s.pos = [v, v, v];
        s.heading = v;
        let back = KartState::decode(&s.encode().unwrap()).unwrap();
        assert_eq!(back.pos[0].to_bits(), v.to_bits());
        assert_eq!(back.heading.to_bits(), v.to_bits());
    }
}

#[test]
fn kart_state_rejections() {
    let good = golden_state().encode().unwrap();
    assert!(matches!(
        KartState::decode(&good[..41]),
        Err(DecodeError::Truncated { .. })
    ));
    assert!(matches!(
        KartState::decode(&[]),
        Err(DecodeError::Truncated { .. })
    ));
    let mut long = good.to_vec();
    long.push(0);
    assert!(matches!(
        KartState::decode(&long),
        Err(DecodeError::TrailingBytes { .. })
    ));
    let mut v2 = good;
    v2[0] = 2;
    assert_eq!(KartState::decode(&v2), Err(DecodeError::UnknownVersion(2)));
    let mut nan = good;
    nan[6..10].copy_from_slice(&f32::NAN.to_bits().to_be_bytes());
    assert_eq!(KartState::decode(&nan), Err(DecodeError::NonFinite("pos")));
    let mut inf = good;
    inf[38..42].copy_from_slice(&f32::INFINITY.to_bits().to_be_bytes());
    assert_eq!(
        KartState::decode(&inf),
        Err(DecodeError::NonFinite("trackT"))
    );
    for (off, val, field) in [(34, 4, "drift"), (35, 9, "item"), (36, 4, "itemCount")] {
        let mut b = good;
        b[off] = val;
        assert_eq!(KartState::decode(&b), Err(DecodeError::OutOfRange(field)));
    }
    let mut t1 = good;
    t1[38..42].copy_from_slice(&1.0f32.to_bits().to_be_bytes());
    assert_eq!(
        KartState::decode(&t1),
        Err(DecodeError::OutOfRange("trackT"))
    );
    let mut s = golden_state();
    s.vel[1] = f32::NAN;
    assert_eq!(s.encode(), Err(EncodeError::NonFinite("vel")));
}

#[test]
fn event_rejections() {
    let good = golden_lap().encode().unwrap();
    assert!(
        matches!(
            Event::decode(&good[..11]),
            Err(DecodeError::Truncated { .. })
        ),
        "short header"
    );
    assert!(
        matches!(
            Event::decode(&good[..good.len() - 1]),
            Err(DecodeError::Truncated { .. })
        ),
        "short payload"
    );
    let mut long = good.clone();
    long.push(0);
    assert!(matches!(
        Event::decode(&long),
        Err(DecodeError::TrailingBytes { .. })
    ));
    let mut v0 = good.clone();
    v0[0] = 0;
    assert_eq!(Event::decode(&v0), Err(DecodeError::UnknownVersion(0)));

    // len over the limit is refused from the header alone, before any payload.
    let mut huge = good[..10].to_vec();
    huge.extend_from_slice(&((MAX_EVENT_PAYLOAD + 1) as u16).to_be_bytes());
    assert_eq!(
        Event::decode(&huge),
        Err(DecodeError::PayloadTooLarge(MAX_EVENT_PAYLOAD + 1))
    );
    let big = Event {
        seq: 0,
        tick: 0,
        body: EventBody::Unknown {
            event_type: 200,
            payload: vec![0; MAX_EVENT_PAYLOAD + 1],
        },
    };
    assert_eq!(
        big.encode(),
        Err(EncodeError::PayloadTooLarge(MAX_EVENT_PAYLOAD + 1))
    );

    // A known fixed-size type with the wrong len.
    let mut wrong = good[..10].to_vec();
    wrong.extend_from_slice(&6u16.to_be_bytes());
    wrong.extend_from_slice(&[2, 0, 0, 0xc3, 0x50, 0]);
    assert_eq!(
        Event::decode(&wrong),
        Err(DecodeError::WrongPayloadLength {
            event_type: event_type::LAP,
            expected: 5,
            got: 6
        })
    );
    // Type 0 is reserved.
    let mut t0 = good.clone();
    t0[1] = 0;
    assert_eq!(Event::decode(&t0), Err(DecodeError::OutOfRange("type")));
    // RaceStart internals: bad grid kind, too many slots, track id overrun.
    let rs = golden_race_start().encode().unwrap();
    let mut bad_kind = rs.clone();
    let grid_kind_at = EVENT_HEADER_LEN + 8 + 8 + 3 + 9 + 1;
    bad_kind[grid_kind_at] = 9;
    assert_eq!(
        Event::decode(&bad_kind),
        Err(DecodeError::OutOfRange("grid slot kind"))
    );
    let mut bad_id = rs.clone();
    bad_id[EVENT_HEADER_LEN + 18] = 200;
    assert_eq!(
        Event::decode(&bad_id),
        Err(DecodeError::OutOfRange("track_id"))
    );
    let too_many = EventBody::RaceStart {
        start_at_ms: 0,
        seed: 0,
        laps: 1,
        class: 0,
        track_id: "x".into(),
        grid: vec![GridSlot::Ai(0); MAX_GRID + 1],
    };
    assert_eq!(
        Event {
            seq: 0,
            tick: 0,
            body: too_many
        }
        .encode(),
        Err(EncodeError::OutOfRange("grid"))
    );
    // ItemUse of "nothing" is meaningless.
    let use0 = unhex("01 04 00000000 00000000 0001 00");
    assert_eq!(Event::decode(&use0), Err(DecodeError::OutOfRange("item")));
}

/// Defined behaviour for a type this version does not know: decoded as
/// `Unknown` (bounded by `len`), so the receiver can skip and count it.
#[test]
fn unknown_event_types_are_kept_verbatim_not_rejected() {
    let b = unhex("01 c8 00000005 00000009 0003 aabbcc");
    let ev = Event::decode(&b).unwrap();
    assert_eq!(
        ev,
        Event {
            seq: 5,
            tick: 9,
            body: EventBody::Unknown {
                event_type: 200,
                payload: vec![0xaa, 0xbb, 0xcc]
            }
        }
    );
    assert_eq!(ev.encode().unwrap(), b);
    // But a known type cannot be smuggled through `Unknown`.
    let fake = Event {
        seq: 0,
        tick: 0,
        body: EventBody::Unknown {
            event_type: event_type::LAP,
            payload: vec![],
        },
    };
    assert!(fake.encode().is_err());
}

#[test]
fn sequence_check_dedupes_and_reports_gaps() {
    let mut s = EventSequence::default();
    assert_eq!(s.check(0), SeqCheck::Next);
    assert_eq!(s.check(1), SeqCheck::Next);
    assert_eq!(s.check(1), SeqCheck::Duplicate);
    assert_eq!(s.check(0), SeqCheck::Duplicate);
    assert_eq!(s.check(5), SeqCheck::Gap { expected: 2 });
    assert_eq!(s.check(2), SeqCheck::Next);
}

/// Deterministic garbage and mutation sweep: nothing panics, and whatever
/// decodes re-encodes to the same bytes.
#[test]
fn garbage_never_panics_and_valid_decodes_are_canonical() {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let seeds = [
        golden_state().encode().unwrap().to_vec(),
        golden_lap().encode().unwrap(),
        golden_race_start().encode().unwrap(),
        golden_hit().encode().unwrap(),
    ];
    let (mut ok_state, mut ok_event) = (0, 0);
    for i in 0..200_000 {
        let mut b = if i % 2 == 0 {
            let len = (next() % 80) as usize;
            (0..len).map(|_| next() as u8).collect::<Vec<u8>>()
        } else {
            let mut b = seeds[(i / 2) % seeds.len()].clone();
            for _ in 0..(1 + next() % 3) {
                let at = (next() as usize) % b.len();
                b[at] = next() as u8;
            }
            if next() % 4 == 0 {
                b.truncate((next() as usize) % (b.len() + 1));
            }
            b
        };
        if i % 3 == 0 && !b.is_empty() {
            b[0] = VERSION;
        }
        if let Ok(s) = KartState::decode(&b) {
            ok_state += 1;
            assert_eq!(s.encode().unwrap().to_vec(), b);
        }
        if let Ok(e) = Event::decode(&b) {
            ok_event += 1;
            assert_eq!(e.encode().unwrap(), b);
        }
    }
    println!(
        "sweep: 200000 inputs, {ok_state} valid kart states, {ok_event} valid events, 0 panics"
    );
}

#[test]
fn golden_fixture_file_matches() {
    let s = golden_state();
    let fixture = serde_json::json!({
        "format": "turbo-kart-rally wire v1",
        "byte_order": "big-endian",
        "kart_state": [{
            "name": "airborne-drifting",
            "hex": hex(&s.encode().unwrap()),
            "fields": {
                "version": 1, "flags": s.flags, "tick": s.tick, "pos": s.pos, "vel": s.vel,
                "heading": s.heading, "drift": s.drift, "item": s.item, "itemCount": s.item_count,
                "lap": s.lap, "trackT": s.track_t
            }
        }],
        "events": [
            { "name": "lap", "hex": hex(&golden_lap().encode().unwrap()),
              "fields": { "version": 1, "type": 7, "seq": 7, "tick": 3600, "len": 5, "lap": 2, "lap_ms": 50000 } },
            { "name": "race-start", "hex": hex(&golden_race_start().encode().unwrap()),
              "fields": { "version": 1, "type": 1, "seq": 0, "tick": 0, "len": 71, "start_at_ms": 1_700_000_000_000u64,
                          "seed": "0x0102030405060708", "laps": 3, "class": 2, "track_id": "palm-cove",
                          "grid": [ {"ai": 1}, {"player": "aa".repeat(AGENT_KEY_LEN)} ] } },
            { "name": "hit-max-seq", "hex": hex(&golden_hit().encode().unwrap()),
              "fields": { "version": 1, "type": 6, "seq": 4294967294u32, "tick": 4294967295u32, "len": 6,
                          "kind": 1, "by_slot": 5, "projectile_id": 3735928559u32 } },
            { "name": "unknown-type-200", "hex": "01c80000000500000009" .to_string() + "0003aabbcc",
              "fields": { "version": 1, "type": 200, "seq": 5, "tick": 9, "len": 3, "decoder": "keep as Unknown, skip" } }
        ],
        "rejects": [
            { "name": "kart-state-41-bytes", "hex": hex(&s.encode().unwrap()[..41]), "error": "Truncated" },
            { "name": "kart-state-version-2", "hex": "02".to_string() + &hex(&s.encode().unwrap()[1..]), "error": "UnknownVersion" },
            { "name": "event-len-over-1024", "hex": "0107000000000000000004010000", "error": "PayloadTooLarge" }
        ]
    });
    let text = serde_json::to_string_pretty(&fixture).unwrap() + "\n";
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/golden.json");
    if std::env::var("KART_WIRE_BLESS").is_ok() {
        std::fs::write(path, &text).unwrap();
    }
    let on_disk = std::fs::read_to_string(path)
        .expect("fixtures/golden.json (run once with KART_WIRE_BLESS=1)");
    assert_eq!(on_disk, text, "golden fixture out of date");
    // And the reject vectors really are rejected.
    for r in fixture["rejects"].as_array().unwrap() {
        let b = unhex(r["hex"].as_str().unwrap());
        assert!(
            KartState::decode(&b).is_err() && Event::decode(&b).is_err(),
            "{}",
            r["name"]
        );
    }
}
