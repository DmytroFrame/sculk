//! Horizon's chunk store hook (`horizon_hooks::storage`, patches/horizon/0001) backed by AnvilDB: the engine reads
//! and writes chunk records there instead of its own region files, and holds a lease on every chunk it reads, so
//! no other worker writes it meanwhile (docs/docs/03-mikrosvity.md, "Zone and leases").
//!
//! The whole engine is one micro world for now: one lease holder per worker.

use horizon_hooks::storage::ChunkStore;
use sculk_proto::anvildb::{
    AcquireRequest, ChunkKey, ChunkPos, ChunkWrite, LeaseHolder, ReadChunkRequest, ReleaseRequest, RenewRequest,
    SavedInRegionRequest, WriteChunksRequest, acquire_response, chunks_client::ChunksClient,
    leases_client::LeasesClient,
};
use sculk_proto::cluster::{ConflictReport, io_client::IoClient};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use tonic::transport::Channel;

/// How long an unloaded chunk keeps its lease when no write of it comes: Horizon's region writer saves a changed
/// chunk (and its entities) after the game lets it go, and AnvilDB refuses writes without the lease. A chunk that
/// is written after it was unloaded is released at once.
const RELEASE_GRACE: Duration = Duration::from_secs(10);
/// The thread Horizon saves region files on.
const REGION_WRITER: &str = "horizon-region-writer";
/// How long after the last write of an unloaded chunk its lease is released.
const AFTER_LAST_WRITE: Duration = Duration::from_secs(2);
/// How often the engine tells IO about chunks another engine holds, per holder.
const CONFLICT_REPORT_EVERY: Duration = Duration::from_secs(1);
/// How long a chunk another engine is letting go of is waited for (its unload, last saves and release).
const WAIT_FOR_OWNER: Duration = Duration::from_secs(20);

type Chunk = (String, i32, i32);

#[derive(Default)]
struct Leases {
    /// Chunks (dimension, x, z) this engine holds a lease on.
    held: HashSet<Chunk>,
    /// Held chunks the game unloaded, and when: released after RELEASE_GRACE unless read again.
    releasing: HashMap<Chunk, Instant>,
    /// When IO was last told about each other worker holding chunks this engine wanted, and its answer.
    reported: HashMap<u32, (Instant, bool)>,
    /// Chunk records not in AnvilDB yet (it was out of reach): the newest of each, sent with the next write or
    /// lease renewal. Their chunks keep their leases until they are in.
    pending: HashMap<RecordKey, Option<Vec<u8>>>,
}

/// A chunk record's place: dimension, folder, x, z.
type RecordKey = (String, String, i32, i32);

/// Whether AnvilDB could not be reached (as opposed to refusing the request).
fn unreachable(status: &tonic::Status) -> bool {
    matches!(
        status.code(),
        tonic::Code::Unavailable | tonic::Code::Unknown | tonic::Code::DeadlineExceeded | tonic::Code::Cancelled
    )
}

/// How long the first of several reads waits for others before their leases are asked for together.
const BATCH_WINDOW: Duration = Duration::from_millis(2);

/// How long a read waits for AnvilDB to come back before the chunk counts as unreadable.
const WAIT_FOR_ANVILDB: Duration = Duration::from_secs(30);

/// gRPC messages carry batches of chunk records.
const MAX_MESSAGE: usize = 256 << 20;

pub struct AnvilStore {
    runtime: tokio::runtime::Runtime,
    chunks: ChunksClient<Channel>,
    leases: LeasesClient<Channel>,
    /// IO, told about chunks another engine holds so it moves the players here to that engine.
    io: Option<IoClient<Channel>>,
    world: String,
    holder: LeaseHolder,
    state: Mutex<Leases>,
    /// Taking a lease and releasing leases never interleave, so a chunk read again while its lease is being
    /// released is leased anew after the release, not before it.
    gate: tokio::sync::Mutex<()>,
    /// One flush of pending records at a time, so records reach AnvilDB in the order the game wrote them.
    flushing: tokio::sync::Mutex<()>,
    /// Reads waiting for their chunks' leases, taken together (`lease_in_batch`).
    batch: Mutex<Vec<(Chunk, std::sync::mpsc::Sender<io::Result<bool>>)>>,
}

