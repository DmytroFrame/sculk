//! Micro worlds merging across engines (docs/docs/03-mikrosvity.md, "Merge"): an engine that wants a chunk another
//! engine holds tells IO, and IO moves that engine's players near the chunk to the owner. Each move is Horizon's
//! hand-over (docs/server.md, "A hand-over, in order"): the player is expected on the new engine and frozen on the
//! old one, the client is switched over (`proxy`), its record imported and placed on the new engine, and only then
//! released from the old one. A step that fails is undone, and the player plays on where it was.

use crate::control::Shared;
use crate::proxy::Command;
use anyhow::{Context, bail};
use horizon_protocol::crypto::uuid_string;
use sculk_proto::cluster::{ConflictReport, EngineCommand, IoMessage, io_message};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// How long a worker's engine may take to answer one request.
const ENGINE_ANSWER: Duration = Duration::from_secs(15);
/// A player that moved stays put this long, so two micro worlds at their border don't pass it back and forth.
const COOLDOWN: Duration = Duration::from_secs(15);
/// Players of the asking engine this close to the chunk (in chunks) move: those whose view reached it.
const REACH: i32 = 16;
/// How often engines are checked for micro worlds to split.
const SPLIT_CHECK: Duration = Duration::from_secs(5);

/// When a micro world splits.
#[derive(Debug, Clone, Copy)]
pub struct Split {
    /// Chunks between two groups' zones.
    pub gap: i32,
    /// How long the gap must last.
    pub after: Duration,
    /// The radius of a player's zone, in chunks.
    pub view_distance: i32,
    /// An engine slower than this (milliseconds a tick, on average) splits without waiting.
    pub overload_mspt: f32,
}

/// A UUID as the engine writes it, with or without dashes.
pub fn parse_uuid(text: &str) -> Option<u128> {
    u128::from_str_radix(&text.replace('-', ""), 16).ok()
}

/// How long the new engine may take to have the player's login held, ready for its record.
const HELD: Duration = Duration::from_secs(10);

impl Shared {
    /// Sends a request to a worker's engine and returns its answer; an `{"error": ...}` answer is an error.
    pub async fn engine_command(&self, worker_id: u32, line: &str) -> anyhow::Result<Value> {
        let worker = self.registry().worker(worker_id).with_context(|| format!("worker #{worker_id} is gone"))?;
        let (request_id, answer) = self.registry().engine_request();
        let msg = EngineCommand { request_id, line: line.to_owned() };
        if worker.send(Ok(IoMessage { msg: Some(io_message::Msg::EngineCommand(msg)) })).await.is_err() {
            bail!("worker #{worker_id} is gone");
        }
        let reply = tokio::time::timeout(ENGINE_ANSWER, answer)
            .await
            .with_context(|| format!("worker #{worker_id}'s engine did not answer"))?
            .with_context(|| format!("worker #{worker_id} is gone"))?;
        if !reply.error.is_empty() {
            bail!("worker #{worker_id}: {}", reply.error);
        }
        let value: Value = serde_json::from_str(&reply.reply)
            .with_context(|| format!("worker #{worker_id}'s engine answered {:?}", reply.reply))?;
        if let Some(error) = value.get("error") {
            bail!("worker #{worker_id}'s engine: {}", error.as_str().unwrap_or_default());
        }
        Ok(value)
    }

