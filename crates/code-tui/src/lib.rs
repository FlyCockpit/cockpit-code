//! Ratatui front end for cockpit-code.
mod agents;
mod agentsview;
mod app;
mod attach;
mod clipboard;
mod composer;
mod criteria;
mod decisionsview;
mod dialog;
mod diff;
mod diffview;
mod engine;
mod forkchat;
mod git;
mod highlight;
mod links;
mod list;
mod markdown;
mod pager;
mod paste;
mod picker;
mod questions;
mod rows;
mod termview;
mod theme;
mod transcript;
mod ui;
mod usage;
mod vim;
mod wrap;

use futures_util::StreamExt;
use ratatui::crossterm::{
    cursor::SetCursorStyle,
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, EventStream, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use std::{io, path::PathBuf, time::Duration};

/// Startup options; the session flags match headless `-p`.
pub struct Options {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: PathBuf,
    pub resume: Option<String>,
    pub continue_session: bool,
    pub fork: Option<String>,
    pub claim_lock: bool,
    pub mode: Option<String>,
    pub flat: bool,
}

fn restore() {
    let _ = execute!(
        io::stdout(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableFocusChange,
        DisableMouseCapture,
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
}

/// Take the terminal back after the external editor (the inverse of
/// [`restore`]).
fn reclaim(mouse: bool) -> io::Result<()> {
    // Drop input the editor left unread (answers to its terminal queries),
    // so it is not parsed as keys.
    #[cfg(unix)]
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableFocusChange
    )?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
    }
    if supports_keyboard_enhancement().unwrap_or(false) {
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    Ok(())
}

/// Run `$VISUAL` (else `$EDITOR`) on `text` with the TUI suspended: input
/// reading stopped, mouse capture, bracketed paste, keyboard flags, the
/// alternate screen and raw mode all released, then restored. Ok(None)
/// when the editor exited unsuccessfully (the text is left unchanged).
async fn external_edit(mouse: bool, text: String) -> Result<Option<String>, String> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|e| !e.trim().is_empty()))
        .ok_or("set $VISUAL or $EDITOR to edit in an external editor")?;
    let path = std::env::temp_dir().join(format!("cockpit-code-{}.md", paste::fresh_id("")));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    opts.open(&path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| format!("editor: temp file: {e}"))?;
    restore();
    let p = path.clone();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(&p)
            .status()
    })
    .await;
    let reclaimed = reclaim(mouse);
    let result = match status {
        Ok(Ok(s)) if s.success() => std::fs::read_to_string(&path)
            .map(|t| Some(t.strip_suffix('\n').unwrap_or(&t).to_string()))
            .map_err(|e| format!("editor: reading the temp file: {e}")),
        Ok(Ok(s)) => {
            let _ = s;
            Ok(None)
        }
        Ok(Err(e)) => Err(format!("editor: {e}")),
        Err(e) => Err(format!("editor: {e}")),
    };
    let _ = std::fs::remove_file(&path);
    reclaimed.map_err(|e| format!("terminal restore: {e}"))?;
    result
}

/// Keystrokes typed while the session opens: the terminal stops echoing
/// and line-buffering them (signals still work, so Ctrl+C still quits),
/// and they are collected for the composer once the UI is up.
#[cfg(unix)]
struct EarlyInput(Option<libc::termios>);

#[cfg(unix)]
impl EarlyInput {
    fn begin() -> Self {
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) == 0 {
                return Self(None);
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return Self(None);
            }
            let mut t = saved;
            t.c_lflag &= !(libc::ICANON | libc::ECHO);
            t.c_cc[libc::VMIN] = 0;
            t.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
            Self(Some(saved))
        }
    }
    /// Read what was typed, then give the terminal back as it was (raw mode
    /// saves and restores the original settings).
    fn end(self) -> Vec<u8> {
        let mut out = vec![];
        if let Some(saved) = self.0 {
            let mut chunk = [0u8; 256];
            loop {
                let n = unsafe {
                    libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len())
                };
                if n <= 0 {
                    break;
                }
                out.extend_from_slice(&chunk[..n as usize]);
            }
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved);
            }
        }
        out
    }
}

#[cfg(not(unix))]
struct EarlyInput;

#[cfg(not(unix))]
impl EarlyInput {
    fn begin() -> Self {
        EarlyInput
    }
    fn end(self) -> Vec<u8> {
        vec![]
    }
}

