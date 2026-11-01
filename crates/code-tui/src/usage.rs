//! Provider usage (D85), live quota (m7) and soft budgets (D87) as plain
//! view types, plus the `cockpit-code.usage/1` parser and display helpers.
//! The data comes through the engine adapter; nothing here runs commands.
use chrono::{DateTime, Local, TimeZone, Utc};
use serde_json::Value;

/// One limit bar: a quota window, a credit pool, a budget.
#[derive(Clone, Debug, Default)]
pub struct Limit {
    pub id: String,
    pub label: String,
    pub used_percent: Option<f64>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub unit: Option<String>,
    /// RFC 3339, normalized from a string or epoch seconds.
    pub resets_at: Option<String>,
    /// A quota event's `remaining` (no unit given by the protocol).
    pub remaining: Option<f64>,
}

impl Limit {
    /// Used share in percent, from `usedPercent` or `used / limit`.
    pub fn percent(&self) -> Option<f64> {
        self.used_percent.or_else(|| match (self.used, self.limit) {
            (Some(u), Some(l)) if l > 0.0 => Some(u / l * 100.0),
            _ => None,
        })
    }
    /// "41 / 100 requests", "$4.10 / $10", or nothing.
    pub fn amounts(&self) -> Option<String> {
        let unit = self.unit.as_deref().unwrap_or("");
        let fmt = |v: f64| {
            if unit == "usd" {
                format!("${v:.2}")
            } else if v.fract() == 0.0 {
                format!("{v:.0}")
            } else {
                format!("{v:.1}")
            }
        };
        let suffix = if unit.is_empty() || unit == "usd" {
            String::new()
        } else {
            format!(" {unit}")
        };
        match (self.used, self.limit) {
            (Some(u), Some(l)) => Some(format!("{} / {}{suffix}", fmt(u), fmt(l))),
            (Some(u), None) => Some(format!("{} used{suffix}", fmt(u))),
            _ => self
                .remaining
                .map(|r| format!("{} remaining{suffix}", fmt(r))),
        }
    }
}

/// A soft per-provider budget (D87); `spent` is what the harness recorded.
#[derive(Clone, Debug)]
pub struct Budget {
    pub usd: f64,
    /// day, week or month.
    pub period: String,
    pub spent: Option<f64>,
    /// The engine's end of the current (UTC) period, when known.
    pub resets: Option<String>,
}

impl Budget {
    pub fn limit(&self) -> Limit {
        Limit {
            id: "budget".into(),
            label: format!("budget · per {}", self.period),
            used: Some(self.spent.unwrap_or(0.0)),
            limit: Some(self.usd),
            unit: Some("usd".into()),
            resets_at: self.resets.clone().or_else(|| period_end(&self.period)),
            ..Limit::default()
        }
    }
}

/// Everything `/usage` shows for one provider.
#[derive(Clone, Debug, Default)]
pub struct ProviderUsage {
    pub provider: String,
    pub label: String,
    /// Every model of the provider bills by subscription.
    pub subscription: bool,
    /// The provider declares a `usage` command (D85).
    pub declared: bool,
    pub account: Option<String>,
    pub limits: Vec<Limit>,
    pub credits: Option<String>,
    pub notes: Option<String>,
    pub budget: Option<Budget>,
    pub error: Option<String>,
    /// When the shown document was produced.
    pub fetched: Option<DateTime<Local>>,
    /// The document is past its TTL (a refresh failed or is due).
    pub stale: bool,
    /// The engine is running the usage command now.
    pub refreshing: bool,
    /// Calls in the budget period with no known cost (D87).
    pub unknown_cost_calls: usize,
    /// Advisory 80%/100% budget warnings.
    pub warnings: Vec<String>,
}

impl ProviderUsage {
    /// Every bar: reported limits, then the budget.
    pub fn bars(&self) -> Vec<Limit> {
        let mut v = self.limits.clone();
        if let Some(b) = &self.budget {
            v.push(b.limit());
        }
        v
    }
    /// The tightest bar with a known share.
    pub fn tightest(&self) -> Option<Limit> {
        tightest(&self.bars())
    }
}

pub fn tightest(limits: &[Limit]) -> Option<Limit> {
    limits
        .iter()
        .filter(|l| l.percent().is_some())
        .max_by(|a, b| a.percent().partial_cmp(&b.percent()).unwrap())
        .cloned()
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// A JSON value as one display line (objects: their scalar values).
/// A string that holds a decimal number.
fn numeric(s: &str) -> Option<f64> {
    let t = s.trim();
    (t.contains('.')
        && t.chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-'))
    .then(|| t.parse::<f64>().ok())
    .flatten()
}

/// A number for display: two decimals when fractional, thousands grouped.
fn fmt_number(f: f64) -> String {
    let s = if f.fract() != 0.0 {
        format!("{f:.2}")
    } else {
        format!("{f:.0}")
    };
    let (int, frac) = s
        .split_once('.')
        .map_or((s.as_str(), None), |(a, b)| (a, Some(b)));
    let (sign, digits) = int.strip_prefix('-').map_or(("", int), |d| ("-", d));
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    match frac {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

pub fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        // Decimal strings ("61133.9613550000") read like numbers.
        Value::String(s) => Some(match numeric(s) {
            Some(f) => fmt_number(f),
            None => s.clone(),
        }),
        Value::Number(n) => Some(match n.as_f64() {
            Some(f) => fmt_number(f),
            None => n.to_string(),
        }),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().filter_map(text).collect();
            (!parts.is_empty()).then(|| parts.join(" · "))
        }
        // Objects: their scalar values, in order ("plus · me@example.com").
        Value::Object(m) => {
            let parts: Vec<String> = m
                .iter()
                .filter_map(|(k, v)| match v {
                    Value::Object(_) | Value::Array(_) => None,
                    Value::Number(n) if k.to_lowercase().contains("usd") => {
                        Some(format!("${:.2}", n.as_f64().unwrap_or_default()))
                    }
                    // Flags read as their name when set, and not at all when not.
                    Value::Bool(true) => Some(k.replace('_', " ")),
                    Value::Bool(false) => None,
                    v => text(v).map(|s| match v.as_str() {
                        Some(raw) if numeric(raw).is_none() => s,
                        _ => format!("{} {s}", k.replace('_', " ")),
                    }),
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join(" · "))
        }
    }
}

/// A reset time as RFC 3339, from a string or epoch seconds/milliseconds.
fn reset(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc).to_rfc3339())
            .or_else(|| Some(s.to_string()));
    }
    let n = v.as_i64()?;
    let secs = if n > 10_000_000_000 { n / 1000 } else { n };
    Utc.timestamp_opt(secs, 0).single().map(|d| d.to_rfc3339())
}

