//! The agent access policy (Settings › Tools › Agent Access): what agents connected through MCP
//! (Claude Code over `/ide`, the headless `codecraft-cli mcp`, built-in agent jobs) may do with
//! CodeCraft's engine tools.
//!
//! Each capability is Off, Ask or Allow. Reading (files, problems, symbols, state) is allowed by
//! default; running programs, debugging, profiling, writing files and modifying databases ask.
//! In the IDE, Ask shows an approval card (Allow once / Always for this session / Deny) and the
//! tool call waits for it. Headless there is nobody to ask: Ask is denied, and `--allow …` turns
//! capabilities on. Agent jobs launched in Auto mode get Ask capabilities without prompts (their
//! user chose Auto); Review jobs never write the open project's files (their calls run on their
//! worktree, `crate::agent_worktree`). The policy is saved in `$CODECRAFT_HOME/agent-access.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::approvals::AgentMode;
use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::params::{opt_str, req_str};
use crate::{CommandError, Session};

/// The Settings page for the policy.
pub const PAGE: &str = crate::settings::PAGE_AGENT_ACCESS;
const FILE_NAME: &str = "agent-access.json";
/// Approval cards of agent tool calls come from `"<caller> · <capability>"`.
pub const SOURCE_SEPARATOR: &str = " · ";

/// The approval-card source of `caller` asking for `cap`.
pub fn approval_source(caller: &str, cap: Capability) -> String {
    format!("{caller}{SOURCE_SEPARATOR}{}", cap.id())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    /// Read files, problems, symbols, debugger/profiler/test state, data source metadata.
    Read,
    /// Run configurations, tests, builds, inspections that start processes.
    Run,
    /// Start and control debug sessions, breakpoints, evaluation.
    Debug,
    /// Start and stop the profiler.
    Profile,
    /// Change files (and any other command that edits the project).
    Write,
    /// SQL that modifies data.
    DbWrite,
    /// Anything that runs on, connects to or changes an SSH host or a Docker container (remote
    /// run targets, remote debugging, deployment, data sources behind an SSH tunnel). For now
    /// this is always asked, once per call: it can be Off or Ask, never Allow, "Always for this
    /// session" doesn't apply, Auto jobs are asked too, and headless servers refuse it.
    Remote,
    SelfUpdate,
}

impl Capability {
    pub const ALL: [Capability; 8] = [
        Capability::Read,
        Capability::Run,
        Capability::Debug,
        Capability::Profile,
        Capability::Write,
        Capability::DbWrite,
        Capability::Remote,
        Capability::SelfUpdate,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Run => "run",
            Self::Debug => "debug",
            Self::Profile => "profile",
            Self::Write => "write",
            Self::DbWrite => "db-write",
            Self::Remote => "remote",
            Self::SelfUpdate => "self-update",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "Read project and IDE state",
            Self::Run => "Run programs and tests",
            Self::Debug => "Debug",
            Self::Profile => "Profile",
            Self::Write => "Edit files",
            Self::DbWrite => "Modify databases",
            Self::Remote => "Act on SSH hosts and Docker containers",
            Self::SelfUpdate => "Rebuild or restart CodeCraft",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Read => {
                "Files (with unsaved changes), problems, symbols, usages, test results, debugger and profiler state"
            }
            Self::Run => "Run configurations, tests, cargo commands and inspections",
            Self::Debug => "Start debug sessions, set breakpoints, step, evaluate expressions",
            Self::Profile => "Record CPU profiles of run configurations",
            Self::Write => "Change files and run commands that edit the project",
            Self::DbWrite => "INSERT/UPDATE/DELETE/DDL on data sources (read-only queries only need Read)",
            Self::SelfUpdate => "Prepare, update, build or restart CodeCraft. Always asked for every call.",
            Self::Remote => {
                "Run, debug, test or profile on an SSH host or Docker container, deployment, remote hosts, and \
                 data sources behind an SSH tunnel. Always asked, every time (never Allow)"
            }
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace('_', "-");
        Self::ALL.into_iter().find(|c| c.id() == s || (s == "dbwrite" && *c == Self::DbWrite))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Off,
    Ask,
    Allow,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Off, Level::Ask, Level::Allow];

    pub fn id(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ask => "ask",
            Self::Allow => "allow",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Ask => "Ask",
            Self::Allow => "Allow",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.id().eq_ignore_ascii_case(s.trim()))
    }
}

/// What a tool call may do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Ask the user (interactive sessions only).
    Ask,
    Deny(String),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    levels: BTreeMap<Capability, Level>,
}

