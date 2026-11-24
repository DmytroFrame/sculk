//! Java Edition clients (docs/docs/05-io-server.md, "Duties"): handshake, server list ping, login and
//! configuration happen here; in play IO only frames packets and forwards them between the client and its worker.
//!
//! Online mode (encryption and Mojang's session server) is `auth`'s. Packets are `horizon_protocol`'s.

use crate::auth::{Keys, Profile};
use crate::conn::{Reader, Writer};
use crate::control::Shared;
use anyhow::{Context, bail};
use horizon_protocol::packets::{configuration as cfg, handshake, login, play, status};
use horizon_protocol::{GAME_VERSION, PROTOCOL_VERSION};
use horizon_world::nbt::{JavaString, Tag};
use sculk_data::GameData;
use sculk_proto::cluster::{AssignSession, CloseSession, IoMessage, PlayerPackets, PreparePlayer, io_message};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// How long a client may take from connect to play.
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a worker may take to place a player.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Clientbound packet batches waiting for a player's connection before IO gives up on it.
const PLAYER_QUEUE: usize = 4096;

pub struct Settings {
    pub motd: String,
    pub max_players: u32,
    pub world: String,
    pub compression_threshold: i32,
    pub data: Arc<GameData>,
    pub placement: crate::Placement,
    /// The server list's picture as a data URL, when there is one.
    pub favicon: Option<String>,
    /// Online mode: the server's key pair. `None` plays offline (behind a proxy, or for bots).
    pub keys: Option<Keys>,
}

pub async fn serve(listener: TcpListener, shared: Arc<Shared>, settings: Arc<Settings>) -> anyhow::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let (shared, settings) = (shared.clone(), settings.clone());
        tokio::spawn(async move {
            let _ = stream.set_nodelay(true);
            let (read, write) = stream.into_split();
            if let Err(e) = handle(Reader::new(read), Writer::new(write), peer, &shared, &settings).await {
                tracing::debug!(%peer, error = format!("{e:#}"), "client connection ended");
            }
        });
    }
}

fn text(s: &str) -> Tag {
    Tag::String(JavaString::from(s))
}

fn decode_err(e: horizon_protocol::Error) -> anyhow::Error {
    anyhow::anyhow!(e.0)
}

/// Players through Nether Tunnel (docs/docs/11-nether-tunnel.md): each tunnel session is a connection like a TCP one.
pub async fn serve_tunnel(socket: tokio::net::UdpSocket, shared: Arc<Shared>, settings: Arc<Settings>) {
    sculk_tunnel::serve(socket, move |stream, peer| {
        let (shared, settings) = (shared.clone(), settings.clone());
        tokio::spawn(async move {
            let (read, write) = tokio::io::split(stream);
            if let Err(e) = handle(Reader::new(read), Writer::new(write), peer, &shared, &settings).await {
                tracing::debug!(%peer, error = format!("{e:#}"), "tunnel connection ended");
            }
        });
    })
    .await
}

async fn handle(
    mut r: Reader,
    mut w: Writer,
    peer: SocketAddr,
    shared: &Arc<Shared>,
    settings: &Settings,
) -> anyhow::Result<()> {
    let Some((id, payload)) = r.packet().await? else { return Ok(()) };
    let intention = handshake::decode_intention(id, &payload, 255).map_err(decode_err)?;
    match intention.intent {
        handshake::STATUS => status(&mut r, &mut w, shared, settings).await,
        handshake::LOGIN | handshake::TRANSFER => {
            let joined = tokio::time::timeout(JOIN_TIMEOUT, join(&mut r, &mut w, peer, &intention, shared, settings))
                .await
                .context("client took too long to join")??;
            match joined {
                Some(Joined::Play(session)) => play(r, w, session, shared).await,
                Some(Joined::Proxy { worker_id, engine, profile }) => {
                    let who = crate::proxy::Who {
                        profile,
                        peer,
                        protocol: intention.protocol,
                        host: intention.host.clone(),
                        port: intention.port,
                        max_players: settings.max_players,
                    };
                    crate::proxy::proxy(r, w, who, worker_id, &engine, shared).await
                }
                None => Ok(()),
            }
        }
        other => bail!("unknown handshake intent {other}"),
    }
}

