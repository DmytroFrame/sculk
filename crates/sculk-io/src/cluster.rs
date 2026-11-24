//! What players share across engines, so the world feels like one server wherever its players are
//! (docs/docs/09-hlobalnyi-stan.md): chat, players joining and leaving, and the time of day. Each engine shows its
//! own players' messages; IO carries them to the others, as the engines' console would say them.

use crate::control::Shared;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the engines' clocks are set to the reference engine's.
const CLOCK_SYNC: Duration = Duration::from_secs(10);

/// How far an engine's clock may be from the reference's, in ticks.
const MAX_DRIFT: i64 = 100;
/// How far past its ticks a clock must jump to count as a skipped night.
const SKIP: i64 = 1_000;

impl Shared {
    /// An engine's clock: the overworld's time in ticks (26.3's `/time query time`: "Clock minecraft:overworld is at
    /// 963 tick(s)").
    async fn clock(&self, worker_id: u32) -> Option<i64> {
        let reply = self.engine_command(worker_id, "console time query time").await.ok()?;
        reply["output"]
            .as_array()?
            .iter()
            .filter_map(|l| l.as_str())
            .find_map(|l| l.split_whitespace().rev().find_map(|n| n.parse::<i64>().ok()))
    }

    /// An engine's weather: raining, and thundering (vanilla's `weather_check` predicate).
    async fn weather(&self, worker_id: u32) -> Option<(bool, bool)> {
        let check = async |what: &str| {
            let line = format!("console execute if predicate {{type:\"minecraft:weather_check\",{what}:true}}");
            let reply = self.engine_command(worker_id, &line).await.ok()?;
            let output = reply["output"].as_array()?.iter().filter_map(|l| l.as_str()).collect::<String>();
            Some(output.contains("passed"))
        };
        Some((check("raining").await?, check("thundering").await?))
    }

    /// Shows a text component to every player on the engines other than `except`.
    pub fn broadcast(self: &Arc<Self>, except: u32, component: serde_json::Value) {
        let engines: Vec<u32> = self.registry().engines_with_players(1).into_iter().filter(|&w| w != except).collect();
        if engines.is_empty() {
            return;
        }
        let line = format!("console tellraw @a {component}");
        for worker_id in engines {
            let (shared, line) = (self.clone(), line.clone());
            tokio::spawn(async move {
                if let Err(e) = shared.engine_command(worker_id, &line).await {
                    tracing::debug!(worker_id, error = format!("{e:#}"), "message not shown on an engine");
                }
            });
        }
    }

    /// A player's chat message, for the players of the other engines: `<name> message`, as vanilla's chat type.
    pub fn chat(self: &Arc<Self>, from: u32, name: &str, message: &str) {
        self.broadcast(from, serde_json::json!({ "translate": "chat.type.text", "with": [name, message] }));
    }

    /// `name joined the game` / `name left the game` for the players of the other engines.
    pub fn joined(self: &Arc<Self>, on: u32, name: &str, joined: bool) {
        let key = if joined { "multiplayer.player.joined" } else { "multiplayer.player.left" };
        self.broadcast(on, serde_json::json!({ "translate": key, "with": [name], "color": "yellow" }));
    }

    /// Keeps one time of day and one weather across the engines (docs/docs/09-hlobalnyi-stan.md, "What lives where"): the engine
    /// with the most players (the lowest ID among equals) keeps the time, the others are set to it when they drift.
    /// A night skipped on any engine (its players slept, by vanilla's rule among them) is skipped for all: an
    /// engine whose clock jumped ahead of what its ticks explain sets everyone's (docs/docs/13-obmezhennia.md, the
    /// open question of sleeping on a big server, answered per micro world).
    pub async fn sync_clocks(self: Arc<Self>) {
        let mut every = tokio::time::interval(CLOCK_SYNC);
        let mut last: HashMap<u32, (i64, Instant)> = HashMap::new();
        loop {
            every.tick().await;
            let engines = self.registry().engines_with_players(1);
            last.retain(|w, _| engines.contains(w));
            if engines.len() < 2 {
                last.clear();
                continue;
            }
            let Some(reference) = self.registry().primary_engine() else { continue };
            let mut times = HashMap::new();
            for &worker_id in &engines {
                if let Some(time) = self.clock(worker_id).await {
                    times.insert(worker_id, time);
                }
            }
            let skipped = times.iter().find(|(w, t)| {
                last.get(*w).is_some_and(|(before, at)| {
                    let ticked = (at.elapsed().as_millis() / 50) as i64;
                    *t - before > ticked + SKIP
                })
            });
            if let Some(time) = skipped.map(|(_, t)| *t).or_else(|| times.get(&reference).copied()) {
                for (&worker_id, &own) in &times {
                    // Set only when it drifted: setting it tells the operators.
                    if (own - time).abs() > MAX_DRIFT {
                        let _ = self.engine_command(worker_id, &format!("console time set {time}")).await;
                    }
                    last.insert(worker_id, (time, Instant::now()));
                }
            }
            // One weather too: the reference's, set where it differs.
            let Some(weather) = self.weather(reference).await else { continue };
            for &worker_id in engines.iter().filter(|&&w| w != reference) {
                if self.weather(worker_id).await.is_some_and(|own| own != weather) {
                    let name = match weather {
                        (_, true) => "thunder",
                        (true, false) => "rain",
                        (false, false) => "clear",
                    };
                    let _ = self.engine_command(worker_id, &format!("console weather {name}")).await;
                }
            }
        }
    }
}
