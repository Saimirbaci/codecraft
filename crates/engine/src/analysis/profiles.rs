//! Inspection profiles (Settings › Editor › Inspections): which analyzers run and, per rule,
//! on/off and a severity (Error, Warning, Weak Warning, Info), plus the analyzer options a
//! profile carries (Clippy lint groups, Ruff selectors, Clang Static Analyzer checkers,
//! clang-tidy checks, cppcheck checks).
//!
//! Built-in profiles (`Default`, `Strict`) are read-only; changing one saves a copy with the same
//! name in the project (Reset removes it). Profiles live in the project
//! (`<project>/.codecraft/inspection-profiles.json`, with the active profile and the commit check
//! switch) or in the IDE (`$CODECRAFT_HOME/inspection-profiles.json`); a project profile hides an
//! IDE or built-in profile with the same name.
//!
//! Rules are keyed `analyzer:rule` (`clippy:clippy::needless_return`, `ruff:F401`,
//! `clang_analyzer:core.NullDereference`); Clippy and cargo check share `clippy:`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_workspace::Severity;

use crate::Session;
use crate::inspections::{Finding, Tool};
use crate::user_store::{UserHome, load_json, save_json};

pub const DEFAULT: &str = "Default";
pub const STRICT: &str = "Strict";
const FILE_NAME: &str = "inspection-profiles.json";
const FORMAT: &str = "codecraft-inspection-profiles/1";
/// Profiles kept per file and rules per profile (hostile files stay small).
const MAX_PROFILES: usize = 64;
const MAX_RULES: usize = 5000;
const MAX_NAME: usize = 80;

// ---------------------------------------------------------------- severity levels

/// RustRover's highlighting levels, mapped onto the workspace severities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Error,
    Warning,
    WeakWarning,
    Info,
}

impl Level {
    pub const ALL: [Level; 4] = [Self::Error, Self::Warning, Self::WeakWarning, Self::Info];

    pub fn to_severity(self) -> Severity {
        match self {
            Self::Error => Severity::Error,
            Self::Warning => Severity::Warning,
            Self::WeakWarning => Severity::Info,
            Self::Info => Severity::Hint,
        }
    }

    pub fn of(sev: Severity) -> Self {
        match sev {
            Severity::Error => Self::Error,
            Severity::Warning => Self::Warning,
            Severity::Info => Self::WeakWarning,
            Severity::Hint => Self::Info,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
            "error" | "errors" => Some(Self::Error),
            "warning" | "warnings" => Some(Self::Warning),
            "weak_warning" | "weak" | "weakwarning" => Some(Self::WeakWarning),
            "info" | "information" | "hint" | "typo" => Some(Self::Info),
            _ => None,
        }
    }

    /// The id in JSON (`weak_warning`).
    pub fn id(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::WeakWarning => "weak_warning",
            Self::Info => "info",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::Warning => "Warning",
            Self::WeakWarning => "Weak Warning",
            Self::Info => "Info",
        }
    }
}

// ---------------------------------------------------------------- the profile

fn yes() -> bool {
    true
}

/// On/off and a severity override for an analyzer or a rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toggle {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Level>,
}

impl Default for Toggle {
    fn default() -> Self {
        Self { enabled: true, severity: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectionProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Analyzer id (`clippy`, `rust_analyzer`, `ruff`, `clang_analyzer`, …) → switch. Missing:
    /// on.
    #[serde(default)]
    pub analyzers: BTreeMap<String, Toggle>,
    /// `analyzer:rule` → switch. Missing: what the analyzer reports.
    #[serde(default)]
    pub rules: BTreeMap<String, Toggle>,
    /// Clippy lint groups turned on (`pedantic`, `nursery`, `cargo`, `restriction`).
    #[serde(default)]
    pub clippy_groups: BTreeSet<String>,
    /// Ruff selectors added to the project's (`--extend-select`: `B`, `UP`, `SIM`, …).
    #[serde(default)]
    pub ruff_select: Vec<String>,
    /// Clang Static Analyzer checkers or packages turned on (`security`, `optin.…`).
    #[serde(default)]
    pub clang_checkers: Vec<String>,
    /// clang-tidy `--checks` (empty: the project's `.clang-tidy`).
    #[serde(default)]
    pub clang_tidy_checks: String,
    /// cppcheck `--enable` (empty: `warning,style,performance,portability`).
    #[serde(default)]
    pub cppcheck_enable: String,
    /// Security advisories: `cargo audit` and `cargo deny check advisories` fetch the RustSec
    /// advisory database over the network, so they run only when this is on (QA M14 #21; off in
    /// the built-in profiles). Off, cargo deny still checks licenses, bans and sources offline
    /// when the project has a `deny.toml`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub advisories: bool,
}

/// Clippy lint groups a profile can turn on.
pub const CLIPPY_GROUPS: [&str; 4] = ["pedantic", "nursery", "cargo", "restriction"];

/// The profile family of a tool for rule keys (Clippy and cargo check report the same lints).
pub fn family(tool: Tool) -> &'static str {
    match tool {
        Tool::Clippy | Tool::CargoCheck => "clippy",
        t => t.id(),
    }
}

pub fn rule_key(tool: Tool, rule: &str) -> String {
    format!("{}:{rule}", family(tool))
}

fn analyzer_key(tool: Tool) -> &'static str {
    tool.id()
}

