//! Human approval of agent actions. Shared by every agent integration (headless CLI jobs,
//! ACP agents, the Claude Code IDE bridge, feedback jobs).
//!
//! Background services hold an [`ApprovalSink`] and call [`ApprovalSink::ask`] from their own
//! thread; the request lands in [`Session::approvals`] on the next pump, the UI shows it, and
//! the user's [`Decision`] goes back through the returned channel. Nothing blocks the UI thread.

use std::path::PathBuf;

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How much an agent may do without asking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentMode {
    /// Edits are applied directly to the project; no prompts.
    Auto,
    /// Every file change and command needs an explicit approval (diff shown first).
    Ask,
    /// The agent works in a separate git worktree; its result is reviewed file by file and
    /// only accepted changes reach the project.
    #[default]
    Review,
}

impl AgentMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Ask => "Ask before each change",
            Self::Review => "Review in a worktree",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ApprovalId(pub u64);

/// What is being asked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ApprovalKind {
    /// Write a file: the UI shows a diff of `original` → `proposed`.
    FileEdit { path: PathBuf, original: String, proposed: String },
    /// Run a shell command.
    Command { command: String, cwd: Option<PathBuf> },
    /// Any other tool call (name + raw input).
    ToolUse { tool: String, input: Value },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: ApprovalId,
    /// Who asks: "Claude Code", "Codex (job 3)", …
    pub source: String,
    /// One-line summary for the card header.
    pub title: String,
    pub kind: ApprovalKind,
    /// Asked every time: no "Always" (an earlier "Always" for the source doesn't answer it, and
    /// choosing it counts as Allow once). Used for calls that act on SSH hosts and containers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub once: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Allow,
    /// Allow this and similar requests from the same source for the rest of the session.
    AllowAlways,
    Deny,
}

type Inbound = (ApprovalRequest, Sender<Decision>);

/// Handle for background services to ask for approval. Cheap to clone.
#[derive(Clone)]
pub struct ApprovalSink {
    tx: Sender<Inbound>,
    next: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ApprovalSink {
    /// Queue a request; the decision arrives on the returned channel (it disconnects without a
    /// decision if the IDE shuts down; treat that as Deny).
    pub fn ask(
        &self,
        source: impl Into<String>,
        title: impl Into<String>,
        kind: ApprovalKind,
    ) -> (ApprovalId, Receiver<Decision>) {
        self.send(source.into(), title.into(), kind, false)
    }

    /// [`ApprovalSink::ask`] for a request that must be asked every time (no "Always").
    pub fn ask_once(
        &self,
        source: impl Into<String>,
        title: impl Into<String>,
        kind: ApprovalKind,
    ) -> (ApprovalId, Receiver<Decision>) {
        self.send(source.into(), title.into(), kind, true)
    }

    fn send(&self, source: String, title: String, kind: ApprovalKind, once: bool) -> (ApprovalId, Receiver<Decision>) {
        let id = ApprovalId(self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let req = ApprovalRequest { id, source, title, kind, once };
        if self.tx.send((req, tx)).is_err() {
            log::warn!("approval queue closed; request {id:?} denied");
        }
        (id, rx)
    }
}

/// Pending approvals as the UI sees them.
pub struct Approvals {
    rx: Receiver<Inbound>,
    sink: ApprovalSink,
    pending: Vec<(ApprovalRequest, Sender<Decision>)>,
    /// Sources the user chose "allow always" for (this session).
    always: Vec<String>,
}

impl Default for Approvals {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self { rx, sink: ApprovalSink { tx, next: Default::default() }, pending: Vec::new(), always: Vec::new() }
    }
}

impl Approvals {
    pub fn sink(&self) -> ApprovalSink {
        self.sink.clone()
    }

    /// Move newly arrived requests into the pending list (auto-allowing "always" sources).
    /// Returns true if anything arrived.
    pub fn pump(&mut self) -> bool {
        let mut changed = false;
        while let Ok((req, tx)) = self.rx.try_recv() {
            changed = true;
            if !req.once && self.always.contains(&req.source) {
                let _ = tx.send(Decision::Allow);
            } else {
                self.pending.push((req, tx));
            }
        }
        changed
    }

