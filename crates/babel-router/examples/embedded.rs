use async_trait::async_trait;
use babel_router::SequenceStore;
use babel_router::{BabelRouter, RouteExporter, RouteSnapshot, RouterId};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Example host-owned checkpoint. Use an exclusively owned state path and stable
// domain-unique Router-ID. Abrupt restarts still have a random sequence fallback;
// this deliberately does not pretend to provide crash-safe sequence continuity.
#[derive(Clone)]
struct Checkpoint {
    path: PathBuf,
    id: RouterId,
}

impl Checkpoint {
    async fn write(&self, sequence: Option<u16>) -> std::io::Result<()> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        tokio::fs::create_dir_all(parent).await?;
        let temporary = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        let contents = sequence.map_or_else(
            || format!("{}\n", self.id),
            |seq| format!("{} {seq}\n", self.id),
        );
        let result = async {
            let mut file = tokio::fs::File::create(&temporary).await?;
            file.write_all(contents.as_bytes()).await?;
            file.sync_all().await?;
            tokio::fs::rename(&temporary, &self.path).await?;
            tokio::fs::File::open(parent).await?.sync_all().await
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        result
    }

    async fn consume(&self) -> Result<u16, Box<dyn std::error::Error>> {
        let contents = match tokio::fs::read_to_string(&self.path).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let fields: Vec<_> = contents.split_whitespace().collect();
        if !fields.is_empty() && (fields[0] != self.id.to_string() || fields.len() > 2) {
            return Err("state does not belong to this identity or is malformed".into());
        }
        let sequence = if fields.len() == 2 {
            fields[1].parse::<u16>()?.wrapping_add(1)
        } else {
            let mut random = [0; 2];
            tokio::fs::File::open("/dev/urandom")
                .await?
                .read_exact(&mut random)
                .await?;
            u16::from_ne_bytes(random)
        };
        // Consume the prior orderly checkpoint durably before advertising.
        self.write(None).await?;
        Ok(sequence)
    }
}

#[async_trait]
impl SequenceStore for Checkpoint {
    async fn persist(
        &self,
        sequence_number: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.write(Some(sequence_number)).await?;
        Ok(())
    }
}

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
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err(
            "usage: embedded INTERFACE STATE_FILE ROUTER_ID_HEX (unique stable 16-digit ID)".into(),
        );
    }
    let id = RouterId::new(u64::from_str_radix(&args[3].replace(':', ""), 16)?.to_be_bytes())
        .ok_or("reserved Router-ID")?;
    let store = Checkpoint {
        path: PathBuf::from(&args[2]),
        id,
    };
    let sequence = store.consume().await?;
    let router = BabelRouter::builder()
        .router_id(id)
        .sequence_number(sequence)
        .sequence_store(store)
        .interface(&args[1])
        .exporter(PrintExporter)
        .start()
        .await?;
    let handle = router.handle();
    let wait = router.wait();
    tokio::pin!(wait);
    tokio::select! {
        result = &mut wait => result?,
        result = tokio::signal::ctrl_c() => {
            result?;
            handle.request_shutdown();
            wait.await?;
        }
    }
    Ok(())
}
