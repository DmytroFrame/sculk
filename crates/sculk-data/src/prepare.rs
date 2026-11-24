//! `sculk prepare`: takes the synchronized registries and their tags out of Mojang's 26.3 `server.jar`.
//!
//! The jar is only read as a zip; nothing in it runs.

use crate::{GAME_VERSION, STATIC_TAGGED, SYNCHRONIZED, registry_path};
use anyhow::{Context, bail};
use sha1::{Digest, Sha1};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

const MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
/// The bundler keeps the real server jar inside itself.
/// The folder inside `out` a run writes to before its files replace the old ones.
const STAGING: &str = ".staging";
const INNER_JAR: &str = "META-INF/versions/26.3/server-26.3.jar";

pub enum PrepareSource {
    /// Download `server.jar` from Mojang and check its SHA-1 against the version manifest.
    Download,
    /// A `server.jar` already on disk.
    Jar(PathBuf),
}

/// Writes `version.json` and `data/minecraft/{registry,tags/registry}/...` for every synchronized registry, and the
/// tags of the built-in registries in [`STATIC_TAGGED`], to `out`. `reports` is the output folder of Mojang's data
/// generator (`--reports`), whose `registries.json` and `blocks.json` give built-in IDs; without it built-in tags go
/// out empty.
pub fn prepare(source: PrepareSource, reports: Option<&Path>, out: &Path) -> anyhow::Result<()> {
    prepare_with(source, reports, None, out)
}

/// As [`prepare`]; with `java` (a Java 25 runtime) and no `reports`, Mojang's data generator is run first, in a
/// folder of its own that goes away afterwards, so the built-in IDs and tags are complete in one command.
pub fn prepare_with(
    source: PrepareSource,
    reports: Option<&Path>,
    java: Option<&Path>,
    out: &Path,
) -> anyhow::Result<()> {
    let jar = match source {
        PrepareSource::Jar(path) => std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        PrepareSource::Download => download_server_jar()?,
    };
    // The generator's folder lives until the reports are read.
    let generated = match (reports, java) {
        (None, Some(java)) => Some(generate_reports(java, &jar)?),
        _ => None,
    };
    let generated_reports = generated.as_ref().map(|d| d.path().join("out").join("reports"));
    let reports = reports.or(generated_reports.as_deref());
    let mut bundler = zip::ZipArchive::new(Cursor::new(jar)).context("server.jar is not a zip")?;
    let mut inner = Vec::new();
    bundler
        .by_name(INNER_JAR)
        .with_context(|| format!("{INNER_JAR} not in server.jar: is it the {GAME_VERSION} server?"))?
        .read_to_end(&mut inner)?;
    let mut server = zip::ZipArchive::new(Cursor::new(inner))?;

    let version = read_entry(&mut server, "version.json")?;
    let id = serde_json::from_slice::<serde_json::Value>(&version)?["id"].as_str().unwrap_or_default().to_owned();
    if id != GAME_VERSION {
        bail!("server.jar is Minecraft {id}; Sculk needs {GAME_VERSION}");
    }

    let mut prefixes: Vec<String> = SYNCHRONIZED
        .iter()
        .flat_map(|r| {
            let path = registry_path(r);
            [format!("data/minecraft/{path}/"), format!("data/minecraft/tags/{path}/")]
        })
        .collect();
    prefixes.extend(STATIC_TAGGED.iter().map(|r| format!("data/minecraft/tags/{}/", registry_path(r))));

    // Write into a fresh folder inside `out`, then swap its contents in, so a failed run leaves the old data
    // intact. `out` itself stays: in a container it is a volume's mount point, which can't be removed or renamed.
    let staging = out.join(STAGING);
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    std::fs::write(staging.join("version.json"), &version)?;
    let mut written = 0;
    for i in 0..server.len() {
        let mut file = server.by_index(i)?;
        let Some(name) = file.enclosed_name() else { continue };
        let name_str = name.to_string_lossy().replace('\\', "/");
        if file.is_dir() || !name_str.ends_with(".json") || !prefixes.iter().any(|p| name_str.starts_with(p.as_str())) {
            continue;
        }
        let path = staging.join(&name);
        std::fs::create_dir_all(path.parent().expect("file has a parent"))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        std::fs::write(path, bytes)?;
        written += 1;
    }
    if let Some(dir) = reports {
        // Either the generator's output folder or its `reports` folder.
        let dir = if dir.join("reports").is_dir() { dir.join("reports") } else { dir.to_owned() };
        std::fs::create_dir_all(staging.join("reports"))?;
        for name in ["registries.json", "blocks.json"] {
            std::fs::copy(dir.join(name), staging.join("reports").join(name))
                .with_context(|| format!("copying {}", dir.join(name).display()))?;
        }
    }
    for entry in std::fs::read_dir(out)? {
        let path = entry?.path();
        if path.file_name() == Some(std::ffi::OsStr::new(STAGING)) {
            continue;
        }
        if path.is_dir() { std::fs::remove_dir_all(&path)? } else { std::fs::remove_file(&path)? }
    }
    for entry in std::fs::read_dir(&staging)? {
        let entry = entry?;
        std::fs::rename(entry.path(), out.join(entry.file_name()))?;
    }
    std::fs::remove_dir(&staging)?;
    tracing::info!(files = written, out = %out.display(), "game data prepared");
    Ok(())
}

