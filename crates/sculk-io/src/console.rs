//! The web console for debugging a cluster (PLAN.md, block H; the idea of IWM's `web-console.html`): a map of the
//! micro worlds' zones (every leased chunk, coloured by the worker that holds it) with the players on it, and the
//! workers' load. `GET /` is the page, `GET /api/state` what it draws. Plain HTTP/1.1 on IO's runtime: it is a
//! tool for the cluster's owner, so it listens on loopback unless told otherwise.

use crate::control::Shared;
use sculk_proto::anvildb::{ListLeasesRequest, leases_client::LeasesClient};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tonic::transport::Channel;

const PAGE: &str = include_str!("console.html");

pub async fn serve(listen: SocketAddr, shared: Arc<Shared>, anvildb: &str, world: String) -> anyhow::Result<()> {
    let leases = LeasesClient::new(tonic::transport::Endpoint::from_shared(anvildb.to_owned())?.connect_lazy());
    let listener = TcpListener::bind(listen).await?;
    tracing::info!(%listen, "web console listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let (shared, leases, world) = (shared.clone(), leases.clone(), world.clone());
        tokio::spawn(async move {
            if let Err(e) = answer(stream, &shared, leases, &world).await {
                tracing::debug!(error = %e, "web console request failed");
            }
        });
    }
}

async fn answer(
    mut stream: TcpStream,
    shared: &Arc<Shared>,
    leases: LeasesClient<Channel>,
    world: &str,
) -> anyhow::Result<()> {
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await?;
        if n == 0 || request.len() > 64 << 10 {
            return Ok(());
        }
        request.extend_from_slice(&buf[..n]);
    }
    let line = String::from_utf8_lossy(&request);
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let (status, kind, body) = match path {
        "/" => ("200 OK", "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
        "/api/state" => ("200 OK", "application/json", state(shared, leases, world).await.to_string().into_bytes()),
        _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

/// What the page draws: workers, players (with where their engines say they are) and leased chunks.
async fn state(shared: &Arc<Shared>, mut leases: LeasesClient<Channel>, world: &str) -> Value {
    let workers = shared.registry().workers_summary();
    let mut positions = std::collections::HashMap::new();
    for w in workers.iter().filter(|w| w["engine"].is_string() && w["sessions"].as_u64() > Some(0)) {
        let Some(id) = w["id"].as_u64() else { continue };
        if let Ok(status) = shared.engine_command(id as u32, "server-status").await {
            for p in status["players"].as_array().into_iter().flatten() {
                if let Some(uuid) = p["uuid"].as_str().and_then(crate::handover::parse_uuid) {
                    positions.insert(uuid, (p["dimension"].clone(), p["position"].clone()));
                }
            }
        }
    }
    let players: Vec<Value> = shared
        .registry()
        .cluster_players()
        .into_iter()
        .map(|(uuid, name, _, worker)| {
            let (dimension, position) = positions.remove(&uuid).unwrap_or((Value::Null, Value::Null));
            json!({ "name": name, "uuid": horizon_protocol::crypto::uuid_string(uuid), "worker": worker,
                    "dimension": dimension, "position": position })
        })
        .collect();
    let chunks: Vec<Value> = match leases.list(ListLeasesRequest { world: world.to_owned() }).await {
        Ok(list) => list
            .into_inner()
            .leases
            .into_iter()
            .filter_map(|l| {
                let (chunk, holder) = (l.chunk?, l.holder?);
                Some(json!([holder.worker_id, chunk.dimension, chunk.x, chunk.z]))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    json!({ "world": world, "workers": workers, "players": players, "chunks": chunks })
}
