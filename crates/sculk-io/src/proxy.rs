//! Players on workers' game engines: IO logs in to an engine as a Velocity proxy would (signed forwarding of the
//! player's name, UUID, address and skin), then passes frames both ways as they come. Every engine compresses alike
//! (the worker sets `network-compression-threshold`), so frames cross untouched, only re-encrypted for an online
//! client.
//!
//! A player moves to another engine without leaving the server (docs/docs/03-mikrosvity.md, "Злиття"): IO logs in
//! to the new engine itself, sends the client back to configuration (`StartConfiguration`, as Velocity does when it
//! switches servers), and from the client's acknowledgement on, the client talks to the new engine, which runs the
//! configuration and the join again. The client shows a short loading screen. `handover` decides when, and moves the
//! player's state between the engines.

use crate::auth::Profile;
use crate::conn::{Reader, Writer};
use crate::control::Shared;
use anyhow::{Context, bail};
use horizon_protocol::ids::configuration::serverbound as cfg_sb;
use horizon_protocol::ids::play::serverbound as play_sb;
use horizon_protocol::packets::{Empty, handshake, login, play};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

/// How long IO keeps trying to reach a worker's engine (it may still be starting).
const ENGINE_CONNECT: Duration = Duration::from_secs(10);
/// How long the client may take to acknowledge going back to configuration.
const ACKNOWLEDGE: Duration = Duration::from_secs(10);
/// How long the old engine's connection stays open after a move, when nobody releases it sooner.
const OLD_LINK: Duration = Duration::from_secs(60);

/// What a player's relay is asked to do.
pub enum Command {
    /// Switch the client to this worker's engine. The player is frozen on its engine and expected on the new one.
    /// The answer comes once the client talks to the new engine, or with why it could not, and whether the client
    /// is still on the old engine (`false`: the client was already sent away and its connection is lost).
    Move { worker_id: u32, engine: String, done: oneshot::Sender<Result<(), (String, bool)>> },
    /// The old engine let go of the player (`player-release ... moved`): its connection closes.
    Release,
    /// A frame from the client for its engine, held back while the player moved (a `/tp` to a player there).
    Forward(Vec<u8>),
}

/// Who the player is, for logging in to an engine on its behalf.
#[derive(Clone)]
pub struct Who {
    pub profile: Profile,
    pub peer: SocketAddr,
    pub protocol: i32,
    pub host: String,
    pub port: u16,
    /// The server's player limit, for `/list`.
    pub max_players: u32,
}

/// Where the client is in the protocol, as IO follows it from what the client sends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Before the client acknowledged the login.
    Login,
    Configuration,
    Play,
}

/// What a client sent that IO acts on.
enum Seen {
    Chat(String),
    Command(String),
}

/// What IO keeps of the client to tell a new engine: its last client information (view distance, language, skin
/// parts...), as Velocity resends a client's settings when it switches servers.
struct Client {
    phase: Phase,
    /// The ClientInformation payload, without its packet ID.
    information: Option<Vec<u8>>,
    /// When the client last went into play.
    playing_since: tokio::time::Instant,
}

impl Client {
    /// Follows a frame the client sent; returns what IO acts on.
    fn saw(&mut self, r: &mut Reader, frame: &[u8]) -> anyhow::Result<Option<Seen>> {
        let body = r.body(frame.to_vec())?;
        let mut reader = horizon_protocol::buf::Reader::new(&body);
        let id = reader.varint().map_err(decode_err)?;
        match (self.phase, id) {
            (Phase::Login, _) => self.phase = Phase::Configuration,
            (Phase::Configuration, cfg_sb::CLIENT_INFORMATION) | (Phase::Play, play_sb::CLIENT_INFORMATION) => {
                self.information = Some(reader.rest().to_vec());
            }
            (Phase::Configuration, cfg_sb::FINISH_CONFIGURATION) => {
                self.phase = Phase::Play;
                self.playing_since = tokio::time::Instant::now();
            }
            (Phase::Play, play_sb::CHAT) => {
                if let Ok(play::Serverbound::Chat(chat)) = play::Serverbound::decode(id, reader.rest()) {
                    return Ok(Some(Seen::Chat(chat.message)));
                }
            }
            (Phase::Play, play_sb::CHAT_COMMAND) => {
                if let Ok(play::Serverbound::ChatCommand(c)) = play::Serverbound::decode(id, reader.rest()) {
                    return Ok(Some(Seen::Command(c.command)));
                }
            }
            _ => {}
        }
        Ok(None)
    }

