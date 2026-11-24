//! Minecraft 26.3 game data that Sculk reads instead of shipping: the synchronized registries (entry names in
//! network order) and their tags. `sculk prepare` takes them out of Mojang's `server.jar` ([`prepare`]); nothing of
//! Mojang's is in this repository.

mod prepare;
mod tags;

pub use prepare::{PrepareSource, prepare, prepare_with};

use anyhow::{Context, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The game version the data must come from.
pub const GAME_VERSION: &str = "26.3";

/// RegistryDataLoader.SYNCHRONIZED_REGISTRIES of 26.3, in vanilla's order (from Horizon's `lan/data.rs`).
pub const SYNCHRONIZED: &[&str] = &[
    "minecraft:worldgen/biome",
    "minecraft:chat_type",
    "minecraft:trim_pattern",
    "minecraft:trim_material",
    "minecraft:wolf_variant",
    "minecraft:wolf_sound_variant",
    "minecraft:pig_variant",
    "minecraft:pig_sound_variant",
    "minecraft:frog_variant",
    "minecraft:cat_variant",
    "minecraft:cat_sound_variant",
    "minecraft:cow_sound_variant",
    "minecraft:cow_variant",
    "minecraft:chicken_sound_variant",
    "minecraft:chicken_variant",
    "minecraft:zombie_nautilus_variant",
    "minecraft:painting_variant",
    "minecraft:sulfur_cube_archetype",
    "minecraft:dimension_type",
    "minecraft:damage_type",
    "minecraft:banner_pattern",
    "minecraft:enchantment",
    "minecraft:jukebox_song",
    "minecraft:instrument",
    "minecraft:test_environment",
    "minecraft:test_instance",
    "minecraft:dialog",
    "minecraft:world_clock",
    "minecraft:timeline",
    "minecraft:decorated_pot_pattern",
    "minecraft:block_transformer",
    "minecraft:worldgen/block_state_provider",
];

/// Built-in registries whose tags the client needs: synchronized entries point at them (`dimension_type` at
/// `#minecraft:infiniburn_overworld`, enchantments at `#minecraft:enchantable/*`), and a client that lacks a tag
/// refuses the registries.
pub const STATIC_TAGGED: &[&str] = &[
    "minecraft:block",
    "minecraft:item",
    "minecraft:entity_type",
    "minecraft:fluid",
    "minecraft:game_event",
    "minecraft:potion",
    "minecraft:point_of_interest_type",
];

/// Where `sculk prepare` puts the data by default.
pub fn default_dir() -> PathBuf {
    PathBuf::from("game-data")
}

/// A synchronized registry: entry names in the order they are sent, which is their network ID.
#[derive(Debug, Clone)]
pub struct Registry {
    pub name: String,
    pub entries: Vec<String>,
    index: HashMap<String, i32>,
}

impl Registry {
    pub fn new(name: impl Into<String>, entries: Vec<String>) -> Self {
        let index = entries.iter().enumerate().map(|(i, e)| (e.clone(), i as i32)).collect();
        Self { name: name.into(), entries, index }
    }

    pub fn id(&self, entry: &str) -> Option<i32> {
        self.index.get(entry).copied()
    }
}

/// One registry's tags, resolved to network IDs.
#[derive(Debug, Clone)]
pub struct RegistryTags {
    pub registry: String,
    pub tags: Vec<(String, Vec<i32>)>,
}

#[derive(Debug, Clone)]
pub struct GameData {
    pub version: String,
    pub registries: Vec<Registry>,
    pub tags: Vec<RegistryTags>,
    pub blocks: blocks::Blocks,
    /// Whether the generator's reports were found: without them, built-in tags have no members.
    pub has_reports: bool,
}

/// `minecraft:worldgen/biome` -> `worldgen/biome`.
fn registry_path(registry: &str) -> &str {
    registry.strip_prefix("minecraft:").unwrap_or(registry)
}

/// Every `*.json` under `dir` as its path relative to `dir` without the extension, in vanilla's resource order
/// (sorted by the file name with its extension).
fn json_files(dir: &Path) -> anyhow::Result<Vec<String>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else if path.extension().is_some_and(|e| e == "json") {
                let rel = path.strip_prefix(root).expect("walked from root");
                let rel: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                out.push(rel.join("/"));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    if dir.is_dir() {
        walk(dir, dir, &mut files).with_context(|| format!("reading {}", dir.display()))?;
    }
    files.sort();
    Ok(files.into_iter().map(|f| f.trim_end_matches(".json").to_owned()).collect())
}

impl GameData {
    /// Reads what `sculk prepare` wrote to `dir`.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let version_file = dir.join("version.json");
        let version: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&version_file)
                .with_context(|| format!("{} not found: run `sculk prepare` first", version_file.display()))?,
        )?;
        let version = version["id"].as_str().unwrap_or_default().to_owned();
        if version != GAME_VERSION {
            bail!("{} holds data of Minecraft {version}; Sculk needs {GAME_VERSION}", dir.display());
        }

        let data = dir.join("data/minecraft");
        let mut registries = Vec::new();
        for &name in SYNCHRONIZED {
            let entries =
                json_files(&data.join(registry_path(name)))?.into_iter().map(|e| format!("minecraft:{e}")).collect();
            registries.push(Registry::new(name, entries));
        }
        let mut tags: Vec<RegistryTags> = registries
            .iter()
            .map(|r| tags::load(&data.join("tags").join(registry_path(&r.name)), &r.name, Some(r)))
            .collect::<anyhow::Result<_>>()?;

        let reports = dir.join("reports");
        let builtin = load_builtin(&reports.join("registries.json"))?;
        for &name in STATIC_TAGGED {
            let registry = builtin.as_ref().and_then(|b| b.get(name));
            tags.push(tags::load(&data.join("tags").join(registry_path(name)), name, registry)?);
        }
        let blocks = match std::fs::read_to_string(reports.join("blocks.json")) {
            Ok(text) => blocks::Blocks::from_report(&serde_json::from_str(&text)?)?,
            Err(_) => blocks::Blocks::default(),
        };
        Ok(Self { version, registries, tags, blocks, has_reports: builtin.is_some() })
    }

    pub fn registry(&self, name: &str) -> Option<&Registry> {
        self.registries.iter().find(|r| r.name == name)
    }

    pub fn id(&self, registry: &str, entry: &str) -> Option<i32> {
        self.registry(registry)?.id(entry)
    }
}