/// Run the TUI until the user quits.
pub async fn run(options: Options) -> Result<(), String> {
    let early = EarlyInput::begin();
    let started = engine::Engine::start(engine::Launch {
        model: options.model,
        effort: options.effort,
        cwd: options.cwd,
        resume: options.resume,
        continue_session: options.continue_session,
        fork: options.fork,
        claim_lock: options.claim_lock,
        mode: options.mode,
        flat: options.flat,
    });
    let mut typed = early.end();
    let (engine, events, issue) = started?;
    let catalog = engine.catalog();
    let theme_setting = catalog.theme;
    enable_raw_mode().map_err(|e| e.to_string())?;
    let (dark, more) = theme::detect_dark(&theme_setting);
    typed.extend(more);
    theme::init(dark);
    let mut stdout = io::stdout();
    let setup = execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableFocusChange
    );
    let setup = setup.and_then(|()| {
        if catalog.mouse {
            execute!(stdout, EnableMouseCapture)
        } else {
            Ok(())
        }
    });
    if let Err(e) = setup {
        restore();
        return Err(e.to_string());
    }
    if supports_keyboard_enhancement().unwrap_or(false) {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        hook(info);
    }));
    let mut app = app::App::new(engine, events, issue);
    let result = event_loop(&mut app, &typed).await;
    restore();
    let hint = app.resume_hint();
    app.shutdown().await;
    if result.is_ok()
        && let Some(id) = hint
    {
        println!("resume this session with: cockpit-code --resume {id}");
    }
    result
}

async fn event_loop(app: &mut app::App, typed: &[u8]) -> Result<(), String> {
    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = ratatui::Terminal::new(backend).map_err(|e| e.to_string())?;
    let mut input = Some(EventStream::new());
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut block_cursor = false;
    let mut title = String::new();
    let mut early = Some(typed);
    loop {
        if let Some(req) = app.editor.take() {
            // The editor owns the terminal: stop reading input first.
            drop(input.take());
            let result = external_edit(app.mouse, req.text.clone()).await;
            input = Some(EventStream::new());
            let _ = terminal.clear();
            block_cursor = false;
            app.editor_done(req, result);
        }
        let want_title = app.window_title();
        if want_title != title {
            title = want_title;
            let _ = execute!(io::stdout(), SetTitle(&title));
        }
        if app.mouse_changed {
            app.mouse_changed = false;
            let _ = if app.mouse {
                execute!(io::stdout(), EnableMouseCapture)
            } else {
                execute!(io::stdout(), DisableMouseCapture)
            };
        }
        if app.dirty {
            app.dirty = false;
            terminal.draw(|f| app.draw(f)).map_err(|e| e.to_string())?;
            links::emit(&app.links);
            if app.block_cursor() != block_cursor {
                block_cursor = app.block_cursor();
                let _ = execute!(
                    io::stdout(),
                    if block_cursor {
                        SetCursorStyle::SteadyBlock
                    } else {
                        SetCursorStyle::DefaultUserShape
                    }
                );
            }
        }
        if app.quit {
            break;
        }
        if let Some(typed) = early.take() {
            app.early_input(typed);
            continue;
        }
        let (engine_rx, term_rx) = app.channels();
        tokio::select! {
            ev = async {
                match input.as_mut() {
                    Some(i) => i.next().await,
                    None => std::future::pending().await,
                }
            } => match ev {
                Some(Ok(ev)) => app.on_term(ev),
                Some(Err(e)) => return Err(e.to_string()),
                None => break,
            },
            ev = app::recv(engine_rx) => {
                app.on_engine(ev);
                // Coalesce bursts of stream deltas into one frame.
                while let Some(rx) = app.events() {
                    match rx.try_recv() {
                        Ok(ev) => app.on_engine(Some(ev)),
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(_) => { app.on_engine(None); break; }
                    }
                }
            },
            msg = app::recv_term(term_rx) => {
                app.on_term_msg(msg);
                while let Some(rx) = app.term_events() {
                    match rx.try_recv() {
                        Ok(msg) => app.on_term_msg(Some(msg)),
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(_) => { app.on_term_msg(None); break; }
                    }
                }
            },
            _ = tick.tick() => {
                app.tick += 1;
                if app.on_tick() || app.animating() || app.has_hint() {
                    app.dirty = true;
                }
                app.attention_tick();
                if let Some(signal) = app.take_signal() {
                    use std::io::Write;
                    let mut out = io::stdout();
                    let _ = out.write_all(signal.as_bytes());
                    let _ = out.flush();
                }
            },
        }
    }
    Ok(())
}
