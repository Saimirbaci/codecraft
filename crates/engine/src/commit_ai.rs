//! Commit message generation with a language model: OpenRouter, Ollama (configurable URL),
//! Claude (Anthropic) or OpenAI.
//!
//! Request building and response parsing are pure and tested; the HTTP call goes through the
//! system `curl` (reading its configuration from stdin, so API keys never show up in the process
//! list) and always runs on a worker thread.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Longest diff (in chars) sent to the model.
pub const MAX_DIFF_CHARS: usize = 24_000;
const TIMEOUT_SECS: &str = "120";

const SYSTEM_PROMPT: &str = "You write Git commit messages. Reply with the commit message only, no \
commentary and no code fences. Use a concise imperative subject line of at most 72 characters; add a \
blank line and a short body only when the change needs explaining.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenRouter,
    Ollama,
    Claude,
    OpenAi,
}

impl Provider {
    pub const ALL: [Provider; 4] = [Provider::OpenRouter, Provider::Ollama, Provider::Claude, Provider::OpenAi];

    pub fn id(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter",
            Provider::Ollama => "ollama",
            Provider::Claude => "claude",
            Provider::OpenAi => "openai",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::OpenRouter => "OpenRouter",
            Provider::Ollama => "Ollama",
            Provider::Claude => "Claude",
            Provider::OpenAi => "OpenAI",
        }
    }

    pub fn from_id(id: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.id() == id.trim().to_ascii_lowercase())
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openai/gpt-4o-mini",
            Provider::Ollama => "llama3.2",
            Provider::Claude => "claude-haiku-5-5",
            Provider::OpenAi => "gpt-4o-mini",
        }
    }

    /// Environment variable holding the API key (`None`: no key needed).
    pub fn key_env(self) -> Option<&'static str> {
        match self {
            Provider::OpenRouter => Some("OPENROUTER_API_KEY"),
            Provider::Ollama => None,
            Provider::Claude => Some("ANTHROPIC_API_KEY"),
            Provider::OpenAi => Some("OPENAI_API_KEY"),
        }
    }
}

/// What the user configured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub provider: Provider,
    /// Empty = the provider's default.
    pub model: String,
    pub ollama_url: String,
    pub api_key: String,
}

impl Config {
    /// From the settings; an empty key falls back to the provider's environment variable.
    pub fn from_settings(st: &codecraft_workspace::Settings) -> Result<Config, String> {
        let provider = Provider::from_id(&st.commit_ai_provider)
            .ok_or_else(|| format!("unknown commit message provider `{}`", st.commit_ai_provider))?;
        let mut api_key = st.commit_ai_keys.get(provider.id()).map(|k| k.trim().to_string()).unwrap_or_default();
        if api_key.is_empty()
            && let Some(var) = provider.key_env()
        {
            api_key = std::env::var(var).unwrap_or_default().trim().to_string();
        }
        Ok(Config {
            provider,
            model: st.commit_ai_model.trim().to_string(),
            ollama_url: st.ollama_url.clone(),
            api_key,
        })
    }

    pub fn model(&self) -> &str {
        if self.model.is_empty() { self.provider.default_model() } else { &self.model }
    }
}

/// An HTTP POST.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

pub fn build_request(cfg: &Config, user: &str) -> Result<Request, String> {
    let model = cfg.model();
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    let key = cfg.api_key.trim();
    if key.is_empty()
        && let Some(var) = cfg.provider.key_env()
    {
        return Err(format!(
            "No {} API key: set it in Settings or the {var} environment variable",
            cfg.provider.label()
        ));
    }
    let chat = json!([{ "role": "system", "content": SYSTEM_PROMPT }, { "role": "user", "content": user }]);
    let (url, body) = match cfg.provider {
        Provider::OpenRouter | Provider::OpenAi => {
            headers.push(("Authorization".into(), format!("Bearer {key}")));
            let url = if cfg.provider == Provider::OpenAi {
                "https://api.openai.com/v1/chat/completions"
            } else {
                "https://openrouter.ai/api/v1/chat/completions"
            };
            (url.to_string(), json!({ "model": model, "messages": chat }))
        }
        Provider::Claude => {
            headers.push(("x-api-key".into(), key.to_string()));
            headers.push(("anthropic-version".into(), "2023-06-01".into()));
            (
                "https://api.anthropic.com/v1/messages".to_string(),
                json!({
                    "model": model,
                    "max_tokens": 512,
                    "system": SYSTEM_PROMPT,
                    "messages": [{ "role": "user", "content": user }],
                }),
            )
        }
        Provider::Ollama => {
            let base = cfg.ollama_url.trim().trim_end_matches('/');
            if base.is_empty() {
                return Err("Set the Ollama URL in Settings".into());
            }
            let base = if base.contains("://") { base.to_string() } else { format!("http://{base}") };
            (format!("{base}/api/chat"), json!({ "model": model, "messages": chat, "stream": false }))
        }
    };
    Ok(Request { url, headers, body })
}