impl InspectionProfile {
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            description: String::new(),
            analyzers: BTreeMap::new(),
            rules: BTreeMap::new(),
            clippy_groups: BTreeSet::new(),
            ruff_select: Vec::new(),
            clang_checkers: Vec::new(),
            clang_tidy_checks: String::new(),
            cppcheck_enable: String::new(),
            advisories: false,
        }
    }

    /// Every analyzer with what it reports by default.
    pub fn default_profile() -> Self {
        let mut p = Self::named(DEFAULT);
        p.description = "Every installed analyzer with its own defaults".into();
        p
    }

    /// Clippy pedantic, more Ruff rules, the analyzer's security checkers.
    pub fn strict_profile() -> Self {
        let mut p = Self::named(STRICT);
        p.description = "Clippy pedantic, Ruff B/UP/SIM/C4/RUF, Clang Static Analyzer security checkers".into();
        p.clippy_groups.insert("pedantic".into());
        p.ruff_select = ["B", "UP", "SIM", "C4", "RUF"].map(String::from).to_vec();
        p.clang_checkers = ["security", "optin.cplusplus.UninitializedObject"].map(String::from).to_vec();
        p
    }

    pub fn builtins() -> Vec<Self> {
        vec![Self::default_profile(), Self::strict_profile()]
    }

    /// The analyzer's own switch (Clippy and cargo check separately).
    fn analyzer_toggle(&self, tool: Tool) -> Option<&Toggle> {
        self.analyzers.get(analyzer_key(tool))
    }

    /// Whether the analyzer runs. Clippy runs when Clippy or cargo check is on (cargo check
    /// alone runs `cargo check`).
    pub fn analyzer_enabled(&self, tool: Tool) -> bool {
        let on = |t: Tool| self.analyzer_toggle(t).is_none_or(|x| x.enabled);
        match tool {
            Tool::Clippy | Tool::CargoCheck => on(Tool::Clippy) || on(Tool::CargoCheck),
            t => on(t),
        }
    }

    /// `cargo clippy` rather than `cargo check`.
    pub fn clippy_enabled(&self) -> bool {
        self.analyzer_toggle(Tool::Clippy).is_none_or(|x| x.enabled)
    }

    pub fn rule(&self, tool: Tool, rule: &str) -> Option<&Toggle> {
        self.rules.get(&rule_key(tool, rule))
    }

    /// Apply the profile to a finding: `false` drops it (rule or analyzer off); otherwise its
    /// severity becomes the rule's, else the analyzer's, when set. `explicit`: the analyzer was
    /// asked for by name, so its own switch is ignored.
    pub fn apply(&self, f: &mut Finding, explicit: bool) -> bool {
        if !explicit && !self.analyzer_enabled(f.tool) {
            return false;
        }
        let rule = self.rule(f.tool, f.rule()).or_else(|| {
            // Ruff findings are also known by their full inspection name (`F401 unused-import`).
            (f.inspection != f.rule()).then(|| self.rule(f.tool, &f.inspection)).flatten()
        });
        if let Some(t) = rule {
            if !t.enabled {
                return false;
            }
            if let Some(level) = t.severity {
                f.severity = level.to_severity();
                return true;
            }
        }
        if let Some(level) = self.analyzer_toggle(f.tool).and_then(|t| t.severity) {
            f.severity = level.to_severity();
        }
        true
    }

    /// Rules of a family with a switch: (rule, toggle).
    fn rules_of<'a>(&'a self, fam: &'a str) -> impl Iterator<Item = (&'a str, &'a Toggle)> + 'a {
        self.rules.iter().filter_map(move |(k, t)| {
            let (f, rule) = k.split_once(':')?;
            (f == fam).then_some((rule, t))
        })
    }

    /// Flags after `cargo clippy --`: lint groups (`-W clippy::pedantic`), rules turned off
    /// (`-A lint`) and on (`-W lint`, for allow-by-default lints).
    pub fn clippy_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        for g in self.clippy_groups.iter().filter(|g| CLIPPY_GROUPS.contains(&g.as_str())) {
            out.extend(["-W".to_string(), format!("clippy::{g}")]);
        }
        for (rule, t) in self.rules_of("clippy") {
            if crate::inspections::clippy::is_lint_name(rule) {
                out.extend([if t.enabled { "-W" } else { "-A" }.to_string(), rule.to_string()]);
            }
        }
        out
    }

    /// Ruff flags: `--extend-select` (profile selectors and rules turned on), `--ignore`.
    pub fn ruff_args(&self) -> Vec<String> {
        let valid = |c: &str| !c.is_empty() && c.len() <= 16 && c.chars().all(|ch| ch.is_ascii_alphanumeric());
        let mut select: Vec<String> = self.ruff_select.iter().map(|s| s.trim().to_string()).filter(|s| valid(s)).collect();
        let mut ignore = Vec::new();
        for (rule, t) in self.rules_of("ruff") {
            let code = rule.split_whitespace().next().unwrap_or("");
            if !valid(code) {
                continue;
            }
            if t.enabled { select.push(code.to_string()) } else { ignore.push(code.to_string()) }
        }
        let mut out = Vec::new();
        if !select.is_empty() {
            out.push(format!("--extend-select={}", select.join(",")));
        }
        if !ignore.is_empty() {
            out.push(format!("--ignore={}", ignore.join(",")));
        }
        out
    }

    /// `clang --analyze` flags: checkers turned on and off.
    pub fn clang_analyzer_args(&self) -> Vec<String> {
        let valid = |c: &str| !c.is_empty() && c.len() <= 120 && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_');
        let mut out = Vec::new();
        let mut on: Vec<&str> = self.clang_checkers.iter().map(|c| c.trim()).filter(|c| valid(c)).collect();
        let mut off = Vec::new();
        for (rule, t) in self.rules_of("clang_analyzer") {
            if valid(rule) {
                if t.enabled { on.push(rule) } else { off.push(rule) }
            }
        }
        for c in on {
            out.extend(["-Xclang".to_string(), format!("-analyzer-checker={c}")]);
        }
        for c in off {
            out.extend(["-Xclang".to_string(), format!("-analyzer-disable-checker={c}")]);
        }
        out
    }

    /// clang-tidy `--checks`: the profile's, plus rules turned on (`check`) or off (`-check`);
    /// `None` leaves the project's `.clang-tidy`.
    pub fn clang_tidy_checks(&self) -> Option<String> {
        let valid = |c: &str| !c.is_empty() && c.len() <= 120 && c.chars().all(|ch| ch.is_ascii_alphanumeric() || "-_.*".contains(ch));
        let mut parts: Vec<String> =
            self.clang_tidy_checks.split(',').map(str::trim).filter(|c| valid(c.trim_start_matches('-'))).map(str::to_string).collect();
        for (rule, t) in self.rules_of("clang_tidy") {
            if valid(rule) {
                parts.push(if t.enabled { rule.to_string() } else { format!("-{rule}") });
            }
        }
        (!parts.is_empty()).then(|| parts.join(","))
    }

    /// cppcheck `--enable`.
    pub fn cppcheck_enable(&self) -> String {
        let e = self.cppcheck_enable.trim();
        if !e.is_empty() && e.chars().all(|c| c.is_ascii_lowercase() || c == ',') {
            e.to_string()
        } else {
            "warning,style,performance,portability".into()
        }
    }

    /// Hostile files: cap sizes and drop junk.
    fn sanitize(&mut self) {
        self.name = self.name.trim().chars().take(MAX_NAME).collect();
        if self.rules.len() > MAX_RULES {
            let keep: BTreeMap<String, Toggle> = std::mem::take(&mut self.rules).into_iter().take(MAX_RULES).collect();
            self.rules = keep;
        }
        self.ruff_select.truncate(200);
        self.clang_checkers.truncate(200);
        self.clang_tidy_checks = self.clang_tidy_checks.chars().take(4096).collect();
        self.cppcheck_enable = self.cppcheck_enable.chars().take(200).collect();
    }
}