async fn status(r: &mut Reader, w: &mut Writer, shared: &Shared, settings: &Settings) -> anyhow::Result<()> {
    while let Some((id, payload)) = r.packet().await? {
        match status::Serverbound::decode(id, &payload).map_err(decode_err)? {
            status::Serverbound::StatusRequest(_) => {
                // As vanilla: up to 12 players in the hover list, never more.
                let sample: Vec<serde_json::Value> = shared
                    .registry()
                    .online()
                    .into_iter()
                    .take(12)
                    .map(|(uuid, name)| serde_json::json!({ "id": horizon_protocol::crypto::uuid_string(uuid), "name": name }))
                    .collect();
                let mut json = serde_json::json!({
                    "version": { "name": GAME_VERSION, "protocol": PROTOCOL_VERSION },
                    "players": { "max": settings.max_players, "online": shared.registry().player_count(), "sample": sample },
                    "description": { "text": settings.motd },
                    "enforcesSecureChat": false,
                    "preventsChatReports": true,
                });
                if let Some(icon) = &settings.favicon {
                    json["favicon"] = icon.clone().into();
                }
                let response = status::StatusResponse { json: json.to_string() };
                w.send(&status::Clientbound::StatusResponse(response).encode()).await?;
            }
            status::Serverbound::PingRequest(ping) => {
                w.send(&status::Clientbound::PongResponse(ping).encode()).await?;
                break;
            }
            status::Serverbound::Unknown(id, _) => bail!("unknown status packet {id}"),
        }
    }
    Ok(())
}

/// The server list's picture: vanilla's `server-icon.png`, a 64x64 PNG, as a data URL. Anything else is left out
/// with a warning, as vanilla does.
pub fn favicon(path: &std::path::Path) -> Option<String> {
    let png = std::fs::read(path).ok()?;
    let size = (png.len() >= 24 && png.starts_with(b"\x89PNG\r\n\x1a\n")).then(|| {
        (u32::from_be_bytes(png[16..20].try_into().unwrap()), u32::from_be_bytes(png[20..24].try_into().unwrap()))
    });
    if size != Some((64, 64)) {
        tracing::warn!(path = %path.display(), "the server icon must be a 64x64 PNG; it is left out");
        return None;
    }
    Some(format!("data:image/png;base64,{}", base64(&png)))
}

