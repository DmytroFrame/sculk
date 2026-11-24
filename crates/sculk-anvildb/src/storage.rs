//! Chunk records in vanilla's layout and region files (`horizon_world::region`), so a world folder copies in and
//! out of AnvilDB as it is (docs/docs/07-anvildb.md, "Класичний" format):
//!
//! ```text
//! <data>/<world>/dimensions/<namespace>/<path>/region/r.X.Z.mca
//! ```
//!
//! as 26.3 saves every dimension, the overworld too (`minecraft/overworld`).
//!
//! with `entities/` and `poi/` beside `region/`.

use anyhow::{Context, bail};
use horizon_core::position::ChunkPos;
use horizon_world::compression::Compression;
use horizon_world::region::{ChunkRecord, RegionFile};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const FOLDERS: [&str; 3] = ["region", "entities", "poi"];
/// Records larger than this are refused (vanilla's region files take up to 64 MiB compressed).
const MAX_RECORD: usize = 64 << 20;

/// Where one chunk's record lives.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordKey {
    pub world: String,
    pub dimension: String,
    pub folder: String,
    pub x: i32,
    pub z: i32,
}

pub struct RegionStore {
    root: PathBuf,
    /// How new records are compressed. Each record says its own, so a region may mix them, as vanilla's may.
    compression: Compression,
    open: Mutex<HashMap<PathBuf, Arc<Mutex<RegionFile>>>>,
    /// Where archived regions go when not in `<data>/archive/`: an S3 bucket, with the keys it holds (kept in
    /// `archive/s3-index.txt`, so a region that was never archived costs no request).
    s3: Option<(crate::s3::S3, Mutex<std::collections::BTreeSet<String>>)>,
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-.".contains(c))
        && s != ".."
        && s != "."
}

