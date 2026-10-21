//! Pure translation at the JSON-RPC boundary, including URI object keys in WorkspaceEdit.
//! Source text and edits are opaque: strings inside them must never be rewritten.
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    ToServer,
    ToClient,
}

pub trait PathMapper: Send + Sync {
    fn translate(&self, message: &mut Value, direction: Direction);
}

#[derive(Clone, Debug)]
pub struct RootMapper {
    local: String,
    remote: String,
    external: Option<String>,
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_owned()
}

impl RootMapper {
    /// Roots must be absolute, canonical paths (resolve `~` on the host before constructing).
    pub fn new(local: &str, remote: &str) -> Self {
        Self { local: normalize(local), remote: normalize(remote), external: None }
    }

    /// Read-only cache prefix for sources outside the project (fetching belongs to the caller).
    pub fn with_external_cache(mut self, cache: &str) -> Self {
        self.external = Some(normalize(cache));
        self
    }

    pub fn map_path(&self, path: &str, direction: Direction) -> Option<String> {
        let path = normalize(path);
        // Never turn an untrusted URI containing traversal into a writable mirror path.
        if path.split('/').any(|p| p == ".." || p == ".") {
            return None;
        }
        let (from, to) = match direction {
            Direction::ToServer => (&self.local, &self.remote),
            Direction::ToClient => (&self.remote, &self.local),
        };
        if let Some(tail) = path.strip_prefix(from)
            && (tail.is_empty() || tail.starts_with('/'))
        {
            return Some(format!("{to}{tail}"));
        }
        if let Some(cache) = &self.external {
            match direction {
                Direction::ToClient if path.starts_with('/') => return Some(format!("{cache}{path}")),
                Direction::ToServer => {
                    if let Some(tail) = path.strip_prefix(cache)
                        && tail.starts_with('/')
                    {
                        return Some(tail.to_owned());
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn map_string(&self, text: &str, direction: Direction) -> Option<String> {
        if text.starts_with("file://") {
            let uri: lsp_types::Uri = text.parse().ok()?;
            let host = uri.authority().map_or("", |authority| authority.host().as_str());
            if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
                return None;
            }
            let bytes = crate::uri::percent_decode(uri.path().as_str())?;
            let decoded = String::from_utf8(bytes).ok()?;
            let path =
                if decoded.starts_with('/') && decoded.get(2..3) == Some(":") { decoded.get(1..)? } else { &decoded };
            let mapped = self.map_path(path, direction)?;
            let mut uri = if mapped.starts_with('/') { "file://".to_string() } else { "file:///".to_string() };
            crate::uri::percent_encode_into(mapped.as_bytes(), &mut uri);
            Some(uri)
        } else {
            self.map_path(text, direction)
        }
    }

    fn wire_string(&self, text: &str, direction: Direction) -> Option<String> {
        self.map_string(text, direction).or_else(|| {
            // An unmappable remote file URI must not fall through as a local filesystem path.
            (direction == Direction::ToClient && self.external.is_some() && text.starts_with("file://"))
                .then(|| "codecraft-unmapped:///invalid-remote-source".to_owned())
        })
    }

    fn walk(&self, value: &mut Value, direction: Direction, field: &str, depth: usize) {
        if depth > 128 {
            return;
        }
        // TextDocumentItem.text, TextEdit.newText, snippets and command-line text are content.
        if matches!(
            field,
            "text" | "newName" | "newText" | "insertText" | "label" | "documentation" | "detail" | "message"
        ) {
            return;
        }
        match value {
            Value::String(s) => {
                if let Some(mapped) = self.wire_string(s, direction) {
                    *s = mapped;
                } else if field == "title" {
                    let (from, to) = match direction {
                        Direction::ToServer => (&self.local, &self.remote),
                        Direction::ToClient => (&self.remote, &self.local),
                    };
                    *s = s.replace(from, to);
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.walk(item, direction, field, depth + 1);
                }
            }
            Value::Object(map) => {
                let old = std::mem::take(map);
                for (key, mut val) in old {
                    self.walk(&mut val, direction, &key, depth + 1);
                    let mapped =
                        if key.starts_with("file://") { self.wire_string(&key, direction).unwrap_or(key) } else { key };
                    map.insert(mapped, val);
                }
            }
            _ => {}
        }
    }
}
impl PathMapper for RootMapper {
    fn translate(&self, message: &mut Value, direction: Direction) {
        if direction == Direction::ToServer
            && message.get("method").and_then(Value::as_str) == Some("initialize")
            && let Some(params) = message.get_mut("params").and_then(Value::as_object_mut)
        {
            // A PID on the client machine cannot be monitored by the remote server.
            params.insert("processId".into(), Value::Null);
        }
        self.walk(message, direction, "", 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn every_protocol_shape_round_trips_without_rewriting_buffer_text() {
        let mapper = RootMapper::new("/mirror/naïve #", "/srv/项目");
        let mut value = json!({"rootUri":"file:///mirror/na%C3%AFve%20%23", "rootPath":"/mirror/naïve #",
            "workspaceFolders":[{"uri":"file:///mirror/na%C3%AFve%20%23/src/a.rs"}],
            "initializationOptions":{"cacheDir":"/mirror/naïve #/cache"},
            "textDocument":{"uri":"file:///mirror/na%C3%AFve%20%23/src/a.rs", "text":"file:///mirror/na%C3%AFve%20%23"},
            "changes":{"file:///mirror/na%C3%AFve%20%23/src/a.rs":[{"newText":"/mirror/naïve #/literal"}]},
            "documentChanges":[{"oldUri":"file:///mirror/na%C3%AFve%20%23/a","newUri":"file:///mirror/na%C3%AFve%20%23/b"}],
            "diagnostics":[{"relatedInformation":[{"location":{"uri":"file:///mirror/na%C3%AFve%20%23/b"}}]}],
            "watchers":[{"globPattern":"/mirror/naïve #/**/*.rs"}],"title":"Index /mirror/naïve #"});
        let original = value.clone();
        mapper.translate(&mut value, Direction::ToServer);
        assert_eq!(value["rootUri"], "file:///srv/%E9%A1%B9%E7%9B%AE");
        assert!(value["changes"].get("file:///srv/%E9%A1%B9%E7%9B%AE/src/a.rs").is_some());
        assert_eq!(value["textDocument"]["text"], original["textDocument"]["text"]);
        mapper.translate(&mut value, Direction::ToClient);
        assert_eq!(value, original);
    }
    #[test]
    fn rejected_remote_uris_cannot_become_local_file_operations() {
        let mapper = RootMapper::new("/mirror", "/srv/project").with_external_cache("/cache/sources");
        let mut edit =
            json!({"uri":"file:///srv/project/../../etc/passwd", "changes":{"file:///srv/project/%2E%2E/secret":[]}});
        mapper.translate(&mut edit, Direction::ToClient);
        let uri: lsp_types::Uri = edit["uri"].as_str().unwrap().parse().unwrap();
        assert!(crate::uri_to_path(&uri).is_none());
        assert!(edit["changes"].as_object().unwrap().keys().all(|key| !key.starts_with("file://")));
    }

    #[test]
    fn external_cache_and_localhost_uris_are_reversible() {
        let mapper = RootMapper::new("/mirror", "/srv/project").with_external_cache("/cache/sources");
        assert_eq!(
            mapper.map_string("file://localhost/srv/project/a.rs", Direction::ToClient).as_deref(),
            Some("file:///mirror/a.rs")
        );
        let outside = "file:///opt/rust/%E9%A1%B9%E7%9B%AE%20%23/lib.rs";
        let cached = mapper.map_string(outside, Direction::ToClient).unwrap();
        assert!(cached.starts_with("file:///cache/sources/opt/rust/"));
        assert_eq!(mapper.map_string(&cached, Direction::ToServer).as_deref(), Some(outside));
        assert!(mapper.map_string("file://untrusted/srv/project/a.rs", Direction::ToClient).is_none());
    }

    #[test]
    fn windows_separators_boundaries_and_traversal() {
        let m = RootMapper::new(r"C:\Users\é\project", "/srv/project");
        assert_eq!(
            m.map_path(r"C:\Users\é\project\src\a.rs", Direction::ToServer).as_deref(),
            Some("/srv/project/src/a.rs")
        );
        assert_eq!(
            m.map_string("file:///C:/Users/%C3%A9/project/a.rs", Direction::ToServer).as_deref(),
            Some("file:///srv/project/a.rs")
        );
        assert_eq!(
            m.map_string("file:///srv/project/a.rs", Direction::ToClient).as_deref(),
            Some("file:///C:/Users/%C3%A9/project/a.rs")
        );
        for bad in ["/srv/project2/a", "/srv/project/../secret", "/outside/a"] {
            assert!(m.map_path(bad, Direction::ToClient).is_none());
        }
        for bad in ["https://example.org/a", "file:///srv/project/%zz", "file:///srv/project/%2E%2E/secret"] {
            assert!(m.map_string(bad, Direction::ToClient).is_none());
        }
    }
}
