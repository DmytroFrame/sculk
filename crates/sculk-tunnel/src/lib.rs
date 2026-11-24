//! Nether Tunnel (docs/docs/11-nether-tunnel.md): an optional program for players for whom latency matters. It
//! listens on a local address, takes the Minecraft client's TCP connection there, and carries it to IO over UDP
//! with its own reliable ordered transport (`conn`) and compression (`wire`). IO unwraps it and handles it as any
//! other connection. TCP stays the way everyone plays; the tunnel only helps on lossy or distant paths.

pub mod conn;
pub mod wire;

use anyhow::{Context, bail};
use conn::{Conn, WINDOW};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::Instant;
use wire::{Kind, Segment};

/// A session nobody heard from this long ends (Minecraft's keep-alives come every 15 s).
const IDLE: Duration = Duration::from_secs(60);
/// How often a session wakes up with nothing else to do.
const TICK: Duration = Duration::from_millis(50);
/// How many times the client says hello before it gives up.
const HELLOS: u32 = 20;

/// Where a session's datagrams go: a connected socket (the client), or a peer of the shared one (the server).
#[derive(Clone)]
struct Link {
    socket: Arc<UdpSocket>,
    peer: Option<SocketAddr>,
}

impl Link {
    async fn send(&self, segment: &Segment) {
        let bytes = segment.encode();
        let _ = match self.peer {
            Some(peer) => self.socket.send_to(&bytes, peer).await,
            None => self.socket.send(&bytes).await,
        };
    }
}

/// Carries one session between its datagrams and the byte stream `app` until either side ends.
async fn drive(session: u64, mut incoming: mpsc::Receiver<Segment>, link: Link, app: impl AsyncRead + AsyncWrite) {
    let (mut app_read, mut app_write) = tokio::io::split(app);
    let mut conn = Conn::new(session, std::time::Instant::now());
    let mut buf = vec![0u8; 16 * 1024];
    let mut app_done = false;
    loop {
        let wake = conn
            .next_timeout()
            .map(Instant::from_std)
            .unwrap_or_else(|| Instant::now() + TICK)
            .min(Instant::now() + TICK);
        tokio::select! {
            segment = incoming.recv() => match segment {
                Some(segment) if segment.kind == Kind::Close => break,
                Some(segment) => conn.receive(std::time::Instant::now(), segment),
                None => break,
            },
            // Read from the application only as fast as the peer acknowledges.
            n = app_read.read(&mut buf), if !app_done && conn.backlog() < WINDOW * 2 => match n {
                Ok(0) | Err(_) => app_done = true,
                Ok(n) => conn.send(&buf[..n]),
            },
            _ = tokio::time::sleep_until(wake) => {}
        }
        let now = std::time::Instant::now();
        for segment in conn.poll(now) {
            link.send(&segment).await;
        }
        let delivered = conn.take_delivered();
        if !delivered.is_empty() && app_write.write_all(&delivered).await.is_err() {
            break;
        }
        if now.duration_since(conn.last_heard) > IDLE || (app_done && conn.backlog() == 0) {
            break;
        }
    }
    link.send(&Segment::control(Kind::Close, session)).await;
    let _ = app_write.shutdown().await;
    tracing::debug!(session, retransmitted = conn.retransmitted, "tunnel session ended");
}

/// The player's side: takes Minecraft clients on `listen` and carries each over its own tunnel session to `server`
/// (IO's tunnel address).
pub async fn client(listen: SocketAddr, server: String) -> anyhow::Result<()> {
    let listener = TcpListener::bind(listen).await.with_context(|| format!("listening on {listen}"))?;
    tracing::info!(%listen, %server, "Nether Tunnel: point Minecraft at the local address");
    loop {
        let (tcp, from) = listener.accept().await?;
        let _ = tcp.set_nodelay(true);
        let server = server.clone();
        tokio::spawn(async move {
            if let Err(e) = client_session(tcp, &server).await {
                tracing::warn!(%from, error = format!("{e:#}"), "tunnel session failed");
            }
        });
    }
}