/// Standard base64 with padding.
fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ABC[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Where a login goes on.
enum Joined {
    /// Into play on a worker's built-in world, through IO.
    Play(Session),
    /// To a worker's game engine, which takes the login from here.
    Proxy { worker_id: u32, engine: String, profile: Profile },
}

/// A player in play on a worker.
struct Session {
    id: u64,
    worker_id: u32,
    name: String,
    /// The worker's batched packet stream.
    packets: mpsc::Sender<PlayerPackets>,
    from_worker: mpsc::Receiver<Vec<Vec<u8>>>,
}

/// Login and configuration. `None` when the player was turned away (and told why).
async fn join(
    r: &mut Reader,
    w: &mut Writer,
    peer: SocketAddr,
    intention: &handshake::Intention,
    shared: &Shared,
    settings: &Settings,
) -> anyhow::Result<Option<Joined>> {
    let refuse_login = async |w: &mut Writer, reason: String| -> anyhow::Result<Option<Joined>> {
        let reason_json = serde_json::json!({ "text": reason }).to_string();
        w.send(&login::Clientbound::Disconnect(login::Disconnect { reason_json }).encode()).await?;
        Ok(None)
    };

    let Some((id, payload)) = r.packet().await? else { return Ok(None) };
    let login::Serverbound::Hello(hello) = login::Serverbound::decode(id, &payload).map_err(decode_err)? else {
        bail!("expected the login hello");
    };
    if intention.protocol != PROTOCOL_VERSION {
        let reason = format!(
            "Sculk speaks Minecraft {GAME_VERSION} (protocol {PROTOCOL_VERSION}); your client sent protocol {}",
            intention.protocol
        );
        return refuse_login(w, reason).await;
    }
    if shared.registry().player_count() >= settings.max_players as usize {
        return refuse_login(w, "The server is full".into()).await;
    }
    let profile = match &settings.keys {
        None => Profile::offline(&hello.name),
        Some(keys) => match authenticate(r, w, &hello.name, keys).await? {
            Ok(profile) => profile,
            Err(reason) => return refuse_login(w, reason).await,
        },
    };
    // As vanilla: the same player logging in again takes over, and the older connection is closed.
    {
        let mut registry = shared.registry();
        if let Some(old) = registry.session_of(profile.uuid) {
            tracing::info!(name = %profile.name, session_id = old, "player logged in from another location");
            registry.drop_player(old);
        }
    }
    // A player whose zone falls in a micro world goes to that micro world's worker; anyone else to the least
    // loaded one (docs/docs/05-io-server.md, "Duties").
    // The player appears where it left, or at the spawn the first time.
    let spawn = match shared.saved_chunk(&settings.world, profile.uuid).await {
        Some((dimension, x, z)) => sculk_proto::anvildb::ChunkPos { world: settings.world.clone(), dimension, x, z },
        None => sculk_proto::anvildb::ChunkPos {
            world: settings.world.clone(),
            dimension: sculk_data::SPAWN_DIMENSION.into(),
            x: (sculk_data::SPAWN[0].floor() as i32) >> 4,
            z: (sculk_data::SPAWN[2].floor() as i32) >> 4,
        },
    };
    let owner = match shared.chunk_owner(spawn).await {
        Ok(owner) => owner,
        Err(e) => {
            tracing::warn!(error = %e, "AnvilDB did not say who owns the spawn");
            return refuse_login(w, "The world database is unavailable; try again soon".into()).await;
        }
    };
    // A worker that runs a game engine plays the rest itself: IO only proxies.
    let engine = {
        let mut registry = shared.registry();
        let target = owner
            .filter(|&id| registry.worker(id).is_some())
            .or_else(|| match settings.placement {
                crate::Placement::Spread => registry.least_loaded_engine(),
                crate::Placement::Single => registry.primary_engine(),
            })
            .or_else(|| registry.least_loaded().map(|(id, _)| id));
        let engine = target.and_then(|id| registry.engine(id).map(|addr| (id, addr)));
        if let Some((id, _)) = &engine {
            registry.joining(*id);
        }
        engine
    };
    if let Some((worker_id, engine)) = engine {
        if let Err(reason) = prepare_player(shared, worker_id, profile.uuid).await {
            shared.registry().joined(worker_id);
            return refuse_login(w, reason).await;
        }
        return Ok(Some(Joined::Proxy { worker_id, engine, profile }));
    }
    let Profile { name, uuid, properties } = profile;
    if let Ok(threshold) = usize::try_from(settings.compression_threshold) {
        let set = login::Clientbound::Compression(login::Compression { threshold: settings.compression_threshold });
        w.send(&set.encode()).await?;
        w.set_compression(threshold);
        r.set_compression(threshold);
    }
    let profile = login::GameProfile { uuid, name: name.clone(), properties };
    w.send(&login::Clientbound::LoginFinished(login::LoginFinished { profile, session: uuid }).encode()).await?;
    loop {
        let Some((id, payload)) = r.packet().await? else { return Ok(None) };
        match login::Serverbound::decode(id, &payload).map_err(decode_err)? {
            login::Serverbound::LoginAcknowledged(_) => break,
            login::Serverbound::CustomQueryAnswer(_) | login::Serverbound::CookieResponse(_) => {}
            other => bail!("unexpected {} during login", other.name()),
        }
    }

    // Configuration: the client takes the registries from its own vanilla pack and gets only the entry names.
    let refuse = async |w: &mut Writer, reason: &str| -> anyhow::Result<Option<Joined>> {
        w.send(&cfg::Clientbound::Disconnect(cfg::Disconnect { reason: text(reason) }).encode()).await?;
        Ok(None)
    };
    let mut brand = horizon_protocol::buf::Writer::new();
    brand.string("sculk");
    let core = cfg::KnownPack { namespace: "minecraft".into(), id: "core".into(), version: GAME_VERSION.into() };
    w.send_all(&[
        cfg::Clientbound::CustomPayload(cfg::CustomPayload { channel: "minecraft:brand".into(), data: brand.data })
            .encode(),
        cfg::Clientbound::UpdateEnabledFeatures(cfg::UpdateEnabledFeatures {
            features: vec!["minecraft:vanilla".into()],
        })
        .encode(),
        cfg::Clientbound::SelectKnownPacks(cfg::SelectKnownPacks { packs: vec![core.clone()] }).encode(),
    ])
    .await?;
    let mut view_distance = 10;
    let known = loop {
        let Some((id, payload)) = r.packet().await? else { return Ok(None) };
        match cfg::Serverbound::decode(id, &payload).map_err(decode_err)? {
            cfg::Serverbound::ClientInformation(info) => view_distance = info.view_distance,
            cfg::Serverbound::SelectKnownPacks(p) => break p.packs == vec![core.clone()],
            _ => {}
        }
    };
    if !known {
        return refuse(w, "Sculk needs the vanilla 26.3 client: it did not confirm the minecraft:core pack").await;
    }
    let mut packets: Vec<Vec<u8>> = settings
        .data
        .registries
        .iter()
        .filter(|reg| !reg.entries.is_empty())
        .map(|reg| {
            let entries = reg.entries.iter().map(|e| cfg::RegistryEntry { id: e.clone(), data: None }).collect();
            cfg::Clientbound::RegistryData(cfg::RegistryData { registry: reg.name.clone(), entries }).encode()
        })
        .collect();
    let registries = settings
        .data
        .tags
        .iter()
        .map(|t| cfg::RegistryTags {
            registry: t.registry.clone(),
            tags: t.tags.iter().map(|(name, ids)| cfg::TagEntry { name: name.clone(), ids: ids.clone() }).collect(),
        })
        .collect();
    packets.push(cfg::Clientbound::UpdateTags(cfg::UpdateTags { registries }).encode());
    w.send_all(&packets).await?;

    // A worker takes the player before configuration ends, so a refusal can still be shown.
    let view = view_distance.max(2) as u32;
    let placed = match place(shared, owner, &name, uuid, view, settings.compression_threshold).await {
        Ok(placed) => placed,
        Err(reason) => return refuse(w, &reason).await,
    };
    shared.registry().set_player(placed.session_id, uuid, &name);
    tracing::info!(%peer, %name, uuid = %horizon_protocol::crypto::uuid_string(uuid), worker_id = placed.worker_id, session_id = placed.session_id, "player placed");

    w.send(&cfg::Clientbound::FinishConfiguration(Default::default()).encode()).await?;
    loop {
        let packet = match r.packet().await {
            Ok(Some(p)) => p,
            Ok(None) => {
                close_session(shared, placed.session_id).await;
                return Ok(None);
            }
            Err(e) => {
                close_session(shared, placed.session_id).await;
                return Err(e);
            }
        };
        if let cfg::Serverbound::FinishConfiguration(_) =
            cfg::Serverbound::decode(packet.0, &packet.1).map_err(decode_err)?
        {
            break;
        }
    }
    let Placed { worker_id, worker, session_id, from_worker } = placed;
    let Some(packets) = shared.registry().worker_packets(worker_id) else {
        close_session(shared, session_id).await;
        return Ok(None);
    };
    drop(worker);
    Ok(Some(Joined::Play(Session { id: session_id, worker_id, name, packets, from_worker })))
}

/// Online mode's part of the login (vanilla's ServerLoginPacketListenerImpl): the encryption request and response,
/// encryption from then on, and the session server's word on who the player is. The inner error is what the player
/// is told.
async fn authenticate(
    r: &mut Reader,
    w: &mut Writer,
    name: &str,
    keys: &Keys,
) -> anyhow::Result<Result<Profile, String>> {
    let (request, challenge) = keys.request()?;
    w.send(&login::Clientbound::Hello(request).encode()).await?;
    let Some((id, payload)) = r.packet().await? else { bail!("the client left during the encryption") };
    let login::Serverbound::Key(key) = login::Serverbound::decode(id, &payload).map_err(decode_err)? else {
        bail!("expected the encryption response");
    };
    let secret = keys.shared_secret(&key, &challenge)?;
    // Everything after the response is encrypted, the disconnect for a failed check too.
    r.encrypt(&secret);
    w.encrypt(&secret);
    Ok(match keys.has_joined(name, &secret, None).await {
        Ok(Some(profile)) => Ok(profile),
        Ok(None) => Err("Failed to verify username!".into()),
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "session server unavailable");
            Err("Authentication servers are down. Please try again later, sorry!".into())
        }
    })
}

