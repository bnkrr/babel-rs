mod common;

use std::collections::HashMap;

use babel_protocol::{
    Action, INFINITY, OutboundTlv, ResolvedUpdate, ResourceLimits, RouteKey, RouterId, Tlv,
};
use common::{ConformanceHarness, id, key};

const SOURCE_LIFETIME_MS: u64 = 180_000;
const CHURN_SECONDS: u64 = 900;
const DRAIN_SECONDS: u64 = 300;

fn origin(generation: u64) -> RouterId {
    RouterId::new((1_000_000 + generation).to_be_bytes()).unwrap()
}

fn routes() -> Vec<RouteKey> {
    (0..8)
        .flat_map(|group| {
            let v4 = key(&format!("192.0.{}.0/24", group + 2));
            let v6 = key(&format!("2001:db8:{group:x}::/64"));
            [
                v4,
                RouteKey::new(v4.destination, Some("10.0.0.0/8".parse().unwrap())).unwrap(),
                v6,
                RouteKey::new(v6.destination, Some("2001:db8:aaaa::/48".parse().unwrap())).unwrap(),
            ]
        })
        .collect()
}

fn refresh_neighbor(seqno: u16) -> Vec<Tlv> {
    vec![
        Tlv::Hello {
            unicast: false,
            seqno,
            interval_cs: 400,
            sub_tlvs: vec![],
        },
        Tlv::Ihu {
            address: None,
            rxcost: 96,
            interval_cs: 1200,
            sub_tlvs: vec![],
        },
    ]
}

fn update(route: RouteKey, router_id: RouterId, seqno: u16, metric: u16) -> Tlv {
    Tlv::Update(ResolvedUpdate {
        key: Some(route),
        router_id: Some(router_id),
        next_hop: Some("fe80::2".parse().unwrap()),
        interval_cs: 1600,
        seqno,
        metric,
        v4_via_v6: route.destination.addr().is_ipv4(),
        sub_tlvs: vec![],
    })
}

/// Expected history comes from public finite advertisements, not engine state.
/// Receiving Updates or sending retractions must not extend an old source's life.
#[derive(Default)]
struct AdvertisedHistory(HashMap<(RouteKey, RouterId), u64>);

impl AdvertisedHistory {
    fn observe(&mut self, actions: Vec<Action>, now_ms: u64) {
        for action in actions {
            if let Action::Send { packet, .. } = action {
                for tlv in packet.tlvs {
                    if let OutboundTlv::Update(update) = tlv
                        && update.metric != INFINITY
                    {
                        self.0
                            .insert((update.key.unwrap(), update.router_id.unwrap()), now_ms);
                    }
                }
            }
        }
    }

    fn tick(&mut self, h: &mut ConformanceHarness, now_ms: u64) {
        self.0
            .retain(|_, last| now_ms <= *last + SOURCE_LIFETIME_MS);
        self.observe(h.tick(now_ms), now_ms);
        assert_eq!(
            h.engine.resource_status().sources,
            self.0.len(),
            "at {now_ms}ms"
        );
    }

    fn receive(
        &mut self,
        h: &mut ConformanceHarness,
        interface: &str,
        source: &str,
        tlvs: Vec<Tlv>,
    ) {
        let actions = h.receive(interface, source, tlvs);
        self.observe(actions, h.now_ms);
        assert_eq!(
            h.engine.resource_status().sources,
            self.0.len(),
            "at {}ms",
            h.now_ms
        );
    }
}

