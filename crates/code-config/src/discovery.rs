//! Bounded, argv-only discovery. Cached data is public model metadata, never credentials.
use super::*;
use std::{
    hash::{Hash, Hasher},
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant, UNIX_EPOCH},
};

pub const PROTOCOL: &str = "cockpit-code.provider/1";
pub const FAILURE_TTL_SECS: u64 = 300;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryErrorKind {
    Configuration,
    Spawn,
    Timeout,
    Wait,
    Exit,
    Output,
    Json,
    Protocol,
    Schema,
    Cache,
}
impl DiscoveryErrorKind {
    fn message(self) -> &'static str {
        match self {
            Self::Configuration => "invalid discovery command configuration",
            Self::Spawn => "discovery spawn failed",
            Self::Timeout => "discovery timed out",
            Self::Wait => "discovery wait failed",
            Self::Exit => "discovery exited unsuccessfully",
            Self::Output => "discovery output exceeded limit or could not be read",
            Self::Json => "invalid discovery JSON",
            Self::Protocol => "unsupported discovery protocol or missing models",
            Self::Schema => "discovery model schema invalid",
            Self::Cache => "could not read or write discovery cache",
        }
    }
}
/// Safe to display: messages never contain argv, command output or credentials.
#[derive(Clone, Debug, Serialize, Deserialize, Error)]
#[serde(rename_all = "camelCase")]
#[error("{message}")]
pub struct DiscoveryError {
    pub kind: DiscoveryErrorKind,
    pub message: String,
    pub cached: bool,
    /// Unix seconds; cached failures expire at this time. Refresh bypasses it.
    pub retry_at: u64,
}
impl DiscoveryError {
    fn new(kind: DiscoveryErrorKind) -> Self {
        Self {
            kind,
            message: kind.message().into(),
            cached: false,
            retry_at: now().saturating_add(FAILURE_TTL_SECS),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Failure {
    kind: DiscoveryErrorKind,
    failed_at: u64,
    provider_key: u64,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn provider_key(provider: &Provider) -> u64 {
    // Only a hash of unresolved configuration is stored, never connection data.
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_vec(provider)
        .unwrap_or_default()
        .hash(&mut hash);
    hash.finish()
}
fn failure_path(cache: &Path) -> PathBuf {
    cache.with_extension("error.json")
}
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
fn cache_failure(provider: &Provider, cache: &Path, error: &DiscoveryError) {
    let failure = Failure {
        kind: error.kind,
        failed_at: error.retry_at.saturating_sub(FAILURE_TTL_SECS),
        provider_key: provider_key(provider),
    };
    if let Ok(bytes) = serde_json::to_vec(&failure) {
        let _ = atomic_write(&failure_path(cache), &bytes);
    }
}
/// Compatibility entry point. Snapshot callers also receive structured errors.
pub fn discover(
    provider: &Provider,
    cache: &Path,
    refresh: bool,
) -> Result<Option<serde_json::Value>> {
    discover_cached(provider, cache, refresh, true).map_err(|e| ConfigError::Invalid(e.to_string()))
}
fn discover_cached(
    provider: &Provider,
    cache: &Path,
    refresh: bool,
    execute: bool,
) -> std::result::Result<Option<serde_json::Value>, DiscoveryError> {
    let Some(config) = provider.command.as_ref().and_then(|c| c.discover.as_ref()) else {
        return Ok(None);
    };
    if !refresh {
        if let Ok(failure) = parse::<Failure>(&failure_path(cache)) {
            let retry_at = failure.failed_at.saturating_add(FAILURE_TTL_SECS);
            if failure.provider_key == provider_key(provider)
                && failure.failed_at <= now()
                && now() < retry_at
            {
                return Err(DiscoveryError {
                    kind: failure.kind,
                    message: failure.kind.message().into(),
                    cached: true,
                    retry_at,
                });
            }
        }
        let fresh = modified(cache)
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() < config.ttl_secs.unwrap_or(86400));
        // A failure supersedes an older success even after its short TTL expires.
        if fresh
            && modified(&failure_path(cache)).is_none_or(|failed| modified(cache) > Some(failed))
            && let Ok(value) = parse::<serde_json::Value>(cache)
            && value["protocol"] == PROTOCOL
            && value["models"].is_object()
        {
            return Ok(Some(value));
        }
    }
    if !execute {
        return Ok(None);
    }
    let result = run(provider).and_then(|value| {
        atomic_write(cache, &serde_json::to_vec(&value).unwrap())
            .map_err(|_| DiscoveryError::new(DiscoveryErrorKind::Cache))?;
        let _ = fs::remove_file(failure_path(cache));
        Ok(Some(value))
    });
    if let Err(error) = &result {
        cache_failure(provider, cache, error);
    }
    result
}
fn run(provider: &Provider) -> std::result::Result<serde_json::Value, DiscoveryError> {
    use DiscoveryErrorKind as Kind;
    let resolved = provider
        .connection_command()
        .ok_or_else(|| DiscoveryError::new(Kind::Configuration))?;
    let argv = resolved
        .discover_argv
        .as_ref()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| DiscoveryError::new(Kind::Configuration))?;
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .envs(&resolved.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(cwd) = &resolved.cwd {
        command.current_dir(
            cwd.replace(
                "{cwd}",
                &env::current_dir()
                    .map_err(|_| DiscoveryError::new(Kind::Configuration))?
                    .to_string_lossy(),
            ),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|_| DiscoveryError::new(Kind::Spawn))?;
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(_) => break Err(DiscoveryError::new(Kind::Wait)),
            _ => {}
        }
        if Instant::now() >= deadline {
            break Err(DiscoveryError::new(Kind::Timeout));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Always reap the process group and join its stdout reader, including failures.
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    let bytes = reader.join().ok().and_then(|r| r.ok());
    let status = status?;
    if !status.success() {
        return Err(DiscoveryError::new(Kind::Exit));
    }
    let bytes = bytes
        .filter(|b| b.len() <= 4 * 1024 * 1024)
        .ok_or_else(|| DiscoveryError::new(Kind::Output))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| DiscoveryError::new(Kind::Json))?;
    if value["protocol"] != PROTOCOL || !value["models"].is_object() {
        return Err(DiscoveryError::new(Kind::Protocol));
    }
    Ok(value)
}
fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok().and_then(|m| m.modified().ok())
}
fn merge(base: &mut serde_json::Value, overrides: &serde_json::Value) {
    if let (Some(base), Some(overrides)) = (base.as_object_mut(), overrides.as_object()) {
        for (key, value) in overrides {
            if let Some(old) = base.get_mut(key) {
                merge(old, value);
            } else {
                base.insert(key.clone(), value.clone());
            }
        }
    } else {
        *base = overrides.clone();
    }
}

pub(super) fn enrich(
    snapshot: &mut Snapshot,
    dirs: &Directories,
    refresh: &BTreeSet<String>,
    execute: bool,
) {
    enrich_selected(snapshot, dirs, refresh, execute, None);
}

pub(super) fn enrich_selected(
    snapshot: &mut Snapshot,
    dirs: &Directories,
    refresh: &BTreeSet<String>,
    execute: bool,
    selected: Option<&str>,
) {
    for (id, provider) in &mut snapshot.providers {
        if selected.is_some_and(|selected| selected != id)
            || !provider.enabled
            || provider
                .command
                .as_ref()
                .and_then(|c| c.discover.as_ref())
                .is_none()
        {
            continue;
        }
        let source = dirs.config.join("providers").join(format!("{id}.json"));
        let cache = dirs.cache.join("discover").join(format!("{id}.json"));
        let latest = modified(&cache).max(modified(&failure_path(&cache)));
        let changed = modified(&source)
            .zip(latest)
            .is_some_and(|(config, cached)| config > cached);
        let result = discover_cached(provider, &cache, refresh.contains(id) || changed, execute)
            .and_then(|value| {
                let Some(value) = value else { return Ok(None) };
                let result = (|| -> Result<Provider> {
                    let mut raw: serde_json::Value = parse(&source)?;
                    let mut models = value["models"].clone();
                    if raw["models"].is_object() {
                        merge(&mut models, &raw["models"]);
                    }
                    raw["models"] = models;
                    let mut next: Provider = serde_json::from_value(raw).map_err(|_| {
                        ConfigError::Invalid("invalid discovered model schema".into())
                    })?;
                    next.resolve()?;
                    Ok(next)
                })();
                result.map(Some).map_err(|_| {
                    let error = DiscoveryError::new(DiscoveryErrorKind::Schema);
                    cache_failure(provider, &cache, &error);
                    error
                })
            });
        match result {
            Ok(Some(next)) => *provider = next,
            Ok(None) => {}
            Err(error) => {
                snapshot
                    .errors
                    .push(format!("{id}: {error}; using static models"));
                snapshot.discovery_errors.insert(id.clone(), error);
            }
        }
    }
    for (id, provider) in &mut snapshot.providers {
        if provider
            .command
            .as_ref()
            .is_some_and(|c| c.history.as_deref() == Some("latest"))
        {
            for (mid, model) in &mut provider.models {
                if model.kind.unwrap_or(provider.kind) != ProviderType::Command {
                    continue;
                }
                let warning = "Warning: history: latest disables hidden forks and selfEvaluateWrites; context usage requires provider reporting.";
                model.description.push_str(&format!("\n{warning}"));
                snapshot.errors.push(format!("{id}/{mid}: {warning}"));
            }
        }
    }
}
