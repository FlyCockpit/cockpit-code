//! Manual frontend harness for the terminal API; no private bytes are printed.
//! Build: cargo build -p code-engine --example terminal_console
//! Run with MODEL (`provider/model`) as argv[1], optional `no-terminal` or
//! `unattended` as argv[2] to exercise sessions without the terminal tool.
//!
//! stdin accepts one JSON object per line, tagged by `op`:
//! `{"op":"send","prompt":…}`, `{"op":"input","id":…,"chars":…}`,
//! `{"op":"resize","id":…,"cols":…,"rows":…}`,
//! `{"op":"share","id":…,"share":"status"|"output"}`,
//! `{"op":"selection","id":…,"output":…|null}`,
//! `{"op":"close","id":…,"note":…|null}`, `{"op":"snapshot","id":…}`,
//! `{"op":"job_output","id":…,"offset":…,"limit":…}`,
//! `{"op":"mode","unattended":bool}`, `{"op":"cancel"}`, `{"op":"status"}`,
//! `{"op":"shutdown"}`. Each command answers `command_ok` or `command_error`;
//! an unparsable line ends the run. stdout carries JSON lines with event
//! metadata and terminal byte counts only. Pipe stdin so the parent tty does
//! not echo input.
use code_config::{ConfigStore, Directories, PermissionMode};
use code_engine::{Session, SessionOptions, schema::EngineEvent, terminals::ShareMode};
use serde::Deserialize;
use serde_json::json;
use std::{io::BufRead, path::PathBuf};

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Command {
    Send { prompt: String },
    Input { id: String, chars: String },
    Resize { id: String, cols: u16, rows: u16 },
    Share { id: String, share: ShareMode },
    Selection { id: String, output: Option<String> },
    Close { id: String, note: Option<String> },
    Snapshot { id: String },
    JobOutput { id: String, offset: u64, limit: u64 },
    Mode { unattended: bool },
    Cancel,
    Status,
    Shutdown,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let store = ConfigStore::load(Directories::resolve()?)?;
    let session = Session::create(
        store,
        std::env::current_dir()?,
        SessionOptions {
            model: args.get(1).cloned(),
            ..Default::default()
        },
    )?;
    let unavailable = args.get(2).map(String::as_str);
    session.set_mode(if unavailable == Some("unattended") {
        PermissionMode::Unattended
    } else {
        PermissionMode::Yolo
    })?;
    if unavailable != Some("no-terminal") {
        session.enable_user_terminals()?;
    }
    let mut events = session.subscribe();
    let mut bytes = session.subscribe_terminals();
    let (tx, mut commands) = tokio::sync::mpsc::channel(16);
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            // A parse failure is deliberately not echoed; lines may contain input.
            let parsed = serde_json::from_str::<Command>(&line).ok();
            if tx.blocking_send(parsed).is_err() {
                break;
            }
        }
    });
    let mut core_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let core_disabled = unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut core_limit) } == 0
        && core_limit.rlim_cur == 0
        && core_limit.rlim_max == 0;
    println!(
        "{}",
        json!({"type":"ready","sessionId":session.id(),"cwd":PathBuf::from("."),"coreDumpsDisabled":core_disabled})
    );
    loop {
        tokio::select! {
            command=commands.recv() => {
                let Some(Some(command))=command else { println!("{}",json!({"type":"command_error","message":"invalid command or EOF"})); break; };
                let result=match command {
                    Command::Send{prompt} => { let mut rx=session.send(prompt); tokio::spawn(async move {while rx.recv().await.is_some() {}}); Ok(()) },
                    Command::Input{id,chars} => session.terminal_input(&id,&chars).await,
                    Command::Resize{id,cols,rows} => session.terminal_resize(&id,cols,rows).await,
                    Command::Share{id,share} => session.terminal_set_share(&id,share),
                    Command::Selection{id,output} => session.terminal_share_output(&id,output),
                    Command::Close{id,note} => session.terminal_close(&id,note).await,
                    Command::Snapshot{id} => session.terminal_snapshot(&id).map(|snapshot| {
                        println!("{}",json!({"type":"snapshot","terminalId":snapshot.terminal.id,"bytes":snapshot.bytes.len(),"offset":snapshot.offset,"truncated":snapshot.truncated,"debug":format!("{snapshot:?}"),"controlStrings":snapshot.bytes.windows(2).any(|bytes| bytes == b"\x1b]" || bytes == b"\x1bP")}));
                    }),
                    Command::JobOutput{id,offset,limit} => session.terminal_job_output(&id,offset,limit).map(|output| println!("{}",json!({"type":"job_output","page":output}))),
                    Command::Mode{unattended} => session.set_mode(if unattended {PermissionMode::Unattended} else {PermissionMode::Yolo}),
                    Command::Cancel => {session.cancel();Ok(())},
                    Command::Status => {println!("{}",json!({"type":"status","active":session.is_active(),"terminals":session.terminals()}));Ok(())},
                    Command::Shutdown => break,
                };
                match result { Ok(()) => println!("{}",json!({"type":"command_ok"})), Err(error) => println!("{}",json!({"type":"command_error","message":error.message})) }
            },
            Some(event)=events.recv() => match event {
                EngineEvent::TerminalChanged{..}|EngineEvent::ToolEnd{..}|EngineEvent::TurnEnd{..}|EngineEvent::AgentStarted{..}|EngineEvent::AgentFinished{..}|EngineEvent::Error{..}|EngineEvent::AgentMessage{..} => println!("{}",serde_json::to_string(&event)?),
                EngineEvent::AgentEvent{agent_id,event,..} => if matches!(*event,EngineEvent::ToolEnd{..}|EngineEvent::Error{..}) {println!("{}",json!({"type":"child_event","agentId":agent_id,"event":event}));},
                _=>{},
            },
            event=bytes.recv() => match event {
                Ok(code_engine::terminals::TerminalEvent::Output{terminal_id,offset,bytes}) => println!("{}",json!({"type":"terminal_bytes","terminalId":terminal_id,"offset":offset,"bytes":bytes.len()})),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => println!("{}",json!({"type":"terminal_lagged","count":count})),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    session.shutdown().await?;
    println!("{}", json!({"type":"closed"}));
    Ok(())
}
