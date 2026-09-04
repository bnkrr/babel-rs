#![allow(dead_code, clippy::too_many_arguments)]

use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;

use babel_proto::{
    Action, AdditiveMetric, Engine, EngineConfig, Event, OutboundTlv, Packet, ResolvedUpdate,
    RouteKey, RouteSelectionConfig, RouterId, Tlv, WiredMetric,
};
use ipnet::IpNet;

pub fn id(value: u8) -> RouterId {
    RouterId::new([value; 8]).unwrap()
}

pub fn key(prefix: &str) -> RouteKey {
    RouteKey::new(IpNet::from_str(prefix).unwrap(), None).unwrap()
}

pub struct ConformanceHarness {
    pub engine: Engine,
    pub now_ms: u64,
}

impl ConformanceHarness {
    pub fn new(router_id: RouterId) -> Self {
        Self {
            engine: Engine::new(EngineConfig {
                router_id,
                metric: Arc::new(WiredMetric::new(96, 1, 1).unwrap()),
                metric_algebra: Arc::new(AdditiveMetric),
                sequence_number: 7,
                hello_interval_cs: 400,
                update_interval_cs: 1600,
                route_selection: RouteSelectionConfig {
                    switch_margin_percent: 0,
                    switch_margin_metric: 0,
                    better_for_ms: 0,
                },
            }),
            now_ms: 0,
        }
    }

    pub fn interface(&mut self, name: &str) {
        self.engine.handle(Event::InterfaceUp {
            interface: name.into(),
            local_addresses: vec![],
            now_ms: self.now_ms,
        });
    }

    pub fn receive(&mut self, interface: &str, source: &str, tlvs: Vec<Tlv>) -> Vec<Action> {
        self.now_ms += 1;
        self.engine.handle(Event::PacketReceived {
            interface: interface.into(),
            source: source.parse().unwrap(),
            packet: Packet { tlvs },
            now_ms: self.now_ms,
        })
    }

    pub fn establish_neighbour(&mut self, interface: &str, source: &str) {
        self.receive(
            interface,
            source,
            vec![
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
        );
    }

    pub fn update(
        &mut self,
        interface: &str,
        source: &str,
        route: RouteKey,
        router_id: RouterId,
        seqno: u16,
        metric: u16,
        interval_cs: u16,
    ) -> Vec<Action> {
        self.receive(
            interface,
            source,
            vec![Tlv::Update(ResolvedUpdate {
                key: Some(route),
                router_id: Some(router_id),
                next_hop: (metric != babel_proto::INFINITY)
                    .then(|| IpAddr::from_str(source).unwrap()),
                interval_cs,
                seqno,
                metric,
                v4_via_v6: false,
                sub_tlvs: vec![],
            })],
        )
    }

    pub fn tick(&mut self, now_ms: u64) -> Vec<Action> {
        self.now_ms = now_ms;
        self.engine.handle(Event::Tick { now_ms })
    }
}

pub fn sent_tlv(actions: &[Action], predicate: impl Fn(&OutboundTlv) -> bool) -> bool {
    actions.iter().any(|action| match action {
        Action::Send { packet, .. } => packet.tlvs.iter().any(&predicate),
        _ => false,
    })
}