    /// The client information as a configuration packet, for a new engine.
    fn information_packet(&self) -> Option<Vec<u8>> {
        let payload = self.information.as_ref()?;
        let mut w = horizon_protocol::buf::Writer::new();
        w.varint(cfg_sb::CLIENT_INFORMATION);
        w.data.extend_from_slice(payload);
        Some(w.data)
    }
}

/// A connection to an engine.
struct Link {
    r: Reader,
    w: Writer,
    worker_id: u32,
}

async fn connect(engine: &str) -> anyhow::Result<TcpStream> {
    let deadline = tokio::time::Instant::now() + ENGINE_CONNECT;
    loop {
        match TcpStream::connect(engine).await {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(e) if tokio::time::Instant::now() >= deadline => return Err(e).context(format!("engine at {engine}")),
            Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

fn decode_err(e: horizon_protocol::Error) -> anyhow::Error {
    anyhow::anyhow!(e.0)
}

/// Logs in to `engine` for the player: the engine's login packets after the forwarding (Compression, LoginFinished)
/// as it sent them, or its disconnect when it turned the player away.
async fn login(
    engine: &str,
    worker_id: u32,
    who: &Who,
    secret: &str,
) -> anyhow::Result<Result<(Link, Vec<Vec<u8>>), Vec<u8>>> {
    let (read, write) = connect(engine).await?.into_split();
    let (mut r, mut w) = (Reader::new(read), Writer::new(write));
    let intention = handshake::Intention {
        protocol: who.protocol,
        host: who.host.clone(),
        port: who.port,
        intent: handshake::LOGIN,
    };
    w.send(&handshake::Serverbound::Intention(intention).encode()).await?;
    let hello = login::ServerHello { name: who.profile.name.clone(), uuid: who.profile.uuid };
    w.send(&login::Serverbound::Hello(hello).encode()).await?;
    let mut sent = Vec::new();
    loop {
        let Some(body) = r.next().await? else { bail!("engine of worker #{worker_id} closed the login") };
        let mut reader = horizon_protocol::buf::Reader::new(&body);
        let id = reader.varint().map_err(decode_err)?;
        match login::Clientbound::decode(id, reader.rest()).map_err(decode_err)? {
            login::Clientbound::CustomQuery(q) if q.channel == horizon_protocol::forwarding::CHANNEL => {
                let forwarded = horizon_protocol::forwarding::Forwarded {
                    address: who.peer.ip(),
                    uuid: who.profile.uuid,
                    name: who.profile.name.clone(),
                    properties: who.profile.properties.clone(),
                };
                let payload = horizon_protocol::forwarding::sign(secret.as_bytes(), &forwarded);
                let answer = login::CustomQueryAnswer { transaction: q.transaction, payload: Some(payload) };
                w.send(&login::Serverbound::CustomQueryAnswer(answer).encode()).await?;
            }
            login::Clientbound::CustomQuery(q) => {
                let answer = login::CustomQueryAnswer { transaction: q.transaction, payload: None };
                w.send(&login::Serverbound::CustomQueryAnswer(answer).encode()).await?;
            }
            login::Clientbound::CookieRequest(c) => {
                let answer = login::CookieResponse { key: c.key, payload: None };
                w.send(&login::Serverbound::CookieResponse(answer).encode()).await?;
            }
            login::Clientbound::Compression(c) => {
                sent.push(body);
                if let Ok(threshold) = usize::try_from(c.threshold) {
                    r.set_compression(threshold);
                    w.set_compression(threshold);
                }
            }
            login::Clientbound::LoginFinished(_) => {
                sent.push(body);
                return Ok(Ok((Link { r, w, worker_id }, sent)));
            }
            login::Clientbound::Disconnect(_) => return Ok(Err(body)),
            login::Clientbound::Hello(_) => bail!("engine of worker #{worker_id} is in online mode"),
            login::Clientbound::Unknown(id, _) => bail!("engine of worker #{worker_id} sent login packet {id}"),
        }
    }
}

/// Takes the player off its worker's joining count when dropped.
struct Joining<'a> {
    shared: &'a Shared,
    worker_id: u32,
}

impl Drop for Joining<'_> {
    fn drop(&mut self) {
        self.shared.registry().joined(self.worker_id);
    }
}

fn scopeguard(shared: &Shared, worker_id: u32) -> Joining<'_> {
    Joining { shared, worker_id }
}