// ---------------------------------------------------------------- storage

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfilesFile {
    #[serde(default)]
    format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<String>,
    #[serde(default)]
    profiles: Vec<InspectionProfile>,
    /// Commit tool window: Analyze code before committing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit_check: Option<bool>,
}

/// Where a profile is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileScope {
    Builtin,
    Ide,
    Project,
}

impl ProfileScope {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "project" => Some(Self::Project),
            "ide" | "global" | "application" => Some(Self::Ide),
            _ => None,
        }
    }
}

/// The profiles of the open project and the IDE.
#[derive(Debug, Default)]
pub struct Profiles {
    pub home: UserHome,
    /// The project whose file is loaded (reloaded when the project changes).
    loaded_for: Option<PathBuf>,
    ide_loaded: bool,
    pub project: Vec<InspectionProfile>,
    pub ide: Vec<InspectionProfile>,
    /// The active profile's name.
    pub active: String,
    /// Analyze code before committing (the Commit tool window's check).
    pub commit_check: bool,
    /// The last load or save problem (shown on the settings page).
    pub error: Option<String>,
}

impl Profiles {
    fn ide_file(&self) -> Option<PathBuf> {
        self.home.file(FILE_NAME)
    }

    /// A profile by name: project, then IDE, then built-in.
    pub fn find(&self, name: &str) -> Option<(InspectionProfile, ProfileScope)> {
        if let Some(p) = self.project.iter().find(|p| p.name == name) {
            return Some((p.clone(), ProfileScope::Project));
        }
        if let Some(p) = self.ide.iter().find(|p| p.name == name) {
            return Some((p.clone(), ProfileScope::Ide));
        }
        InspectionProfile::builtins().into_iter().find(|p| p.name == name).map(|p| (p, ProfileScope::Builtin))
    }

    /// Every profile name once (the highest-priority copy), built-ins first.
    pub fn list(&self) -> Vec<(InspectionProfile, ProfileScope, bool)> {
        let mut names: Vec<String> = InspectionProfile::builtins().into_iter().map(|p| p.name).collect();
        for p in self.ide.iter().chain(self.project.iter()) {
            if !names.contains(&p.name) {
                names.push(p.name.clone());
            }
        }
        names
            .into_iter()
            .filter_map(|n| {
                let (p, scope) = self.find(&n)?;
                let shadows_builtin = scope != ProfileScope::Builtin && InspectionProfile::builtins().iter().any(|b| b.name == n);
                Some((p, scope, shadows_builtin))
            })
            .collect()
    }
}

fn project_file(root: &Path) -> PathBuf {
    root.join(".codecraft").join(FILE_NAME)
}

fn read_file(path: &Path) -> Result<ProfilesFile, String> {
    let mut f: ProfilesFile = load_json(path)?.unwrap_or_default();
    f.profiles.truncate(MAX_PROFILES);
    for p in &mut f.profiles {
        p.sanitize();
    }
    f.profiles.retain(|p| !p.name.is_empty());
    Ok(f)
}

/// Load the project's and the IDE's profiles once (and again after the project changed).
pub fn ensure_loaded(s: &mut Session) {
    let root = s.project_root();
    let p = &mut s.analysis.profiles;
    if !p.ide_loaded {
        p.ide_loaded = true;
        if let Some(file) = p.ide_file() {
            match read_file(&file) {
                Ok(f) => p.ide = f.profiles,
                Err(e) => p.error = Some(e),
            }
        }
    }
    if p.loaded_for == root && !p.active.is_empty() {
        return;
    }
    p.loaded_for = root.clone();
    p.project.clear();
    p.active = DEFAULT.to_string();
    p.commit_check = false;
    if let Some(root) = root {
        match read_file(&project_file(&root)) {
            Ok(f) => {
                p.project = f.profiles;
                if let Some(a) = f.active.filter(|a| !a.trim().is_empty()) {
                    p.active = a;
                }
                p.commit_check = f.commit_check.unwrap_or(false);
            }
            Err(e) => p.error = Some(e),
        }
    }
    if p.find(&p.active).is_none() {
        p.active = DEFAULT.to_string();
    }
}

/// Write the project's file (and the IDE's when `ide`).
fn save(s: &mut Session, ide: bool) -> Result<(), String> {
    let p = &s.analysis.profiles;
    if let Some(root) = s.project_root() {
        let f = ProfilesFile {
            format: FORMAT.into(),
            active: Some(p.active.clone()).filter(|a| a != DEFAULT),
            profiles: p.project.clone(),
            commit_check: p.commit_check.then_some(true),
        };
        let path = project_file(&root);
        // Nothing to keep and no file yet: do not create one.
        if f.active.is_some() || !f.profiles.is_empty() || f.commit_check.is_some() || path.exists() {
            save_json(&path, &f, false)?;
        }
    }
    if ide {
        let file = p.ide_file().ok_or("cannot find the home directory; set CODECRAFT_HOME")?;
        save_json(&file, &ProfilesFile { format: FORMAT.into(), profiles: p.ide.clone(), ..Default::default() }, false)?;
    }
    Ok(())
}

