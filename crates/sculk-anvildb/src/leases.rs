//! The lease table: who may read and write each chunk (docs/docs/07-anvildb.md, "Оренди чанків").
//!
//! Pure logic with the clock passed in, so tests control time. An expired lease counts as free.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Holder {
    pub mw_id: u64,
    pub worker_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChunkKey {
    pub world: String,
    pub dimension: String,
    pub x: i32,
    pub z: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub chunk: ChunkKey,
    pub holder: Holder,
    pub generation: u64,
    pub expires_at: Instant,
}

/// The first chunk of a request that another micro world holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub chunk: ChunkKey,
    pub owner: Holder,
}

/// One lease as saved on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedLease {
    pub world: String,
    pub dimension: String,
    pub x: i32,
    pub z: i32,
    pub mw_id: u64,
    pub worker_id: u32,
    pub generation: u64,
    pub left_ms: u64,
}

/// The lease table as saved on disk (`LeaseTable::snapshot`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub next_generation: u64,
    pub leases: Vec<SavedLease>,
}

#[derive(Debug, Clone)]
struct Entry {
    holder: Holder,
    generation: u64,
    expires_at: Instant,
}

impl Entry {
    fn live(&self, now: Instant) -> bool {
        self.expires_at > now
    }
}

pub struct LeaseTable {
    ttl: Duration,
    /// Grows with every change, so a saver knows whether there is anything new to save.
    changes: u64,
    next_generation: u64,
    leases: HashMap<ChunkKey, Entry>,
    by_mw: HashMap<u64, HashSet<ChunkKey>>,
}

