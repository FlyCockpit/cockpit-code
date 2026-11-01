//! `!cmd`: a local shell command run in the session's directory. Its row
//! shows in the transcript like a bash row, marked local. The output is
//! never sent to the model or written to the session; when it finishes the
//! viewed agent is told that it ran (D122: command, exit code, duration,
//! output size).
use super::App;
use crate::engine::LocalRun;
use crate::transcript::Kind;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;

/// Output kept per stream.
const LIMIT: usize = 256 * 1024;

pub struct Shell {
    pub id: String,
    /// Transcript and notice recipient, pinned when the command starts.
    pub agent: String,
    /// Row text, error flag, and the facts the agent is told.
    pub rx: mpsc::Receiver<(String, bool, LocalRun)>,
}

/// The kept text (capped) and the total byte count.
fn read_capped(mut r: impl Read) -> (String, u64) {
    let mut buf = Vec::new();
    let _ = r.by_ref().take(LIMIT as u64).read_to_end(&mut buf);
    // Drain the rest so the child never blocks on a full pipe.
    let rest = std::io::copy(&mut r, &mut std::io::sink()).unwrap_or(0);
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if buf.len() >= LIMIT {
        s.push_str("\n… output truncated");
    }
    (s, buf.len() as u64 + rest)
}

impl App {
    pub(super) fn run_shell(&mut self, cmd: String) {
        self.shell_seq += 1;
        let id = format!("local-shell-{}", self.shell_seq);
        let agent = self.view.clone().unwrap_or_else(|| "root".into());
        if let Some(transcript) = self.transcript_of(&agent) {
            transcript.tool_start(
                id.clone(),
                "shell".into(),
                serde_json::json!({ "command": cmd }),
            );
        }
        self.scroll = None;
        let (tx, rx) = mpsc::channel();
        let cwd = self.engine.cwd().to_path_buf();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
            let child = Command::new(shell)
                .arg("-c")
                .arg(&cmd)
                .current_dir(&cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();
            let (text, failed, code, bytes) = match child {
                Err(e) => (format!("Error: {e}"), true, None, 0),
                Ok(mut c) => {
                    let out = c.stdout.take().unwrap();
                    let err = c.stderr.take().unwrap();
                    let t = std::thread::spawn(move || read_capped(err));
                    let (stdout, n_out) = read_capped(out);
                    let (stderr, n_err) = t.join().unwrap_or_default();
                    let code = c.wait().ok().and_then(|s| s.code());
                    let shown = code.unwrap_or(-1);
                    // The bash tool's shape, so the row reads the same.
                    (
                        format!("exit: {shown}\nstdout:\n{stdout}\nstderr:\n{stderr}"),
                        shown != 0,
                        code,
                        n_out + n_err,
                    )
                }
            };
            let run = LocalRun {
                command: cmd,
                exit_code: code,
                duration_ms: started.elapsed().as_millis() as u64,
                output_bytes: bytes,
            };
            let _ = tx.send((text, failed, run));
        });
        self.shells.push(Shell { id, agent, rx });
    }

    /// Finished local commands: true when any row changed.
    pub(super) fn poll_shells(&mut self) -> bool {
        let mut done = vec![];
        for (i, s) in self.shells.iter().enumerate() {
            if let Ok((out, err, run)) = s.rx.try_recv() {
                done.push((i, s.id.clone(), s.agent.clone(), out, err, run));
            }
        }
        let changed = !done.is_empty();
        for (i, id, agent, out, err, run) in done.into_iter().rev() {
            self.shells.remove(i);
            self.post_local_run(&agent, run);
            if let Some(transcript) = self.transcript_of(&agent) {
                transcript.tool_end(&id, out, err, Default::default());
                // A finished row is open, like a terminal.
                if let Some(e) = transcript
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|e| matches!(&e.kind, Kind::Tool(t) if t.id == id))
                {
                    e.expanded = true;
                }
            }
            self.git.poke();
        }
        changed
    }
}
