//! Git state for the status line and `/diff`: the branch and dirty counts
//! from one `git status --porcelain=v2 --branch` run off the UI thread,
//! refreshed after turns and file-changing tools (never while one runs).
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitState {
    pub branch: String,
    /// Commits ahead / behind the upstream.
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub modified: u32,
    pub untracked: u32,
    pub conflicted: u32,
}

impl GitState {
    pub fn dirty(&self) -> bool {
        self.staged + self.modified + self.untracked + self.conflicted > 0
    }
    /// `git main +2 ~3 ?1`: staged, modified, untracked. Plain text: a
    /// branch glyph (⎇ or a Nerd Font one) is missing from many fonts.
    pub fn label(&self) -> String {
        let mut s = format!("git {}", self.branch);
        if self.ahead > 0 {
            s.push_str(&format!(" ↑{}", self.ahead));
        }
        if self.behind > 0 {
            s.push_str(&format!(" ↓{}", self.behind));
        }
        for (n, mark) in [
            (self.conflicted, '!'),
            (self.staged, '+'),
            (self.modified, '~'),
            (self.untracked, '?'),
        ] {
            if n > 0 {
                s.push_str(&format!(" {mark}{n}"));
            }
        }
        s
    }
}

/// Parse `git status --porcelain=v2 --branch` output.
pub fn parse(out: &str) -> GitState {
    let mut g = GitState::default();
    for line in out.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            g.branch = head.to_string();
        } else if let Some(oid) = line.strip_prefix("# branch.oid ")
            && g.branch == "(detached)"
        {
            g.branch = oid.chars().take(8).collect();
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    g.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    g.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let xy = line.split_whitespace().nth(1).unwrap_or("..");
            let mut c = xy.chars();
            if c.next().is_some_and(|x| x != '.') {
                g.staged += 1;
            }
            if c.next().is_some_and(|y| y != '.') {
                g.modified += 1;
            }
        } else if line.starts_with("u ") {
            g.conflicted += 1;
        } else if line.starts_with("? ") {
            g.untracked += 1;
        }
    }
    if g.branch == "(detached)" {
        g.branch = "detached".into();
    }
    g
}

fn status(cwd: &Path) -> Option<GitState> {
    let out = Command::new("git")
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .current_dir(cwd)
        // Never take the index lock for a status line.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse(&String::from_utf8_lossy(&out.stdout)))
}

/// The background refresher: at most one `git status` in flight.
pub struct GitWatch {
    cwd: PathBuf,
    pub state: Option<GitState>,
    rx: Option<mpsc::Receiver<Option<GitState>>>,
    last: Option<Instant>,
    /// Not a repository (or no git): stop asking.
    off: bool,
    want: bool,
}

impl GitWatch {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            state: None,
            rx: None,
            last: None,
            off: false,
            want: true,
        }
    }

    /// Ask for a refresh (after a turn or a file-changing tool).
    pub fn poke(&mut self) {
        self.want = true;
    }

    /// Start a refresh when one is wanted (or the last is a minute old),
    /// and pick up a finished one. True when the state changed.
    pub fn poll(&mut self) -> bool {
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(s) => {
                    self.rx = None;
                    if s.is_none() && self.state.is_none() {
                        self.off = true;
                    }
                    let changed = s != self.state;
                    self.state = s;
                    return changed;
                }
                Err(mpsc::TryRecvError::Empty) => return false,
                Err(mpsc::TryRecvError::Disconnected) => self.rx = None,
            }
        }
        let stale = self
            .last
            .is_none_or(|t| t.elapsed() > Duration::from_secs(60));
        if self.off || !(self.want || stale) {
            return false;
        }
        if self
            .last
            .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
        {
            return false;
        }
        self.want = false;
        self.last = Some(Instant::now());
        let (tx, rx) = mpsc::channel();
        let cwd = self.cwd.clone();
        std::thread::spawn(move || {
            let _ = tx.send(status(&cwd));
        });
        self.rx = Some(rx);
        false
    }
}
