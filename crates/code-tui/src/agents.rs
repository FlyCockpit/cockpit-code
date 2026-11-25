//! Agent tree state (subagents, their live transcripts, background jobs) and
//! their descendants (the agent strip is drawn by the app).
use crate::engine::{AgentView, JobView, age, alive};
use crate::transcript::{ChildLink, Transcript};
use std::collections::HashMap;

pub struct Agent {
    pub view: AgentView,
    /// Short display name: `build#1`.
    pub label: String,
    /// Live context meter: (used, window).
    pub ctx: (u64, Option<u64>),
    pub ctx_estimate: bool,
    pub ctx_unknown: bool,
    pub transcript: Transcript,
    /// The transcript holds the full history (live from the start, or replayed).
    pub loaded: bool,
    pub compacting: bool,
}

#[derive(Default)]
pub struct Tree {
    pub agents: Vec<Agent>,
    jobs: Vec<JobView>,
    counts: HashMap<String, usize>,
}

impl Tree {
    pub fn clear(&mut self) {
        self.agents.clear();
        self.jobs.clear();
        self.counts.clear();
    }
    /// Register an agent; `live` agents are seen from their first event.
    pub fn add(&mut self, view: AgentView, live: bool) -> usize {
        if let Some(i) = self.index(&view.id) {
            // A follow-up (`task {continue}`) restarts a known child.
            let a = &mut self.agents[i].view;
            a.status = view.status;
            a.task = view.task;
            a.background = view.background;
            return i;
        }
        let n = self.counts.entry(view.role.clone()).or_default();
        *n += 1;
        let label = format!("{}#{n}", view.role);
        self.agents.push(Agent {
            view,
            label,
            ctx: (0, None),
            ctx_estimate: false,
            ctx_unknown: false,
            transcript: Transcript::default(),
            loaded: live,
            compacting: false,
        });
        self.agents.len() - 1
    }
    pub fn index(&self, id: &str) -> Option<usize> {
        self.agents.iter().position(|a| a.view.id == id)
    }
    pub fn get(&self, id: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.view.id == id)
    }
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Agent> {
        self.agents.iter_mut().find(|a| a.view.id == id)
    }
    /// `root`, `build#1`, or the raw id for an unknown agent.
    pub fn label(&self, id: &str) -> String {
        match id {
            "root" | "user" | "jobs" => id.to_string(),
            _ => self
                .get(id)
                .map_or_else(|| id.to_string(), |a| a.label.clone()),
        }
    }
    /// An engine breadcrumb (`root › build-1a2b › explore-3c4d`) with display labels.
    pub fn crumb(&self, breadcrumb: &str) -> String {
        breadcrumb
            .split(" › ")
            .map(|id| self.label(id.trim()))
            .collect::<Vec<_>>()
            .join(" › ")
    }
    /// `root › build#1 › explore#2` for an agent, from the parent links.
    pub fn path(&self, id: &str) -> String {
        let mut chain = vec![];
        let mut next = Some(id.to_string());
        while let Some(id) = next {
            if chain.len() > 64 {
                break;
            }
            chain.push(self.label(&id));
            next = self.get(&id).map(|a| a.view.parent.clone());
        }
        if chain.last().map(String::as_str) != Some("root") {
            chain.push("root".into());
        }
        chain.reverse();
        chain.join(" › ")
    }
    /// Whether `id` is `root_id` or one of its descendants.
    pub fn in_subtree(&self, id: &str, root_id: &str) -> bool {
        if root_id == "root" {
            return true;
        }
        let mut next = Some(id.to_string());
        let mut n = 0;
        while let Some(cur) = next {
            if cur == root_id {
                return true;
            }
            n += 1;
            if n > 64 {
                break;
            }
            next = self.get(&cur).map(|a| a.view.parent.clone());
        }
        false
    }
    pub fn link(&self, id: &str, follow_up: bool) -> Option<ChildLink> {
        self.get(id).map(|a| ChildLink {
            id: id.to_string(),
            label: a.label.clone(),
            status: a.view.status.clone(),
            follow_up,
        })
    }
    pub fn running(&self) -> usize {
        self.agents.iter().filter(|a| alive(&a.view.status)).count()
    }
    pub fn upsert_job(&mut self, job: JobView) {
        match self.jobs.iter_mut().find(|j| j.id == job.id) {
            Some(j) => *j = job,
            None => self.jobs.push(job),
        }
    }
    pub fn job(&self, id: &str) -> Option<&JobView> {
        self.jobs.iter().find(|j| j.id == id)
    }
    pub fn job_running(&self, id: &str) -> bool {
        self.jobs.iter().any(|j| j.id == id && j.running())
    }
    /// The running background job of agent `agent`.
    pub fn agent_job(&self, agent: &str) -> Option<String> {
        self.jobs
            .iter()
            .find(|j| j.running() && j.agent.as_deref() == Some(agent))
            .map(|j| j.id.clone())
    }
    pub fn running_jobs(&self) -> usize {
        self.jobs.iter().filter(|j| j.running()).count()
    }
    /// Agents depth-first from root: (depth, index), root's children at depth 1.
    pub fn order(&self) -> Vec<(usize, usize)> {
        fn walk(tree: &Tree, parent: &str, depth: usize, out: &mut Vec<(usize, usize)>) {
            for (i, a) in tree.agents.iter().enumerate() {
                if a.view.parent == parent {
                    out.push((depth, i));
                    walk(tree, &a.view.id, depth + 1, out);
                }
            }
        }
        let mut out = vec![];
        walk(self, "root", 1, &mut out);
        // Orphans (a parent missing from the registry) go last.
        for i in 0..self.agents.len() {
            if !out.iter().any(|(_, j)| *j == i) {
                out.push((1, i));
            }
        }
        out
    }
}

pub fn job_age(job: &JobView) -> String {
    age(if job.running() {
        &job.started
    } else {
        &job.updated
    })
}