impl RegionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_compression(root, Compression::Zlib)
    }

    /// A store writing records with `compression`: vanilla's `region-file-compression` (`deflate` is zlib, `lz4`,
    /// `none`), all of which vanilla 26.3 reads.
    pub fn with_compression(root: impl Into<PathBuf>, compression: Compression) -> Self {
        Self { root: root.into(), compression, open: Mutex::new(HashMap::new()), s3: None }
    }

    /// The folder holding `folder`'s region files for a dimension. Names come from the network, so anything that
    /// could leave the data folder is refused.
    pub fn folder(&self, world: &str, dimension: &str, folder: &str) -> anyhow::Result<PathBuf> {
        if !valid_name(world) {
            bail!("bad world name {world:?}");
        }
        if !FOLDERS.contains(&folder) {
            bail!("bad folder {folder:?}");
        }
        let (namespace, path) = dimension.split_once(':').with_context(|| format!("bad dimension {dimension:?}"))?;
        if !valid_name(namespace) || path.is_empty() || !path.split('/').all(valid_name) {
            bail!("bad dimension {dimension:?}");
        }
        let mut dir = self.root.join(world).join("dimensions").join(namespace);
        for part in path.split('/') {
            dir = dir.join(part);
        }
        Ok(dir.join(folder))
    }

    fn region(&self, dir: &Path, rx: i32, rz: i32, create: bool) -> anyhow::Result<Option<Arc<Mutex<RegionFile>>>> {
        let path = dir.join(format!("r.{rx}.{rz}.mca"));
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(region) = open.get(&path) {
            return Ok(Some(region.clone()));
        }
        // A region archived as cold comes back as it was, the first time it is wanted again.
        self.restore(&path)?;
        if !create && !path.exists() {
            return Ok(None);
        }
        std::fs::create_dir_all(dir)?;
        let region = Arc::new(Mutex::new(RegionFile::open_writable(&path, create)?));
        open.insert(path, region.clone());
        Ok(Some(region))
    }

    /// Archives to an S3 bucket instead of `archive/`.
    pub fn with_s3(mut self, s3: crate::s3::S3) -> Self {
        let index = std::fs::read_to_string(self.s3_index()).unwrap_or_default();
        let keys = index.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
        self.s3 = Some((s3, Mutex::new(keys)));
        self
    }

    fn s3_index(&self) -> PathBuf {
        self.root.join("archive").join("s3-index.txt")
    }

    fn save_s3_index(&self, keys: &std::collections::BTreeSet<String>) -> anyhow::Result<()> {
        let path = self.s3_index();
        std::fs::create_dir_all(path.parent().expect("in archive/"))?;
        let temp = path.with_extension("txt.writing");
        std::fs::write(&temp, keys.iter().map(|k| format!("{k}\n")).collect::<String>())?;
        std::fs::rename(temp, path)?;
        Ok(())
    }

    /// A region's key in the bucket: its path in the data folder, gzipped.
    fn s3_key(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        Some(format!("{}.gz", parts.join("/")))
    }

    /// Where an archived region file is kept: the same path under `archive/`, gzipped.
    fn archived(&self, path: &Path) -> Option<PathBuf> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let mut archived = self.root.join("archive").join(rel).into_os_string();
        archived.push(".gz");
        Some(archived.into())
    }

    /// Puts an archived region file back in its place.
    fn restore(&self, path: &Path) -> anyhow::Result<()> {
        let Some(archived) = self.archived(path) else { return Ok(()) };
        if path.exists() {
            return Ok(());
        }
        if !archived.exists() {
            return self.restore_from_s3(path);
        }
        let bytes = Compression::Gzip.decode(&std::fs::read(&archived)?, 1 << 30)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension("mca.restoring");
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, path)?;
        std::fs::remove_file(&archived)?;
        tracing::info!(region = %path.display(), "cold region restored from the archive");
        Ok(())
    }

    /// Puts a region archived in the bucket back in its place.
    fn restore_from_s3(&self, path: &Path) -> anyhow::Result<()> {
        let (Some((s3, keys)), Some(key)) = (&self.s3, self.s3_key(path)) else { return Ok(()) };
        if !keys.lock().unwrap_or_else(|e| e.into_inner()).contains(&key) {
            return Ok(());
        }
        let gz = s3.get(&key)?.with_context(|| format!("the bucket lost {key}"))?;
        let bytes = Compression::Gzip.decode(&gz, 1 << 30)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension("mca.restoring");
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, path)?;
        let mut keys = keys.lock().unwrap_or_else(|e| e.into_inner());
        keys.remove(&key);
        self.save_s3_index(&keys)?;
        drop(keys);
        s3.delete(&key)?;
        tracing::info!(region = %path.display(), "cold region restored from S3");
        Ok(())
    }

    /// Archives the region files no write touched for `cold` (docs/docs/07-anvildb.md, "архів холодних регіонів"):
    /// each is gzipped under `archive/` and its open handle dropped. Returns how many were archived.
    pub fn archive_cold(&self, cold: std::time::Duration) -> anyhow::Result<usize> {
        let mut regions = Vec::new();
        collect_regions(&self.root, &self.root.join("archive"), &mut regions)?;
        let mut archived = 0;
        for path in regions {
            if !std::fs::metadata(&path)?.modified()?.elapsed().is_ok_and(|age| age >= cold) {
                continue;
            }
            // No reader opens it while it moves.
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            open.remove(&path);
            let gz = Compression::Gzip.encode(&std::fs::read(&path)?)?;
            if let (Some((s3, keys)), Some(key)) = (&self.s3, self.s3_key(&path)) {
                s3.put(&key, &gz)?;
                let mut keys = keys.lock().unwrap_or_else(|e| e.into_inner());
                keys.insert(key);
                self.save_s3_index(&keys)?;
                drop(keys);
                std::fs::remove_file(&path)?;
                drop(open);
                archived += 1;
                continue;
            }
            let Some(target) = self.archived(&path) else { continue };
            if let Some(dir) = target.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let temp = target.with_extension("gz.writing");
            std::fs::write(&temp, gz)?;
            std::fs::rename(&temp, &target)?;
            std::fs::remove_file(&path)?;
            drop(open);
            archived += 1;
        }
        Ok(archived)
    }

    /// Puts every archived region back: before copying the world folder out as a vanilla world.
    pub fn restore_all(&self) -> anyhow::Result<usize> {
        let mut archived = Vec::new();
        collect_files(&self.root.join("archive"), "gz", &mut archived)?;
        let mut restored = 0;
        for gz in archived {
            let Ok(rel) = gz.strip_prefix(self.root.join("archive")) else { continue };
            let path = self.root.join(rel).with_extension("");
            self.restore(&path)?;
            restored += 1;
        }
        let in_s3: Vec<String> = match &self.s3 {
            Some((_, keys)) => keys.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect(),
            None => Vec::new(),
        };
        for key in in_s3 {
            let path = self.root.join(key.trim_end_matches(".gz"));
            self.restore_from_s3(&path)?;
            restored += 1;
        }
        Ok(restored)
    }

    /// The record as uncompressed named NBT, or `None` when the chunk has none.
    pub fn read(&self, key: &RecordKey) -> anyhow::Result<Option<Vec<u8>>> {
        let dir = self.folder(&key.world, &key.dimension, &key.folder)?;
        let Some(region) = self.region(&dir, key.x >> 5, key.z >> 5, false)? else { return Ok(None) };
        let record = region.lock().unwrap_or_else(|e| e.into_inner()).read(ChunkPos::new(key.x, key.z))?;
        Ok(match record {
            Some(r) => Some(r.compression.decode(&r.compressed, MAX_RECORD)?),
            None => None,
        })
    }

    /// Writes (or with `None` deletes) records, then flushes each region touched once.
    pub fn write(&self, records: &[(RecordKey, Option<Vec<u8>>)]) -> anyhow::Result<()> {
        let timestamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32);
        let mut touched: HashMap<PathBuf, Arc<Mutex<RegionFile>>> = HashMap::new();
        for (key, nbt) in records {
            let dir = self.folder(&key.world, &key.dimension, &key.folder)?;
            let Some(region) = self.region(&dir, key.x >> 5, key.z >> 5, nbt.is_some())? else { continue };
            let pos = ChunkPos::new(key.x, key.z);
            {
                let mut file = region.lock().unwrap_or_else(|e| e.into_inner());
                match nbt {
                    Some(bytes) => {
                        let compressed = self.compression.encode(bytes)?;
                        file.write(pos, &ChunkRecord { compression: self.compression, compressed, timestamp })?;
                    }
                    None => {
                        file.clear(pos, timestamp)?;
                    }
                }
            }
            touched.insert(dir.join(format!("r.{}.{}", key.x >> 5, key.z >> 5)), region);
        }
        for region in touched.values() {
            region.lock().unwrap_or_else(|e| e.into_inner()).sync()?;
        }
        Ok(())
    }

    /// Bits `x + z * 32` of the chunks of region `rx, rz` that have a record.
    pub fn saved_in_region(
        &self,
        world: &str,
        dimension: &str,
        folder: &str,
        rx: i32,
        rz: i32,
    ) -> anyhow::Result<[u64; 16]> {
        let dir = self.folder(world, dimension, folder)?;
        let mut bits = [0u64; 16];
        if let Some(region) = self.region(&dir, rx, rz, false)? {
            for (pos, _) in region.lock().unwrap_or_else(|e| e.into_inner()).chunks() {
                let i = ((pos.x & 31) + (pos.z & 31) * 32) as usize;
                bits[i / 64] |= 1 << (i % 64);
            }
        }
        Ok(bits)
    }
}

