//! Clock message validation: hand-written golden bytes, every rejection path,
//! and a deterministic garbage sweep (no panic, accepted inputs are canonical).

use holochain_moq_clock::*;

fn unhex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

const PING_HEX: &str = "01 01 0102030405060708 0000002a 000000003b9aca00";
const PONG_HEX: &str =
    "01 02 0102030405060708 0000002a 000000003b9aca00 000000003b9b0000 000000003b9b0400";

fn ping() -> ClockPing {
    ClockPing {
        nonce: 0x0102_0304_0506_0708,
        seq: 42,
        t0: 1_000_000_000,
    }
}
fn pong() -> ClockPong {
    ClockPong {
        nonce: 0x0102_0304_0506_0708,
        seq: 42,
        t0: 1_000_000_000,
        t1: 1_000_013_824,
        t2: 1_000_014_848,
    }
}

#[test]
fn golden_bytes_and_round_trip() {
    assert_eq!(ping().encode().to_vec(), unhex(PING_HEX));
    assert_eq!(pong().encode().to_vec(), unhex(PONG_HEX));
    assert_eq!(
        ClockMessage::decode(&unhex(PING_HEX)),
        Ok(ClockMessage::Ping(ping()))
    );
    assert_eq!(
        ClockMessage::decode(&unhex(PONG_HEX)),
        Ok(ClockMessage::Pong(pong()))
    );
    assert_eq!(wire_sizes(), (22, 38));
}

fn wire_sizes() -> (usize, usize) {
    (ping().encode().len(), pong().encode().len())
}

#[test]
fn rejections() {
    let p = unhex(PONG_HEX);
    assert_eq!(ClockMessage::decode(&[]), Err(WireError::Empty));
    assert!(
        matches!(
            ClockMessage::decode(&[1]),
            Err(WireError::WrongLength { .. })
        ),
        "version only"
    );
    assert!(matches!(
        ClockMessage::decode(&p[..37]),
        Err(WireError::WrongLength {
            msg_type: 2,
            expected: 38,
            got: 37
        })
    ));
    let mut long = p.clone();
    long.push(0);
    assert!(
        matches!(
            ClockMessage::decode(&long),
            Err(WireError::WrongLength { .. })
        ),
        "trailing byte"
    );
    // A ping-length body claiming to be a pong (impossible length for its type).
    let mut short_pong = unhex(PING_HEX);
    short_pong[1] = 2;
    assert!(matches!(
        ClockMessage::decode(&short_pong),
        Err(WireError::WrongLength { msg_type: 2, .. })
    ));
    let mut v = p.clone();
    v[0] = 2;
    assert_eq!(ClockMessage::decode(&v), Err(WireError::UnknownVersion(2)));
    let mut t = p.clone();
    t[1] = 9;
    assert_eq!(ClockMessage::decode(&t), Err(WireError::UnknownType(9)));
    let mut t0 = p.clone();
    t0[1] = 0;
    assert_eq!(ClockMessage::decode(&t0), Err(WireError::UnknownType(0)));
    // Timestamps above 2^62 (overflow guard).
    let mut big = p.clone();
    big[22..30].copy_from_slice(&u64::MAX.to_be_bytes());
    assert_eq!(
        ClockMessage::decode(&big),
        Err(WireError::TimestampOutOfRange)
    );
    // Responder "sent before it received".
    let backwards = ClockPong {
        t1: 10,
        t2: 9,
        ..pong()
    }
    .encode();
    assert_eq!(
        ClockMessage::decode(&backwards),
        Err(WireError::TimestampsOutOfOrder)
    );
    // A pong that makes RTT negative is rejected when turned into a sample.
    assert!(ClockSample::from_timestamps(100, 0, 500, 200).is_err());
}

#[test]
fn garbage_never_panics_and_accepted_inputs_are_canonical() {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let seeds = [unhex(PING_HEX), unhex(PONG_HEX)];
    let mut accepted = 0;
    for i in 0..200_000 {
        let mut b = if i % 2 == 0 {
            (0..(next() % 48) as usize)
                .map(|_| next() as u8)
                .collect::<Vec<u8>>()
        } else {
            let mut b = seeds[(i / 2) % 2].clone();
            for _ in 0..(1 + next() % 3) {
                let at = (next() as usize) % b.len();
                b[at] = next() as u8;
            }
            b
        };
        if i % 3 == 0 && !b.is_empty() {
            b[0] = 1;
        }
        if let Ok(m) = ClockMessage::decode(&b) {
            accepted += 1;
            let again = match m {
                ClockMessage::Ping(p) => p.encode().to_vec(),
                ClockMessage::Pong(p) => p.encode().to_vec(),
            };
            assert_eq!(again, b);
        }
    }
    println!("clock sweep: 200000 inputs, {accepted} accepted, 0 panics");
    assert!(accepted > 1000, "the sweep exercised the accept path");
}
