//! Skills shipped inside the binary (§3 config skill, D12), installed into
//! `~/.agents/skills/<name>/` on first run and refreshed on upgrade while the
//! installed copy is still exactly what we wrote. A user-owned skill of the same
//! name (by directory or frontmatter `name`), a user-edited copy and a deliberate
//! deletion are all left alone; a marker in DATA records what we installed.
use std::{
    fs,
    path::{Path, PathBuf},
};

struct Bundled {
    name: &'static str,
    files: &'static [(&'static str, &'static str)],
}

const SKILLS: &[Bundled] = &[Bundled {
    name: "cockpit-code-config",
    files: &[
        (
            "references/models-dev.md",
            include_str!("../skills/cockpit-code-config/references/models-dev.md"),
        ),
        (
            "SKILL.md",
            include_str!("../skills/cockpit-code-config/SKILL.md"),
        ),
        (
            "references/providers.md",
            include_str!("../skills/cockpit-code-config/references/providers.md"),
        ),
        (
            "references/command-providers.md",
            include_str!("../skills/cockpit-code-config/references/command-providers.md"),
        ),
        (
            "references/config-json.md",
            include_str!("../skills/cockpit-code-config/references/config-json.md"),
        ),
        (
            "references/mcps.md",
            include_str!("../skills/cockpit-code-config/references/mcps.md"),
        ),
    ],
}];

/// Installs each bundled skill once, and refreshes a copy we installed when it
/// is unmodified and the binary ships different content. Returns the
/// directories written. `home` is the user's home; `data` is the resolved DATA
/// directory, which holds `bundled-skills/<name>` markers
/// (`{"version", "hash"}`; `hash` covers every file of the installed copy).
pub fn install(home: &Path, data: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut written = install_in(&home.join(".agents/skills"), &data.join("bundled-skills"))?;
    let targets: Vec<PathBuf> = fs::read(data.join("bundled-skill-targets.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    for target in targets {
        written.extend(install_at(&target, data)?);
    }
    Ok(written)
}
/// Additional agent skill directory, with independent D78 ownership markers.
pub fn install_at(skills_dir: &Path, data: &Path) -> std::io::Result<Vec<PathBuf>> {
    let skills_dir = if skills_dir.is_absolute() {
        skills_dir.to_owned()
    } else {
        std::env::current_dir()?.join(skills_dir)
    };
    let registry = data.join("bundled-skill-targets.json");
    let mut targets: Vec<PathBuf> = fs::read(&registry)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if !targets.contains(&skills_dir) {
        targets.push(skills_dir.clone());
        fs::create_dir_all(data)?;
        fs::write(registry, serde_json::to_vec(&targets)?)?;
    }
    let key = crate::context::digest(skills_dir.to_string_lossy().as_bytes());
    install_in(&skills_dir, &data.join("bundled-skills-targets").join(key))
}
pub fn config_skill() -> &'static str {
    include_str!("../skills/cockpit-code-config/SKILL.md")
}
fn install_in(skills_dir: &Path, markers: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for skill in SKILLS {
        let marker = markers.join(skill.name);
        let target = skills_dir.join(skill.name);
        let bundled = bundled_hash(skill);
        if marker.exists() {
            // Refresh only our own unmodified copy; a deleted, edited or
            // unreadable copy (or an old-format marker) is left alone.
            let recorded = fs::read_to_string(&marker)
                .ok()
                .and_then(|m| serde_json::from_str::<serde_json::Value>(&m).ok())
                .and_then(|m| m["hash"].as_str().map(str::to_owned));
            let Some(recorded) = recorded else { continue };
            if recorded == bundled || !target.is_dir() || dir_hash(&target)? != recorded {
                continue;
            }
            replace(skill, skills_dir, &target)?;
            written.push(target);
        } else if !exists(skills_dir, skill.name) {
            replace(skill, skills_dir, &target)?;
            written.push(target);
        } else {
            // A user-owned skill of that name: never ours, never touched.
            write_marker(&marker, "user-owned")?;
            continue;
        }
        write_marker(&marker, &bundled)?;
    }
    Ok(written)
}

fn write_marker(marker: &Path, hash: &str) -> std::io::Result<()> {
    fs::create_dir_all(marker.parent().unwrap())?;
    fs::write(
        marker,
        serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "hash": hash}).to_string(),
    )
}

/// Stage the bundled files beside the target, then swap them in, so a crash
/// never leaves half a skill.
fn replace(skill: &Bundled, skills_dir: &Path, target: &Path) -> std::io::Result<()> {
    fs::create_dir_all(skills_dir)?;
    let staging = tempfile::Builder::new()
        .prefix(".cockpit-code-skill-")
        .tempdir_in(skills_dir)?;
    for (path, body) in skill.files {
        let file = staging.path().join(path);
        fs::create_dir_all(file.parent().unwrap())?;
        fs::write(file, body)?;
    }
    let staged = staging.keep();
    let old = target
        .exists()
        .then(|| skills_dir.join(format!(".cockpit-code-skill-old-{}", uuid::Uuid::new_v4())));
    if let Some(old) = &old
        && let Err(e) = fs::rename(target, old)
    {
        let _ = fs::remove_dir_all(&staged);
        return Err(e);
    }
    if let Err(e) = fs::rename(&staged, target) {
        let _ = fs::remove_dir_all(&staged);
        if let Some(old) = &old {
            let _ = fs::rename(old, target);
        }
        // Lost a race with another first run: the skill now exists.
        if !target.exists() {
            return Err(e);
        }
    }
    if let Some(old) = old {
        let _ = fs::remove_dir_all(old);
    }
    Ok(())
}

fn digest(mut files: Vec<(String, Vec<u8>)>) -> String {
    use sha2::{Digest, Sha256};
    files.sort();
    let mut hasher = Sha256::new();
    for (path, body) in files {
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update((body.len() as u64).to_le_bytes());
        hasher.update(&body);
    }
    format!("{:x}", hasher.finalize())
}

fn bundled_hash(skill: &Bundled) -> String {
    digest(
        skill
            .files
            .iter()
            .map(|(p, b)| ((*p).to_owned(), b.as_bytes().to_vec()))
            .collect(),
    )
}

/// Hash of every file under `dir` (relative `/` paths), so added, removed or
/// edited files all count as a modification.
fn dir_hash(dir: &Path) -> std::io::Result<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, fs::read(&path)?));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files)?;
    Ok(digest(files))
}

fn exists(skills_dir: &Path, name: &str) -> bool {
    if skills_dir.join(name).exists() {
        return true;
    }
    let Ok(entries) = fs::read_dir(skills_dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        fs::read_to_string(entry.path().join("SKILL.md")).is_ok_and(|body| {
            body.strip_prefix("---")
                .and_then(|rest| rest.split("\n---").next())
                .is_some_and(|front| {
                    front.lines().any(|line| {
                        line.strip_prefix("name:")
                            .is_some_and(|v| v.trim().trim_matches(['"', '\'']) == name)
                    })
                })
        })
    })
}