/// The built-in registries from the generator's `reports/registries.json`, by network ID (`protocol_id`).
fn load_builtin(path: &Path) -> anyhow::Result<Option<HashMap<String, Registry>>> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(None) };
    let json: serde_json::Value = serde_json::from_str(&text).with_context(|| path.display().to_string())?;
    let mut out = HashMap::new();
    for (name, registry) in json.as_object().context("registries report is not an object")? {
        let Some(entries) = registry["entries"].as_object() else { continue };
        let mut by_id: Vec<(i64, String)> =
            entries.iter().map(|(e, v)| (v["protocol_id"].as_i64().unwrap_or(-1), e.clone())).collect();
        by_id.sort();
        if by_id.iter().enumerate().any(|(i, (id, _))| *id != i as i64) {
            bail!("{name} in the registries report has gaps in its protocol IDs");
        }
        out.insert(name.clone(), Registry::new(name.clone(), by_id.into_iter().map(|(_, e)| e).collect()));
    }
    Ok(Some(out))
}

/// Where players appear: the world spawn, until it is global state in AnvilDB (PLAN.md, block C).
pub const SPAWN: [f64; 3] = [0.5, -60.0, 0.5];
pub const SPAWN_DIMENSION: &str = "minecraft:overworld";

/// Block state IDs of 26.3 for the blocks Sculk places itself, until the block report is read
/// (`net.minecraft.data.Main --reports`). These are the first states of the block registry, unchanged for years.
pub mod blocks {
    use anyhow::Context;