fn read_entry<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    zip.by_name(name).with_context(|| format!("{name} not in the server jar"))?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn get(url: &str) -> anyhow::Result<Vec<u8>> {
    let mut response = ureq::get(url).call().with_context(|| format!("GET {url}"))?;
    Ok(response.body_mut().with_config().limit(256 << 20).read_to_vec()?)
}

/// Runs `java -DbundlerMainClass=net.minecraft.data.Main -jar server.jar --reports` in a new temporary folder.
fn generate_reports(java: &Path, jar: &[u8]) -> anyhow::Result<tempfile::TempDir> {
    // The generator runs in its own folder: a relative path to Java must not change meaning there.
    let java = if java.components().count() > 1 { std::path::absolute(java)? } else { java.to_path_buf() };
    let java = java.as_path();
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("server.jar"), jar)?;
    tracing::info!(java = %java.display(), "running Mojang's data generator (--reports)");
    let status = std::process::Command::new(java)
        .current_dir(dir.path())
        .args(["-DbundlerMainClass=net.minecraft.data.Main", "-jar", "server.jar", "--reports", "--output", "out"])
        .stdout(std::process::Stdio::null())
        .status()
        .with_context(|| format!("starting {}", java.display()))?;
    if !status.success() || !dir.path().join("out/reports/blocks.json").exists() {
        bail!("the data generator failed ({status}); it needs Java 25");
    }
    Ok(dir)
}

fn download_server_jar() -> anyhow::Result<Vec<u8>> {
    let manifest: serde_json::Value = serde_json::from_slice(&get(MANIFEST)?)?;
    let url = manifest["versions"]
        .as_array()
        .and_then(|v| v.iter().find(|v| v["id"] == GAME_VERSION))
        .and_then(|v| v["url"].as_str())
        .with_context(|| format!("Minecraft {GAME_VERSION} is not in Mojang's version manifest"))?;
    let version: serde_json::Value = serde_json::from_slice(&get(url)?)?;
    let server = &version["downloads"]["server"];
    let (Some(url), Some(sha1)) = (server["url"].as_str(), server["sha1"].as_str()) else {
        bail!("the {GAME_VERSION} manifest has no server download");
    };
    tracing::info!(url, "downloading server.jar");
    let jar = get(url)?;
    let actual = hex::encode(Sha1::digest(&jar));
    if actual != sha1 {
        bail!("server.jar SHA-1 is {actual}, Mojang lists {sha1}");
    }
    Ok(jar)
}