/// How long a worker may take to bring a player's files from AnvilDB.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a player's last worker may take to save it in AnvilDB after it left (its engine saves the player as it
/// leaves; the worker sends the file within a second).
const SAVE_AFTER_LEAVING: Duration = Duration::from_secs(3);

/// Has `worker_id` put the player's saved files where its engine reads them. A player that just left another
/// worker waits until that worker had time to save it. The error is what the player is told.
pub(crate) async fn prepare_player(shared: &Shared, worker_id: u32, uuid: u128) -> Result<(), String> {
    let left = shared.registry().left_recently(uuid, SAVE_AFTER_LEAVING);
    if let Some((from, at)) = left
        && from != worker_id
    {
        tokio::time::sleep_until((at + SAVE_AFTER_LEAVING).into()).await;
    }
    let Some(worker) = shared.registry().worker(worker_id) else {
        return Err(format!("Worker #{worker_id} went away"));
    };
    let (request_id, answer) = shared.registry().prepare();
    let msg = IoMessage {
        msg: Some(io_message::Msg::PreparePlayer(PreparePlayer { request_id, uuid: uuid.to_be_bytes().to_vec() })),
    };
    if worker.send(Ok(msg)).await.is_err() {
        return Err(format!("Worker #{worker_id} went away"));
    }
    match tokio::time::timeout(PREPARE_TIMEOUT, answer).await {
        Ok(Ok(error)) if error.is_empty() => Ok(()),
        Ok(Ok(error)) => {
            tracing::warn!(worker_id, error, "worker could not take a player");
            Err(format!("Worker #{worker_id} could not take you: {error}"))
        }
        Ok(Err(_)) => Err(format!("Worker #{worker_id} went away")),
        Err(_) => Err(format!("Worker #{worker_id} did not load your data in {} s", PREPARE_TIMEOUT.as_secs())),
    }
}