/// Hands the login to a worker's game engine and relays the player until it leaves.
#[allow(clippy::too_many_arguments)]
pub async fn proxy(
    mut r: Reader,
    mut w: Writer,
    who: Who,
    worker_id: u32,
    engine: &str,
    shared: &Arc<Shared>,
) -> anyhow::Result<()> {
    let logged_in = login(engine, worker_id, &who, &shared.forwarding_secret).await;
    // The player now counts as a session there, or not at all.
    let joined = scopeguard(shared, worker_id);
    let (link, sent) = match logged_in {
        Ok(Ok(logged_in)) => logged_in,
        // The engine refusing the player: the client sees why.
        Ok(Err(disconnect)) => {
            w.send(&disconnect).await?;
            return Ok(());
        }
        Err(e) => {
            let reason_json =
                serde_json::json!({ "text": format!("Worker #{worker_id} is not taking players yet") }).to_string();
            w.send(&login::Clientbound::Disconnect(login::Disconnect { reason_json }).encode()).await?;
            return Err(e);
        }
    };
    // The client compresses as the engine does, so frames cross as they are.
    for body in sent {
        w.send(&body).await?;
        if let Some(threshold) = link.r.threshold() {
            r.set_compression(threshold);
            w.set_compression(threshold);
        }
    }

    let (closer, closed) = mpsc::channel(1);
    let (commands, commands_rx) = mpsc::channel(4);
    drop(joined);
    let session_id = {
        let mut registry = shared.registry();
        let (session_id, _) = registry.open_session(worker_id, closer);
        registry.set_player(session_id, who.profile.uuid, &who.profile.name);
        registry.set_properties(session_id, who.profile.properties.clone());
        registry.set_commands(session_id, commands.clone());
        session_id
    };
    let name = who.profile.name.clone();
    tracing::info!(peer = %who.peer, %name, uuid = %horizon_protocol::crypto::uuid_string(who.profile.uuid), worker_id, session_id, "player proxied to engine");
    shared.joined(worker_id, &name, true);
    let why = relay(r, w, link, who, commands_rx, commands, closed, shared).await;
    let last_worker = shared.registry().session_worker(session_id).unwrap_or(worker_id);
    shared.registry().close_session(session_id);
    shared.joined(last_worker, &name, false);
    tracing::info!(%name, session_id, why = %format!("{why:#}"), "proxied player left");
    Ok(())
}

