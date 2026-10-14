//! Code › Code Completion › Call Inline Completion (`Alt+Shift+\`) and Insert Inline Proposal
//! (whole, word by word, line by line): a completion of the code at the caret from the
//! configured language model (the commit message provider: OpenRouter, Ollama, Claude or OpenAI),
//! shown as ghost text after the caret.
//!
//! The request runs on a worker thread (through the system `curl`, like commit messages). The
//! proposal stays while the caret is where it was asked for, or while what is typed matches its
//! beginning; Tab inserts it, Alt+Right its next word, Cmd+Right the rest of its line, Escape
//! dismisses it (the UI routes those keys only while a proposal is visible).

use crossbeam_channel::{Receiver, TryRecvError};
use serde_json::{Value, json};

use codecraft_text::{ChangeSet, Edit, EditGroup, SelectionSet};
use codecraft_workspace::EditorId;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, ok, spec};
use crate::commit_ai::{self, Provider};
use crate::{CommandError, Session};

/// Text before and after the caret sent with the request (chars).
const PREFIX_CHARS: usize = 4000;
const SUFFIX_CHARS: usize = 1500;
/// Longest proposal kept (chars).
const MAX_PROPOSAL: usize = 4000;

const SYSTEM: &str = "You are a code completion engine. Continue the code at <CURSOR>. Reply with only \
the text to insert there: no explanations, no code fences, and do not repeat text that is already \
before or after the cursor. Keep it short: finish the current statement or block (at most 8 lines).";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub editor: EditorId,
    /// Where it was asked for (char offset) and the document revision then.
    pub offset: usize,
    pub revision: u64,
    pub text: String,
}

struct Pending {
    editor: EditorId,
    offset: usize,
    revision: u64,
    rx: Receiver<Result<String, String>>,
}

#[derive(Default)]
pub struct InlineState {
    pub proposal: Option<Proposal>,
    pending: Option<Pending>,
    pub error: Option<String>,
}

impl InlineState {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
}

/// The request's user message: the file's language and name, and the code around the cursor.
pub fn prompt(language: &str, file: &str, prefix: &str, suffix: &str) -> String {
    format!("Language: {language}\nFile: {file}\n\n{prefix}<CURSOR>{suffix}")
}

/// The provider request for an inline completion (the commit-message request with this
/// feature's instructions).
pub fn build_request(cfg: &commit_ai::Config, user: &str) -> Result<commit_ai::Request, String> {
    let mut req = commit_ai::build_request(cfg, user)?;
    match cfg.provider {
        Provider::Claude => {
            req.body["system"] = Value::String(SYSTEM.into());
            req.body["max_tokens"] = json!(256);
        }
        _ => {
            if let Some(first) = req.body.get_mut("messages").and_then(|m| m.get_mut(0)) {
                first["content"] = Value::String(SYSTEM.into());
            }
        }
    }
    Ok(req)
}

/// The completion text in a provider's reply (whitespace kept; a fenced reply is unwrapped).
pub fn parse_reply(provider: Provider, raw: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(raw).map_err(|_| {
        let head: String = raw.trim().chars().take(200).collect();
        if head.is_empty() { "the server sent an empty reply".to_string() } else { format!("unexpected reply: {head}") }
    })?;
    if let Some(err) = v.get("error") {
        let msg = err.get("message").and_then(Value::as_str).or_else(|| err.as_str()).unwrap_or("request failed");
        return Err(format!("{}: {msg}", provider.label()));
    }
    let text = match provider {
        Provider::OpenRouter | Provider::OpenAi => v.pointer("/choices/0/message/content").and_then(Value::as_str).map(str::to_string),
        Provider::Ollama => v.pointer("/message/content").and_then(Value::as_str).map(str::to_string),
        Provider::Claude => v
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("")),
    }
    .ok_or_else(|| format!("{} sent no completion", provider.label()))?;
    let mut text = text.replace("\r\n", "\n");
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let body = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        text = body.trim_end().strip_suffix("```").unwrap_or(body).trim_end().to_string();
    }
    let text = text.replace("<CURSOR>", "");
    let text: String = text.trim_end().chars().take(MAX_PROPOSAL).collect();
    if text.trim().is_empty() { Err("no completion here".into()) } else { Ok(text) }
}

