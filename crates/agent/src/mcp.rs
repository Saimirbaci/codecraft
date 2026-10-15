//! MCP servers handed to an agent (CodeCraft's own tools, through the IDE bridge), in each
//! backend's dialect: Claude Code's `--mcp-config` JSON, Codex `-c mcp_servers.…` overrides, and
//! ACP's `mcpServers` in `session/new` / `session/load`.

use std::path::PathBuf;

use serde_json::{Map, Value, json};

/// A stdio MCP server: the agent starts `command args…` and speaks MCP over its stdio.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpServer {
    /// The server's name as the agent shows it (`mcp__<name>__<tool>`).
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// A TOML basic string (a JSON string literal is one).
fn toml_string(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

/// A bare TOML key, or a quoted one.
fn toml_key(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        s.to_string()
    } else {
        toml_string(s)
    }
}

impl McpServer {
    /// Claude Code's `--mcp-config` value for `servers`.
    pub fn claude_config(servers: &[McpServer]) -> String {
        let mut map = Map::new();
        for s in servers {
            let env: Map<String, Value> = s.env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            map.insert(s.name.clone(), json!({ "type": "stdio", "command": s.command, "args": s.args, "env": env }));
        }
        json!({ "mcpServers": map }).to_string()
    }

    /// Codex config overrides (`-c key=value` pairs) declaring this server.
    pub fn codex_overrides(&self) -> Vec<String> {
        let key = format!("mcp_servers.{}", toml_key(&self.name));
        let args = format!("[{}]", self.args.iter().map(|a| toml_string(a)).collect::<Vec<_>>().join(", "));
        let mut out = vec![
            "-c".to_string(),
            format!("{key}.command={}", toml_string(&self.command.to_string_lossy())),
            "-c".to_string(),
            format!("{key}.args={args}"),
        ];
        if !self.env.is_empty() {
            let env = self.env.iter().map(|(k, v)| format!("{}={}", toml_key(k), toml_string(v))).collect::<Vec<_>>();
            out.push("-c".to_string());
            out.push(format!("{key}.env={{{}}}", env.join(", ")));
        }
        out
    }

    /// An ACP `McpServer` (stdio) object.
    pub fn acp_json(&self) -> Value {
        let env: Vec<Value> = self.env.iter().map(|(k, v)| json!({ "name": k, "value": v })).collect();
        json!({ "name": self.name, "command": self.command, "args": self.args, "env": env })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> McpServer {
        McpServer {
            name: "codecraft".into(),
            command: PathBuf::from("/Applications/Code Craft.app/codecraft"),
            args: vec!["--mcp-bridge".into(), "--port".into(), "4100".into(), "quote\"d".into()],
            env: vec![("A".into(), "1".into())],
        }
    }

    #[test]
    fn claude_config_is_json_with_stdio_servers() {
        let v: Value = serde_json::from_str(&McpServer::claude_config(&[server()])).unwrap();
        assert_eq!(v["mcpServers"]["codecraft"]["type"], "stdio");
        assert_eq!(v["mcpServers"]["codecraft"]["command"], "/Applications/Code Craft.app/codecraft");
        assert_eq!(v["mcpServers"]["codecraft"]["args"][3], "quote\"d");
        assert_eq!(v["mcpServers"]["codecraft"]["env"]["A"], "1");
    }

    #[test]
    fn codex_overrides_are_toml() {
        let o = server().codex_overrides();
        assert_eq!(o[0], "-c");
        assert_eq!(o[1], "mcp_servers.codecraft.command=\"/Applications/Code Craft.app/codecraft\"");
        assert_eq!(o[3], r#"mcp_servers.codecraft.args=["--mcp-bridge", "--port", "4100", "quote\"d"]"#);
        assert_eq!(o[5], r#"mcp_servers.codecraft.env={A="1"}"#);
        let odd = McpServer { name: "a b".into(), ..McpServer::default() };
        assert!(odd.codex_overrides()[1].starts_with("mcp_servers.\"a b\".command="));
    }

    #[test]
    fn acp_shape() {
        let v = server().acp_json();
        assert_eq!(v["name"], "codecraft");
        assert_eq!(v["env"][0], json!({ "name": "A", "value": "1" }));
        assert_eq!(v["args"][0], "--mcp-bridge");
    }
}
