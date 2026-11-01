//! Context hints are appended once at delivery, preserving historical request bytes.
use crate::{Control, model, schema::HistoryEvent};

pub(crate) fn tag(percent: u64, child: bool, compact_at: f64) -> String {
    let hint = if child && percent as f64 >= compact_at * 100.0 - 10.0 {
        format!(
            " - self-compact soon (auto at {})",
            (compact_at * 100.0).round()
        )
    } else if child && percent >= 50 {
        " - pick a good point to compact".into()
    } else if !child && percent >= 70 {
        " - consider delegating to subagents or asking the user to /compact".into()
    } else {
        String::new()
    };
    let id = &uuid::Uuid::new_v4().simple().to_string()[..4];
    format!("\n<ctxPercentage id=\"{id}\">{percent}{hint}</ctxPercentage id=\"{id}\">")
}
impl Control {
    pub(crate) fn context_tag(&self, agent: &str, incoming: &str) -> String {
        let names = self.tool_names(agent);
        let config = self.config.lock().unwrap().clone();
        let j = self.journal.lock().unwrap();
        let history = j.histories.get(agent).map(Vec::as_slice).unwrap_or(&[]);
        let checkpoint = j
            .requests
            .get(agent)
            .filter(|p| j.contexts.get(agent) == Some(&p.context));
        let id = if agent == "root" {
            j.settings
                .model
                .as_deref()
                .or(config.config.default_model.as_deref())
        } else {
            j.agents.get(agent).and_then(|a| a.model.as_deref())
        };
        let model_config = id
            .and_then(|id| id.split_once('/'))
            .and_then(|(p, m)| config.providers.get(p)?.models.get(m));
        let model_config = checkpoint.map(|p| &p.config).or(model_config);
        let window = model_config.and_then(|m| m.context_window);
        let compact_at = id
            .and_then(|id| j.settings.compact_at.get(id))
            .copied()
            .or_else(|| model_config.and_then(|m| m.compact_at))
            .unwrap_or(0.8);
        let start = checkpoint.map_or(0, |p| p.history_len.min(history.len()));
        let delta = history[start..]
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    HistoryEvent::User { .. }
                        | HistoryEvent::Assistant { .. }
                        | HistoryEvent::ToolResult { .. }
                        | HistoryEvent::SystemReminder { .. }
                        | HistoryEvent::Compaction { .. }
                )
            })
            .collect::<Vec<_>>();
        let base = checkpoint.map_or(0, |p| {
            history
                .iter()
                .rev()
                .find_map(|e| match e {
                    HistoryEvent::Usage { usage } if usage.kind == "agent" => {
                        usage.tokens.input_tokens
                    }
                    _ => None,
                })
                .unwrap_or(p.request.input_estimate)
        });
        let used = base
            + model::count_tokens(&serde_json::to_vec(&delta).unwrap_or_default())
            + model::count_tokens(incoming.as_bytes());
        names.rewrite(&tag(
            window.map_or(0, |w| used.saturating_mul(100) / w.max(1)),
            agent != "root",
            compact_at,
        ))
    }
}

impl crate::State {
    pub(crate) fn suggest_root_compaction(
        &self,
        model: &model::Model,
    ) -> Result<bool, crate::schema::EngineError> {
        if self.agent_id != "root" {
            return Ok(false);
        }
        let meter = self.control.meter.lock().unwrap().clone();
        let Some(window) = meter.window.filter(|w| *w > 0) else {
            return Ok(false);
        };
        let threshold = model.config.root_compact_suggest_at.unwrap_or(0.75);
        let context = self.control.journal.lock().unwrap().contexts["root"];
        let key = format!("{}:{context}", model.id());
        let high = meter.used as f64 / window as f64 >= threshold;
        let fire = {
            let mut crossing = self.control.compact_suggestion.lock().unwrap();
            let previous = crossing
                .as_ref()
                .is_some_and(|(k, high)| *k == key && *high);
            *crossing = Some((key, high));
            high && !previous
        };
        if fire {
            self.control.commit("root", HistoryEvent::SystemReminder { text: format!("Root context is at {}% ({}/{window} tokens). You may ask the user to run /compact at a suitable stopping point. Only the user chooses to compact the root; continue the current work.", meter.used.saturating_mul(100) / window, meter.used) })?;
            self.control
                .emit(crate::schema::EngineEvent::RootCompactionSuggested {
                    used: meter.used,
                    window,
                    threshold,
                });
        }
        Ok(fire)
    }
}
