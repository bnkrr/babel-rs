use super::*;
use std::collections::VecDeque;

fn entropy() -> impl FnMut() -> Result<[u8; 16], MacError> {
    let mut next = 0u128;
    move || {
        next += 1;
        Ok(next.to_be_bytes())
    }
}
fn config(algorithm: MacAlgorithm, key: u8) -> MacConfig {
    MacConfig::new(vec![MacKey::new(algorithm, vec![key; 32]).unwrap()]).unwrap()
}
fn endpoints(reverse: bool, multicast: bool, v4: bool) -> DatagramEndpoints {
    let (a, b, m) = if v4 {
        ("192.0.2.1:6696", "192.0.2.2:6696", "224.0.0.111:6696")
    } else {
        ("[fe80::1]:6696", "[fe80::2]:6696", "[ff02::1:6]:6696")
    };
    DatagramEndpoints {
        source: if reverse { b } else { a }.parse().unwrap(),
        destination: if multicast {
            m
        } else if reverse {
            a
        } else {
            b
        }
        .parse()
        .unwrap(),
    }
}
const HELLO: &[u8] = &[42, 2, 0, 8, 4, 6, 0, 0, 0, 1, 1, 144];

fn handshake(
    a: &mut MacSession,
    b: &mut MacSession,
    random: &mut impl FnMut() -> Result<[u8; 16], MacError>,
    v4: bool,
    now: u64,
) {
    let mut queue = VecDeque::from([(
        false,
        a.sign(HELLO, endpoints(false, true, v4), random).unwrap(),
        true,
    )]);
    let mut time = now;
    let mut rounds = 0;
    while let Some((reverse, packet, multicast)) = queue.pop_front() {
        rounds += 1;
        assert!(rounds < 20, "challenge must converge");
        let peer = if reverse { &mut *a } else { &mut *b };
        let result = peer.receive(&packet, endpoints(reverse, multicast, v4), time, random);
        for control in result.controls {
            queue.push_back((
                !reverse,
                peer.sign(&control, endpoints(!reverse, false, v4), random)
                    .unwrap(),
                false,
            ));
        }
        time += 301;
    }
    assert!(a.peers.values().any(|p| p.index.is_some()));
    assert!(b.peers.values().any(|p| p.index.is_some()));
}

#[test]
fn independent_digest_vectors_and_redacted_keys() {
    for (algorithm, expected) in [
        (
            MacAlgorithm::HmacSha256,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
        ),
        (MacAlgorithm::Blake2s128, "86bd6e709a50dc78906f06e6ea30a441"),
    ] {
        let key = MacKey::new(algorithm, vec![11; 20]).unwrap();
        assert_eq!(
            key.digest(b"Hi There")
                .iter()
                .map(|v| format!("{v:02x}"))
                .collect::<String>(),
            expected
        );
        assert!(!format!("{key:?}").contains("bytes"));
    }
}

#[test]
fn handshake_authentication_replay_and_endpoint_binding() {
    for algorithm in [MacAlgorithm::HmacSha256, MacAlgorithm::Blake2s128] {
        for v4 in [false, true] {
            let mut random = entropy();
            let mut a = MacSession::new(config(algorithm, 1), 16, &mut random).unwrap();
            let mut b = MacSession::new(config(algorithm, 1), 16, &mut random).unwrap();
            handshake(&mut a, &mut b, &mut random, v4, 0);
            let meta = endpoints(false, false, v4);
            let packet = a.sign(HELLO, meta, &mut random).unwrap();
            assert_eq!(packet.len(), HELLO.len() + a.overhead());
            assert!(b.receive(&packet, meta, 5000, &mut random).accepted);
            assert!(!b.receive(&packet, meta, 5001, &mut random).accepted);
            let mut forged = packet.clone();
            forged[10] ^= 1;
            assert!(!b.receive(&forged, meta, 5002, &mut random).accepted);
            for wrong in [
                DatagramEndpoints {
                    destination: SocketAddr::new(meta.destination.ip(), 6666),
                    ..meta
                },
                endpoints(false, true, v4),
                endpoints(true, false, v4),
            ] {
                assert!(!b.receive(&packet, wrong, 5003, &mut random).accepted);
            }
            // No authentication state is allocated by unsigned or wrong-key input.
            let mut fresh = MacSession::new(config(algorithm, 2), 16, &mut random).unwrap();
            assert!(!fresh.receive(&packet, meta, 0, &mut random).accepted);
            assert!(!fresh.receive(HELLO, meta, 0, &mut random).accepted);
            assert!(fresh.peers.is_empty());
        }
    }
}

