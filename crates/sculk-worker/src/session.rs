//! One life of a worker on IO: from Hello to the end of the Register stream. A worker that loses IO loses its micro
//! worlds, since IO has already freed their leases.

use crate::Config;
use crate::control::EngineControl;
use crate::flat::FlatChunk;
use crate::mw::{self, Chunk, MicroWorld, Player};
use crate::play::{PlayState, World};
use crate::worldfiles::WorldFiles;
use anyhow::{Context, bail};
use sculk_data::{GameData, SPAWN};
use sculk_proto::anvildb::{
    AcquireRequest, LeaseHolder, ReleaseRequest, RenewRequest, acquire_response, leases_client::LeasesClient,
};
use sculk_proto::cluster::{
    AllocateEntityIdsRequest, AssignSession, CloseSession, Draining, EngineReply, Heartbeat, Hello, PlayerPackets,
    PlayerPrepared, SessionReady, WorkerMessage, io_client::IoClient, io_message, worker_message,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

/// Vanilla's tick: 20 per second.
const TICK: Duration = Duration::from_millis(50);
const DIMENSION: &str = "minecraft:overworld";

struct State {
    worker_id: u32,
    next_mw: u64,
    /// The entity IDs IO gave this worker: [next, end).
    next_entity_id: i32,
    entity_id_end: i32,
    mws: HashMap<u64, MicroWorld>,
    /// Session -> the micro world the player lives in.
    sessions: HashMap<u64, u64>,
    /// Session -> its connection's compression threshold.
    compression: HashMap<u64, Option<usize>>,
    mspt: f32,
}

#[derive(Clone)]
struct Ctx {
    config: Arc<Config>,
    world: Arc<World>,
    state: Arc<Mutex<State>>,
    leases: LeasesClient<Channel>,
    /// IO's control service, for blocks of entity IDs.
    io: IoClient<Channel>,
    out: mpsc::Sender<WorkerMessage>,
    /// Play packets for IO, sent in batches.
    player_packets: mpsc::Sender<PlayerPackets>,
}

impl Ctx {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn holder(&self, mw_id: u64) -> LeaseHolder {
        LeaseHolder { mw_id, worker_id: self.state().worker_id }
    }

    async fn send(&self, msg: worker_message::Msg) {
        let _ = self.out.send(WorkerMessage { msg: Some(msg) }).await;
    }

    /// Sends packet bodies to a player as frame contents, compressed here when its connection compresses
    /// (docs/docs/05-io-server.md: the worker compresses, IO only frames).
    async fn send_packets(&self, session_id: u64, bodies: Vec<Vec<u8>>) {
        if bodies.is_empty() {
            return;
        }
        let Some(&threshold) = self.state().compression.get(&session_id) else { return };
        let packets = bodies
            .iter()
            .filter_map(|body| match horizon_protocol::frame::contents(body, threshold) {
                Ok(contents) => Some(contents),
                Err(e) => {
                    tracing::error!(session_id, error = %e.0, "packet could not be framed");
                    None
                }
            })
            .collect();
        let _ = self.player_packets.send(PlayerPackets { session_id, packets }).await;
    }
}

/// Aborts a background task when the session ends, however it ends.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The world every player of this worker sees, built from the game data.
pub fn world(data: &GameData) -> anyhow::Result<World> {
    let biomes = data.registry("minecraft:worldgen/biome").context("no biome registry")?;
    let plains = biomes.id("minecraft:plains").context("no minecraft:plains biome")?;
    let overworld_type = data.id("minecraft:dimension_type", DIMENSION).context("no overworld dimension type")?;
    if !data.has_reports {
        tracing::warn!(
            "no block report in the game data: block IDs are guessed and block/item tags are empty (see `sculk prepare --reports`)"
        );
    }
    Ok(World { flat: FlatChunk::new(&data.blocks, plains, biomes.entries.len()), overworld_type })
}

/// Why a session ended without an error.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// IO closed the stream: connect again.
    Lost,
    /// The worker was told to stop (a signal, or `stop` on the console).
    Stopped,
}