#[test]
fn sustained_router_id_churn_reclaims_history_and_preserves_feasibility() {
    let rotating = routes();
    let healthy = key("2001:db8:ffff::/64");
    let mut h = ConformanceHarness::with_limits(
        id(1),
        ResourceLimits {
            max_neighbors: 2,
            max_candidates: rotating.len() + 1,
            max_candidates_per_neighbor: rotating.len(),
        },
    );
    for interface in ["churn", "healthy", "out"] {
        h.interface(interface);
    }
    let mut history = AdvertisedHistory::default();
    let mut peak_sources = 0;
    let check_healthy = |h: &ConformanceHarness| {
        let selected = h.engine.selected_routes();
        let stable = selected.iter().find(|route| route.key == healthy).unwrap();
        assert_eq!(
            (stable.router_id, stable.seqno, stable.metric),
            (id(3), 500, 116)
        );
        assert_eq!(stable.interface, "healthy");
    };

    // Fifteen minutes of simulated time: 28,800 source identities on the same
    // 32 candidates, across five full GC windows. IPv4/IPv6 ordinary and SADR
    // routes deliberately share destinations to check complete source keys.
    for second in 1..=CHURN_SECONDS + DRAIN_SECONDS {
        history.tick(&mut h, second * 1000);
        if second > 1 {
            check_healthy(&h);
        }
        let mut noisy = refresh_neighbor(second as u16);
        if second <= CHURN_SECONDS {
            noisy.extend(
                rotating
                    .iter()
                    .map(|route| update(*route, origin(second), 100, 10)),
            );
        }
        history.receive(&mut h, "churn", "fe80::2", noisy);
        if second > 1 {
            check_healthy(&h);
        }
        let mut quiet = refresh_neighbor(second as u16);
        let mut healthy_update = update(healthy, id(3), 500, 20);
        if let Tlv::Update(value) = &mut healthy_update {
            value.next_hop = Some("fe80::3".parse().unwrap());
        }
        quiet.push(healthy_update);
        history.receive(&mut h, "healthy", "fe80::3", quiet);
        check_healthy(&h);

        let selected = h.engine.selected_routes();
        if second <= CHURN_SECONDS {
            assert_eq!(selected.len(), rotating.len() + 1);
            for route in selected.iter().filter(|route| route.key != healthy) {
                assert_eq!(
                    (route.router_id, route.seqno, route.metric),
                    (origin(second), 100, 106)
                );
                assert_eq!(route.interface, "churn");
            }
        }

        // Probe an old, still-live source with a worse, older distance. Churn
        // must not bypass feasibility by prematurely forgetting source history.
        if second <= CHURN_SECONDS && second.is_multiple_of(100) {
            let probe = rotating[(second / 100) as usize % rotating.len()];
            history.receive(
                &mut h,
                "churn",
                "fe80::2",
                vec![update(probe, origin(second - 30), 0, 500)],
            );
            check_healthy(&h);
            assert!(
                h.engine
                    .selected_routes()
                    .iter()
                    .all(|route| route.key != probe)
            );
            history.receive(
                &mut h,
                "churn",
                "fe80::2",
                vec![update(probe, origin(second), 100, 10)],
            );
            check_healthy(&h);
            assert_eq!(h.engine.selected_routes().len(), rotating.len() + 1);
        }

        let resources = h.engine.resource_status();
        let neighbors = h.engine.neighbour_status(h.now_ms);
        assert_eq!(neighbors.len(), 2);
        assert_eq!(
            neighbors
                .iter()
                .map(|neighbor| neighbor.candidates)
                .sum::<usize>(),
            resources.candidates
        );
        assert!(
            neighbors
                .iter()
                .all(|neighbor| neighbor.candidates <= rotating.len())
        );
        assert!(resources.candidates <= rotating.len() + 1);
        assert_eq!(resources.rejected_candidates_global, 0);
        assert_eq!(resources.rejected_candidates_per_neighbor, 0);
        // At most the last three minutes, with one extra boundary generation
        // for periodic advertisements just before a source changes.
        assert!(resources.sources <= rotating.len() * 182 + 1);
        peak_sources = peak_sources.max(resources.sources);
    }

    assert!(
        peak_sources >= rotating.len() * 180,
        "test failed to accumulate sustained history"
    );
    assert_eq!(h.engine.resource_status().sources, 1);
    assert_eq!(h.engine.resource_status().candidates, 1);
    assert_eq!(h.engine.resource_status().pending_requests, 0);
    assert_eq!(h.engine.selected_routes().len(), 1);

    // After expiry, the very first Router-ID can start with an older sequence
    // and worse metric. Remaining healthy routes and reclaimed quota still work.
    history.receive(
        &mut h,
        "churn",
        "fe80::2",
        rotating
            .iter()
            .map(|route| update(*route, origin(1), 0, 500))
            .collect(),
    );
    assert_eq!(h.engine.selected_routes().len(), rotating.len() + 1);
    assert_eq!(h.engine.resource_status().sources, rotating.len() + 1);
    for route in h
        .engine
        .selected_routes()
        .iter()
        .filter(|route| route.key != healthy)
    {
        assert_eq!(
            (route.router_id, route.seqno, route.metric),
            (origin(1), 0, 596)
        );
    }
}
