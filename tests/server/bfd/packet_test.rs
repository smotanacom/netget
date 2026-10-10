//! The BFD codec and session state machine on their own: every authentication type signs and
//! verifies, a wrong key or a replayed sequence number is refused, and every §6.8.6 check the
//! decoder makes refuses what it should.
use netget::server::bfd::packet::{self, AuthConfig, AuthType, ControlPacket, State};
use netget::server::bfd::session::{Session, Timers};

fn control(state: State, yours: u32) -> ControlPacket {
    ControlPacket {
        diag: 0,
        state,
        poll: false,
        final_: false,
        control_plane_independent: false,
        demand: false,
        detect_mult: 3,
        my_discriminator: 0x0101_0101,
        your_discriminator: yours,
        desired_min_tx_us: 1_000_000,
        required_min_rx_us: 300_000,
        required_min_echo_rx_us: 0,
        auth: None,
    }
}

#[test]
fn every_auth_type_signs_and_verifies() {
    for kind in [
        AuthType::SimplePassword,
        AuthType::KeyedMd5,
        AuthType::MeticulousKeyedMd5,
        AuthType::KeyedSha1,
        AuthType::MeticulousKeyedSha1,
    ] {
        let right = AuthConfig::new(kind, 5, "secret").unwrap();
        let wrong = AuthConfig::new(kind, 5, "secreT").unwrap();
        let bytes = control(State::Down, 0).encode(Some((&right, 42)));
        let p = packet::decode(&bytes).unwrap();
        let section = p.auth.clone().unwrap();
        assert_eq!((section.kind, section.key_id), (kind, 5));
        assert_eq!(
            section.sequence,
            (kind != AuthType::SimplePassword).then_some(42)
        );
        packet::verify(&bytes, &p, Some(&right)).unwrap();
        assert!(
            packet::verify(&bytes, &p, Some(&wrong)).is_err(),
            "{kind:?} accepted a wrong key"
        );
        assert!(
            packet::verify(&bytes, &p, None).is_err(),
            "{kind:?} accepted by a session without auth"
        );
        // A flipped bit in the mandatory section breaks a digest.
        if kind != AuthType::SimplePassword {
            let mut tampered = bytes.clone();
            tampered[16] ^= 1;
            let tp = packet::decode(&tampered).unwrap();
            assert!(packet::verify(&tampered, &tp, Some(&right)).is_err());
        }
    }
    let plain = control(State::Down, 0).encode(None);
    let p = packet::decode(&plain).unwrap();
    let key = AuthConfig::new(AuthType::KeyedSha1, 1, "k").unwrap();
    assert!(
        packet::verify(&plain, &p, Some(&key)).is_err(),
        "an unauthenticated packet passed"
    );
    assert!(AuthConfig::new(AuthType::KeyedMd5, 1, &"x".repeat(17)).is_err());
}

#[test]
fn decoder_refusals() {
    let good = control(State::Down, 0).encode(None);
    packet::decode(&good).unwrap();
    let mutate = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = good.clone();
        f(&mut b);
        packet::decode(&b)
    };
    assert!(
        mutate(&|b| b[0] = (2 << 5) | (b[0] & 0x1f)).is_err(),
        "version 2"
    );
    assert!(mutate(&|b| b[2] = 0).is_err(), "Detect Mult 0");
    assert!(mutate(&|b| b[1] |= 1).is_err(), "Multipoint");
    assert!(
        mutate(&|b| b[4..8].copy_from_slice(&[0; 4])).is_err(),
        "My Discriminator 0"
    );
    assert!(mutate(&|b| b[3] = 30).is_err(), "Length past the datagram");
    assert!(mutate(&|b| b[3] = 20).is_err(), "Length below 24");
    assert!(mutate(&|b| b.truncate(20)).is_err(), "short datagram");
    assert!(
        mutate(&|b| b.resize(packet::MAX_PACKET + 1, 0)).is_err(),
        "oversized datagram"
    );
    assert!(mutate(&|b| b[1] |= 0x04).is_err(), "A bit with no section");
    let up_without_yours = control(State::Up, 0).encode(None);
    assert!(
        packet::decode(&up_without_yours).is_err(),
        "Your Discriminator 0 while Up"
    );
}

#[test]
fn state_machine_and_meticulous_sequence() {
    let now = tokio::time::Instant::now();
    let key = AuthConfig::new(AuthType::MeticulousKeyedSha1, 1, "k").unwrap();
    let mut s = Session::new(9, Timers::default(), Some(key.clone()), false);
    let send = |s: &mut Session, p: ControlPacket, seq: u32| {
        let bytes = p.encode(Some((&key, seq)));
        let decoded = packet::decode(&bytes).unwrap();
        s.receive(&bytes, &decoded, now)
    };
    let r = send(&mut s, control(State::Down, 0), 100).unwrap();
    assert_eq!(r.transition, Some((State::Down, State::Init)));
    // Meticulous: the same sequence number again is a replay.
    assert!(send(&mut s, control(State::Init, 9), 100).is_err());
    let r = send(&mut s, control(State::Init, 9), 101).unwrap();
    assert_eq!(r.transition, Some((State::Init, State::Up)));
    // Up lowers Desired Min TX from a second to the configured 300 ms — by Poll Sequence.
    let p = packet::decode(&s.packet(false)).unwrap();
    assert!(p.poll && p.desired_min_tx_us == 300_000, "{p:?}");
    // Too far ahead of the window (3 × Detect Mult) is refused too.
    assert!(send(&mut s, control(State::Up, 9), 101 + 10).is_err());
    // The peer going Down takes us Down with "neighbor signaled session down".
    let r = send(&mut s, control(State::Down, 9), 102).unwrap();
    assert_eq!(r.transition, Some((State::Up, State::Down)));
    assert_eq!(s.local_diag, 3);
}
