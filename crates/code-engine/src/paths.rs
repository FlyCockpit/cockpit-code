//! One resolver for execution and policy, including HOME and symlinked ancestors.
use anyhow::{Context, Result, bail};
use std::path::{Component, Path, PathBuf};
#[derive(Clone)]
pub struct Resolver {
    pub cwd: PathBuf,
    home: Option<PathBuf>,
}
impl Resolver {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
    pub fn resolve(&self, raw: &str) -> Result<PathBuf> {
        let literal = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.cwd.join(raw)
        };
        let cleaned;
        let raw = if literal.exists() {
            raw
        } else {
            cleaned = normalize(raw)?;
            &cleaned
        };
        let expanded = if raw == "~" {
            self.home.clone().context("HOME is not set")?
        } else if let Some(tail) = raw.strip_prefix("~/") {
            self.home.as_ref().context("HOME is not set")?.join(tail)
        } else if raw.starts_with('~') {
            bail!("~user paths are unsupported; use an absolute path")
        } else {
            PathBuf::from(raw)
        };
        let absolute = if expanded.is_absolute() {
            // Salvage only a unique existing suffix inside cwd. Never redirect
            // a real absolute target or guess a destination for a new file.
            if !expanded.exists()
                && !expanded
                    .components()
                    .any(|c| matches!(c, Component::ParentDir))
            {
                let parts = expanded
                    .components()
                    .filter_map(|c| match c {
                        Component::Normal(p) => Some(p),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let mut candidates = std::collections::BTreeSet::new();
                for start in 1..parts.len() {
                    let suffix: PathBuf = parts[start..].iter().collect();
                    let candidate = self.cwd.join(suffix);
                    if let Ok(real) = std::fs::canonicalize(&candidate)
                        && real.starts_with(&self.cwd)
                    {
                        candidates.insert(real);
                    }
                }
                if candidates.len() == 1 {
                    candidates.into_iter().next().unwrap()
                } else {
                    expanded
                }
            } else {
                expanded
            }
        } else {
            self.cwd.join(expanded)
        };
        let mut resolved = PathBuf::new();
        for part in absolute.components() {
            match part {
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::CurDir => {}
                _ => {
                    resolved.push(part.as_os_str());
                    if std::fs::symlink_metadata(&resolved).is_ok() {
                        resolved = std::fs::canonicalize(&resolved)
                            .with_context(|| format!("resolve {}", resolved.display()))?;
                    }
                }
            }
        }
        Ok(resolved)
    }
    pub fn validate(&self, path: &Path) -> Result<()> {
        if self.resolve(path.to_str().context("path is not UTF-8")?)? != path {
            bail!("prepared path changed; retry the operation");
        }
        Ok(())
    }
}

fn normalize(raw: &str) -> Result<String> {
    let mut text = raw;
    loop {
        let trimmed = text.trim();
        let inner = [('<', '>'), ('`', '`'), ('\"', '\"'), ('\'', '\'')]
            .into_iter()
            .find_map(|(open, close)| {
                trimmed
                    .strip_prefix(open)
                    .and_then(|s| s.strip_suffix(close))
            });
        if let Some(inner) = inner {
            text = inner;
        } else {
            break;
        }
    }
    if text.starts_with("file://") {
        return url::Url::parse(text)?
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("file URL must name a local filesystem path"))?
            .into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("path is not UTF-8"));
    }
    Ok(text.to_owned())
}