/// The message in a provider's reply, or the provider's error.
pub fn parse_response(provider: Provider, raw: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(raw).map_err(|_| {
        let head: String = raw.trim().chars().take(200).collect();
        if head.is_empty() { "the server sent an empty reply".to_string() } else { format!("unexpected reply: {head}") }
    })?;
    if let Some(err) = v.get("error") {
        let msg = err.get("message").and_then(Value::as_str).or_else(|| err.as_str()).unwrap_or("request failed");
        return Err(format!("{}: {msg}", provider.label()));
    }
    let text = match provider {
        Provider::OpenRouter | Provider::OpenAi => {
            v.pointer("/choices/0/message/content").and_then(Value::as_str).map(str::to_string)
        }
        Provider::Ollama => v.pointer("/message/content").and_then(Value::as_str).map(str::to_string),
        Provider::Claude => v.get("content").and_then(Value::as_array).map(|parts| {
            parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("")
        }),
    };
    let message = clean_message(&text.ok_or_else(|| format!("{} sent no message", provider.label()))?);
    if message.is_empty() { Err(format!("{} sent an empty message", provider.label())) } else { Ok(message) }
}

/// Strip code fences, surrounding quotes and whitespace models like to add.
pub fn clean_message(raw: &str) -> String {
    let mut text = raw.trim();
    if let Some(rest) = text.strip_prefix("```") {
        // Drop the fence line (and its language tag) and a closing fence.
        let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        text = rest.trim_end().strip_suffix("```").unwrap_or(rest).trim();
    }
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text = text.get(1..text.len() - 1).unwrap_or(text).trim();
    }
    text.to_string()
}

/// The user message: changed files and their diff, cut to [`MAX_DIFF_CHARS`].
pub fn build_prompt(files: &[String], diff: &str) -> String {
    let mut diff_text: String = diff.chars().take(MAX_DIFF_CHARS).collect();
    if diff_text.len() < diff.len() {
        diff_text.push_str("\n… (diff truncated)");
    }
    format!("Write a commit message for these changes.\n\nChanged files:\n{}\n\nDiff:\n{diff_text}\n", files.join("\n"))
}

/// Quote a value for curl's `--config` format.
fn config_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The curl config for a request (kept off the command line: it carries the API key).
pub fn curl_config(req: &Request) -> String {
    let mut cfg = format!("url = {}\nrequest = \"POST\"\n", config_quote(&req.url));
    for (k, v) in &req.headers {
        cfg.push_str(&format!("header = {}\n", config_quote(&format!("{k}: {v}"))));
    }
    cfg.push_str(&format!("data = {}\n", config_quote(&req.body.to_string())));
    cfg
}

