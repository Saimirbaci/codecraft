//! Specs for spec-driven development (standalone: std + serde only).
//!
//! A project keeps its specs in `specs/<feature>/`, three Markdown files per feature after Kiro's
//! layout: `requirements.md` (requirements with stable ids `REQ-n`, ideally in EARS form),
//! `design.md` and `tasks.md` (a checklist naming the requirements each task implements). Code
//! points back with `// spec: REQ-n` comments or `#[spec("REQ-n")]` attributes.
//!
//! This crate reads all of that without ever failing on bad content (problems come back as
//! [`Diagnostic`]s), links code to requirements, and says for each requirement whether it is
//! in sync: [`status`] compares its fingerprint with the one saved at the last sync
//! ([`Baseline`], kept in `.codecraft/spec-state.json`, private to this checkout). Nothing here
//! changes code or rewrites a spec file.

mod code;
mod edit;
mod feature;
mod hash;
mod index;
mod links;
mod markdown;
mod model;
mod requirements;
mod store;
mod tasks;

pub use code::{Symbol, Syntax, attach_symbols};
pub use edit::{add_requirement, edit_requirement, set_task_state};
pub use feature::{load_feature, load_features, parse_feature};
pub use hash::{fingerprint, fnv1a};
pub use index::{
    CodeChange, CodeChangeKind, CodeItem, Dangling, LinkIndex, LinkRef, SpecStatus, build_index, code_changes,
    code_items, requirement_key, status,
};
pub use links::{is_feature_name, mentions_spec, scan_links};
pub use model::{CodeLink, Diagnostic, EarsKind, Feature, Heading, LinkKind, Requirement, Severity, Task, TaskState};
pub use requirements::{RequirementsDoc, ears_kind, parse_requirements};
pub use store::{
    Baseline, SpecError, baseline_path, load_baseline, mark_synced, new_feature, next_requirement_number,
    save_baseline, slug,
};
pub use tasks::{parse_outline, parse_tasks, requirement_ids};

/// The directory (under the project root) that holds the features.
pub const SPEC_DIR: &str = "specs";
/// The spec files of a feature.
pub const REQUIREMENTS_FILE: &str = "requirements.md";
pub const DESIGN_FILE: &str = "design.md";
pub const TASKS_FILE: &str = "tasks.md";

/// Spec files larger than this are reported, not read.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
pub const MAX_FEATURES: usize = 200;
pub const MAX_REQUIREMENTS: usize = 2000;
pub const MAX_TASKS: usize = 5000;
pub const MAX_HEADINGS: usize = 200;
/// Longest requirement or task text kept.
pub const MAX_TEXT_CHARS: usize = 8000;
/// Code lines longer than this (minified bundles) are not scanned for references.
pub const MAX_LINE_BYTES: usize = 2000;
pub const MAX_LINKS_PER_FILE: usize = 500;
/// Longest item (lines) a reference is followed into.
pub const MAX_ITEM_LINES: usize = 3000;
/// Lines examined per file when fingerprinting the items references are attached to.
pub const MAX_SYMBOL_WORK: usize = 200_000;
/// Most dangling references listed.
pub const MAX_DANGLING: usize = 500;
