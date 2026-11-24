//! The worker registry: worker IDs, load, sessions and entity ID blocks (docs/docs/05-io-server.md, 09).

use sculk_proto::cluster::{Heartbeat, IoMessage, SessionReady};
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tonic::Status;

pub type WorkerTx = mpsc::Sender<Result<IoMessage, Status>>;
/// Clientbound play packet bodies for one player's connection.
pub type PlayerTx = mpsc::Sender<Vec<Vec<u8>>>;

/// Entity IDs handed to a worker at a time.
pub const ENTITY_ID_BLOCK: u32 = 4096;

struct Worker {
    tx: WorkerTx,
    /// Its game engine's address, when it runs one.
    engine: Option<String>,
    load: Heartbeat,
    sessions: u32,
    /// Players sent here who are still logging in: counted as load, so logins at once spread out.
    joining: u32,
    /// The worker is stopping: nobody new goes there.
    draining: bool,
    /// Where players' packets for it go, to be batched.
    packets: Option<mpsc::Sender<sculk_proto::cluster::PlayerPackets>>,
    last_seen: Instant,
}

impl Worker {
    fn players(&self) -> u32 {
        self.load.player_count.max(self.sessions) + self.joining
    }
}

#[derive(Default)]
pub struct Registry {
    workers: HashMap<u32, Worker>,
    last_worker_id: u16,
    next_entity_id: i32,
    next_session_id: u64,
    sessions: HashMap<u64, u32>,
    pending: HashMap<u64, oneshot::Sender<SessionReady>>,
    players: HashMap<u64, PlayerTx>,
    /// Who is in each session once logged in: UUID and name.
    profiles: HashMap<u64, (u128, String)>,
    /// The skins (profile properties) of the players proxied to engines, for other engines' tab lists.
    properties: HashMap<u64, Vec<horizon_protocol::packets::login::Property>>,
    /// What each proxied player's relay is asked to do (moves between engines).
    commands: HashMap<u64, mpsc::Sender<crate::proxy::Command>>,
    /// EngineCommand requests waiting for their worker's answer.
    engine_requests: HashMap<u64, oneshot::Sender<sculk_proto::cluster::EngineReply>>,
    /// PreparePlayer requests waiting for their worker's answer.
    next_prepare: u64,
    preparing: HashMap<u64, oneshot::Sender<String>>,
    /// Where and when each player last left, to give its worker time to save it before another worker loads it.
    left: HashMap<u128, (u32, Instant)>,
}

impl Registry {
    /// Gives a new worker a free 16-bit ID. IDs are handed out round-robin so a dead worker's ID is not reused at
    /// once.
    pub fn register(&mut self, tx: WorkerTx, engine: Option<String>, now: Instant) -> Option<u32> {
        for _ in 0..u16::MAX {
            self.last_worker_id = self.last_worker_id.checked_add(1).unwrap_or(1);
            let id = u32::from(self.last_worker_id);
            if let std::collections::hash_map::Entry::Vacant(slot) = self.workers.entry(id) {
                slot.insert(Worker {
                    tx,
                    engine,
                    load: Heartbeat::default(),
                    sessions: 0,
                    joining: 0,
                    draining: false,
                    packets: None,
                    last_seen: now,
                });
                return Some(id);
            }
        }
        None
    }

    /// The worker was heard from.
    pub fn seen(&mut self, worker_id: u32, now: Instant) {
        if let Some(w) = self.workers.get_mut(&worker_id) {
            w.last_seen = now;
        }
    }

    pub fn heartbeat(&mut self, worker_id: u32, load: Heartbeat, now: Instant) {
        if let Some(w) = self.workers.get_mut(&worker_id) {
            w.load = load;
            w.last_seen = now;
        }
    }

