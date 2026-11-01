//! Provider account usage and soft budgets. Catalog reads never run a command.
use crate::schema::{EngineError, UsageRecord};
use chrono::{Datelike, Duration, TimeZone, Utc};
use code_config::{BudgetPeriod, ConfigStore, Provider, Snapshot};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex, OnceLock, Weak},
};
use tokio::io::AsyncReadExt;

pub const PROTOCOL: &str = "cockpit-code.usage/1";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limit {
    pub id: String,
    pub label: String,
    pub used_percent: Option<f64>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub unit: Option<String>,
    pub resets_at: Option<String>,
}
impl Limit {
    pub fn percent(&self) -> Option<f64> {
        self.used_percent.or_else(|| {
            self.used
                .zip(self.limit)
                .filter(|(_, l)| *l > 0.0)
                .map(|(u, l)| u / l * 100.0)
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    pub protocol: String,
    #[serde(default)]
    pub account: Value,
    #[serde(default)]
    pub limits: Vec<Limit>,
    #[serde(default)]
    pub credits: Option<Value>,
    #[serde(default)]
    pub notes: Option<Value>,
}
impl Document {
    fn validate(&self) -> Result<(), EngineError> {
        let mut ids = BTreeSet::new();
        if self.protocol != PROTOCOL
            || self.limits.iter().any(|l| {
                l.id.is_empty()
                    || !ids.insert(&l.id)
                    || l.label.is_empty()
                    || [l.used_percent, l.used, l.limit]
                        .into_iter()
                        .flatten()
                        .any(|n| !n.is_finite() || n < 0.0)
                    || l.limit == Some(0.0)
                    || l.resets_at
                        .as_ref()
                        .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_err())
            })
        {
            return Err(EngineError::bad(
                "usage command returned an invalid cockpit-code.usage/1 document",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cached {
    pub document: Option<Document>,
    pub fetched_at: Option<String>,
    pub checked_at: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    fingerprint: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsage {
    pub provider: String,
    pub label: Option<String>,
    pub document: Option<Document>,
    pub fetched_at: Option<String>,
    pub error: Option<String>,
    pub stale: bool,
    pub refreshing: bool,
    pub budget: Option<Limit>,
    pub spent_usd: f64,
    pub unknown_cost_calls: usize,
    pub warnings: Vec<String>,
    pub headroom: Value,
}
#[derive(Default)]
struct Data {
    cached: BTreeMap<String, Cached>,
    running: BTreeSet<String>,
    costs: BTreeMap<String, (String, UsageRecord)>,
    costs_loaded: bool,
}
pub struct Cache {
    config: Mutex<Snapshot>,
    data: Mutex<Data>,
    cache_dir: PathBuf,
    data_dir: PathBuf,
}
fn instances() -> &'static Mutex<BTreeMap<PathBuf, Weak<Cache>>> {
    static INSTANCES: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Cache>>>> = OnceLock::new();
    INSTANCES.get_or_init(Mutex::default)
}
fn fingerprint(provider: &Provider) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                provider.usage_argv(),
                provider.connection_command().map(|c| &c.env)
            ))
            .unwrap_or_default()
        )
    )
}
impl Cache {
    /// Starts at frontend startup; all sessions in this process share the same cache.
    pub fn start(store: &ConfigStore) -> Arc<Self> {
        let mut instances = instances().lock().unwrap();
        if let Some(cache) = instances
            .get(&store.directories.data)
            .and_then(Weak::upgrade)
        {
            cache.update_config(store.snapshot());
            return cache;
        }
        let cache = Arc::new(Self {
            config: Mutex::new(store.snapshot().clone()),
            data: Mutex::default(),
            cache_dir: store.directories.cache.join("usage"),
            data_dir: store.directories.data.clone(),
        });
        instances.insert(store.directories.data.clone(), Arc::downgrade(&cache));
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&cache);
            runtime.spawn(async move {
                let Some(cache) = weak.upgrade() else {
                    return;
                };
                let data_dir = cache.data_dir.clone();
                let cache_dir = cache.cache_dir.clone();
                let (cached, costs) = tokio::task::spawn_blocking(move || {
                    (
                        load_cache(cache_dir),
                        crate::sessions::usage_records(&data_dir),
                    )
                })
                .await
                .unwrap_or_default();
                let mut quotas = costs
                    .iter()
                    .filter(|(_, u)| u.quota.is_some())
                    .cloned()
                    .collect::<Vec<_>>();
                quotas.sort_by(|a, b| a.0.cmp(&b.0));
                {
                    let mut data = cache.data.lock().unwrap();
                    // A live turn may have arrived while historical records were loading.
                    for (ts, usage) in costs {
                        data.costs
                            .entry(usage.call_id.clone())
                            .or_insert((ts, usage));
                    }
                    for (id, value) in cached {
                        data.cached.entry(id).or_insert(value);
                    }
                    data.costs_loaded = true;
                }
                for (ts, usage) in quotas {
                    let newer = cache
                        .data
                        .lock()
                        .unwrap()
                        .cached
                        .get(&usage.provider)
                        .and_then(|c| c.fetched_at.as_ref())
                        .is_none_or(|prior| prior < &ts);
                    if newer {
                        cache.observe(&ts, &usage);
                    }
                }
                drop(cache);
                loop {
                    let Some(cache) = weak.upgrade() else {
                        break;
                    };
                    cache.refresh(false).await;
                    drop(cache);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            });
        }
        cache
    }
    pub fn update_config(&self, snapshot: &Snapshot) {
        *self.config.lock().unwrap() = snapshot.clone();
    }
    pub fn observe(&self, ts: &str, usage: &UsageRecord) {
        let fingerprint = self
            .config
            .lock()
            .unwrap()
            .providers
            .get(&usage.provider)
            .map(fingerprint);
        let mut data = self.data.lock().unwrap();
        data.costs
            .entry(usage.call_id.clone())
            .or_insert_with(|| (ts.into(), usage.clone()));
        if let Some(quota) = &usage.quota {
            let document = if quota["protocol"] == PROTOCOL {
                serde_json::from_value::<Document>(quota.clone())
                    .ok()
                    .filter(|d| d.validate().is_ok())
            } else {
                let limits = quota.get("limits").cloned().unwrap_or_else(|| json!([{
                    "id":quota["id"].as_str().unwrap_or("live"),"label":quota["label"].as_str().unwrap_or("Provider quota"),
                    "usedPercent":quota["usedPercent"],"used":quota["used"],"limit":quota["limit"],"unit":quota["unit"],"resetsAt":quota["resetsAt"]
                }]));
                serde_json::from_value::<Vec<Limit>>(limits)
                    .ok()
                    .map(|limits| Document {
                        protocol: PROTOCOL.into(),
                        account: Value::Null,
                        limits,
                        credits: None,
                        notes: None,
                    })
                    .filter(|d| d.validate().is_ok())
            };
            if let Some(document) = document {
                let entry = data.cached.entry(usage.provider.clone()).or_default();
                if let Some(fingerprint) = fingerprint {
                    entry.fingerprint = fingerprint;
                }
                let target = entry.document.get_or_insert_with(|| Document {
                    protocol: PROTOCOL.into(),
                    account: Value::Null,
                    limits: vec![],
                    credits: None,
                    notes: None,
                });
                for limit in document.limits {
                    target.limits.retain(|old| old.id != limit.id);
                    target.limits.push(limit);
                }
                if !document.account.is_null() {
                    target.account = document.account;
                }
                if document.credits.is_some() {
                    target.credits = document.credits;
                }
                entry.fetched_at = Some(ts.into());
                entry.error = None;
            }
        }
    }
    pub fn request_refresh(self: &Arc<Self>) {
        let cache = self.clone();
        tokio::spawn(async move {
            cache.refresh(true).await;
        });
    }
    pub async fn refresh(self: &Arc<Self>, force: bool) {
        let providers = self.config.lock().unwrap().providers.clone();
        let mut tasks = Vec::new();
        for (id, provider) in providers {
            if !provider.enabled || provider.usage_argv().is_none() {
                continue;
            }
            let fingerprint = fingerprint(&provider);
            {
                let mut data = self.data.lock().unwrap();
                let fresh = data.cached.get(&id).is_some_and(|c| {
                    c.fingerprint == fingerprint
                        && c.checked_at
                            .as_ref()
                            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                            .is_some_and(|t| {
                                (Utc::now() - t.with_timezone(&Utc)).num_seconds()
                                    < provider
                                        .usage
                                        .as_ref()
                                        .and_then(|u| u.ttl_secs)
                                        .unwrap_or(300)
                                        .min(i64::MAX as u64)
                                        as i64
                            })
                });
                if data.running.contains(&id) || (!force && fresh) {
                    continue;
                }
                data.running.insert(id.clone());
            }
            let cache = self.clone();
            tasks.push(tokio::spawn(async move {
                let result = read_command(&provider).await;
                let entry = {
                    let mut data = cache.data.lock().unwrap();
                    data.running.remove(&id);
                    let entry = data.cached.entry(id.clone()).or_default();
                    if entry.fingerprint != fingerprint {
                        *entry = Cached::default();
                    }
                    entry.fingerprint = fingerprint;
                    entry.checked_at = Some(crate::sessions::now());
                    match result {
                        Ok(document) => {
                            entry.document = Some(document);
                            entry.fetched_at = entry.checked_at.clone();
                            entry.error = None;
                        }
                        Err(error) => entry.error = Some(error.message),
                    }
                    entry.clone()
                };
                let path = cache
                    .cache_dir
                    .join(format!("{:x}.json", Sha256::digest(id.as_bytes())));
                let _ = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                    std::fs::create_dir_all(path.parent().unwrap())?;
                    use std::io::Write;
                    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
                    serde_json::to_writer(&mut file, &json!({"provider":id,"cached":entry}))?;
                    file.flush()?;
                    file.persist(path)?;
                    Ok(())
                })
                .await;
            }));
        }
        for task in tasks {
            let _ = task.await;
        }
    }
    pub fn snapshot(&self) -> Vec<ProviderUsage> {
        let config = self.config.lock().unwrap();
        let data = self.data.lock().unwrap();
        config
            .providers
            .iter()
            .map(|(id, provider)| view(id, provider, &data))
            .collect()
    }
    pub fn headroom(&self, provider: &str) -> Value {
        let config = self.config.lock().unwrap();
        let data = self.data.lock().unwrap();
        config
            .providers
            .get(provider)
            .map(|p| view(provider, p, &data).headroom)
            .unwrap_or(Value::Null)
    }
}
fn load_cache(path: PathBuf) -> BTreeMap<String, Cached> {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let value: Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
            let cached: Cached = serde_json::from_value(value["cached"].clone()).ok()?;
            if cached
                .document
                .as_ref()
                .is_some_and(|d| d.validate().is_err())
            {
                return None;
            }
            Some((value["provider"].as_str()?.into(), cached))
        })
        .collect()
}
fn period(period: BudgetPeriod) -> (String, String) {
    let now = Utc::now().date_naive();
    let (start, end) = match period {
        BudgetPeriod::Day => (now, now + Duration::days(1)),
        BudgetPeriod::Week => {
            let start = now - Duration::days(now.weekday().num_days_from_monday().into());
            (start, start + Duration::days(7))
        }
        BudgetPeriod::Month => {
            let start = now.with_day(1).unwrap();
            let end = if now.month() == 12 {
                chrono::NaiveDate::from_ymd_opt(now.year() + 1, 1, 1).unwrap()
            } else {
                chrono::NaiveDate::from_ymd_opt(now.year(), now.month() + 1, 1).unwrap()
            };
            (start, end)
        }
    };
    let stamp = |date: chrono::NaiveDate| {
        Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    (stamp(start), stamp(end))
}
fn view(id: &str, provider: &Provider, data: &Data) -> ProviderUsage {
    let cached = data
        .cached
        .get(id)
        .filter(|c| c.fingerprint.is_empty() || c.fingerprint == fingerprint(provider))
        .cloned()
        .unwrap_or_default();
    let document = cached.document.clone();
    let bounds = provider.budget.as_ref().map(|b| period(b.period));
    let mut spent = 0.0;
    let mut unknown = 0;
    for (ts, usage) in data.costs.values().filter(|(_, u)| u.provider == id) {
        if bounds.as_ref().is_some_and(|(start, end)| {
            let t = chrono::DateTime::parse_from_rfc3339(ts).ok();
            t.is_none_or(|t| {
                t < chrono::DateTime::parse_from_rfc3339(start).unwrap()
                    || t >= chrono::DateTime::parse_from_rfc3339(end).unwrap()
            })
        }) {
            continue;
        }
        if let Some(cost) = usage.cost_usd.filter(|c| c.is_finite() && *c >= 0.0) {
            spent += cost;
        } else {
            unknown += 1;
        }
    }
    // Account-reported spend can include requests made outside this harness. Require matching period.
    if let (Some((start, _)), Some(credits)) =
        (&bounds, document.as_ref().and_then(|d| d.credits.as_ref()))
        && credits["periodStart"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            == chrono::DateTime::parse_from_rfc3339(start).ok()
        && let Some(reported) = credits["spentUsd"]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
    {
        spent = spent.max(reported);
    }
    let budget = provider.budget.as_ref().map(|b| Limit {
        id: "budget".into(),
        label: format!("Soft {:?} budget", b.period),
        used_percent: Some(spent / b.usd * 100.0),
        used: Some(spent),
        limit: Some(b.usd),
        unit: Some("USD".into()),
        resets_at: bounds.map(|(_, end)| end),
    });
    let mut warnings = Vec::new();
    if let Some(budget) = &budget {
        let percent = budget.percent().unwrap_or(0.0);
        if percent >= 80.0 {
            warnings.push(format!(
                "{id}: soft budget at {:.0}%{}; calls remain enabled",
                if percent >= 100.0 { 100.0 } else { 80.0 },
                if percent >= 100.0 { " or more" } else { "" }
            ));
        }
    }
    let quota = document
        .as_ref()
        .into_iter()
        .flat_map(|d| &d.limits)
        .filter_map(|l| l.percent().map(|p| (l, p)))
        .max_by(|a, b| a.1.total_cmp(&b.1));
    let tightest = quota
        .map(|(l, p)| ("quota", l, p))
        .into_iter()
        .chain(
            budget
                .as_ref()
                .and_then(|l| l.percent().map(|p| ("budget", l, p))),
        )
        .max_by(|a, b| a.2.total_cmp(&b.2));
    let mut headroom = if let Some((kind, limit, percent)) = tightest {
        json!({"kind":kind,"remainingPercent":(100.0-percent).max(0.0),"resetsAt":limit.resets_at})
    } else {
        json!({"kind":if provider.usage.is_some() {"quota"} else {"unlimited"}})
    };
    if data.costs_loaded {
        headroom["spentUsd"] = json!(spent);
    } else if headroom["kind"] == "budget" {
        headroom.as_object_mut().unwrap().remove("remainingPercent");
    }
    let expired = cached
        .fetched_at
        .as_ref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .is_none_or(|t| {
            (Utc::now() - t.with_timezone(&Utc)).num_seconds()
                >= provider
                    .usage
                    .as_ref()
                    .and_then(|u| u.ttl_secs)
                    .unwrap_or(300)
                    .min(i64::MAX as u64) as i64
        });
    ProviderUsage {
        provider: id.into(),
        label: provider.label.clone(),
        document,
        fetched_at: cached.fetched_at,
        error: cached.error.clone(),
        stale: cached.error.is_some()
            || (provider.usage.is_some() && expired)
            || !data.costs_loaded,
        refreshing: data.running.contains(id),
        budget,
        spent_usd: spent,
        unknown_cost_calls: unknown,
        warnings,
        headroom,
    }
}
async fn read_command(provider: &Provider) -> Result<Document, EngineError> {
    let argv = provider
        .usage_argv()
        .ok_or_else(|| EngineError::bad("no usage command"))?;
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(config) = provider.connection_command() {
        command.envs(&config.env);
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|_| EngineError::bad("could not spawn usage command"))?;
    let _group = crate::model::command::ProcessGroup(child.id().unwrap());
    let stdout = child.stdout.take().unwrap();
    let mut bytes = Vec::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        stdout
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| EngineError::bad("could not read usage command"))?;
        if bytes.len() > 1024 * 1024 {
            return Err(EngineError::bad("usage document exceeds 1 MiB"));
        }
        let status = child
            .wait()
            .await
            .map_err(|_| EngineError::bad("could not wait for usage command"))?;
        if !status.success() {
            return Err(EngineError::bad(format!(
                "usage command exited {}",
                status.code().map_or("by signal".into(), |c| c.to_string())
            )));
        }
        let document: Document = serde_json::from_slice(&bytes)
            .map_err(|_| EngineError::bad("usage command returned invalid JSON"))?;
        document.validate()?;
        Ok(document)
    })
    .await
    .unwrap_or_else(|_| Err(EngineError::bad("usage command timed out after 30s")));
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result
}
impl crate::Session {
    pub fn provider_usage(&self) -> Vec<ProviderUsage> {
        self.control.usage.snapshot()
    }
    pub fn refresh_usage(&self) {
        self.control.usage.request_refresh();
    }
}
