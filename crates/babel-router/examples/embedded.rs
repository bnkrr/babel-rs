use async_trait::async_trait;
use babel_router::{BabelRouter, RouteExporter, RouteSnapshot, RouterId};

#[derive(Clone, Default)]
struct PrintExporter;

#[async_trait]
impl RouteExporter for PrintExporter {
    async fn reconcile(
        &self,
        snapshot: RouteSnapshot,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("generation {}: {:#?}", snapshot.generation, snapshot.routes);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let router = BabelRouter::builder()
        .router_id(RouterId::new([1, 2, 3, 4, 5, 6, 7, 8]).unwrap())
        .interface("wg0")
        .exporter(PrintExporter)
        .build()
        .await?;
    let handle = router.handle();
    let task = tokio::spawn(router.run());

    tokio::signal::ctrl_c().await?;
    handle.shutdown();
    task.await??;
    Ok(())
}