/// Passes frames between the client and its engine until either side stops, and carries out moves.
#[allow(clippy::too_many_arguments)]
async fn relay(
    mut r: Reader,
    mut w: Writer,
    mut link: Link,
    who: Who,
    mut commands: mpsc::Receiver<Command>,
    self_commands: mpsc::Sender<Command>,
    mut closed: mpsc::Receiver<Vec<Vec<u8>>>,
    shared: &Arc<Shared>,
) -> anyhow::Error {
    let secret = shared.forwarding_secret.as_str();
    // The engine the player just left, kept open until it releases the player.
    let mut old: Option<Link> = None;
    let mut client = Client { phase: Phase::Login, information: None, playing_since: tokio::time::Instant::now() };
    // The players of other engines this client's tab list shows, put there by IO.
    let mut tab: HashSet<u128> = HashSet::new();
    let mut tab_every = tokio::time::interval(TAB_LIST);
    let old_deadline = tokio::time::sleep(OLD_LINK);
    tokio::pin!(old_deadline);
    let result: anyhow::Result<&str> = async {
        loop {
            tokio::select! {
                frame = r.next_frame() => match frame? {
                    Some(frame) => match client.saw(&mut r, &frame)? {
                        // Chat reaches the players of the other engines through IO.
                        Some(Seen::Chat(message)) => {
                            shared.chat(link.worker_id, &who.profile.name, &message);
                            link.w.send_contents(&[frame]).await?;
                        }
                        // `/list` counts the whole cluster's players, which only IO knows.
                        Some(Seen::Command(command)) if matches!(command.trim(), "list" | "list uuids") => {
                            let uuids = command.trim() == "list uuids";
                            w.send(&list_reply(shared, who.max_players, uuids)).await?;
                        }
                        // A teleport to a player on another engine: the player goes to that engine first, where
                        // the command then runs (and is allowed or not, as there).
                        Some(Seen::Command(command)) if tp_target(&command, &who.profile.name)
                            .and_then(|target| shared.engine_of(&target))
                            .is_some_and(|w| w != link.worker_id) =>
                        {
                            let target = tp_target(&command, &who.profile.name).expect("matched");
                            let (shared, commands, uuid) = (shared.clone(), self_commands.clone(), who.profile.uuid);
                            tokio::spawn(async move {
                                if let Err(e) = shared.join_player(uuid, &target).await {
                                    tracing::info!(error = format!("{e:#}"), "the teleport runs where the player is");
                                }
                                let _ = commands.send(Command::Forward(frame)).await;
                            });
                        }
                        _ => link.w.send_contents(&[frame]).await?,
                    },
                    None => return Ok("client left"),
                },
                frame = link.r.next_frame() => match frame {
                    Ok(Some(frame)) => w.send_contents(&[frame]).await?,
                    // The engine's connection ended: a kick (the client was told why), or the engine died with its
                    // worker, and the player goes on on another engine.
                    Ok(None) | Err(_) => {
                        if client.phase != Phase::Play || !rescue(shared, &mut r, &mut w, &mut link, &mut client, &who).await {
                            return Ok("engine closed");
                        }
                        tab.clear();
                    }
                },
                // What the old engine still sends is for a player who is gone from it.
                frame = async { old.as_mut().expect("guarded").r.next_frame().await }, if old.is_some() => {
                    if !matches!(frame, Ok(Some(_))) {
                        old = None;
                    }
                }
                _ = &mut old_deadline, if old.is_some() => old = None,
                command = commands.recv() => match command {
                    Some(Command::Move { worker_id, engine, done }) => {
                        match switch(&mut r, &mut w, &mut link, &mut client, worker_id, &engine, &who, secret).await {
                            Ok(previous) => {
                                // A client back from configuration starts a new tab list.
                                tab.clear();
                                old = Some(previous);
                                old_deadline.as_mut().reset(tokio::time::Instant::now() + OLD_LINK);
                                let _ = done.send(Ok(()));
                            }
                            Err((reason, still_here)) => {
                                let _ = done.send(Err((reason.clone(), still_here)));
                                if !still_here {
                                    bail!("moving to worker #{worker_id} failed: {reason}");
                                }
                            }
                        }
                    }
                    Some(Command::Release) => old = None,
                    Some(Command::Forward(frame)) => link.w.send_contents(&[frame]).await?,
                    None => {}
                },
                // IO dropped the session: the player logged in from another location.
                _ = closed.recv() => return Ok("logged in from another location"),
                // The players of the other engines in the tab list.
                _ = tab_every.tick(), if client.phase == Phase::Play && client.playing_since.elapsed() >= TAB_LIST * 2 => {
                    if let Some(update) = tab_list(shared, &mut tab, link.worker_id, who.profile.uuid) {
                        w.send_all(&update).await?;
                    }
                }
            }
        }
    }
    .await;
    w.shutdown().await;
    match result {
        Ok(why) => anyhow::anyhow!(why),
        Err(e) => e,
    }
}