/// How long a stopping worker waits for IO to move its players to other engines.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the engine's saved world files go to AnvilDB.
const FILE_PUSH: Duration = Duration::from_secs(1);

pub async fn run(
    config: Arc<Config>,
    world: Arc<World>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
    console: &mut mpsc::Receiver<String>,
) -> anyhow::Result<Ended> {
    let leases = LeasesClient::connect(config.anvildb.clone()).await.context("connecting to AnvilDB")?;
    let mut io = IoClient::connect(config.io.clone()).await.context("connecting to IO")?;

    let (out, out_rx) = mpsc::channel(4096);
    let engine = config.engine();
    let hello = Hello {
        version: sculk_proto::VERSION.into(),
        engine_addr: engine.as_ref().map(|e| e.address()).unwrap_or_default(),
    };
    out.send(WorkerMessage { msg: Some(worker_message::Msg::Hello(hello)) }).await?;
    let mut inbound = io.register(ReceiverStream::new(out_rx)).await.context("opening the IO stream")?.into_inner();

    let welcome = match inbound.message().await?.and_then(|m| m.msg) {
        Some(io_message::Msg::Welcome(w)) => w,
        _ => bail!("IO did not answer Hello with Welcome"),
    };
    let ids = welcome.entity_ids.unwrap_or_default();
    tracing::info!(
        worker_id = welcome.worker_id,
        entity_ids = %format!("{}..{}", ids.start, ids.start + ids.len as i32),
        "registered on IO"
    );

    let state = State {
        worker_id: welcome.worker_id,
        next_mw: 0,
        next_entity_id: ids.start,
        entity_id_end: ids.start + ids.len as i32,
        mws: HashMap::new(),
        sessions: HashMap::new(),
        compression: HashMap::new(),
        mspt: 0.0,
    };
    // The engine's folder is ready once IO has given this worker its ID and the forwarding secret. The engine itself
    // starts when IO first sends a player here (or the console is used): a standby worker runs none, so it holds no
    // chunks the engine in use needs. It stops (and saves) with the session.
    let mut files = None;
    if let Some(e) = &engine {
        crate::engine::prepare_dir(e, &welcome.forwarding_secret)?;
        let dir = crate::engine::world_dir(e);
        let wf = WorldFiles::new(&config.anvildb, &config.world, e.dir.clone(), dir, welcome.worker_id)?;
        files = Some(Arc::new(tokio::sync::Mutex::new(wf)));
    }
    let mut engine = None;
    // While the engine saves, IO must still count this worker alive: it frees a dead worker's leases, and AnvilDB
    // would refuse the engine's last saves.
    let alive = out.clone();
    let heartbeat = Duration::from_millis(welcome.heartbeat_interval_ms.max(100).into());
    let result =
        serve(&config, world, state, (leases, io), out, &welcome, inbound, &mut engine, files.clone(), stop, console)
            .await;
    let _still_alive = Task(tokio::spawn(async move {
        let mut every = tokio::time::interval(heartbeat);
        loop {
            every.tick().await;
            let hb = WorkerMessage { msg: Some(worker_message::Msg::Heartbeat(Heartbeat::default())) };
            if alive.send(hb).await.is_err() {
                break;
            }
        }
    }));
    if let Some(engine) = engine {
        engine.stop().await;
    }
    if let Some(files) = files {
        match files.lock().await.push().await {
            Ok(pushed) => tracing::info!(pushed, "world files saved in AnvilDB"),
            Err(e) => tracing::error!(error = format!("{e:#}"), "the engine's last world files were not saved"),
        }
    }
    result
}

