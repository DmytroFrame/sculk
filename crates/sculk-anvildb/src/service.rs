//! The `Leases` gRPC service over [`LeaseTable`].

use crate::files::FileStore;
use crate::leases::{ChunkKey, Holder, Lease, LeaseTable};
use crate::storage::{RecordKey, RegionStore};
use sculk_proto::anvildb::{self as pb, chunks_server::Chunks, files_server::Files, leases_server::Leases};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tonic::{Request, Response, Status};

#[derive(Clone)]
pub struct LeaseService {
    table: Arc<Mutex<LeaseTable>>,
}

impl LeaseService {
    pub fn new(table: Arc<Mutex<LeaseTable>>) -> Self {
        Self { table }
    }

    fn table(&self) -> std::sync::MutexGuard<'_, LeaseTable> {
        // A panic while holding the lock leaves the table consistent: every method finishes its writes first.
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn holder(h: Option<pb::LeaseHolder>) -> Result<Holder, Status> {
    let h = h.ok_or_else(|| Status::invalid_argument("holder is required"))?;
    let worker_id = u16::try_from(h.worker_id).map_err(|_| Status::invalid_argument("worker_id is 16-bit"))?;
    Ok(Holder { mw_id: h.mw_id, worker_id: worker_id.into() })
}

fn chunk(c: pb::ChunkPos) -> ChunkKey {
    ChunkKey { world: c.world, dimension: c.dimension, x: c.x, z: c.z }
}

fn chunk_pb(c: &ChunkKey) -> pb::ChunkPos {
    pb::ChunkPos { world: c.world.clone(), dimension: c.dimension.clone(), x: c.x, z: c.z }
}

fn holder_pb(h: Holder) -> pb::LeaseHolder {
    pb::LeaseHolder { mw_id: h.mw_id, worker_id: h.worker_id }
}

fn lease_pb(l: &Lease, now: Instant) -> pb::Lease {
    pb::Lease {
        chunk: Some(chunk_pb(&l.chunk)),
        holder: Some(holder_pb(l.holder)),
        generation: l.generation,
        expires_in_ms: l.expires_at.saturating_duration_since(now).as_millis() as u64,
    }
}

#[tonic::async_trait]
impl Leases for LeaseService {
    async fn acquire(&self, req: Request<pb::AcquireRequest>) -> Result<Response<pb::AcquireResponse>, Status> {
        let req = req.into_inner();
        let h = holder(req.holder)?;
        let chunks: Vec<ChunkKey> = req.chunks.into_iter().map(chunk).collect();
        let now = Instant::now();
        let result = match self.table().acquire(h, &chunks, now) {
            Ok(leases) => pb::acquire_response::Result::Granted(pb::Granted {
                leases: leases.iter().map(|l| lease_pb(l, now)).collect(),
            }),
            Err(c) => pb::acquire_response::Result::Conflict(pb::Conflict {
                chunk: Some(chunk_pb(&c.chunk)),
                owner: Some(holder_pb(c.owner)),
            }),
        };
        Ok(Response::new(pb::AcquireResponse { result: Some(result) }))
    }

    async fn renew(&self, req: Request<pb::RenewRequest>) -> Result<Response<pb::RenewResponse>, Status> {
        let h = holder(req.into_inner().holder)?;
        let renewed = self.table().renew(h, Instant::now());
        Ok(Response::new(pb::RenewResponse { renewed }))
    }

    async fn release(&self, req: Request<pb::ReleaseRequest>) -> Result<Response<pb::ReleaseResponse>, Status> {
        let req = req.into_inner();
        let h = holder(req.holder)?;
        let chunks: Vec<ChunkKey> = req.chunks.into_iter().map(chunk).collect();
        let released = self.table().release(h, &chunks);
        Ok(Response::new(pb::ReleaseResponse { released }))
    }

    async fn transfer(&self, req: Request<pb::TransferRequest>) -> Result<Response<pb::TransferResponse>, Status> {
        let req = req.into_inner();
        let (from, to) = (holder(req.from)?, holder(req.to)?);
        let now = Instant::now();
        let moved = self.table().transfer(from, to, now);
        Ok(Response::new(pb::TransferResponse { leases: moved.iter().map(|l| lease_pb(l, now)).collect() }))
    }

    async fn owner(&self, req: Request<pb::OwnerRequest>) -> Result<Response<pb::OwnerResponse>, Status> {
        let c = req.into_inner().chunk.ok_or_else(|| Status::invalid_argument("chunk is required"))?;
        let now = Instant::now();
        let lease = self.table().owner(&chunk(c), now).map(|l| lease_pb(&l, now));
        Ok(Response::new(pb::OwnerResponse { lease }))
    }

    async fn list(&self, req: Request<pb::ListLeasesRequest>) -> Result<Response<pb::ListLeasesResponse>, Status> {
        let world = req.into_inner().world;
        let now = Instant::now();
        let leases = self.table().list(&world, now).iter().map(|l| lease_pb(l, now)).collect();
        Ok(Response::new(pb::ListLeasesResponse { leases }))
    }