pub struct AgentAccess {
    levels: BTreeMap<Capability, Level>,
    /// Someone can answer approval cards (false for the headless CLI).
    interactive: bool,
    /// `(caller, capability)` pairs the user allowed for the rest of the session.
    session: BTreeSet<(String, Capability)>,
    /// Instead of `$CODECRAFT_HOME/agent-access.json` (tests, `Session::isolate_home`).
    pub path_override: Option<PathBuf>,
    /// Never read or write the file (headless policies from `--allow`).
    ephemeral: bool,
    loaded: bool,
    pub error: Option<String>,
}

impl Default for AgentAccess {
    fn default() -> Self {
        Self {
            levels: default_levels(),
            interactive: true,
            session: BTreeSet::new(),
            path_override: None,
            ephemeral: false,
            loaded: false,
            error: None,
        }
    }
}

fn default_levels() -> BTreeMap<Capability, Level> {
    Capability::ALL.into_iter().map(|c| (c, if c == Capability::Read { Level::Allow } else { Level::Ask })).collect()
}

impl AgentAccess {
    /// A policy for a headless server: read plus the `allowed` capabilities; everything else is
    /// denied (nobody can approve). Not saved.
    pub fn headless(allowed: &[Capability]) -> Self {
        let mut levels = default_levels();
        for c in allowed.iter().filter(|c| !matches!(**c, Capability::Remote | Capability::SelfUpdate)) {
            levels.insert(*c, Level::Allow);
        }
        Self { levels, interactive: false, ephemeral: true, loaded: true, ..Self::default() }
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    pub fn level(&self, cap: Capability) -> Level {
        let level = self.levels.get(&cap).copied().unwrap_or(Level::Ask);
        if matches!(cap, Capability::Remote | Capability::SelfUpdate) && level == Level::Allow {
            Level::Ask
        } else {
            level
        }
    }

    /// Change one capability (saved unless ephemeral).
    pub fn set_level(&mut self, cap: Capability, level: Level) -> Result<(), String> {
        if matches!(cap, Capability::Remote | Capability::SelfUpdate) && level == Level::Allow {
            return Err("SSH hosts and Docker containers are always asked for now: `remote` can be off or ask".into());
        }
        self.ensure_loaded();
        self.levels.insert(cap, level);
        self.session.retain(|(_, c)| *c != cap);
        self.save()
    }

    /// Back to the defaults; forgets session approvals.
    pub fn reset(&mut self) -> Result<(), String> {
        self.levels = default_levels();
        self.session.clear();
        self.loaded = true;
        self.save()
    }

    /// Remember "Always for this session" for a caller and capability.
    pub fn allow_for_session(&mut self, caller: &str, cap: Capability) {
        self.session.insert((caller.to_string(), cap));
    }

    pub fn session_approvals(&self) -> Vec<(String, Capability)> {
        self.session.iter().cloned().collect()
    }

    pub fn clear_session(&mut self) {
        self.session.clear();
    }

    /// Whether `caller` (in agent job mode `job_mode`, if it is a job) may use `cap`.
    pub fn verdict(&mut self, caller: &str, job_mode: Option<AgentMode>, cap: Capability) -> Verdict {
        self.ensure_loaded();
        let level = self.level(cap);
        if level == Level::Off {
            return Verdict::Deny(format!(
                "denied: agent access to `{}` is off (Settings › Tools › Agent Access)",
                cap.id()
            ));
        }
        if matches!(cap, Capability::Remote | Capability::SelfUpdate) {
            // Always asked, once per call: no Allow level, no session approval, no Auto bypass.
            if !self.interactive {
                return Verdict::Deny(
                    "denied: acting on an SSH host or Docker container needs the user's approval in the IDE, \
                     and this server is headless (there is no --allow for it)"
                        .to_string(),
                );
            }
            return Verdict::Ask;
        }
        if job_mode == Some(AgentMode::Review) && cap == Capability::Write {
            return Verdict::Deny(
                "denied: a Review job changes files in its worktree, not through the IDE".to_string(),
            );
        }
        if level == Level::Allow || self.session.contains(&(caller.to_string(), cap)) {
            return Verdict::Allow;
        }
        if job_mode == Some(AgentMode::Auto) {
            return Verdict::Allow;
        }
        if !self.interactive {
            return Verdict::Deny(format!(
                "denied: `{0}` needs approval and this server is headless; restart it with `--allow {0}`",
                cap.id()
            ));
        }
        Verdict::Ask
    }

    pub fn path(&self) -> Option<PathBuf> {
        if self.ephemeral {
            return None;
        }
        if let Some(p) = &self.path_override {
            return Some(p.clone());
        }
        if let Some(home) = std::env::var_os("CODECRAFT_HOME").filter(|h| !h.is_empty()) {
            return Some(PathBuf::from(home).join(FILE_NAME));
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codecraft").join(FILE_NAME))
    }

    fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(path) = self.path() else { return };
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        match serde_json::from_str::<Saved>(&text) {
            Ok(saved) => {
                for (c, l) in saved.levels {
                    self.levels.insert(c, l);
                }
            }
            Err(e) => self.error = Some(format!("{}: {e}", path.display())),
        }
    }

    fn save(&mut self) -> Result<(), String> {
        let Some(path) = self.path() else { return Ok(()) };
        let saved = Saved { levels: self.levels.clone() };
        let text = serde_json::to_string_pretty(&saved).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        codecraft_fs::write_text_file_atomic(&path, &text).map_err(|e| e.to_string())?;
        self.error = None;
        Ok(())
    }

    /// The policy as the Settings page and agents see it.
    pub fn to_json(&mut self) -> Value {
        self.ensure_loaded();
        let caps: Vec<Value> = Capability::ALL
            .into_iter()
            .map(|c| {
                json!({
                    "capability": c.id(),
                    "label": c.label(),
                    "description": c.description(),
                    "level": self.level(c).id(),
                })
            })
            .collect();
        let session: Vec<Value> =
            self.session.iter().map(|(who, c)| json!({ "caller": who, "capability": c.id() })).collect();
        json!({
            "capabilities": caps,
            "interactive": self.interactive,
            "sessionApprovals": session,
            "file": self.path(),
            "error": self.error,
        })
    }
}

// ---------------------------------------------------------------- commands

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "agentAccess.get",
            "Agent Access Policy",
            None,
            "→ {capabilities: [{capability, label, description, level: off|ask|allow}], interactive, sessionApprovals, file}",
            Context::Global,
            always,
            |s, _| Ok(s.agent_access.to_json()),
        ),
        spec(
            "agentAccess.set",
            "Set Agent Access",
            None,
            "{capability: read|run|debug|profile|write|db-write|remote, level: off|ask|allow} — what agents connected \
             over MCP may do (Settings › Tools › Agent Access; `remote` is off or ask)",
            Context::Global,
            always,
            set,
        ),
        spec(
            "agentAccess.reset",
            "Reset Agent Access",
            None,
            "Read: Allow; everything else: Ask. Forgets the approvals given for this session",
            Context::Global,
            always,
            |s, _| {
                s.agent_access.reset().map_err(CommandError::Failed)?;
                s.approvals.forget_always(|src| src.contains(SOURCE_SEPARATOR));
                Ok(s.agent_access.to_json())
            },
        ),
        spec(
            "agentAccess.clearSession",
            "Forget Session Approvals",
            None,
            "Forget the \"Always for this session\" approvals",
            Context::Global,
            always,
            |s, _| {
                s.agent_access.clear_session();
                s.approvals.forget_always(|src| src.contains(SOURCE_SEPARATOR));
                Ok(s.agent_access.to_json())
            },
        ),
    ]
}