/// The session's work until IO goes, the engine stops or the worker is told to stop.
#[allow(clippy::too_many_arguments)]
async fn serve(
    config: &Arc<Config>,
    world: Arc<World>,
    state: State,
    (leases, io): (LeasesClient<Channel>, IoClient<Channel>),
    out: mpsc::Sender<WorkerMessage>,
    welcome: &sculk_proto::cluster::Welcome,
    mut inbound: tonic::Streaming<sculk_proto::cluster::IoMessage>,
    engine: &mut Option<crate::engine::Engine>,
    files: Option<Arc<tokio::sync::Mutex<WorldFiles>>>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
    console: &mut mpsc::Receiver<String>,
) -> anyhow::Result<Ended> {
    let config = config.clone();
    let player_packets =
        sculk_proto::batcher(out.clone(), |batch| WorkerMessage { msg: Some(worker_message::Msg::PlayerBatch(batch)) });
    let ctx = Ctx { config, world, state: Arc::new(Mutex::new(state)), leases, io, out, player_packets };
    let heartbeat = Duration::from_millis(welcome.heartbeat_interval_ms.max(100).into());
    let mut _tasks = vec![
        Task(tokio::spawn(heartbeats(ctx.clone(), heartbeat))),
        Task(tokio::spawn(renewals(ctx.clone()))),
        Task(tokio::spawn(ticks(ctx.clone()))),
    ];
    if let Some(files) = &files {
        _tasks.push(Task(tokio::spawn(push_files(files.clone()))));
    }

    // Set once the worker asked IO to move its players away before it stops: until when it waits.
    let mut draining: Option<tokio::time::Instant> = None;
    // IO's messages are taken off the stream as they come, whatever the loop below is busy with: HTTP/2 drops a
    // connection whose received frames pile up unread.
    let (taken, mut from_io) = mpsc::unbounded_channel();
    let _reader = Task(tokio::spawn(async move {
        loop {
            let msg = inbound.message().await;
            let end = !matches!(msg, Ok(Some(_)));
            if taken.send(msg).is_err() || end {
                break;
            }
        }
    }));
    loop {
        let msg = tokio::select! {
            msg = from_io.recv() => match msg {
                Some(Ok(Some(msg))) => msg,
                Some(Err(e)) => return Err(e.into()),
                Some(Ok(None)) | None => break,
            },
            status = engine_exit(engine) => bail!("the engine stopped ({status:?})"),
            _ = stopped(stop), if draining.is_none() => {
                // Players go to other engines first (a rolling restart), when there are any here.
                if ctx.config.drain && engine.is_some() {
                    tracing::info!("stopping: IO moves the players to other engines first");
                    ctx.send(worker_message::Msg::Draining(Draining {})).await;
                    draining = Some(tokio::time::Instant::now() + DRAIN_TIMEOUT);
                    continue;
                }
                tracing::info!("stopping: the engine saves the world and players");
                return Ok(Ended::Stopped);
            }
            _ = async { tokio::time::sleep_until(draining.expect("guarded")).await }, if draining.is_some() => {
                tracing::warn!("IO did not move every player in time; the engine saves the rest");
                return Ok(Ended::Stopped);
            }
            Some(line) = console.recv() => {
                // `stop` stops the server, as on vanilla's console: the engine saves, then the worker ends.
                if line.trim() == "stop" {
                    tracing::info!("stop typed on the console: the engine saves the world and players");
                    return Ok(Ended::Stopped);
                }
                if let Err(e) = start_engine(&ctx, welcome.worker_id, engine, &files).await {
                    tracing::error!(error = format!("{e:#}"), "the engine could not start");
                } else if let Some(engine) = engine.as_mut()
                    && let Err(e) = engine.command(&line).await
                {
                    tracing::warn!(error = %e, "the engine's console is closed");
                }
                continue;
            }
        };
        match msg.msg {
            Some(io_message::Msg::AssignSession(a)) => {
                let session_id = a.session_id;
                let ready = assign(&ctx, a).await;
                let placed = ready.error.is_empty();
                ctx.send(worker_message::Msg::SessionReady(ready)).await;
                if placed {
                    let packets = {
                        let mut s = ctx.state();
                        player_mut(&mut s, session_id).map(|p| p.play.join(&ctx.world)).unwrap_or_default()
                    };
                    ctx.send_packets(session_id, packets).await;
                }
            }
            Some(io_message::Msg::PlayerPackets(p)) => packets(&ctx, p).await,
            Some(io_message::Msg::PlayerBatch(batch)) => {
                for p in batch.packets {
                    packets(&ctx, p).await;
                }
            }
            Some(io_message::Msg::CloseSession(c)) => close(&ctx, c).await,
            Some(io_message::Msg::PreparePlayer(p)) => {
                if let Err(e) = start_engine(&ctx, welcome.worker_id, engine, &files).await {
                    let error = format!("the engine could not start: {e:#}");
                    tracing::error!(error, "player not taken");
                    ctx.send(worker_message::Msg::PlayerPrepared(PlayerPrepared { request_id: p.request_id, error }))
                        .await;
                    continue;
                }
                let (ctx, files) = (ctx.clone(), files.clone());
                tokio::spawn(async move {
                    let error = match (files, <[u8; 16]>::try_from(p.uuid.as_slice())) {
                        (_, Err(_)) => "a UUID is 16 bytes".to_owned(),
                        (None, _) => String::new(),
                        (Some(files), Ok(uuid)) => {
                            let mut files = files.lock().await;
                            // What this engine saved goes first, so an older copy never replaces it.
                            let pushed = files.push().await;
                            match pushed.and(files.pull_player(u128::from_be_bytes(uuid)).await) {
                                Ok(_) => String::new(),
                                Err(e) => format!("{e:#}"),
                            }
                        }
                    };
                    ctx.send(worker_message::Msg::PlayerPrepared(PlayerPrepared { request_id: p.request_id, error }))
                        .await;
                });
            }
            Some(io_message::Msg::EngineCommand(c)) => {
                let control = match (ctx.config.engine(), engine.as_ref()) {
                    (Some(e), Some(_)) => crate::engine::control_endpoint(&e).ok().map(EngineControl::new),
                    _ => None,
                };
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let (reply, error) = match control {
                        None => (String::new(), "this worker's engine is not running".to_owned()),
                        Some(control) => match control.request(&c.line).await {
                            Ok(reply) => (reply, String::new()),
                            Err(e) => (String::new(), format!("{e:#}")),
                        },
                    };
                    let reply = EngineReply { request_id: c.request_id, reply, error };
                    ctx.send(worker_message::Msg::EngineReply(reply)).await;
                });
            }
            Some(io_message::Msg::Drained(d)) => {
                tracing::info!(
                    moved = d.moved,
                    left = d.left,
                    "players moved to other engines; the engine saves the rest"
                );
                return Ok(Ended::Stopped);
            }
            Some(io_message::Msg::Welcome(_)) | None => {}
        }
    }
    Ok(Ended::Lost)
}

