//! Managed tool defaults: refresh each untouched entry independently.
use crate::tools::{ToolBox, ToolDefinition};
use serde_json::{Value, json};
use std::{fs, path::Path};

pub fn definitions() -> Vec<ToolDefinition> {
    let mut defs = ToolBox::definitions();
    defs.push(crate::script::definition());
    defs.extend(crate::session_tools::definitions());
    defs.extend(crate::tree::definitions(None, true));
    defs.extend(
        crate::decisions::definitions(true)
            .into_iter()
            .filter(|d| d.name == "decisions"),
    );
    defs.extend(crate::tools::web::definitions());
    defs.push(crate::tools::generate::definition());
    crate::tool_profiles::add_hints(&mut defs);
    defs
}
pub fn defaults() -> Value {
    let mut out = serde_json::Map::new();
    for d in definitions() {
        let parameters = d.schema["properties"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, s)| {
                (
                    name.clone(),
                    json!({"description":s["description"].as_str().unwrap_or("")}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        out.insert(d.name.clone(), json!({"name":d.name,"description":d.description,"enabled":true,"parameters":parameters}));
    }
    Value::Object(out)
}
fn hash(value: &Value) -> String {
    crate::context::digest(&serde_json::to_vec(value).unwrap())
}
/// Explicit init creates defaults. Normal startup calls only when tools.json exists.
/// Returns whether tools.json changed. Unmanaged files and edited/deleted entries are kept.
pub fn install(config: &Path, data: &Path, force: bool) -> std::io::Result<bool> {
    let path = config.join("tools.json");
    let marker = data.join("tool-defaults.json");
    let exists = path.exists();
    let mut pins: serde_json::Map<String, Value> = fs::read(&marker)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if exists && pins.is_empty() && !force {
        return Ok(false);
    }
    let mut value = if exists && !force {
        serde_json::from_slice::<Value>(&fs::read(&path)?)?
    } else {
        json!({})
    };
    let entries = value
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other("tools.json must be an object"))?;
    let mut changed = !exists || force;
    for (name, default) in defaults().as_object().unwrap() {
        let current = entries.get(name);
        let managed = current
            .is_some_and(|v| pins.get(name).and_then(Value::as_str) == Some(hash(v).as_str()));
        if force || !exists || managed || (current.is_none() && !pins.contains_key(name)) {
            if current != Some(default) {
                entries.insert(name.clone(), default.clone());
                changed = true;
            }
            pins.insert(name.clone(), json!(hash(default)));
        }
    }
    if changed {
        fs::create_dir_all(config)?;
        atomic_json(&path, &value)?;
    }
    fs::create_dir_all(data)?;
    atomic_json(&marker, &Value::Object(pins))?;
    Ok(changed)
}
fn atomic_json(path: &Path, value: &Value) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_writes_current_defaults_and_preserves_unknown_entries() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let data = temp.path().join("data");
        assert!(install(&config, &data, false).unwrap());
        let path = config.join("tools.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(value.get("search").is_none());
        for name in crate::intel::NAMES {
            assert!(value.get(name).is_some());
        }
        value["search"] = json!({"enabled":true});
        value["unknown"] = json!({"enabled":true});
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let marker = data.join("tool-defaults.json");
        let mut pins: Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
        pins["search"] = json!(hash(&value["search"]));
        fs::write(&marker, serde_json::to_vec(&pins).unwrap()).unwrap();
        install(&config, &data, false).unwrap();
        let after: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(after["search"], value["search"]);
        assert_eq!(after["unknown"], value["unknown"]);
    }
}
