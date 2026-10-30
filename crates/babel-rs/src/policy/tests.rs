use super::*;

fn config(policy: &str) -> Config {
    Config::parse(&format!(
        "[[interfaces]]\nmatch = [\"lan*\"]\n[export]\n[[export.views]]\ntable = 201\n{policy}"
    ))
    .unwrap()
}

fn key(destination: &str, source: Option<&str>) -> RouteKey {
    RouteKey::new(
        destination.parse().unwrap(),
        source.map(|s| s.parse().unwrap()),
    )
    .unwrap()
}

#[test]
fn zero_source_matches_only_ordinary_routes_in_that_family() {
    for (zero, subnet, child, host) in [
        ("0.0.0.0/0", "192.0.2.0/24", "192.0.2.0/25", "192.0.2.7/24"),
        ("::/0", "2001:db8::/32", "2001:db8::/48", "2001:db8::7/32"),
    ] {
        let cfg = config(&format!(
            r#"
[policy.import]
default = "allow"
[[policy.import.rules]]
action = "deny"
source = {{ prefix = "{zero}", match = "exact" }}
destination = {{ prefix = "{zero}", match = "exact" }}
"#
        ));
        assert!(!cfg.policy.import.allows_key(key(zero, None)));
        assert!(cfg.policy.import.allows_key(key(zero, Some(subnet))));
        assert!(cfg.policy.import.allows_key(key(subnet, None)));
        let cfg = config(&format!(
            r#"
[policy.import]
default = "deny"
[[policy.import.rules]]
action = "allow"
source = {{ prefix = "{host}", match = "within" }}
"#
        ));
        assert!(cfg.policy.import.allows_key(key(zero, Some(subnet))));
        assert!(cfg.policy.import.allows_key(key(zero, Some(child))));
        assert!(!cfg.policy.import.allows_key(key(zero, None)));
    }
}

#[test]
fn ordered_rules_match_route_context_and_keep_stages_independent() {
    let cfg = config(
        r#"
[policy.import]
default = "deny"
[[policy.import.rules]]
action = "deny"
destination = { prefix = "192.0.2.128/25", match = "within" }
[[policy.import.rules]]
action = "allow"
destination = { prefix = "192.0.2.0/24", match = "within" }
interfaces = ["lan*"]
neighbor = "fe80::1"

[policy.announce]
default = "allow"
[[policy.announce.rules]]
action = "deny"
interfaces = ["uplink*"]
locally_originated = false
"#,
    );
    let received = key("192.0.2.0/24", None);
    let peer = Some("fe80::1".parse().unwrap());
    assert!(cfg.policy.import.allows(received, Some("lan0"), peer, None));
    assert!(
        !cfg.policy
            .import
            .allows(key("192.0.2.128/25", None), Some("lan0"), peer, None)
    );
    assert!(
        cfg.policy
            .import
            .allows(key("192.0.2.0/25", None), Some("lan0"), peer, None)
    );
    assert!(!cfg.policy.import.allows(
        received,
        Some("lan0"),
        Some("fe80::2".parse().unwrap()),
        None
    ));
    assert!(!cfg.policy.import.allows(received, Some("wan0"), peer, None));
    assert!(
        !cfg.policy
            .announce
            .allows(received, Some("uplink0"), None, Some(false))
    );
    assert!(
        cfg.policy
            .announce
            .allows(received, Some("uplink0"), None, Some(true))
    );
    assert!(
        cfg.policy
            .announce
            .allows(received, Some("lan0"), None, Some(false))
    );
}

#[test]
fn filtering_a_table_does_not_change_import_or_announcement() {
    let cfg = config("[export.views.filter]\ndefault = \"deny\"\n");
    let key = key("192.0.2.0/24", None);
    assert!(!cfg.export.views[0].filter.allows_key(key));
    assert!(cfg.policy.import.allows_key(key));
    assert!(cfg.policy.announce.allows_key(key));
}

#[test]
fn invalid_stage_fields_and_ambiguous_prefix_rules_are_rejected() {
    let base = "[[interfaces]]\nmatch = [\"lan0\"]\n[export]\n[[export.views]]\ntable = 201\n";
    for invalid in [
        "[[policy.import.rules]]\naction = \"allow\"\nlocally_originated = true",
        "[[policy.announce.rules]]\naction = \"allow\"\nneighbor = \"fe80::1\"",
        "[[export.views.filter.rules]]\naction = \"allow\"\ninterfaces = [\"lan0\"]",
        "[[policy.import.rules]]\naction = \"allow\"\ninterfaces = []",
        "[[policy.import.rules]]\naction = \"allow\"\ndestination = { prefix = \"192.0.2.0/24\" }",
        "[[policy.import.rules]]\naction = \"allow\"\nsource = { prefix = \"::/0\", match = \"exact\" }\ndestination = { prefix = \"192.0.2.0/24\", match = \"exact\" }",
        "ordinary_prefixes = [\"192.0.2.0/24\"]",
        "[[policy.import.rules]]\naction = \"allow\"\nroute_kind = \"ordinary\"",
    ] {
        assert!(
            Config::parse(&format!("{base}{invalid}\n")).is_err(),
            "{invalid}"
        );
    }
}
