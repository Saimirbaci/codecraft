//! LLDB console commands and their output, for adapters that lack a DAP request CodeCraft needs.
//!
//! Older lldb-dap builds (Xcode's, for one) don't implement `readMemory`, `writeMemory`,
//! `dataBreakpointInfo` or stepping granularity, and disassemble misaligned code before the
//! instruction pointer. They do run LLDB commands sent as `evaluate` with `context: "repl"`, so
//! CodeCraft falls back to `memory read`, `memory write`, `watchpoint set`, `thread step-inst`
//! and `disassemble --frame`, and parses the text LLDB prints (documented LLDB command output).

use crate::memory::parse_address;

/// What a watchpoint stops on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WatchAccess {
    /// Break when the value changes (is written).
    #[default]
    Write,
    Read,
    /// Break on any access.
    ReadWrite,
}

impl WatchAccess {
    /// The DAP `accessType`.
    pub fn dap(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Read => "read",
            Self::ReadWrite => "readWrite",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "write" | "change" => Some(Self::Write),
            "read" => Some(Self::Read),
            "readWrite" | "read_write" | "access" => Some(Self::ReadWrite),
            _ => None,
        }
    }

    fn lldb(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Read => "read",
            Self::ReadWrite => "read_write",
        }
    }
}

/// `memory read` of `count` bytes as hex, eight per line.
pub fn memory_read_command(address: u64, count: usize) -> String {
    format!("memory read --size 1 --format x --count {count} 0x{address:x}")
}

/// `memory write` of `bytes` at `address`.
pub fn memory_write_command(address: u64, bytes: &[u8]) -> String {
    let mut command = format!("memory write --size 1 0x{address:x}");
    for b in bytes {
        command.push_str(&format!(" 0x{b:02x}"));
    }
    command
}

/// A watchpoint on a frame variable path (`x`, `s.field`, `v[2]`).
pub fn watchpoint_set_command(variable: &str, access: WatchAccess) -> String {
    format!("watchpoint set variable --watch {} -- {variable}", access.lldb())
}

pub fn watchpoint_delete_command(id: u32) -> String {
    format!("watchpoint delete {id}")
}

pub fn watchpoint_enable_command(id: u32, enabled: bool) -> String {
    format!("watchpoint {} {id}", if enabled { "enable" } else { "disable" })
}

/// One instruction (`over`: `thread step-inst-over`, which steps over calls).
pub fn step_instruction_command(over: bool) -> &'static str {
    if over { "thread step-inst-over" } else { "thread step-inst" }
}

/// The selected frame's function with source lines mixed in.
pub const DISASSEMBLE_FRAME_COMMAND: &str = "disassemble --frame --mixed";

/// The command output without the `(lldb) command` echo some lldb-dap versions put first.
pub fn strip_echo(output: &str) -> &str {
    match output.strip_prefix("(lldb) ") {
        Some(rest) => rest.split_once('\n').map_or("", |(_, after)| after),
        None => output,
    }
}

/// The first `error: …` line, if the command failed.
pub fn error_of(output: &str) -> Option<String> {
    strip_echo(output).lines().find_map(|l| l.trim().strip_prefix("error:").map(|e| e.trim().to_owned()))
}

/// `memory read --format x --size 1` output (`0x16fdfccf8: 0x29 0x00 …`) as (start, bytes).
/// Lines that don't look like that are skipped.
pub fn parse_memory_read(output: &str) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    for line in strip_echo(output).lines() {
        let Some((address, rest)) = line.trim().split_once(':') else { continue };
        let Some(address) = parse_address(address) else { continue };
        let bytes: Option<Vec<u8>> = rest
            .split_whitespace()
            .map(|t| t.strip_prefix("0x").and_then(|h| u8::from_str_radix(h, 16).ok()))
            .collect();
        if let Some(bytes) = bytes.filter(|b| !b.is_empty()) {
            out.push((address, bytes));
        }
    }
    out
}

/// The bytes `memory read` returned from `start`, contiguous (gaps end the read).
pub fn memory_bytes(output: &str, start: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut next = start;
    for (address, chunk) in parse_memory_read(output) {
        if address != next {
            break;
        }
        next = next.saturating_add(chunk.len() as u64);
        bytes.extend(chunk);
    }
    bytes
}

/// The id in `Watchpoint created: Watchpoint 3: addr = …`.
pub fn parse_watchpoint_created(output: &str) -> Option<u32> {
    let rest = strip_echo(output).split("Watchpoint created: Watchpoint ").nth(1)?;
    rest.split(':').next()?.trim().parse().ok()
}

/// A row of `disassemble --mixed` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisassemblyLine {
    /// `t`main:` — the function the following instructions belong to.
    Symbol(String),
    /// A source line LLDB mixed in (`** 3   \tint main(void) {`), as printed.
    Source(String),
    Instruction {
        address: u64,
        /// `<+60>`, the offset in the function.
        offset: Option<String>,
        text: String,
        /// Marked `->`: the current instruction.
        current: bool,
    },
}

