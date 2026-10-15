//! Code › Modify Selected Code… (Cmd+\, M13 track RC): a prompt over the selection (or the
//! caret's line); the selected code and the instruction go to the configured language model (the
//! inline-completion / commit-message provider: OpenRouter, Ollama, Claude or OpenAI) on a worker
//! thread, and its rewrite is shown as a diff to accept (one undo step) or reject. Nothing in the
//! file changes until Accept; if the code under the selection changed meanwhile, Accept refuses.

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_agent::diff::{DiffLine, LineTag};
use codecraft_text::{ChangeSet, Edit, EditGroup, SelectionSet};
use codecraft_workspace::{DocId, EditorId};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, ok, spec};
use crate::commit_ai::{self, Provider};
use crate::params::opt_str;
use crate::{CommandError, Session};

/// Largest selection sent (chars).
const MAX_SELECTION: usize = 30_000;
/// Context sent before and after the selection (chars).
const CONTEXT_CHARS: usize = 2_000;
/// Longest rewrite kept (chars).
const MAX_REPLY: usize = 120_000;
const MAX_INSTRUCTION: usize = 4_000;

const SYSTEM: &str = "You are a code editing engine inside an IDE. You get a code fragment, the code around it and \
an instruction. Rewrite only the fragment as instructed. Reply with only the new fragment: no explanations, no \
code fences, no text before or after it. Keep its indentation and style, and keep everything the instruction \
doesn't ask to change.";

/// The prompt popup over the selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModifyPrompt {
    pub editor: EditorId,
    /// The char range being modified.
    pub start: usize,
    pub end: usize,
    pub instruction: String,
}

/// A rewrite waiting for Accept/Reject.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModifyProposal {
    pub editor: EditorId,
    pub doc: DocId,
    pub start: usize,
    pub end: usize,
    pub instruction: String,
    pub original: String,
    pub replacement: String,
    /// Line diff of `original` → `replacement` (every line, with its tag).
    pub diff: Vec<DiffLine>,
    pub added: usize,
    pub removed: usize,
}

struct Pending {
    editor: EditorId,
    doc: DocId,
    start: usize,
    end: usize,
    instruction: String,
    original: String,
    rx: Receiver<Result<String, String>>,
}

#[derive(Default)]
pub struct ModifyState {
    pub prompt: Option<ModifyPrompt>,
    pub proposal: Option<ModifyProposal>,
    pub error: Option<String>,
    pending: Option<Pending>,
}

impl ModifyState {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Something is shown over the editor (prompt, waiting, or a proposal).
    pub fn active(&self) -> bool {
        self.prompt.is_some() || self.pending.is_some() || self.proposal.is_some()
    }
}

/// The user message: the language, the file, the code around and the fragment.
pub fn build_prompt(
    language: &str,
    file: &str,
    before: &str,
    fragment: &str,
    after: &str,
    instruction: &str,
) -> String {
    format!(
        "Language: {language}\nFile: {file}\n\nInstruction: {instruction}\n\nCode before the fragment:\n{before}\n\n\
         <FRAGMENT>\n{fragment}\n</FRAGMENT>\n\nCode after the fragment:\n{after}\n\nReply with the rewritten fragment only."
    )
}

/// The provider request (the commit-message request with this feature's instructions).
pub fn build_request(cfg: &commit_ai::Config, user: &str) -> Result<commit_ai::Request, String> {
    let mut req = commit_ai::build_request(cfg, user)?;
    match cfg.provider {
        Provider::Claude => {
            req.body["system"] = Value::String(SYSTEM.into());
            req.body["max_tokens"] = json!(8192);
        }
        _ => {
            if let Some(first) = req.body.get_mut("messages").and_then(|m| m.get_mut(0)) {
                first["content"] = Value::String(SYSTEM.into());
            }
        }
    }
    Ok(req)
}

