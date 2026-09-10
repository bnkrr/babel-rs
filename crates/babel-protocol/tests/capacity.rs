mod common;

use babel_protocol::{Event, INFINITY, ResolvedUpdate, ResourceLimits, RouteKey, Tlv};
use common::{ConformanceHarness, id, key};

fn harness(neighbors: usize, total: usize, per_neighbor: usize) -> ConformanceHarness {
    let mut h = ConformanceHarness::with_limits(
        id(1),
        ResourceLimits {
            max_neighbors: neighbors,
            max_candidates: total,
            max_candidates_per_neighbor: per_neighbor,
        },
    );
    h.interface("a");
    h.interface("b");
    h
}

fn update(route: RouteKey, metric: u16) -> Tlv {
    Tlv::Update(ResolvedUpdate {
        key: Some(route),
        router_id: Some(id(2)),
        next_hop: Some("fe80::2".parse().unwrap()),
        interval_cs: 10,
        seqno: 10,
        metric,
        v4_via_v6: false,
        sub_tlvs: vec![],
    })
}

fn check_accounting(h: &ConformanceHarness) {
    let details = h.engine.neighbour_status(h.now_ms);
    let status = h.engine.resource_status();
    assert_eq!(
        details.iter().map(|n| n.candidates).sum::<usize>(),
        status.candidates
    );
    assert!(status.candidates <= status.limits.max_candidates);
    assert!(
        details
            .iter()
            .all(|n| n.candidates <= status.limits.max_candidates_per_neighbor)
    );
}

#[test]
fn neighbor_limit_preserves_existing_adjacency_and_recovers_after_removal() {
    let mut h = harness(1, 4, 2);
    h.establish_neighbour("a", "fe80::2");
    h.establish_neighbour("b", "fe80::3");
    assert_eq!(h.engine.neighbour_count(), 1);
    assert_eq!(h.engine.resource_status().rejected_neighbors, 1);
    h.update("a", "fe80::2", key("2001:db8:1::/64"), id(2), 1, 10, 100);
    assert_eq!(h.engine.selected_routes().len(), 1);
    h.engine.handle(Event::InterfaceDown {
        interface: "a".into(),
        now_ms: h.now_ms,
    });
    h.establish_neighbour("b", "fe80::3");
    assert_eq!(h.engine.neighbour_count(), 1);
    assert_eq!(
        h.engine.neighbour_status(h.now_ms)[0].address,
        "fe80::3".parse::<std::net::IpAddr>().unwrap()
    );
    check_accounting(&h);
}

#[test]
fn per_neighbor_limit_leaves_room_for_other_neighbors_and_counts_alternates() {
    let mut h = harness(3, 4, 2);
    h.establish_neighbour("a", "fe80::2");
    h.establish_neighbour("b", "fe80::3");
    let first = key("2001:db8:1::/64");
    h.receive(
        "a",
        "fe80::2",
        vec![update(first, 10), update(key("2001:db8:2::/64"), 10)],
    );
    let sources = h.engine.resource_status().sources;
    for n in 3..103 {
        h.receive(
            "a",
            "fe80::2",
            vec![update(key(&format!("2001:db8:{n:x}::/64")), 10)],
        );
    }
    assert_eq!(h.engine.resource_status().sources, sources);
    assert_eq!(
        h.engine.resource_status().rejected_candidates_per_neighbor,
        100
    );
    h.update("b", "fe80::3", first, id(2), 10, 20, 100);
    h.update(
        "b",
        "fe80::3",
        key("2001:db8:ffff::/64"),
        id(3),
        10,
        10,
        100,
    );
    assert_eq!(h.engine.resource_status().candidates, 4);
    assert_eq!(h.engine.selected_routes().len(), 3);
    check_accounting(&h);
}

#[test]
fn global_limit_does_not_block_updates_router_id_changes_or_retractions_in_same_packet() {
    let mut h = harness(2, 2, 3);
    h.establish_neighbour("a", "fe80::2");
    h.establish_neighbour("b", "fe80::3");
    let first = key("2001:db8:1::/64");
    let second = key("2001:db8:2::/64");
    h.receive("a", "fe80::2", vec![update(first, 10), update(second, 10)]);
    h.update("b", "fe80::3", first, id(3), 10, 10, 100);
    assert_eq!(h.engine.resource_status().rejected_candidates_global, 1);
    // A changed Router-ID must not get a fresh neighbor quota or need a new
    // candidate slot; source history still follows the normal protocol rules.
    h.update("a", "fe80::2", first, id(4), 11, 20, 10);
    assert_eq!(
        h.engine
            .selected_routes()
            .iter()
            .find(|r| r.key == first)
            .unwrap()
            .router_id,
        id(4)
    );
    h.receive(
        "a",
        "fe80::2",
        vec![update(key("2001:db8:3::/64"), 10), update(second, INFINITY)],
    );
    assert_eq!(h.engine.resource_status().rejected_candidates_global, 2);
    assert!(h.engine.selected_routes().iter().all(|r| r.key != second));
    // Retractions retain a slot until normal GC, preventing churn from bypassing
    // the budget. Keep the neighbor alive while its candidates expire.
    h.tick(400);
    check_accounting(&h);
    h.receive("b", "fe80::3", vec![update(key("2001:db8:3::/64"), 10)]);
    assert!(
        h.engine
            .selected_routes()
            .iter()
            .any(|r| r.key == key("2001:db8:3::/64"))
    );
    h.tick(1000);
    check_accounting(&h);
}

