//! `sculk`: one binary, three roles (docs/docs/04-arkhitektura.md, "Один бінарник — три ролі").

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "sculk", version, about = "Distributed Minecraft: Java Edition server")]
struct Cli {
    #[command(subcommand)]
    role: Role,
}

#[derive(Subcommand)]
enum Role {
    /// Player entry point: connections, login, worker management. One per cluster.
    Io(sculk_io::Config),
    /// Game logic: ticks micro worlds. As many as you like.
    Worker(sculk_worker::Config),
    /// The database of worlds and chunk leases. One per cluster.
    Anvildb(sculk_anvildb::Config),
    /// Take Minecraft 26.3's registries and tags out of Mojang's server.jar into the game data folder.
    Prepare {
        /// A server.jar on disk; without it, the jar is downloaded from Mojang and its SHA-1 checked.
        #[arg(long)]
        server_jar: Option<std::path::PathBuf>,
        /// The output of Mojang's data generator (`java -DbundlerMainClass=net.minecraft.data.Main -jar server.jar
        /// --reports`): real block and item IDs. Without it built-in tags have no members.
        #[arg(long)]
        reports: Option<std::path::PathBuf>,
        /// Where the data goes; every role reads it from here.
        #[arg(long, env = "SCULK_GAME_DATA", default_value = "game-data")]
        out: std::path::PathBuf,
        /// Do nothing when `out` already holds usable data (for a container that prepares on every start).
        #[arg(long)]
        if_missing: bool,
        /// A Java 25 runtime: without `--reports`, Mojang's data generator is run with it first, so built-in IDs
        /// and tags are complete.
        #[arg(long, env = "SCULK_JAVA")]
        java: Option<std::path::PathBuf>,
    },
    /// Nether Tunnel, the player's side (docs/docs/11-nether-tunnel.md): point Minecraft at `--listen`, and the
    /// connection goes to the server over UDP, with faster recovery from lost packets than TCP's.
    Tunnel {
        /// The server's tunnel address (its SCULK_TUNNEL_LISTEN), host:port.
        #[arg(long)]
        server: String,
        /// Where Minecraft connects.
        #[arg(long, default_value = "127.0.0.1:25565")]
        listen: std::net::SocketAddr,
    },
    /// Puts AnvilDB's archived cold regions back in place, so its world folders are whole vanilla worlds again (to
    /// copy out). Run it while AnvilDB is stopped.
    Unarchive {
        #[arg(long, env = "SCULK_ANVILDB_DATA", default_value = "anvildb-data")]
        data_dir: std::path::PathBuf,
    },
    /// All three roles in this process, on their default addresses: for development and small servers. With
    /// SCULK_ENGINE and SCULK_RESOURCES set, workers run the vanilla game (Horizon's engine).
    Dev {
        /// Where Minecraft clients connect.
        #[arg(long, default_value = "0.0.0.0:25565")]
        listen: std::net::SocketAddr,
        /// How many workers to start.
        #[arg(long, default_value_t = 1)]
        workers: u32,
    },
}

/// Parses one role's flags with nothing given, so `dev` gets the same defaults (and environment) as the role itself.
fn defaults<T: clap::Args>() -> T {
    #[derive(Parser)]
    struct Wrap<T: clap::Args> {
        #[command(flatten)]
        inner: T,
    }
    Wrap::<T>::parse_from(["sculk"]).inner
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    // Workers stop their engines on Ctrl-C or SIGTERM (docker stop), so the world and players are saved first.
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let role = Cli::parse().role;
    let has_workers = matches!(role, Role::Worker(_) | Role::Dev { .. });
    let run = async {
        match role {
            Role::Io(c) => sculk_io::run(c).await,
            Role::Worker(c) => sculk_worker::run(c, stopping).await,
            Role::Anvildb(c) => sculk_anvildb::run(c).await,
            Role::Prepare { server_jar, reports, out, if_missing, java } => {
                if if_missing && sculk_data::GameData::load(&out).is_ok() {
                    tracing::info!(out = %out.display(), "game data already prepared");
                    return Ok(());
                }
                let source = match server_jar {
                    Some(path) => sculk_data::PrepareSource::Jar(path),
                    None => sculk_data::PrepareSource::Download,
                };
                tokio::task::spawn_blocking(move || {
                    sculk_data::prepare_with(source, reports.as_deref(), java.as_deref(), &out)
                })
                .await?
            }
            Role::Tunnel { server, listen } => sculk_tunnel::client(listen, server).await,
            Role::Unarchive { data_dir } => {
                let restored = sculk_anvildb::storage::RegionStore::new(&data_dir).restore_all()?;
                tracing::info!(restored, "archived regions put back");
                Ok(())
            }
            Role::Dev { listen, workers } => {
                let io = sculk_io::Config { listen, ..defaults() };
                let mut tasks = tokio::task::JoinSet::new();
                tasks.spawn(sculk_anvildb::run(defaults()));
                tasks.spawn(sculk_io::run(io));
                let mut workers_left = tokio::task::JoinSet::new();
                for i in 0..workers {
                    // Each worker's engine (SCULK_ENGINE) gets its own folder and port: engine, engine-2...
                    let mut worker: sculk_worker::Config = defaults();
                    // One console for the terminal: the first worker's engine.
                    worker.console = i == 0;
                    worker.drain = false;
                    if i > 0 {
                        let name = format!("{}-{}", worker.engine_dir.display(), i + 1);
                        worker.engine_dir = name.into();
                        worker.engine_port += i as u16;
                    }
                    workers_left.spawn(sculk_worker::run(worker, stopping.clone()));
                    if i == 0 {
                        // The console's worker registers first: IO gives it the lowest ID, which makes its engine the
                        // one players go to while nobody plays.
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
                // Every role runs until told to stop; the first to end takes the others with it. Workers end first
                // on a stop, while AnvilDB still takes their saves.
                let first = tokio::select! {
                    r = tasks.join_next() => r.expect("roles were spawned")?,
                    r = workers_left.join_next() => r.expect("workers were spawned")?,
                };
                // One worker stopped (`stop` on its console): the others save and stop too.
                let _ = stop.send(true);
                first?;
                while let Some(r) = workers_left.join_next().await {
                    r??;
                }
                Ok(())
            }
        }
    };
    tokio::pin!(run);

    tokio::select! {
        r = &mut run => r,
        _ = signal() => {
            tracing::info!("stopping");
            if !has_workers {
                return Ok(());
            }
            let _ = stop.send(true);
            // Workers save before they return; the other roles stop where they are.
            match tokio::time::timeout(std::time::Duration::from_secs(90), run).await {
                Ok(r) => r,
                Err(_) => {
                    tracing::warn!("did not stop in 90 s");
                    Ok(())
                }
            }
        }
    }
}

/// Ctrl-C, or SIGTERM where there is one (what `docker stop` sends).
async fn signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing the SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
