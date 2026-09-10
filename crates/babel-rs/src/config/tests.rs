use super::*;

#[test]
fn ipv4_next_hop_defaults_overrides_and_reloads() {
    let base = "[[interfaces]]\nmatch = [\"lan*\"]\n";
    let export = "[export]\n[[export.views]]\ntable = 201\n";
    let original = Config::parse(&format!("{base}{export}")).unwrap();
    assert_eq!(
        original.effective_interface("lan0").unwrap().ipv4_next_hop,
        Ipv4NextHop::Auto
    );
    for (mode, expected) in [
        ("auto", babel_protocol::Ipv4NextHop::Auto),
        ("ipv4", babel_protocol::Ipv4NextHop::Ipv4),
        ("ipv6", babel_protocol::Ipv4NextHop::Ipv6),
    ] {
        let changed =
            Config::parse(&format!("{base}ipv4_next_hop = \"{mode}\"\n{export}")).unwrap();
        assert!(original.reload_identity_matches(&changed));
        assert_eq!(
            changed
                .effective_interface("lan0")
                .unwrap()
                .build_policy()
                .unwrap()
                .ipv4_next_hop,
            expected
        );
    }
    assert!(Config::parse(&format!("{base}ipv4_next_hop = \"both\"\n{export}")).is_err());
}

#[test]
fn shutdown_timeout_defaults_validates_and_can_reload() {
    let base = "[[interfaces]]\nmatch = [\"wg0\"]\n[export]\n[[export.views]]\ntable = 20001\n";
    let original = Config::parse(base).unwrap();
    assert_eq!(original.shutdown_timeout_ms, 5_000);
    let changed = Config::parse(&format!("shutdown_timeout_ms = 250\n{base}")).unwrap();
    assert_eq!(changed.shutdown_timeout_ms, 250);
    assert!(original.reload_identity_matches(&changed));
    for value in ["0", "-1", "1.5", "4294967296"] {
        assert!(Config::parse(&format!("shutdown_timeout_ms = {value}\n{base}")).is_err());
    }
}

#[test]
fn limits_default_override_zero_and_reload_identity() {
    let base = "[[interfaces]]\nmatch = [\"wg0\"]\n[export]\n[[export.views]]\ntable = 20001\n";
    let original = Config::parse(base).unwrap();
    assert_eq!(
        original.limits.effective(),
        babel_protocol::ResourceLimits::default()
    );
    let explicit = Config::parse(&format!("{base}[limits]\nmax_candidates = 16384\n")).unwrap();
    assert!(original.reload_identity_matches(&explicit));
    for field in [
        "max_neighbors",
        "max_candidates",
        "max_candidates_per_neighbor",
    ] {
        let changed = Config::parse(&format!("{base}[limits]\n{field} = 0\n")).unwrap();
        assert!(!original.reload_identity_matches(&changed));
        assert!(Config::parse(&format!("{base}[limits]\n{field} = -1\n")).is_err());
    }
    let partial = Config::parse(&format!(
        "{base}[limits]\nmax_candidates_per_neighbor = 20\n"
    ))
    .unwrap();
    assert_eq!(partial.limits.effective().max_candidates_per_neighbor, 20);
    assert_eq!(partial.limits.effective().max_candidates, 16384);
    assert!(Config::parse(&format!("{base}[limits]\nmax_routes = 20\n")).is_err());
}

#[test]
fn strict_config_supports_policy_views() {
    let config: Config = toml::from_str(
        r#"
[[interfaces]]
match = ["wg0"]
[[origins]]
destination = "192.0.2.0/24"
source = "10.0.0.0/8"
[export]
manage_rules = true
[[export.views]]
table = 20000
[[export.views]]
table = 20001
source = "10.0.0.0/8"
"#,
    )
    .unwrap();
    config.validate().unwrap();
    assert!(config.origins[0].key().unwrap().source.is_some());
    assert_eq!(config.export.rule_priority(config.export.views[1]), 10120);
}

#[test]
fn mixed_families_are_rejected() {
    let origin = Origin {
        destination: "192.0.2.0/24".parse().unwrap(),
        source: Some("2001:db8::/32".parse().unwrap()),
        metric: 0,
    };
    assert!(matches!(
        origin.key(),
        Err(ConfigError::MixedAddressFamilies)
    ));
}

#[test]
fn source_views_cannot_collide_in_one_table() {
    let config: Config = toml::from_str(
        r#"
[[interfaces]]
match = ["wg0"]
[export]
[[export.views]]
table = 20001
source = "10.0.0.0/8"
[[export.views]]
table = 20001
source = "10.1.0.0/16"
"#,
    )
    .unwrap();
    assert!(matches!(
        config.validate(),
        Err(ConfigError::SharedSourceTable)
    ));
}