/// Parse `disassemble` output (with or without `--mixed`).
pub fn parse_disassembly(output: &str) -> Vec<DisassemblyLine> {
    let mut out = Vec::new();
    for raw in strip_echo(output).lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let (current, body) = match line.strip_prefix("->") {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let trimmed = body.trim_start();
        if trimmed.starts_with("0x")
            && let Some((address, rest)) = split_token(trimmed)
            && let Some(address) = parse_address(address)
        {
            let rest = rest.trim_start();
            let (offset, text) = match rest.strip_prefix('<').and_then(|r| r.split_once(">:")) {
                Some((offset, text)) => (Some(format!("<{offset}>")), text),
                None => (None, rest.strip_prefix(':').unwrap_or(rest)),
            };
            out.push(DisassemblyLine::Instruction { address, offset, text: text.trim().to_owned(), current });
        } else if !line.starts_with(char::is_whitespace) && line.ends_with(':') && !line.starts_with("**") {
            out.push(DisassemblyLine::Symbol(line.trim_end_matches(':').to_owned()));
        } else {
            out.push(DisassemblyLine::Source(line.strip_prefix("** ").unwrap_or(line).to_owned()));
        }
    }
    out
}

fn split_token(text: &str) -> Option<(&str, &str)> {
    let end = text.find(|c: char| c.is_whitespace() || c == ':').unwrap_or(text.len());
    Some((text.get(..end)?, text.get(end..)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_read_output_parses() {
        let out = "(lldb) memory read -s1 -fx -c 20 0x000000016fdfccf8\n0x16fdfccf8: 0x29 0x00 0x00 0x00 0x00 0x00 0x00 0x00\n0x16fdfcd00: 0x68 0x65 0x6c 0x6c 0x6f 0x00 0x00 0x00\n0x16fdfcd08: 0xc5 0x00 0x10 0x5b\n";
        let bytes = memory_bytes(out, 0x0001_6fdf_ccf8);
        assert_eq!(bytes.len(), 20);
        assert_eq!(bytes.get(8..13), Some(&b"hello"[..]));
        // A different start: nothing contiguous.
        assert!(memory_bytes(out, 0x10).is_empty());
        assert_eq!(
            error_of("(lldb) memory read 0x0\nerror: memory read failed for 0x0\n").as_deref(),
            Some("memory read failed for 0x0")
        );
        assert!(error_of(out).is_none());
        // Without the echo (newer lldb-dap) and with junk lines.
        assert_eq!(memory_bytes("0x10: 0x01 0x02\nwarning: x\n0x12: 0xzz\n", 0x10), vec![1, 2]);
    }

    #[test]
    fn commands_are_well_formed() {
        assert_eq!(memory_read_command(0x10, 256), "memory read --size 1 --format x --count 256 0x10");
        assert_eq!(memory_write_command(0xff, &[0x2a, 0]), "memory write --size 1 0xff 0x2a 0x00");
        assert_eq!(
            watchpoint_set_command("s.x", WatchAccess::ReadWrite),
            "watchpoint set variable --watch read_write -- s.x"
        );
        assert_eq!(WatchAccess::parse("readWrite"), Some(WatchAccess::ReadWrite));
        assert_eq!(WatchAccess::parse("nope"), None);
        assert_eq!(WatchAccess::Read.dap(), "read");
    }

    #[test]
    fn watchpoint_ids_parse() {
        let out = "(lldb) watchpoint set variable -w write -- counter\nWatchpoint created: Watchpoint 12: addr = 0x100008000 size = 4 state = enabled type = w\n";
        assert_eq!(parse_watchpoint_created(out), Some(12));
        assert_eq!(parse_watchpoint_created("error: no variable named 'nosuch' found in this frame\n"), None);
    }

    #[test]
    fn disassembly_parses_mixed_output() {
        let out = "(lldb) disassemble --frame --mixed\n\n** 3   \tint main(void) {\n\nt`main:\n    0x100003eb8 <+0>:   sub    sp, sp, #0x40\n->  0x100003ef4 <+60>:  ldr    w8, [sp, #0x18]\n    0x100003ef8:  nop\n";
        let lines = parse_disassembly(out);
        assert_eq!(lines[0], DisassemblyLine::Source("3   \tint main(void) {".into()));
        assert_eq!(lines[1], DisassemblyLine::Symbol("t`main".into()));
        assert_eq!(
            lines[2],
            DisassemblyLine::Instruction {
                address: 0x0001_0000_3eb8,
                offset: Some("<+0>".into()),
                text: "sub    sp, sp, #0x40".into(),
                current: false
            }
        );
        assert!(matches!(&lines[3], DisassemblyLine::Instruction { current: true, address: 0x0001_0000_3ef4, .. }));
        assert!(matches!(&lines[4], DisassemblyLine::Instruction { offset: None, text, .. } if text == "nop"));
        assert!(parse_disassembly("").is_empty());
        assert!(parse_disassembly("0xZZ <+0>: x").iter().all(|l| matches!(l, DisassemblyLine::Source(_))));
    }
}