struct Placed {
    worker_id: u32,
    worker: crate::registry::WorkerTx,
    session_id: u64,
    from_worker: mpsc::Receiver<Vec<Vec<u8>>>,
}

/// Gives the player to `target` (the worker that owns its zone) or the least loaded worker. A worker that finds the
/// zone taken on another worker names it, and the player goes there once. The error is what the player is told.
async fn place(
    shared: &Shared,
    mut target: Option<u32>,
    name: &str,
    uuid: u128,
    view: u32,
    compression_threshold: i32,
) -> Result<Placed, String> {
    for _ in 0..2 {
        let (to_player, from_worker) = mpsc::channel(PLAYER_QUEUE);
        let opened = {
            let mut registry = shared.registry();
            let worker =
                target.and_then(|id| registry.worker(id).map(|tx| (id, tx))).or_else(|| registry.least_loaded());
            worker.map(|(worker_id, worker)| {
                let (session_id, ready) = registry.open_session(worker_id, to_player);
                (worker_id, worker, session_id, ready)
            })
        };
        let Some((worker_id, worker, session_id, ready)) = opened else {
            return Err("No workers are online: start one with `sculk worker`".into());
        };
        let assign = AssignSession {
            session_id,
            username: name.to_owned(),
            uuid: uuid.to_be_bytes().to_vec(),
            position: None,
            view_distance: view,
            compression_threshold,
        };
        let msg = IoMessage { msg: Some(io_message::Msg::AssignSession(assign)) };
        let (reason, redirect) = if worker.send(Ok(msg)).await.is_err() {
            (format!("Worker #{worker_id} went away"), 0)
        } else {
            match tokio::time::timeout(READY_TIMEOUT, ready).await {
                Ok(Ok(r)) if r.error.is_empty() => return Ok(Placed { worker_id, worker, session_id, from_worker }),
                Ok(Ok(r)) => (format!("Worker #{worker_id} could not place you: {}", r.error), r.redirect_worker_id),
                Ok(Err(_)) => (format!("Worker #{worker_id} went away"), 0),
                Err(_) => (format!("Worker #{worker_id} did not answer in {} s", READY_TIMEOUT.as_secs()), 0),
            }
        };
        close_session(shared, session_id).await;
        if redirect == 0 || redirect == worker_id {
            return Err(reason);
        }
        tracing::debug!(name, from = worker_id, to = redirect, "player redirected to the worker of its zone");
        target = Some(redirect);
    }
    Err("Your micro world moved between workers while you joined; try again".into())
}