    /// Forgets a worker and returns the sessions it held, or `None` if it was already gone. Players proxied to its
    /// engine keep their sessions: their connections go on to another engine, or end by themselves.
    pub fn remove(&mut self, worker_id: u32) -> Option<Vec<u64>> {
        self.workers.remove(&worker_id)?;
        let lost: Vec<u64> = self
            .sessions
            .iter()
            .filter(|&(s, &w)| w == worker_id && !self.commands.contains_key(s))
            .map(|(&s, _)| s)
            .collect();
        for s in &lost {
            self.sessions.remove(s);
            self.pending.remove(s);
            self.profiles.remove(s);
            // Dropping the channel ends the player's connection.
            self.players.remove(s);
        }
        Some(lost)
    }

    /// Workers not heard from since `deadline`.
    pub fn silent_since(&self, deadline: Instant) -> Vec<u32> {
        self.workers.iter().filter(|(_, w)| w.last_seen < deadline).map(|(&id, _)| id).collect()
    }

    pub fn player_count(&self) -> usize {
        self.sessions.len()
    }

    /// The worker with the fewest players, counting sessions IO assigned since its last heartbeat.
    pub fn least_loaded(&self) -> Option<(u32, WorkerTx)> {
        self.workers.iter().min_by_key(|(id, w)| (w.players(), **id)).map(|(&id, w)| (id, w.tx.clone()))
    }

    /// The engine worker most players are on (the lowest ID among equals), or the first engine worker when nobody
    /// plays. Until players move between engines (PLAN.md, block D), every player shares one engine: two engines
    /// whose players met would each see only their own copy of the chunks there.
    pub fn primary_engine(&self) -> Option<u32> {
        self.workers
            .iter()
            .filter(|(_, w)| w.engine.is_some() && !w.draining)
            .max_by_key(|(id, w)| (w.players(), std::cmp::Reverse(**id)))
            .map(|(&id, _)| id)
    }

    /// The worker is stopping: no player goes there any more.
    pub fn set_draining(&mut self, worker_id: u32) {
        if let Some(w) = self.workers.get_mut(&worker_id) {
            w.draining = true;
        }
    }

    /// Engine workers that take players (not stopping).
    pub fn open_engines(&self) -> Vec<u32> {
        let mut ids: Vec<u32> =
            self.workers.iter().filter(|(_, w)| w.engine.is_some() && !w.draining).map(|(&id, _)| id).collect();
        ids.sort();
        ids
    }

    /// A player is on its way to `worker_id`: it counts as there until [`joined`](Self::joined).
    pub fn joining(&mut self, worker_id: u32) {
        if let Some(w) = self.workers.get_mut(&worker_id) {
            w.joining += 1;
        }
    }

    /// The player sent with [`joining`](Self::joining) arrived, or never will.
    pub fn joined(&mut self, worker_id: u32) {
        if let Some(w) = self.workers.get_mut(&worker_id) {
            w.joining = w.joining.saturating_sub(1);
        }
    }

    /// Every worker as the web console shows it.
    pub fn workers_summary(&self) -> Vec<serde_json::Value> {
        let mut workers: Vec<serde_json::Value> = self
            .workers
            .iter()
            .map(|(id, w)| {
                serde_json::json!({
                    "id": id,
                    "engine": w.engine,
                    "players": w.load.player_count.max(w.sessions),
                    "sessions": w.sessions,
                    "mspt": w.load.mspt,
                })
            })
            .collect();
        workers.sort_by_key(|w| w["id"].as_u64());
        workers
    }

