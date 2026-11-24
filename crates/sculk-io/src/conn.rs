//! A client connection on tokio, framed by `horizon_protocol::frame`. Packets go in and out as bodies: VarInt ID
//! then payload, as `horizon_protocol`'s packet enums encode and decode them.

use anyhow::{Context, bail};
use horizon_protocol::crypto::Cfb8;
use horizon_protocol::frame::{self, Body, Inflater};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A connection's reading side: a TCP socket's, or a Nether Tunnel session's.
type ReadHalf = Box<dyn AsyncRead + Send + Unpin>;
/// A connection's writing side.
type WriteHalf = Box<dyn AsyncWrite + Send + Unpin>;

pub struct Reader {
    half: ReadHalf,
    buf: Vec<u8>,
    threshold: Option<usize>,
    inflater: Inflater,
    /// Online mode: everything after the encryption response comes in AES/CFB8.
    cipher: Option<Cfb8>,
}

impl Reader {
    pub fn new(half: impl AsyncRead + Send + Unpin + 'static) -> Self {
        Self { half: Box::new(half), buf: Vec::new(), threshold: None, inflater: Inflater::default(), cipher: None }
    }

    /// The next packet body, or `None` when the client closed the connection between packets.
    pub async fn next(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        let Some(contents) = self.next_frame().await? else { return Ok(None) };
        self.body(contents).map(Some)
    }

    /// The next frame's contents as they came, still compressed when the connection compresses: what a proxy
    /// passes on to a peer that compresses alike ([`Writer::send_contents`]). `None` when the peer closed the
    /// connection between frames. Cancel-safe: a frame half read stays in the buffer.
    pub async fn next_frame(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        loop {
            if let Some((head, len)) = frame::header(&self.buf).map_err(|e| anyhow::anyhow!(e.0))?
                && self.buf.len() >= head + len
            {
                return Ok(Some(self.buf.drain(..head + len).skip(head).collect()));
            }
            let mut chunk = [0u8; 8192];
            let n = self.half.read(&mut chunk).await?;
            if n == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                bail!("connection closed in the middle of a packet");
            }
            if let Some(cipher) = &mut self.cipher {
                cipher.decrypt(&mut chunk[..n]);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// A frame's packet body: VarInt ID, then payload.
    pub fn body(&mut self, contents: Vec<u8>) -> anyhow::Result<Vec<u8>> {
        Ok(match frame::body(&contents, self.threshold, &mut self.inflater).map_err(|e| anyhow::anyhow!(e.0))? {
            Body::Plain(start) => contents[start..].to_vec(),
            Body::Inflated(body) => body,
        })
    }

    /// The packet ID a frame carries.
    pub fn frame_id(&mut self, contents: &[u8]) -> anyhow::Result<i32> {
        let body = self.body(contents.to_vec())?;
        horizon_protocol::buf::Reader::new(&body).varint().map_err(|e| anyhow::anyhow!(e.0))
    }

    /// The compression threshold frames use, if any.
    pub fn threshold(&self) -> Option<usize> {
        self.threshold
    }

    /// Bytes from now on are decrypted with the shared secret. Called right after the client's encryption
    /// response, before it can send anything encrypted, so the buffer holds no ciphertext yet.
    pub fn encrypt(&mut self, secret: &[u8; 16]) {
        self.cipher = Some(Cfb8::new(secret, secret));
    }

    /// Frames from now on carry the uncompressed length (Set Compression).
    pub fn set_compression(&mut self, threshold: usize) {
        self.threshold = Some(threshold);
    }

    /// The next packet split into its ID and payload.
    pub async fn packet(&mut self) -> anyhow::Result<Option<(i32, Vec<u8>)>> {
        let Some(body) = self.next().await? else { return Ok(None) };
        let mut r = horizon_protocol::buf::Reader::new(&body);
        let id = r.varint().map_err(|e| anyhow::anyhow!(e.0)).context("packet ID")?;
        let start = body.len() - r.remaining();
        Ok(Some((id, body[start..].to_vec())))
    }
}

pub struct Writer {
    half: WriteHalf,
    threshold: Option<usize>,
    cipher: Option<Cfb8>,
}

impl Writer {
    pub fn new(half: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self { half: Box::new(half), threshold: None, cipher: None }
    }

    async fn write(&mut self, mut bytes: Vec<u8>) -> anyhow::Result<()> {
        if let Some(cipher) = &mut self.cipher {
            cipher.encrypt(&mut bytes);
        }
        self.half.write_all(&bytes).await?;
        Ok(())
    }

    pub async fn send(&mut self, body: &[u8]) -> anyhow::Result<()> {
        let framed = frame::encode(body, self.threshold).map_err(|e| anyhow::anyhow!(e.0))?;
        self.write(framed).await
    }

    /// Bytes from now on are encrypted with the shared secret.
    pub fn encrypt(&mut self, secret: &[u8; 16]) {
        self.cipher = Some(Cfb8::new(secret, secret));
    }

    pub fn set_compression(&mut self, threshold: usize) {
        self.threshold = Some(threshold);
    }

    /// Frames a worker already prepared (`frame::contents`, compressed when the connection is): only the length
    /// prefix is added.
    pub async fn send_contents(&mut self, contents: &[Vec<u8>]) -> anyhow::Result<()> {
        let mut out = Vec::new();
        for c in contents {
            out.extend(frame::prefixed(c));
        }
        self.write(out).await
    }

    pub async fn send_all(&mut self, bodies: &[Vec<u8>]) -> anyhow::Result<()> {
        let mut out = Vec::new();
        for body in bodies {
            out.extend(frame::encode(body, self.threshold).map_err(|e| anyhow::anyhow!(e.0))?);
        }
        self.write(out).await
    }

    pub async fn shutdown(&mut self) {
        let _ = self.half.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_protocol::packets::status;

    #[tokio::test]
    async fn packets_cross_a_socket_in_pieces() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            let ping = status::Serverbound::PingRequest(status::Ping { time: 77 }).encode();
            let mut bytes = frame::encode(&ping, None).unwrap();
            bytes
                .extend(frame::encode(&status::Serverbound::StatusRequest(Default::default()).encode(), None).unwrap());
            // One byte at a time: the reader must wait for whole frames.
            for b in bytes {
                s.write_all(&[b]).await.unwrap();
                s.flush().await.unwrap();
            }
        });
        let (stream, _) = listener.accept().await.unwrap();
        let (read, _write) = stream.into_split();
        let mut r = Reader::new(read);
        let (id, payload) = r.packet().await.unwrap().unwrap();
        assert_eq!(
            status::Serverbound::decode(id, &payload).unwrap(),
            status::Serverbound::PingRequest(status::Ping { time: 77 })
        );
        let (id, payload) = r.packet().await.unwrap().unwrap();
        assert!(matches!(status::Serverbound::decode(id, &payload).unwrap(), status::Serverbound::StatusRequest(_)));
        client.await.unwrap();
        assert!(r.next().await.unwrap().is_none());
    }
}