/// The part of the proposal still to insert for an editor, at which offset: the whole text at the
/// offset it was asked for, or the rest after what has been typed (or accepted) since.
pub fn visible(s: &Session, editor: EditorId) -> Option<(usize, String)> {
    let p = s.inline.proposal.as_ref().filter(|p| p.editor == editor)?;
    let e = s.ws.editor(editor)?;
    let d = s.ws.doc(e.doc)?;
    let head = e.selections.primary().head;
    if e.selections.len() > 1 || !e.selections.primary().is_empty() {
        return None;
    }
    if d.doc.revision() == p.revision {
        return (head == p.offset).then(|| (p.offset, p.text.clone()));
    }
    if head <= p.offset || head - p.offset > p.text.chars().count() {
        return None;
    }
    let typed = d.doc.slice(p.offset..head);
    let rest = p.text.strip_prefix(typed.as_str())?;
    (!rest.is_empty()).then(|| (head, rest.to_string()))
}

/// What the editor draws: the visible proposal's first line (with `…` when more lines follow).
pub fn ghost_text(s: &Session, editor: EditorId) -> Option<(usize, String)> {
    let (at, rest) = visible(s, editor)?;
    let mut lines = rest.split('\n');
    let first = lines.next().unwrap_or("");
    let more = lines.next().is_some();
    let shown = match (first.is_empty(), more) {
        (true, true) => "⏎ …".to_string(),
        (false, true) => format!("{first} …"),
        _ => first.to_string(),
    };
    Some((at, shown))
}

fn has_visible(s: &Session) -> Result<(), String> {
    let id = s.ws.active_editor().ok_or("no editor is open")?;
    visible(s, id).map(|_| ()).ok_or_else(|| "there is no inline proposal".into())
}

fn call(s: &mut Session, _: &Value) -> CmdResult {
    let (e, d) = s.ws.active().ok_or_else(|| CommandError::Failed("no editor is open".into()))?;
    let head = e.selections.primary().head;
    let text: Vec<char> = d.doc.text().chars().collect();
    let prefix: String = text.get(head.saturating_sub(PREFIX_CHARS)..head.min(text.len())).map(|c| c.iter().collect()).unwrap_or_default();
    let suffix: String = text.get(head.min(text.len())..(head + SUFFIX_CHARS).min(text.len())).map(|c| c.iter().collect()).unwrap_or_default();
    let file = d.path.as_deref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "untitled".into());
    let user = prompt(d.language.name(), &file, &prefix, &suffix);
    let (editor, revision) = (e.id, d.doc.revision());
    let cfg = commit_ai::Config::from_settings(&s.ws.settings).map_err(CommandError::Failed)?;
    let req = build_request(&cfg, &user).map_err(CommandError::Failed)?;
    let provider = cfg.provider;
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name("codecraft-inline-completion".into())
        .spawn(move || {
            let r = commit_ai::send(&req).and_then(|raw| parse_reply(provider, &raw));
            let _ = tx.send(r);
        })
        .map_err(|e| CommandError::Failed(format!("cannot start the request: {e}")))?;
    s.inline.proposal = None;
    s.inline.error = None;
    s.inline.pending = Some(Pending { editor, offset: head, revision, rx });
    s.status = format!("Inline completion from {}…", provider.label());
    Ok(json!({ "requested": true, "provider": provider.id() }))
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(p) = &s.inline.pending else { return false };
    let r = match p.rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => Err("the request stopped unexpectedly".into()),
    };
    let Some(p) = s.inline.pending.take() else { return true };
    match r {
        Ok(text) => {
            // Only if the caret and the text are still where the request was made.
            let still = s.ws.editor(p.editor).is_some_and(|e| {
                e.selections.primary().head == p.offset && s.ws.doc(e.doc).is_some_and(|d| d.doc.revision() == p.revision)
            });
            if still {
                s.inline.proposal = Some(Proposal { editor: p.editor, offset: p.offset, revision: p.revision, text });
                s.status = "Inline proposal: Tab to insert, ⌥→ word, ⌘→ line, Esc to dismiss".into();
            } else {
                s.status = "Inline proposal dropped: the editor changed".into();
            }
        }
        Err(e) => {
            s.status = format!("Inline completion: {e}");
            s.inline.error = Some(e);
        }
    }
    true
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    All,
    Word,
    Line,
}

