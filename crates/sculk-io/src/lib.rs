//! IO: the cluster's only entry point (docs/docs/05-io-server.md). It holds client connections and manages workers,
//! and runs no game logic.

mod auth;
mod cluster;
mod conn;
mod console;
mod control;
mod handover;
mod minecraft;
mod proxy;
mod registry;

use anyhow::Context;
use control::{ControlService, Shared};
use sculk_proto::cluster::io_server::IoServer;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Placement {
    Spread,
    Single,
}

#[derive(Debug, Clone, clap::Args)]
pub struct Config {
    /// Where Minecraft clients connect.
    #[arg(long, env = "SCULK_LISTEN", default_value = "0.0.0.0:25565")]
    pub listen: SocketAddr,
    /// gRPC address workers connect to.
    #[arg(long = "io-control-listen", env = "SCULK_IO_CONTROL_LISTEN", default_value = "127.0.0.1:7100")]
    pub control_listen: SocketAddr,
    /// AnvilDB, to free a dead worker's leases.
    #[arg(long = "anvildb", env = "SCULK_ANVILDB", default_value = "http://127.0.0.1:7200")]
    pub anvildb: String,
    /// The server list's message.
    #[arg(long, env = "SCULK_MOTD", default_value = "A Minecraft Server")]
    pub motd: String,
    /// The server list's picture: a 64x64 PNG, as vanilla's server-icon.png. Missing is fine.
    #[arg(long, env = "SCULK_ICON", default_value = "server-icon.png")]
    pub icon: std::path::PathBuf,
    #[arg(long, env = "SCULK_MAX_PLAYERS", default_value_t = 10_000)]
    pub max_players: u32,
    /// How often workers report; three missed reports and IO counts the worker as dead.
    #[arg(long, env = "SCULK_HEARTBEAT_MS", default_value_t = 1_000)]
    pub heartbeat_ms: u64,
    /// Packets of this many bytes or more are compressed (vanilla's network-compression-threshold); -1 turns it off.
    #[arg(long, env = "SCULK_COMPRESSION_THRESHOLD", default_value_t = 256)]
    pub compression_threshold: i32,
    /// The world in AnvilDB.
    #[arg(long, env = "SCULK_WORLD", default_value = "world")]
    pub world: String,
    /// Check players with Mojang's session server and encrypt their connections, as vanilla's `online-mode`.
    /// Off for offline servers, bots and servers behind a proxy that checks players itself.
    #[arg(long, env = "SCULK_ONLINE_MODE", default_value_t = false, action = clap::ArgAction::Set)]
    pub online_mode: bool,
    /// Where a player goes when nobody plays around the spawn: `spread` puts it on the engine with the fewest
    /// players (their micro worlds merge onto one engine when they meet), `single` keeps every player on one engine.
    #[arg(long, env = "SCULK_PLACEMENT", value_enum, default_value_t = Placement::Spread)]
    pub placement: Placement,
    /// A micro world splits when the zones of its players' groups stay this many chunks apart (docs/docs/03)...
    #[arg(long, env = "SCULK_SPLIT_GAP", default_value_t = 8)]
    pub split_gap: i32,
    /// ...for this many seconds.
    #[arg(long, env = "SCULK_SPLIT_AFTER_SECS", default_value_t = 300)]
    pub split_after_secs: u64,
    /// ...or at once when its engine takes longer than this many milliseconds a tick (vanilla's budget is 50).
    #[arg(long, env = "SCULK_SPLIT_OVERLOAD_MSPT", default_value_t = 45.0)]
    pub split_overload_mspt: f32,
    /// The radius of a player's zone in chunks: the engines' view distance.
    #[arg(long, env = "SCULK_VIEW_DISTANCE", default_value_t = 10)]
    pub view_distance: i32,
    /// Nether Tunnel's UDP address (docs/docs/11-nether-tunnel.md): players running `sculk tunnel` connect here.
    /// Empty turns it off.
    #[arg(long, env = "SCULK_TUNNEL_LISTEN", default_value = "")]
    pub tunnel_listen: String,
    /// The web console (a map of the micro worlds, leases and workers); empty turns it off. Loopback by default:
    /// it shows where every player is.
    #[arg(long, env = "SCULK_CONSOLE_LISTEN", default_value = "127.0.0.1:7180")]
    pub console_listen: String,
    /// What `sculk prepare` wrote.
    #[arg(long, env = "SCULK_GAME_DATA", default_value = "game-data")]
    pub game_data: std::path::PathBuf,
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let shared = Arc::new(Shared::new(Duration::from_millis(config.heartbeat_ms), &config.anvildb)?);
    let data = Arc::new(sculk_data::GameData::load(&config.game_data)?);
    let settings = Arc::new(minecraft::Settings {
        motd: config.motd.clone(),
        max_players: config.max_players,
        world: config.world.clone(),
        compression_threshold: config.compression_threshold,
        data,
        favicon: minecraft::favicon(&config.icon),
        placement: config.placement,
        keys: if config.online_mode { Some(auth::Keys::generate()?) } else { None },
    });

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding the Minecraft port {}", config.listen))?;
    tracing::info!(listen = %config.listen, control = %config.control_listen, online_mode = config.online_mode, "IO listening");

    tokio::spawn(shared.clone().watchdog());
    tokio::spawn(shared.clone().sync_clocks());
    if !config.tunnel_listen.is_empty() {
        let listen: SocketAddr = config.tunnel_listen.parse().context("SCULK_TUNNEL_LISTEN")?;
        let socket =
            tokio::net::UdpSocket::bind(listen).await.with_context(|| format!("binding the tunnel {listen}"))?;
        tracing::info!(%listen, "Nether Tunnel listening");
        tokio::spawn(minecraft::serve_tunnel(socket, shared.clone(), settings.clone()));
    }
    if !config.console_listen.is_empty() {
        let listen: SocketAddr = config.console_listen.parse().context("SCULK_CONSOLE_LISTEN")?;
        let (shared, anvildb, world) = (shared.clone(), config.anvildb.clone(), config.world.clone());
        tokio::spawn(async move {
            if let Err(e) = console::serve(listen, shared, &anvildb, world).await {
                tracing::error!(error = format!("{e:#}"), "web console stopped");
            }
        });
    }
    tokio::spawn(shared.clone().split_micro_worlds(handover::Split {
        gap: config.split_gap,
        after: Duration::from_secs(config.split_after_secs),
        view_distance: config.view_distance,
        overload_mspt: config.split_overload_mspt,
    }));
    let control = tonic::transport::Server::builder()
        .add_service(IoServer::new(ControlService::new(shared.clone())))
        .serve(config.control_listen);

    tokio::select! {
        r = control => r.context("IO control gRPC server"),
        r = minecraft::serve(listener, shared, settings) => r.context("Minecraft listener"),
    }
}