#[test]
fn multicast_and_unicast_reordering_uses_separate_counters() {
    let mut random = entropy();
    let mut a = MacSession::new(config(MacAlgorithm::HmacSha256, 1), 16, &mut random).unwrap();
    let mut b = MacSession::new(config(MacAlgorithm::HmacSha256, 1), 16, &mut random).unwrap();
    handshake(&mut a, &mut b, &mut random, false, 0);
    let m = endpoints(false, true, false);
    let u = endpoints(false, false, false);
    let multicast = a.sign(HELLO, m, &mut random).unwrap();
    let unicast = a.sign(HELLO, u, &mut random).unwrap();
    assert!(b.receive(&unicast, u, 5000, &mut random).accepted);
    assert!(b.receive(&multicast, m, 5001, &mut random).accepted);
    assert!(!b.receive(&multicast, m, 5002, &mut random).accepted);
    assert!(!b.receive(&unicast, u, 5003, &mut random).accepted);
}

#[test]
fn restart_counter_rollover_challenge_expiry_and_rate_limits() {
    let mut random = entropy();
    let mut a = MacSession::new(config(MacAlgorithm::HmacSha256, 1), 16, &mut random).unwrap();
    let mut b = MacSession::new(config(MacAlgorithm::HmacSha256, 1), 16, &mut random).unwrap();
    handshake(&mut a, &mut b, &mut random, false, 0);
    let old_index = a.index;
    a.counter = u32::MAX;
    let meta = endpoints(false, false, false);
    let packet = a.sign(HELLO, meta, &mut random).unwrap();
    assert_ne!(a.index, old_index);
    let first = b.receive(&packet, meta, 5000, &mut random);
    assert!(!first.accepted);
    assert_eq!(first.controls.len(), 1);
    assert!(
        b.receive(&packet, meta, 5299, &mut random)
            .controls
            .is_empty()
    );
    assert_eq!(
        b.receive(&packet, meta, 5300, &mut random).controls.len(),
        1
    );
    let old_reply = control(REPLY, &first.controls[0][6..]);
    let reply = a.sign(&old_reply, meta, &mut random).unwrap();
    assert!(!b.receive(&reply, meta, 40_000, &mut random).accepted);
    // Failed challenges cannot prolong the old accepted Index.
    b.expire(310_000);
    assert!(b.peers.is_empty());
    let mut restarted =
        MacSession::new(config(MacAlgorithm::HmacSha256, 1), 16, &mut random).unwrap();
    handshake(&mut restarted, &mut b, &mut random, false, 311_000);
}

#[test]
fn rotation_overlap_accepts_either_key_and_removal_rejects_old_key() {
    let mut random = entropy();
    let old = config(MacAlgorithm::HmacSha256, 1);
    let new = config(MacAlgorithm::Blake2s128, 2);
    let overlap = MacConfig::new([old.keys.clone(), new.keys.clone()].concat()).unwrap();
    for config in [old.clone(), new.clone()] {
        let mut a = MacSession::new(overlap.clone(), 16, &mut random).unwrap();
        let mut b = MacSession::new(config, 16, &mut random).unwrap();
        handshake(&mut a, &mut b, &mut random, false, 0);
    }
    let mut a = MacSession::new(old, 16, &mut random).unwrap();
    let mut b = MacSession::new(new, 16, &mut random).unwrap();
    let packet = a
        .sign(HELLO, endpoints(false, true, false), &mut random)
        .unwrap();
    assert!(
        b.receive(&packet, endpoints(false, true, false), 0, &mut random)
            .controls
            .is_empty()
    );
    assert!(b.peers.is_empty());
}