/// The rewrite in a provider's reply: fences and `<FRAGMENT>` tags removed; a trailing line
/// break follows the original's.
pub fn parse_reply(provider: Provider, raw: &str, original: &str) -> Result<String, String> {
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
    }
    .ok_or_else(|| format!("{} sent no code", provider.label()))?;
    let mut text = text.replace("\r\n", "\n");
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let body = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        text = body.trim_end().strip_suffix("```").unwrap_or(body).to_string();
    }
    let text = text
        .replace("<FRAGMENT>\n", "")
        .replace("\n</FRAGMENT>", "")
        .replace("<FRAGMENT>", "")
        .replace("</FRAGMENT>", "");
    // Leading blank lines are the model's; the fragment's own indentation stays.
    let text = text.trim_start_matches('\n').trim_end().to_string();
    if text.trim().is_empty() {
        return Err("the model sent an empty rewrite".into());
    }
    let mut text: String = text.chars().take(MAX_REPLY).collect();
    if original.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// The proposal for `original` → `replacement`.
pub fn proposal(
    editor: EditorId,
    doc: DocId,
    (start, end): (usize, usize),
    instruction: String,
    original: String,
    replacement: String,
) -> ModifyProposal {
    let diff: Vec<DiffLine> = codecraft_agent::diff::diff(&original, &replacement, usize::MAX / 4)
        .into_iter()
        .flat_map(|h| h.lines)
        .take(20_000)
        .collect();
    let added = diff.iter().filter(|l| l.tag == LineTag::Insert).count();
    let removed = diff.iter().filter(|l| l.tag == LineTag::Delete).count();
    ModifyProposal { editor, doc, start, end, instruction, original, replacement, diff, added, removed }
}

/// The range to modify: the selection, else the caret's line (with its line break).
fn target_range(s: &Session) -> Result<(EditorId, usize, usize), CommandError> {
    let (e, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
    let sel = e.selections.primary();
    let (start, end) = if sel.is_empty() {
        let line = d.doc.char_to_line(sel.head);
        let start = d.doc.line_start(line);
        let end = (start + d.doc.line_len(line) + 1).min(d.doc.len_chars());
        (start, end)
    } else {
        (sel.start(), sel.end())
    };
    if start >= end {
        return Err(CommandError::Failed("select the code to modify".into()));
    }
    if end - start > MAX_SELECTION {
        return Err(CommandError::Failed(format!("the selection is too long (at most {MAX_SELECTION} characters)")));
    }
    Ok((e.id, start, end))
}

fn open(s: &mut Session, p: &Value) -> CmdResult {
    let (editor, start, end) = target_range(s)?;
    s.modify.proposal = None;
    s.modify.error = None;
    s.modify.prompt = Some(ModifyPrompt { editor, start, end, instruction: String::new() });
    if let Some(instruction) = opt_str(p, "instruction").filter(|i| !i.trim().is_empty()) {
        return submit(s, &json!({ "instruction": instruction }));
    }
    Ok(json!({ "start": start, "end": end }))
}

fn submit(s: &mut Session, p: &Value) -> CmdResult {
    let prompt =
        s.modify.prompt.clone().ok_or_else(|| CommandError::Failed("Modify Selected Code is not open".into()))?;
    let instruction = opt_str(p, "instruction").map(str::to_string).unwrap_or_else(|| prompt.instruction.clone());
    let instruction = instruction.trim().to_string();
    if instruction.is_empty() {
        return Err(CommandError::BadParams("say how to modify the code".into()));
    }
    if instruction.chars().count() > MAX_INSTRUCTION {
        return Err(CommandError::BadParams("the instruction is too long".into()));
    }
    let e = s.ws.editor(prompt.editor).ok_or_else(|| CommandError::Failed("the editor was closed".into()))?;
    let doc_id = e.doc;
    let d = s.ws.doc(doc_id).ok_or_else(|| CommandError::Failed("the editor was closed".into()))?;
    let len = d.doc.len_chars();
    let (start, end) = (prompt.start.min(len), prompt.end.min(len));
    let original = d.doc.slice(start..end);
    let before = d.doc.slice(start.saturating_sub(CONTEXT_CHARS)..start);
    let after = d.doc.slice(end..end.saturating_add(CONTEXT_CHARS).min(len));
    let file = d
        .path
        .as_deref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "untitled".into());
    let user = build_prompt(d.language.name(), &file, &before, &original, &after, &instruction);
    let cfg = commit_ai::Config::from_settings(&s.ws.settings).map_err(CommandError::Failed)?;
    let req = build_request(&cfg, &user).map_err(CommandError::Failed)?;
    let provider = cfg.provider;
    let (tx, rx) = crossbeam_channel::bounded(1);
    let original_for_reply = original.clone();
    std::thread::Builder::new()
        .name("codecraft-modify-code".into())
        .spawn(move || {
            let r = commit_ai::send(&req).and_then(|raw| parse_reply(provider, &raw, &original_for_reply));
            let _ = tx.send(r);
        })
        .map_err(|e| CommandError::Failed(format!("cannot start the request: {e}")))?;
    if let Some(pr) = &mut s.modify.prompt {
        pr.instruction = instruction.clone();
    }
    s.modify.error = None;
    s.modify.proposal = None;
    s.modify.pending = Some(Pending { editor: prompt.editor, doc: doc_id, start, end, instruction, original, rx });
    s.status = format!("Modify Selected Code: asking {}…", provider.label());
    Ok(json!({ "requested": true, "provider": provider.id() }))
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(p) = &s.modify.pending else { return false };
    let r = match p.rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => Err("the request stopped unexpectedly".into()),
    };
    let Some(p) = s.modify.pending.take() else { return true };
    match r {
        Ok(replacement) => {
            let pr = proposal(p.editor, p.doc, (p.start, p.end), p.instruction, p.original, replacement);
            s.status = if pr.added == 0 && pr.removed == 0 {
                "Modify Selected Code: no changes proposed".into()
            } else {
                format!("Modify Selected Code: +{} −{} lines — Accept or Reject", pr.added, pr.removed)
            };
            s.modify.prompt = None;
            s.modify.proposal = Some(pr);
        }
        Err(e) => {
            s.status = format!("Modify Selected Code: {e}");
            s.modify.error = Some(e);
        }
    }
    true
}

