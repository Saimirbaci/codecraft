//! The Services tool window's sections (Database results, Docker, HTTP requests). Tracks that
//! show something in Services call [`show`] with their section.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::params::req_str;
use crate::{CommandError, Session};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServicesSection {
    #[default]
    Database,
    Docker,
    Http,
}

impl ServicesSection {
    pub const ALL: [ServicesSection; 3] = [Self::Database, Self::Docker, Self::Http];

    pub fn title(self) -> &'static str {
        match self {
            Self::Database => "Database",
            Self::Docker => "Docker",
            Self::Http => "HTTP Requests",
        }
    }

    /// Stable id used in command params.
    pub fn key(self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Docker => "docker",
            Self::Http => "http",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.key() == key)
    }
}

#[derive(Debug, Default)]
pub struct ServicesState {
    pub section: ServicesSection,
}

/// Select `section` and open the Services tool window.
pub fn show(s: &mut Session, section: ServicesSection) {
    s.services.section = section;
    s.ws.layout.show(ToolWindowId::Services);
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![spec(
        "services.select",
        "Select Services Section",
        None,
        "{section: \"database\"|\"docker\"|\"http\"}",
        Context::Global,
        always,
        select,
    )]
}

fn select(s: &mut Session, p: &Value) -> CmdResult {
    let key = req_str(p, "section")?;
    let section = ServicesSection::from_key(key).ok_or_else(|| CommandError::BadParams(format!("unknown section {key}")))?;
    show(s, section);
    ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn select_switches_section_and_opens_services() {
        let mut s = Session::new();
        s.execute("services.select", &json!({ "section": "docker" })).unwrap();
        assert_eq!(s.services.section, ServicesSection::Docker);
        assert!(s.ws.layout.is_visible(ToolWindowId::Services));
        assert!(s.execute("services.select", &json!({ "section": "nope" })).is_err());
    }
}