#[test]
fn overlapping_source_views_query_more_specific_sources_first() {
    let config = Config::parse(
        r#"
[[interfaces]]
match = ["wg0"]
[export]
[[export.views]]
table = 20001
source = "10.0.0.0/8"
[[export.views]]
table = 20002
source = "10.1.0.0/16"
"#,
    )
    .unwrap();
    assert!(
        config.export.rule_priority(config.export.views[1])
            < config.export.rule_priority(config.export.views[0])
    );
}

#[test]
fn source_view_priority_overrides_and_canonical_prefixes_are_validated() {
    let text = r#"
[[interfaces]]
match = ["wg0"]
[export]
automatic_sources = false
source_rule_priority = 500
[[export.views]]
table = 201
source = "10.1.2.3/16"
rule_priority = 200
[[export.views]]
table = 202
source = "10.1.2.0/24"
rule_priority = 100
"#;
    let config = Config::parse(text).unwrap();
    assert_eq!(
        config.export.views[0].source.unwrap().to_string(),
        "10.1.0.0/16"
    );
    assert!(matches!(
        Config::parse(&text.replace("rule_priority = 100", "rule_priority = 300")),
        Err(ConfigError::SourceRuleOrder(_, _))
    ));
    assert!(matches!(
        Config::parse(&text.replace("automatic_sources = false", "automatic_sources = true")),
        Err(ConfigError::AutomaticSourceConfig)
    ));
    assert!(matches!(
        Config::parse(&text.replace("source_rule_priority = 500", "source_rule_priority = 0")),
        Err(ConfigError::SourcePriorityRange)
    ));
    let auto = Config::parse(
        &text
            .replace("automatic_sources = false", "automatic_sources = true")
            .replace("rule_priority = 200", "")
            .replace("rule_priority = 100", ""),
    )
    .unwrap();
    assert_eq!(auto.export.rule_priority(auto.export.views[0]), 612);
    assert_eq!(auto.export.rule_priority(auto.export.views[1]), 604);
}

#[test]
fn key_files_are_reread_on_reload_and_secret_errors_are_redacted() {
    let path = std::env::temp_dir().join(format!("babel-mac-config-{}.key", std::process::id()));
    let text = format!(
        r#"
[[interfaces]]
match = ["wg0"]
[[interfaces.mac.keys]]
key_file = "{}"
[export]
[[export.views]]
table = 201
"#,
        path.display()
    );
    fs::write(&path, "11".repeat(32)).unwrap();
    let first = Config::parse(&text).unwrap();
    let first_mac = first.effective_interface("wg0").unwrap().mac.unwrap();
    assert!(first_mac.is_strict());
    fs::write(&path, "22".repeat(32)).unwrap();
    let second = Config::parse(&text)
        .unwrap()
        .effective_interface("wg0")
        .unwrap()
        .mac
        .unwrap();
    assert_ne!(first_mac, second);
    let secret = "not-a-valid-secret";
    fs::write(&path, secret).unwrap();
    let error = Config::parse(&text).unwrap_err();
    assert!(!error.to_string().contains(secret));
    fs::remove_file(path).unwrap();
    assert!(matches!(Config::parse(&text), Err(ConfigError::Mac(_))));
}

#[test]
fn zero_length_source_is_normalised_to_the_ordinary_view() {
    let config = Config::parse(
        r#"
[[interfaces]]
match = ["wg0"]
[export]
[[export.views]]
table = 20000
source = "0.0.0.0/0"
"#,
    )
    .unwrap();
    assert_eq!(config.export.views[0].source, None);
}

#[test]
fn interface_patterns_support_exact_star_and_question() {
    let config: Config = toml::from_str(
        r#"
[[interfaces]]
match = ["test-*", "backbone?"]
[export]
[[export.views]]
table = 20000
"#,
    )
    .unwrap();
    config.validate().unwrap();
    assert!(config.effective_interface("test-a-b").is_some());
    assert!(config.effective_interface("backbone0").is_some());
    assert!(config.effective_interface("access0").is_none());
    assert!(config.effective_interface("backbone10").is_none());
}

#[test]
fn duplicate_interface_patterns_are_rejected() {
    let config: Config = toml::from_str(
        r#"
[[interfaces]]
match = ["test-*", "test-*"]
[export]
[[export.views]]
table = 20000
"#,
    )
    .unwrap();
    assert!(matches!(
        config.validate(),
        Err(ConfigError::DuplicateInterfacePattern(_))
    ));
}