/// Send the request with curl and return the reply body. Blocks: call from a worker thread.
pub fn send(req: &Request) -> Result<String, String> {
    let mut child = Command::new("curl")
        .args(["--silent", "--show-error", "--max-time", TIMEOUT_SECS, "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(curl_config(req).as_bytes()).map_err(|e| format!("cannot send the request: {e}"))?;
    }
    let out = child.wait_with_output().map_err(|e| format!("curl failed: {e}"))?;
    if !out.status.success() && out.stdout.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        return Err(if err.is_empty() { "the request failed".into() } else { err.to_string() });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Ask the configured model for a commit message.
pub fn generate(cfg: &Config, files: &[String], diff: &str) -> Result<String, String> {
    let req = build_request(cfg, &build_prompt(files, diff))?;
    parse_response(cfg.provider, &send(&req)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(p: Provider) -> Config {
        Config { provider: p, model: String::new(), ollama_url: "http://gpu-box:11434/".into(), api_key: "k".into() }
    }

    #[test]
    fn requests_per_provider() {
        let r = build_request(&cfg(Provider::OpenAi), "hi").unwrap();
        assert_eq!(r.url, "https://api.openai.com/v1/chat/completions");
        assert!(r.headers.contains(&("Authorization".into(), "Bearer k".into())));
        assert_eq!(r.body["model"], "gpt-4o-mini");

        let r = build_request(&cfg(Provider::OpenRouter), "hi").unwrap();
        assert!(r.url.starts_with("https://openrouter.ai/"));

        let r = build_request(&cfg(Provider::Claude), "hi").unwrap();
        assert!(r.headers.contains(&("x-api-key".into(), "k".into())));
        assert!(r.body.get("system").is_some() && r.body["max_tokens"] == 512);

        let r = build_request(&cfg(Provider::Ollama), "hi").unwrap();
        assert_eq!(r.url, "http://gpu-box:11434/api/chat");
        assert_eq!(r.body["stream"], false);
    }

    #[test]
    fn ollama_needs_no_key_but_a_url_and_adds_a_scheme() {
        let mut c = cfg(Provider::Ollama);
        c.api_key.clear();
        c.ollama_url = "gpu-box:11434".into();
        assert_eq!(build_request(&c, "x").unwrap().url, "http://gpu-box:11434/api/chat");
        c.ollama_url = "  ".into();
        assert!(build_request(&c, "x").is_err());
    }

    #[test]
    fn missing_key_is_an_error_naming_the_variable() {
        let mut c = cfg(Provider::Claude);
        c.api_key.clear();
        assert!(build_request(&c, "x").unwrap_err().contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn parses_replies() {
        let openai = r#"{"choices":[{"message":{"content":"  Fix the bug\n"}}]}"#;
        assert_eq!(parse_response(Provider::OpenAi, openai).unwrap(), "Fix the bug");
        let claude = r#"{"content":[{"type":"text","text":"Add "},{"type":"text","text":"feature"}]}"#;
        assert_eq!(parse_response(Provider::Claude, claude).unwrap(), "Add feature");
        let ollama = r#"{"message":{"role":"assistant","content":"```\nAdd x\n```"}}"#;
        assert_eq!(parse_response(Provider::Ollama, ollama).unwrap(), "Add x");
    }

    #[test]
    fn parses_errors_and_garbage() {
        let e = parse_response(Provider::OpenAi, r#"{"error":{"message":"bad key"}}"#).unwrap_err();
        assert_eq!(e, "OpenAI: bad key");
        assert!(parse_response(Provider::Ollama, r#"{"error":"model not found"}"#).unwrap_err().contains("not found"));
        assert!(parse_response(Provider::Claude, "<html>").unwrap_err().contains("unexpected reply"));
        assert!(parse_response(Provider::Claude, "").is_err());
        assert!(parse_response(Provider::OpenAi, r#"{"choices":[]}"#).is_err());
    }

    #[test]
    fn cleans_messages() {
        assert_eq!(clean_message("```text\nFix it\n\nBody\n```"), "Fix it\n\nBody");
        assert_eq!(clean_message("\"Fix it\""), "Fix it");
        assert_eq!(clean_message("\""), "\"");
        assert_eq!(clean_message("```"), "");
    }

    #[test]
    fn prompt_truncates_on_char_boundaries() {
        let diff = "é".repeat(MAX_DIFF_CHARS + 10);
        let p = build_prompt(&["a.rs".into()], &diff);
        assert!(p.contains("diff truncated") && p.contains("a.rs"));
    }

    #[test]
    fn curl_config_escapes_and_hides_the_key_in_stdin_only() {
        let mut r = build_request(&cfg(Provider::OpenAi), "say \"hi\"\nthere").unwrap();
        r.body = json!({ "a": "q\"\\\n" });
        let c = curl_config(&r);
        assert!(c.contains("header = \"Authorization: Bearer k\""));
        assert!(c.contains(r#"data = "{\"a\":\"q\\\"\\\\\\n\"}""#), "{c}");
        assert_eq!(c.lines().count(), 5);
    }

    #[test]
    fn provider_ids_round_trip() {
        for p in Provider::ALL {
            assert_eq!(Provider::from_id(p.id()), Some(p));
        }
        assert_eq!(Provider::from_id(" Claude "), Some(Provider::Claude));
        assert_eq!(Provider::from_id("x"), None);
    }

    #[test]
    fn config_from_settings_prefers_the_stored_key() {
        let mut st = codecraft_workspace::Settings { commit_ai_provider: "openai".into(), ..Default::default() };
        st.commit_ai_keys.insert("openai".into(), " sk-1 ".into());
        let c = Config::from_settings(&st).unwrap();
        assert_eq!((c.api_key.as_str(), c.model()), ("sk-1", "gpt-4o-mini"));
        st.commit_ai_provider = "nope".into();
        assert!(Config::from_settings(&st).is_err());
    }
}