    pub fn pending(&self) -> impl Iterator<Item = &ApprovalRequest> {
        self.pending.iter().map(|(r, _)| r)
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Answer a request. Returns false if it is no longer pending.
    pub fn decide(&mut self, id: ApprovalId, decision: Decision) -> bool {
        let Some(pos) = self.pending.iter().position(|(r, _)| r.id == id) else { return false };
        let (req, tx) = self.pending.remove(pos);
        let decision = if req.once && decision == Decision::AllowAlways { Decision::Allow } else { decision };
        if decision == Decision::AllowAlways && !self.always.contains(&req.source) {
            self.always.push(req.source.clone());
        }
        let _ = tx.send(decision);
        true
    }

    /// Remove a card without a decision: the request was cancelled or its client went away. The
    /// asker sees its channel close. Returns false if it is no longer pending.
    pub fn withdraw(&mut self, id: ApprovalId) -> bool {
        let Some(pos) = self.pending.iter().position(|(r, _)| r.id == id) else { return false };
        self.pending.remove(pos);
        true
    }

    /// Forget "allow always" for the sources matching `f` (their next requests ask again).
    pub fn forget_always(&mut self, f: impl Fn(&str) -> bool) {
        self.always.retain(|s| !f(s));
    }

    /// Deny everything still pending (e.g. on shutdown).
    pub fn deny_all(&mut self) {
        for (_, tx) in self.pending.drain(..) {
            let _ = tx.send(Decision::Deny);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A once-only request is never answered by an earlier "Always", and "Always" on it counts
    /// as Allow once.
    #[test]
    fn once_only_requests_are_asked_every_time() {
        let mut a = Approvals::default();
        let sink = a.sink();
        let (_, rx) = sink.ask("agent · write", "w", ApprovalKind::Command { command: "x".into(), cwd: None });
        a.pump();
        let id = a.pending().next().unwrap().id;
        a.decide(id, Decision::AllowAlways);
        assert_eq!(rx.try_recv(), Ok(Decision::AllowAlways));
        let (_, rx) = sink.ask_once("agent · write", "w", ApprovalKind::Command { command: "x".into(), cwd: None });
        a.pump();
        assert_eq!(a.len(), 1, "an earlier Always doesn't answer a once-only request");
        let req = a.pending().next().unwrap().clone();
        assert!(req.once);
        a.decide(req.id, Decision::AllowAlways);
        assert_eq!(rx.try_recv(), Ok(Decision::Allow));
        assert!(a.is_empty());
        let (_, _rx) = sink.ask_once("agent · remote", "r", ApprovalKind::Command { command: "x".into(), cwd: None });
        a.pump();
        let id = a.pending().next().unwrap().id;
        a.decide(id, Decision::AllowAlways);
        let (_, _rx) = sink.ask("agent · remote", "r", ApprovalKind::Command { command: "x".into(), cwd: None });
        a.pump();
        assert_eq!(a.len(), 1, "\"Always\" on a once-only request isn't remembered for its source");
    }

    #[test]
    fn ask_decide_roundtrip() {
        let mut a = Approvals::default();
        let sink = a.sink();
        let (id, rx) =
            sink.ask("Claude Code", "Edit main.rs", ApprovalKind::Command { command: "ls".into(), cwd: None });
        assert!(a.pump());
        assert_eq!(a.len(), 1);
        assert!(a.decide(id, Decision::AllowAlways));
        assert_eq!(rx.recv().ok(), Some(Decision::AllowAlways));
        // Same source is now auto-allowed.
        let (_, rx2) = sink.ask("Claude Code", "again", ApprovalKind::Command { command: "ls".into(), cwd: None });
        a.pump();
        assert!(a.is_empty());
        assert_eq!(rx2.recv().ok(), Some(Decision::Allow));
        assert!(!a.decide(id, Decision::Deny));
    }

    #[test]
    fn withdrawn_cards_go_without_a_decision() {
        let mut a = Approvals::default();
        let (id, rx) = a.sink().ask("Codex (job 2)", "Run tests", ApprovalKind::Command { command: "x".into(), cwd: None });
        a.pump();
        assert!(a.withdraw(id));
        assert!(a.is_empty());
        assert!(rx.recv().is_err(), "the asker sees the channel close, not a decision");
        assert!(!a.withdraw(id));
        assert!(!a.decide(id, Decision::Allow));
    }
}
