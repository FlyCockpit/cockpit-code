//! Paged history reads for frontends (D103): one agent context file at a
//! time, and the usage records of the whole tree without materializing
//! every event. Glue added by the tui-v10 lane.
use crate::Session;
use crate::schema::{EngineError, EventEnvelope, HistoryEvent, UsageRecord};
use std::io::{BufRead, BufReader};

impl Session {
    fn context_dir(&self, agent: &str) -> Option<std::path::PathBuf> {
        let agent = self.resolve_agent_id(agent).ok()?;
        let j = self.control.journal.lock().unwrap();
        if !j.is_persisted() || agent.contains(['/', '\\']) || agent.starts_with('.') {
            return None;
        }
        Some(j.dir.join("agents").join(agent).join("contexts"))
    }

    /// The context file numbers of an agent, ascending (empty when the
    /// session is not on disk yet).
    pub fn context_numbers(&self, agent: &str) -> Vec<u32> {
        let Some(dir) = self.context_dir(agent) else {
            return vec![];
        };
        let mut out: Vec<u32> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension()? == "jsonl")
                    .then(|| p.file_stem()?.to_str()?.parse().ok())
                    .flatten()
            })
            .collect();
        out.sort_unstable();
        out
    }

    /// One context file of an agent, canonicalized like a snapshot.
    pub fn read_context(
        &self,
        agent: &str,
        context: u32,
    ) -> Result<Vec<EventEnvelope>, EngineError> {
        let dir = self
            .context_dir(agent)
            .ok_or_else(|| EngineError::bad("no such context"))?;
        let path = dir.join(format!("{context:04}.jsonl"));
        let file = std::fs::File::open(&path).map_err(|e| EngineError::bad(e.to_string()))?;
        let mut out = vec![];
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| EngineError::bad(e.to_string()))?;
            let mut event: EventEnvelope = serde_json::from_str(&line).map_err(|_| {
                EngineError::bad(format!(
                    "invalid event at {}:{}",
                    path.display(),
                    out.len() + 1
                ))
            })?;
            crate::injection::canonicalize(&mut event.event, &event.id);
            out.push(event);
        }
        Ok(out)
    }

    /// Frontend transcript for one context, including partial reports received
    /// from direct children. Original envelopes retain their child provenance;
    /// raw history reads and model replay remain unchanged.
    pub fn read_transcript_context(
        &self,
        agent: &str,
        context: u32,
    ) -> Result<Vec<EventEnvelope>, EngineError> {
        let agent = self.resolve_agent_id(agent)?;
        let mut out = self.read_context(&agent, context)?;
        let start = out.first().map_or(0, |event| event.sequence);
        let end = self
            .context_numbers(&agent)
            .into_iter()
            .find(|number| *number > context)
            .map(|next| {
                let path = self
                    .context_dir(&agent)
                    .unwrap()
                    .join(format!("{next:04}.jsonl"));
                let file =
                    std::fs::File::open(path).map_err(|e| EngineError::bad(e.to_string()))?;
                let line = BufReader::new(file)
                    .lines()
                    .next()
                    .transpose()
                    .map_err(|e| EngineError::bad(e.to_string()))?;
                line.map(|line| {
                    serde_json::from_str::<EventEnvelope>(&line)
                        .map(|event| event.sequence)
                        .map_err(|_| EngineError::bad("invalid context boundary event"))
                })
                .transpose()
            })
            .transpose()?
            .flatten()
            .unwrap_or(u64::MAX);
        // A child's own report belongs to its parent's transcript, just as live.
        out.retain(|event| {
            !matches!(&event.event,
            HistoryEvent::AgentPartialReport { agent_id, .. } if agent_id == &agent)
        });
        let children = self
            .control
            .journal
            .lock()
            .unwrap()
            .agents
            .values()
            .filter(|child| child.parent_id.as_deref() == Some(agent.as_str()))
            .map(|child| child.agent_id.clone())
            .collect::<Vec<_>>();
        for child in children {
            let dir = self.context_dir(&child).unwrap();
            for number in self.context_numbers(&child) {
                let path = dir.join(format!("{number:04}.jsonl"));
                let file =
                    std::fs::File::open(&path).map_err(|e| EngineError::bad(e.to_string()))?;
                // Parse only report lines: do not materialize child transcripts.
                for (line_number, line) in BufReader::new(file).lines().enumerate() {
                    let line = line.map_err(|e| EngineError::bad(e.to_string()))?;
                    if !line.contains("\"agent_partial_report\"") {
                        continue;
                    }
                    let event: EventEnvelope = serde_json::from_str(&line).map_err(|_| {
                        EngineError::bad(format!(
                            "invalid event at {}:{}",
                            path.display(),
                            line_number + 1
                        ))
                    })?;
                    if event.sequence >= start
                        && event.sequence < end
                        && event.agent_id == child
                        && matches!(&event.event, HistoryEvent::AgentPartialReport { agent_id, .. } if agent_id == &child)
                    {
                        out.push(event);
                    }
                }
            }
        }
        out.sort_by_key(|event| event.sequence);
        Ok(out)
    }

    /// Every finished usage record of the tree (for per-model cost tables),
    /// parsing only the usage lines.
    pub fn usage_records(&self) -> Vec<UsageRecord> {
        let root = {
            let j = self.control.journal.lock().unwrap();
            if !j.is_persisted() {
                return vec![];
            }
            j.dir.join("agents")
        };
        let mut out = vec![];
        for agent in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let contexts = agent.path().join("contexts");
            for file in std::fs::read_dir(contexts).into_iter().flatten().flatten() {
                let Ok(f) = std::fs::File::open(file.path()) else {
                    continue;
                };
                for line in BufReader::new(f).lines().map_while(Result::ok) {
                    if !line.contains("\"kind\":\"usage\"") {
                        continue;
                    }
                    if let Ok(e) = serde_json::from_str::<EventEnvelope>(&line)
                        && let HistoryEvent::Usage { usage } = e.event
                        && !usage.partial
                    {
                        out.push(usage);
                    }
                }
            }
        }
        out
    }
}
