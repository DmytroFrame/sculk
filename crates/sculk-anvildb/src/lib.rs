//! AnvilDB: the database of Minecraft worlds and chunk leases (docs/docs/07-anvildb.md).
//!
//! It holds the lease table in memory, chunk records in vanilla region files (`storage`) and the rest of a world
//! (level.dat, players, saved data) as vanilla's files beside them (`files`).

pub mod files;
pub mod leases;
pub mod s3;
mod service;
pub mod storage;

use anyhow::Context;
use leases::LeaseTable;
use sculk_proto::anvildb::{chunks_server::ChunksServer, files_server::FilesServer, leases_server::LeasesServer};
use service::{ChunkService, FileService, LeaseService};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The largest gRPC message: a batch of chunk records.
pub const MAX_MESSAGE: usize = 256 << 20;

#[derive(Debug, Clone, clap::Args)]
pub struct Config {
    /// gRPC address for workers, IO and AnvilDB Connector.
    #[arg(long = "anvildb-listen", env = "SCULK_ANVILDB_LISTEN", default_value = "127.0.0.1:7200")]
    pub listen: SocketAddr,
    /// How long a lease lives without a renew.
    #[arg(long, env = "SCULK_LEASE_TTL_MS", default_value_t = 10_000)]
    pub lease_ttl_ms: u64,
    /// Where worlds are kept, in vanilla's folder layout (a world folder copies in and out as it is).
    #[arg(long, env = "SCULK_ANVILDB_DATA", default_value = "anvildb-data")]
    pub data_dir: std::path::PathBuf,
    /// Region files no write touched for this many days are gzipped under `<data>/archive/` and come back when
    /// read again; 0 keeps every region as it is.
    #[arg(long, env = "SCULK_ARCHIVE_AFTER_DAYS", default_value_t = 30)]
    pub archive_after_days: u64,
    /// How chunk records are compressed, as vanilla's `region-file-compression`: `deflate` (vanilla's default),
    /// `lz4` (several times faster, a little larger) or `none`. Vanilla reads all three, and a world may mix them.
    #[arg(long, env = "SCULK_REGION_COMPRESSION", default_value = "lz4", value_parser = ["deflate", "lz4", "none"])]
    pub region_compression: String,
    /// Archive cold regions to this S3 bucket (`https://s3.<region>.amazonaws.com/<bucket>`, or any S3 API's
    /// `http://host:port/<bucket>`) instead of `<data>/archive/`; credentials from SCULK_S3_ACCESS_KEY and
    /// SCULK_S3_SECRET_KEY.
    #[arg(long, env = "SCULK_ARCHIVE_S3")]
    pub archive_s3: Option<String>,
    #[arg(long, env = "SCULK_S3_REGION", default_value = "us-east-1")]
    pub s3_region: String,
}

/// Where the lease table is saved between restarts.
fn leases_file(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("leases.json")
}

/// The lease table as saved before the last stop, or an empty one.
fn load_leases(data_dir: &std::path::Path, ttl: Duration) -> LeaseTable {
    let path = leases_file(data_dir);
    match std::fs::read(&path).map(|bytes| serde_json::from_slice::<leases::Snapshot>(&bytes)) {
        Ok(Ok(snapshot)) => {
            let count = snapshot.leases.len();
            tracing::info!(leases = count, "lease table restored");
            LeaseTable::restore(ttl, snapshot, Instant::now())
        }
        Ok(Err(e)) => {
            tracing::warn!(path = %path.display(), error = %e, "the saved lease table is unreadable; starting empty");
            LeaseTable::new(ttl)
        }
        Err(_) => LeaseTable::new(ttl),
    }
}

