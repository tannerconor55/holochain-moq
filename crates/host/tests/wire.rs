//! Host-authority wire format: hand-written golden bytes, rejections, and a
//! deterministic garbage sweep (no panic; accepted inputs are canonical).

use bytes::Bytes;
use holochain_moq_host::*;

fn unhex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn golden_bytes() {
    let hb = HostMessage::Heartbeat(Heartbeat {
        epoch: 7,
        seq: 3,
        host_time_us: 1_000_000_000,
        host: "uhCAkA".into(),
    });
    assert_eq!(
        hb.encode().unwrap(),
        unhex(&format!(
            "01 01 00000007 00000003 000000003b9aca00 06 {}",
            hex("uhCAkA")
        ))
    );
    let hc = HostMessage::HostChanged(HostChanged {
        epoch: 8,
        new_host: "uhCAkB".into(),
        prev_epoch: 7,
        prev_host: "uhCAkA".into(),
    });
    assert_eq!(
        hc.encode().unwrap(),
        unhex(&format!(
            "01 02 00000008 06 {} 00000007 06 {}",
            hex("uhCAkB"),
            hex("uhCAkA")
        ))
    );
    let ai = HostMessage::AiFrame(AiFrame {
        epoch: 8,
        slot: 3,
        payload: Bytes::from_static(&[0xaa, 0xbb]),
    });
    assert_eq!(ai.encode().unwrap(), unhex("01 03 00000008 03 0002 aabb"));
    for m in [hb, hc, ai] {
        assert_eq!(HostMessage::decode(&m.encode().unwrap()).unwrap(), m);
    }
}

#[test]
fn rejections() {
    let good = HostMessage::HostChanged(HostChanged {
        epoch: 8,
        new_host: "uhCAkB".into(),
        prev_epoch: 7,
        prev_host: "uhCAkA".into(),
    })
    .encode()
    .unwrap();
    assert_eq!(HostMessage::decode(&[]), Err(WireError::Truncated));
    assert_eq!(
        HostMessage::decode(&good[..good.len() - 1]),
        Err(WireError::Truncated)
    );
    let mut long = good.clone();
    long.push(0);
    assert_eq!(HostMessage::decode(&long), Err(WireError::TrailingBytes));
    let mut v = good.clone();
    v[0] = 2;
    assert_eq!(HostMessage::decode(&v), Err(WireError::UnknownVersion(2)));
    let mut t = good.clone();
    t[1] = 9;
    assert_eq!(HostMessage::decode(&t), Err(WireError::UnknownType(9)));
    let mut z = good.clone();
    z[2..6].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(
        HostMessage::decode(&z),
        Err(WireError::ZeroEpoch),
        "epoch 0 is never valid"
    );
    let mut bad_agent = good.clone();
    bad_agent[7] = b'/';
    assert_eq!(HostMessage::decode(&bad_agent), Err(WireError::BadAgent));
    let mut zero_len = good.clone();
    zero_len[6] = 0;
    assert_eq!(HostMessage::decode(&zero_len), Err(WireError::BadAgent));
    let big = unhex("01 03 00000008 03 03f8"); // len 1016 > 1015, checked before reading
    assert_eq!(HostMessage::decode(&big), Err(WireError::PayloadTooLarge));
    let too_big = HostMessage::AiFrame(AiFrame {
        epoch: 1,
        slot: 0,
        payload: Bytes::from(vec![0; MAX_AI_PAYLOAD + 1]),
    });
    assert_eq!(too_big.encode(), Err(WireError::PayloadTooLarge));
    let zero = HostMessage::Heartbeat(Heartbeat {
        epoch: 0,
        seq: 0,
        host_time_us: 0,
        host: "a".into(),
    });
    assert_eq!(zero.encode(), Err(WireError::ZeroEpoch));
    let slash = HostMessage::Heartbeat(Heartbeat {
        epoch: 1,
        seq: 0,
        host_time_us: 0,
        host: "a/b".into(),
    });
    assert_eq!(slash.encode(), Err(WireError::BadAgent));
}

#[test]
fn garbage_never_panics_and_accepted_inputs_are_canonical() {
    let mut x: u64 = 0x1357_9bdf_2468_ace1;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let seeds: Vec<Vec<u8>> = vec![
        HostMessage::Heartbeat(Heartbeat {
            epoch: 7,
            seq: 3,
            host_time_us: 9,
            host: "uhCAkA".into(),
        })
        .encode()
        .unwrap(),
        HostMessage::HostChanged(HostChanged {
            epoch: 8,
            new_host: "uhCAkB".into(),
            prev_epoch: 7,
            prev_host: "uhCAkA".into(),
        })
        .encode()
        .unwrap(),
        HostMessage::AiFrame(AiFrame {
            epoch: 8,
            slot: 1,
            payload: Bytes::from_static(b"state"),
        })
        .encode()
        .unwrap(),
    ];
    let mut accepted = 0;
    for i in 0..200_000usize {
        let mut b = if i % 2 == 0 {
            (0..(next() % 80) as usize)
                .map(|_| next() as u8)
                .collect::<Vec<u8>>()
        } else {
            let mut b = seeds[(i / 2) % seeds.len()].clone();
            for _ in 0..(1 + next() % 3) {
                let at = (next() as usize) % b.len();
                b[at] = next() as u8;
            }
            b
        };
        if i % 3 == 0 && !b.is_empty() {
            b[0] = 1;
        }
        if let Ok(m) = HostMessage::decode(&b) {
            accepted += 1;
            assert_eq!(m.encode().unwrap(), b);
        }
    }
    println!("host wire sweep: 200000 inputs, {accepted} accepted, 0 panics");
    assert!(accepted > 1000);
}