/// The profile a run uses: `name`, else the project's active one.
pub fn resolve(s: &mut Session, name: Option<&str>) -> Result<InspectionProfile, String> {
    ensure_loaded(s);
    let p = &s.analysis.profiles;
    let name = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&p.active).to_string();
    p.find(&name).map(|(p, _)| p).ok_or_else(|| format!("no inspection profile named '{name}'"))
}

/// The commit check switch (loaded with the project's profiles).
pub fn commit_check(s: &mut Session) -> bool {
    ensure_loaded(s);
    s.analysis.profiles.commit_check
}

pub(crate) fn set_commit_check(s: &mut Session, on: bool) -> Result<(), String> {
    ensure_loaded(s);
    s.analysis.profiles.commit_check = on;
    save(s, false)
}

/// Edit a profile in place: the project or IDE copy, or a new project copy of a built-in.
fn edit(s: &mut Session, name: Option<&str>, f: impl FnOnce(&mut InspectionProfile)) -> Result<InspectionProfile, String> {
    ensure_loaded(s);
    let name = name.map(str::trim).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| s.analysis.profiles.active.clone());
    let (found, scope) =
        s.analysis.profiles.find(&name).ok_or_else(|| format!("no inspection profile named '{name}'"))?;
    let has_project = s.project_root().is_some();
    let store = &mut s.analysis.profiles;
    let (list, ide) = match scope {
        ProfileScope::Ide => (&mut store.ide, true),
        ProfileScope::Project => (&mut store.project, false),
        ProfileScope::Builtin => {
            if !has_project {
                return Err("open a project to change a built-in profile (or copy it to the IDE)".into());
            }
            store.project.push(found);
            (&mut store.project, false)
        }
    };
    let Some(profile) = list.iter_mut().rev().find(|p| p.name == name) else {
        return Err(format!("no inspection profile named '{name}'"));
    };
    f(profile);
    profile.sanitize();
    let out = profile.clone();
    save(s, ide)?;
    Ok(out)
}

fn valid_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > MAX_NAME || n.chars().any(char::is_control) {
        return Err(format!("a profile name is 1 to {MAX_NAME} printable characters"));
    }
    Ok(n.to_string())
}

// ---------------------------------------------------------------- the rules the settings page lists

/// Well-known rules per analyzer for the settings tree (names from the tools' public
/// documentation; descriptions are CodeCraft's own). Rules seen in results are added at run time.
pub fn known_rules(tool: Tool) -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> =
        super::KNOWN.iter().filter(|(_, t, _)| *t == tool).map(|(n, _, d)| (*n, *d)).collect();
    let extra: &[(&str, &str)] = match tool {
        Tool::RustAnalyzer => &[
            ("unresolved-module", "A `mod` without its file"),
            ("unresolved-import", "An import that does not resolve"),
            ("inactive-code", "Code disabled by `#[cfg]`"),
            ("unlinked-file", "A file no crate includes"),
            ("mismatched-arg-count", "A call with the wrong number of arguments"),
            ("missing-fields", "A struct literal without every field"),
            ("E0308", "Mismatched types"),
            ("E0425", "An unresolved name"),
        ],
        Tool::ClangAnalyzer => &[
            ("core.NullDereference", "Dereference of a null pointer"),
            ("core.DivideZero", "Division by zero"),
            ("core.uninitialized.Assign", "Assignment of an uninitialized value"),
            ("deadcode.DeadStores", "A value stored and never read"),
            ("unix.Malloc", "Memory leaks, double free, use after free"),
            ("cplusplus.NewDelete", "new/delete misuse"),
            ("security.insecureAPI.strcpy", "Unbounded string copies"),
            ("optin.cplusplus.UninitializedObject", "Fields left uninitialized by a constructor"),
        ],
        Tool::Clangd => &[
            ("-Wunused-variable", "An unused variable"),
            ("-Wunused-parameter", "An unused parameter"),
            ("-Wsign-compare", "Comparison of integers of different signs"),
            ("-Wshadow", "A declaration shadows another"),
            ("undeclared_var_use", "Use of an undeclared identifier"),
            ("pp_file_not_found", "An #include that cannot be found"),
        ],
        Tool::ClangTidy => &[
            ("bugprone-use-after-move", "Use of a moved-from object"),
            ("modernize-use-nullptr", "NULL or 0 instead of nullptr"),
            ("performance-unnecessary-copy-initialization", "A copy that could be a reference"),
            ("readability-identifier-naming", "Names against the naming conventions"),
        ],
        Tool::Cppcheck => &[
            ("nullPointer", "Null pointer dereference"),
            ("uninitvar", "An uninitialized variable is used"),
            ("memleak", "Memory leak"),
            ("arrayIndexOutOfBounds", "Array index out of bounds"),
            ("unusedVariable", "An unused variable"),
        ],
        Tool::Mypy => &[
            ("arg-type", "An argument of the wrong type"),
            ("assignment", "Incompatible types in an assignment"),
            ("import-not-found", "A module without stubs or source"),
            ("return-value", "A return value of the wrong type"),
        ],
        Tool::Pyright => &[
            ("reportMissingImports", "An import that cannot be resolved"),
            ("reportAttributeAccessIssue", "Access to an unknown attribute"),
            ("reportArgumentType", "An argument of the wrong type"),
            ("reportOptionalMemberAccess", "Member access on an optional value"),
        ],
        Tool::Ruff => &[
            ("E501", "Line too long"),
            ("B006", "Mutable default argument"),
            ("UP006", "Use `list` instead of `List` in annotations"),
            ("SIM108", "if/else that could be a ternary"),
        ],
        Tool::Sql => &[
            ("SqlResolve", "Unresolved table or column"),
            ("SqlWithoutWhere", "UPDATE or DELETE without WHERE"),
            ("SqlAmbiguousColumn", "A column that is in several tables"),
            ("SqlInsertValues", "INSERT with the wrong number of values"),
            ("SqlSelectStarInView", "SELECT * in a view"),
            ("SqlUnusedAlias", "An alias that is never used"),
            ("SqlUnusedCte", "A CTE that is never used"),
        ],
        _ => &[],
    };
    for e in extra {
        if !out.iter().any(|(n, _)| n == &e.0) {
            out.push(*e);
        }
    }
    out
}