fn set(s: &mut Session, p: &Value) -> CmdResult {
    let cap_id = req_str(p, "capability")?;
    let cap = Capability::parse(cap_id).ok_or_else(|| {
        CommandError::BadParams(format!("unknown capability `{cap_id}` (read, run, debug, profile, write, db-write, remote)"))
    })?;
    let level_id = opt_str(p, "level").ok_or_else(|| CommandError::BadParams("missing `level`".into()))?;
    let level = Level::parse(level_id)
        .ok_or_else(|| CommandError::BadParams(format!("unknown level `{level_id}` (off, ask, allow)")))?;
    s.agent_access.set_level(cap, level).map_err(CommandError::Failed)?;
    // "Always for this session" no longer applies once the level changes.
    let suffix = format!("{SOURCE_SEPARATOR}{}", cap.id());
    s.approvals.forget_always(|src| src.ends_with(&suffix));
    s.status = format!("Agent access: {} → {}", cap.label(), level.label());
    Ok(s.agent_access.to_json())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_policy(dir: &std::path::Path) -> AgentAccess {
        AgentAccess { path_override: Some(dir.join(FILE_NAME)), ..AgentAccess::default() }
    }

    /// SSH hosts and Docker containers are always asked (M16, the user's rule): never Allow, no
    /// session approval, Auto jobs asked too, headless refused, a saved Allow loads as Ask.
    #[test]
    fn remote_is_always_asked() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = temp_policy(dir.path());
        assert_eq!(a.verdict("x", None, Capability::Remote), Verdict::Ask);
        assert_eq!(a.verdict("x", Some(AgentMode::Auto), Capability::Remote), Verdict::Ask);
        a.allow_for_session("x", Capability::Remote);
        assert_eq!(a.verdict("x", None, Capability::Remote), Verdict::Ask);
        assert!(a.set_level(Capability::Remote, Level::Allow).is_err());
        assert_eq!(a.level(Capability::Remote), Level::Ask);
        a.set_level(Capability::Remote, Level::Off).unwrap();
        assert!(matches!(a.verdict("x", None, Capability::Remote), Verdict::Deny(_)));
        // A hand-edited file that says allow still asks.
        std::fs::write(dir.path().join(FILE_NAME), r#"{"levels":{"remote":"allow"}}"#).unwrap();
        let mut b = temp_policy(dir.path());
        assert_eq!(b.verdict("x", None, Capability::Remote), Verdict::Ask);
        // Headless: refused even when listed.
        let mut h = AgentAccess::headless(&[Capability::Run, Capability::Remote]);
        assert_eq!(h.verdict("cli", None, Capability::Run), Verdict::Allow);
        match h.verdict("cli", None, Capability::Remote) {
            Verdict::Deny(why) => assert!(why.contains("headless"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn defaults_ask_except_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = temp_policy(dir.path());
        assert_eq!(a.verdict("x", None, Capability::Read), Verdict::Allow);
        for c in [Capability::Run, Capability::Debug, Capability::Profile, Capability::Write, Capability::DbWrite] {
            assert_eq!(a.verdict("x", None, c), Verdict::Ask, "{c:?}");
        }
    }

    #[test]
    fn levels_persist_and_session_approvals_are_per_caller() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = temp_policy(dir.path());
        a.set_level(Capability::Debug, Level::Allow).unwrap();
        a.set_level(Capability::Write, Level::Off).unwrap();
        let mut b = temp_policy(dir.path());
        assert_eq!(b.verdict("x", None, Capability::Debug), Verdict::Allow);
        assert!(matches!(b.verdict("x", None, Capability::Write), Verdict::Deny(m) if m.contains("off")));
        b.allow_for_session("Claude Code", Capability::Run);
        assert_eq!(b.verdict("Claude Code", None, Capability::Run), Verdict::Allow);
        assert_eq!(b.verdict("codex", None, Capability::Run), Verdict::Ask);
        b.reset().unwrap();
        assert_eq!(b.verdict("Claude Code", None, Capability::Run), Verdict::Ask);
        assert_eq!(temp_policy(dir.path()).verdict("x", None, Capability::Write), Verdict::Ask);
        // A broken file keeps the defaults and reports the error.
        std::fs::write(dir.path().join(FILE_NAME), "{nope").unwrap();
        let mut c = temp_policy(dir.path());
        assert_eq!(c.verdict("x", None, Capability::Read), Verdict::Allow);
        assert!(c.error.is_some());
    }

    #[test]
    fn job_modes_and_headless() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = temp_policy(dir.path());
        assert_eq!(a.verdict("job", Some(AgentMode::Auto), Capability::Debug), Verdict::Allow);
        assert_eq!(a.verdict("job", Some(AgentMode::Ask), Capability::Debug), Verdict::Ask);
        assert!(matches!(a.verdict("job", Some(AgentMode::Review), Capability::Write), Verdict::Deny(_)));
        a.set_level(Capability::Profile, Level::Off).unwrap();
        assert!(matches!(a.verdict("job", Some(AgentMode::Auto), Capability::Profile), Verdict::Deny(_)));

        let mut h = AgentAccess::headless(&[Capability::Debug]);
        assert!(h.path().is_none());
        assert_eq!(h.verdict("cli", None, Capability::Debug), Verdict::Allow);
        assert_eq!(h.verdict("cli", None, Capability::Read), Verdict::Allow);
        match h.verdict("cli", None, Capability::Write) {
            Verdict::Deny(m) => assert!(m.contains("--allow write"), "{m}"),
            v => panic!("{v:?}"),
        }
        assert_eq!(Capability::parse("DB_WRITE"), Some(Capability::DbWrite));
        assert_eq!(Capability::parse("dbwrite"), Some(Capability::DbWrite));
        assert_eq!(Capability::parse("nope"), None);
        assert_eq!(Level::parse("Allow"), Some(Level::Allow));
    }

    #[test]
    fn commands_validate() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(dir.path());
        let v = s.execute("agentAccess.set", &json!({ "capability": "debug", "level": "allow" })).unwrap();
        assert!(
            v["capabilities"].as_array().unwrap().iter().any(|c| c["capability"] == "debug" && c["level"] == "allow")
        );
        assert!(dir.path().join(FILE_NAME).is_file());
        assert!(s.execute("agentAccess.set", &json!({ "capability": "root", "level": "allow" })).is_err());
        assert!(s.execute("agentAccess.set", &json!({ "capability": "run", "level": "maybe" })).is_err());
        assert!(s.execute("agentAccess.set", &json!({ "capability": "run" })).is_err());
        let v = s.execute("agentAccess.reset", &Value::Null).unwrap();
        assert!(v["capabilities"].as_array().unwrap().iter().all(|c| c["level"] != "off"));
        s.execute("agentAccess.clearSession", &Value::Null).unwrap();
        assert_eq!(s.execute("agentAccess.get", &Value::Null).unwrap()["interactive"], true);
    }
}
