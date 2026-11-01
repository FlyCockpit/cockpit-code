mod init;
use clap::{Parser, Subcommand};
use code_config::{ConfigStore, Directories, PermissionMode, ProviderType, ThinkingMode};
use code_engine::{
    Session, SessionOptions,
    model::Model,
    schema::{ApprovalDecision, EngineEvent, HistoryEvent},
    sessions::{self, ForkPoint},
};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
};
#[derive(Parser)]
#[command(
    version,
    about = "A terminal coding agent: interactive TUI by default, headless runs with -p, and an ACP server"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,
    /// Run headless: send PROMPT, stream the reply and exit (bare -p sends no text).
    #[arg(short = 'p', long, value_name = "PROMPT", num_args=0..=1, default_missing_value="")]
    prompt: Option<String>,
    /// With -p, run the prompt as an autonomous goal; the exit code reports the outcome.
    #[arg(long, requires = "prompt", conflicts_with = "attachments")]
    goal: bool,
    /// Attach a file to the -p prompt (repeatable; relative paths use the session cwd).
    #[arg(long = "attach", value_name = "FILE", requires = "prompt")]
    attachments: Vec<PathBuf>,
    /// Model to use, as provider/model (overrides the configured default).
    #[arg(long, global = true, value_name = "PROVIDER/MODEL")]
    model: Option<String>,
    #[arg(long, global = true, help = "Use one-layer subagents; fixed after the first message")]
    flat: bool,
    /// Reasoning effort level for the model.
    #[arg(long, global = true, value_name = "LEVEL")]
    effort: Option<String>,
    /// Working directory (default: the current directory).
    #[arg(long, global = true, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// Print JSON instead of text (-p, sessions, guidance, compact, prune, init).
    #[arg(long, global = true)]
    json: bool,
    /// Resume a saved session by id or unique id prefix.
    #[arg(long, value_name = "ID", conflicts_with_all=["continue_session","fork"])]
    resume: Option<String>,
    /// Resume the most recent session for the working directory.
    #[arg(long = "continue", conflicts_with = "fork")]
    continue_session: bool,
    /// Start a new session forked from a session id or prefix, optionally at ID@AGENT:CONTEXT:LINE.
    #[arg(long, value_name = "ID")]
    fork: Option<String>,
    /// Take over a resumed session's lock even if another process holds it.
    #[arg(long)]
    claim_lock: bool,
    /// Permission mode for this session.
    #[arg(long, global = true,value_parser=["manual","auto","unattended","yolo"])]
    mode: Option<String>,
    #[arg(long, help = "Compact before sending -p (or use compact <id>)")]
    compact: bool,
    /// What the --compact summary should focus on.
    #[arg(long, value_name = "TEXT")]
    focus: Option<String>,
}
#[derive(Subcommand)]
enum Commands {
    /// Scaffold config, provider templates, tool defaults and the config skill.
    Init(init::InitArgs),
    /// List command-provider models, vendor efforts and common-scale mappings.
    Discover {
        /// Only this provider (default: every command provider).
        provider: Option<String>,
        /// Re-run discovery instead of using cached results.
        #[arg(long)]
        refresh: bool,
    },
    /// Test or discover a single provider.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// Show config paths, providers, models, MCP servers, LSP detection and config warnings.
    Doctor {
        /// Send a tiny request to PROVIDER as each API type and report which ones it accepts.
        #[arg(long, value_name = "PROVIDER")]
        probe: Option<String>,
    },
    /// List saved sessions for the working directory.
    Sessions {
        /// List sessions from every directory.
        #[arg(long)]
        all: bool,
    },
    /// Serve the Agent Client Protocol (v1) on stdio, for editors such as Zed.
    Acp,
    /// View journaled guidance selections and reasons without inference.
    Guidance {
        /// Session id or unique id prefix.
        id: String,
    },
    /// Prune old model-visible history (deliberately breaks the prompt cache).
    Prune {
        /// Session id or unique id prefix.
        id: String,
        /// Take over the session's lock even if another process holds it.
        #[arg(long)]
        claim_lock: bool,
    },
    /// Compact a saved session's history into a summary.
    Compact {
        /// Session id or unique id prefix.
        id: String,
        /// What the summary should focus on.
        #[arg(long, value_name = "TEXT")]
        focus: Option<String>,
        /// Take over the session's lock even if another process holds it.
        #[arg(long)]
        claim_lock: bool,
    },
}
#[derive(Subcommand)]
enum ProviderCommand {
    /// Run discovery and one minimal inference call on a command provider; prints a JSON report.
    Test {
        /// Provider id.
        provider: String,
    },
    /// List one command provider's models (same as `discover <provider>`).
    Discover {
        /// Provider id.
        provider: String,
        /// Re-run discovery instead of using cached results.
        #[arg(long)]
        refresh: bool,
    },
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            if let Some(exit) = e.downcast_ref::<GoalExit>() {
                return std::process::ExitCode::from(exit.0);
            }
            eprintln!("cockpit-code: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = Args::parse();
    if args.goal && args.prompt.as_deref().is_some_and(|p| p.trim().is_empty()) {
        return Err("--goal needs the goal text: cockpit-code -p \"<goal>\" --goal".into());
    }
    if let Some(Commands::Init(options)) = &args.command {
        if args.mode.is_some() || args.model.is_some() {
            return Err("init writes defaults: use --default-mode and --default-model".into());
        }
        return init::run(
            options,
            args.json,
            &args.cwd.clone().unwrap_or(std::env::current_dir()?),
        );
    }
    install_bundled_skills();
    if let Some(Commands::Acp) = &args.command {
        return Ok(code_acp::run(code_acp::Options {
            model: args.model,
            effort: args.effort,
            mode: args.mode,
            flat: args.flat,
        })
        .await?);
    }
    let mut store = ConfigStore::load(Directories::resolve()?)?;
    match &args.command {
        Some(Commands::Discover { provider, refresh }) => {
            discover_providers(&mut store, provider.as_deref(), *refresh)?;
            return Ok(());
        }
        Some(Commands::Provider {
            command: ProviderCommand::Discover { provider, refresh },
        }) => {
            discover_providers(&mut store, Some(provider), *refresh)?;
            return Ok(());
        }
        Some(Commands::Provider {
            command: ProviderCommand::Test { provider },
        }) => {
            provider_test(
                &mut store,
                provider,
                args.cwd.clone().unwrap_or(std::env::current_dir()?),
            )
            .await?;
            return Ok(());
        }
        _ => {}
    }
    // Session ids may be given as a unique prefix (the TUI shows 8 chars).
    let data = store.directories.data.clone();
    args.resume = args
        .resume
        .map(|id| resolve_session(&data, &id))
        .transpose()?;
    args.fork = args
        .fork
        .map(|f| match f.split_once('@') {
            Some((id, at)) => resolve_session(&data, id).map(|id| format!("{id}@{at}")),
            None => resolve_session(&data, &f),
        })
        .transpose()?;
    if let Some(
        Commands::Compact { id, .. } | Commands::Prune { id, .. } | Commands::Guidance { id },
    ) = &mut args.command
    {
        *id = resolve_session(&data, id)?;
    }
    let _usage = code_engine::usage::Cache::start(&store);
    let cwd = args.cwd.clone().unwrap_or(std::env::current_dir()?);
    if let Some(Commands::Doctor { probe }) = &args.command {
        let detection = code_engine::lsp_defaults::resolve(&store.snapshot().config.lsp, &cwd);
        for (id, server) in &detection.servers {
            println!("lsp {id}: detected {}", server.command);
        }
        for hint in &detection.hints {
            println!("lsp install hint: {hint}");
        }
        if detection.scan_truncated {
            println!("lsp: language scan capped at 100000 entries");
        }
        println!(
            "CONFIG={}\nDATA={}\nCACHE={}",
            store.directories.config.display(),
            store.directories.data.display(),
            store.directories.cache.display()
        );
        for (id, p) in &store.snapshot().providers {
            println!("provider {id} type={:?} enabled={}", p.kind, p.enabled);
            for (model, c) in &p.models {
                println!("  model {id}/{model} type={:?}", c.kind.unwrap_or(p.kind));
                for warning in
                    code_engine::tool_names::warnings(store.snapshot(), &format!("{id}/{model}"))
                {
                    println!("  warning: {warning}");
                }
                if c.autocompact_at_idle
                    .zip(c.cache_ttl_secs)
                    .is_some_and(|(idle, ttl)| idle > ttl)
                {
                    println!(
                        "  warning: {id}/{model} autocompactAtIdle exceeds cacheTtlSecs; the cache may be cold"
                    );
                }
            }
        }
        for id in store.snapshot().mcps.keys() {
            println!("mcp {id} (parsed)");
        }
        for e in &store.snapshot().errors {
            println!("skipped {e}");
        }
        if let Some(provider) = probe {
            probe_provider(&store, provider).await?;
        }
        return Ok(());
    }
    if let Some(Commands::Sessions { all }) = args.command {
        let listing = sessions::list_with_problems_in(
            &store.directories.data,
            if all { None } else { Some(&cwd) },
        )?;
        for problem in listing.problems {
            eprintln!("{}: {}", problem.path.display(), problem.error);
        }
        let rows = listing.sessions;
        if args.json {
            println!("{}", serde_json::to_string(&rows)?);
        } else {
            for row in rows {
                println!(
                    "{}  {}  {}  {}{}",
                    row.header.id,
                    row.updated_at,
                    row.header.cwd.display(),
                    row.header.title,
                    if row.lock.holder.is_some() {
                        " [locked]"
                    } else {
                        ""
                    }
                );
            }
        }
        return Ok(());
    }
    let options = SessionOptions {
        model: args.model,
        effort: args.effort,
    };
    if let Some(Commands::Guidance { id }) = &args.command {
        let session = Session::resume(store, id, options)?;
        if args.json {
            println!(
                "{}",
                serde_json::to_string(&session.guidance_selections(None))?
            );
        } else {
            println!("{}", session.guidance_report());
        }
        session.shutdown().await?;
        return Ok(());
    }
    if let Some(Commands::Prune { id, claim_lock }) = &args.command {
        let session = if *claim_lock {
            Session::claim(store, id, options)?
        } else {
            Session::resume(store, id, options)?
        };
        let preview = session.prune_preview()?;
        if args.json {
            println!(
                "{}",
                serde_json::json!({"type":"prune_preview","report":preview})
            );
        } else {
            eprintln!("{}", preview.confirmation);
        }
        let result = consume(&session, session.prune(), args.json).await;
        shutdown(&session, args.json).await?;
        return result;
    }
    if let Some(Commands::Compact {
        id,
        focus,
        claim_lock,
    }) = args.command
    {
        let session = if claim_lock {
            Session::claim(store, &id, options)?
        } else {
            Session::resume(store, &id, options)?
        };
        if let Some(mode) = args.mode {
            session.set_mode(mode_value(&mode))?;
        }
        eprintln!("Session: {}", session.id());
        let result = consume(&session, session.compact(focus), args.json).await;
        shutdown(&session, args.json).await?;
        return result;
    }
    let Some(prompt) = args.prompt else {
        #[cfg(feature = "tui")]
        return Ok(code_tui::run(code_tui::Options {
            model: options.model,
            effort: options.effort,
            cwd,
            resume: args.resume,
            continue_session: args.continue_session,
            fork: args.fork,
            claim_lock: args.claim_lock,
            mode: args.mode,
            flat: args.flat,
        })
        .await?);
        #[cfg(not(feature = "tui"))]
        return Err("this binary was built without the tui feature; use -p or acp".into());
    };
    let resume = if args.continue_session {
        Some(
            sessions::list_in(&store.directories.data, Some(&cwd))?
                .first()
                .ok_or("no sessions for this cwd")?
                .header
                .id
                .clone(),
        )
    } else {
        args.resume
    };
    if args.claim_lock && resume.is_none() {
        return Err("--claim-lock requires --resume <id> or --continue".into());
    }
    let session = if let Some(fork) = args.fork {
        let (id, point) = parse_fork(&fork)?;
        Session::fork(store, &id, point, options)?
    } else if let Some(id) = resume {
        if args.claim_lock {
            Session::claim(store, &id, options)?
        } else {
            Session::resume(store, &id, options)?
        }
    } else {
        Session::create(store, cwd.clone(), options)?
    };
    if args.flat {
        session.set_subagents(code_engine::SubagentMode::Flat)?;
    }
    if let Some(mode) = args.mode {
        session.set_mode(mode_value(&mode))?;
    }
    eprintln!("Session: {}", session.id());
    let cancel_session = session.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_session.cancel();
        }
    });
    let result = async {
        if args.compact {
            consume(&session, session.compact(args.focus), args.json).await?;
        }
        if args.goal {
            consume(&session,session.goal_start(prompt)?,args.json).await?;
            let drafted=session.goal_status().ok_or("goal drafting failed")?;
            if drafted.phase!="draft" {
                let exit=drafted.exit_code();
                if args.json {println!("{}",serde_json::json!({"type":"goal_result","contract":drafted.contract,"goal":drafted,"exitCode":exit}));}
                return Err(Box::new(GoalExit(exit)) as Box<dyn std::error::Error>);
            }
            let events = session.subscribe();
            let initial = session.goal_approve(drafted.contract)?;
            consume(&session,settling_events(&session, events, initial),args.json).await?;
            let state=session.goal_status().ok_or("goal state missing")?;
            let exit=state.exit_code();
            if args.json {println!("{}",serde_json::json!({"type":"goal_result","contract":state.contract,"goal":state,"exitCode":exit}));}
            if exit!=0 {return Err(Box::new(GoalExit(exit)) as Box<dyn std::error::Error>);}
            return Ok(());
        }
        let attachments = args
            .attachments
            .iter()
            .map(|path| {
                session.attach_file(if path.is_absolute() {
                    path.clone()
                } else {
                    session.cwd().join(path)
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let events = session.subscribe();
        session.queue_message_with_attachments("root", prompt, attachments, code_config::QueueClass::NextTurn)?;
        let (initial_tx, initial) = tokio::sync::mpsc::unbounded_channel();
        drop(initial_tx);
        let rx = settling_events(&session, events, initial);
        consume(
            &session,
            rx,
            args.json,
        )
        .await
    }
    .await;
    signal.abort();
    if args.json
        && result
            .as_ref()
            .err()
            .is_some_and(|e| e.downcast_ref::<GoalExit>().is_none())
        && let Some(state) = session.goal_status()
    {
        println!(
            "{}",
            serde_json::json!({"type":"goal_result","contract":state.contract,"goal":state,"exitCode":1})
        );
    }
    shutdown(&session, args.json).await?;
    eprintln!("{}", session.totals().summary());
    result
}
fn settling_events(
    session: &Session,
    mut events: tokio::sync::mpsc::UnboundedReceiver<EngineEvent>,
    mut initial: tokio::sync::mpsc::UnboundedReceiver<EngineEvent>,
) -> tokio::sync::mpsc::UnboundedReceiver<EngineEvent> {
    tokio::spawn(async move { while initial.recv().await.is_some() {} });
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let settling = session.clone();
    tokio::spawn(async move {
        let idle = settling.wait_until_idle();
        tokio::pin!(idle);
        loop {
            tokio::select! {
                biased;
                Some(event) = events.recv() => { if tx.send(event).is_err() { break; } },
                _ = &mut idle => break,
            }
        }
    });
    rx
}
async fn shutdown(session: &Session, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut events = session.subscribe();
    session.shutdown().await?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    while let Ok(event) = events.try_recv() {
        let _ = tx.send(event);
    }
    drop(tx);
    let result = consume(session, rx, json).await;
    let records = session.open_decisions();
    if !records.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::json!({"type":"open_decisions","openCount":records.len(),"records":records})
            );
        } else {
            eprintln!("Open decisions ({}):", records.len());
            for record in records {
                eprintln!(
                    "- {} [{}] {} → {} ({})",
                    record.id, record.deciding_agent, record.question, record.choice, record.reason
                );
            }
        }
    }
    result
}
fn mode_value(s: &str) -> PermissionMode {
    match s {
        "manual" => PermissionMode::Manual,
        "auto" => PermissionMode::Auto,
        "unattended" => PermissionMode::Unattended,
        _ => PermissionMode::Yolo,
    }
}
/// A full session id from an exact id or a unique prefix of one.
fn resolve_session(data: &std::path::Path, id: &str) -> Result<String, Box<dyn std::error::Error>> {
    if data.join("sessions").join(id).is_dir() {
        return Ok(id.to_string());
    }
    let matches: Vec<String> = sessions::list_in(data, None)?
        .into_iter()
        .map(|s| s.header.id)
        .filter(|s| s.starts_with(id))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => {
            Err(format!("no session matches {id} (cockpit-code sessions --all lists them)").into())
        }
        many => Err(format!("{id} matches {} sessions; give more of the id", many.len()).into()),
    }
}

