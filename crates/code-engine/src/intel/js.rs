//! Indexed tsconfig/jsconfig paths plus conservative common source aliases.
use super::resolve::{join, parent_dir, resolve_js};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Default)]
struct Options {
    base: Option<String>,
    paths: Option<(String, BTreeMap<String, Vec<String>>)>,
}
#[derive(Default)]
pub(super) struct Aliases {
    configs: Vec<(String, Options)>,
    packages: Vec<String>,
}
impl Aliases {
    pub fn new<'a>(sources: impl Iterator<Item = (&'a str, &'a str)>) -> Self {
        let sources = sources.collect::<BTreeMap<_, _>>();
        let packages = sources
            .iter()
            .filter(|(p, _)| p.rsplit('/').next() == Some("package.json"))
            .filter_map(|(p, text)| {
                let value = jsonc(text)?;
                Some((value["name"].as_str()?.to_owned(), parent_dir(p).to_owned()))
            })
            .collect::<BTreeMap<_, _>>();
        let mut config_paths = sources
            .keys()
            .filter(|p| {
                matches!(
                    p.rsplit('/').next(),
                    Some("tsconfig.json" | "jsconfig.json")
                )
            })
            .copied()
            .collect::<Vec<_>>();
        config_paths.sort_by_key(|path| {
            (
                std::cmp::Reverse(parent_dir(path).len()),
                path.ends_with("jsconfig.json"),
            )
        });
        let configs = config_paths
            .into_iter()
            .map(|path| {
                (
                    parent_dir(path).to_owned(),
                    load(path, &sources, &packages, &mut HashSet::new()),
                )
            })
            .collect();
        let mut dirs = sources
            .keys()
            .filter(|p| p.ends_with("/package.json") || **p == "package.json")
            .map(|p| parent_dir(p).to_owned())
            .collect::<Vec<_>>();
        dirs.sort_by_key(|dir| std::cmp::Reverse(dir.len()));
        Self {
            configs,
            packages: dirs,
        }
    }
    pub fn resolve(&self, importer: &str, raw: &str, existing: &HashSet<String>) -> Option<String> {
        if raw.starts_with(['.', '/']) {
            return resolve_js(importer, raw, existing);
        }
        let contains = |dir: &str| {
            dir.is_empty()
                || importer
                    .strip_prefix(dir)
                    .is_some_and(|p| p.starts_with('/'))
        };
        let config = self.configs.iter().find(|(dir, _)| contains(dir));
        let candidate = |base: &str| resolve_js("", &format!("/{base}"), existing);
        if let Some((_, options)) = config {
            if let Some((origin, paths)) = &options.paths {
                let mut matches = paths
                    .iter()
                    .filter_map(|(pattern, targets)| {
                        if let Some((prefix, suffix)) = pattern.split_once('*') {
                            let value = raw.strip_prefix(prefix)?.strip_suffix(suffix)?;
                            Some((false, prefix.len(), suffix.len(), value, targets))
                        } else {
                            (pattern == raw).then_some((true, pattern.len(), 0, "", targets))
                        }
                    })
                    .collect::<Vec<_>>();
                matches.sort_by_key(|(exact, prefix, suffix, _, _)| {
                    std::cmp::Reverse((*exact, *prefix, *suffix))
                });
                if let Some((_, _, _, value, targets)) = matches.first() {
                    return targets.iter().find_map(|target| {
                        candidate(&join(
                            options.base.as_deref().unwrap_or(origin),
                            &target.replace('*', value),
                        ))
                    });
                }
            }
            if let Some(base) = &options.base
                && let Some(path) = candidate(&join(base, raw))
            {
                return Some(path);
            }
        }
        // These conventions are hints only; do not reinterpret arbitrary package names.
        let tail = raw.strip_prefix("@/").or_else(|| raw.strip_prefix("~/"))?;
        let package = self
            .packages
            .iter()
            .find(|dir| contains(dir))
            .map(String::as_str)
            .unwrap_or("");
        let dir = config
            .map(|(d, _)| d.as_str())
            .filter(|d| d.len() > package.len())
            .unwrap_or(package);
        candidate(&join(dir, &format!("src/{tail}"))).or_else(|| candidate(&join(dir, tail)))
    }
}
fn load(
    path: &str,
    sources: &BTreeMap<&str, &str>,
    packages: &BTreeMap<String, String>,
    seen: &mut HashSet<String>,
) -> Options {
    if seen.len() >= 16 || !seen.insert(path.into()) {
        return Options::default();
    }
    let result = (|| {
        let value = jsonc(sources.get(path)?)?;
        let dir = parent_dir(path);
        let extends = value["extends"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_else(|| value["extends"].as_str().into_iter().collect());
        let mut result = Options::default();
        for reference in extends {
            let base = if reference.starts_with('.') {
                join(dir, reference)
            } else {
                let Some((name, package)) = packages
                    .iter()
                    .filter(|(name, _)| {
                        reference == name.as_str()
                            || reference
                                .strip_prefix(name.as_str())
                                .is_some_and(|p| p.starts_with('/'))
                    })
                    .max_by_key(|(name, _)| name.len())
                else {
                    continue;
                };
                let suffix = reference[name.len()..].trim_start_matches('/');
                join(
                    package,
                    if suffix.is_empty() {
                        "tsconfig.json"
                    } else {
                        suffix
                    },
                )
            };
            if let Some(resolved) = [
                base.clone(),
                format!("{base}.json"),
                format!("{base}/tsconfig.json"),
            ]
            .iter()
            .find(|p| sources.contains_key(p.as_str()))
            {
                let inherited = load(resolved, sources, packages, seen);
                if inherited.base.is_some() {
                    result.base = inherited.base;
                }
                if inherited.paths.is_some() {
                    result.paths = inherited.paths;
                }
            }
        }
        let options = &value["compilerOptions"];
        if let Some(base) = options["baseUrl"].as_str() {
            result.base = Some(join(dir, base));
        }
        if let Some(paths) = options["paths"].as_object() {
            result.paths = Some((
                dir.into(),
                paths
                    .iter()
                    .map(|(pattern, values)| {
                        (
                            pattern.clone(),
                            values
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect(),
                        )
                    })
                    .collect(),
            ));
        }
        Some(result)
    })()
    .unwrap_or_default();
    seen.remove(path);
    result
}
/// JSONC comments and trailing commas, preserving quoted text and escapes.
fn jsonc(text: &str) -> Option<Value> {
    let mut bytes = text.as_bytes().to_vec();
    let mut quoted = false;
    let mut i = 0;
    while i < bytes.len() {
        if quoted {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                quoted = false;
            }
        } else if bytes[i] == b'"' {
            quoted = true;
        } else if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                bytes[i] = b' ';
                i += 1;
            }
            continue;
        } else if bytes[i..].starts_with(b"/*") {
            bytes[i..i + 2].fill(b' ');
            i += 2;
            while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                bytes[i] = b' ';
                i += 1;
            }
            if i + 1 < bytes.len() {
                bytes[i..i + 2].fill(b' ');
                i += 2;
            }
            continue;
        }
        i += 1;
    }
    quoted = false;
    i = 0;
    while i < bytes.len() {
        if quoted {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                quoted = false;
            }
        } else if bytes[i] == b'"' {
            quoted = true;
        } else if bytes[i] == b','
            && bytes[i + 1..]
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                .is_some_and(|b| matches!(b, b']' | b'}'))
        {
            bytes[i] = b' ';
        }
        i += 1;
    }
    serde_json::from_slice(&bytes).ok()
}