impl LeaseTable {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, changes: 0, next_generation: 1, leases: HashMap::new(), by_mw: HashMap::new() }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn len(&self) -> usize {
        self.leases.len()
    }

    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }

    /// How many changes the table has seen.
    pub fn changes(&self) -> u64 {
        self.changes
    }

    /// The table as it can be saved: every live lease with the time it has left, and the next generation, so that
    /// fencing tokens keep growing across a restart.
    pub fn snapshot(&self, now: Instant) -> Snapshot {
        let leases = self
            .leases
            .iter()
            .filter(|(_, e)| e.live(now))
            .map(|(c, e)| SavedLease {
                world: c.world.clone(),
                dimension: c.dimension.clone(),
                x: c.x,
                z: c.z,
                mw_id: e.holder.mw_id,
                worker_id: e.holder.worker_id,
                generation: e.generation,
                left_ms: e.expires_at.saturating_duration_since(now).as_millis() as u64,
            })
            .collect();
        Snapshot { next_generation: self.next_generation, leases }
    }

    /// A table from a snapshot taken before a restart. Each lease keeps the time it had left, so a holder that
    /// renews in time keeps it, and one that died before the restart loses it when its TTL runs out.
    pub fn restore(ttl: Duration, snapshot: Snapshot, now: Instant) -> Self {
        let mut table = Self::new(ttl);
        table.next_generation = snapshot.next_generation.max(1);
        for l in snapshot.leases {
            let chunk = ChunkKey { world: l.world, dimension: l.dimension, x: l.x, z: l.z };
            let holder = Holder { mw_id: l.mw_id, worker_id: l.worker_id };
            let expires_at = now + Duration::from_millis(l.left_ms).min(ttl);
            table.next_generation = table.next_generation.max(l.generation + 1);
            table.by_mw.entry(holder.mw_id).or_default().insert(chunk.clone());
            table.leases.insert(chunk, Entry { holder, generation: l.generation, expires_at });
        }
        table
    }

    fn generation(&mut self) -> u64 {
        let g = self.next_generation;
        self.next_generation += 1;
        g
    }

    fn lease(chunk: &ChunkKey, e: &Entry) -> Lease {
        Lease { chunk: chunk.clone(), holder: e.holder, generation: e.generation, expires_at: e.expires_at }
    }

    fn unindex(&mut self, mw_id: u64, chunk: &ChunkKey) {
        if let Some(set) = self.by_mw.get_mut(&mw_id) {
            set.remove(chunk);
            if set.is_empty() {
                self.by_mw.remove(&mw_id);
            }
        }
    }

    /// Takes every chunk for `holder`, or none of them. Chunks the micro world already holds are renewed and keep
    /// their generation.
    pub fn acquire(&mut self, holder: Holder, chunks: &[ChunkKey], now: Instant) -> Result<Vec<Lease>, Conflict> {
        self.changes += 1;
        for chunk in chunks {
            if let Some(e) = self.leases.get(chunk)
                && e.live(now)
                && e.holder.mw_id != holder.mw_id
            {
                return Err(Conflict { chunk: chunk.clone(), owner: e.holder });
            }
        }
        let expires_at = now + self.ttl;
        let mut granted = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let keep = self.leases.get(chunk).filter(|e| e.live(now) && e.holder == holder).map(|e| e.generation);
            if let Some(old) = self.leases.get(chunk).map(|e| e.holder.mw_id)
                && old != holder.mw_id
            {
                self.unindex(old, chunk);
            }
            let generation = match keep {
                Some(g) => g,
                None => self.generation(),
            };
            let entry = Entry { holder, generation, expires_at };
            granted.push(Self::lease(chunk, &entry));
            self.leases.insert(chunk.clone(), entry);
            self.by_mw.entry(holder.mw_id).or_default().insert(chunk.clone());
        }
        Ok(granted)
    }

    /// Extends every live lease of `holder`. Leases that already expired are lost and dropped.
    pub fn renew(&mut self, holder: Holder, now: Instant) -> u32 {
        self.changes += 1;
        let Some(chunks) = self.by_mw.get(&holder.mw_id).cloned() else { return 0 };
        let mut renewed = 0;
        for chunk in chunks {
            let Some(e) = self.leases.get_mut(&chunk) else { continue };
            if e.holder != holder {
                continue;
            }
            if e.live(now) {
                e.expires_at = now + self.ttl;
                renewed += 1;
            } else {
                self.leases.remove(&chunk);
                self.unindex(holder.mw_id, &chunk);
            }
        }
        renewed
    }

    /// Releases the given chunks of `holder`, or all of them when `chunks` is empty.
    pub fn release(&mut self, holder: Holder, chunks: &[ChunkKey]) -> u32 {
        self.changes += 1;
        let targets: Vec<ChunkKey> = if chunks.is_empty() {
            self.by_mw.get(&holder.mw_id).map(|s| s.iter().cloned().collect()).unwrap_or_default()
        } else {
            chunks.to_vec()
        };
        let mut released = 0;
        for chunk in targets {
            if self.leases.get(&chunk).is_some_and(|e| e.holder == holder) {
                self.leases.remove(&chunk);
                self.unindex(holder.mw_id, &chunk);
                released += 1;
            }
        }
        released
    }

    /// Moves every live lease of `from` to `to` in one step, each with a new generation so that writes from `from`
    /// are refused from now on.
    pub fn transfer(&mut self, from: Holder, to: Holder, now: Instant) -> Vec<Lease> {
        self.changes += 1;
        let Some(chunks) = self.by_mw.remove(&from.mw_id) else { return Vec::new() };
        let expires_at = now + self.ttl;
        let mut moved = Vec::new();
        for chunk in chunks {
            let live = self.leases.get(&chunk).is_some_and(|e| e.holder == from && e.live(now));
            if !live {
                if self.leases.get(&chunk).is_some_and(|e| e.holder == from) {
                    self.leases.remove(&chunk);
                }
                continue;
            }
            let entry = Entry { holder: to, generation: self.generation(), expires_at };
            moved.push(Self::lease(&chunk, &entry));
            self.leases.insert(chunk.clone(), entry);
            self.by_mw.entry(to.mw_id).or_default().insert(chunk);
        }
        moved
    }

    /// Every live lease of `world`.
    pub fn list(&self, world: &str, now: Instant) -> Vec<Lease> {
        self.leases.iter().filter(|(c, e)| c.world == world && e.live(now)).map(|(c, e)| Self::lease(c, e)).collect()
    }

    pub fn owner(&self, chunk: &ChunkKey, now: Instant) -> Option<Lease> {
        self.leases.get(chunk).filter(|e| e.live(now)).map(|e| Self::lease(chunk, e))
    }

    /// Fencing check for a chunk write: only the current, live generation may write.
    pub fn may_write(&self, chunk: &ChunkKey, generation: u64, now: Instant) -> bool {
        self.leases.get(chunk).is_some_and(|e| e.live(now) && e.generation == generation)
    }

    /// Whether `worker_id` holds a live lease on `chunk`, through any of its micro worlds: the fence for chunk
    /// writes.
    pub fn held_by_worker(&self, chunk: &ChunkKey, worker_id: u32, now: Instant) -> bool {
        self.leases.get(chunk).is_some_and(|e| e.live(now) && e.holder.worker_id == worker_id)
    }

    /// Drops every lease of a dead worker (docs/docs/10-nadiinist.md).
    pub fn release_worker(&mut self, worker_id: u32) -> u32 {
        self.changes += 1;
        let dead: Vec<(ChunkKey, u64)> = self
            .leases
            .iter()
            .filter(|(_, e)| e.holder.worker_id == worker_id)
            .map(|(c, e)| (c.clone(), e.holder.mw_id))
            .collect();
        for (chunk, mw_id) in &dead {
            self.leases.remove(chunk);
            self.unindex(*mw_id, chunk);
        }
        dead.len() as u32
    }

    /// Removes expired leases. Called on a timer; correctness never depends on it.
    pub fn sweep(&mut self, now: Instant) -> usize {
        let expired: Vec<(ChunkKey, u64)> =
            self.leases.iter().filter(|(_, e)| !e.live(now)).map(|(c, e)| (c.clone(), e.holder.mw_id)).collect();
        for (chunk, mw_id) in &expired {
            self.leases.remove(chunk);
            self.unindex(*mw_id, chunk);
        }
        expired.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(10);

    fn chunk(x: i32, z: i32) -> ChunkKey {
        ChunkKey { world: "world".into(), dimension: "overworld".into(), x, z }
    }

    fn holder(mw_id: u64, worker_id: u32) -> Holder {
        Holder { mw_id, worker_id }
    }

    #[test]
    fn a_restored_table_keeps_holders_and_fencing() {
        let mut t = LeaseTable::new(TTL);
        let now = Instant::now();
        let (a, b) = (holder(1, 1), holder(2, 2));
        t.acquire(a, &[chunk(0, 0), chunk(0, 1)], now).unwrap();
        let g = t.acquire(b, &[chunk(9, 9)], now).unwrap()[0].generation;
        let snapshot = t.snapshot(now + Duration::from_secs(4));

        let later = Instant::now();
        let mut back = LeaseTable::restore(TTL, snapshot, later);
        assert_eq!(back.owner(&chunk(0, 1), later).unwrap().holder, a);
        assert_eq!(back.owner(&chunk(9, 9), later).unwrap().generation, g);
        // About 6 s were left: a holder renewing in time keeps its leases; another's run out.
        assert_eq!(back.renew(a, later + Duration::from_secs(5)), 2);
        assert!(back.owner(&chunk(9, 9), later + Duration::from_secs(7)).is_none());
        // New generations stay above every old one.
        let fresh = back.acquire(holder(3, 3), &[chunk(9, 9)], later + Duration::from_secs(7)).unwrap();
        assert!(fresh[0].generation > g);
    }

    #[test]
    fn acquire_is_all_or_nothing() {
        let mut t = LeaseTable::new(TTL);
        let now = Instant::now();
        let a = holder(1, 1);
        let b = holder(2, 1);
        t.acquire(a, &[chunk(0, 0), chunk(0, 1)], now).unwrap();
        let err = t.acquire(b, &[chunk(5, 5), chunk(0, 1)], now).unwrap_err();
        assert_eq!(err, Conflict { chunk: chunk(0, 1), owner: a });
        assert!(t.owner(&chunk(5, 5), now).is_none(), "nothing granted on conflict");
    }

    #[test]
    fn reacquire_keeps_generation() {
        let mut t = LeaseTable::new(TTL);
        let now = Instant::now();
        let a = holder(1, 1);
        let g = t.acquire(a, &[chunk(0, 0)], now).unwrap()[0].generation;
        let again = t.acquire(a, &[chunk(0, 0), chunk(1, 0)], now).unwrap();
        assert_eq!(again[0].generation, g);
        assert_ne!(again[1].generation, g);
    }

    #[test]
    fn expired_lease_is_free_and_fenced() {
        let mut t = LeaseTable::new(TTL);
        let now = Instant::now();
        let a = holder(1, 1);
        let b = holder(2, 2);
        let old = t.acquire(a, &[chunk(0, 0)], now).unwrap()[0].generation;
        let later = now + TTL + Duration::from_millis(1);
        let new = t.acquire(b, &[chunk(0, 0)], later).unwrap()[0].generation;
        assert!(new > old);
        assert!(!t.may_write(&chunk(0, 0), old, later), "the stale owner may not write");
        assert!(t.may_write(&chunk(0, 0), new, later));
        assert_eq!(t.renew(a, later), 0, "the stale owner cannot renew");
    }

    #[test]
    fn renew_extends_and_drops_expired() {
        let mut t = LeaseTable::new(TTL);
        let now = Instant::now();
        let a = holder(1, 1);
        t.acquire(a, &[chunk(0, 0)], now).unwrap();
        let mid = now + TTL / 2;
        assert_eq!(t.renew(a, mid), 1);
        assert!(t.owner(&chunk(0, 0), now + TTL + Duration::from_secs(1)).is_some());
        let late = mid + TTL * 2;
        assert_eq!(t.renew(a, late), 0);
        assert!(t.is_empty());
    }

    #[test]
    fn transfer_moves_everything_with_new_generations() {
        let mut t = LeaseTable::new(TTL);
        let now = Instant::now();
        let a = holder(1, 1);
        let b = holder(2, 2);
        let before = t.acquire(b, &[chunk(0, 0), chunk(0, 1)], now).unwrap();
        let moved = t.transfer(b, a, now);
        assert_eq!(moved.len(), 2);
        for lease in &moved {
            assert_eq!(lease.holder, a);
            assert!(!t.may_write(&lease.chunk, before[0].generation, now));
            assert!(!t.may_write(&lease.chunk, before[1].generation, now));
        }
        assert_eq!(t.renew(b, now), 0);
        assert_eq!(t.renew(a, now), 2);
    }

    #[test]
    fn release_only_touches_own_leases() {
        let mut t = LeaseTable::new(TTL);
        let now = Instant::now();
        let a = holder(1, 1);
        let b = holder(2, 1);
        t.acquire(a, &[chunk(0, 0), chunk(0, 1)], now).unwrap();
        t.acquire(b, &[chunk(9, 9)], now).unwrap();
        assert_eq!(t.release(b, &[chunk(0, 0)]), 0);
        assert_eq!(t.release(a, &[chunk(0, 0)]), 1);
        assert_eq!(t.release(a, &[]), 1);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn writes_are_fenced_to_the_worker_holding_the_lease() {
        let mut t = LeaseTable::new(TTL);
        let now = Instant::now();
        t.acquire(holder(1, 7), &[chunk(0, 0)], now).unwrap();
        assert!(t.held_by_worker(&chunk(0, 0), 7, now));
        assert!(!t.held_by_worker(&chunk(0, 0), 8, now), "another worker");
        assert!(!t.held_by_worker(&chunk(1, 0), 7, now), "a chunk nobody leased");
        assert!(!t.held_by_worker(&chunk(0, 0), 7, now + TTL), "an expired lease");
    }

    #[test]
    fn release_worker_frees_all_its_micro_worlds() {
        let mut t = LeaseTable::new(TTL);
        let now = Instant::now();
        t.acquire(holder(1, 7), &[chunk(0, 0)], now).unwrap();
        t.acquire(holder(2, 7), &[chunk(5, 5)], now).unwrap();
        t.acquire(holder(3, 8), &[chunk(9, 9)], now).unwrap();
        assert_eq!(t.release_worker(7), 2);
        assert_eq!(t.len(), 1);
        t.acquire(holder(4, 8), &[chunk(0, 0)], now).unwrap();
    }

    #[test]
    fn sweep_removes_only_expired() {
        let mut t = LeaseTable::new(TTL);
        let now = Instant::now();
        t.acquire(holder(1, 1), &[chunk(0, 0)], now).unwrap();
        t.acquire(holder(2, 1), &[chunk(1, 1)], now + TTL).unwrap();
        assert_eq!(t.sweep(now + TTL + Duration::from_millis(1)), 1);
        assert_eq!(t.len(), 1);
    }
}