fn accept(s: &mut Session, _: &Value) -> CmdResult {
    let pr = s.modify.proposal.clone().ok_or_else(|| CommandError::Failed("there is no proposal".into()))?;
    let current = s.ws.doc(pr.doc).filter(|d| pr.end <= d.doc.len_chars()).map(|d| d.doc.slice(pr.start..pr.end));
    if current.as_deref() != Some(pr.original.as_str()) {
        s.modify.proposal = None;
        return Err(CommandError::Failed("the code changed since the request; ask again".into()));
    }
    if s.ws.active_editor() != Some(pr.editor) && !s.ws.set_active_editor(pr.editor) {
        s.modify.proposal = None;
        return Err(CommandError::Failed("the editor was closed".into()));
    }
    let len = pr.replacement.chars().count();
    s.edit_active(EditGroup::Standalone, |_, _, _| Edit {
        changes: ChangeSet::single(pr.start, pr.end, pr.replacement.clone()),
        selections: SelectionSet::single(codecraft_text::Selection::new(pr.start, pr.start + len)),
    })?;
    s.modify.proposal = None;
    s.status = format!("Modified: +{} −{} lines", pr.added, pr.removed);
    Ok(json!({ "added": pr.added, "removed": pr.removed }))
}

fn has_proposal(s: &Session) -> Result<(), String> {
    s.modify.proposal.as_ref().map(|_| ()).ok_or_else(|| "there is no proposal".into())
}