/// Starts the engine if this worker runs one and it isn't running yet, with the world's files from AnvilDB in its
/// folder.
async fn start_engine(
    ctx: &Ctx,
    worker_id: u32,
    engine: &mut Option<crate::engine::Engine>,
    files: &Option<Arc<tokio::sync::Mutex<WorldFiles>>>,
) -> anyhow::Result<()> {
    let (Some(config), Some(files), None) = (ctx.config.engine(), files, engine.as_ref()) else { return Ok(()) };
    let pulled = files.lock().await.pull("").await?;
    tracing::info!(pulled, "world files from AnvilDB in the engine's folder");
    *engine = Some(crate::engine::start(&config, worker_id, &ctx.config.io, &ctx.config.anvildb, &ctx.config.world)?);
    Ok(())
}

/// Waits for the engine to exit; never returns for a worker without one.
async fn engine_exit(engine: &mut Option<crate::engine::Engine>) -> std::io::Result<std::process::ExitStatus> {
    match engine {
        Some(engine) => engine.exited().await,
        None => std::future::pending().await,
    }
}

/// Sends the engine's saved world files to AnvilDB as it saves them.
async fn push_files(files: Arc<tokio::sync::Mutex<WorldFiles>>) {
    let mut every = tokio::time::interval(FILE_PUSH);
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        match files.lock().await.push().await {
            Ok(0) => {}
            Ok(pushed) => tracing::debug!(pushed, "world files saved in AnvilDB"),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "world files not saved yet"),
        }
    }
}

fn player_mut(s: &mut State, session_id: u64) -> Option<&mut Player> {
    let mw_id = *s.sessions.get(&session_id)?;
    s.mws.get_mut(&mw_id)?.players.get_mut(&session_id)
}