    /// Engine workers with at least `min` proxied players.
    pub fn engines_with_players(&self, min: usize) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .workers
            .iter()
            .filter(|&(&id, w)| w.engine.is_some() && self.players_on(id).len() >= min)
            .map(|(&id, _)| id)
            .collect();
        ids.sort();
        ids
    }

    /// An engine worker other than `but` with nobody on it or joining (the lowest ID).
    pub fn empty_engine(&self, but: u32) -> Option<u32> {
        self.workers
            .iter()
            .filter(|&(&id, w)| id != but && w.engine.is_some() && !w.draining && w.players() == 0)
            .map(|(&id, _)| id)
            .min()
    }

    /// A worker's milliseconds per tick, as it last reported.
    pub fn mspt(&self, worker_id: u32) -> f32 {
        self.workers.get(&worker_id).map_or(0.0, |w| w.load.mspt)
    }

    /// The engine worker with the fewest players (the lowest ID among equals).
    pub fn least_loaded_engine(&self) -> Option<u32> {
        self.workers
            .iter()
            .filter(|(_, w)| w.engine.is_some() && !w.draining)
            .min_by_key(|(id, w)| (w.players(), **id))
            .map(|(&id, _)| id)
    }

    /// Where a worker's game engine takes players, when it runs one.
    pub fn engine(&self, worker_id: u32) -> Option<String> {
        self.workers.get(&worker_id)?.engine.clone()
    }

    pub fn set_packets(&mut self, worker_id: u32, packets: mpsc::Sender<sculk_proto::cluster::PlayerPackets>) {
        if let Some(w) = self.workers.get_mut(&worker_id) {
            w.packets = Some(packets);
        }
    }

    /// Where players' packets for a worker go (batched).
    pub fn worker_packets(&self, worker_id: u32) -> Option<mpsc::Sender<sculk_proto::cluster::PlayerPackets>> {
        self.workers.get(&worker_id)?.packets.clone()
    }

    pub fn worker(&self, worker_id: u32) -> Option<WorkerTx> {
        self.workers.get(&worker_id).map(|w| w.tx.clone())
    }

    /// Opens a session on `worker_id`. The receiver gets the worker's SessionReady; the worker's play packets for
    /// the player go to `player`, where they wait until the connection reaches the play state.
    pub fn open_session(&mut self, worker_id: u32, player: PlayerTx) -> (u64, oneshot::Receiver<SessionReady>) {
        self.next_session_id += 1;
        let id = self.next_session_id;
        let (tx, rx) = oneshot::channel();
        self.sessions.insert(id, worker_id);
        self.pending.insert(id, tx);
        self.players.insert(id, player);
        if let Some(w) = self.workers.get_mut(&worker_id) {
            w.sessions += 1;
        }
        (id, rx)
    }

    pub fn session_ready(&mut self, ready: SessionReady) {
        if let Some(tx) = self.pending.remove(&ready.session_id) {
            let _ = tx.send(ready);
        }
    }

    /// Where a worker's packets for `session_id` go.
    pub fn player(&self, session_id: u64) -> Option<PlayerTx> {
        self.players.get(&session_id).cloned()
    }

    /// Records who logged in to a session.
    pub fn set_player(&mut self, session_id: u64, uuid: u128, name: &str) {
        if self.sessions.contains_key(&session_id) {
            self.profiles.insert(session_id, (uuid, name.to_owned()));
        }
    }

    /// The session a player with this UUID is in, anywhere in the cluster.
    pub fn session_of(&self, uuid: u128) -> Option<u64> {
        self.profiles.iter().find(|(_, (u, _))| *u == uuid).map(|(&s, _)| s)
    }

    /// Every player in the cluster: UUID and name, by name.
    pub fn online(&self) -> Vec<(u128, String)> {
        let mut all: Vec<(u128, String)> = self.profiles.values().cloned().collect();
        all.sort_by_key(|(_, name)| name.to_lowercase());
        all
    }

    pub fn set_commands(&mut self, session_id: u64, commands: mpsc::Sender<crate::proxy::Command>) {
        if self.sessions.contains_key(&session_id) {
            self.commands.insert(session_id, commands);
        }
    }

    pub fn set_properties(&mut self, session_id: u64, properties: Vec<horizon_protocol::packets::login::Property>) {
        if self.sessions.contains_key(&session_id) {
            self.properties.insert(session_id, properties);
        }
    }

    /// Every player in the cluster with the worker it is on: UUID, name, skin and worker.
    pub fn cluster_players(&self) -> Vec<(u128, String, Vec<horizon_protocol::packets::login::Property>, u32)> {
        self.profiles
            .iter()
            .filter_map(|(s, (uuid, name))| {
                let worker = *self.sessions.get(s)?;
                Some((*uuid, name.clone(), self.properties.get(s).cloned().unwrap_or_default(), worker))
            })
            .collect()
    }

    /// Where a proxied player's relay takes commands.
    pub fn commands(&self, session_id: u64) -> Option<mpsc::Sender<crate::proxy::Command>> {
        self.commands.get(&session_id).cloned()
    }

    /// The worker a session is on.
    pub fn session_worker(&self, session_id: u64) -> Option<u32> {
        self.sessions.get(&session_id).copied()
    }

    /// The proxied players on a worker: session, UUID and name.
    pub fn players_on(&self, worker_id: u32) -> Vec<(u64, u128, String)> {
        self.sessions
            .iter()
            .filter(|&(s, &w)| w == worker_id && self.commands.contains_key(s))
            .filter_map(|(&s, _)| self.profiles.get(&s).map(|(u, n)| (s, *u, n.clone())))
            .collect()
    }

    /// The session now lives on `to`: its player moved there.
    pub fn move_session(&mut self, session_id: u64, to: u32) {
        let Some(from) = self.sessions.get_mut(&session_id) else { return };
        let from = std::mem::replace(from, to);
        if let Some(w) = self.workers.get_mut(&from) {
            w.sessions = w.sessions.saturating_sub(1);
        }
        if let Some(w) = self.workers.get_mut(&to) {
            w.sessions += 1;
        }
    }

    /// A new EngineCommand request: its ID and where its answer arrives.
    pub fn engine_request(&mut self) -> (u64, oneshot::Receiver<sculk_proto::cluster::EngineReply>) {
        self.next_prepare += 1;
        let (tx, rx) = oneshot::channel();
        self.engine_requests.insert(self.next_prepare, tx);
        (self.next_prepare, rx)
    }

    pub fn engine_reply(&mut self, reply: sculk_proto::cluster::EngineReply) {
        if let Some(tx) = self.engine_requests.remove(&reply.request_id) {
            let _ = tx.send(reply);
        }
    }

    /// A new PreparePlayer request: its ID and where its answer arrives.
    pub fn prepare(&mut self) -> (u64, oneshot::Receiver<String>) {
        self.next_prepare += 1;
        let (tx, rx) = oneshot::channel();
        self.preparing.insert(self.next_prepare, tx);
        (self.next_prepare, rx)
    }

    pub fn player_prepared(&mut self, p: sculk_proto::cluster::PlayerPrepared) {
        if let Some(tx) = self.preparing.remove(&p.request_id) {
            let _ = tx.send(p.error);
        }
    }

    /// The worker a player last left and when, if it left within `recent`.
    pub fn left_recently(&mut self, uuid: u128, recent: std::time::Duration) -> Option<(u32, Instant)> {
        let now = Instant::now();
        self.left.retain(|_, (_, at)| now.duration_since(*at) < recent);
        self.left.get(&uuid).copied()
    }

    /// Ends a player's connection; its session closes when the connection does.
    pub fn drop_player(&mut self, session_id: u64) {
        self.players.remove(&session_id);
    }

    /// Closes a session and returns its worker's channel, to tell the worker.
    pub fn close_session(&mut self, session_id: u64) -> Option<WorkerTx> {
        self.pending.remove(&session_id);
        self.players.remove(&session_id);
        self.commands.remove(&session_id);
        self.properties.remove(&session_id);
        let profile = self.profiles.remove(&session_id);
        let worker_id = self.sessions.remove(&session_id)?;
        if let Some((uuid, _)) = profile {
            self.left.insert(uuid, (worker_id, Instant::now()));
        }
        let w = self.workers.get_mut(&worker_id)?;
        w.sessions = w.sessions.saturating_sub(1);
        Some(w.tx.clone())
    }

    /// The next block of cluster-wide unique entity IDs, or `None` when the 31-bit space is used up.
    pub fn allocate_entity_ids(&mut self) -> Option<(i32, u32)> {
        let start = self.next_entity_id.max(1);
        let next = start.checked_add(ENTITY_ID_BLOCK as i32)?;
        self.next_entity_id = next;
        Some((start, ENTITY_ID_BLOCK))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx() -> WorkerTx {
        mpsc::channel(1).0
    }

    fn ptx() -> PlayerTx {
        mpsc::channel(1).0
    }

    #[test]
    fn worker_ids_are_not_reused_at_once() {
        let mut r = Registry::default();
        let now = Instant::now();
        let a = r.register(tx(), None, now).unwrap();
        let b = r.register(tx(), None, now).unwrap();
        r.remove(a);
        let c = r.register(tx(), None, now).unwrap();
        assert_eq!((a, b, c), (1, 2, 3));
    }

    #[test]
    fn players_share_the_busiest_engine() {
        let mut r = Registry::default();
        let now = Instant::now();
        assert_eq!(r.primary_engine(), None);
        let flat = r.register(tx(), None, now).unwrap();
        let a = r.register(tx(), Some("a:1".into()), now).unwrap();
        let b = r.register(tx(), Some("b:1".into()), now).unwrap();
        assert_eq!(r.primary_engine(), Some(a));
        r.open_session(flat, ptx());
        r.open_session(b, ptx());
        assert_eq!(r.primary_engine(), Some(b));
        r.remove(b);
        assert_eq!(r.primary_engine(), Some(a));
    }

    #[test]
    fn worker_ids_wrap_past_u16() {
        let mut r = Registry { last_worker_id: u16::MAX - 1, ..Default::default() };
        let now = Instant::now();
        assert_eq!(r.register(tx(), None, now), Some(u32::from(u16::MAX)));
        assert_eq!(r.register(tx(), None, now), Some(1));
    }

    #[test]
    fn least_loaded_counts_fresh_sessions() {
        let mut r = Registry::default();
        let now = Instant::now();
        let a = r.register(tx(), None, now).unwrap();
        let b = r.register(tx(), None, now).unwrap();
        assert_eq!(r.least_loaded().unwrap().0, a);
        r.open_session(a, ptx());
        assert_eq!(r.least_loaded().unwrap().0, b);
        r.heartbeat(b, Heartbeat { player_count: 5, ..Default::default() }, now);
        assert_eq!(r.least_loaded().unwrap().0, a);
    }

    #[test]
    fn removing_a_worker_drops_its_sessions() {
        let mut r = Registry::default();
        let now = Instant::now();
        let a = r.register(tx(), None, now).unwrap();
        let b = r.register(tx(), None, now).unwrap();
        let (s1, _) = r.open_session(a, ptx());
        r.open_session(b, ptx());
        assert_eq!(r.remove(a), Some(vec![s1]));
        assert_eq!(r.remove(a), None);
        assert_eq!(r.player_count(), 1);
        assert!(r.player(s1).is_none(), "the lost player's connection is dropped");
        assert!(r.close_session(s1).is_none());
    }

    #[test]
    fn entity_id_blocks_do_not_overlap() {
        let mut r = Registry::default();
        let (a, n) = r.allocate_entity_ids().unwrap();
        let (b, _) = r.allocate_entity_ids().unwrap();
        assert_eq!(a, 1);
        assert_eq!(b, a + n as i32);
        r.next_entity_id = i32::MAX - 10;
        assert!(r.allocate_entity_ids().is_none());
    }

    #[test]
    fn silent_workers_are_found() {
        let mut r = Registry::default();
        let t0 = Instant::now();
        let a = r.register(tx(), None, t0).unwrap();
        let b = r.register(tx(), None, t0).unwrap();
        let t1 = t0 + std::time::Duration::from_secs(5);
        r.heartbeat(b, Heartbeat::default(), t1);
        assert_eq!(r.silent_since(t1), vec![a]);
    }
}
