//! Worker: ticks many micro worlds at once (docs/docs/06-worker.md). It knows nothing about the world as a whole,
//! only the micro worlds IO gave it.

mod control;
mod engine;
mod flat;
mod mw;
mod play;
mod session;
mod worldfiles;

use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, clap::Args)]
pub struct Config {
    /// IO's control address.
    #[arg(long = "io", env = "SCULK_IO", default_value = "http://127.0.0.1:7100")]
    pub io: String,
    #[arg(long = "anvildb", env = "SCULK_ANVILDB", default_value = "http://127.0.0.1:7200")]
    pub anvildb: String,
    /// The world in AnvilDB.
    #[arg(long, env = "SCULK_WORLD", default_value = "world")]
    pub world: String,
    /// Radius of a player's zone, in chunks.
    #[arg(long, env = "SCULK_VIEW_DISTANCE", default_value_t = 10)]
    pub view_distance: u32,
    /// How often leases are renewed; keep it well under AnvilDB's lease TTL.
    #[arg(long, env = "SCULK_LEASE_RENEW_MS", default_value_t = 3_000)]
    pub lease_renew_ms: u64,
    /// What `sculk prepare` wrote.
    #[arg(long, env = "SCULK_GAME_DATA", default_value = "game-data")]
    pub game_data: std::path::PathBuf,
    /// Run Horizon's game for this worker's players: the `sculk-engine` executable. Without it the worker plays
    /// its built-in flat world.
    #[arg(long, env = "SCULK_ENGINE", requires = "resources")]
    pub engine: Option<std::path::PathBuf>,
    /// Horizon's prepared resources for the engine (`horizon-prepare`).
    #[arg(long, env = "SCULK_RESOURCES")]
    pub resources: Option<std::path::PathBuf>,
    /// The engine's server folder (server.properties, eula.txt, logs).
    #[arg(long, env = "SCULK_ENGINE_DIR", default_value = "engine")]
    pub engine_dir: std::path::PathBuf,
    /// The address the engine listens on: loopback beside IO, `0.0.0.0` in a container (IO then reaches it at
    /// this worker's IP).
    #[arg(long, env = "SCULK_ENGINE_BIND", default_value = "127.0.0.1")]
    pub engine_bind: String,
    /// The port IO reaches the engine on.
    #[arg(long, env = "SCULK_ENGINE_PORT", default_value_t = 25601)]
    pub engine_port: u16,
    /// The owner accepts Minecraft's EULA (https://aka.ms/MinecraftEULA) for the engine.
    #[arg(long, env = "SCULK_EULA", default_value_t = false)]
    pub eula: bool,
    /// The world seed; every worker of a cluster must use the same one.
    #[arg(long, env = "SCULK_SEED", default_value = "sculk")]
    pub seed: String,
    /// The cluster's server.properties (gamemode, difficulty, pvp, spawn-protection...): its keys go into every
    /// engine's, so all workers play by the same rules.
    #[arg(long, env = "SCULK_PROPERTIES")]
    pub properties: Option<std::path::PathBuf>,
    /// Lines typed on this process's standard input go to the engine's console (`op`, `whitelist`, `say`...), as on
    /// a vanilla server. Off for all but one worker of `sculk dev`.
    #[arg(skip = true)]
    pub console: bool,
    /// On a stop signal, have IO move the engine's players to other engines first, so a restart disconnects
    /// nobody. Off in `sculk dev`, where every worker stops at once.
    #[arg(skip = true)]
    pub drain: bool,
}

impl Config {
    fn engine(&self) -> Option<engine::EngineConfig> {
        Some(engine::EngineConfig {
            program: self.engine.clone()?,
            dir: self.engine_dir.clone(),
            resources: self.resources.clone()?,
            bind: self.engine_bind.clone(),
            port: self.engine_port,
            seed: self.seed.clone(),
            properties: self.properties.clone(),
            eula: self.eula,
        })
    }
}

/// Runs until `stop` turns true, reconnecting to IO whenever the stream ends. On stop the engine saves the world
/// and its players before the worker returns.
pub async fn run(config: Config, mut stop: tokio::sync::watch::Receiver<bool>) -> anyhow::Result<()> {
    let world = Arc::new(session::world(&sculk_data::GameData::load(&config.game_data)?)?);
    let (lines, mut console) = tokio::sync::mpsc::channel(64);
    if config.console && config.engine.is_some() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = stdin.next_line().await {
                if lines.send(line).await.is_err() {
                    break;
                }
            }
        });
    }
    let config = Arc::new(config);
    loop {
        let result = session::run(config.clone(), world.clone(), &mut stop, &mut console).await;
        let stopped = *stop.borrow();
        if stopped {
            return Ok(());
        }
        match result {
            Ok(session::Ended::Stopped) => return Ok(()),
            Ok(session::Ended::Lost) => tracing::warn!("IO closed the stream; reconnecting"),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "lost IO or AnvilDB; reconnecting"),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = stop.wait_for(|s| *s) => return Ok(()),
        }
    }
}
