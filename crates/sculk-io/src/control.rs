//! The IO side of the IO <-> Worker channel: registration, heartbeats, worker death.

use crate::registry::Registry;
use sculk_proto::anvildb::{
    ChunkPos, GetFileRequest, OwnerRequest, ReleaseWorkerRequest, files_client::FilesClient,
    leases_client::LeasesClient,
};
use sculk_proto::cluster::{
    AllocateEntityIdsRequest, ConflictAck, ConflictReport, Drained, EntityIdBlock, IoMessage, PlayerPackets, Welcome,
    WorkerMessage, io_message, io_server::Io, worker_message,
};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::{Request, Response, Status, Streaming};

pub struct Shared {
    registry: Mutex<Registry>,
    pub heartbeat: Duration,
    anvildb: LeasesClient<Channel>,
    files: FilesClient<Channel>,
    /// The Velocity forwarding secret IO signs logins with; workers hand it to their engines.
    pub forwarding_secret: String,
    /// Players moving between engines, or moved lately: when the move started (`handover`).
    pub(crate) moves: Mutex<std::collections::HashMap<u128, Instant>>,
    /// Pairs of engines merging now (`handover`).
    pub(crate) merging: Mutex<std::collections::HashSet<(u32, u32)>>,
}

impl Shared {
    /// `anvildb` is connected on first use, so IO may start before AnvilDB.
    pub fn new(heartbeat: Duration, anvildb: &str) -> anyhow::Result<Self> {
        let channel = tonic::transport::Endpoint::from_shared(anvildb.to_owned())?.connect_lazy();
        let mut secret = [0u8; 24];
        getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("no randomness for the forwarding secret: {e}"))?;
        Ok(Self {
            registry: Mutex::new(Registry::default()),
            heartbeat,
            anvildb: LeasesClient::new(channel.clone()),
            files: FilesClient::new(channel).max_decoding_message_size(64 << 20),
            forwarding_secret: hex::encode(secret),
            moves: Mutex::new(std::collections::HashMap::new()),
            merging: Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// The worker whose micro world holds `chunk`, if any (docs/docs/05-io-server.md, "Розподіл").
    pub async fn chunk_owner(&self, chunk: ChunkPos) -> anyhow::Result<Option<u32>> {
        let lease = self.anvildb.clone().owner(OwnerRequest { chunk: Some(chunk) }).await?.into_inner().lease;
        Ok(lease.and_then(|l| l.holder).map(|h| h.worker_id))
    }

    /// Where a player was when it last left, from its saved file in AnvilDB: dimension and chunk. `None` for a
    /// player who never played here, or a file IO cannot read.
    pub async fn saved_chunk(&self, world: &str, uuid: u128) -> Option<(String, i32, i32)> {
        let path = format!("players/data/{}.dat", horizon_protocol::crypto::uuid_string(uuid));
        let request = GetFileRequest { world: world.to_owned(), path };
        let data = self.files.clone().get(request).await.ok()?.into_inner().data?;
        let nbt = horizon_world::compression::Compression::Gzip.decode(&data, 16 << 20).ok()?;
        let (root, _) = horizon_world::nbt::read_named(&nbt, horizon_world::nbt::Limits::default()).ok()?;
        let Some(horizon_world::nbt::Tag::List(pos)) = root.tag.get("Pos") else { return None };
        let coordinate = |i: usize| match pos.get(i) {
            Some(horizon_world::nbt::Tag::Double(bits)) => Some(f64::from_bits(*bits)),
            _ => None,
        };
        let (x, z) = (coordinate(0)?, coordinate(2)?);
        let dimension = match root.tag.get("Dimension") {
            Some(horizon_world::nbt::Tag::String(d)) => d.to_string_lossy(),
            _ => "minecraft:overworld".to_owned(),
        };
        Some((dimension, (x.floor() as i32) >> 4, (z.floor() as i32) >> 4))
    }

    pub fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn release_leases(&self, worker_id: u32) -> anyhow::Result<u32> {
        let r = self.anvildb.clone().release_worker(ReleaseWorkerRequest { worker_id }).await?;
        Ok(r.into_inner().released)
    }

    /// A worker's stream ended or it went silent: forget it and free its leases in AnvilDB.
    pub async fn worker_gone(&self, worker_id: u32, why: &str) {
        let Some(lost) = self.registry().remove(worker_id) else { return };
        tracing::warn!(worker_id, sessions = lost.len(), why, "worker gone");
        match self.release_leases(worker_id).await {
            Ok(released) => tracing::info!(worker_id, released, "leases of the dead worker released"),
            // The leases still expire by TTL.
            Err(e) => tracing::error!(worker_id, error = %e, "could not release the dead worker's leases"),
        }
    }

    /// Hands a worker's play packets to the player's connection. A player whose connection can't keep up is
    /// dropped: skipping packets would leave its client in a state the worker doesn't know.
    fn deliver(&self, p: PlayerPackets) {
        let Some(player) = self.registry().player(p.session_id) else { return };
        match player.try_send(p.packets) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(session_id = p.session_id, "player too slow for its packets; disconnecting");
                // Dropping the channel ends the connection, which closes the session.
                self.registry().drop_player(p.session_id);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// Drops workers that missed three heartbeats.
    pub async fn watchdog(self: Arc<Self>) {
        let mut every = tokio::time::interval(self.heartbeat);
        loop {
            every.tick().await;
            let Some(deadline) = Instant::now().checked_sub(self.heartbeat * 3) else { continue };
            let silent = self.registry().silent_since(deadline);
            for worker_id in silent {
                self.worker_gone(worker_id, "missed heartbeats").await;
            }
        }
    }
}

/// Where IO reaches a worker's engine: its address as the worker gave it, with an unspecified IP (`0.0.0.0`, an
/// engine listening on every interface of a container) replaced by the IP the worker connected from.
fn engine_address(given: &str, peer: Option<std::net::SocketAddr>) -> Option<String> {
    if given.is_empty() {
        return None;
    }
    match (given.parse::<std::net::SocketAddr>(), peer) {
        (Ok(addr), Some(peer)) if addr.ip().is_unspecified() => {
            Some(std::net::SocketAddr::new(peer.ip(), addr.port()).to_string())
        }
        _ => Some(given.to_owned()),
    }
}

pub struct ControlService {
    shared: Arc<Shared>,
}

impl ControlService {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }
}

#[tonic::async_trait]
impl Io for ControlService {
    type RegisterStream = ReceiverStream<Result<IoMessage, Status>>;

    async fn register(&self, req: Request<Streaming<WorkerMessage>>) -> Result<Response<Self::RegisterStream>, Status> {
        let peer = req.remote_addr();
        let mut inbound = req.into_inner();
        let hello = match inbound.message().await?.and_then(|m| m.msg) {
            Some(worker_message::Msg::Hello(h)) => h,
            _ => return Err(Status::invalid_argument("the first message must be Hello")),
        };
        if hello.version != sculk_proto::VERSION {
            return Err(Status::failed_precondition(format!(
                "worker runs sculk {}, IO runs {}: run the same build everywhere",
                hello.version,
                sculk_proto::VERSION
            )));
        }

        let (tx, rx) = mpsc::channel(256);
        let (worker_id, (start, len)) = {
            let mut registry = self.shared.registry();
            let ids = registry.allocate_entity_ids().ok_or_else(|| Status::resource_exhausted("no entity IDs left"))?;
            let worker_id = registry
                .register(tx.clone(), engine_address(&hello.engine_addr, peer), Instant::now())
                .ok_or_else(|| Status::resource_exhausted("all 65535 worker IDs are in use"))?;
            // Players' packets for the worker go in batches.
            let packets = sculk_proto::batcher(tx.clone(), |batch| {
                Ok(IoMessage { msg: Some(io_message::Msg::PlayerBatch(batch)) })
            });
            registry.set_packets(worker_id, packets);
            (worker_id, ids)
        };
        // A worker of an earlier IO run may have held this ID; its leases must not pass to the new one.
        match self.shared.release_leases(worker_id).await {
            Ok(0) => {}
            Ok(released) => tracing::info!(worker_id, released, "stale leases of this worker ID released"),
            Err(e) => {
                self.shared.registry().remove(worker_id);
                return Err(Status::unavailable(format!("AnvilDB: {e}")));
            }
        }
        let welcome = Welcome {
            worker_id,
            heartbeat_interval_ms: self.shared.heartbeat.as_millis() as u32,
            entity_ids: Some(EntityIdBlock { start, len }),
            forwarding_secret: self.shared.forwarding_secret.clone(),
        };
        tx.send(Ok(IoMessage { msg: Some(io_message::Msg::Welcome(welcome)) }))
            .await
            .map_err(|_| Status::cancelled("worker left"))?;
        drop(tx);
        tracing::info!(worker_id, ?peer, "worker registered");

        let shared = self.shared.clone();
        tokio::spawn(async move {
            let why = loop {
                let message = inbound.message().await;
                // Whatever the worker sends shows it alive: a heartbeat can queue behind a busy worker's packets.
                if matches!(message, Ok(Some(_))) {
                    shared.registry().seen(worker_id, Instant::now());
                }
                match message {
                    Ok(Some(WorkerMessage { msg: Some(msg) })) => match msg {
                        worker_message::Msg::Heartbeat(hb) => {
                            shared.registry().heartbeat(worker_id, hb, Instant::now());
                        }
                        worker_message::Msg::SessionReady(ready) => shared.registry().session_ready(ready),
                        worker_message::Msg::PlayerPackets(p) => shared.deliver(p),
                        worker_message::Msg::PlayerBatch(batch) => {
                            for p in batch.packets {
                                shared.deliver(p);
                            }
                        }
                        worker_message::Msg::PlayerPrepared(p) => shared.registry().player_prepared(p),
                        worker_message::Msg::EngineReply(r) => shared.registry().engine_reply(r),
                        worker_message::Msg::Draining(_) => {
                            let shared = shared.clone();
                            tokio::spawn(async move {
                                let (moved, left) = shared.drain(worker_id).await;
                                tracing::info!(worker_id, moved, left, "worker drained");
                                let worker = shared.registry().worker(worker_id);
                                if let Some(worker) = worker {
                                    let msg =
                                        IoMessage { msg: Some(io_message::Msg::Drained(Drained { moved, left })) };
                                    let _ = worker.send(Ok(msg)).await;
                                }
                            });
                        }
                        worker_message::Msg::Hello(_) => tracing::warn!(worker_id, "Hello sent twice, ignored"),
                    },
                    Ok(Some(WorkerMessage { msg: None })) => {}
                    Ok(None) => break "stream closed",
                    Err(e) => {
                        tracing::warn!(worker_id, error = %e, "worker stream failed");
                        break "stream failed";
                    }
                }
            };
            shared.worker_gone(worker_id, why).await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn report_conflict(&self, req: Request<ConflictReport>) -> Result<Response<ConflictAck>, Status> {
        let wait = self.shared.clone().on_conflict(req.into_inner()).await;
        Ok(Response::new(ConflictAck { wait }))
    }

    async fn allocate_entity_ids(
        &self,
        _req: Request<AllocateEntityIdsRequest>,
    ) -> Result<Response<EntityIdBlock>, Status> {
        let (start, len) = self
            .shared
            .registry()
            .allocate_entity_ids()
            .ok_or_else(|| Status::resource_exhausted("no entity IDs left"))?;
        Ok(Response::new(EntityIdBlock { start, len }))
    }
}

#[cfg(test)]
mod tests {
    use super::engine_address;

    #[test]
    fn an_engine_on_every_interface_is_reached_at_the_workers_ip() {
        let peer = Some("172.18.0.5:41000".parse().unwrap());
        assert_eq!(engine_address("0.0.0.0:25601", peer).as_deref(), Some("172.18.0.5:25601"));
        assert_eq!(engine_address("127.0.0.1:25601", peer).as_deref(), Some("127.0.0.1:25601"));
        assert_eq!(engine_address("engine-1:25601", peer).as_deref(), Some("engine-1:25601"));
        assert_eq!(engine_address("", peer), None);
    }
}