#[test]
fn interface_sections_use_first_match_and_type_defaults() {
    let config = Config::parse(
        r#"
[[interfaces]]
match = ["test-special-*"]
link_type = "wireless"
hello_interval_ms = 1000

[[interfaces]]
match = ["test-*"]
link_type = "tunnel"

[export]
[[export.views]]
table = 20000
"#,
    )
    .unwrap();

    let special = config.effective_interface("test-special-0").unwrap();
    assert_eq!(special.section, 0);
    assert_eq!(special.link_type, LinkType::Wireless);
    assert_eq!(special.metric.build().unwrap().name(), "etx");
    assert!(!special.split_horizon);
    assert_eq!(special.hello_interval_cs, 100);
    assert_eq!(special.update_interval_cs, 400);

    let tunnel = config.effective_interface("test-normal-0").unwrap();
    assert_eq!(tunnel.section, 1);
    assert_eq!(tunnel.metric.build().unwrap().name(), "rtt(wired)");
    assert!(tunnel.split_horizon);
    assert_eq!(tunnel.hello_interval_cs, 400);
    assert_eq!(tunnel.update_interval_cs, 1600);
    assert!(config.effective_interface("eth0").is_none());
}

#[test]
fn explicit_interface_values_replace_type_defaults() {
    let config = Config::parse(
        r#"
[[interfaces]]
match = ["mesh0"]
link_type = "wireless"
split_horizon = true
hello_interval_ms = 2500
update_interval_ms = 7000
[interfaces.metric]
type = "wired"
nominal_cost = 128

[export]
[[export.views]]
table = 20000
"#,
    )
    .unwrap();
    let policy = config.effective_interface("mesh0").unwrap();
    assert_eq!(policy.metric.build().unwrap().name(), "wired");
    assert!(policy.split_horizon);
    assert_eq!(policy.hello_interval_cs, 250);
    assert_eq!(policy.update_interval_cs, 700);
}

#[test]
fn legacy_configuration_forms_are_rejected() {
    for contents in [
        r#"
interfaces = ["eth0"]
[export]
[[export.views]]
table = 20000
"#,
        r#"
[[interfaces]]
match = ["eth0"]
[metric]
type = "wired"
[export]
[[export.views]]
table = 20000
"#,
    ] {
        assert!(matches!(
            Config::parse(contents),
            Err(ConfigError::Parse(_))
        ));
    }
}

#[test]
fn interface_intervals_require_wire_representable_centiseconds() {
    for value in [0, 11, 655_360] {
        let error = Config::parse(&format!(
            r#"
[[interfaces]]
match = ["eth0"]
hello_interval_ms = {value}
[export]
[[export.views]]
table = 20000
"#
        ))
        .unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidInterfaceInterval { .. }
        ));
    }
}

#[test]
fn omitted_metric_uses_rfc_wired_defaults() {
    let config = Config::parse(
        r#"
[[interfaces]]
match = ["eth0"]
[export]
[[export.views]]
table = 20000
"#,
    )
    .unwrap();
    assert_eq!(
        config
            .effective_interface("eth0")
            .unwrap()
            .metric
            .build()
            .unwrap()
            .name(),
        "wired"
    );
}

#[test]
fn rtt_metric_supports_a_configured_etx_base() {
    let config = Config::parse(
        r#"
[[interfaces]]
match = ["mesh0"]
[interfaces.metric]
type = "rtt"
probe_interval_ms = 1500
half_life_ms = 5000
min_rtt_ms = 5
max_rtt_ms = 80
max_penalty = 200
[interfaces.metric.base]
type = "etx"
window = 8
[route_selection]
switch_margin_percent = 7
switch_margin_metric = 12
better_for_ms = 5000
[export]
[[export.views]]
table = 20000
"#,
    )
    .unwrap();
    let profile = config
        .effective_interface("mesh0")
        .unwrap()
        .metric
        .build()
        .unwrap();
    assert_eq!(profile.name(), "rtt(etx)");
    assert!(profile.timestamps_enabled());
    assert_eq!(profile.rtt_probe_interval_ms(), Some(1500));
    assert_eq!(config.route_selection.better_for_ms, 5000);
}

#[test]
fn invalid_metric_parameters_are_rejected() {
    let error = Config::parse(
        r#"
[[interfaces]]
match = ["eth0"]
[interfaces.metric]
type = "wired"
received = 4
window = 3
[export]
[[export.views]]
table = 20000
"#,
    )
    .unwrap_err();
    assert!(matches!(error, ConfigError::InvalidMetric(_)));
}

#[test]
fn invalid_route_selection_margin_is_rejected() {
    let error = Config::parse(
        r#"
[[interfaces]]
match = ["eth0"]
[route_selection]
switch_margin_percent = 101
[export]
[[export.views]]
table = 20000
"#,
    )
    .unwrap_err();
    assert!(matches!(error, ConfigError::InvalidRouteSelection(_)));
}

#[test]
fn duplicate_origins_are_rejected_before_reload_commit() {
    let error = Config::parse(
        r#"
[[interfaces]]
match = ["eth0"]
[[origins]]
destination = "2001:db8::/64"
[[origins]]
destination = "2001:db8::/64"
[export]
[[export.views]]
table = 20000
"#,
    )
    .unwrap_err();
    assert!(matches!(error, ConfigError::DuplicateOrigin(_)));
}
