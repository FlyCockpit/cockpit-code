//! Private model branches. Only usage and a bounded outcome escape this module.
//! The supplied functions are the entire capability set; the ordinary dispatcher is never used.
use crate::{
    model::{Model, PreparedRequest},
    schema::{EngineError, EngineEvent, HiddenForkOutcome, HistoryEvent},
    script::{HostFunction, MontyRuntime, ScriptLimits, ScriptProgress, ScriptRuntime},
};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A function available only in the private script VM. The index identifies its copy.
#[derive(Clone)]
pub struct ForkFunction {
    pub name: String,
    pub description: String,
    pub call: Arc<dyn Fn(usize, Value) -> Result<Value, String> + Send + Sync>,
}
#[derive(Clone)]
pub struct HiddenFork {
    pub model: Model,
    pub author: PreparedRequest,
    /// Already canonicalized D52 notice, appended as one user-role message.
    pub tail: String,
    pub copies: usize,
    /// None leaves agent-like forks uncapped; mechanical title/variation forks use Some.
    pub max_steps: Option<usize>,
    pub functions: Vec<ForkFunction>,
    pub agent_id: String,
    pub purpose: String,
}
#[derive(Clone)]
pub(crate) enum ForkTask {
    Functions,
    Guidance {
        injected: Arc<std::sync::atomic::AtomicBool>,
    },
    AgentAsk {
        reader: Box<crate::read_only::ReadOnly>,
    },
    GoalReview {
        reader: Box<crate::read_only::ReadOnly>,
    },
    Candidate {
        focuses: Vec<String>,
        name: String,
        reader: Box<crate::read_only::ReadOnly>,
        proposals: Arc<Mutex<BTreeMap<usize, crate::schema::ToolCall>>>,
    },
    Evaluator {
        selection: Arc<Mutex<Option<Value>>>,
    },
}
impl HiddenFork {
    /// Construct the exact first request for inspection, then verify the frozen prefix.
    pub fn prepare(&self) -> Result<PreparedRequest, EngineError> {
        self.prepare_tail(&self.tail)
    }
    fn prepare_tail(&self, supplied_tail: &str) -> Result<PreparedRequest, EngineError> {
        if self.model.command_latest() {
            return Err(EngineError::bad("history: latest disables hidden forks"));
        }
        if !self.author.body.ends_with(b"]}") || !self.author.body.starts_with(&self.author.prefix)
        {
            return Err(EngineError::bad("hidden fork author request is malformed"));
        }
        let percent = self.model.config.context_window.map_or(0, |w| {
            self.author.input_estimate.saturating_mul(100) / w.max(1)
        });
        let tail = format!(
            "{}{}",
            supplied_tail,
            crate::pressure::tag(
                percent,
                self.agent_id != "root",
                self.model.config.compact_at.unwrap_or(0.8)
            )
        );
        let mut request = self.author.with_user_tail(&tail)?;
        request.agent_id = self.agent_id.clone();
        request.kind = self.purpose.clone();
        request.retry_limit = 0;
        self.author.verify_fork_prefix(&request)?;
        Ok(request)
    }
    /// A cold endpoint completes its first copy before the remaining copies fan out.
    /// The output channel receives Usage only; no text, reasoning or tool arguments escape.
    pub async fn run(
        &self,
        usage: &mpsc::UnboundedSender<EngineEvent>,
        cancel: &CancellationToken,
    ) -> Result<Vec<HiddenForkOutcome>, EngineError> {
        let scheduler = std::sync::Arc::new(crate::scheduler::Scheduler::default());
        let _slot = scheduler.short(&self.agent_id, &self.model, cancel).await?;
        self.run_for(usage, cancel, &ForkTask::Functions).await
    }
    pub(crate) async fn run_for(
        &self,
        usage: &mpsc::UnboundedSender<EngineEvent>,
        cancel: &CancellationToken,
        task: &ForkTask,
    ) -> Result<Vec<HiddenForkOutcome>, EngineError> {
        if self.copies == 0 || self.max_steps == Some(0) {
            return Err(EngineError::bad(
                "hidden fork requires positive copies and any configured step limit",
            ));
        }
        if !self.author.body.ends_with(b"]}") || !self.author.body.starts_with(&self.author.prefix)
        {
            return Err(EngineError::bad("hidden fork author request is malformed"));
        }
        if !self
            .author
            .offered_tools
            .iter()
            .any(|t| self.author.canonical_tool_name(&t.name) == "script")
        {
            return Err(EngineError::bad(
                "hidden fork requires the author's script tool",
            ));
        }
        let mut functions = BTreeMap::new();
        for function in &self.functions {
            if function.name.is_empty() || functions.insert(&function.name, function).is_some() {
                return Err(EngineError::bad(
                    "hidden fork function names must be unique",
                ));
            }
        }
        let mut results = Vec::with_capacity(self.copies);
        if self.model.provider.max_concurrent_contexts.is_some()
            || self.model.config.max_concurrent_contexts.is_some()
        {
            for index in 0..self.copies {
                results.push(self.copy(index, usage, cancel, task).await);
            }
            return Ok(results);
        }
        let start = if self.copies > 1
            && (self.model.provider.prompt_caching == code_config::PromptCaching::Off
                || !self.model.observed_cache_hit())
        {
            results.push(self.copy(0, usage, cancel, task).await);
            1
        } else {
            0
        };
        let mut copies = stream::iter(start..self.copies)
            .map(|index| self.copy(index, usage, cancel, task))
            .buffer_unordered(self.copies.max(1));
        while let Some(outcome) = copies.next().await {
            results.push(outcome);
        }
        results.sort_by_key(|o| o.copy);
        Ok(results)
    }
    async fn copy(
        &self,
        index: usize,
        usage: &mpsc::UnboundedSender<EngineEvent>,
        cancel: &CancellationToken,
        task: &ForkTask,
    ) -> HiddenForkOutcome {
        let mut outcome = HiddenForkOutcome {
            accepted_plain_text: false,
            purpose: self.purpose.clone(),
            copy: index,
            steps: 0,
            succeeded: false,
            reason: None,
            author_prefix_bytes: self.author.author_prefix().len(),
            author_prefix_sha256: format!("{:x}", Sha256::digest(self.author.author_prefix())),
            cache_hit_observed: self.model.observed_cache_hit(),
        };
        let result = self
            .run_copy(
                index,
                usage,
                cancel,
                &mut outcome.steps,
                &mut outcome.accepted_plain_text,
                task,
            )
            .await;
        outcome.cache_hit_observed = self.model.observed_cache_hit();
        match result {
            Ok(()) => outcome.succeeded = true,
            Err(error) => {
                outcome.reason = Some(if cancel.is_cancelled() {
                    "cancelled".into()
                } else {
                    error.message
                })
            }
        }
        outcome
    }
    async fn run_copy(
        &self,
        index: usize,
        usage: &mpsc::UnboundedSender<EngineEvent>,
        cancel: &CancellationToken,
        steps: &mut usize,
        accepted_plain_text: &mut bool,
        task: &ForkTask,
    ) -> Result<(), EngineError> {
        let mut local_task = task.clone();
        match &mut local_task {
            ForkTask::AgentAsk { reader }
            | ForkTask::GoalReview { reader }
            | ForkTask::Candidate { reader, .. } => {
                **reader = reader
                    .as_ref()
                    .clone()
                    .with_aliases(self.author.tool_aliases.clone())
                    .with_allowed(
                        self.author
                            .catalog()
                            .iter()
                            .map(|d| self.author.canonical_tool_name(&d.name).to_owned())
                            .collect(),
                    );
            }
            _ => {}
        }
        let task = &local_task;
        let mut request = if let ForkTask::Candidate { focuses, .. } = task {
            self.prepare_tail(&format!(
                "{}\n{}",
                self.tail,
                crate::injection::notice(format!(
                    "Variation focus: {}",
                    focuses
                        .get(index)
                        .map(String::as_str)
                        .unwrap_or("write an alternative")
                ))
            ))?
        } else {
            self.prepare()?
        };
        let mut called = false;
        loop {
            if *steps > 0
                && self.purpose != "auto_title"
                && self.purpose != "goal_review"
                && (*steps as u64)
                    .is_multiple_of(self.model.config.model_reminder_turns.unwrap_or(10).max(1))
                && let Some(text) = &self.model.config.model_reminder
            {
                request = request
                    .with_user_tail(&crate::injection::notice(format!("Model reminder: {text}")))?;
            }
            if self.max_steps.is_some_and(|limit| *steps >= limit) {
                return Err(EngineError::bad(
                    "hidden fork exhausted its model step limit",
                ));
            }
            if cancel.is_cancelled() {
                return Err(EngineError::cancelled());
            }
            self.author.verify_fork_prefix(&request)?;
            *steps += 1;
            let (private_tx, mut private_rx) = mpsc::unbounded_channel();
            let completion = {
                let pending = self.model.stream(&request, &private_tx, cancel);
                tokio::pin!(pending);
                loop {
                    tokio::select! {
                        result = &mut pending => break result,
                        event = private_rx.recv() => {
                            if let Some(event @ EngineEvent::Usage { .. }) = event {
                                let _ = usage.send(event);
                            }
                        }
                    }
                }
            };
            drop(private_tx);
            while let Some(event) = private_rx.recv().await {
                if matches!(event, EngineEvent::Usage { .. }) {
                    let _ = usage.send(event);
                }
            }
            let completion = completion.map_err(|error| {
                if cancel.is_cancelled() {
                    EngineError::cancelled()
                } else {
                    error
                }
            })?;
            if !completion.status.executable() {
                return Err(EngineError::bad(
                    "hidden fork received a non-executable completion",
                ));
            }
            if let ForkTask::Candidate {
                name, proposals, ..
            } = task
                && completion.tool_calls.len() == 1
                && self
                    .author
                    .canonical_tool_name(&completion.tool_calls[0].name)
                    == name
            {
                let mut call = completion.tool_calls[0].clone();
                crate::repair::repair(&mut call, &self.author.offered_tools)
                    .map_err(EngineError::bad)?;
                call.name = name.clone();
                proposals.lock().unwrap().insert(index, call);
                return Ok(());
            }
            if let ForkTask::Evaluator { selection } = task
                && completion.tool_calls.is_empty()
            {
                let text = completion
                    .text
                    .trim()
                    .trim_start_matches("```json")
                    .trim_start_matches("```")
                    .trim_end_matches("```")
                    .trim();
                let value = serde_json::from_str(text)
                    .map_err(|_| EngineError::bad("evaluator must return JSON {choice,reason}"))?;
                *selection.lock().unwrap() = Some(value);
                return Ok(());
            }
            // AgentAsk's result contract is text. Route a final prose answer through
            // the same validated sink as the supplied function, without loosening
            // structured/function-only fork contracts or executing any author tool.
            if completion.tool_calls.is_empty()
                && matches!(task, ForkTask::AgentAsk { .. })
                && !completion.text.trim().is_empty()
            {
                let report = self
                    .functions
                    .iter()
                    .find(|f| f.name == "report_agent_answer")
                    .ok_or_else(|| EngineError::bad("text-answer fork has no answer sink"))?;
                (report.call)(index, json!({"answer":completion.text.trim()}))
                    .map_err(EngineError::bad)?;
                *accepted_plain_text = true;
                return Ok(());
            }
            if completion.tool_calls.is_empty() && matches!(task, ForkTask::Guidance { .. }) {
                request=request.with_history_tail(&[HistoryEvent::Assistant{text:completion.text,reasoning:completion.reasoning,tool_calls:vec![],replay:completion.replay,status:completion.status,native_reason:completion.stop_reason},HistoryEvent::User{text:crate::injection::notice(format!("Finish through {} calling inject(ids=[...],reason='...'); use inject(ids=[],reason='no applicable items') when nothing applies. Prose does not complete selection.",self.author.offered_tools.iter().find(|t|self.author.canonical_tool_name(&t.name)=="script").map(|t|t.name.as_str()).unwrap_or("script"))),attachments:vec![]}])?;
                continue;
            }
            if completion.tool_calls.is_empty() {
                return if called {
                    Ok(())
                } else {
                    Err(EngineError::bad(
                        "hidden fork did not call a supplied function",
                    ))
                };
            }
            let mut tail = vec![HistoryEvent::Assistant {
                text: completion.text,
                reasoning: completion.reasoning,
                tool_calls: completion.tool_calls.clone(),
                replay: completion.replay,
                status: completion.status,
                native_reason: completion.stop_reason,
            }];
            let mut accepted = false;
            for mut call in completion.tool_calls {
                let wire_name = call.name.clone();
                call.name = self.author.canonical_tool_name(&call.name).to_owned();
                let allowed = match task {
                    ForkTask::AgentAsk { .. } => "Allowed actions: read, grep, glob, and script using those functions plus report_agent_answer(answer=...).".into(),
                    ForkTask::GoalReview { .. } => "Allowed actions: read, grep, glob, and script using those functions, goal_check(id=...), report_goal_review(passed=..., gaps=[...], nextStep=...).".into(),
                    ForkTask::Guidance { .. } => format!("Private guidance selector: the author write already committed. A refused fork call does not undo that write. Only {} using search_guidance, read_guidance and inject is allowed. List trusted items, then inject applicable IDs; do not replay the author task.",self.author.offered_tools.iter().find(|t|self.author.canonical_tool_name(&t.name)=="script").map(|t|t.name.as_str()).unwrap_or("script")),
                    ForkTask::Functions => format!(
                        "Allowed actions: script calling {}.",
                        self.functions
                            .iter()
                            .map(|f| f.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    ForkTask::Candidate { name, .. } => format!(
                        "Allowed actions: propose one {name} call; read, grep, glob, and script using read, grep, glob{}.",
                        if self.functions.is_empty() {
                            String::new()
                        } else {
                            format!(
                                ", {}",
                                self.functions
                                    .iter()
                                    .map(|f| f.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        }
                    ),
                    ForkTask::Evaluator { .. } => {
                        "Allowed actions: return plain JSON {choice,reason}; no tool calls.".into()
                    }
                };
                let mut proposed = false;
                let result = match task {
                    ForkTask::GoalReview { reader } | ForkTask::AgentAsk { reader }
                        if call.name == "script" && call.complete =>
                    {
                        reader
                            .fork_script(&call.arguments, index, &self.functions, cancel)
                            .await
                            .map(|(output, called)| {
                                proposed = called;
                                output
                            })
                    }
                    ForkTask::GoalReview { reader } | ForkTask::AgentAsk { reader } => {
                        reader.call(&call, cancel).await
                    }
                    ForkTask::Functions | ForkTask::Guidance { .. }
                        if call.name == "script" && call.complete =>
                    {
                        self.script(
                            index,
                            &call.arguments,
                            cancel,
                            matches!(task, ForkTask::Guidance { .. }),
                        )
                        .await
                        .map(|v| v.to_string())
                    }
                    ForkTask::Candidate { reader, .. }
                        if !self.functions.is_empty() && call.name == "script" && call.complete =>
                    {
                        reader
                            .fork_script(&call.arguments, index, &self.functions, cancel)
                            .await
                            .map(|(output, called)| {
                                proposed = called;
                                output
                            })
                    }
                    ForkTask::Candidate { reader, .. } => reader.call(&call, cancel).await,
                    _ => Err(EngineError::bad(&allowed)),
                };
                accepted |= result.is_ok()
                    && (matches!(task, ForkTask::Functions)
                        || matches!(task,ForkTask::Guidance{injected} if injected.load(std::sync::atomic::Ordering::Acquire))
                        || proposed);
                let is_error = result.is_err();
                let output = result.unwrap_or_else(|e| format!("Refused: {} {allowed}", e.message));
                tail.push(HistoryEvent::ToolResult {
                    details: Default::default(),
                    id: call.id,
                    name: wire_name,
                    output,
                    is_error,
                });
            }
            called |= accepted;
            // A successful supplied action is the branch's outcome; no acknowledgement step needed.
            if called {
                return Ok(());
            }
            if let Some(HistoryEvent::ToolResult { output, .. }) = tail.last_mut() {
                let percent = self
                    .model
                    .config
                    .context_window
                    .map_or(0, |w| request.input_estimate.saturating_mul(100) / w.max(1));
                output.push_str(&crate::pressure::tag(
                    percent,
                    self.agent_id != "root",
                    self.model.config.compact_at.unwrap_or(0.8),
                ));
            }
            request = request.with_history_tail(&tail)?;
        }
    }
    async fn script(
        &self,
        index: usize,
        args: &Value,
        cancel: &CancellationToken,
        capture: bool,
    ) -> Result<Value, EngineError> {
        if args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(EngineError::bad(
                "hidden fork scripts cannot run in the background",
            ));
        }
        let code = args["code"]
            .as_str()
            .ok_or_else(|| EngineError::bad("script requires code"))?
            .to_string();
        let functions = self.functions.clone();
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            let mut runtime = MontyRuntime::new(cancel.clone());
            runtime
                .load_host_functions(
                    functions
                        .iter()
                        .map(|f| HostFunction {
                            name: f.name.clone(),
                            description: f.description.clone(),
                        })
                        .collect(),
                )
                .map_err(|_| EngineError::bad("hidden fork script setup failed"))?;
            // Guidance search/read stdout is private and bounded; other purposes discard it.
            let mut output = ForkPrint {
                capture,
                bytes: Vec::new(),
            };
            let mut progress = runtime.run(
                &code,
                ScriptLimits {
                    cpu_secs: 1,
                    max_calls: 32,
                },
                &mut output,
            );
            let mut called = false;
            loop {
                if cancel.is_cancelled() {
                    return Err(EngineError::cancelled());
                }
                match progress {
                    Ok(ScriptProgress::Complete(value)) if called => {
                        return Ok(if capture {
                            json!({"stdout":String::from_utf8_lossy(&output.bytes),"value":value})
                        } else {
                            value
                        });
                    }
                    Ok(ScriptProgress::Complete(_)) => {
                        return Err(EngineError::bad(
                            "script did not invoke a supplied function",
                        ));
                    }
                    Ok(ScriptProgress::Call { tool, args }) => {
                        let result = functions
                            .iter()
                            .find(|f| f.name == tool)
                            .map(|function| (function.call)(index, args))
                            .unwrap_or_else(|| {
                                Err(format!(
                                    "Allowed actions: {}",
                                    functions
                                        .iter()
                                        .map(|f| f.name.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ))
                            });
                        called |= result.is_ok();
                        progress = runtime.resume(result, &mut output);
                    }
                    Err(_) => {
                        return Err(EngineError::bad("hidden fork script was refused or failed"));
                    }
                }
            }
        })
        .await
        .map_err(|_| EngineError::bad("hidden fork script worker failed"))?
    }
}

/// The title-only capability. Validation happens before a proposal reaches session state.
pub fn title_function(proposal: Arc<std::sync::Mutex<Option<String>>>) -> ForkFunction {
    ForkFunction {
        name: "rename_session".into(),
        description:
            "Set a readable session title, at most 60 characters. Use rename_session(title=...)."
                .into(),
        call: Arc::new(move |_, args| {
            let title = args["title"]
                .as_str()
                .ok_or("title must be a string")?
                .trim();
            if title.is_empty() || title.chars().count() > 60 || title.chars().any(char::is_control)
            {
                return Err("title must contain 1 to 60 readable characters".into());
            }
            *proposal.lock().unwrap() = Some(title.into());
            Ok(json!({"renamed":true}))
        }),
    }
}

struct ForkPrint {
    capture: bool,
    bytes: Vec<u8>,
}
impl std::io::Write for ForkPrint {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.capture {
            let take = bytes.len().min(8192usize.saturating_sub(self.bytes.len()));
            self.bytes.extend_from_slice(&bytes[..take]);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