/// Places a player: a new micro world if the zone is free, or the micro world that owns it if that one is here.
async fn assign(ctx: &Ctx, a: AssignSession) -> SessionReady {
    let session_id = a.session_id;
    let fail = |error: String| SessionReady { session_id, error, ..Default::default() };
    let pos = a.position.map(|p| [p.x, p.y, p.z]).unwrap_or(SPAWN);
    let zone = mw::zone_around(DIMENSION, pos[0], pos[2], ctx.config.view_distance);
    let chunks: Vec<_> = zone.iter().map(|c| c.to_pb(&ctx.config.world)).collect();
    let view = a.view_distance.clamp(2, ctx.config.view_distance.max(2)) as i32;

    let used_up = {
        let s = ctx.state();
        s.next_entity_id >= s.entity_id_end
    };
    if used_up {
        // The next block of cluster-wide unique IDs (docs/docs/09-hlobalnyi-stan.md).
        let worker_id = ctx.state().worker_id;
        match ctx.io.clone().allocate_entity_ids(AllocateEntityIdsRequest { worker_id }).await {
            Ok(block) => {
                let block = block.into_inner();
                tracing::info!(start = block.start, len = block.len, "new block of entity IDs");
                let mut s = ctx.state();
                s.next_entity_id = block.start;
                s.entity_id_end = block.start + block.len as i32;
            }
            Err(e) => return fail(format!("no entity IDs left: {}", e.message())),
        }
    }
    let (mw_id, entity_id) = {
        let mut s = ctx.state();
        let entity_id = s.next_entity_id;
        s.next_entity_id += 1;
        s.next_mw += 1;
        (mw::mw_id(s.worker_id, s.next_mw), entity_id)
    };
    let player = Player { name: a.username, play: PlayState::new(entity_id, view, pos) };
    ctx.state().compression.insert(session_id, usize::try_from(a.compression_threshold).ok());
    let mut leases = ctx.leases.clone();
    let acquired = leases.acquire(AcquireRequest { holder: Some(ctx.holder(mw_id)), chunks: chunks.clone() }).await;
    let result = match acquired.map(|r| r.into_inner().result) {
        Ok(Some(r)) => r,
        Ok(None) => return fail("AnvilDB sent an empty answer".into()),
        Err(e) => return fail(format!("AnvilDB: {}", e.message())),
    };

    match result {
        acquire_response::Result::Granted(_) => {
            tracing::info!(mw_id = %format!("{mw_id:#x}"), player = %player.name, chunks = zone.len(), "micro world born");
            let mut s = ctx.state();
            s.mws.insert(mw_id, MicroWorld::new(mw_id, session_id, player, zone));
            s.sessions.insert(session_id, mw_id);
            SessionReady { session_id, mw_id, ..Default::default() }
        }
        acquire_response::Result::Conflict(c) => {
            let owner = c.owner.unwrap_or_default();
            let here = {
                let s = ctx.state();
                owner.worker_id == s.worker_id && s.mws.contains_key(&owner.mw_id)
            };
            if !here {
                return SessionReady {
                    session_id,
                    error: format!("the zone belongs to micro world {:#x} on worker #{}", owner.mw_id, owner.worker_id),
                    redirect_worker_id: owner.worker_id,
                    ..Default::default()
                };
            }
            // The owner is here: the player joins it and its zone grows by the player's.
            let grown = leases.acquire(AcquireRequest { holder: Some(owner), chunks }).await;
            match grown.map(|r| r.into_inner().result) {
                Ok(Some(acquire_response::Result::Granted(_))) => {
                    let mut s = ctx.state();
                    let Some(mw) = s.mws.get_mut(&owner.mw_id) else {
                        return fail("the micro world closed meanwhile".into());
                    };
                    mw.players.insert(session_id, player);
                    mw.zone.extend(zone);
                    tracing::info!(mw_id = %format!("{:#x}", owner.mw_id), players = mw.players.len(), "player joined micro world");
                    s.sessions.insert(session_id, owner.mw_id);
                    SessionReady { session_id, mw_id: owner.mw_id, joined_existing: true, ..Default::default() }
                }
                Ok(_) => fail("the zone also overlaps a third micro world".into()),
                Err(e) => fail(format!("AnvilDB: {}", e.message())),
            }
        }
    }
}