    /// The block states Sculk places, from the block report when there is one.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Blocks {
        pub air: i32,
        pub stone: i32,
        pub grass_block: i32,
        pub dirt: i32,
        /// Bits of the global block state palette.
        pub global_bits: u8,
    }

    impl Default for Blocks {
        fn default() -> Self {
            Self { air: AIR, stone: STONE, grass_block: GRASS_BLOCK, dirt: DIRT, global_bits: GLOBAL_BITS }
        }
    }

    impl Blocks {
        /// Reads `reports/blocks.json`: each block's default state.
        pub fn from_report(report: &serde_json::Value) -> anyhow::Result<Self> {
            let default = |name: &str| -> anyhow::Result<i32> {
                let states =
                    report[name]["states"].as_array().with_context(|| format!("{name} not in the block report"))?;
                let state = states.iter().find(|s| s["default"] == true).or(states.first()).context("no states")?;
                Ok(state["id"].as_i64().context("state without id")? as i32)
            };
            let max = report
                .as_object()
                .context("block report is not an object")?
                .values()
                .flat_map(|b| b["states"].as_array().into_iter().flatten())
                .filter_map(|s| s["id"].as_i64())
                .max()
                .unwrap_or(0);
            Ok(Self {
                air: default("minecraft:air")?,
                stone: default("minecraft:stone")?,
                grass_block: default("minecraft:grass_block")?,
                dirt: default("minecraft:dirt")?,
                global_bits: (u64::BITS - (max as u64).leading_zeros()) as u8,
            })
        }
    }

    pub const AIR: i32 = 0;
    pub const STONE: i32 = 1;
    /// `grass_block[snowy=false]`.
    pub const GRASS_BLOCK: i32 = 9;
    pub const DIRT: i32 = 10;
    /// Bits of the global block state palette (more than 16 384 states).
    pub const GLOBAL_BITS: u8 = 15;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn loads_registries_in_resource_order() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "version.json", r#"{"id": "26.3"}"#);
        for name in ["plains", "ocean", "deep_ocean", "ocean_b"] {
            write(dir.path(), &format!("data/minecraft/worldgen/biome/{name}.json"), "{}");
        }
        write(dir.path(), "data/minecraft/dialog/sub/inner.json", "{}");
        write(dir.path(), "data/minecraft/dimension_type/overworld.json", "{}");

        let data = GameData::load(dir.path()).unwrap();
        let biomes = data.registry("minecraft:worldgen/biome").unwrap();
        // "ocean.json" sorts before "ocean_b.json" ('.' < '_').
        assert_eq!(
            biomes.entries,
            ["minecraft:deep_ocean", "minecraft:ocean", "minecraft:ocean_b", "minecraft:plains"]
        );
        assert_eq!(data.id("minecraft:worldgen/biome", "minecraft:plains"), Some(3));
        assert_eq!(data.registry("minecraft:dialog").unwrap().entries, ["minecraft:sub/inner"]);
        assert_eq!(data.id("minecraft:dimension_type", "minecraft:overworld"), Some(0));
        assert!(data.registry("minecraft:chat_type").unwrap().entries.is_empty());
    }

    #[test]
    fn builtin_tags_without_a_report_have_names_only() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "version.json", r#"{"id": "26.3"}"#);
        write(
            dir.path(),
            "data/minecraft/tags/block/infiniburn_overworld.json",
            r#"{"values": ["minecraft:netherrack"]}"#,
        );
        let data = GameData::load(dir.path()).unwrap();
        assert!(!data.has_reports);
        let blocks = data.tags.iter().find(|t| t.registry == "minecraft:block").unwrap();
        assert_eq!(blocks.tags, [("minecraft:infiniburn_overworld".to_string(), vec![])]);
        assert_eq!(data.blocks, blocks::Blocks::default());
    }

    #[test]
    fn builtin_tags_resolve_with_a_report() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "version.json", r#"{"id": "26.3"}"#);
        write(
            dir.path(),
            "data/minecraft/tags/block/infiniburn_overworld.json",
            r#"{"values": ["minecraft:netherrack", "minecraft:magma_block"]}"#,
        );
        write(
            dir.path(),
            "reports/registries.json",
            r#"{"minecraft:block": {"entries": {"minecraft:air": {"protocol_id": 0}, "minecraft:magma_block": {"protocol_id": 2}, "minecraft:netherrack": {"protocol_id": 1}}}}"#,
        );
        write(
            dir.path(),
            "reports/blocks.json",
            r#"{"minecraft:air": {"states": [{"id": 0, "default": true}]}, "minecraft:stone": {"states": [{"id": 1, "default": true}]},
                "minecraft:grass_block": {"states": [{"id": 8}, {"id": 9, "default": true}]}, "minecraft:dirt": {"states": [{"id": 10, "default": true}]},
                "minecraft:last": {"states": [{"id": 29999, "default": true}]}}"#,
        );
        let data = GameData::load(dir.path()).unwrap();
        assert!(data.has_reports);
        let blocks = data.tags.iter().find(|t| t.registry == "minecraft:block").unwrap();
        assert_eq!(blocks.tags, [("minecraft:infiniburn_overworld".to_string(), vec![1, 2])]);
        assert_eq!(data.blocks.grass_block, 9);
        assert_eq!(data.blocks.global_bits, 15);
    }

    #[test]
    fn refuses_another_version() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "version.json", r#"{"id": "1.21.4"}"#);
        assert!(GameData::load(dir.path()).is_err());
    }
}