async fn client_session(tcp: TcpStream, server: &str) -> anyhow::Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect(server).await.with_context(|| format!("reaching {server}"))?;
    let socket = Arc::new(socket);
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).map_err(|e| anyhow::anyhow!("no randomness for the session: {e}"))?;
    let session = u64::from_le_bytes(id);
    let hello = Segment::control(Kind::Hello, session).encode();
    let mut buf = vec![0u8; 2048];
    let mut welcomed = false;
    for _ in 0..HELLOS {
        socket.send(&hello).await?;
        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(250), socket.recv(&mut buf)).await
            && Segment::decode(&buf[..n]).is_some_and(|s| s.kind == Kind::Welcome && s.session == session)
        {
            welcomed = true;
            break;
        }
    }
    if !welcomed {
        bail!("{server} did not answer: is IO's tunnel on (SCULK_TUNNEL_LISTEN)?");
    }
    let (to_session, incoming) = mpsc::channel(4096);
    let reader = socket.clone();
    let receive = tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        while let Ok(n) = reader.recv(&mut buf).await {
            if let Some(segment) = Segment::decode(&buf[..n]).filter(|s| s.session == session)
                && to_session.send(segment).await.is_err()
            {
                break;
            }
        }
    });
    drive(session, incoming, Link { socket, peer: None }, tcp).await;
    receive.abort();
    Ok(())
}

/// IO's side: takes tunnel sessions on `socket` and hands each to `on_session` as a byte stream, with the player's
/// address, as a TCP connection would be.
pub async fn serve(socket: UdpSocket, on_session: impl Fn(DuplexStream, SocketAddr) + Send + Sync + 'static) {
    let socket = Arc::new(socket);
    let mut sessions: HashMap<u64, (SocketAddr, mpsc::Sender<Segment>)> = HashMap::new();
    let (ended, mut ended_rx) = mpsc::unbounded_channel::<u64>();
    let mut buf = vec![0u8; 2048];
    loop {
        let (n, peer) = tokio::select! {
            received = socket.recv_from(&mut buf) => match received {
                Ok(received) => received,
                Err(_) => continue,
            },
            Some(session) = ended_rx.recv() => {
                sessions.remove(&session);
                continue;
            }
        };
        let Some(segment) = Segment::decode(&buf[..n]) else { continue };
        let session = segment.session;
        match (segment.kind, sessions.get(&session)) {
            (Kind::Hello, Some(_)) => {}
            (Kind::Hello, None) => {
                let (ours, theirs) = tokio::io::duplex(256 * 1024);
                let (to_session, incoming) = mpsc::channel(4096);
                sessions.insert(session, (peer, to_session));
                let (link, ended) = (Link { socket: socket.clone(), peer: Some(peer) }, ended.clone());
                tokio::spawn(async move {
                    drive(session, incoming, link, ours).await;
                    let _ = ended.send(session);
                });
                on_session(theirs, peer);
            }
            // Only the address that opened a session speaks for it.
            (_, Some((from, to_session))) if *from == peer => {
                let _ = to_session.try_send(segment);
                continue;
            }
            _ => {
                let _ = socket.send_to(&Segment::control(Kind::Close, session).encode(), peer).await;
                continue;
            }
        }
        // A hello, first or repeated: welcomed, in case the last welcome was lost.
        let _ = socket.send_to(&Segment::control(Kind::Welcome, session).encode(), peer).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_tcp_connection_crosses_the_tunnel_both_ways() {
        // IO's side: echoes back what the session says, reversed per read, so both directions are checked.
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = udp.local_addr().unwrap().to_string();
        tokio::spawn(serve(udp, |mut stream, _peer| {
            tokio::spawn(async move {
                let mut all = Vec::new();
                let mut buf = vec![0u8; 4096];
                while all.len() < 200_000 {
                    let n = stream.read(&mut buf).await.unwrap();
                    all.extend_from_slice(&buf[..n]);
                }
                stream.write_all(&all.iter().map(|b| b ^ 0xff).collect::<Vec<_>>()).await.unwrap();
            });
        }));
        let listen = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        tokio::spawn(client(listen, server));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut minecraft = TcpStream::connect(listen).await.unwrap();
        let sent: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
        minecraft.write_all(&sent).await.unwrap();
        let mut back = vec![0u8; sent.len()];
        tokio::time::timeout(Duration::from_secs(10), minecraft.read_exact(&mut back)).await.unwrap().unwrap();
        assert_eq!(back, sent.iter().map(|b| b ^ 0xff).collect::<Vec<_>>());
    }
}