/// How long IO waits, after an engine's connection ended, to tell a dead worker from a kick.
const WORKER_GONE: Duration = Duration::from_millis(1500);

/// The player's engine went away with its worker: the player goes on on another engine, loaded from what AnvilDB
/// has of it (docs/docs/10-nadiinist.md, "Відмова воркера"), instead of losing its connection. False when its
/// worker is alive (the engine closed the connection on purpose) or no other engine takes the player.
async fn rescue(
    shared: &Arc<Shared>,
    r: &mut Reader,
    w: &mut Writer,
    link: &mut Link,
    client: &mut Client,
    who: &Who,
) -> bool {
    let dead = link.worker_id;
    tokio::time::sleep(WORKER_GONE).await;
    if shared.registry().worker(dead).is_some() {
        return false;
    }
    let uuid = who.profile.uuid;
    let Some(session_id) = shared.registry().session_of(uuid) else { return false };
    let Some((to, engine)) = ({
        let registry = shared.registry();
        registry.least_loaded_engine().and_then(|id| registry.engine(id).map(|addr| (id, addr)))
    }) else {
        return false;
    };
    tracing::warn!(name = %who.profile.name, from = dead, to, "worker died; the player goes on on another engine");
    if let Err(e) = crate::minecraft::prepare_player(shared, to, uuid).await {
        tracing::warn!(error = e, "the player could not be taken in");
        return false;
    }
    match switch(r, w, link, client, to, &engine, who, &shared.forwarding_secret).await {
        Ok(_) => {
            shared.registry().move_session(session_id, to);
            true
        }
        Err((reason, _)) => {
            tracing::warn!(reason, "the player could not be taken in");
            false
        }
    }
}

/// How often a client's tab list is brought up to date with the other engines' players.
const TAB_LIST: Duration = Duration::from_secs(1);

/// The tab list's changes for a client on `worker_id`: the other engines' players added, those who left the
/// cluster removed. A player who came to the client's own engine is left to it: the engine lists it, and removing
/// it here would take the engine's entry away.
fn tab_list(shared: &Shared, tab: &mut HashSet<u128>, worker_id: u32, own: u128) -> Option<Vec<Vec<u8>>> {
    let players = shared.registry().cluster_players();
    let everyone: HashSet<u128> = players.iter().map(|p| p.0).collect();
    let remote: Vec<_> = players.into_iter().filter(|p| p.3 != worker_id && p.0 != own).collect();
    let added: Vec<play::PlayerInfo> = remote
        .iter()
        .filter(|p| !tab.contains(&p.0))
        .map(|(uuid, name, properties, _)| play::PlayerInfo {
            uuid: *uuid,
            name: Some(name.clone()),
            properties: properties.clone(),
            game_mode: Some(0),
            listed: Some(true),
            latency: Some(0),
            ..Default::default()
        })
        .collect();
    let gone: Vec<u128> = tab.iter().copied().filter(|u| !everyone.contains(u)).collect();
    // Remote players now, and players who moved to this engine (no longer IO's to show).
    tab.retain(|u| remote.iter().any(|p| p.0 == *u));
    let mut packets = Vec::new();
    if !added.is_empty() {
        tab.extend(added.iter().map(|p| p.uuid));
        // ADD_PLAYER, UPDATE_GAME_MODE, UPDATE_LISTED, UPDATE_LATENCY.
        let update = play::PlayerInfoUpdate { actions: 1 | 4 | 8 | 16, entries: added };
        packets.push(play::Clientbound::PlayerInfoUpdate(update).encode());
    }
    if !gone.is_empty() {
        packets.push(play::Clientbound::PlayerInfoRemove(play::PlayerInfoRemove { uuids: gone }).encode());
    }
    (!packets.is_empty()).then_some(packets)
}