/// The analyzers listed on the settings page, by language.
pub const ANALYZERS: [(&str, &[Tool]); 6] = [
    ("Rust", &[Tool::Clippy, Tool::CargoCheck, Tool::RustAnalyzer, Tool::CargoAudit, Tool::CargoDeny]),
    ("Python", &[Tool::Ruff, Tool::Mypy, Tool::Pyright]),
    ("C/C++", &[Tool::Clangd, Tool::ClangAnalyzer, Tool::ClangTidy, Tool::Cppcheck]),
    ("SQL", &[Tool::Sql]),
    ("JavaScript/TypeScript", &[Tool::Eslint]),
    ("Zig", &[Tool::ZigFmt]),
];

// ---------------------------------------------------------------- commands

use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::CommandError;
use crate::params::{opt_str, req_str};

fn tool_param(p: &Value) -> Result<Tool, CommandError> {
    let t = req_str(p, "analyzer")?;
    Tool::parse(t).ok_or_else(|| CommandError::BadParams(format!("unknown analyzer '{t}'")))
}

fn level_param(p: &Value) -> Result<Option<Option<Level>>, CommandError> {
    match p.get("severity") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) if s.is_empty() || s == "default" => Ok(Some(None)),
        Some(Value::String(s)) => Level::parse(s)
            .map(|l| Some(Some(l)))
            .ok_or_else(|| CommandError::BadParams(format!("unknown severity '{s}' (error, warning, weak_warning, info)"))),
        Some(v) => Err(CommandError::BadParams(format!("`severity` must be a string, not {v}"))),
    }
}

fn profile_json(p: &InspectionProfile, scope: ProfileScope, active: &str) -> Value {
    json!({
        "name": p.name,
        "scope": scope,
        "active": p.name == active,
        "description": p.description,
        "analyzers": Tool::ALL.iter().map(|t| json!({
            "id": t.id(),
            "label": t.label(),
            "language": t.language(),
            "enabled": p.analyzer_enabled(*t) && (*t != Tool::Clippy || p.clippy_enabled()),
            "severity": p.analyzer_toggle(*t).and_then(|x| x.severity),
        })).collect::<Vec<_>>(),
        "rules": p.rules,
        "clippyGroups": p.clippy_groups,
        "ruffSelect": p.ruff_select,
        "clangCheckers": p.clang_checkers,
        "clangTidyChecks": p.clang_tidy_checks,
        "cppcheckEnable": p.cppcheck_enable,
        "advisories": p.advisories,
    })
}

fn list_cmd(s: &mut Session, _: &Value) -> CmdResult {
    ensure_loaded(s);
    let pr = &s.analysis.profiles;
    let list: Vec<Value> = pr
        .list()
        .into_iter()
        .map(|(p, scope, modified)| {
            json!({ "name": p.name, "scope": scope, "description": p.description, "modified": modified, "active": p.name == pr.active })
        })
        .collect();
    Ok(json!({ "active": pr.active, "profiles": list, "commitCheck": pr.commit_check, "error": pr.error }))
}

fn get_cmd(s: &mut Session, p: &Value) -> CmdResult {
    ensure_loaded(s);
    let pr = &s.analysis.profiles;
    let name = opt_str(p, "name").unwrap_or(&pr.active).to_string();
    let (profile, scope) = pr.find(&name).ok_or_else(|| CommandError::Failed(format!("no inspection profile named '{name}'")))?;
    Ok(profile_json(&profile, scope, &pr.active))
}

fn select_cmd(s: &mut Session, p: &Value) -> CmdResult {
    ensure_loaded(s);
    let name = req_str(p, "name")?.trim().to_string();
    if s.analysis.profiles.find(&name).is_none() {
        return Err(CommandError::Failed(format!("no inspection profile named '{name}'")));
    }
    s.analysis.profiles.active = name.clone();
    save(s, false).map_err(CommandError::Failed)?;
    s.status = format!("Inspection profile: {name}");
    Ok(json!({ "active": name }))
}

fn copy_cmd(s: &mut Session, p: &Value) -> CmdResult {
    ensure_loaded(s);
    let from = opt_str(p, "from").map(str::to_string).unwrap_or_else(|| s.analysis.profiles.active.clone());
    let name = valid_name(req_str(p, "name")?).map_err(CommandError::BadParams)?;
    let scope = match opt_str(p, "scope") {
        None => ProfileScope::Project,
        Some(v) => ProfileScope::parse(v).ok_or_else(|| CommandError::BadParams(format!("unknown scope '{v}' (project, ide)")))?,
    };
    let (mut profile, _) =
        s.analysis.profiles.find(&from).ok_or_else(|| CommandError::Failed(format!("no inspection profile named '{from}'")))?;
    let pr = &mut s.analysis.profiles;
    let taken = match scope {
        ProfileScope::Ide => pr.ide.iter().any(|p| p.name == name),
        _ => pr.project.iter().any(|p| p.name == name),
    };
    if taken || InspectionProfile::builtins().iter().any(|b| b.name == name) {
        return Err(CommandError::Failed(format!("a profile named '{name}' already exists")));
    }
    if pr.project.len() + pr.ide.len() >= MAX_PROFILES {
        return Err(CommandError::Failed("too many inspection profiles".into()));
    }
    profile.name = name.clone();
    match scope {
        ProfileScope::Ide => pr.ide.push(profile),
        _ => {
            if s.project_root().is_none() {
                return Err(CommandError::Failed("no project is open (copy to the IDE instead)".into()));
            }
            s.analysis.profiles.project.push(profile);
        }
    }
    save(s, scope == ProfileScope::Ide).map_err(CommandError::Failed)?;
    Ok(json!({ "name": name, "scope": scope }))
}