fn state(s: &Session) -> Value {
    json!({
        "prompt": s.modify.prompt,
        "pending": s.modify.busy(),
        "proposal": s.modify.proposal,
        "error": s.modify.error,
    })
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "code.modifySelectedCode",
            "Modify Selected Code…",
            Some("Cmd+\\"),
            "{instruction?: string} — open a prompt over the selection (or the caret's line); with `instruction`, \
             send it at once to the configured AI provider (Settings › commit message provider). The rewrite comes \
             back as a diff: editor.modifyCode.accept / reject. Poll editor.modifyCode.state.",
            Context::Editor,
            has_editor,
            open,
        ),
        spec(
            "editor.modifyCode.setInstruction",
            "Modify Code: Instruction",
            None,
            "{text: string} — the prompt's text",
            Context::Global,
            |s| s.modify.prompt.as_ref().map(|_| ()).ok_or_else(|| "Modify Selected Code is not open".into()),
            |s, p| {
                let text: String = opt_str(p, "text").unwrap_or("").chars().take(MAX_INSTRUCTION).collect();
                if let Some(pr) = &mut s.modify.prompt {
                    pr.instruction = text;
                }
                ok()
            },
        ),
        spec(
            "editor.modifyCode.submit",
            "Modify Code: Send",
            None,
            "{instruction?: string} — send the selection and the instruction to the AI provider",
            Context::Global,
            |s| s.modify.prompt.as_ref().map(|_| ()).ok_or_else(|| "Modify Selected Code is not open".into()),
            submit,
        ),
        spec(
            "editor.modifyCode.accept",
            "Modify Code: Accept",
            None,
            "Replace the code with the proposal (one undo step)",
            Context::Global,
            has_proposal,
            accept,
        ),
        spec(
            "editor.modifyCode.reject",
            "Modify Code: Reject",
            None,
            "Drop the proposal",
            Context::Global,
            has_proposal,
            |s, _| {
                s.modify.proposal = None;
                s.status = "Proposal rejected".into();
                ok()
            },
        ),
        spec(
            "editor.modifyCode.cancel",
            "Modify Code: Close",
            None,
            "Close the prompt, drop a pending request or proposal",
            Context::Global,
            always,
            |s, _| {
                s.modify.prompt = None;
                s.modify.pending = None;
                s.modify.proposal = None;
                ok()
            },
        ),
        spec(
            "editor.modifyCode.state",
            "Modify Code: State",
            None,
            "→ {prompt, pending, proposal: {original, replacement, diff, added, removed}, error}",
            Context::Global,
            always,
            |s, _| Ok(state(s)),
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn replies_are_unwrapped() {
        let raw = r#"{"message":{"content":"```rust\n    let x = 2;\n```"}}"#;
        assert_eq!(parse_reply(Provider::Ollama, raw, "    let x = 1;\n").unwrap(), "    let x = 2;\n");
        let raw = r#"{"choices":[{"message":{"content":"<FRAGMENT>\nfoo()\n</FRAGMENT>"}}]}"#;
        assert_eq!(parse_reply(Provider::OpenAi, raw, "bar()").unwrap(), "foo()");
        assert!(parse_reply(Provider::Claude, r#"{"content":[{"text":"  "}]}"#, "x").is_err());
        assert!(
            parse_reply(Provider::Claude, r#"{"error":{"message":"bad key"}}"#, "x").unwrap_err().contains("bad key")
        );
        assert!(parse_reply(Provider::Ollama, "<html>", "x").is_err());
    }

    #[test]
    fn proposals_carry_a_line_diff() {
        let p = proposal(EditorId(1), DocId(1), (0, 10), "i".into(), "a\nb\nc\n".into(), "a\nB\nc\nd\n".into());
        assert_eq!((p.added, p.removed), (2, 1));
        let tags: Vec<LineTag> = p.diff.iter().map(|l| l.tag).collect();
        assert!(tags.contains(&LineTag::Equal));
        let prompt = build_prompt("Rust", "a.rs", "fn f() {", "x", "}", "rename");
        assert!(prompt.contains("<FRAGMENT>\nx\n</FRAGMENT>") && prompt.contains("Instruction: rename"));
    }
}