/// The player a `/tp` or `/teleport` sends `own` to, by name: `tp <target>` or `tp <own> <target>`.
fn tp_target(command: &str, own: &str) -> Option<String> {
    let mut words = command.split_whitespace();
    if !matches!(words.next()?, "tp" | "teleport" | "minecraft:tp" | "minecraft:teleport") {
        return None;
    }
    let args: Vec<&str> = words.collect();
    match args.as_slice() {
        [target] => Some((*target).to_owned()),
        [who, target] if who.eq_ignore_ascii_case(own) || *who == "@s" => Some((*target).to_owned()),
        _ => None,
    }
}

/// `/list` over the whole cluster, as vanilla's ListPlayersCommand words it.
fn list_reply(shared: &Shared, max_players: u32, uuids: bool) -> Vec<u8> {
    let mut players = shared.registry().online();
    players.sort_by_key(|(_, name)| name.to_lowercase());
    let names: Vec<String> =
        players
            .iter()
            .map(|(uuid, name)| {
                if uuids { format!("{name} ({})", horizon_protocol::crypto::uuid_string(*uuid)) } else { name.clone() }
            })
            .collect();
    let (count, max, names) = (players.len().to_string(), max_players.to_string(), names.join(", "));
    let content = horizon_protocol::text::translatable("commands.list.players", &[&count, &max, &names]);
    play::Clientbound::SystemChat(play::SystemChat { content, overlay: false }).encode()
}