/// The piece of `rest` to insert: everything, the next word (with the spaces before it, or one
/// symbol), or up to the end of the line (a leading newline and the next line).
fn piece(rest: &str, part: Part) -> String {
    match part {
        Part::All => rest.to_string(),
        Part::Word => {
            let chars: Vec<char> = rest.chars().collect();
            let mut i = chars.iter().take_while(|c| c.is_whitespace()).count();
            let word = |c: &char| c.is_alphanumeric() || *c == '_';
            if chars.get(i).is_some_and(word) {
                i += chars.get(i..).map(|r| r.iter().take_while(|c| word(c)).count()).unwrap_or(0);
            } else if i < chars.len() {
                i += 1;
            }
            chars.get(..i).map(|c| c.iter().collect()).unwrap_or_default()
        }
        Part::Line => match rest.strip_prefix('\n') {
            Some(after) => format!("\n{}", after.split('\n').next().unwrap_or("")),
            None => rest.split('\n').next().unwrap_or("").to_string(),
        },
    }
}

fn insert(s: &mut Session, part: Part) -> CmdResult {
    let id = s.ws.active_editor().ok_or_else(|| CommandError::Failed("no editor is open".into()))?;
    let (at, rest) = visible(s, id).ok_or_else(|| CommandError::Failed("there is no inline proposal".into()))?;
    let text = piece(&rest, part);
    if text.is_empty() {
        return ok();
    }
    let len = text.chars().count();
    s.edit_active(EditGroup::Standalone, |_, _, _| Edit {
        changes: ChangeSet::single(at, at, text.clone()),
        selections: SelectionSet::caret(at + len),
    })?;
    if part == Part::All || len >= rest.chars().count() {
        s.inline.proposal = None;
    }
    Ok(json!({ "inserted": text }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "code.codeCompletion.callInlineCompletion",
            "Call Inline Completion",
            None,
            "Ask the configured language model (Settings › commit message provider) for a completion at the caret, shown as ghost text",
            Context::Editor,
            has_editor,
            call,
        ),
        spec("code.codeCompletion.insertInlineProposal", "Insert Inline Proposal", None, "Insert the whole proposal (Tab)", Context::Editor, has_visible, |s, _| insert(s, Part::All)),
        spec("code.codeCompletion.insertInlineProposalsWord", "Insert Inline Proposal's Word", None, "Insert the proposal's next word", Context::Editor, has_visible, |s, _| insert(s, Part::Word)),
        spec("code.codeCompletion.insertInlineProposalsLine", "Insert Inline Proposal's Line", None, "Insert the rest of the proposal's line", Context::Editor, has_visible, |s, _| insert(s, Part::Line)),
        spec("editor.inlineCompletion.dismiss", "Dismiss Inline Proposal", None, "", Context::Global, always, |s, _| {
            s.inline.proposal = None;
            ok()
        }),
        spec("editor.inlineCompletion.state", "Inline Proposal", None, "→ {pending, visible: {offset, text}, error}", Context::Global, always, |s, _| {
            let shown = s.ws.active_editor().and_then(|id| visible(s, id));
            Ok(json!({
                "pending": s.inline.busy(),
                "visible": shown.map(|(o, t)| json!({ "offset": o, "text": t })),
                "error": s.inline.error,
            }))
        }),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// A one-shot 127.0.0.1 HTTP server answering like Ollama; returns its URL and the request it got.
    fn fake_ollama(reply: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = conn.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let len = text[..head_end]
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + len {
                        break;
                    }
                }
            }
            let body = json!({ "message": { "content": reply } }).to_string();
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            conn.write_all(resp.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf).to_string()
        });
        (url, h)
    }

    fn curl_available() -> bool {
        std::process::Command::new("curl").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    #[test]
    fn proposal_from_a_local_server_inserted_by_word_line_and_whole() {
        if !curl_available() {
            eprintln!("skipping: curl is not installed");
            return;
        }
        let (url, server) = fake_ollama("total + 1;\n    println!(\"{total}\");");
        let mut s = Session::new();
        s.ws.settings.commit_ai_provider = "ollama".into();
        s.ws.settings.ollama_url = url;
        let d = s.ws.add_document(Some("/tmp/x.rs".into()), "fn f(total: u32) {\n    let n = \n}\n");
        let e = s.ws.open_editor(d).unwrap();
        s.ws.set_selections(e, SelectionSet::caret(31));
        assert!(s.enabled("code.codeCompletion.insertInlineProposal").is_err());
        s.execute("code.codeCompletion.callInlineCompletion", &json!({})).unwrap();
        let start = std::time::Instant::now();
        while s.inline.busy() && start.elapsed().as_secs() < 30 {
            s.pump();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let request = server.join().unwrap();
        assert!(request.contains("POST /api/chat"), "{request}");
        assert!(request.contains("let n = <CURSOR>"), "{request}");
        assert_eq!(ghost_text(&s, e), Some((31, "total + 1; …".into())), "{:?}", s.inline.error);
        // Word, then the line, then the rest.
        s.execute("code.codeCompletion.insertInlineProposalsWord", &json!({})).unwrap();
        assert_eq!(s.ws.doc(d).unwrap().doc.line_text(1).unwrap(), "    let n = total");
        s.execute("code.codeCompletion.insertInlineProposalsLine", &json!({})).unwrap();
        assert_eq!(s.ws.doc(d).unwrap().doc.line_text(1).unwrap(), "    let n = total + 1;");
        assert_eq!(ghost_text(&s, e), Some((41, "⏎ …".into())));
        s.execute("code.codeCompletion.insertInlineProposal", &json!({})).unwrap();
        assert_eq!(s.ws.doc(d).unwrap().doc.text(), "fn f(total: u32) {\n    let n = total + 1;\n    println!(\"{total}\");\n}\n");
        assert!(s.inline.proposal.is_none());
    }

    #[test]
    fn typing_the_proposal_keeps_it_and_moving_hides_it() {
        let mut s = Session::new();
        let d = s.ws.add_document(None, "let a = ");
        let e = s.ws.open_editor(d).unwrap();
        s.ws.set_selections(e, SelectionSet::caret(8));
        let revision = s.ws.doc(d).unwrap().doc.revision();
        s.inline.proposal = Some(Proposal { editor: e, offset: 8, revision, text: "vec![1, 2];".into() });
        s.execute("editor.type", &json!({ "text": "v" })).unwrap();
        assert_eq!(visible(&s, e), Some((9, "ec![1, 2];".into())));
        s.execute("editor.type", &json!({ "text": "x" })).unwrap();
        assert_eq!(visible(&s, e), None);
        s.execute("editor.backspace", &json!({})).unwrap();
        s.execute("editor.left", &json!({})).unwrap();
        assert_eq!(visible(&s, e), None);
        s.execute("editor.inlineCompletion.dismiss", &json!({})).unwrap();
        assert!(s.inline.proposal.is_none());
    }

    #[test]
    fn replies_and_pieces() {
        assert_eq!(parse_reply(Provider::Ollama, r#"{"message":{"content":"```rust\nx + 1\n```"}}"#).unwrap(), "x + 1");
        assert_eq!(parse_reply(Provider::OpenAi, r#"{"choices":[{"message":{"content":"  y<CURSOR>"}}]}"#).unwrap(), "  y");
        assert!(parse_reply(Provider::Claude, r#"{"content":[{"text":"   "}]}"#).is_err());
        assert!(parse_reply(Provider::Claude, r#"{"error":{"message":"bad key"}}"#).unwrap_err().contains("bad key"));
        assert!(parse_reply(Provider::Ollama, "garbage").is_err());
        assert_eq!(piece("  foo.bar()", Part::Word), "  foo");
        assert_eq!(piece(".bar()", Part::Word), ".");
        assert_eq!(piece("a\nb\nc", Part::Line), "a");
        assert_eq!(piece("\nb\nc", Part::Line), "\nb");
        assert_eq!(piece("", Part::Word), "");
        let cfg = commit_ai::Config { provider: Provider::Claude, model: String::new(), ollama_url: String::new(), api_key: "k".into() };
        let req = build_request(&cfg, "u").unwrap();
        assert!(req.body["system"].as_str().unwrap().contains("code completion"));
        let cfg = commit_ai::Config { provider: Provider::OpenRouter, model: String::new(), ollama_url: String::new(), api_key: String::new() };
        if std::env::var_os("OPENROUTER_API_KEY").is_none() {
            assert!(build_request(&cfg, "u").is_err(), "no key, no request");
        }
    }
}