fn parse_fork(s: &str) -> Result<(String, Option<ForkPoint>), Box<dyn std::error::Error>> {
    let Some((id, p)) = s.split_once('@') else {
        return Ok((s.into(), None));
    };
    let parts: Vec<_> = p.split(':').collect();
    if parts.len() != 3 {
        return Err("fork point must be <session>@<agent>:<context>:<line>".into());
    }
    Ok((
        id.into(),
        Some(ForkPoint {
            session_id: id.into(),
            agent_id: parts[0].into(),
            context: parts[1].parse()?,
            line: parts[2].parse()?,
        }),
    ))
}
async fn tty_line(prompt: String) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        let mut tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        write!(tty, "{prompt}").ok()?;
        tty.flush().ok()?;
        let mut line = String::new();
        if io::BufReader::new(tty).read_line(&mut line).ok()? == 0 {
            return None;
        }
        Some(line.trim().into())
    })
    .await
    .ok()
    .flatten()
}
async fn tty_question(
    spec: &code_engine::schema::QuestionSpec,
) -> code_engine::schema::QuestionAnswer {
    use code_engine::schema::{QuestionAnswer, QuestionKind};
    let mut prompt = format!("\n{}\n", spec.text);
    for (i, option) in spec.options.iter().enumerate() {
        prompt.push_str(&format!(
            "{}. {}{}\n",
            i + 1,
            option.label,
            option
                .description
                .as_ref()
                .map(|d| format!(" — {d}"))
                .unwrap_or_default()
        ));
    }
    prompt.push_str(match spec.kind {
        QuestionKind::Choice if spec.multi_select => {
            "Choose labels/numbers separated by commas, or type Other text. /skip skips: "
        }
        QuestionKind::Choice => "Choose a label/number, or type Other text. /skip skips: ",
        QuestionKind::Boolean => "yes/no, or Other text. /skip skips: ",
        QuestionKind::Text => "Answer (/skip skips): ",
    });
    let mut answer = QuestionAnswer {
        id: spec.id.clone(),
        ..Default::default()
    };
    let Some(text) = tty_line(prompt).await else {
        answer.text = Some("user unavailable".into());
        return answer;
    };
    if text == "/skip" || text.is_empty() {
        answer.skipped = true;
        return answer;
    }
    match spec.kind {
        QuestionKind::Text => answer.text = Some(text),
        QuestionKind::Boolean => match text.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" => answer.boolean = Some(true),
            "n" | "no" | "false" => answer.boolean = Some(false),
            _ => answer.other = Some(text),
        },
        QuestionKind::Choice => {
            let labels = text
                .split(',')
                .map(|part| {
                    let part = part.trim();
                    part.parse::<usize>()
                        .ok()
                        .and_then(|i| i.checked_sub(1))
                        .and_then(|i| spec.options.get(i))
                        .or_else(|| spec.options.iter().find(|o| o.label == part))
                        .map(|o| o.label.clone())
                })
                .collect::<Option<Vec<_>>>();
            if let Some(labels) = labels.filter(|labels| spec.multi_select || labels.len() == 1) {
                for label in labels {
                    if !answer.labels.contains(&label) {
                        answer.labels.push(label);
                    }
                }
            } else {
                answer.other = Some(text);
            }
        }
    }
    answer
}
async fn consume(
    session: &Session,
    mut events: tokio::sync::mpsc::UnboundedReceiver<EngineEvent>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut failed = false;
    let mut seen = false;
    while let Some(event) = events.recv().await {
        if json {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            let mut value = match &event {
                EngineEvent::AgentEvent {
                    agent_id,
                    breadcrumb,
                    event,
                } => {
                    let mut value = serde_json::to_value(event)?;
                    value["agentId"] = serde_json::json!(agent_id);
                    value["breadcrumb"] = serde_json::json!(breadcrumb);
                    value
                }
                _ => serde_json::to_value(&event)?,
            };
            if value.get("agentId").is_none() {
                let agent = match &event {
                    EngineEvent::Committed { envelope } => envelope.agent_id.as_str(),
                    EngineEvent::Usage { usage } => usage.agent_id.as_str(),
                    EngineEvent::Question { question } => question.asker.as_str(),
                    EngineEvent::ApprovalRequest { request } => request.agent_id.as_str(),
                    EngineEvent::AgentStarted { agent, .. } => agent.agent_id.as_str(),
                    EngineEvent::Job { job } => job.owner.as_str(),
                    EngineEvent::AgentMessage { to, .. } => to.as_str(),
                    _ => "root",
                };
                value["agentId"] = serde_json::json!(agent);
            }
            serde_json::to_writer(&mut out, &value)?;
            writeln!(out)?;
            out.flush()?;
        }
        match event {
            EngineEvent::AgentStarted {
                agent, breadcrumb, ..
            } if !json => eprintln!(
                "[{breadcrumb}] started {}",
                agent.model.as_deref().unwrap_or("")
            ),
            EngineEvent::AgentStatus {
                status, breadcrumb, ..
            } if !json => eprintln!("[{breadcrumb}] {status}"),
            EngineEvent::AgentFinished {
                status, breadcrumb, ..
            } if !json => eprintln!("[{breadcrumb}] finished {status}"),
            EngineEvent::AgentMessage { from, to, text, .. } if !json && to == "root" => eprintln!(
                "[{from} → root] {}",
                text.chars().take(240).collect::<String>()
            ),
            EngineEvent::Job { job } if !json => eprintln!(
                "[{}] job {} {} ({})",
                job.owner, job.id, job.status, job.kind
            ),
            EngineEvent::AgentEvent {
                breadcrumb, event, ..
            } if !json => {
                if let EngineEvent::ToolStart { call } = *event {
                    eprintln!("[{breadcrumb}] {}", call.name);
                }
            }
            EngineEvent::Text { delta } if !json => {
                print!("{delta}");
                io::stdout().flush()?;
                seen = true;
            }
            EngineEvent::Error { error } => {
                failed = true;
                if !json {
                    eprintln!("error: {error}");
                }
            }
            EngineEvent::SessionClaimed { message, .. } => {
                failed = true;
                eprintln!("{message}");
            }
            EngineEvent::ReviewerGuidanceTrustRequest { request } => {
                eprintln!(
                    "warning: ignoring untrusted project {} guidance {} in headless mode",
                    if request.library {
                        "library"
                    } else {
                        "reviewer"
                    },
                    request.path.display()
                );
                session.resolve_reviewer_guidance(&request.id, false)?;
            }
            EngineEvent::ApprovalRequest { request } => {
                let prompt = format!(
                    "\n{}: {} {}\n{}\n{}\nAllow [o]nce, [s]ession, or deny with feedback: ",
                    request.breadcrumb,
                    request.tool,
                    request
                        .target
                        .as_deref()
                        .or(request.command.as_deref())
                        .unwrap_or(""),
                    request.reason,
                    format_args!(
                        "{}\nSession grant: {}",
                        request.diff.as_deref().unwrap_or(""),
                        request.grant_key.as_deref().unwrap_or("once only")
                    )
                );
                let decision = match tty_line(prompt).await {
                    Some(s) if s == "o" || s == "once" => ApprovalDecision::Once,
                    Some(s) if s == "s" || s == "session" => ApprovalDecision::Session,
                    Some(feedback) => ApprovalDecision::Deny { feedback },
                    None => ApprovalDecision::Deny {
                        feedback: "user unavailable: no controlling /dev/tty".into(),
                    },
                };
                session.respond_approval(&request.id, decision)?;
            }
            EngineEvent::ProposalRequest { agent_id, proposal } => {
                session.resolve_proposal(&agent_id,&proposal.id,"reject",Some("Headless owner unavailable; preserve the approved criterion or raise a roadblock.".into()))?;
            }
            EngineEvent::Question { question }
                if question.recipient.is_none()
                    && session.question_answer(&question.id).is_none() =>
            {
                if question.questions.is_empty() {
                    let answer = tty_line(format!(
                        "\n{} asks: {}\nAnswer: ",
                        question.breadcrumb, question.text
                    ))
                    .await
                    .unwrap_or_else(|| "user unavailable".into());
                    if session.question_answer(&question.id).is_none() {
                        session.answer(&question.id, answer)?;
                    }
                } else {
                    let mut answers = Vec::new();
                    for spec in &question.questions {
                        answers.push(tty_question(spec).await);
                    }
                    if session.question_answer(&question.id).is_none() {
                        session.answer_questions(&question.id, answers)?;
                    }
                }
            }
            EngineEvent::Pruned { report } if !json => eprintln!(
                "Saved about {} tokens ({} prompts, {} outputs).",
                report.tokens_saved, report.delegations, report.tool_outputs
            ),
            EngineEvent::Reconnecting { .. } if !json => eprintln!("reconnecting…"),
            EngineEvent::ModelSwitched { from, to, reason } if !json => {
                eprintln!("model fallback: {from} → {to} ({reason})")
            }
            EngineEvent::Retry {
                attempt, delay_ms, ..
            } if !json => {
                eprintln!("retry {attempt} in {delay_ms}ms")
            }
            EngineEvent::StreamReset { .. } if !json && seen => {
                println!("\n[partial response discarded; restarting]")
            }
            EngineEvent::TurnEnd { stop_reason }
                if ["cancelled", "error", "incomplete", "max_tokens"]
                    .contains(&stop_reason.as_str()) =>
            {
                failed = true
            }
            _ => {}
        }
    }
    if !json && seen {
        println!();
    }
    if failed {
        Err("turn failed".into())
    } else {
        Ok(())
    }
}
async fn probe_provider(store: &ConfigStore, id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let provider = store
        .snapshot()
        .providers
        .get(id)
        .ok_or("provider not found")?;
    let (model, config) = provider
        .models
        .iter()
        .next()
        .ok_or("provider has no models to probe")?;
    for kind in [
        ProviderType::Anthropic,
        ProviderType::OpenaiChat,
        ProviderType::OpenaiResponses,
    ] {
        let mut p = provider.clone();
        p.kind = kind;
        p.timeouts.first_token_secs = 30;
        p.timeouts.idle_secs = 30;
        let mut c = config.clone();
        c.kind = Some(kind);
        c.max_output_tokens = Some(16);
        c.thinking = Some(ThinkingMode::None);
        c.reasoning.default = None;
        c.reasoning.efforts.clear();
        c.capabilities.tools = false;
        let result = async {
            let m = Model::new(id.into(), model.clone(), p, c, None)?;
            let mut request = m.prepare(
                "Reply briefly.",
                &[],
                &[HistoryEvent::User {
                    text: "Say OK".into(),
                    attachments: vec![],
                }],
                "doctor-probe",
            )?;
            request.retry_limit = 0;
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            m.stream(&request, &tx, &tokio_util_token()).await
        }
        .await;
        match result {
            Ok(_) => println!(
                "probe {kind:?}: accepted; suggested type=\"{}\" (no files changed)",
                serde_json::to_value(kind)?.as_str().unwrap()
            ),
            Err(e) => println!("probe {kind:?}: {}", e.message),
        }
    }
    Ok(())
}
fn tokio_util_token() -> tokio_util::sync::CancellationToken {
    tokio_util::sync::CancellationToken::new()
}