/// A player's packets: handled at once, and the micro world's zone follows its moves.
async fn packets(ctx: &Ctx, p: PlayerPackets) {
    let (out, mw_id) = {
        let mut s = ctx.state();
        let Some(&mw_id) = s.sessions.get(&p.session_id) else { return };
        let Some(player) = player_mut(&mut s, p.session_id) else { return };
        let mut out = Vec::new();
        for body in &p.packets {
            match player.play.handle(body, &ctx.world) {
                Ok(mut more) => out.append(&mut more),
                Err(e) => tracing::debug!(session_id = p.session_id, error = %e, "bad packet from player"),
            }
        }
        (out, mw_id)
    };
    ctx.send_packets(p.session_id, out).await;
    rezone(ctx, mw_id).await;
}

/// Grows and shrinks a micro world's zone to what its players need (docs/docs/03-mikrosvity.md, "Growth"): leases on
/// new chunks first, then the chunks no one needs are released.
async fn rezone(ctx: &Ctx, mw_id: u64) {
    let (add, remove) = {
        let s = ctx.state();
        let Some(mw) = s.mws.get(&mw_id) else { return };
        let wanted = mw.wanted_zone(DIMENSION, ctx.config.view_distance);
        let add: Vec<Chunk> = wanted.difference(&mw.zone).cloned().collect();
        let remove: Vec<Chunk> = mw.zone.difference(&wanted).cloned().collect();
        (add, remove)
    };
    if add.is_empty() && remove.is_empty() {
        return;
    }
    let holder = ctx.holder(mw_id);
    let world = &ctx.config.world;
    let mut leases = ctx.leases.clone();
    if !add.is_empty() {
        let chunks = add.iter().map(|c| c.to_pb(world)).collect();
        match leases.acquire(AcquireRequest { holder: Some(holder), chunks }).await.map(|r| r.into_inner().result) {
            Ok(Some(acquire_response::Result::Granted(_))) => {
                if let Some(mw) = ctx.state().mws.get_mut(&mw_id) {
                    mw.zone.extend(add);
                }
            }
            Ok(Some(acquire_response::Result::Conflict(c))) => {
                let owner = c.owner.unwrap_or_default();
                // PLAN.md, block D: this is where micro worlds merge.
                tracing::warn!(mw_id = %format!("{mw_id:#x}"), other = %format!("{:#x}", owner.mw_id), "zone touches another micro world; merging is not built yet");
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(mw_id = %format!("{mw_id:#x}"), error = %e.message(), "growing the zone failed"),
        }
    }
    if !remove.is_empty() {
        // Changed chunks are saved to AnvilDB before this once storage exists (PLAN.md, block C).
        let chunks = remove.iter().map(|c| c.to_pb(world)).collect();
        if leases.release(ReleaseRequest { holder: Some(holder), chunks }).await.is_ok()
            && let Some(mw) = ctx.state().mws.get_mut(&mw_id)
        {
            for c in &remove {
                mw.zone.remove(c);
            }
        }
    }
}

/// A player left. A micro world without players releases its leases and ends.
async fn close(ctx: &Ctx, c: CloseSession) {
    let (mw_id, ended) = {
        let mut s = ctx.state();
        s.compression.remove(&c.session_id);
        let Some(mw_id) = s.sessions.remove(&c.session_id) else { return };
        let Some(mw) = s.mws.get_mut(&mw_id) else { return };
        mw.players.remove(&c.session_id);
        let ended = if mw.players.is_empty() { s.mws.remove(&mw_id) } else { None };
        (mw_id, ended)
    };
    let Some(mw) = ended else {
        // The others stay; the zone shrinks to theirs.
        rezone(ctx, mw_id).await;
        return;
    };
    let released = ctx.leases.clone().release(ReleaseRequest { holder: Some(ctx.holder(mw.id)), chunks: vec![] }).await;
    match released {
        Ok(r) => {
            tracing::info!(mw_id = %format!("{:#x}", mw.id), ticks = mw.ticks, released = r.into_inner().released, "micro world ended")
        }
        Err(e) => {
            tracing::warn!(mw_id = %format!("{:#x}", mw.id), error = %e.message(), "release failed; leases expire by TTL")
        }
    }
}

async fn heartbeats(ctx: Ctx, every: Duration) {
    let mut interval = tokio::time::interval(every);
    // A worker with an engine reports the engine's own load (its micro world, players and tick time), as last
    // read: asking the engine is never what makes a heartbeat late.
    let engine_load = Arc::new(Mutex::new(None));
    let _poll = ctx.config.engine().and_then(|e| crate::engine::control_endpoint(&e).ok()).map(|endpoint| {
        let (control, load) = (EngineControl::new(endpoint), engine_load.clone());
        Task(tokio::spawn(async move {
            let mut every = tokio::time::interval(every);
            every.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                every.tick().await;
                let now = read_engine_load(&control).await;
                *load.lock().unwrap_or_else(|e| e.into_inner()) = now;
            }
        }))
    });
    loop {
        interval.tick().await;
        let engine = *engine_load.lock().unwrap_or_else(|e| e.into_inner());
        let hb = match engine {
            Some((players, mspt)) => Heartbeat { mw_count: 1, player_count: players, mspt },
            None => {
                let s = ctx.state();
                Heartbeat { mw_count: s.mws.len() as u32, player_count: s.sessions.len() as u32, mspt: s.mspt }
            }
        };
        ctx.send(worker_message::Msg::Heartbeat(hb)).await;
    }
}

