//! A worker that runs Horizon's game (`sculk-engine`) for its players: the full vanilla game instead of the built-in
//! flat world. IO proxies players straight to the engine with Velocity forwarding; the engine keeps its chunks in
//! AnvilDB under this worker's leases (sculk-engine's chunk store).
//!
//! The worker prepares the engine's server folder, starts it, and stops it when the worker loses IO.

use anyhow::{Context, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::process::{Child, Command};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// The `sculk-engine` executable.
    pub program: PathBuf,
    /// The engine's server folder: server.properties, eula.txt, logs.
    pub dir: PathBuf,
    /// Horizon's prepared resources (`horizon-prepare`).
    pub resources: PathBuf,
    /// The address the engine listens on; `0.0.0.0` in a container, where IO reaches it over the network.
    pub bind: String,
    pub port: u16,
    pub seed: String,
    /// A server.properties the owner writes for the whole cluster: its keys go into every engine's.
    pub properties: Option<PathBuf>,
    /// The owner accepted Minecraft's EULA by setting `SCULK_EULA=true` (in a container there is no eula.txt to
    /// edit by hand).
    pub eula: bool,
}

impl EngineConfig {
    /// What the worker tells IO. An unspecified bind address (`0.0.0.0`) makes IO use the worker's own IP.
    pub fn address(&self) -> String {
        format!("{}:{}", self.bind, self.port)
    }
}

/// Sets `keys` in a `server.properties` text, keeping every other line as it is.
pub fn set_properties(text: &str, keys: &BTreeMap<&str, String>) -> String {
    let mut left = keys.clone();
    let mut out: Vec<String> = text
        .lines()
        .map(|line| {
            let key = line.split_once('=').map(|(k, _)| k.trim());
            match key.and_then(|k| left.remove_entry(k)) {
                Some((k, v)) => format!("{k}={v}"),
                None => line.to_owned(),
            }
        })
        .collect();
    out.extend(left.into_iter().map(|(k, v)| format!("{k}={v}")));
    out.join("\n") + "\n"
}

/// Writes what the engine needs to take IO's players: the forwarding secret, and server.properties set for a
/// backend behind a proxy on loopback. The EULA is the owner's to accept, not Sculk's.
fn prepare(config: &EngineConfig, secret: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.dir)?;
    if config.eula {
        std::fs::write(
            config.dir.join("eula.txt"),
            "# Accepted by the server owner through SCULK_EULA=true.\neula=true\n",
        )?;
    }
    let eula = std::fs::read_to_string(config.dir.join("eula.txt")).unwrap_or_default();
    if !eula.lines().any(|l| l.trim() == "eula=true") {
        bail!(
            "accept Minecraft's EULA (https://aka.ms/MinecraftEULA) for the engine: put `eula=true` in {}, or set SCULK_EULA=true",
            config.dir.join("eula.txt").display()
        );
    }
    std::fs::write(config.dir.join("forwarding.secret"), secret)?;
    let path = config.dir.join("server.properties");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // A new engine starts from vanilla's defaults where Horizon's differ: a vanilla server lets anyone in.
        Err(_) => set_properties("", &BTreeMap::from([("white-list", "false".to_owned())])),
    };
    // The cluster's own settings (gamemode, difficulty, pvp...), the same on every worker.
    let shared = match &config.properties {
        Some(file) => match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %file.display(), "no cluster server.properties; the engine keeps its own");
                String::new()
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
        },
        None => String::new(),
    };
    let mut keys: BTreeMap<&str, String> = shared
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim().to_owned()))
        .collect();
    keys.extend([
        // IO authenticates players; the engine trusts its forwarding (docs/velocity-forwarding.md in Horizon).
        ("online-mode", "false".to_owned()),
        ("server-ip", config.bind.clone()),
        ("server-port", config.port.to_string()),
        // Every worker generates the same world.
        ("level-seed", config.seed.clone()),
        // IO counts the cluster's players against its own limit; an engine takes whoever IO sends.
        ("max-players", "10000".to_owned()),
        // The same on every engine: IO passes compressed frames from one engine's player to another engine as
        // they are when it moves the player.
        ("network-compression-threshold", "256".to_owned()),
    ]);
    std::fs::write(path, set_properties(&text, &keys))?;
    // The server's lists beside the cluster's server.properties seed a new engine's; AnvilDB's copies, pulled after
    // this, win once there are any.
    if let Some(config_dir) = config.properties.as_ref().and_then(|p| p.parent()) {
        for list in sculk_anvildb::files::SERVER_LISTS {
            let (from, to) = (config_dir.join(list), config.dir.join(list));
            if from.exists() && !to.exists() {
                std::fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
            }
        }
    }
    // Extensions, the same on every engine so a player plays by the same rules wherever it moves: vanilla's
    // datapacks and Horizon's content mods (docs/mods.md in Horizon), from beside the cluster's server.properties.
    if let Some(config_dir) = config.properties.as_ref().and_then(|p| p.parent()) {
        let level = world_dir(config);
        for (from, to) in [
            (config_dir.join("datapacks"), level.join("datapacks")),
            (config_dir.join("horizon-mods"), config.dir.join("horizon-mods")),
            (config_dir.join("horizon-native-mods"), config.dir.join("horizon-native-mods")),
        ] {
            if from.is_dir() {
                mirror(&from, &to).with_context(|| format!("copying {}", from.display()))?;
            }
        }
        let switches = config_dir.join("horizon-mods.txt");
        if switches.is_file() {
            std::fs::copy(&switches, config.dir.join("horizon-mods.txt"))?;
        }
    }
    // Horizon's own additions to vanilla stay off unless the owner turns them on in server.toml.
    let toml = config.dir.join("server.toml");
    if !toml.exists() {
        std::fs::write(
            toml,
            "# Horizon's additions to vanilla, off as on a vanilla server; set enabled = true to use one.\n\
             [chat_filter]\nenabled = false\n\n[anticheat]\nenabled = false\n",
        )?;
    }
    Ok(())
}