/// Moves the client from `link`'s engine to `engine`: returns the old link, or why not and whether the client is
/// still on the old engine.
#[allow(clippy::too_many_arguments)]
async fn switch(
    r: &mut Reader,
    w: &mut Writer,
    link: &mut Link,
    client: &mut Client,
    worker_id: u32,
    engine: &str,
    who: &Who,
    secret: &str,
) -> Result<Link, (String, bool)> {
    // Logged in first, so a new engine that refuses leaves the client where it was.
    let (mut new, _) = match login(engine, worker_id, who, secret).await {
        Ok(Ok(logged_in)) => logged_in,
        Ok(Err(_)) => return Err((format!("worker #{worker_id}'s engine refused the player"), true)),
        Err(e) => return Err((format!("{e:#}"), true)),
    };
    if new.r.threshold() != link.r.threshold() {
        return Err(("the engines compress differently".into(), true));
    }
    let mut hello = vec![login::Serverbound::LoginAcknowledged(Empty).encode()];
    hello.extend(client.information_packet());
    if let Err(e) = new.w.send_all(&hello).await {
        return Err((format!("{e:#}"), true));
    }
    // From here on the client leaves the old engine.
    let gone = |e: anyhow::Error| (format!("{e:#}"), false);
    w.send(&play::Clientbound::StartConfiguration(Empty).encode()).await.map_err(gone)?;
    // Until it acknowledges, the client still speaks play to the old engine (keep-alives, moves).
    let acknowledged = tokio::time::timeout(ACKNOWLEDGE, async {
        loop {
            let Some(frame) = r.next_frame().await? else { bail!("the client left") };
            if r.frame_id(&frame)? == play_sb::CONFIGURATION_ACKNOWLEDGED {
                return Ok(());
            }
            let _ = client.saw(r, &frame)?;
            // The old engine may be gone (a rescue): what the client still says to it is lost with it.
            let _ = link.w.send_contents(&[frame]).await;
        }
    })
    .await;
    match acknowledged {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(gone(e)),
        Err(_) => return Err(gone(anyhow::anyhow!("the client did not go back to configuration"))),
    }
    client.phase = Phase::Configuration;
    std::mem::swap(link, &mut new);
    tracing::info!(name = %who.profile.name, from = new.worker_id, to = worker_id, "player switched engines");
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_protocol::packets::configuration as cfg;
    use tokio::net::TcpListener;

    /// A stand-in engine: asks for forwarding and checks it with Horizon's own `verify`, compresses, logs the
    /// player in, then sends `hello` and echoes back the first packet the player sends.
    async fn fake_engine(listener: TcpListener, secret: String, hello: &'static str, said: oneshot::Sender<String>) {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, write) = stream.into_split();
        let (mut r, mut w) = (Reader::new(read), Writer::new(write));
        let (id, payload) = r.packet().await.unwrap().unwrap();
        assert_eq!(handshake::decode_intention(id, &payload, 255).unwrap().intent, handshake::LOGIN);
        let (id, payload) = r.packet().await.unwrap().unwrap();
        assert!(matches!(login::Serverbound::decode(id, &payload).unwrap(), login::Serverbound::Hello(_)));
        let query = login::CustomQuery {
            transaction: 1,
            channel: horizon_protocol::forwarding::CHANNEL.into(),
            data: horizon_protocol::forwarding::request(),
        };
        w.send(&login::Clientbound::CustomQuery(query).encode()).await.unwrap();
        let (id, payload) = r.packet().await.unwrap().unwrap();
        let login::Serverbound::CustomQueryAnswer(answer) = login::Serverbound::decode(id, &payload).unwrap() else {
            panic!("expected the forwarding answer")
        };
        let forwarded = horizon_protocol::forwarding::verify(secret.as_bytes(), &answer.payload.unwrap()).unwrap();
        w.send(&login::Clientbound::Compression(login::Compression { threshold: 64 }).encode()).await.unwrap();
        r.set_compression(64);
        w.set_compression(64);
        let profile = login::GameProfile { uuid: forwarded.uuid, name: forwarded.name.clone(), properties: vec![] };
        w.send(&login::Clientbound::LoginFinished(login::LoginFinished { profile, session: forwarded.uuid }).encode())
            .await
            .unwrap();
        let (id, payload) = r.packet().await.unwrap().unwrap();
        assert!(matches!(login::Serverbound::decode(id, &payload).unwrap(), login::Serverbound::LoginAcknowledged(_)));
        // A long packet, compressed on the wire.
        let brand = cfg::CustomPayload { channel: "minecraft:brand".into(), data: hello.repeat(40).into_bytes() };
        w.send(&cfg::Clientbound::CustomPayload(brand).encode()).await.unwrap();
        let (id, payload) = r.packet().await.unwrap().unwrap();
        let cfg::Serverbound::CustomPayload(echo) = cfg::Serverbound::decode(id, &payload).unwrap() else {
            panic!("expected the player's packet")
        };
        said.send(String::from_utf8(echo.data).unwrap()).unwrap();
        // Connected until IO closes it.
        while let Ok(Some(_)) = r.next().await {}
    }

    /// The client's side: logs in through IO, then reads the configuration packet of whichever engine it is on.
    struct Client {
        r: Reader,
        w: Writer,
    }

    impl Client {
        async fn brand(&mut self) -> String {
            loop {
                let (id, payload) = self.r.packet().await.unwrap().unwrap();
                if let Ok(cfg::Clientbound::CustomPayload(p)) = cfg::Clientbound::decode(id, &payload) {
                    return String::from_utf8(p.data).unwrap();
                }
            }
        }

        async fn say(&mut self, text: &str) {
            let p = cfg::CustomPayload { channel: "sculk:test".into(), data: text.as_bytes().to_vec() };
            self.w.send(&cfg::Serverbound::CustomPayload(p).encode()).await.unwrap();
        }
    }

    #[test]
    fn a_teleport_to_a_player_names_its_target() {
        assert_eq!(tp_target("tp Alex", "Steve").as_deref(), Some("Alex"));
        assert_eq!(tp_target("minecraft:teleport steve Alex", "Steve").as_deref(), Some("Alex"));
        assert_eq!(tp_target("tp @s Alex", "Steve").as_deref(), Some("Alex"));
        // Someone else sent, coordinates, or another command: the engine has it as it is.
        assert_eq!(tp_target("tp Bob Alex", "Steve"), None);
        assert_eq!(tp_target("tp 0 64 0", "Steve").as_deref(), None);
        assert_eq!(tp_target("give Alex dirt", "Steve"), None);
    }

    #[tokio::test]
    async fn a_player_moves_to_another_engine_through_configuration() {
        let shared = Shared::new(Duration::from_secs(1), "http://127.0.0.1:1").unwrap();
        let shared: &'static Arc<Shared> = Box::leak(Box::new(Arc::new(shared)));
        let (a, b) = (TcpListener::bind("127.0.0.1:0").await.unwrap(), TcpListener::bind("127.0.0.1:0").await.unwrap());
        let (a_addr, b_addr) = (a.local_addr().unwrap().to_string(), b.local_addr().unwrap().to_string());
        let ((said_a, engine_a), (said_b, engine_b)) = (oneshot::channel(), oneshot::channel());
        tokio::spawn(fake_engine(a, shared.forwarding_secret.clone(), "A", said_a));
        tokio::spawn(fake_engine(b, shared.forwarding_secret.clone(), "B", said_b));

        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(front.local_addr().unwrap()).await.unwrap();
        let (io_side, peer) = front.accept().await.unwrap();
        let (read, write) = io_side.into_split();
        let who = Who {
            profile: Profile::offline("Tester"),
            peer,
            protocol: 0,
            host: "sculk".into(),
            port: 25565,
            max_players: 20,
        };
        tokio::spawn(async move { proxy(Reader::new(read), Writer::new(write), who, 1, &a_addr, shared).await });

        // The client gets the engine's compression and login, as the engine sent them.
        let (read, write) = stream.into_split();
        let mut client = Client { r: Reader::new(read), w: Writer::new(write) };
        let (id, payload) = client.r.packet().await.unwrap().unwrap();
        assert_eq!(
            login::Clientbound::decode(id, &payload).unwrap(),
            login::Clientbound::Compression(login::Compression { threshold: 64 })
        );
        client.r.set_compression(64);
        client.w.set_compression(64);
        let (id, payload) = client.r.packet().await.unwrap().unwrap();
        assert!(matches!(login::Clientbound::decode(id, &payload).unwrap(), login::Clientbound::LoginFinished(_)));
        client.w.send(&login::Serverbound::LoginAcknowledged(Empty).encode()).await.unwrap();
        assert_eq!(client.brand().await, "A".repeat(40));
        client.say("to A").await;
        assert_eq!(engine_a.await.unwrap(), "to A");

        // Move: the client is sent back to configuration and, once it acknowledges, is on B.
        let session = shared.registry().session_of(horizon_protocol::crypto::offline_uuid("Tester")).unwrap();
        let commands = shared.registry().commands(session).unwrap();
        let (done, moved) = oneshot::channel();
        commands.send(Command::Move { worker_id: 2, engine: b_addr, done }).await.unwrap();
        let (id, _) = client.r.packet().await.unwrap().unwrap();
        assert_eq!(id, horizon_protocol::ids::play::clientbound::START_CONFIGURATION);
        client.w.send(&play::Serverbound::ConfigurationAcknowledged(Empty).encode()).await.unwrap();
        moved.await.unwrap().unwrap();
        assert_eq!(client.brand().await, "B".repeat(40));
        client.say("to B").await;
        assert_eq!(engine_b.await.unwrap(), "to B");
    }
}
