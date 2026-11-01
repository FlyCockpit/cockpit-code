//! User invocation is explicit authority; model discovery honors user-only skills.
use crate::{
    Session, State,
    context::{FrozenContext, Skill},
    model::EventSender,
    schema::{EngineError, EngineEvent, ToolCall},
    tools::CommitBarrier,
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
impl Session {
    /// Full user catalog, including user-only skills and shadowing metadata.
    pub fn skills(&self) -> Result<Vec<Skill>, EngineError> {
        let (frozen, cwd) = {
            let j = self.control.journal.lock().unwrap();
            (
                j.histories.get("root").and_then(|h| {
                    h.iter().find_map(|e| {
                        if let crate::schema::HistoryEvent::PromptContext { context } = e {
                            Some(context.skills.clone())
                        } else {
                            None
                        }
                    })
                }),
                j.header.cwd.clone(),
            )
        };
        if let Some(skills) = frozen {
            return Ok(skills);
        }
        let store = code_config::ConfigStore::load(self.directories.clone())
            .map_err(|e| EngineError::bad(e.to_string()))?;
        Ok(FrozenContext::capture(&store, &cwd, crate::BUILD_PROMPT.into(), None)?.skills)
    }
    /// Build the tagged user message; queued frontends may pass this to queue_message.
    pub fn skill_message(&self, name: &str, args: &str) -> Result<String, EngineError> {
        let name = name.strip_prefix("skill:").unwrap_or(name);
        let skill = self
            .skills()?
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| EngineError::bad(format!("skill not found: {name}")))?;
        let raw =
            std::fs::read_to_string(&skill.path).map_err(|e| EngineError::bad(e.to_string()))?;
        if raw.len() > 1024 * 1024 {
            return Err(EngineError::bad("skill exceeds 1 MiB"));
        }
        let body = skill_body(&raw);
        let id = uuid::Uuid::new_v4().simple().to_string();
        let escaped = name
            .replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        Ok(format!(
            "<skill id=\"{id}\" name=\"{escaped}\">\n{body}\n\nArguments: {args}\n</skill id=\"{id}\">"
        ))
    }
    pub fn invoke_skill(
        &self,
        name: &str,
        args: impl AsRef<str>,
    ) -> Result<mpsc::UnboundedReceiver<EngineEvent>, EngineError> {
        Ok(self.send(self.skill_message(name, args.as_ref())?))
    }
}
fn skill_body(raw: &str) -> &str {
    if let Some(front) = raw.strip_prefix("---\r\n")
        && let Some((_, body)) = front.split_once("\r\n---")
    {
        return body.trim_start_matches(['\r', '\n']);
    }
    if let Some(front) = raw.strip_prefix("---\n")
        && let Some((_, body)) = front.split_once("\n---")
    {
        return body.trim_start_matches(['\r', '\n']);
    }
    raw
}
pub(crate) async fn read(
    state: &State,
    args: &serde_json::Value,
    cancel: &CancellationToken,
    barrier: Arc<CommitBarrier>,
    tx: &EventSender,
) -> Result<String, EngineError> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| EngineError::bad("skill name required"))?;
    let skill = state
        .context_state
        .frozen
        .as_ref()
        .and_then(|c| c.skills.iter().find(|s| s.name == name))
        .ok_or_else(|| EngineError::bad("skill not found"))?;
    model_allowed(skill)?;
    if skill.disable_model_invocation {
        return Err(EngineError::bad(format!(
            "Skill {name} is user-only (disable-model-invocation); ask the user to invoke it."
        )));
    }
    let output = crate::execute(
        state,
        ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: "read".into(),
            arguments: serde_json::json!({"path":skill.path,"limit":10000}),
            complete: true,
        },
        cancel.clone(),
        barrier,
        tx,
    )
    .await;
    if output.is_error {
        return Err(EngineError::bad(output.output));
    }
    Ok(output.output)
}

pub(crate) fn model_allowed(skill: &Skill) -> Result<(), EngineError> {
    if std::fs::metadata(&skill.path)
        .map_err(|e| EngineError::bad(e.to_string()))?
        .len()
        > 1024 * 1024
    {
        return Err(EngineError::bad("skill exceeds 1 MiB"));
    }
    let body = std::fs::read_to_string(&skill.path).map_err(|e| EngineError::bad(e.to_string()))?;
    let (_, _, current) = crate::context::skill_metadata(&body, &skill.name)?;
    if skill.disable_model_invocation || current {
        return Err(EngineError::bad(format!(
            "Skill {} is user-only (disable-model-invocation); ask the user to invoke it.",
            skill.name
        )));
    }
    Ok(())
}
