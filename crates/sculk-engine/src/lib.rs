//! A worker's game engine: Horizon's dedicated server (`horizon-client --dedicated`) with Sculk's hooks registered
//! first, as Horizon's own executables register theirs (docs/layers.md in Horizon, "How a Horizon crate plugs in").
//! The vanilla game comes from Horizon; Sculk adds where the world is kept and who may change it.

pub mod store;

use horizon_hooks::storage::ChunkStore;
use std::time::Duration;
use store::AnvilStore;

/// How often the engine renews its leases; well under AnvilDB's lease TTL.
const LEASE_RENEW: Duration = Duration::from_secs(3);

struct SculkHooks {
    store: &'static AnvilStore,
}

impl horizon_hooks::Hooks for SculkHooks {
    fn chunk_store(&self) -> Option<&dyn ChunkStore> {
        Some(self.store)
    }
}

/// Registers Sculk's hooks from the environment the worker starts the engine with: `SCULK_WORKER_ID` (from IO),
/// `SCULK_ANVILDB`, `SCULK_IO` and `SCULK_WORLD`. Without `SCULK_WORKER_ID` the engine is plain Horizon, region files and all.
pub fn register() -> anyhow::Result<()> {
    let Ok(worker_id) = std::env::var("SCULK_WORKER_ID") else { return Ok(()) };
    let worker_id: u32 = worker_id.parse()?;
    let url = std::env::var("SCULK_ANVILDB").unwrap_or_else(|_| "http://127.0.0.1:7200".into());
    let world = std::env::var("SCULK_WORLD").unwrap_or_else(|_| "world".into());
    exit_with_worker();
    let io = std::env::var("SCULK_IO").ok();
    let store: &'static AnvilStore = Box::leak(Box::new(AnvilStore::new(&url, io.as_deref(), &world, worker_id)?));
    store.keep_leases(LEASE_RENEW);
    horizon_hooks::register(Box::leak(Box::new(SculkHooks { store })))
        .map_err(|e| anyhow::anyhow!("registering Sculk's hooks: {e}"))?;
    Ok(())
}

/// Ends the engine when the worker that started it dies without stopping it (killed, crashed): IO has freed the
/// worker's leases by then, and an engine left behind would take chunks in the name of a worker that is gone.
fn exit_with_worker() {
    #[cfg(unix)]
    {
        let parent = std::os::unix::process::parent_id();
        std::thread::Builder::new()
            .name("sculk-worker-watch".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    if std::os::unix::process::parent_id() != parent {
                        log::error!("Sculk: the worker is gone; stopping the engine");
                        std::process::exit(3);
                    }
                }
            })
            .expect("starting the worker watch");
    }
}
