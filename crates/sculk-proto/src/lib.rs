//! Protobuf schemas shared by IO, workers and AnvilDB. They live in one crate so that every role in the
//! `sculk` binary speaks the same version (docs/docs/08-protokoly.md).

/// Every role must run the same build: IO refuses a worker whose version differs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod anvildb {
    tonic::include_proto!("sculk.anvildb.v1");
}

pub mod cluster {
    tonic::include_proto!("sculk.cluster.v1");
}

/// Merges play packets sent at about the same moment into one message (`PlayerBatch`): `wrap` makes the stream's
/// message of a batch. A busy stream batches more, an idle one sends each packet at once.
pub fn batcher<M: Send + 'static>(
    out: tokio::sync::mpsc::Sender<M>,
    wrap: impl Fn(cluster::PlayerBatch) -> M + Send + 'static,
) -> tokio::sync::mpsc::Sender<cluster::PlayerPackets> {
    /// Packets in one batch at most.
    const MAX_BATCH: usize = 1024;
    let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut packets = vec![first];
            while packets.len() < MAX_BATCH
                && let Ok(more) = rx.try_recv()
            {
                packets.push(more);
            }
            if out.send(wrap(cluster::PlayerBatch { packets })).await.is_err() {
                break;
            }
        }
    });
    tx
}
