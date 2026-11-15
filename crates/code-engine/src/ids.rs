//! One canonical resolver for tool, Monty, and frontend tree identifiers.
use crate::{
    Control, Session,
    schema::{EngineError, ToolCall},
    sessions::Journal,
};

pub(crate) fn short(id: &str) -> String {
    if let Some((role, uuid)) = id.split_once('-')
        && uuid::Uuid::parse_str(uuid).is_ok()
    {
        return format!("{role}-{}", &uuid[..8]);
    }
    id.into()
}
impl Journal {
    pub(crate) fn resolve_agent_id(&self, input: &str) -> Result<String, EngineError> {
        if self.agents.contains_key(input) {
            return Ok(input.into());
        }
        let ids = self
            .agents
            .keys()
            .filter(|id| {
                short(id) == input || id.split_once('-').is_some_and(|(_, uuid)| uuid == input)
            })
            .collect::<Vec<_>>();
        match ids.as_slice() {
            [id] => Ok((*id).clone()),
            [] => Err(EngineError::bad(format!("agent not found: {input}"))),
            _ => Err(EngineError::bad(format!(
                "ambiguous agent id: {input}; use the full UUID"
            ))),
        }
    }
    pub(crate) fn resolve_job_id(&self, input: &str) -> Result<String, EngineError> {
        if self.jobs.contains_key(input) || self.questions.contains_key(input) {
            return Ok(input.into());
        }
        if let Ok(agent) = self.resolve_agent_id(input) {
            return self
                .jobs
                .values()
                .filter(|job| job.agent_id.as_deref() == Some(&agent))
                .max_by_key(|job| (&job.started, &job.id))
                .map(|j| j.id.clone())
                .ok_or_else(|| EngineError::bad("agent has no job; use agents show"));
        }
        let prefix = input.strip_prefix("job-").unwrap_or(input);
        let ids = self
            .jobs
            .keys()
            .filter(|id| prefix.len() >= 8 && id.starts_with(prefix))
            .collect::<Vec<_>>();
        match ids.as_slice() {
            [id] => Ok((*id).clone()),
            [] => Err(EngineError::bad(format!("job not found: {input}"))),
            _ => Err(EngineError::bad("ambiguous job id; use the full UUID")),
        }
    }
}
impl Control {
    pub(crate) fn normalize_ids(&self, call: &mut ToolCall) -> Result<(), EngineError> {
        let fields: &[&str] = match call.name.as_str() {
            "message" => &["to"],
            "task" => &["continue"],
            "criteria" => &["agentId", "scope"],
            "scope" | "decisions" => &["agent"],
            "agents" => &["id", "subtree"],
            _ => &[],
        };
        let j = self.journal.lock().unwrap();
        for field in fields {
            if let Some(id) = call.arguments[*field].as_str()
                && ![
                    "self",
                    "parent",
                    "children",
                    "descendants",
                    "subtree",
                    "ancestors",
                    "tree",
                    "mine",
                ]
                .contains(&id)
            {
                call.arguments[*field] = j.resolve_agent_id(id)?.into();
            }
        }
        if call.name == "jobs"
            && let Some(id) = call.arguments["jobId"].as_str()
        {
            call.arguments["jobId"] = j.resolve_job_id(id)?.into();
        }
        Ok(())
    }
}
impl Session {
    pub fn resolve_agent_id(&self, id: &str) -> Result<String, EngineError> {
        self.control.journal.lock().unwrap().resolve_agent_id(id)
    }
    pub fn resolve_job_id(&self, id: &str) -> Result<String, EngineError> {
        self.control.journal.lock().unwrap().resolve_job_id(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_agent_ids_resolve_by_full_id_short_id_and_uuid() {
        let (temp, store) = crate::test_support::store(serde_json::json!({}));
        let session = Session::create(store, temp.path().to_owned(), Default::default()).unwrap();
        {
            let mut j = session.control.journal.lock().unwrap();
            let mut agent = j.agents["root"].clone();
            let uuid = "12345678-1234-4234-8234-123456789abc";
            let id = format!("build-{uuid}");
            agent.agent_id = id.clone();
            agent.parent_id = Some("root".into());
            j.register(agent).unwrap();
            for input in [id.as_str(), "build-12345678", uuid] {
                assert_eq!(j.resolve_agent_id(input).unwrap(), id);
            }
            assert_eq!(j.resolve_agent_id("root").unwrap(), "root");
        }
        session.shutdown().await.unwrap();
    }
}