// Add a PC and sign manually to exercise malformed/ambiguous authenticated input.
fn signed_body(body: &[u8], config: &MacConfig, meta: DatagramEndpoints) -> Vec<u8> {
    let mut packet = vec![42, 2];
    packet.extend((body.len() as u16).to_be_bytes());
    packet.extend(body);
    let digest = config.keys[0].digest(&meta.authenticated_bytes(&packet).unwrap());
    packet.extend([16, digest.len() as u8]);
    packet.extend(digest);
    packet
}

#[test]
fn malformed_authentication_is_bounded_and_multicast_challenges_are_ignored() {
    let mut random = entropy();
    let config = config(MacAlgorithm::HmacSha256, 1);
    let mut b = MacSession::new(config.clone(), 1, &mut random).unwrap();
    let meta = endpoints(false, true, false);
    for size in 0..HELLO.len() {
        assert!(!b.receive(&HELLO[..size], meta, 0, &mut random).accepted);
    }
    let missing_pc = signed_body(&[18, 0], &config, meta);
    let received = b.receive(&missing_pc, meta, 0, &mut random);
    assert!(!received.accepted);
    assert!(received.controls.is_empty());
    // Valid first PC wins over a second PC. Zero-length indexes/nonces are legal.
    let packet = signed_body(&[17, 4, 0, 0, 0, 9, 17, 4, 0, 0, 0, 99], &config, meta);
    let peer = b.peers.get_mut(&meta.source).unwrap();
    peer.index = Some(Vec::new());
    peer.multicast = 8;
    assert!(b.receive(&packet, meta, 1, &mut random).accepted);
    assert_eq!(b.peers[&meta.source].multicast, 9);
    for body in [
        vec![17, 255],
        vec![17, 3, 0, 0, 0],
        vec![16, 32, 0],
        vec![18, 193].into_iter().chain([0; 193]).collect(),
    ] {
        let bytes = signed_body(&body, &config, meta);
        assert!(!b.receive(&bytes, meta, 2, &mut random).accepted);
    }
    let other = endpoints(true, false, false);
    let packet = signed_body(&[17, 4, 0, 0, 0, 10], &config, other);
    assert!(
        b.receive(&packet, other, 1000, &mut random)
            .controls
            .is_empty()
    );
    assert_eq!(b.peers.len(), 1);
}

#[test]
fn challenge_reply_is_unicast_rate_limited_and_migration_is_explicit() {
    let mut random = entropy();
    let config = config(MacAlgorithm::HmacSha256, 1);
    let mut b = MacSession::new(config.clone(), 8, &mut random).unwrap();
    let meta = endpoints(false, false, false);
    let request = signed_body(&[18, 0, 17, 4, 0, 0, 0, 1], &config, meta);
    let first = b.receive(&request, meta, 0, &mut random);
    assert_eq!(first.controls[0], control(REPLY, &[]));
    assert!(
        b.receive(&request, meta, 299, &mut random)
            .controls
            .is_empty()
    );
    assert_eq!(
        b.receive(&request, meta, 300, &mut random).controls[0],
        control(REPLY, &[])
    );
    let mut migration = MacSession::new(config.accept_unverified(true), 8, &mut random).unwrap();
    assert!(migration.receive(HELLO, meta, 0, &mut random).accepted);
    assert_eq!(
        migration.sign(HELLO, meta, &mut random).unwrap().len(),
        HELLO.len() + migration.overhead()
    );
}
