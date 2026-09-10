use std::collections::BTreeMap;
use std::sync::Arc;

use babel_protocol::{
    ConfigError, Engine, EngineConfig, Event, INFINITY, InterfacePolicy, ResourceLimits, RouteKey,
    RouteSelectionConfig, RouterId, WiredMetric,
};

fn config() -> EngineConfig {
    EngineConfig::recommended(RouterId::new([1; 8]).unwrap())
}

fn key() -> RouteKey {
    RouteKey::new("2001:db8::/64".parse().unwrap(), None).unwrap()
}

#[test]
fn configuration_boundaries_agree_with_engine_construction() {
    for percent in [0, 100, 101, 255] {
        for metric in [0, INFINITY - 1, INFINITY] {
            let mut config = config();
            config.route_selection = RouteSelectionConfig {
                switch_margin_percent: percent,
                switch_margin_metric: metric,
                better_for_ms: 0,
            };
            let expected = percent <= 100 && metric < INFINITY;
            assert_eq!(config.route_selection.validate().is_ok(), expected);
            assert_eq!(config.validate().is_ok(), expected);
            assert_eq!(Engine::try_new(config).is_ok(), expected);
        }
    }
    for (hello, update) in [(0, 1), (1, 0), (1, 1), (u16::MAX, u16::MAX)] {
        let mut config = config();
        config.hello_interval_cs = hello;
        config.update_interval_cs = update;
        let policy = InterfacePolicy {
            ipv4_next_hop: Default::default(),
            metric: Arc::clone(&config.metric),
            hello_interval_cs: hello,
            update_interval_cs: update,
            split_horizon: false,
        };
        assert_eq!(config.validate(), policy.validate());
        assert_eq!(Engine::try_new(config).is_ok(), hello != 0 && update != 0);
    }
    let mut config = config();
    config.limits = ResourceLimits {
        max_neighbors: 0,
        max_candidates: 0,
        max_candidates_per_neighbor: usize::MAX,
    };
    assert!(Engine::try_new(config).is_ok());
}

#[test]
fn invalid_local_events_leave_existing_state_unchanged() {
    let mut engine = Engine::try_new(config()).unwrap();
    let mut reference = Engine::try_new(config()).unwrap();
    for event in [
        Event::InterfaceUp {
            interface: "test0".into(),
            local_addresses: vec!["fe80::1".parse().unwrap()],
            now_ms: 0,
        },
        Event::Originate {
            key: key(),
            metric: 0,
            now_ms: 0,
        },
    ] {
        assert_eq!(
            engine.try_handle(event.clone()).unwrap(),
            reference.handle(event)
        );
    }
    let invalid_policy = InterfacePolicy {
        ipv4_next_hop: Default::default(),
        metric: Arc::new(WiredMetric::default()),
        hello_interval_cs: 0,
        update_interval_cs: 1,
        split_horizon: false,
    };
    let other = RouteKey::new("2001:db8:1::/64".parse().unwrap(), None).unwrap();
    for event in [
        Event::Originate {
            key: key(),
            metric: INFINITY,
            now_ms: 0,
        },
        Event::ReplaceOrigins {
            origins: BTreeMap::from([(key(), 1), (other, INFINITY)]),
            now_ms: 0,
        },
        Event::InterfaceUpWithPolicy {
            interface: "new0".into(),
            local_addresses: vec![],
            policy: invalid_policy.clone(),
            now_ms: 0,
        },
        Event::InterfacePolicyChanged {
            interface: "test0".into(),
            policy: invalid_policy,
            reset_metric: true,
            now_ms: 0,
        },
    ] {
        assert!(engine.try_handle(event).is_err());
        assert_eq!(engine.sequence_number(), reference.sequence_number());
        assert_eq!(engine.resource_status(), reference.resource_status());
    }
    for event in [
        Event::Tick { now_ms: 4000 },
        Event::Withdraw {
            key: key(),
            now_ms: 4000,
        },
    ] {
        assert_eq!(engine.handle(event.clone()), reference.handle(event));
    }
}

#[test]
fn local_route_keys_require_the_constructor_canonical_form() {
    for raw in [
        RouteKey {
            destination: "2001:db8::/64".parse().unwrap(),
            source: Some("192.0.2.0/24".parse().unwrap()),
        },
        RouteKey {
            destination: "2001:db8::1/64".parse().unwrap(),
            source: None,
        },
        RouteKey {
            destination: "2001:db8::/64".parse().unwrap(),
            source: Some("::/0".parse().unwrap()),
        },
    ] {
        assert_eq!(raw.validate(), Err(ConfigError::InvalidRouteKey));
        if let Some(normalized) = RouteKey::new(raw.destination, raw.source) {
            assert!(normalized.validate_origin(0).is_ok());
        }
        let mut engine = Engine::try_new(config()).unwrap();
        assert_eq!(
            engine.try_handle(Event::Withdraw {
                key: raw,
                now_ms: 0
            }),
            Err(ConfigError::InvalidRouteKey)
        );
    }
}

#[test]
#[should_panic(expected = "invalid Babel engine configuration")]
fn infallible_constructor_does_not_bypass_validation() {
    let mut config = config();
    config.hello_interval_cs = 0;
    Engine::new(config);
}

#[test]
#[should_panic(expected = "invalid local Babel event")]
fn infallible_event_handler_does_not_bypass_validation() {
    Engine::new(config()).handle(Event::Originate {
        key: key(),
        metric: INFINITY,
        now_ms: 0,
    });
}

#[test]
#[should_panic(expected = "Hello window must be in 1..=16")]
fn hello_history_rejects_invalid_public_window() {
    babel_protocol::HelloHistory::default().received(17);
}