fn delete_cmd(s: &mut Session, p: &Value) -> CmdResult {
    ensure_loaded(s);
    let name = req_str(p, "name")?.trim().to_string();
    let scope = opt_str(p, "scope").and_then(ProfileScope::parse);
    let pr = &mut s.analysis.profiles;
    let (removed, ide) = if scope != Some(ProfileScope::Ide) && pr.project.iter().any(|p| p.name == name) {
        pr.project.retain(|p| p.name != name);
        (true, false)
    } else if scope != Some(ProfileScope::Project) && pr.ide.iter().any(|p| p.name == name) {
        pr.ide.retain(|p| p.name != name);
        (true, true)
    } else {
        (false, false)
    };
    if !removed {
        let builtin = InspectionProfile::builtins().iter().any(|b| b.name == name);
        return Err(CommandError::Failed(if builtin {
            format!("'{name}' is built in and has no changes to reset")
        } else {
            format!("no inspection profile named '{name}'")
        }));
    }
    if pr.find(&pr.active).is_none() {
        pr.active = DEFAULT.to_string();
    }
    save(s, ide).map_err(CommandError::Failed)?;
    Ok(json!({ "deleted": name, "active": s.analysis.profiles.active }))
}

fn set_analyzer_cmd(s: &mut Session, p: &Value) -> CmdResult {
    let tool = tool_param(p)?;
    let enabled = p.get("enabled").and_then(Value::as_bool);
    let severity = level_param(p)?;
    let profile = edit(s, opt_str(p, "name"), |prof| {
        let t = prof.analyzers.entry(analyzer_key(tool).to_string()).or_default();
        if let Some(e) = enabled {
            t.enabled = e;
        }
        if let Some(sev) = severity {
            t.severity = sev;
        }
        if t == &Toggle::default() {
            prof.analyzers.remove(analyzer_key(tool));
        }
    })
    .map_err(CommandError::Failed)?;
    Ok(json!({ "profile": profile.name, "analyzer": tool.id(), "toggle": profile.analyzers.get(analyzer_key(tool)) }))
}

fn set_rule_cmd(s: &mut Session, p: &Value) -> CmdResult {
    let tool = tool_param(p)?;
    let rule = req_str(p, "rule")?.trim().to_string();
    if rule.is_empty() || rule.len() > 200 || rule.chars().any(char::is_control) {
        return Err(CommandError::BadParams("`rule` must be a rule id such as clippy::needless_return or F401".into()));
    }
    let enabled = p.get("enabled").and_then(Value::as_bool);
    let severity = level_param(p)?;
    let reset = p.get("reset").and_then(Value::as_bool).unwrap_or(false);
    let key = rule_key(tool, &rule);
    let profile = edit(s, opt_str(p, "name"), |prof| {
        if reset {
            prof.rules.remove(&key);
            return;
        }
        let t = prof.rules.entry(key.clone()).or_default();
        if let Some(e) = enabled {
            t.enabled = e;
        }
        if let Some(sev) = severity {
            t.severity = sev;
        }
    })
    .map_err(CommandError::Failed)?;
    Ok(json!({ "profile": profile.name, "rule": key, "toggle": profile.rules.get(&key) }))
}

fn string_list(v: &Value) -> Result<Vec<String>, CommandError> {
    match v {
        Value::String(s) => Ok(s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()),
        Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().map(str::to_string).ok_or_else(|| CommandError::BadParams("expected strings".into())))
            .collect(),
        Value::Null => Ok(Vec::new()),
        _ => Err(CommandError::BadParams("expected a list of strings".into())),
    }
}

