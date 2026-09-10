//! Repeatable engine load, refresh, overload, withdrawal and recovery workload.
//! Run release builds; generation and packet cloning are outside event timing.
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;
use std::time::Instant;

use babel_protocol::{
    Engine, EngineConfig, Event, INFINITY, Packet, ResolvedUpdate, ResourceLimits, RouteKey,
    RouterId, Tlv, WiredMetric,
};

fn neighbor(peer: u16) -> IpAddr {
    Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, peer).into()
}

fn packet(peer: u16, routes: std::ops::Range<usize>, seqno: u16) -> Packet {
    Packet {
        tlvs: routes
            .map(|n| {
                Tlv::Update(ResolvedUpdate {
                    key: Some(
                        RouteKey::new(
                            format!("2001:db8:{peer:x}:{n:x}::/64").parse().unwrap(),
                            None,
                        )
                        .unwrap(),
                    ),
                    router_id: Some(RouterId::new([peer as u8; 8]).unwrap()),
                    next_hop: Some(neighbor(peer)),
                    interval_cs: 1600,
                    seqno,
                    metric: 100,
                    v4_via_v6: false,
                    sub_tlvs: vec![],
                })
            })
            .collect(),
    }
}

fn receive(engine: &mut Engine, peer: u16, packet: Packet, timings: &mut Vec<u128>) {
    let event = Event::PacketReceived {
        interface: format!("p{peer}"),
        source: neighbor(peer),
        packet,
        now_ms: 1,
    };
    let start = Instant::now();
    let actions = engine.handle(event);
    timings.push(start.elapsed().as_nanos());
    std::hint::black_box(actions);
}

fn report(engine: &Engine, phase: &str, timings: &mut [u128]) {
    timings.sort_unstable();
    let status = engine.resource_status();
    println!(
        "{{\"phase\":\"{phase}\",\"events\":{},\"total_ms\":{:.3},\"p99_us\":{:.3},\"max_us\":{:.3},\"candidates\":{},\"sources\":{},\"selected\":{},\"rejected\":{}}}",
        timings.len(),
        timings.iter().sum::<u128>() as f64 / 1e6,
        timings[timings.len() * 99 / 100] as f64 / 1e3,
        timings[timings.len() - 1] as f64 / 1e3,
        status.candidates,
        status.sources,
        engine.selected_routes().len(),
        status.rejected_candidates_per_neighbor + status.rejected_candidates_global,
    );
}

fn main() {
    let count: usize = std::env::args()
        .nth(1)
        .map_or(4096, |s| s.parse().expect("candidate count"));
    assert!((1..=32768).contains(&count));
    let mut config = EngineConfig::recommended(RouterId::new([1; 8]).unwrap());
    config.metric = Arc::new(WiredMetric::new(96, 1, 1).unwrap());
    config.limits = ResourceLimits {
        max_neighbors: 4,
        max_candidates: count * 4,
        max_candidates_per_neighbor: count,
    };
    let mut engine = Engine::new(config);
    for peer in 2..=5 {
        engine.handle(Event::InterfaceUp {
            interface: format!("p{peer}"),
            local_addresses: vec![],
            now_ms: 0,
        });
        receive(
            &mut engine,
            peer,
            Packet {
                tlvs: vec![
                    Tlv::Hello {
                        unicast: false,
                        seqno: 1,
                        interval_cs: 400,
                        sub_tlvs: vec![],
                    },
                    Tlv::Ihu {
                        address: None,
                        rxcost: 96,
                        interval_cs: 1200,
                        sub_tlvs: vec![],
                    },
                ],
            },
            &mut vec![],
        );
    }
    let generated = Instant::now();
    let inputs: Vec<_> = (2..=5)
        .flat_map(|peer| {
            (0..count)
                .step_by(32)
                .map(move |n| (peer, packet(peer, n..(n + 32).min(count), 1)))
        })
        .collect();
    println!(
        "{{\"phase\":\"generate\",\"per_neighbor\":{count},\"batch\":32,\"ms\":{:.3}}}",
        generated.elapsed().as_secs_f64() * 1000.0
    );
    for phase in ["learn", "refresh"] {
        let mut timings = Vec::new();
        for (peer, input) in &inputs {
            receive(&mut engine, *peer, input.clone(), &mut timings);
        }
        assert_eq!(engine.resource_status().candidates, count * 4);
        report(&engine, phase, &mut timings);
    }
    let overflow = packet(2, count..count + 32, 1);
    let mut timings = Vec::new();
    for _ in 0..1000 {
        receive(&mut engine, 2, overflow.clone(), &mut timings);
    }
    assert_eq!(
        engine.resource_status().rejected_candidates_per_neighbor,
        32_000
    );
    report(&engine, "overload", &mut timings);
    let mut timings = Vec::new();
    let retract = Tlv::Update(ResolvedUpdate {
        key: None,
        router_id: None,
        next_hop: None,
        seqno: 0,
        metric: INFINITY,
        interval_cs: 1600,
        v4_via_v6: false,
        sub_tlvs: vec![],
    });
    receive(
        &mut engine,
        2,
        Packet {
            tlvs: vec![retract],
        },
        &mut timings,
    );
    assert_eq!(engine.selected_routes().len(), count * 3);
    report(&engine, "withdraw_neighbor", &mut timings);
    let mut timings = Vec::new();
    for n in (0..count).step_by(32) {
        receive(
            &mut engine,
            2,
            packet(2, n..(n + 32).min(count), 2),
            &mut timings,
        );
    }
    assert_eq!(engine.selected_routes().len(), count * 4);
    report(&engine, "recover", &mut timings);
}