/// The engine's players and average milliseconds per tick (`server-status`), when it runs.
async fn read_engine_load(control: &EngineControl) -> Option<(u32, f32)> {
    let reply = control.request("server-status").await.ok()?;
    let status: serde_json::Value = serde_json::from_str(&reply).ok()?;
    let players = status["players"].as_array().map_or(0, |p| p.len() as u32);
    let mspt = status["mspt"]["average"].as_f64().unwrap_or(0.0) as f32;
    Some((players, mspt))
}

/// Keeps every micro world's leases alive. A micro world that lost some is only reported for now.
async fn renewals(ctx: Ctx) {
    let mut interval = tokio::time::interval(Duration::from_millis(ctx.config.lease_renew_ms));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let mws: Vec<(u64, usize)> = ctx.state().mws.values().map(|m| (m.id, m.zone.len())).collect();
        for (mw_id, held) in mws {
            match ctx.leases.clone().renew(RenewRequest { holder: Some(ctx.holder(mw_id)) }).await {
                Ok(r) if r.get_ref().renewed as usize >= held => {}
                Ok(r) => {
                    tracing::error!(mw_id = %format!("{mw_id:#x}"), held, renewed = r.get_ref().renewed, "micro world lost leases")
                }
                Err(e) => tracing::warn!(mw_id = %format!("{mw_id:#x}"), error = %e.message(), "lease renew failed"),
            }
        }
    }
}

/// Ticks every micro world 20 times a second. One thread for now; the per-core scheduler of
/// docs/docs/06-worker.md ("Threading model") replaces it.
async fn ticks(ctx: Ctx) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let start = Instant::now();
        let mut keep_alives = Vec::new();
        {
            let mut s = ctx.state();
            for mw in s.mws.values_mut() {
                mw.tick();
                for (&session_id, player) in mw.players.iter_mut() {
                    if let Some(packet) = player.play.keep_alive(start) {
                        keep_alives.push((session_id, packet));
                    }
                }
            }
            let ms = start.elapsed().as_secs_f32() * 1000.0;
            s.mspt = s.mspt * 0.95 + ms * 0.05;
        }
        for (session_id, packet) in keep_alives {
            ctx.send_packets(session_id, vec![packet]).await;
        }
    }
}

/// Waits until the worker is told to stop (without holding the watch's lock while others run).
async fn stopped(stop: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}
