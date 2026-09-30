//! Explicit opt-in tests, run by tests/e2e/netns-route-reconcile.py.
use super::*;
use rtnetlink::sys::{Socket, SocketAddr, protocols::NETLINK_ROUTE};

fn ip(args: &[&str]) {
    let status = std::process::Command::new("ip")
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "ip {args:?}");
}

// Count actual writes, including deletion, rather than trusting exporter logs.
fn events(socket: &Socket) -> usize {
    let mut count = 0;
    loop {
        let mut bytes = Vec::with_capacity(65536);
        match socket.recv(&mut bytes, libc::MSG_DONTWAIT | libc::MSG_TRUNC) {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => assert_eq!(result.unwrap(), bytes.len(), "truncated kernel event"),
        }
        let mut offset = 0;
        while offset < bytes.len() {
            assert!(bytes.len() - offset >= 16);
            let length = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = u16::from_ne_bytes(bytes[offset + 4..offset + 6].try_into().unwrap());
            assert!(length >= 16 && offset + length <= bytes.len());
            assert!(!matches!(kind, 2 | 4), "kernel event error/overrun");
            if matches!(kind, 24 | 25) && length >= 28 && bytes[offset + 21] == 203 {
                count += 1;
            }
            offset += (length + 3) & !3;
        }
    }
}

#[tokio::test]
#[ignore = "requires a disposable network namespace"]
async fn kernel_route_reconciliation() {
    assert_ne!(
        std::fs::read_link("/proc/self/ns/net").unwrap(),
        std::fs::read_link("/proc/1/ns/net").unwrap(),
        "refusing the host network namespace"
    );
    ip(&["link", "set", "lo", "up"]);
    for name in ["test-a", "test-b"] {
        ip(&["link", "add", name, "type", "dummy"]);
        ip(&["link", "set", name, "up"]);
        ip(&["addr", "add", "198.51.100.1/24", "dev", name]);
        ip(&["-6", "addr", "add", "fe80::1/64", "dev", name, "nodad"]);
    }
    let mut socket = Socket::new(NETLINK_ROUTE).unwrap();
    socket.bind(&SocketAddr::new(0, 0x40 | 0x400)).unwrap();
    let mut export = tests::automatic_export();
    export.manage_rules = false;
    export.automatic_sources = false;
    let ownership = Arc::new(crate::ownership::ProtocolOwnership::acquire(203).unwrap());
    let exporter = LinuxExporter::new(export.clone(), ownership).unwrap();

    for device_only in [true, false] {
        export.device_only = device_only;
        exporter.update_export(export.clone()).await;
        for (destination, next_hop) in [
            ("192.0.2.0/24", "198.51.100.2"),
            ("192.0.2.0/24", "fe80::2"),
            ("2001:db8::/64", "fe80::2"),
        ] {
            let mut selected = tests::selected(destination, None, 100);
            selected.interface = "test-a".into();
            selected.next_hop = next_hop.parse().unwrap();
            let mut snapshot = RouteSnapshot {
                generation: 1,
                routes: vec![selected.clone()],
                unreachable: vec![],
            };
            exporter.reconcile(snapshot.clone()).await.unwrap();
            assert_eq!(events(&socket), 1);
            for _ in 0..4 {
                exporter.reconcile(snapshot.clone()).await.unwrap();
                assert_eq!(events(&socket), 0, "unchanged unicast was rewritten");
            }

            // A same-priority output change replaces the old identity. Also
            // require unrelated stale cleanup to finish in the same attempt.
            let mut obsolete = selected.clone();
            obsolete.key = RouteKey::new("203.0.113.0/24".parse().unwrap(), None).unwrap();
            obsolete.next_hop = "198.51.100.2".parse().unwrap();
            snapshot.routes.push(obsolete);
            exporter.reconcile(snapshot.clone()).await.unwrap();
            selected.interface = "test-b".into();
            snapshot.routes = vec![selected.clone()];
            exporter.reconcile(snapshot.clone()).await.unwrap();
            let current = exporter.owned_routes(203).await.unwrap();
            assert_eq!(current.len(), 1);
            assert_eq!(
                route_identity(&current[0]).unwrap().output_interface,
                netlink::interface_index("test-b").unwrap()
            );
            events(&socket);
            exporter.reconcile(snapshot.clone()).await.unwrap();
            assert_eq!(events(&socket), 0);

            selected.metric += 1;
            snapshot.routes = vec![selected.clone()];
            exporter.reconcile(snapshot.clone()).await.unwrap();
            let current = exporter.owned_routes(203).await.unwrap();
            assert_eq!(current.len(), 1, "old metric was not removed");
            assert_eq!(
                route_identity(&current[0]).unwrap().priority,
                DYNAMIC_PRIORITY_BASE + 101
            );
            events(&socket);

            snapshot.routes.clear();
            snapshot.unreachable = vec![selected.key];
            exporter.reconcile(snapshot.clone()).await.unwrap();
            events(&socket);
            for _ in 0..4 {
                exporter.reconcile(snapshot.clone()).await.unwrap();
                let current = exporter.owned_routes(203).await.unwrap();
                assert_eq!(current.len(), 1, "unreachable hold disappeared");
                assert!(route_identity(&current[0]).unwrap().unreachable);
                assert_eq!(
                    events(&socket),
                    0,
                    "unchanged unreachable hold was rewritten"
                );
            }
            // Missing state still needs repair; idempotence must not cache it.
            let route = exporter.owned_routes(203).await.unwrap().pop().unwrap();
            exporter.handle.route().del(route).execute().await.unwrap();
            events(&socket);
            exporter.reconcile(snapshot.clone()).await.unwrap();
            assert_eq!(events(&socket), 1);
            snapshot.routes = vec![selected];
            snapshot.unreachable.clear();
            exporter.reconcile(snapshot).await.unwrap();
            assert_eq!(exporter.owned_routes(203).await.unwrap().len(), 1);
            exporter.reconcile(RouteSnapshot::default()).await.unwrap();
            assert!(exporter.owned_routes(203).await.unwrap().is_empty());
            events(&socket);
        }
    }
}