fn set_option_cmd(s: &mut Session, p: &Value) -> CmdResult {
    let option = req_str(p, "option")?.to_string();
    let value = p.get("value").cloned().unwrap_or(Value::Null);
    // Security advisories (network) on or off (M15 EA).
    if option == "advisories" {
        let on = value.as_bool().ok_or_else(|| CommandError::BadParams("`advisories` takes true or false".into()))?;
        let profile = edit(s, opt_str(p, "name"), move |p| p.advisories = on).map_err(CommandError::Failed)?;
        let active = s.analysis.profiles.active.clone();
        let scope = s.analysis.profiles.find(&profile.name).map(|(_, sc)| sc).unwrap_or(ProfileScope::Project);
        return Ok(profile_json(&profile, scope, &active));
    }
    let list = string_list(&value)?;
    if list.len() > 200 {
        return Err(CommandError::BadParams("too many values".into()));
    }
    let apply: Box<dyn FnOnce(&mut InspectionProfile)> = match option.as_str() {
        "clippyGroups" | "clippy_groups" => {
            if let Some(bad) = list.iter().find(|g| !CLIPPY_GROUPS.contains(&g.as_str())) {
                return Err(CommandError::BadParams(format!("unknown Clippy group '{bad}' ({})", CLIPPY_GROUPS.join(", "))));
            }
            Box::new(move |p| p.clippy_groups = list.into_iter().collect())
        }
        "ruffSelect" | "ruff_select" => Box::new(move |p| p.ruff_select = list),
        "clangCheckers" | "clang_checkers" => Box::new(move |p| p.clang_checkers = list),
        "clangTidyChecks" | "clang_tidy_checks" => Box::new(move |p| p.clang_tidy_checks = list.join(",")),
        "cppcheckEnable" | "cppcheck_enable" => Box::new(move |p| p.cppcheck_enable = list.join(",")),
        "description" => {
            let d = value.as_str().unwrap_or("").chars().take(500).collect::<String>();
            Box::new(move |p| p.description = d)
        }
        other => {
            return Err(CommandError::BadParams(format!(
                "unknown option '{other}' (clippyGroups, ruffSelect, clangCheckers, clangTidyChecks, cppcheckEnable, advisories, description)"
            )));
        }
    };
    let profile = edit(s, opt_str(p, "name"), apply).map_err(CommandError::Failed)?;
    let active = s.analysis.profiles.active.clone();
    let scope = s.analysis.profiles.find(&profile.name).map(|(_, sc)| sc).unwrap_or(ProfileScope::Project);
    Ok(profile_json(&profile, scope, &active))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    let g = Context::Global;
    vec![
        spec(
            "analysis.profiles",
            "Inspection Profiles",
            None,
            "→ {active, profiles: [{name, scope: builtin|ide|project, description, modified, active}], commitCheck}",
            g,
            always,
            list_cmd,
        ),
        spec(
            "analysis.profile.get",
            "Inspection Profile",
            None,
            "{name?: string} → the profile (default: the active one): analyzers [{id, enabled, severity}], rules {\"analyzer:rule\": {enabled, severity}}, clippyGroups, ruffSelect, clangCheckers, clangTidyChecks, cppcheckEnable",
            g,
            always,
            get_cmd,
        ),
        spec("analysis.profile.select", "Select Inspection Profile", None, "{name: string} — the project's active profile", g, always, select_cmd),
        spec(
            "analysis.profile.copy",
            "Copy Inspection Profile",
            None,
            "{name: string, from?: string (default: the active one), scope?: \"project\"|\"ide\"} — a new profile from another",
            g,
            always,
            copy_cmd,
        ),
        spec(
            "analysis.profile.delete",
            "Delete Inspection Profile",
            None,
            "{name: string, scope?: \"project\"|\"ide\"} — delete a profile (for a built-in: reset its changes)",
            g,
            always,
            delete_cmd,
        ),
        spec(
            "analysis.profile.setAnalyzer",
            "Set Analyzer in Profile",
            None,
            "{analyzer: string, enabled?: bool, severity?: \"error\"|\"warning\"|\"weak_warning\"|\"info\"|null, name?: string} — turn an analyzer on/off or give all its problems one severity",
            g,
            always,
            set_analyzer_cmd,
        ),
        spec(
            "analysis.profile.setRule",
            "Set Inspection Rule",
            None,
            "{analyzer: string, rule: string, enabled?: bool, severity?: level|null, reset?: bool, name?: string} — one rule on/off and its severity (`clippy` + `clippy::unwrap_used`, `ruff` + `F401`, `clang_analyzer` + `core.NullDereference`)",
            g,
            always,
            set_rule_cmd,
        ),
        spec(
            "analysis.profile.setOption",
            "Set Inspection Profile Option",
            None,
            "{option: \"clippyGroups\"|\"ruffSelect\"|\"clangCheckers\"|\"clangTidyChecks\"|\"cppcheckEnable\"|\"advisories\"|\"description\", value: [string]|string|bool, name?: string} — `advisories: true` lets cargo audit / cargo deny fetch the RustSec advisory database (network)",
            g,
            always,
            set_option_cmd,
        ),
        spec(
            "settings.inspections",
            "Inspections…",
            None,
            "Open Settings › Editor › Inspections",
            g,
            always,
            |s, _| {
                s.execute("app.settings", &Value::Null)?;
                s.execute("settings.dialog.page", &json!({ "page": crate::settings::PAGE_INSPECTIONS }))?;
                Ok(json!({ "page": crate::settings::PAGE_INSPECTIONS }))
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use codecraft_text::Position;

    fn finding(tool: Tool, rule: &str, sev: Severity) -> Finding {
        let mut f = Finding::new(tool, rule, sev, "/a".into(), Position::new(0, 0));
        f.code = Some(rule.into());
        f
    }

    #[test]
    fn levels_map_to_severities() {
        for l in Level::ALL {
            assert_eq!(Level::of(l.to_severity()), l);
            assert_eq!(Level::parse(l.id()), Some(l));
            assert_eq!(Level::parse(l.label()), Some(l));
        }
        assert_eq!(Level::parse("loud"), None);
    }

    #[test]
    fn profiles_switch_rules_and_severities() {
        let mut p = InspectionProfile::default_profile();
        let mut f = finding(Tool::Clippy, "clippy::needless_return", Severity::Warning);
        assert!(p.apply(&mut f, false));
        p.rules.insert(rule_key(Tool::CargoCheck, "clippy::needless_return"), Toggle { enabled: true, severity: Some(Level::Error) });
        assert!(p.apply(&mut f, false));
        assert_eq!(f.severity, Severity::Error);
        p.rules.insert(rule_key(Tool::Clippy, "clippy::needless_return"), Toggle { enabled: false, severity: None });
        assert!(!p.apply(&mut f, false));
        // Analyzer severity for everything else it reports.
        p.analyzers.insert("ruff".into(), Toggle { enabled: true, severity: Some(Level::WeakWarning) });
        let mut r = finding(Tool::Ruff, "F401", Severity::Warning);
        r.inspection = "F401 unused-import".into();
        assert!(p.apply(&mut r, false));
        assert_eq!(r.severity, Severity::Info);
        // An analyzer turned off drops its findings unless it was asked for by name.
        p.analyzers.insert("ruff".into(), Toggle { enabled: false, severity: None });
        assert!(!p.apply(&mut r, false));
        assert!(p.apply(&mut r, true));
        assert!(!p.analyzer_enabled(Tool::Ruff));
        // Clippy off, cargo check on: the Rust block still runs (as cargo check).
        p.analyzers.insert("clippy".into(), Toggle { enabled: false, severity: None });
        assert!(p.analyzer_enabled(Tool::Clippy));
        assert!(!p.clippy_enabled());
        p.analyzers.insert("cargo_check".into(), Toggle { enabled: false, severity: None });
        assert!(!p.analyzer_enabled(Tool::CargoCheck));
    }

    #[test]
    fn analyzer_flags_from_the_profile() {
        let mut p = InspectionProfile::strict_profile();
        assert_eq!(p.clippy_args(), ["-W", "clippy::pedantic"]);
        p.rules.insert("clippy:clippy::unwrap_used".into(), Toggle::default());
        p.rules.insert("clippy:dead_code".into(), Toggle { enabled: false, severity: None });
        p.rules.insert("clippy:not a lint".into(), Toggle::default());
        assert_eq!(p.clippy_args(), ["-W", "clippy::pedantic", "-W", "clippy::unwrap_used", "-A", "dead_code"]);
        p.rules.insert("ruff:F401 unused-import".into(), Toggle { enabled: false, severity: None });
        p.rules.insert("ruff:E501".into(), Toggle::default());
        p.rules.insert("ruff:$(rm)".into(), Toggle::default());
        assert_eq!(p.ruff_args(), ["--extend-select=B,UP,SIM,C4,RUF,E501", "--ignore=F401"]);
        p.rules.insert("clang_analyzer:core.NullDereference".into(), Toggle { enabled: false, severity: None });
        let a = p.clang_analyzer_args();
        assert!(a.contains(&"-analyzer-checker=security".to_string()));
        assert!(a.contains(&"-analyzer-disable-checker=core.NullDereference".to_string()));
        assert_eq!(p.clang_tidy_checks(), None);
        p.clang_tidy_checks = "-*,bugprone-*".into();
        p.rules.insert("clang_tidy:modernize-use-nullptr".into(), Toggle::default());
        assert_eq!(p.clang_tidy_checks().as_deref(), Some("-*,bugprone-*,modernize-use-nullptr"));
        assert_eq!(p.cppcheck_enable(), "warning,style,performance,portability");
        p.cppcheck_enable = "all; rm".into();
        assert_eq!(p.cppcheck_enable(), "warning,style,performance,portability");
        assert!(InspectionProfile::default_profile().ruff_args().is_empty());
    }

    #[test]
    fn profiles_are_stored_per_project_and_ide() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(&tmp.path().join("home"));
        s.execute("file.open", &json!({ "path": root })).unwrap();
        let v = s.execute("analysis.profiles", &json!({})).unwrap();
        assert_eq!(v["active"], "Default");
        assert_eq!(v["profiles"].as_array().unwrap().len(), 2);
        // Changing a built-in makes a project copy.
        s.execute("analysis.profile.setRule", &json!({ "analyzer": "clippy", "rule": "clippy::unwrap_used", "enabled": true, "severity": "error" }))
            .unwrap();
        let file = root.join(".codecraft").join("inspection-profiles.json");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("clippy:clippy::unwrap_used"), "{text}");
        let v = s.execute("analysis.profiles", &json!({})).unwrap();
        assert_eq!(v["profiles"][0]["scope"], "project");
        assert_eq!(v["profiles"][0]["modified"], true);
        // An IDE profile, selected for the project.
        s.execute("analysis.profile.copy", &json!({ "from": "Strict", "name": "Team", "scope": "ide" })).unwrap();
        assert!(tmp.path().join("home").join("inspection-profiles.json").is_file());
        s.execute("analysis.profile.select", &json!({ "name": "Team" })).unwrap();
        s.execute("analysis.profile.setAnalyzer", &json!({ "analyzer": "rust_analyzer", "enabled": false })).unwrap();
        s.execute("analysis.profile.setOption", &json!({ "option": "clippyGroups", "value": ["pedantic", "nursery"] })).unwrap();
        assert!(s.execute("analysis.profile.setOption", &json!({ "option": "clippyGroups", "value": ["galaxy"] })).is_err());
        let p = resolve(&mut s, None).unwrap();
        assert_eq!(p.name, "Team");
        assert!(!p.analyzer_enabled(Tool::RustAnalyzer));
        assert_eq!(p.clippy_args(), ["-W", "clippy::nursery", "-W", "clippy::pedantic"]);
        // A fresh session reads both files back.
        let mut s2 = Session::new();
        s2.lsp_enabled = false;
        s2.isolate_home(&tmp.path().join("home"));
        s2.execute("file.open", &json!({ "path": root })).unwrap();
        let p = resolve(&mut s2, None).unwrap();
        assert_eq!((p.name.as_str(), p.analyzer_enabled(Tool::RustAnalyzer)), ("Team", false));
        assert_eq!(resolve(&mut s2, Some("Default")).unwrap().rule(Tool::Clippy, "clippy::unwrap_used").unwrap().severity, Some(Level::Error));
        // Reset the built-in, delete the IDE profile: back to Default.
        s2.execute("analysis.profile.delete", &json!({ "name": "Default" })).unwrap();
        assert!(s2.execute("analysis.profile.delete", &json!({ "name": "Default" })).is_err());
        s2.execute("analysis.profile.delete", &json!({ "name": "Team" })).unwrap();
        assert_eq!(s2.execute("analysis.profiles", &json!({})).unwrap()["active"], "Default");
        // Bad params.
        assert!(s2.execute("analysis.profile.copy", &json!({ "name": "  " })).is_err());
        assert!(s2.execute("analysis.profile.copy", &json!({ "name": "Strict" })).is_err());
        assert!(s2.execute("analysis.profile.setRule", &json!({ "analyzer": "nope", "rule": "x" })).is_err());
        assert!(s2.execute("analysis.profile.setRule", &json!({ "analyzer": "ruff", "rule": "F401", "severity": "loud" })).is_err());
        assert!(s2.execute("analysis.profile.select", &json!({ "name": "Missing" })).is_err());
        // A corrupt project file is reported, not fatal.
        std::fs::write(&file, "{ nope").unwrap();
        let mut s3 = Session::new();
        s3.lsp_enabled = false;
        s3.isolate_home(&tmp.path().join("home"));
        s3.execute("file.open", &json!({ "path": root })).unwrap();
        let v = s3.execute("analysis.profiles", &json!({})).unwrap();
        assert_eq!(v["active"], "Default");
        assert!(v["error"].as_str().unwrap().contains("corrupt"));
    }
}