/// A running engine. Dropping it kills the process; [`stop`](Engine::stop) lets it save first.
pub struct Engine {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
}

/// How long the engine may take to save and stop before it is killed.
const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl Engine {
    /// Waits for the engine to exit by itself.
    pub async fn exited(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// Runs a console command, as typed into a vanilla server's console.
    pub async fn command(&mut self, line: &str) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let Some(stdin) = &mut self.stdin else { return Err(std::io::ErrorKind::BrokenPipe.into()) };
        stdin.write_all(format!("{line}\n").as_bytes()).await?;
        stdin.flush().await
    }

    /// Stops the engine as vanilla's `stop` does: players are saved and kicked, the world saved. Killed if it takes
    /// longer than STOP_TIMEOUT.
    pub async fn stop(mut self) {
        if self.command("stop").await.is_ok()
            && let Ok(status) = tokio::time::timeout(STOP_TIMEOUT, self.child.wait()).await
        {
            tracing::info!(?status, "engine stopped");
            return;
        }
        tracing::warn!("engine did not stop in time; killing it");
        let _ = self.child.kill().await;
    }
}

/// The engine's world folder, where it keeps level.dat and the players' files (its chunks are in AnvilDB).
pub fn world_dir(config: &EngineConfig) -> PathBuf {
    let text = std::fs::read_to_string(config.dir.join("server.properties")).unwrap_or_default();
    let name = text
        .lines()
        .find_map(|l| l.strip_prefix("level-name=").map(|v| v.trim().to_owned()))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "world".into());
    config.dir.join(name)
}

/// Writes the engine's server folder for `worker_id` without starting it.
pub fn prepare_dir(config: &EngineConfig, secret: &str) -> anyhow::Result<()> {
    prepare(config, secret)
}

/// The engine's control endpoint file, in its server folder.
pub fn control_endpoint(config: &EngineConfig) -> anyhow::Result<PathBuf> {
    Ok(absolute(&config.dir)?.join(".horizon").join("engine.json"))
}

/// How the engine takes players handed over from another engine (Horizon's `[transfer]`, docs/server.md): IO
/// moves players between engines when their micro worlds meet. A stopping engine does not wait for IO to move its
/// players out: they are saved, as on vanilla's stop.
const TRANSFERS: &str = "expect_seconds=15,max_freeze_seconds=30,max_hold_seconds=60,stop_wait_seconds=0";