/// Saves the lease table about once a second when it changed: written beside, then renamed over the old one.
async fn save_leases(table: Arc<Mutex<LeaseTable>>, data_dir: std::path::PathBuf) {
    let path = leases_file(&data_dir);
    let temp = data_dir.join(".leases.json.tmp");
    let mut saved = u64::MAX;
    let mut every = tokio::time::interval(Duration::from_secs(1));
    loop {
        every.tick().await;
        let snapshot = {
            let table = table.lock().unwrap_or_else(|e| e.into_inner());
            if table.changes() == saved {
                continue;
            }
            saved = table.changes();
            table.snapshot(Instant::now())
        };
        let (path, temp) = (path.clone(), temp.clone());
        let written = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&temp, serde_json::to_vec(&snapshot)?)?;
            std::fs::rename(&temp, &path)
        })
        .await;
        if !matches!(written, Ok(Ok(()))) {
            tracing::warn!(?written, "the lease table was not saved");
            saved = u64::MAX;
        }
    }
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let ttl = Duration::from_millis(config.lease_ttl_ms);
    let table = Arc::new(Mutex::new(load_leases(&config.data_dir, ttl)));
    tokio::spawn(save_leases(table.clone(), config.data_dir.clone()));

    let sweeper = table.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_secs(1));
        loop {
            every.tick().await;
            let swept = sweeper.lock().unwrap_or_else(|e| e.into_inner()).sweep(Instant::now());
            if swept > 0 {
                tracing::debug!(swept, "expired leases removed");
            }
        }
    });

    let compression = match config.region_compression.as_str() {
        "lz4" => horizon_world::compression::Compression::Lz4,
        "none" => horizon_world::compression::Compression::None,
        _ => horizon_world::compression::Compression::Zlib,
    };
    let mut regions = storage::RegionStore::with_compression(&config.data_dir, compression);
    // Empty (as compose passes an unset variable) is no bucket.
    if let Some(url) = config.archive_s3.as_deref().filter(|u| !u.is_empty()) {
        let (endpoint, bucket) =
            url.trim_end_matches('/').rsplit_once('/').context("SCULK_ARCHIVE_S3 is <endpoint>/<bucket>")?;
        regions = regions.with_s3(s3::S3 {
            endpoint: endpoint.to_owned(),
            bucket: bucket.to_owned(),
            region: config.s3_region.clone(),
            access_key: std::env::var("SCULK_S3_ACCESS_KEY").context("SCULK_S3_ACCESS_KEY")?,
            secret_key: std::env::var("SCULK_S3_SECRET_KEY").context("SCULK_S3_SECRET_KEY")?,
        });
    }
    let regions = Arc::new(regions);
    if config.archive_after_days > 0 {
        let (regions, cold) = (regions.clone(), Duration::from_secs(config.archive_after_days * 86_400));
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(3600));
            loop {
                every.tick().await;
                let regions = regions.clone();
                match tokio::task::spawn_blocking(move || regions.archive_cold(cold)).await {
                    Ok(Ok(0)) => {}
                    Ok(Ok(archived)) => tracing::info!(archived, "cold regions archived"),
                    Ok(Err(e)) => tracing::warn!(error = format!("{e:#}"), "archiving cold regions failed"),
                    Err(e) => tracing::warn!(error = %e, "archiving cold regions failed"),
                }
            }
        });
    }
    tracing::info!(listen = %config.listen, ttl_ms = config.lease_ttl_ms, "AnvilDB listening");
    tonic::transport::Server::builder()
        .add_service(LeasesServer::new(LeaseService::new(table.clone())))
        .add_service(
            ChunksServer::new(ChunkService::new(regions, table))
                // A batch of chunk records passes tonic's default 4 MiB.
                .max_decoding_message_size(MAX_MESSAGE)
                .max_encoding_message_size(MAX_MESSAGE),
        )
        .add_service(
            FilesServer::new(FileService::new(Arc::new(files::FileStore::new(&config.data_dir))))
                .max_decoding_message_size(MAX_MESSAGE)
                .max_encoding_message_size(MAX_MESSAGE),
        )
        .serve(config.listen)
        .await
        .context("AnvilDB gRPC server")
}
