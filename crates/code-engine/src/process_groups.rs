//! Registry of the live child process groups this process owns.
//!
//! Tool, provider, terminal, LSP and MCP children run in their own process
//! groups, so a terminal interrupt does not reach them and their `Drop` guards
//! are what normally kill them. A forced exit (`std::process::exit`) skips every
//! destructor, so the CLI calls [`kill_all`] first.

use std::collections::BTreeSet;
use std::sync::Mutex;

static GROUPS: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

fn groups() -> std::sync::MutexGuard<'static, BTreeSet<i32>> {
    GROUPS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records a live process group id (the group leader's pid).
pub(crate) fn track(pgid: i32) {
    if pgid > 0 {
        groups().insert(pgid);
    }
}

/// Forgets a process group once its owner has killed or reaped it.
pub(crate) fn forget(pgid: i32) {
    groups().remove(&pgid);
}

/// Sends SIGKILL to every tracked process group. Call this immediately before a
/// forced exit; ordinary shutdown relies on the owners' `Drop` guards.
///
/// The registry lock is held while signalling, so no owner can forget its group
/// halfway through. A group whose leader was already reaped carries the same
/// pid-reuse caveat as the owner's own `Drop` guard, which also kills by group id.
pub fn kill_all() {
    let mut tracked = groups();
    signal_groups(&tracked, KILL);
    tracked.clear();
}

#[cfg(unix)]
const KILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const KILL: i32 = 9;

fn signal_groups(groups: &BTreeSet<i32>, signal: i32) {
    #[cfg(unix)]
    for &pgid in groups {
        // SAFETY: kill(2) with a negative pid only signals that process group.
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
    #[cfg(not(unix))]
    let _ = (groups, signal);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_and_forgets_groups() {
        track(i32::MAX - 7);
        assert!(groups().contains(&(i32::MAX - 7)));
        forget(i32::MAX - 7);
        assert!(!groups().contains(&(i32::MAX - 7)));
        track(0);
        track(-3);
        assert!(!groups().contains(&0) && !groups().contains(&-3));
    }

    // Exercises the signalling path on a local set: calling the process-wide
    // `kill_all` here would kill children owned by tests running in parallel.
    #[cfg(unix)]
    #[test]
    fn signal_groups_kills_each_group() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        signal_groups(&BTreeSet::from([pgid]), KILL);
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }
}