/// Starts the engine for `worker_id`. The child dies with the returned handle.
pub fn start(config: &EngineConfig, worker_id: u32, io: &str, anvildb: &str, world: &str) -> anyhow::Result<Engine> {
    let endpoint = control_endpoint(config)?;
    if let Some(dir) = endpoint.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The endpoint holds the token that moves players: only this user may read it.
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let _ = std::fs::remove_file(&endpoint);
    // The game's own options come before `--dedicated`; everything after it is the dedicated server's.
    let mut child = Command::new(&config.program)
        .arg("--resources")
        .arg(absolute(&config.resources)?)
        .arg("--control-socket")
        .arg(&endpoint)
        .args(["--transfers", TRANSFERS])
        .args(["--dedicated", "--nogui"])
        .current_dir(&config.dir)
        .env("SCULK_WORKER_ID", worker_id.to_string())
        .env("SCULK_ANVILDB", anvildb)
        .env("SCULK_IO", io)
        .env("SCULK_WORLD", world)
        .env("HORIZON_NO_AUDIO", "1")
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", config.program.display()))?;
    tracing::info!(worker_id, address = %config.address(), pid = child.id(), "engine started");
    let stdin = child.stdin.take();
    Ok(Engine { child, stdin })
}

/// Makes `to` a copy of `from`: files copied over, files `from` no longer has removed.
fn mirror(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        std::fs::remove_dir_all(to)?;
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            mirror(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn absolute(path: &Path) -> anyhow::Result<PathBuf> {
    std::path::absolute(path).with_context(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_keep_other_lines_and_add_missing_keys() {
        let text = "#Minecraft server properties\nmotd=Hi\nonline-mode=true\nserver-port=25565\n";
        let keys = BTreeMap::from([
            ("online-mode", "false".to_owned()),
            ("server-port", "25601".to_owned()),
            ("level-seed", "42".to_owned()),
        ]);
        assert_eq!(
            set_properties(text, &keys),
            "#Minecraft server properties\nmotd=Hi\nonline-mode=false\nserver-port=25601\nlevel-seed=42\n"
        );
    }

    #[test]
    fn the_clusters_extensions_reach_every_engine() {
        let cluster = tempfile::tempdir().unwrap();
        std::fs::write(cluster.path().join("server.properties"), "difficulty=hard\n").unwrap();
        let pack = cluster.path().join("datapacks/sculk/data/sculk/function");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join("hello.mcfunction"), "say hello\n").unwrap();
        std::fs::create_dir_all(cluster.path().join("horizon-mods")).unwrap();
        std::fs::write(cluster.path().join("horizon-mods/pipes.hmod"), b"mod").unwrap();
        std::fs::write(cluster.path().join("horizon-mods.txt"), "mod.pipes:true\n").unwrap();
        let engine = tempfile::tempdir().unwrap();
        // A pack the cluster no longer has goes from the engine too.
        std::fs::create_dir_all(engine.path().join("world/datapacks/old")).unwrap();
        let config = EngineConfig {
            program: "sculk-engine".into(),
            dir: engine.path().into(),
            resources: "res".into(),
            bind: "127.0.0.1".into(),
            port: 25601,
            seed: "1".into(),
            properties: Some(cluster.path().join("server.properties")),
            eula: true,
        };
        prepare(&config, "s").unwrap();
        let e = engine.path();
        assert!(e.join("world/datapacks/sculk/data/sculk/function/hello.mcfunction").exists());
        assert!(!e.join("world/datapacks/old").exists());
        assert_eq!(std::fs::read(e.join("horizon-mods/pipes.hmod")).unwrap(), b"mod");
        assert_eq!(std::fs::read_to_string(e.join("horizon-mods.txt")).unwrap(), "mod.pipes:true\n");
        assert!(std::fs::read_to_string(e.join("server.properties")).unwrap().contains("difficulty=hard"));
    }

    #[test]
    fn the_eula_is_never_accepted_for_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            program: "sculk-engine".into(),
            dir: dir.path().into(),
            resources: "res".into(),
            bind: "127.0.0.1".into(),
            port: 25601,
            seed: "1".into(),
            properties: None,
            eula: false,
        };
        assert!(prepare(&config, "s").unwrap_err().to_string().contains("eula=true"));
        assert!(!dir.path().join("eula.txt").exists());
        std::fs::write(dir.path().join("eula.txt"), "eula=true\n").unwrap();
        prepare(&config, "s").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("forwarding.secret")).unwrap(), "s");
        let other = tempfile::tempdir().unwrap();
        prepare(&EngineConfig { dir: other.path().into(), eula: true, ..config.clone() }, "s").unwrap();
        assert!(std::fs::read_to_string(dir.path().join("server.properties")).unwrap().contains("online-mode=false"));
    }
}
