//! The engine's control endpoint (Horizon's docs/server.md, "a supervisor and an engine"): a local TCP port and a
//! token the engine writes to a file, then one request line in and one JSON line out. IO moves players between
//! engines through it (`player-expect`, `player-export`, `player-import`, `player-release`...).

use anyhow::{Context, bail};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// How long one request may take.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct EngineControl {
    /// The endpoint file: `{"transport":"tcp","port":N,"token":"..."}`.
    endpoint: PathBuf,
}

impl EngineControl {
    pub fn new(endpoint: PathBuf) -> Self {
        Self { endpoint }
    }

    /// Sends one request and returns the engine's JSON answer as it came.
    pub async fn request(&self, line: &str) -> anyhow::Result<String> {
        tokio::time::timeout(TIMEOUT, self.ask(line)).await.context("the engine did not answer in time")?
    }

    async fn ask(&self, line: &str) -> anyhow::Result<String> {
        let text = tokio::fs::read_to_string(&self.endpoint)
            .await
            .with_context(|| format!("the engine's control endpoint {}", self.endpoint.display()))?;
        let endpoint: serde_json::Value = serde_json::from_str(&text)?;
        let port = endpoint["port"].as_u64().and_then(|p| u16::try_from(p).ok()).context("no port")?;
        let token = endpoint["token"].as_str().context("no token")?;
        let stream = TcpStream::connect(("127.0.0.1", port)).await?;
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        write.write_all(format!("auth {token}\n").as_bytes()).await?;
        let mut reply = String::new();
        read.read_line(&mut reply).await?;
        if !reply.contains("\"ok\"") {
            bail!("the engine refused the token: {}", reply.trim());
        }
        // A request is one line: a newline in it would be a second request.
        write.write_all(format!("{}\n", line.replace(['\n', '\r'], " ")).as_bytes()).await?;
        reply.clear();
        read.read_line(&mut reply).await?;
        Ok(reply.trim().to_owned())
    }
}
