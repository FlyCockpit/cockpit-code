//! Paged history reads for frontends: one agent context file at a
//! time, and the usage records of the whole tree without materializing
//! every event.
use crate::Session;
use crate::schema::{EngineError, EventEnvelope, HistoryEvent, UsageRecord};
use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader},
};

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

    /// One context file of an agent, preserving the recorded notice bytes.
    pub fn read_context(
        &self,
        agent: &str,
        context: u32,
    ) -> Result<Vec<EventEnvelope>, EngineError> {
        let dir = self
            .context_dir(agent)
            .ok_or_else(|| EngineError::bad("no such context"))?;
        let path = dir.join(format!("{context:04}.jsonl"));
        crate::sessions::context_events(&path)?.collect()
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
                crate::sessions::context_events(&path)?
                    .next()
                    .transpose()
                    .map(|event| event.map(|event| event.sequence))
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
                // Parse only report lines: do not materialize child transcripts.
                for event in crate::sessions::context_events_containing(
                    &path,
                    Some(b"\"agent_partial_report\""),
                )? {
                    let event = event?;
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
        let mut calls = BTreeSet::new();
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
                        && calls.insert(usage.call_id.clone())
                    {
                        out.push(usage);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn append_bytes(path: &std::path::Path, bytes: &[u8]) {
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    #[tokio::test]
    async fn usage_records_deduplicate_retained_calls_across_contexts() {
        let (temp, store) = crate::test_support::store(serde_json::json!({}));
        let session = Session::create(store, temp.path().to_owned(), Default::default()).unwrap();
        let usage: UsageRecord = serde_json::from_value(serde_json::json!({
            "callId":"call-1", "sessionId":"", "agentId":"root", "provider":"p",
            "endpoint":"", "attempt":0, "model":"m", "kind":"agent", "tokens":{},
            "costUsd":null, "partial":false
        }))
        .unwrap();
        let event = HistoryEvent::Usage {
            usage: usage.clone(),
        };
        session.control.commit("root", event.clone()).unwrap();
        let mut second = usage.clone();
        second.call_id = "call-2".into();
        let mut partial = usage;
        partial.call_id = "partial".into();
        partial.partial = true;
        session
            .control
            .journal
            .lock()
            .unwrap()
            .rotate(
                "root",
                vec![
                    event,
                    HistoryEvent::Usage { usage: second },
                    HistoryEvent::Usage { usage: partial },
                ],
            )
            .unwrap();
        let records = session.usage_records();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records
                .iter()
                .map(|u| u.call_id.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["call-1", "call-2"])
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn paging_recovers_only_invalid_unterminated_final_records() {
        for tail in [
            b"{broken".as_slice(),
            b"{\"text\":\"\xf0\x9f",
            b"{broken\n",
            b"{broken\n{}\n",
        ] {
            let (temp, store) = crate::test_support::store(serde_json::json!({}));
            let session =
                Session::create(store, temp.path().to_owned(), Default::default()).unwrap();
            session
                .control
                .commit(
                    "root",
                    HistoryEvent::SystemReminder {
                        text: "kept".into(),
                    },
                )
                .unwrap();
            let before = session.read_context("root", 1).unwrap();
            let path = session.context_dir("root").unwrap().join("0001.jsonl");
            append_bytes(&path, tail);
            let expected_ok = !tail.ends_with(b"\n");
            for result in [
                session.read_context("root", 1),
                session.read_transcript_context("root", 1),
            ] {
                assert_eq!(result.is_ok(), expected_ok, "{tail:?}");
                if let Ok(events) = result {
                    assert_eq!(events.len(), before.len());
                    assert_eq!(events.last().unwrap().id, before.last().unwrap().id);
                }
            }
            session.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn paging_keeps_valid_final_records_without_a_newline() {
        let (temp, store) = crate::test_support::store(serde_json::json!({}));
        let session = Session::create(store, temp.path().to_owned(), Default::default()).unwrap();
        let event = session
            .control
            .commit(
                "root",
                HistoryEvent::SystemReminder {
                    text: "kept".into(),
                },
            )
            .unwrap();
        let path = session.context_dir("root").unwrap().join("0001.jsonl");
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(
            session.read_context("root", 1).unwrap().last().unwrap().id,
            event.id
        );
        assert_eq!(
            session
                .read_transcript_context("root", 1)
                .unwrap()
                .last()
                .unwrap()
                .id,
            event.id
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn transcript_boundary_ignores_an_entirely_torn_next_context() {
        for tail in [b"{\"text\":\"\xf0\x9f".as_slice(), b"broken\n"] {
            let (temp, store) = crate::test_support::store(serde_json::json!({}));
            let session =
                Session::create(store, temp.path().to_owned(), Default::default()).unwrap();
            session
                .control
                .commit(
                    "root",
                    HistoryEvent::SystemReminder {
                        text: "kept".into(),
                    },
                )
                .unwrap();
            let next = session.context_dir("root").unwrap().join("0002.jsonl");
            std::fs::write(next, tail).unwrap();
            assert_eq!(
                session.read_transcript_context("root", 1).is_ok(),
                !tail.ends_with(b"\n")
            );
            session.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn child_reports_share_torn_tail_recovery_and_strict_middle_records() {
        // (tail, fails): a torn report tail is ignored; a complete line that
        // is not a report is skipped unparsed (child transcripts are not
        // materialized); a corrupt complete report line is an error.
        for (tail, fails) in [
            (b"{\"agent_partial_report\":\"\xf0\x9f".as_slice(), false),
            (b"broken\n".as_slice(), false),
            (b"{\"agent_partial_report\" broken\n".as_slice(), true),
        ] {
            let (temp, store) = crate::test_support::store(serde_json::json!({}));
            let session =
                Session::create(store, temp.path().to_owned(), Default::default()).unwrap();
            session
                .control
                .commit(
                    "root",
                    HistoryEvent::SystemReminder {
                        text: "start".into(),
                    },
                )
                .unwrap();
            {
                let mut journal = session.control.journal.lock().unwrap();
                let mut child = journal.agents["root"].clone();
                child.agent_id = "child".into();
                child.parent_id = Some("root".into());
                journal.register(child).unwrap();
            }
            let report = session
                .control
                .commit(
                    "child",
                    HistoryEvent::AgentPartialReport {
                        agent_id: "child".into(),
                        text: "report".into(),
                    },
                )
                .unwrap();
            let path = session.context_dir("child").unwrap().join("0001.jsonl");
            append_bytes(&path, tail);
            let result = session.read_transcript_context("root", 1);
            if fails {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap().iter().any(|event| event.id == report.id));
            }
            session.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn paged_context_preserves_current_notice_ids() {
        let (temp, store) = crate::test_support::store(serde_json::json!({}));
        let session = Session::create(store, temp.path().to_owned(), Default::default()).unwrap();
        let envelope = session
            .control
            .commit(
                "root",
                HistoryEvent::SystemReminder {
                    text: "notice".into(),
                },
            )
            .unwrap();
        let rows = session.read_context("root", 1).unwrap();
        let actual = rows.iter().find(|e| e.id == envelope.id).unwrap();
        assert_eq!(
            serde_json::to_value(&actual.event).unwrap(),
            serde_json::to_value(envelope.event).unwrap()
        );
        session.shutdown().await.unwrap();
    }
}
