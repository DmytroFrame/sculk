//! The engine's world files besides its chunks (level.dat, players, data/) kept in AnvilDB: pulled into the engine's
//! world folder before it starts, a player's own pulled again before the player arrives from another worker, and
//! every file the engine saves pushed back (docs/docs/09-hlobalnyi-stan.md).
//!
//! The server's lists (ops.json, whitelist.json, bans) beside the engine's server.properties go the same way, as
//! AnvilDB's `server/` files: an operator made on one worker is an operator on all of them.
//!
//! Chunks never touch the engine's folder: its chunk store reads and writes them in AnvilDB directly.

use anyhow::Context;
use sculk_anvildb::files::{SERVER_LISTS, hash, is_world_file, walk};
use sculk_proto::anvildb::{GetFileRequest, ListFilesRequest, PutFilesRequest, WorldFile, files_client::FilesClient};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;
use tonic::transport::Channel;

/// gRPC messages carry whole files.
const MAX_MESSAGE: usize = 64 << 20;

pub struct WorldFiles {
    client: FilesClient<Channel>,
    world: String,
    /// The engine's world folder.
    dir: PathBuf,
    /// The engine's server folder, where its lists are.
    server_dir: PathBuf,
    worker_id: u32,
    /// The hash of each file as AnvilDB has it, as far as this worker knows: what was pulled or pushed last.
    synced: HashMap<String, u64>,
    /// Each local file's modification time and size when it was last hashed, so unchanged files aren't read again.
    seen: HashMap<String, (SystemTime, u64)>,
}

impl WorldFiles {
    pub fn new(anvildb: &str, world: &str, server_dir: PathBuf, dir: PathBuf, worker_id: u32) -> anyhow::Result<Self> {
        let channel = tonic::transport::Endpoint::from_shared(anvildb.to_owned())?.connect_lazy();
        let client =
            FilesClient::new(channel).max_decoding_message_size(MAX_MESSAGE).max_encoding_message_size(MAX_MESSAGE);
        Ok(Self {
            client,
            world: world.to_owned(),
            dir,
            server_dir,
            worker_id,
            synced: HashMap::new(),
            seen: HashMap::new(),
        })
    }

    fn local(&self, path: &str) -> PathBuf {
        match path.strip_prefix("server/") {
            Some(list) => self.server_dir.join(list),
            None => path.split('/').fold(self.dir.clone(), |p, part| p.join(part)),
        }
    }

    /// Brings every file AnvilDB has under `prefix` into the engine's folder where the local copy differs. Files
    /// only this worker has stay, and are pushed by the next [`push`](Self::push).
    pub async fn pull(&mut self, prefix: &str) -> anyhow::Result<usize> {
        let request = ListFilesRequest { world: self.world.clone(), prefix: prefix.to_owned() };
        let files = self.client.list(request).await.context("listing world files in AnvilDB")?.into_inner().files;
        let mut pulled = 0;
        for f in files {
            let local = self.local(&f.path);
            let same = std::fs::read(&local).map(|d| hash(&d) == f.hash).unwrap_or(false);
            if !same {
                let request = GetFileRequest { world: self.world.clone(), path: f.path.clone() };
                let Some(data) = self.client.get(request).await?.into_inner().data else { continue };
                if let Some(dir) = local.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let temp = local.with_extension("sculk-tmp");
                std::fs::write(&temp, &data)?;
                std::fs::rename(&temp, &local)?;
                pulled += 1;
            }
            self.synced.insert(f.path.clone(), f.hash);
            self.seen.remove(&f.path);
        }
        Ok(pulled)
    }

    /// A player's files: data, stats and advancements, from AnvilDB.
    pub async fn pull_player(&mut self, uuid: u128) -> anyhow::Result<usize> {
        let id = horizon_protocol::crypto::uuid_string(uuid);
        let mut pulled = 0;
        for folder in ["data", "stats", "advancements"] {
            let ext = if folder == "data" { "dat" } else { "json" };
            pulled += self.pull(&format!("players/{folder}/{id}.{ext}")).await?;
        }
        Ok(pulled)
    }

    /// Sends AnvilDB every world file the engine changed since the last push.
    pub async fn push(&mut self) -> anyhow::Result<usize> {
        let mut changed: Vec<(String, PathBuf, SystemTime, u64)> = Vec::new();
        walk(&self.dir, &self.dir, &mut |rel, path| {
            if !is_world_file(rel) {
                return Ok(());
            }
            let meta = std::fs::metadata(path)?;
            let stamp = (meta.modified()?, meta.len());
            if self.seen.get(rel) != Some(&stamp) {
                changed.push((rel.to_owned(), path.to_owned(), stamp.0, stamp.1));
            }
            Ok(())
        })?;
        for list in SERVER_LISTS {
            let (rel, path) = (format!("server/{list}"), self.server_dir.join(list));
            if let Ok(meta) = std::fs::metadata(&path) {
                let stamp = (meta.modified()?, meta.len());
                if self.seen.get(&rel) != Some(&stamp) {
                    changed.push((rel, path, stamp.0, stamp.1));
                }
            }
        }
        let mut files = Vec::new();
        for (rel, path, modified, len) in changed {
            let data = match std::fs::read(&path) {
                Ok(data) => data,
                // Replaced or removed by the engine since the walk: the next push sees it.
                Err(_) => continue,
            };
            let h = hash(&data);
            self.seen.insert(rel.clone(), (modified, len));
            if self.synced.get(&rel) != Some(&h) {
                self.synced.insert(rel.clone(), h);
                files.push(WorldFile { path: rel, data });
            }
        }
        if files.is_empty() {
            return Ok(0);
        }
        let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let request = PutFilesRequest { world: self.world.clone(), files, worker_id: self.worker_id };
        if let Err(e) = self.client.put(request).await {
            // Pushed again next time.
            for path in &paths {
                self.seen.remove(path);
                self.synced.remove(path);
            }
            return Err(anyhow::anyhow!("saving world files in AnvilDB: {}", e.message()));
        }
        Ok(paths.len())
    }
}