/// Every `.mca` file under `dir`, the `skip` folder aside.
fn collect_regions(dir: &Path, skip: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if dir == skip {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            collect_regions(&path, skip, out)?;
        } else if path.extension().is_some_and(|e| e == "mca") {
            out.push(path);
        }
    }
    Ok(())
}

/// Every file with extension `ext` under `dir`.
fn collect_files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, ext, out)?;
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(dimension: &str, folder: &str, x: i32, z: i32) -> RecordKey {
        RecordKey { world: "world".into(), dimension: dimension.into(), folder: folder.into(), x, z }
    }

    #[test]
    fn records_survive_a_restart_in_vanilla_layout() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegionStore::new(dir.path());
        let nether = key("minecraft:the_nether", "region", -33, 5);
        store
            .write(&[
                (key("minecraft:overworld", "region", 1, 2), Some(b"overworld".to_vec())),
                (nether.clone(), Some(vec![7; 10_000])),
            ])
            .unwrap();
        assert!(dir.path().join("world/dimensions/minecraft/overworld/region/r.0.0.mca").exists());
        assert!(dir.path().join("world/dimensions/minecraft/the_nether/region/r.-2.0.mca").exists());

        let reopened = RegionStore::new(dir.path());
        assert_eq!(reopened.read(&key("minecraft:overworld", "region", 1, 2)).unwrap().unwrap(), b"overworld");
        assert_eq!(reopened.read(&nether).unwrap().unwrap(), vec![7; 10_000]);
        assert_eq!(reopened.read(&key("minecraft:overworld", "region", 1, 3)).unwrap(), None);
        assert_eq!(reopened.read(&key("minecraft:overworld", "entities", 1, 2)).unwrap(), None);
    }

    #[test]
    fn deleting_clears_the_record_and_its_bit() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegionStore::new(dir.path());
        let a = key("minecraft:overworld", "region", 3, 1);
        store
            .write(&[(a.clone(), Some(vec![1])), (key("minecraft:overworld", "region", 0, 0), Some(vec![2]))])
            .unwrap();
        let bits = store.saved_in_region("world", "minecraft:overworld", "region", 0, 0).unwrap();
        assert_eq!(bits[0], (1 << 0) | (1 << (3 + 32)));
        store.write(&[(a.clone(), None)]).unwrap();
        assert_eq!(store.read(&a).unwrap(), None);
        assert_eq!(store.saved_in_region("world", "minecraft:overworld", "region", 0, 0).unwrap()[0], 1);
    }

    #[test]
    fn cold_regions_are_archived_and_come_back_when_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegionStore::new(dir.path());
        let a = key("minecraft:overworld", "region", 1, 2);
        store.write(&[(a.clone(), Some(b"old terrain".to_vec()))]).unwrap();
        let region = dir.path().join("world/dimensions/minecraft/overworld/region/r.0.0.mca");
        assert_eq!(store.archive_cold(std::time::Duration::from_secs(3600)).unwrap(), 0, "a fresh region stays");
        assert_eq!(store.archive_cold(std::time::Duration::ZERO).unwrap(), 1);
        assert!(!region.exists());
        assert!(dir.path().join("archive/world/dimensions/minecraft/overworld/region/r.0.0.mca.gz").exists());
        // Read again: back in place, whole.
        assert_eq!(store.read(&a).unwrap().unwrap(), b"old terrain");
        assert!(region.exists());
        assert_eq!(store.archive_cold(std::time::Duration::ZERO).unwrap(), 1);
        assert_eq!(store.restore_all().unwrap(), 1);
        assert_eq!(RegionStore::new(dir.path()).read(&a).unwrap().unwrap(), b"old terrain");
    }

    #[test]
    fn cold_regions_go_to_s3_and_come_back_when_read() {
        let dir = tempfile::tempdir().unwrap();
        let s3 = crate::s3::S3 {
            endpoint: crate::s3::tests::fake_bucket(),
            bucket: "worlds".into(),
            region: "us-east-1".into(),
            access_key: "key".into(),
            secret_key: "secret".into(),
        };
        let store = RegionStore::new(dir.path()).with_s3(s3.clone());
        let a = key("minecraft:overworld", "region", 40, 2);
        store.write(&[(a.clone(), Some(b"old terrain".to_vec()))]).unwrap();
        assert_eq!(store.archive_cold(std::time::Duration::ZERO).unwrap(), 1);
        let region = dir.path().join("world/dimensions/minecraft/overworld/region/r.1.0.mca");
        assert!(!region.exists());
        assert!(s3.get("world/dimensions/minecraft/overworld/region/r.1.0.mca.gz").unwrap().is_some());
        // A store started again knows from its index what the bucket holds.
        let again = RegionStore::new(dir.path()).with_s3(s3.clone());
        assert_eq!(again.read(&key("minecraft:overworld", "region", 0, 0)).unwrap(), None, "never archived");
        assert_eq!(again.read(&a).unwrap().unwrap(), b"old terrain");
        assert!(region.exists());
        assert_eq!(s3.get("world/dimensions/minecraft/overworld/region/r.1.0.mca.gz").unwrap(), None);
    }

    #[test]
    fn records_of_every_compression_read_back_in_one_region() {
        let dir = tempfile::tempdir().unwrap();
        let chunk = vec![3u8; 40_000];
        for (i, compression) in [Compression::Zlib, Compression::Lz4, Compression::None].into_iter().enumerate() {
            let store = RegionStore::with_compression(dir.path(), compression);
            store.write(&[(key("minecraft:overworld", "region", i as i32, 0), Some(chunk.clone()))]).unwrap();
        }
        let store = RegionStore::new(dir.path());
        for i in 0..3 {
            assert_eq!(store.read(&key("minecraft:overworld", "region", i, 0)).unwrap().unwrap(), chunk);
        }
    }

    #[test]
    fn names_cannot_leave_the_data_folder() {
        let store = RegionStore::new("/data");
        assert!(store.folder("../etc", "minecraft:overworld", "region").is_err());
        assert!(store.folder("world", "minecraft:../../x", "region").is_err());
        assert!(store.folder("world", "minecraft:overworld", "../region").is_err());
        assert!(store.folder("world", "overworld", "region").is_err());
        assert_eq!(
            store.folder("world", "mymod:sky/high", "poi").unwrap(),
            Path::new("/data/world/dimensions/mymod/sky/high/poi")
        );
    }
}