/// The dimension a Horizon save folder belongs to: `.../dimensions/<namespace>/<path>` is `<namespace>:<path>`,
/// anything else the overworld (saved in the world folder itself).
pub fn dimension_of(dimension_dir: &Path) -> String {
    let parts: Vec<String> = dimension_dir.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    match parts.iter().rposition(|p| p == "dimensions") {
        Some(i) if parts.len() > i + 2 => format!("{}:{}", parts[i + 1], parts[i + 2..].join("/")),
        _ => "minecraft:overworld".into(),
    }
}

fn io_error(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

impl AnvilStore {
    /// Connects lazily to AnvilDB at `url`; `worker_id` is the one IO gave this worker.
    pub fn new(url: &str, io: Option<&str>, world: &str, worker_id: u32) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let (channel, io) = {
            let _enter = runtime.enter();
            let io = match io {
                Some(io) => Some(IoClient::new(tonic::transport::Endpoint::from_shared(io.to_owned())?.connect_lazy())),
                None => None,
            };
            (tonic::transport::Endpoint::from_shared(url.to_owned())?.connect_lazy(), io)
        };
        let chunks = ChunksClient::new(channel.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE);
        // The engine's micro world: the worker ID on top, as `sculk-worker`'s IDs are (mw::mw_id).
        let holder = LeaseHolder { mw_id: u64::from(worker_id) << 48, worker_id };
        Ok(Self {
            runtime,
            chunks,
            leases: LeasesClient::new(channel),
            io,
            world: world.to_owned(),
            holder,
            state: Mutex::new(Leases::default()),
            gate: tokio::sync::Mutex::new(()),
            flushing: tokio::sync::Mutex::new(()),
            batch: Mutex::new(Vec::new()),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Leases> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Releases the leases of chunks unloaded more than RELEASE_GRACE ago.
    async fn release_unloaded(&self) {
        let _gate = self.gate.lock().await;
        let now = Instant::now();
        let due: Vec<Chunk> = {
            let mut state = self.state();
            let due: Vec<Chunk> = state
                .releasing
                .iter()
                .filter(|(_, at)| now.duration_since(**at) >= RELEASE_GRACE)
                .map(|(c, _)| c.clone())
                // A chunk whose last records still wait for AnvilDB keeps its lease until they are in.
                .filter(|(d, x, z)| !state.pending.keys().any(|(pd, _, px, pz)| pd == d && px == x && pz == z))
                .collect();
            for chunk in &due {
                state.releasing.remove(chunk);
                state.held.remove(chunk);
            }
            due
        };
        if due.is_empty() {
            return;
        }
        let chunks = due
            .iter()
            .map(|(dimension, x, z)| ChunkPos { world: self.world.clone(), dimension: dimension.clone(), x: *x, z: *z })
            .collect();
        match self.leases.clone().release(ReleaseRequest { holder: Some(self.holder), chunks }).await {
            Ok(r) => log::debug!("Sculk: released {} leases of unloaded chunks", r.get_ref().released),
            // They expire by TTL: renewals skip them now.
            Err(e) => log::warn!("Sculk: releasing unloaded chunks failed: {}", e.message()),
        }
    }

    /// Renews the engine's leases every `every` and releases those of chunks it unloaded, on the store's own
    /// runtime, for as long as the process runs.
    pub fn keep_leases(&'static self, every: Duration) {
        self.runtime.spawn(async move {
            let mut interval = tokio::time::interval(every);
            loop {
                interval.tick().await;
                if let Err(e) = self.flush().await {
                    log::warn!("Sculk: chunk records wait for AnvilDB: {}", e.message());
                }
                self.release_unloaded().await;
                let held = self.state().held.len();
                if held == 0 {
                    continue;
                }
                match self.leases.clone().renew(RenewRequest { holder: Some(self.holder) }).await {
                    Ok(r) if r.get_ref().renewed as usize >= held => {}
                    Ok(r) => {
                        log::warn!(
                            "Sculk: held {held} leases, AnvilDB renewed {}; taking them again",
                            r.get_ref().renewed
                        );
                        self.reacquire().await;
                    }
                    Err(e) => log::warn!("Sculk: lease renew failed: {}", e.message()),
                }
            }
        });
    }

    /// Takes again the leases AnvilDB no longer has for this engine (AnvilDB restarted, or they expired while it
    /// was out of reach), so the game's chunks can still be saved. A chunk another micro world took meanwhile stays
    /// its: AnvilDB refuses this engine's writes there, and the game's copy is the one that is lost.
    async fn reacquire(&self) {
        let _gate = self.gate.lock().await;
        let held: Vec<Chunk> = self.state().held.iter().cloned().collect();
        let (mut taken, mut lost) = (0, 0);
        let pos = |(dimension, x, z): &Chunk| ChunkPos {
            world: self.world.clone(),
            dimension: dimension.clone(),
            x: *x,
            z: *z,
        };
        for batch in held.chunks(256) {
            // All or none: a batch with a chunk taken by someone else goes again one chunk at a time.
            let request = AcquireRequest { holder: Some(self.holder), chunks: batch.iter().map(pos).collect() };
            let one_by_one = match self.leases.clone().acquire(request).await.map(|r| r.into_inner().result) {
                Ok(Some(acquire_response::Result::Granted(_))) => {
                    taken += batch.len();
                    continue;
                }
                Ok(_) => batch,
                Err(e) => {
                    log::warn!("Sculk: taking leases again failed: {}", e.message());
                    return;
                }
            };
            for chunk in one_by_one {
                let request = AcquireRequest { holder: Some(self.holder), chunks: vec![pos(chunk)] };
                match self.leases.clone().acquire(request).await.map(|r| r.into_inner().result) {
                    Ok(Some(acquire_response::Result::Granted(_))) => taken += 1,
                    Ok(_) => {
                        lost += 1;
                        let mut state = self.state();
                        state.held.remove(chunk);
                        state.releasing.remove(chunk);
                    }
                    Err(e) => {
                        log::warn!("Sculk: taking leases again failed: {}", e.message());
                        return;
                    }
                }
            }
        }
        log::info!("Sculk: took {taken} leases again; {lost} belong to other micro worlds now");
    }

    /// Takes the lease on a chunk before the engine reads it; refuses a chunk another micro world holds.
    fn lease(&self, dimension: &str, x: i32, z: i32) -> io::Result<()> {
        let key = (dimension.to_owned(), x, z);
        {
            let state = self.state();
            if state.held.contains(&key) && !state.releasing.contains_key(&key) {
                return Ok(());
            }
            // The region writer reads a chunk's records while it saves the chunk after an unload: that is no load,
            // and the chunk stays on its way out.
            if state.held.contains(&key) && std::thread::current().name() == Some(REGION_WRITER) {
                return Ok(());
            }
        }
        // Most chunks are free: taken together with the other reads of the moment, in one request.
        if self.lease_in_batch(&key)? {
            return Ok(());
        }
        let waiting_since = Instant::now();
        loop {
            let owner = {
                let _gate = self.gate.blocking_lock();
                {
                    let mut state = self.state();
                    // Unloaded and read again before its release: the game holds it again.
                    if state.releasing.remove(&key).is_some() || state.held.contains(&key) {
                        return Ok(());
                    }
                }
                let chunk = ChunkPos { world: self.world.clone(), dimension: dimension.to_owned(), x, z };
                let request = AcquireRequest { holder: Some(self.holder), chunks: vec![chunk] };
                let answer = self.patiently(|| {
                    let (mut leases, request) = (self.leases.clone(), request.clone());
                    async move { leases.acquire(request).await }
                })?;
                match answer.into_inner().result {
                    Some(acquire_response::Result::Granted(_)) => {
                        self.state().held.insert(key);
                        return Ok(());
                    }
                    Some(acquire_response::Result::Conflict(c)) => c.owner.unwrap_or_default(),
                    None => return Err(io_error("AnvilDB sent an empty answer")),
                }
            };
            // Another micro world holds it. Either it is letting the chunk go (its players left), and the chunk is
            // waited for, or the micro worlds met, and IO moves this engine's players near the chunk to the owner's
            // engine (docs/docs/03-mikrosvity.md, "Merge").
            let wait = self.conflict(owner.worker_id, dimension, x, z);
            if !wait || waiting_since.elapsed() >= WAIT_FOR_OWNER {
                return Err(io_error(format!(
                    "chunk {x},{z} of {dimension} belongs to micro world {:#x} on worker #{}",
                    owner.mw_id, owner.worker_id
                )));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Takes the lease on `key` in one request with the other chunks wanted within BATCH_WINDOW (docs/docs/06-worker.md,
    /// "The connection to AnvilDB"): the first reader waits the window, then asks for all of them. True when the lease is held;
    /// false when the batch met a chunk of another micro world, and each of its chunks goes its own way.
    fn lease_in_batch(&self, key: &Chunk) -> io::Result<bool> {
        let (done, outcome) = std::sync::mpsc::channel();
        let leader = {
            let mut batch = self.batch.lock().unwrap_or_else(|e| e.into_inner());
            batch.push((key.clone(), done));
            batch.len() == 1
        };
        if leader {
            std::thread::sleep(BATCH_WINDOW);
            let batch = std::mem::take(&mut *self.batch.lock().unwrap_or_else(|e| e.into_inner()));
            let granted = self.acquire_batch(batch.iter().map(|(k, _)| k));
            for (_, done) in batch {
                let _ = done.send(match &granted {
                    Ok(g) => Ok(*g),
                    Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
                });
            }
        }
        outcome.recv().map_err(|_| io_error("the lease batch was lost"))?
    }

    /// Acquires every chunk of a batch, all or none. True when all are held (some may have been held already).
    fn acquire_batch<'a>(&self, keys: impl Iterator<Item = &'a Chunk>) -> io::Result<bool> {
        let _gate = self.gate.blocking_lock();
        let wanted: Vec<Chunk> = {
            let mut state = self.state();
            // Unloaded and read again before its release: the game holds it again.
            keys.filter(|k| state.releasing.remove(*k).is_none() && !state.held.contains(*k)).cloned().collect()
        };
        if wanted.is_empty() {
            return Ok(true);
        }
        let chunks: Vec<ChunkPos> = wanted
            .iter()
            .map(|(dimension, x, z)| ChunkPos { world: self.world.clone(), dimension: dimension.clone(), x: *x, z: *z })
            .collect();
        let request = AcquireRequest { holder: Some(self.holder), chunks };
        let answer = self.patiently(|| {
            let (mut leases, request) = (self.leases.clone(), request.clone());
            async move { leases.acquire(request).await }
        })?;
        match answer.into_inner().result {
            Some(acquire_response::Result::Granted(_)) => {
                self.state().held.extend(wanted);
                Ok(true)
            }
            Some(acquire_response::Result::Conflict(_)) => Ok(false),
            None => Err(io_error("AnvilDB sent an empty answer")),
        }
    }

    /// Tells IO that another engine holds a chunk this one wanted, and returns IO's answer: wait for the chunk
    /// (true), or not. One answer per owner is kept CONFLICT_REPORT_EVERY.
    fn conflict(&self, owner: u32, dimension: &str, x: i32, z: i32) -> bool {
        let Some(io) = self.io.clone() else { return true };
        {
            let state = self.state();
            if let Some((at, wait)) = state.reported.get(&owner)
                && at.elapsed() < CONFLICT_REPORT_EVERY
            {
                return *wait;
            }
        }
        let report = ConflictReport {
            worker_id: self.holder.worker_id,
            owner_worker_id: owner,
            dimension: dimension.to_owned(),
            x,
            z,
        };
        let answer = self
            .runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), io.clone().report_conflict(report)).await });
        let wait = match answer {
            Ok(Ok(ack)) => ack.into_inner().wait,
            Ok(Err(e)) => {
                log::warn!("Sculk: telling IO about a chunk of another engine failed: {}", e.message());
                false
            }
            Err(_) => false,
        };
        self.state().reported.insert(owner, (Instant::now(), wait));
        wait
    }

    /// Sends AnvilDB the records waiting for it. A chunk whose lease was lost meanwhile (it expired while AnvilDB
    /// was out of reach, and someone took the chunk) is dropped: its copy here is no longer the world's.
    async fn flush(&self) -> Result<(), tonic::Status> {
        let _flushing = self.flushing.lock().await;
        let records: Vec<(RecordKey, Option<Vec<u8>>)> = self.state().pending.drain().collect();
        if records.is_empty() {
            return Ok(());
        }
        let write = |records: &[(RecordKey, Option<Vec<u8>>)]| {
            let writes = records
                .iter()
                .map(|((d, f, x, z), nbt)| ChunkWrite { key: Some(self.key(d, f, *x, *z)), nbt: nbt.clone() })
                .collect();
            let (mut chunks, worker_id) = (self.chunks.clone(), self.holder.worker_id);
            async move { chunks.write(WriteChunksRequest { worker_id, writes }).await }
        };
        let back = |records: Vec<(RecordKey, Option<Vec<u8>>)>| {
            let mut state = self.state();
            for (key, nbt) in records {
                // A newer record of the chunk came meanwhile: it wins.
                state.pending.entry(key).or_insert(nbt);
            }
        };
        match write(&records).await {
            Ok(_) => Ok(()),
            Err(e) if e.code() == tonic::Code::FailedPrecondition => {
                for record in records {
                    match write(std::slice::from_ref(&record)).await {
                        Ok(_) => {}
                        Err(e) if e.code() == tonic::Code::FailedPrecondition => {
                            let ((d, _, x, z), _) = &record;
                            log::warn!("Sculk: chunk {x},{z} of {d} is no longer this engine's; not saved");
                            self.state().held.remove(&(d.clone(), *x, *z));
                        }
                        Err(e) => {
                            back(vec![record]);
                            return Err(e);
                        }
                    }
                }
                Ok(())
            }
            Err(e) => {
                back(records);
                Err(e)
            }
        }
    }

    /// Runs a request on the game's thread, waiting up to WAIT_FOR_ANVILDB for AnvilDB to come back.
    fn patiently<T, F>(&self, request: impl Fn() -> F) -> io::Result<tonic::Response<T>>
    where
        F: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    {
        let since = Instant::now();
        loop {
            match self.runtime.block_on(request()) {
                Ok(answer) => return Ok(answer),
                Err(e) if unreachable(&e) && since.elapsed() < WAIT_FOR_ANVILDB => {
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => return Err(io_error(e.message())),
            }
        }
    }

    fn key(&self, dimension: &str, folder: &str, x: i32, z: i32) -> ChunkKey {
        ChunkKey { world: self.world.clone(), dimension: dimension.to_owned(), folder: folder.to_owned(), x, z }
    }
}

impl ChunkStore for AnvilStore {
    fn read(&self, dimension_dir: &Path, folder: &str, x: i32, z: i32) -> io::Result<Option<Vec<u8>>> {
        let dimension = dimension_of(dimension_dir);
        self.lease(&dimension, x, z)?;
        // A record still on its way to AnvilDB is the chunk's newest.
        if let Some(nbt) = self.state().pending.get(&(dimension.clone(), folder.to_owned(), x, z)) {
            return Ok(nbt.clone());
        }
        let key = self.key(&dimension, folder, x, z);
        let answer = self.patiently(|| {
            let (mut chunks, key) = (self.chunks.clone(), key.clone());
            async move { chunks.read(ReadChunkRequest { key: Some(key) }).await }
        })?;
        Ok(answer.into_inner().nbt)
    }

    fn write(&self, dimension_dir: &Path, folder: &str, records: &[((i32, i32), Option<Vec<u8>>)]) -> io::Result<()> {
        let dimension = dimension_of(dimension_dir);
        // A chunk this engine holds no lease on is another micro world's: its copy here (generated when the read was
        // refused) is not the world's, and is never saved over the owner's.
        let (ours, theirs): (Vec<_>, Vec<_>) = {
            let state = self.state();
            records.iter().partition(|((x, z), _)| state.held.contains(&(dimension.clone(), *x, *z)))
        };
        if !theirs.is_empty() {
            log::debug!("Sculk: {} chunks of other micro worlds not saved", theirs.len());
        }
        {
            let mut state = self.state();
            for ((x, z), nbt) in ours {
                state.pending.insert((dimension.clone(), folder.to_owned(), *x, *z), nbt.clone());
            }
        }
        match self.runtime.block_on(self.flush()) {
            Ok(()) => {}
            // Saved once AnvilDB is back; the game goes on meanwhile.
            Err(e) if unreachable(&e) => {
                log::warn!("Sculk: AnvilDB is out of reach; {} chunk records wait", self.state().pending.len())
            }
            Err(e) => return Err(io_error(e.message())),
        }
        // Unloaded chunks whose last records are now saved need their leases no more (another engine may want them):
        // released AFTER_LAST_WRITE after this write, which leaves time for the chunk's other folders (entities, poi).
        let now = Instant::now();
        let mut state = self.state();
        for ((x, z), _) in records {
            if let Some(at) = state.releasing.get_mut(&(dimension.clone(), *x, *z)) {
                *at = now.checked_sub(RELEASE_GRACE - AFTER_LAST_WRITE).unwrap_or(*at);
            }
        }
        Ok(())
    }

    fn unloaded(&self, dimension_dir: &Path, chunks: &[(i32, i32)]) {
        let dimension = dimension_of(dimension_dir);
        let now = Instant::now();
        let mut state = self.state();
        for &(x, z) in chunks {
            let key = (dimension.clone(), x, z);
            if state.held.contains(&key) {
                state.releasing.insert(key, now);
            }
        }
    }

    fn resident(&self, dimension_dir: &Path, chunks: &mut dyn Iterator<Item = (i32, i32)>) {
        let dimension = dimension_of(dimension_dir);
        let resident: HashSet<(i32, i32)> = chunks.collect();
        let now = Instant::now();
        let mut state = self.state();
        let Leases { held, releasing, .. } = &mut *state;
        // Held chunks the game no longer has go the way of unloaded ones: released unless read again or written.
        for key in held.iter().filter(|(d, x, z)| *d == dimension && !resident.contains(&(*x, *z))) {
            releasing.entry(key.clone()).or_insert(now);
        }
    }

    fn saved_in_region(&self, dimension_dir: &Path, folder: &str, rx: i32, rz: i32) -> io::Result<[u64; 16]> {
        let request = SavedInRegionRequest {
            world: self.world.clone(),
            dimension: dimension_of(dimension_dir),
            folder: folder.to_owned(),
            rx,
            rz,
        };
        let answer =
            self.runtime.block_on(self.chunks.clone().saved_in_region(request)).map_err(|e| io_error(e.message()))?;
        answer.into_inner().bits.try_into().map_err(|_| io_error("AnvilDB sent a region bitmap of the wrong size"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_follows_horizons_folders() {
        assert_eq!(
            dimension_of(Path::new("/srv/universe/world/dimensions/minecraft/overworld")),
            "minecraft:overworld"
        );
        assert_eq!(
            dimension_of(Path::new("/srv/universe/world/dimensions/minecraft/the_nether")),
            "minecraft:the_nether"
        );
        assert_eq!(dimension_of(Path::new("world/dimensions/mymod/sky/high")), "mymod:sky/high");
    }
}
