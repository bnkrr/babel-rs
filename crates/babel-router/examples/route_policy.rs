//! Interactive policy replacement using the memory RIB (does not install kernel routes).
use std::sync::Arc;

use babel_router::{
    BabelRouter, ExportContext, ImportContext, InterfacePolicy, RouteKey, RoutePolicy, RouterId,
    WiredMetric,
};
use tokio::io::{AsyncBufReadExt, BufReader};

struct Rules {
    import: bool,
    export: bool,
}

impl RoutePolicy for Rules {
    fn accept(&self, _: &ImportContext<'_>) -> bool {
        self.import
    }
    fn announce(&self, _: &ExportContext<'_>) -> bool {
        self.export
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: route_policy INTERFACE ROUTER_ID_HEX ORIGIN_PREFIX; stdin: allow, deny-import, deny-export, status, quit".into());
    }
    let id = RouterId::new(u64::from_str_radix(&args[2], 16)?.to_be_bytes())
        .ok_or("reserved Router-ID")?;
    let origin = RouteKey::new(args[3].parse()?, None).ok_or("invalid origin")?;
    let router = BabelRouter::builder()
        .router_id(id)
        .interface_with_policy(
            &args[1],
            InterfacePolicy {
                control_transport: Default::default(),
                ipv4_next_hop: Default::default(),
                metric: Arc::new(WiredMetric::default()),
                hello_interval_cs: 100,
                update_interval_cs: 400,
                split_horizon: true,
            },
        )
        .originate(origin, 0)
        .route_policy(Arc::new(Rules {
            import: true,
            export: true,
        }))
        .start()
        .await?;
    let handle = router.handle();
    let routes = handle.subscribe_routes();
    println!("ready");
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    while let Some(command) = input.next_line().await? {
        let rules = match command.trim() {
            "allow" => Rules {
                import: true,
                export: true,
            },
            "deny-import" => Rules {
                import: false,
                export: true,
            },
            "deny-export" => Rules {
                import: true,
                export: false,
            },
            "status" => {
                let status = handle.status().await?;
                let prefixes = routes
                    .borrow()
                    .routes
                    .iter()
                    .map(|r| r.key.destination.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                println!("neighbors={} routes={prefixes}", status.neighbours);
                continue;
            }
            "quit" => break,
            _ => {
                println!("commands: allow, deny-import, deny-export, status, quit");
                continue;
            }
        };
        // Each command supplies a new immutable version. No callback reads
        // shared mutable configuration behind the engine's back.
        handle.replace_route_policy(Arc::new(rules)).await?;
        println!("ok");
    }
    router.shutdown().await?;
    Ok(())
}