async fn close_session(shared: &Shared, session_id: u64) {
    let worker = shared.registry().close_session(session_id);
    if let Some(worker) = worker {
        let msg = IoMessage { msg: Some(io_message::Msg::CloseSession(CloseSession { session_id })) };
        let _ = worker.send(Ok(msg)).await;
    }
}

/// Forwards packets both ways until either side stops.
async fn play(mut r: Reader, mut w: Writer, mut session: Session, shared: &Shared) -> anyhow::Result<()> {
    tracing::info!(name = %session.name, session_id = session.id, worker_id = session.worker_id, "player in play");
    let result: anyhow::Result<&str> = async {
        loop {
            tokio::select! {
                batch = session.from_worker.recv() => match batch {
                    Some(batch) => w.send_contents(&batch).await?,
                    None => {
                        let reason = text("Your worker stopped; the server will move players to another one in a later version");
                        let _ = w.send(&play::Clientbound::Disconnect(cfg::Disconnect { reason }).encode()).await;
                        break Ok("worker gone");
                    }
                },
                body = r.next() => match body? {
                    Some(body) => {
                        let packets = PlayerPackets { session_id: session.id, packets: vec![body] };
                        if session.packets.send(packets).await.is_err() {
                            break Ok("worker stream closed");
                        }
                    }
                    None => break Ok("client left"),
                },
            }
        }
    }
    .await;
    w.shutdown().await;
    close_session(shared, session.id).await;
    let why = result?;
    tracing::info!(name = %session.name, session_id = session.id, why, "player left");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// A session server that knows one player: it answers `hasJoined` for "Notch" with a profile and a skin.
    async fn session_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let response = if request.contains("username=Notch&") {
                    let body = r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch","properties":[{"name":"textures","value":"e30=","signature":"c2ln"}]}"#;
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                } else {
                    "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_owned()
                };
                s.write_all(response.as_bytes()).await.unwrap();
            }
        });
        url
    }

    /// The client's side of online mode, as vanilla's ClientHandshakePacketListenerImpl: answers the encryption
    /// request and encrypts from then on. Returns the first packet the server sends after that.
    async fn online_client(addr: SocketAddr) -> (i32, Vec<u8>) {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (read, write) = stream.into_split();
        let (mut r, mut w) = (Reader::new(read), Writer::new(write));
        let (id, payload) = r.packet().await.unwrap().unwrap();
        let login::Clientbound::Hello(hello) = login::Clientbound::decode(id, &payload).unwrap() else {
            panic!("expected the encryption request")
        };
        assert!(hello.should_authenticate);
        let public = horizon_protocol::rsa::RsaPublicKey::from_der(&hello.public_key).unwrap();
        let secret = [5u8; 16];
        let key =
            login::Key { key: public.encrypt(&secret).unwrap(), challenge: public.encrypt(&hello.challenge).unwrap() };
        w.send(&login::Serverbound::Key(key).encode()).await.unwrap();
        r.encrypt(&secret);
        w.encrypt(&secret);
        r.packet().await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn online_mode_takes_the_profile_from_the_session_server() {
        let keys = Keys::with_session_server(&session_server().await).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        for (name, expected) in [("Notch", true), ("Herobrine", false)] {
            let client = tokio::spawn(online_client(addr));
            let (stream, _) = listener.accept().await.unwrap();
            let (read, write) = stream.into_split();
            let (mut r, mut w) = (Reader::new(read), Writer::new(write));
            let result = authenticate(&mut r, &mut w, name, &keys).await.unwrap();
            match result {
                Ok(profile) => {
                    assert!(expected);
                    assert_eq!(profile.uuid, 0x069a79f444e94726a5befca90e38aaf5);
                    assert_eq!(profile.properties[0].name, "textures");
                    // The connection is encrypted both ways: a packet sent now arrives whole.
                    let done = login::Clientbound::Compression(login::Compression { threshold: 77 });
                    w.send(&done.encode()).await.unwrap();
                    let (id, payload) = client.await.unwrap();
                    assert_eq!(login::Clientbound::decode(id, &payload).unwrap(), done);
                }
                Err(reason) => {
                    assert!(!expected);
                    assert_eq!(reason, "Failed to verify username!");
                    drop(client);
                }
            }
        }
    }

    #[test]
    fn base64_pads_as_java_does() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
