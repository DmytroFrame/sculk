//! A world's files besides its chunks (docs/docs/09-hlobalnyi-stan.md): level.dat, players' data, stats and
//! advancements, and the saved data under `data/` (weather, game rules, world clocks, scoreboard...). They are kept
//! in vanilla's layout beside the region files, so `<data>/<world>` is a vanilla world folder. The server's lists
//! (operators, whitelist, bans) live under `server/` there, shared by every worker's engine.

use anyhow::bail;
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

/// The server's lists, as vanilla keeps them beside server.properties: kept under `server/`.
pub const SERVER_LISTS: [&str; 4] = ["ops.json", "whitelist.json", "banned-players.json", "banned-ips.json"];

/// The largest file taken (vanilla's map and scoreboard files stay far below it).
pub const MAX_FILE: usize = 32 << 20;

/// Whether a path inside a world folder is one of the world files AnvilDB keeps: a relative `/` path of plain names
/// ending in `.dat` or `.json`, outside the chunk folders. Backups (`_old`), temporary files and the session lock
/// are the game's own business.
pub fn is_world_file(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let plain = |p: &&str| {
        !p.is_empty() && *p != "." && *p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
    };
    if !parts.iter().all(plain) {
        return false;
    }
    let name = parts[parts.len() - 1];
    if !(name.ends_with(".dat") || name.ends_with(".json")) {
        return false;
    }
    if parts.iter().any(|p| matches!(*p, "region" | "entities" | "poi")) {
        return false;
    }
    match parts[0] {
        "level.dat" => parts.len() == 1,
        "server" => parts.len() == 2 && SERVER_LISTS.contains(&parts[1]),
        "players" => parts.len() == 3 && matches!(parts[1], "data" | "stats" | "advancements"),
        "data" => parts.len() >= 2,
        // A dimension's own saved data: dimensions/<namespace>/<path>/data/...
        "dimensions" => parts.len() >= 5 && parts[..parts.len() - 1].contains(&"data"),
        _ => false,
    }
}

/// The first 8 bytes of the SHA-1 of `data`: what tells two copies of a file apart.
pub fn hash(data: &[u8]) -> u64 {
    let digest = Sha1::digest(data);
    u64::from_be_bytes(digest[..8].try_into().expect("SHA-1 has 20 bytes"))
}

pub struct FileStore {
    root: PathBuf,
}

fn valid_world(world: &str) -> bool {
    !world.is_empty()
        && world != "."
        && world != ".."
        && world.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-.".contains(c))
}

impl FileStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, world: &str, path: &str) -> anyhow::Result<PathBuf> {
        if !valid_world(world) {
            bail!("bad world name {world:?}");
        }
        if !is_world_file(path) {
            bail!("not a world file: {path:?}");
        }
        Ok(path.split('/').fold(self.root.join(world), |p, part| p.join(part)))
    }

    pub fn get(&self, world: &str, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(world, path)?) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Replaces each file whole: written beside it, then renamed over it, so a reader never sees half a file.
    pub fn put(&self, world: &str, files: &[(String, Vec<u8>)]) -> anyhow::Result<()> {
        for (path, data) in files {
            if data.len() > MAX_FILE {
                bail!("{path} is {} bytes, more than {MAX_FILE}", data.len());
            }
            let target = self.path(world, path)?;
            let dir = target.parent().expect("a world file is inside the world folder");
            std::fs::create_dir_all(dir)?;
            let temp = dir.join(format!(".{}.sculk-tmp", target.file_name().unwrap_or_default().to_string_lossy()));
            std::fs::write(&temp, data)?;
            std::fs::rename(&temp, &target)?;
        }
        Ok(())
    }

    /// Every world file whose path starts with `prefix`, with its hash.
    pub fn list(&self, world: &str, prefix: &str) -> anyhow::Result<Vec<(String, u64)>> {
        if !valid_world(world) {
            bail!("bad world name {world:?}");
        }
        let root = self.root.join(world);
        let mut out = Vec::new();
        walk(&root, &root, &mut |rel, path| {
            if rel.starts_with(prefix) && is_world_file(rel) {
                out.push((rel.to_owned(), hash(&std::fs::read(path)?)));
            }
            Ok(())
        })?;
        out.sort();
        Ok(out)
    }
}

/// Calls `f` with every file under `dir` (its `/` path relative to `root`), skipping the chunk folders.
pub fn walk(root: &Path, dir: &Path, f: &mut dyn FnMut(&str, &Path) -> std::io::Result<()>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            let name = entry.file_name();
            if !matches!(name.to_str(), Some("region" | "entities" | "poi")) {
                walk(root, &path, f)?;
            }
        } else if kind.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            let rel: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            f(&rel.join("/"), &path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_world_files_are_taken() {
        for good in [
            "level.dat",
            "players/data/069a79f4-44e9-4726-a5be-fca90e38aaf5.dat",
            "players/stats/069a79f4-44e9-4726-a5be-fca90e38aaf5.json",
            "players/advancements/069a79f4-44e9-4726-a5be-fca90e38aaf5.json",
            "data/minecraft/weather.dat",
            "data/scoreboard.dat",
            "dimensions/minecraft/the_end/data/minecraft/ender_dragon_fight.dat",
            "server/ops.json",
        ] {
            assert!(is_world_file(good), "{good}");
        }
        for bad in [
            "level.dat_old",
            "session.lock",
            "../level.dat",
            "/level.dat",
            "players/data/../../x.dat",
            "players/data/x.dat_old",
            "players/x.dat",
            "dimensions/minecraft/overworld/region/r.0.0.mca",
            "dimensions/minecraft/overworld/region/data/x.dat",
            "data/minecraft/.weather.dat.sculk-tmp",
            "server.properties",
            "server/server.properties",
            "server/usercache.json",
            "",
        ] {
            assert!(!is_world_file(bad), "{bad}");
        }
    }

    #[test]
    fn files_round_trip_in_vanilla_layout() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        store
            .put(
                "world",
                &[
                    ("level.dat".into(), b"level".to_vec()),
                    ("players/data/a.dat".into(), b"player".to_vec()),
                    ("data/minecraft/weather.dat".into(), b"rain".to_vec()),
                ],
            )
            .unwrap();
        assert!(dir.path().join("world/players/data/a.dat").exists());
        assert_eq!(store.get("world", "level.dat").unwrap().unwrap(), b"level");
        assert_eq!(store.get("world", "players/data/b.dat").unwrap(), None);
        // A chunk folder beside them is not listed.
        std::fs::create_dir_all(dir.path().join("world/dimensions/minecraft/overworld/region")).unwrap();
        std::fs::write(dir.path().join("world/dimensions/minecraft/overworld/region/r.0.0.mca"), b"x").unwrap();
        let all = store.list("world", "").unwrap();
        assert_eq!(
            all,
            vec![
                ("data/minecraft/weather.dat".into(), hash(b"rain")),
                ("level.dat".into(), hash(b"level")),
                ("players/data/a.dat".into(), hash(b"player")),
            ]
        );
        assert_eq!(store.list("world", "players/").unwrap().len(), 1);
        assert!(store.put("../x", &[("level.dat".into(), vec![])]).is_err());
        assert!(store.get("world", "../../etc/passwd").is_err());
    }
}