    async fn release_worker(
        &self,
        req: Request<pb::ReleaseWorkerRequest>,
    ) -> Result<Response<pb::ReleaseWorkerResponse>, Status> {
        let worker_id = req.into_inner().worker_id;
        let released = self.table().release_worker(worker_id);
        if released > 0 {
            tracing::info!(worker_id, released, "released every lease of a worker");
        }
        Ok(Response::new(pb::ReleaseWorkerResponse { released }))
    }
}

/// The `Chunks` gRPC service over [`RegionStore`], fenced by the lease table.
#[derive(Clone)]
pub struct ChunkService {
    store: Arc<RegionStore>,
    table: Arc<Mutex<LeaseTable>>,
}

impl ChunkService {
    pub fn new(store: Arc<RegionStore>, table: Arc<Mutex<LeaseTable>>) -> Self {
        Self { store, table }
    }
}

fn record_key(k: Option<pb::ChunkKey>) -> Result<RecordKey, Status> {
    let k = k.ok_or_else(|| Status::invalid_argument("key is required"))?;
    Ok(RecordKey { world: k.world, dimension: k.dimension, folder: k.folder, x: k.x, z: k.z })
}

fn internal(e: anyhow::Error) -> Status {
    Status::internal(format!("{e:#}"))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, Status> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Status::internal(e.to_string()))?.map_err(internal)
}

#[tonic::async_trait]
impl Chunks for ChunkService {
    async fn read(&self, req: Request<pb::ReadChunkRequest>) -> Result<Response<pb::ReadChunkResponse>, Status> {
        let key = record_key(req.into_inner().key)?;
        let store = self.store.clone();
        let nbt = blocking(move || store.read(&key)).await?;
        Ok(Response::new(pb::ReadChunkResponse { nbt }))
    }

    async fn write(&self, req: Request<pb::WriteChunksRequest>) -> Result<Response<pb::WriteChunksResponse>, Status> {
        let req = req.into_inner();
        let records =
            req.writes.into_iter().map(|w| Ok((record_key(w.key)?, w.nbt))).collect::<Result<Vec<_>, Status>>()?;
        {
            let table = self.table();
            let now = Instant::now();
            for (k, _) in &records {
                let chunk = ChunkKey { world: k.world.clone(), dimension: k.dimension.clone(), x: k.x, z: k.z };
                if !table.held_by_worker(&chunk, req.worker_id, now) {
                    return Err(Status::failed_precondition(format!(
                        "worker #{} holds no lease on {} {} {},{}",
                        req.worker_id, k.world, k.dimension, k.x, k.z
                    )));
                }
            }
        }
        let store = self.store.clone();
        blocking(move || store.write(&records)).await?;
        Ok(Response::new(pb::WriteChunksResponse {}))
    }

    async fn saved_in_region(
        &self,
        req: Request<pb::SavedInRegionRequest>,
    ) -> Result<Response<pb::SavedInRegionResponse>, Status> {
        let r = req.into_inner();
        let store = self.store.clone();
        let bits = blocking(move || store.saved_in_region(&r.world, &r.dimension, &r.folder, r.rx, r.rz)).await?;
        Ok(Response::new(pb::SavedInRegionResponse { bits: bits.to_vec() }))
    }
}

impl ChunkService {
    fn table(&self) -> std::sync::MutexGuard<'_, LeaseTable> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The `Files` gRPC service over [`FileStore`]: a world's files besides its chunks.
#[derive(Clone)]
pub struct FileService {
    store: Arc<FileStore>,
}

impl FileService {
    pub fn new(store: Arc<FileStore>) -> Self {
        Self { store }
    }
}

#[tonic::async_trait]
impl Files for FileService {
    async fn get(&self, req: Request<pb::GetFileRequest>) -> Result<Response<pb::GetFileResponse>, Status> {
        let r = req.into_inner();
        let store = self.store.clone();
        let data = blocking(move || store.get(&r.world, &r.path)).await?;
        Ok(Response::new(pb::GetFileResponse { data }))
    }

    async fn put(&self, req: Request<pb::PutFilesRequest>) -> Result<Response<pb::PutFilesResponse>, Status> {
        let r = req.into_inner();
        let files: Vec<(String, Vec<u8>)> = r.files.into_iter().map(|f| (f.path, f.data)).collect();
        tracing::debug!(worker_id = r.worker_id, files = files.len(), "world files saved");
        let store = self.store.clone();
        blocking(move || store.put(&r.world, &files)).await?;
        Ok(Response::new(pb::PutFilesResponse {}))
    }

    async fn list(&self, req: Request<pb::ListFilesRequest>) -> Result<Response<pb::ListFilesResponse>, Status> {
        let r = req.into_inner();
        let store = self.store.clone();
        let files = blocking(move || store.list(&r.world, &r.prefix)).await?;
        let files = files.into_iter().map(|(path, hash)| pb::FileInfo { path, hash }).collect();
        Ok(Response::new(pb::ListFilesResponse { files }))
    }
}
