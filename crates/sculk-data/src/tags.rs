//! Tag files (`data/minecraft/tags/<registry>/...`) resolved to network IDs, as TagLoader does: `#tag` references
//! expand, `{"id": ..., "required": false}` entries that don't exist are skipped.

use crate::{Registry, RegistryTags, json_files};
use anyhow::{Context, bail};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// The raw entries of every tag file, by tag name.
type Raw = HashMap<String, Vec<(String, bool)>>;

/// Tags of `registry_name`. Without the registry's entries (`registry` is `None`: a built-in registry and no block
/// report) every tag is sent with no members, which keeps references to it valid.
pub(crate) fn load(dir: &Path, registry_name: &str, registry: Option<&Registry>) -> anyhow::Result<RegistryTags> {
    let mut raw = Raw::new();
    for name in json_files(dir)? {
        let path = dir.join(format!("{name}.json"));
        let json: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("parsing {}", path.display()))?;
        raw.insert(format!("minecraft:{name}"), entries(&json).with_context(|| path.display().to_string())?);
    }
    let mut resolved = BTreeMap::new();
    for name in raw.keys() {
        let mut ids = Vec::new();
        if let Some(registry) = registry {
            resolve(name, &raw, registry, &mut ids, &mut Vec::new())?;
        }
        resolved.insert(name.clone(), ids);
    }
    Ok(RegistryTags { registry: registry_name.to_owned(), tags: resolved.into_iter().collect() })
}

/// (entry, required) pairs of one tag file.
fn entries(json: &Value) -> anyhow::Result<Vec<(String, bool)>> {
    let Some(values) = json["values"].as_array() else { bail!("no \"values\" list") };
    values
        .iter()
        .map(|v| match v {
            Value::String(s) => Ok((s.clone(), true)),
            Value::Object(o) => Ok((
                o.get("id").and_then(Value::as_str).context("tag entry without \"id\"")?.to_owned(),
                o.get("required").and_then(Value::as_bool).unwrap_or(true),
            )),
            _ => bail!("tag entry is neither a string nor an object"),
        })
        .collect()
}

fn resolve(
    name: &str,
    raw: &Raw,
    registry: &Registry,
    out: &mut Vec<i32>,
    stack: &mut Vec<String>,
) -> anyhow::Result<()> {
    if stack.iter().any(|s| s == name) {
        bail!("tag {name} includes itself through {}", stack.join(" -> "));
    }
    stack.push(name.to_owned());
    for (entry, required) in &raw[name] {
        if let Some(tag) = entry.strip_prefix('#') {
            if raw.contains_key(tag) {
                resolve(tag, raw, registry, out, stack)?;
            } else if *required {
                bail!("tag {name} names missing tag #{tag}");
            }
        } else if let Some(id) = registry.id(entry) {
            if !out.contains(&id) {
                out.push(id);
            }
        } else if *required {
            bail!("tag {name} names {entry}, which {} lacks", registry.name);
        }
    }
    stack.pop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::new(
            "minecraft:worldgen/biome",
            vec!["minecraft:a".into(), "minecraft:b".into(), "minecraft:c".into()],
        )
    }

    fn raw(tags: &[(&str, &[(&str, bool)])]) -> Raw {
        tags.iter().map(|(n, e)| (n.to_string(), e.iter().map(|(s, r)| (s.to_string(), *r)).collect())).collect()
    }

    #[test]
    fn nested_tags_expand_without_duplicates() {
        let raw = raw(&[
            ("minecraft:inner", &[("minecraft:b", true), ("minecraft:c", true)]),
            ("minecraft:outer", &[("minecraft:c", true), ("#minecraft:inner", true), ("minecraft:a", true)]),
        ]);
        let mut out = Vec::new();
        resolve("minecraft:outer", &raw, &registry(), &mut out, &mut Vec::new()).unwrap();
        assert_eq!(out, [2, 1, 0]);
    }

    #[test]
    fn optional_missing_entries_are_skipped() {
        let raw =
            raw(&[("minecraft:t", &[("minecraft:nope", false), ("#minecraft:gone", false), ("minecraft:a", true)])]);
        let mut out = Vec::new();
        resolve("minecraft:t", &raw, &registry(), &mut out, &mut Vec::new()).unwrap();
        assert_eq!(out, [0]);
    }

    #[test]
    fn required_missing_entry_and_cycles_fail() {
        let missing = raw(&[("minecraft:t", &[("minecraft:nope", true)])]);
        assert!(resolve("minecraft:t", &missing, &registry(), &mut Vec::new(), &mut Vec::new()).is_err());
        let cycle = raw(&[("minecraft:x", &[("#minecraft:y", true)]), ("minecraft:y", &[("#minecraft:x", true)])]);
        assert!(resolve("minecraft:x", &cycle, &registry(), &mut Vec::new(), &mut Vec::new()).is_err());
    }

    #[test]
    fn reads_both_entry_forms() {
        let json: Value =
            serde_json::from_str(r#"{"values": ["minecraft:a", {"id": "minecraft:b", "required": false}]}"#).unwrap();
        assert_eq!(entries(&json).unwrap(), [("minecraft:a".into(), true), ("minecraft:b".into(), false)]);
    }
}