#[test]
fn source_prefix_and_interface_are_part_of_admission_identity() {
    let mut h = harness(2, 4, 2);
    h.establish_neighbour("a", "fe80::2");
    h.establish_neighbour("b", "fe80::2");
    let ordinary = key("2001:db8:1::/64");
    let specific = RouteKey::new(
        ordinary.destination,
        Some("2001:db8:abcd::/64".parse().unwrap()),
    )
    .unwrap();
    h.receive(
        "a",
        "fe80::2",
        vec![update(ordinary, 10), update(specific, 10)],
    );
    h.receive(
        "b",
        "fe80::2",
        vec![update(ordinary, 10), update(specific, 10)],
    );
    assert_eq!(h.engine.resource_status().candidates, 4);
    check_accounting(&h);
}

#[test]
fn neighbor_expiry_and_interface_replacement_release_all_candidate_slots() {
    let mut h = harness(2, 2, 1);
    h.establish_neighbour("a", "fe80::2");
    h.receive("a", "fe80::2", vec![update(key("2001:db8:1::/64"), 10)]);
    h.tick(100_000);
    assert_eq!(h.engine.neighbour_count(), 0);
    assert_eq!(h.engine.resource_status().candidates, 0);
    h.establish_neighbour("a", "fe80::2");
    h.receive("a", "fe80::2", vec![update(key("2001:db8:2::/64"), 10)]);
    h.engine.handle(Event::InterfaceDown {
        interface: "a".into(),
        now_ms: h.now_ms,
    });
    h.interface("a");
    h.establish_neighbour("a", "fe80::2");
    h.receive("a", "fe80::2", vec![update(key("2001:db8:3::/64"), 10)]);
    assert_eq!(h.engine.resource_status().candidates, 1);
    check_accounting(&h);
}

#[test]
fn zero_limits_disable_new_admission_without_disabling_local_origins() {
    let mut h = harness(0, 0, 0);
    h.establish_neighbour("a", "fe80::2");
    assert_eq!(h.engine.neighbour_count(), 0);
    let actions = h.engine.handle(Event::Originate {
        key: key("2001:db8:1::/64"),
        metric: 0,
        now_ms: h.now_ms,
    });
    assert!(!actions.is_empty());
    let mut h = harness(1, 0, 0);
    h.establish_neighbour("a", "fe80::2");
    h.receive("a", "fe80::2", vec![update(key("2001:db8:1::/64"), 10)]);
    assert_eq!(h.engine.neighbour_count(), 1);
    assert_eq!(h.engine.resource_status().candidates, 0);
    check_accounting(&h);
}

#[test]
fn source_gc_reselects_unchanged_candidates_without_waiting_for_other_changes() {
    let mut h = harness(2, 2, 1);
    for (interface, address) in [("a", "fe80::2"), ("b", "fe80::3")] {
        h.receive(
            interface,
            address,
            vec![
                Tlv::Hello {
                    unicast: false,
                    seqno: 1,
                    interval_cs: u16::MAX,
                    sub_tlvs: vec![],
                },
                Tlv::Ihu {
                    address: None,
                    rxcost: 96,
                    interval_cs: u16::MAX,
                    sub_tlvs: vec![],
                },
            ],
        );
    }
    let prefix = key("2001:db8:1::/64");
    h.update("a", "fe80::2", prefix, id(2), 10, 10, u16::MAX);
    h.update("b", "fe80::3", prefix, id(2), 10, 200, u16::MAX);
    h.update("a", "fe80::2", prefix, id(2), 10, INFINITY, u16::MAX);
    assert!(h.engine.selected_routes().is_empty());
    // Neighbor/candidate timers outlive source GC in this scenario.
    h.tick(180_100);
    assert_eq!(h.engine.selected_routes().len(), 1);
    assert_eq!(h.engine.selected_routes()[0].interface, "b");
    check_accounting(&h);
}