fn limit(v: &Value, i: usize) -> Limit {
    Limit {
        id: v["id"]
            .as_str()
            .map_or_else(|| format!("limit{i}"), String::from),
        label: v["label"]
            .as_str()
            .or(v["id"].as_str())
            .unwrap_or("limit")
            .to_string(),
        used_percent: num(&v["usedPercent"]),
        used: num(&v["used"]),
        limit: num(&v["limit"]),
        unit: v["unit"].as_str().map(String::from),
        resets_at: reset(&v["resetsAt"]),
        remaining: num(&v["remaining"]),
    }
}

/// A `cockpit-code.usage/1` document: (account, limits, credits, notes).
pub type Doc = (Option<String>, Vec<Limit>, Option<String>, Option<String>);

pub fn parse_doc(v: &Value) -> Result<Doc, String> {
    if !v.is_object() {
        return Err("usage output is not a JSON object".into());
    }
    if let Some(p) = v["protocol"].as_str()
        && p != "cockpit-code.usage/1"
    {
        return Err(format!("unsupported usage protocol {p}"));
    }
    let limits = v["limits"]
        .as_array()
        .map(|a| a.iter().enumerate().map(|(i, l)| limit(l, i)).collect())
        .unwrap_or_default();
    let credits = match &v["credits"] {
        Value::Number(n) => Some(format!("${:.2}", n.as_f64().unwrap_or_default())),
        c => text(c),
    };
    Ok((text(&v["account"]), limits, credits, text(&v["notes"])))
}

/// A live JSONL `quota` event (or discovery account quota) as a bar.
pub fn quota_limit(v: &Value) -> Option<Limit> {
    if !v.is_object() {
        return None;
    }
    let l = Limit {
        id: "quota".into(),
        label: v["label"].as_str().unwrap_or("quota").to_string(),
        used_percent: num(&v["usedPercent"]),
        resets_at: reset(&v["resetsAt"]),
        remaining: num(&v["remaining"]),
        ..Limit::default()
    };
    (l.used_percent.is_some() || l.remaining.is_some() || l.resets_at.is_some()).then_some(l)
}

/// Discovery `account` metadata: a D85-shaped `limits` list, a `quota`
/// object, or the account itself carrying quota fields.
pub fn account_limits(account: &Value) -> Vec<Limit> {
    if let Some(a) = account["limits"].as_array() {
        return a.iter().enumerate().map(|(i, l)| limit(l, i)).collect();
    }
    quota_limit(&account["quota"])
        .or_else(|| quota_limit(account))
        .into_iter()
        .collect()
}

/// A reset time for display: "20:00" today, "Tue 09:00" within a week,
/// "Nov 3" after that.
pub fn fmt_reset(rfc: &str) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(rfc) else {
        return rfc.to_string();
    };
    let t = t.with_timezone(&Local);
    let now = Local::now();
    let days = (t.date_naive() - now.date_naive()).num_days();
    if days == 0 {
        t.format("%H:%M").to_string()
    } else if (1..7).contains(&days) {
        t.format("%a %H:%M").to_string()
    } else {
        t.format("%b %-d").to_string()
    }
}

/// The end of the current budget period (local midnight; weeks end on
/// Monday, months on the 1st).
fn period_end(period: &str) -> Option<String> {
    use chrono::{Datelike, Duration};
    let today = Local::now().date_naive();
    let end = match period {
        "day" => today + Duration::days(1),
        "week" => today + Duration::days(7 - today.weekday().num_days_from_monday() as i64),
        "month" => {
            let (y, m) = if today.month() == 12 {
                (today.year() + 1, 1)
            } else {
                (today.year(), today.month() + 1)
            };
            chrono::NaiveDate::from_ymd_opt(y, m, 1)?
        }
        _ => return None,
    };
    Local
        .from_local_datetime(&end.and_hms_opt(0, 0, 0)?)
        .single()
        .map(|d| d.with_timezone(&Utc).to_rfc3339())
}

/// The status-line badge: "quota 41% · resets 20:00".
pub fn badge(l: &Limit) -> Option<String> {
    let p = l.percent()?;
    let what = if l.id == "budget" { "budget" } else { "quota" };
    Some(match &l.resets_at {
        Some(r) => format!("{what} {p:.0}% · resets {}", fmt_reset(r)),
        None => format!("{what} {p:.0}%"),
    })
}