fn discover_providers(
    store: &mut ConfigStore,
    provider: Option<&str>,
    refresh: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(id) = provider
        && !store.snapshot().providers.contains_key(id)
    {
        return Err(format!("unknown provider {id}").into());
    }
    let ids: Vec<_> = store
        .snapshot()
        .providers
        .iter()
        .filter(|(id, p)| {
            provider.is_none_or(|wanted| wanted == id.as_str()) && p.command.is_some()
        })
        .map(|(id, _)| id.clone())
        .collect();
    store.refresh_discovery_many(if refresh { ids.clone() } else { vec![] })?;
    for id in ids {
        let p = &store.snapshot().providers[&id];
        println!(
            "{}",
            serde_json::json!({"provider":id,"protocol":code_config::discovery::PROTOCOL,"models":p.models,"error":store.snapshot().discovery_errors.get(&id)})
        );
    }
    for warning in &store.snapshot().errors {
        eprintln!("{warning}");
    }
    Ok(())
}
async fn provider_test(
    store: &mut ConfigStore,
    id: &str,
    cwd: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    store.refresh_discovery(id)?;
    let p = store
        .snapshot()
        .providers
        .get(id)
        .ok_or("unknown provider")?
        .clone();
    if p.kind != ProviderType::Command {
        return Err("provider test requires type: command".into());
    }
    let (mid, config) = p.models.iter().next().ok_or("provider has no models")?;
    let model = Model::new(id.into(), mid.clone(), p.clone(), config.clone(), None)?;
    let mut request = model.prepare(
        "Reply briefly.",
        &[],
        &[HistoryEvent::User {
            text: "Reply with OK.".into(),
            attachments: vec![],
        }],
        &format!("provider-test-{}", std::process::id()),
    )?;
    request.retry_limit = 0;
    request.command_context.cwd = cwd;
    request.command_context.permission_mode = "manual".into();
    request.command_context.agent = "build".into();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = model
        .stream(&request, &tx, &tokio_util::sync::CancellationToken::new())
        .await;
    drop(tx);
    let mut usages = Vec::new();
    let mut warnings = Vec::new();
    let mut text_deltas = 0;
    let mut reasoning_deltas = 0;
    while let Some(event) = rx.recv().await {
        match event {
            EngineEvent::Usage { usage } => usages.push(usage),
            EngineEvent::Error { error } => warnings.push(error.message),
            EngineEvent::Text { .. } => text_deltas += 1,
            EngineEvent::Reasoning { .. } => reasoning_deltas += 1,
            _ => {}
        }
    }
    match result {
        Ok(c) => {
            println!(
                "{}",
                serde_json::json!({"provider":id,"model":model.id(),"observed":{"textBytes":c.text.len(),"textDeltas":text_deltas,"reasoningDeltas":reasoning_deltas,"toolCalls":c.tool_calls.len(),"stopReason":c.stop_reason,"status":c.status},"usage":usages,"protocolWarnings":warnings,"configWarnings":store.snapshot().errors,"scope":"discovery and one minimal inference call; tools/cancel require live verification"})
            );
            Ok(())
        }
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"provider":id,"error":error,"usage":usages,"protocolWarnings":warnings})
            );
            Err("provider conformance call failed".into())
        }
    }
}
/// First run: install the bundled config skill into `~/.agents/skills/` (§3).
/// Failures are reported but never block startup.
fn install_bundled_skills() {
    let (Some(home), Ok(dirs)) = (std::env::var_os("HOME"), Directories::resolve()) else {
        return;
    };
    if dirs.config.join("tools.json").exists()
        && let Err(e) = code_engine::tool_defaults::install(&dirs.config, &dirs.data, false)
    {
        eprintln!("cockpit-code: could not refresh tool defaults: {e}");
    }
    match code_engine::bundled_skills::install(std::path::Path::new(&home), &dirs.data) {
        Ok(created) => {
            for dir in created {
                eprintln!("cockpit-code: installed bundled skill {}", dir.display());
            }
        }
        Err(e) => eprintln!("cockpit-code: could not install bundled skills: {e}"),
    }
}

#[derive(Debug)]
struct GoalExit(u8);
impl std::fmt::Display for GoalExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "goal stopped (exit {})", self.0)
    }
}
impl std::error::Error for GoalExit {}
