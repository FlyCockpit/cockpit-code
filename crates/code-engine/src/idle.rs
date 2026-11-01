//! Frontends call this tick while open; headless never opts in.
use crate::{Action, Session, build_model, schema::EngineEvent};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Default)]
pub(crate) struct IdleState {
    since: Option<Instant>,
    fired: bool,
}
impl Session {
    /// Tick from TUI/ACP. `frontend_busy` includes composer typing and open dialogs.
    /// Drain the returned receiver (or keep subscribing) until TurnEnd.
    pub fn idle_tick(&self, frontend_busy: bool) -> Option<mpsc::UnboundedReceiver<EngineEvent>> {
        let work = self.running_work();
        let blocked = frontend_busy
            || self.is_active()
            || !work.jobs.is_empty()
            || !work.children.is_empty()
            || !self.pending_reviewer_guidance().is_empty()
            || !self.control.approvals.lock().unwrap().is_empty()
            || !self.control.fork_busy.lock().unwrap().is_empty()
            || self.pending_questions().iter().any(|q| q.answer.is_none())
            || self
                .control
                .journal
                .lock()
                .unwrap()
                .contracts
                .values()
                .any(|c| {
                    c.roadblock().is_some() || c.proposals.iter().any(|p| p.resolution.is_none())
                })
            || self
                .goal_status()
                .is_some_and(|g| g.phase == "draft" || g.disposition == "running");
        let mut idle = self.control.idle.lock().unwrap();
        if blocked {
            idle.since = None;
            return None;
        }
        if idle.fired {
            return None;
        }
        let snapshot = self.configuration();
        let model = build_model(&snapshot, &self.settings()).ok()?;
        let seconds = model.config.autocompact_at_idle?;
        let meter = self.context_meter();
        let minimum = model.config.autocompact_at_idle_min_percent.unwrap_or(30);
        if meter
            .window
            .is_none_or(|w| meter.used.saturating_mul(100) / w.max(1) < minimum)
        {
            idle.since = None;
            return None;
        }
        if idle.since.get_or_insert_with(Instant::now).elapsed() < Duration::from_secs(seconds) {
            return None;
        }
        idle.fired = true;
        drop(idle);
        Some(self.launch(Action::IdleCompact))
    }
}
