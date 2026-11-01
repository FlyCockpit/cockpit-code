//! D122 `!cmd`: the user ran a shell command locally in the frontend. The
//! agent learns that it happened (command, exit code, duration, output
//! size), never the output itself, at its next model request.
use crate::{Session, schema::*};

pub(crate) fn notice_text(
    command: &str,
    exit_code: Option<i32>,
    duration_ms: u64,
    output_bytes: u64,
) -> String {
    let command: String = if command.chars().count() > 2000 {
        command.chars().take(2000).collect::<String>() + "…"
    } else {
        command.to_string()
    };
    let exit = exit_code.map_or_else(
        || "none (failed to start or killed)".into(),
        |c| c.to_string(),
    );
    format!(
        "The user ran a local shell command in their terminal (not one of your tool calls):\n$ {command}\nexit code: {exit} · duration: {:.1}s · output: {output_bytes} bytes\nThe output was not shared with you. If you need it, ask the user to paste it.",
        duration_ms as f64 / 1000.0
    )
}

impl Session {
    /// D122: record that the user ran `command` locally for `agent` (the
    /// viewed agent). Durable: the notice survives resume and is delivered
    /// once before the agent's next model request. It never starts a turn
    /// and never carries the command's output.
    pub fn post_local_command(
        &self,
        agent: &str,
        command: &str,
        exit_code: Option<i32>,
        duration_ms: u64,
        output_bytes: u64,
    ) -> Result<(), EngineError> {
        self.control.check_owner()?;
        let agent = self.resolve_agent_id(agent)?;
        let agent = agent.as_str();
        if !self
            .control
            .journal
            .lock()
            .unwrap()
            .agents
            .contains_key(agent)
        {
            return Err(EngineError::bad("agent not found"));
        }
        self.control.commit(
            agent,
            HistoryEvent::LocalCommand {
                command: command.into(),
                exit_code,
                duration_ms,
                output_bytes,
            },
        )?;
        Ok(())
    }
}

impl crate::State {
    /// Deliver pending local command notices as system reminders.
    pub(crate) fn drain_local_commands(&self) -> Result<(), EngineError> {
        let pending = self
            .control
            .journal
            .lock()
            .unwrap()
            .local_commands
            .get(&self.agent_id)
            .cloned()
            .unwrap_or_default();
        for text in pending {
            self.control
                .commit(&self.agent_id, HistoryEvent::SystemReminder { text })?;
        }
        Ok(())
    }
}
