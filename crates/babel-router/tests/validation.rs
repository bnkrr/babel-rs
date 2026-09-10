use babel_router::{
    BabelRouter, ConfigError, InterfacePolicy, RouteKey, RouteSelectionConfig, RouterError,
    RouterId, WiredMetric,
};
use std::sync::Arc;

fn builder() -> babel_router::BabelRouterBuilder {
    BabelRouter::builder().router_id(RouterId::new([1; 8]).unwrap())
}

fn key() -> RouteKey {
    RouteKey::new("2001:db8::/64".parse().unwrap(), None).unwrap()
}

fn invalid_policy() -> InterfacePolicy {
    InterfacePolicy {
        ipv4_next_hop: Default::default(),
        metric: Arc::new(WiredMetric::default()),
        hello_interval_cs: 0,
        update_interval_cs: 1,
        split_horizon: false,
    }
}

#[tokio::test]
async fn build_validates_the_entire_configuration_before_interface_io() {
    assert!(matches!(
        BabelRouter::builder().validate(),
        Err(RouterError::MissingRouterId)
    ));
    for percent in [101, 255] {
        let config = builder()
            .interface("babel-missing-interface")
            .route_selection(RouteSelectionConfig {
                switch_margin_percent: percent,
                ..RouteSelectionConfig::default()
            });
        assert!(matches!(
            config.validate(),
            Err(RouterError::InvalidConfig(ConfigError::SwitchMarginPercent))
        ));
        assert!(matches!(
            config.build().await,
            Err(RouterError::InvalidConfig(ConfigError::SwitchMarginPercent))
        ));
    }
    assert!(matches!(
        builder()
            .interface("babel-missing-interface")
            .interface_with_policy("second", invalid_policy())
            .build()
            .await,
        Err(RouterError::InvalidInterfacePolicy)
    ));
    assert!(matches!(
        builder()
            .interface("duplicate")
            .interface("duplicate")
            .build()
            .await,
        Err(RouterError::DuplicateInterface(_))
    ));
    assert!(matches!(
        builder()
            .interface("babel-missing-interface")
            .originate(key(), u16::MAX)
            .build()
            .await,
        Err(RouterError::InvalidOriginMetric)
    ));
    assert!(matches!(
        builder()
            .originate(key(), 0)
            .originate(key(), 1)
            .build()
            .await,
        Err(RouterError::DuplicateOrigin(_))
    ));
    assert!(
        builder()
            .route_selection(RouteSelectionConfig {
                switch_margin_percent: 100,
                switch_margin_metric: u16::MAX - 1,
                better_for_ms: 0
            })
            .validate()
            .is_ok()
    );
}

#[tokio::test]
async fn dynamic_input_errors_precede_command_submission() {
    let router = builder().build().await.unwrap();
    let handle = router.handle();
    handle.originate(key(), 0).await.unwrap();
    let seq = handle.status().await.unwrap().sequence_number;
    assert!(matches!(
        handle.originate(key(), u16::MAX).await,
        Err(RouterError::InvalidOriginMetric)
    ));
    assert!(matches!(
        handle.replace_origins(vec![(key(), 1), (key(), 2)]).await,
        Err(RouterError::DuplicateOrigin(_))
    ));
    assert!(matches!(
        handle.replace_origins(vec![(key(), u16::MAX)]).await,
        Err(RouterError::InvalidOriginMetric)
    ));
    assert!(matches!(
        handle
            .add_interface_with_policy("missing", invalid_policy())
            .await,
        Err(RouterError::InvalidInterfacePolicy)
    ));
    assert!(matches!(
        handle
            .update_interface_policy("missing", invalid_policy(), true)
            .await,
        Err(RouterError::InvalidInterfacePolicy)
    ));
    let invalid = RouteKey {
        destination: "2001:db8::/64".parse().unwrap(),
        source: Some("192.0.2.0/24".parse().unwrap()),
    };
    assert!(matches!(
        handle.originate(invalid, 0).await,
        Err(RouterError::InvalidConfig(ConfigError::InvalidRouteKey))
    ));
    assert!(matches!(
        handle.withdraw(invalid).await,
        Err(RouterError::InvalidConfig(ConfigError::InvalidRouteKey))
    ));
    assert_eq!(handle.status().await.unwrap().sequence_number, seq);
    handle.withdraw(key()).await.unwrap();
    assert_eq!(
        handle.status().await.unwrap().sequence_number,
        seq.wrapping_add(1)
    );
    handle.shutdown();
    router.run().await.unwrap();
}