    /// An engine wanted a chunk another engine holds. If the owner has players near it, their micro worlds met: the
    /// players of one near the chunk join the other, the one with more players taking the other's, the lower worker
    /// ID among equals, so two engines that meet at once agree on the direction (docs/docs/03-mikrosvity.md,
    /// "Simultaneous merge"). If not, the owner holds the chunk only until its last saves are done (its players left
    /// or moved away), and the asking engine should wait for it: the answer.
    pub async fn on_conflict(self: Arc<Self>, report: ConflictReport) -> bool {
        let (asking, owner) = (report.worker_id, report.owner_worker_id);
        let engines = {
            let registry = self.registry();
            registry.engine(asking).is_some() && registry.engine(owner).is_some()
        };
        if asking == owner || !engines {
            // A built-in world's micro world, or a worker gone: its leases end by themselves.
            return true;
        }
        let owner_near = match self.players_near(owner, &report).await {
            // Players moving away (a split) are leaving the chunks to whoever asks.
            Ok(near) => near.into_iter().filter(|uuid| !self.is_moving(*uuid)).collect::<Vec<_>>(),
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), owner, "no status from the engine holding the chunk");
                return false;
            }
        };
        if owner_near.is_empty() {
            return true;
        }
        let (from, to) = {
            let registry = self.registry();
            let (a, o) = (registry.players_on(asking).len(), registry.players_on(owner).len());
            if (a, std::cmp::Reverse(asking)) > (o, std::cmp::Reverse(owner)) {
                (owner, asking)
            } else {
                (asking, owner)
            }
        };
        // One merge of two engines at a time; the reports that come meanwhile are the same meeting.
        let pair = (from.min(to), from.max(to));
        if self.merging.lock().unwrap_or_else(|e| e.into_inner()).insert(pair) {
            tokio::spawn(async move {
                self.clone().merge(from, to, &report).await;
                self.merging.lock().unwrap_or_else(|e| e.into_inner()).remove(&pair);
            });
        }
        false
    }

    /// The players of a worker's engine whose view reaches the reported chunk.
    async fn players_near(&self, worker_id: u32, report: &ConflictReport) -> anyhow::Result<Vec<u128>> {
        let status = self.engine_command(worker_id, "server-status").await?;
        Ok(status["players"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["dimension"].as_str() == Some(report.dimension.as_str()))
            .filter(|p| {
                let at = |i: usize| p["position"][i].as_f64().map(|v| (v.floor() as i32) >> 4);
                matches!((at(0), at(2)), (Some(x), Some(z)) if (x - report.x).abs() <= REACH && (z - report.z).abs() <= REACH)
            })
            .filter_map(|p| p["uuid"].as_str().and_then(parse_uuid))
            .collect())
    }

    /// Moves `from`'s players near the reported chunk to `to`.
    async fn merge(self: Arc<Self>, from: u32, to: u32, report: &ConflictReport) {
        let near = match self.players_near(from, report).await {
            Ok(near) => near,
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "no status from the engine that met another");
                return;
            }
        };
        self.move_all(&near, from, to, "micro worlds merged: player moved").await;
    }

    /// Moves these players of `from` to `to`, at once, and waits for every move.
    pub async fn move_all(self: &Arc<Self>, players: &[u128], from: u32, to: u32, what: &'static str) {
        let mut moves = tokio::task::JoinSet::new();
        for &uuid in players {
            let Some(session_id) = self.registry().session_of(uuid) else { continue };
            if self.registry().session_worker(session_id) != Some(from) || !self.start_move(uuid) {
                continue;
            }
            let shared = self.clone();
            moves.spawn(async move {
                let name = uuid_string(uuid);
                match shared.move_player(session_id, uuid, from, to).await {
                    Ok(()) => tracing::info!(player = %name, from, to, what),
                    Err(e) => tracing::warn!(player = %name, from, to, error = format!("{e:#}"), "player not moved"),
                }
            });
        }
        moves.join_all().await;
    }

    /// A worker is stopping: its engine's players move to the other engines, the least loaded first, so a restart
    /// disconnects nobody. Returns how many moved and how many stayed (nowhere to go, or a move failed).
    pub async fn drain(self: &Arc<Self>, worker_id: u32) -> (u32, u32) {
        self.registry().set_draining(worker_id);
        let players: Vec<u128> = self.registry().players_on(worker_id).into_iter().map(|(_, uuid, _)| uuid).collect();
        let mut moved = 0;
        for uuid in &players {
            let target = {
                let registry = self.registry();
                let mut engines: Vec<(u32, usize)> = registry
                    .open_engines()
                    .into_iter()
                    .filter(|&w| w != worker_id)
                    .map(|w| (w, registry.players_on(w).len()))
                    .collect();
                engines.sort_by_key(|&(w, n)| (n, w));
                engines.first().map(|&(w, _)| w)
            };
            let Some(to) = target else { break };
            // A draining worker's players move even if they moved lately.
            self.moves.lock().unwrap_or_else(|e| e.into_inner()).remove(uuid);
            let Some(session_id) = self.registry().session_of(*uuid) else { continue };
            self.start_move(*uuid);
            match self.move_player(session_id, *uuid, worker_id, to).await {
                Ok(()) => moved += 1,
                Err(e) => {
                    tracing::warn!(player = %uuid_string(*uuid), error = format!("{e:#}"), "not moved off the draining worker")
                }
            }
        }
        (moved, players.len() as u32 - moved)
    }

    /// The engine worker a player (by name, in any case) is on.
    pub fn engine_of(&self, name: &str) -> Option<u32> {
        let registry = self.registry();
        registry.cluster_players().into_iter().find(|p| p.1.eq_ignore_ascii_case(name)).map(|p| p.3)
    }

    /// Moves a player to the engine where `target` (a player's name) is, so the two can meet.
    pub async fn join_player(self: &Arc<Self>, uuid: u128, target: &str) -> anyhow::Result<()> {
        let to = self.engine_of(target).context("no such player")?;
        let session_id = self.registry().session_of(uuid).context("the player left")?;
        let from = self.registry().session_worker(session_id).context("the player left")?;
        if from == to {
            return Ok(());
        }
        // A player who asks to go somewhere goes, even if it moved lately.
        self.moves.lock().unwrap_or_else(|e| e.into_inner()).remove(&uuid);
        self.start_move(uuid);
        self.move_player(session_id, uuid, from, to).await
    }

    /// Whether this player is moving between engines or moved lately.
    fn is_moving(&self, uuid: u128) -> bool {
        let moves = self.moves.lock().unwrap_or_else(|e| e.into_inner());
        moves.get(&uuid).is_some_and(|at| at.elapsed() < COOLDOWN)
    }

    /// Splits micro worlds whose players went apart (docs/docs/03-mikrosvity.md, "Split"): every `every`, each
    /// engine's players are grouped by the distance between their zones. When the zones of two groups have stayed at
    /// least `split.gap` chunks apart for `split.after`, the groups without the micro world's founder (the player
    /// there longest) move to engines with nobody on them, one engine per group.
    pub async fn split_micro_worlds(self: Arc<Self>, split: Split) {
        let mut apart_since: HashMap<u32, Instant> = HashMap::new();
        let mut every = tokio::time::interval(SPLIT_CHECK);
        loop {
            every.tick().await;
            let engines: Vec<u32> = self.registry().engines_with_players(2);
            apart_since.retain(|w, _| engines.contains(w));
            for worker_id in engines {
                let groups = match self.groups(worker_id, &split).await {
                    Ok(groups) if groups.len() >= 2 => groups,
                    _ => {
                        apart_since.remove(&worker_id);
                        continue;
                    }
                };
                let since = *apart_since.entry(worker_id).or_insert_with(Instant::now);
                // An engine that can't keep up splits at once: its micro worlds spread over the engines.
                let overloaded = self.registry().mspt(worker_id) > split.overload_mspt;
                if since.elapsed() < split.after && !overloaded {
                    continue;
                }
                if overloaded {
                    tracing::info!(worker_id, "engine overloaded; its micro worlds spread out");
                }
                apart_since.remove(&worker_id);
                // The founder's group stays: the one with the player whose session is oldest here.
                let oldest = |group: &Vec<(u64, u128)>| group.iter().map(|(s, _)| *s).min().unwrap_or(u64::MAX);
                let mut groups = groups;
                groups.sort_by_key(oldest);
                for group in groups.into_iter().skip(1) {
                    let Some(to) = self.registry().empty_engine(worker_id) else {
                        tracing::info!(worker_id, "micro world not split: no engine has room");
                        break;
                    };
                    let players: Vec<u128> = group.iter().map(|(_, uuid)| *uuid).collect();
                    tracing::info!(from = worker_id, to, players = players.len(), "micro world splits");
                    self.move_all(&players, worker_id, to, "micro world split: player moved").await;
                }
            }
        }
    }

    /// A worker's players grouped so that two groups' zones are at least `split.gap` chunks apart: session and UUID.
    async fn groups(&self, worker_id: u32, split: &Split) -> anyhow::Result<Vec<Vec<(u64, u128)>>> {
        let status = self.engine_command(worker_id, "server-status").await?;
        let players: Vec<(u64, u128, String, i32, i32)> = status["players"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let uuid = parse_uuid(p["uuid"].as_str()?)?;
                let session = self.registry().session_of(uuid)?;
                let at = |i: usize| p["position"][i].as_f64().map(|v| (v.floor() as i32) >> 4);
                Some((session, uuid, p["dimension"].as_str()?.to_owned(), at(0)?, at(2)?))
            })
            .filter(|(_, uuid, ..)| !self.is_moving(*uuid))
            .collect();
        // Two players are together while their zones (view distance around each) come closer than the gap.
        let near = 2 * split.view_distance + split.gap;
        let mut group = vec![usize::MAX; players.len()];
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for start in 0..players.len() {
            if group[start] != usize::MAX {
                continue;
            }
            let id = groups.len();
            let mut members = vec![start];
            group[start] = id;
            let mut i = 0;
            while i < members.len() {
                let (_, _, d, x, z) = &players[members[i]];
                for (j, (_, _, dj, xj, zj)) in players.iter().enumerate() {
                    if group[j] == usize::MAX && dj == d && (xj - x).abs().max((zj - z).abs()) < near {
                        group[j] = id;
                        members.push(j);
                    }
                }
                i += 1;
            }
            groups.push(members);
        }
        Ok(groups.into_iter().map(|g| g.into_iter().map(|i| (players[i].0, players[i].1)).collect()).collect())
    }

    /// Whether a move of this player may start now; marks it moving.
    fn start_move(&self, uuid: u128) -> bool {
        let mut moves = self.moves.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        moves.retain(|_, at| now.duration_since(*at) < COOLDOWN);
        if moves.contains_key(&uuid) {
            return false;
        }
        moves.insert(uuid, now);
        true
    }

    /// Moves one player from `from`'s engine to `to`'s.
    pub async fn move_player(&self, session_id: u64, uuid: u128, from: u32, to: u32) -> anyhow::Result<()> {
        let id = uuid_string(uuid);
        let (engine, commands) = {
            let registry = self.registry();
            (registry.engine(to), registry.commands(session_id))
        };
        let (Some(engine), Some(commands)) = (engine, commands) else { bail!("the player or the engine is gone") };
        // The new engine runs (it starts with its first player) and has the player's files.
        crate::minecraft::prepare_player(self, to, uuid).await.map_err(|e| anyhow::anyhow!(e))?;
        let deadline = Instant::now() + HELD;
        loop {
            match self.engine_command(to, &format!("player-expect {id} 15")).await {
                Ok(_) => break,
                // An engine just started has no control endpoint for a moment.
                Err(e) if Instant::now() < deadline && format!("{e:#}").contains("control endpoint") => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(e) => return Err(e).context("expecting the player"),
            }
        }
        let export = match self.engine_command(from, &format!("player-export {id} freeze")).await {
            Ok(export) => export,
            Err(e) => {
                let _ = self.engine_command(to, &format!("player-drop {id}")).await;
                return Err(e).context("freezing the player");
            }
        };
        let record = export["record"].clone();
        let handover = record["handover"].as_str().context("the export has no hand-over")?.to_owned();
        let undo = async |why: anyhow::Error| -> anyhow::Result<()> {
            let _ = self.engine_command(to, &format!("player-drop {id}")).await;
            let _ = self.engine_command(from, &format!("player-release {id} {handover} abort")).await;
            Err(why)
        };

        let (done, switched) = oneshot::channel();
        if commands.send(Command::Move { worker_id: to, engine, done }).await.is_err() {
            return undo(anyhow::anyhow!("the player left")).await;
        }
        match switched.await {
            Ok(Ok(())) => {}
            Ok(Err((reason, _))) => return undo(anyhow::anyhow!(reason)).await,
            Err(_) => return undo(anyhow::anyhow!("the player left")).await,
        }

        // The client is on the new engine, which holds its login once the configuration is done.
        // A move is no arrival: nobody is told the player joined or left.
        let import = serde_json::json!({ "record": record, "place": true, "announce": false }).to_string();
        let deadline = Instant::now() + HELD;
        loop {
            match self.engine_command(to, &format!("player-import {id} {import}")).await {
                Ok(_) => break,
                Err(e) if Instant::now() < deadline && format!("{e:#}").contains("held") => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => return undo(e.context("placing the player on the new engine")).await,
            }
        }
        self.registry().move_session(session_id, to);
        if let Err(e) = self.engine_command(from, &format!("player-release {id} {handover} moved")).await {
            // The new engine has the player and its file; the old one saves its frozen copy as the player leaves.
            tracing::warn!(error = format!("{e:#}"), "the old engine did not let go of the moved player");
        }
        let _ = commands.send(Command::Release).await;
        Ok(())
    }
}
