//! The views against a scripted adapter over in-process pipes: once with the DAP requests
//! (`readMemory`, `disassemble`, `dataBreakpointInfo`, …), once with an adapter that only has
//! the LLDB console (like Xcode's lldb-dap).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_dap::{AdapterKind, Capabilities, DapClient};

use super::*;

const BASE: u64 = 0x2000;
const IP: u64 = 0x1008;

/// The simulated debuggee and what the adapter was asked.
struct World {
    memory: Vec<u8>,
    stops: u32,
    requests: Vec<(String, Value)>,
    console: bool,
    source: PathBuf,
    /// Watchpoints set through the console.
    lldb_watchpoints: u32,
}

type Shared = Arc<Mutex<World>>;

fn write_message(writer: &Arc<Mutex<std::io::PipeWriter>>, message: &Value) {
    let body = serde_json::to_vec(message).unwrap();
    let mut w = writer.lock().unwrap();
    let _ = w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    let _ = w.write_all(&body);
    let _ = w.flush();
}

fn read_message(reader: &mut BufReader<std::io::PipeReader>) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length: ") {
            len = v.parse::<usize>().ok();
        }
    }
    let mut body = vec![0; len?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn event(name: &str, body: Value) -> Value {
    json!({ "seq": 0, "type": "event", "event": name, "body": body })
}

/// Answer one request; `Err` becomes a failed response. May queue events (a step stops again).
fn answer(world: &Shared, command: &str, args: &Value, events: &mut Vec<Value>) -> Result<Value, String> {
    let mut w = world.lock().unwrap();
    w.requests.push((command.to_owned(), args.clone()));
    let pc = IP + u64::from(w.stops) * 4;
    match command {
        "threads" => Ok(json!({ "threads": [{ "id": 1, "name": "main" }] })),
        "stackTrace" => Ok(json!({ "stackFrames": [{
            "id": 100, "name": "main", "line": 2, "column": 1,
            "source": { "path": w.source }, "instructionPointerReference": format!("0x{pc:X}"),
        }] })),
        "scopes" => Ok(json!({ "scopes": [
            { "name": "Locals", "variablesReference": 1, "presentationHint": "locals" },
            { "name": "Registers", "variablesReference": 2, "presentationHint": "registers" },
        ] })),
        "variables" => match args["variablesReference"].as_i64() {
            Some(1) => Ok(json!({ "variables": [{
                "name": "x", "value": w.memory[0].to_string(), "variablesReference": 0, "evaluateName": "x",
            }] })),
            Some(2) => Ok(json!({ "variables": [
                { "name": "General Purpose Registers", "value": "", "variablesReference": 3 },
            ] })),
            Some(3) => Ok(json!({ "variables": [
                { "name": "pc", "value": format!("0x{pc:016x}"), "variablesReference": 0 },
                { "name": "x0", "value": "0x0000000000000001", "variablesReference": 0 },
            ] })),
            _ => Err("no such reference".into()),
        },
        "evaluate" if args["context"] == "repl" => Ok(json!({ "result": console(&mut w, args, events) })),
        "evaluate" => match args["expression"].as_str() {
            Some("&x" | "&(x)") => Ok(json!({ "result": format!("0x{BASE:016x}"), "type": "int *" })),
            Some("x") => Ok(json!({ "result": "41", "type": "int" })),
            _ => Err("error: use of undeclared identifier".into()),
        },
        "readMemory" if !w.console => {
            let address = codecraft_dap::memory::parse_address(args["memoryReference"].as_str().unwrap()).unwrap();
            let count = args["count"].as_u64().unwrap() as usize;
            let start = address.saturating_sub(BASE) as usize;
            let readable: Vec<u8> = w.memory.iter().skip(start).take(count).copied().collect();
            if readable.is_empty() {
                return Ok(json!({ "address": format!("0x{address:x}"), "unreadableBytes": count }));
            }
            Ok(json!({
                "address": format!("0x{address:X}"),
                "data": codecraft_dap::memory::base64_encode(&readable),
                "unreadableBytes": count - readable.len(),
            }))
        }
        "writeMemory" if !w.console => {
            let address = codecraft_dap::memory::parse_address(args["memoryReference"].as_str().unwrap()).unwrap();
            let data = codecraft_dap::memory::base64_decode(args["data"].as_str().unwrap()).unwrap();
            let start = (address - BASE) as usize;
            for (i, b) in data.iter().enumerate() {
                w.memory[start + i] = *b;
            }
            Ok(json!({ "bytesWritten": data.len() }))
        }
        "disassemble" if !w.console => {
            let from = codecraft_dap::memory::parse_address(args["memoryReference"].as_str().unwrap()).unwrap();
            let offset = args["instructionOffset"].as_i64().unwrap();
            let count = args["instructionCount"].as_u64().unwrap();
            let first = (from as i64 + offset * 4) as u64;
            let instructions: Vec<Value> = (0..count)
                .map(|i| {
                    let address = first + i * 4;
                    json!({
                        "address": format!("0x{address:X}"), "instruction": format!("nop ; {i}"),
                        "instructionBytes": "1f 20 03 d5", "symbol": "main",
                        "location": { "path": w.source }, "line": if address < IP + 8 { 2 } else { 3 },
                    })
                })
                .collect();
            Ok(json!({ "instructions": instructions }))
        }
        // Older lldb-dap: decoding starts at a misaligned address and misses the program counter.
        "disassemble" => Ok(json!({ "instructions": [
            { "address": format!("0x{:X}", pc - 3), "instruction": ".long 0xe8f81f03 ; unknown opcode" },
            { "address": format!("0x{:X}", pc + 1), "instruction": ".long 0x08b9401b ; unknown opcode" },
        ] })),
        "dataBreakpointInfo" if !w.console => match args["name"].as_str() {
            Some("x") => Ok(json!({ "dataId": "2000/4", "description": "4 bytes at 2000 x",
                                    "accessTypes": ["read", "write", "readWrite"] })),
            _ => Ok(json!({ "dataId": null, "description": "cannot watch that" })),
        },
        "setDataBreakpoints" if !w.console => {
            let n = args["breakpoints"].as_array().map_or(0, Vec::len);
            Ok(json!({ "breakpoints": vec![json!({ "verified": true }); n] }))
        }
        "next" | "stepIn" | "stepOut" | "continue" => {
            w.stops += 1;
            events.push(event("stopped", json!({ "reason": "step", "threadId": 1 })));
            Ok(Value::Null)
        }
        _ => Err(format!("unsupported request {command}")),
    }
}

/// The LLDB console: the commands the fallbacks send.
fn console(w: &mut World, args: &Value, events: &mut Vec<Value>) -> String {
    let command = args["expression"].as_str().unwrap_or_default().to_owned();
    let mut out = format!("(lldb) {command}\n");
    let words: Vec<&str> = command.split_whitespace().collect();
    match words.as_slice() {
        ["memory", "read", .., address] => {
            let address = codecraft_dap::memory::parse_address(address).unwrap();
            if address < BASE || address >= BASE + w.memory.len() as u64 {
                out.push_str(&format!("error: memory read failed for 0x{address:x}\n"));
            }
            let start = address.saturating_sub(BASE) as usize;
            for (i, chunk) in w.memory.iter().skip(start).take(256).collect::<Vec<_>>().chunks(8).enumerate() {
                let bytes: Vec<String> = chunk.iter().map(|b| format!("0x{b:02x}")).collect();
                out.push_str(&format!("0x{:x}: {}\n", address + i as u64 * 8, bytes.join(" ")));
            }
        }
        ["memory", "write", "--size", "1", address, bytes @ ..] => {
            let start = (codecraft_dap::memory::parse_address(address).unwrap() - BASE) as usize;
            for (i, b) in bytes.iter().enumerate() {
                w.memory[start + i] = u8::from_str_radix(b.trim_start_matches("0x"), 16).unwrap();
            }
        }
        ["watchpoint", "set", "variable", "--watch", _, "--", "x"] => {
            w.lldb_watchpoints += 1;
            out.push_str(&format!(
                "Watchpoint created: Watchpoint {}: addr = 0x2000 size = 4 state = enabled type = w\n",
                w.lldb_watchpoints
            ));
        }
        ["watchpoint", "set", ..] => out.push_str("error: no variable named 'nope' found in this frame\n"),
        ["watchpoint", "delete", _] => out.push_str("1 watchpoints deleted.\n"),
        ["watchpoint", "enable" | "disable", _] => out.push_str("1 watchpoints changed.\n"),
        ["disassemble", ..] => {
            out.push_str("\n** 2   \tint x = 41;\n\nt`main:\n");
            for i in 0..6u64 {
                let address = IP - 8 + i * 4;
                let marker = if address == IP + u64::from(w.stops) * 4 { "->" } else { "  " };
                out.push_str(&format!("{marker}  0x{address:x} <+{}>:  nop\n", i * 4));
            }
        }
        ["thread", "step-inst-over"] | ["thread", "step-inst"] => {
            w.stops += 1;
            events.push(event("stopped", json!({ "reason": "step", "threadId": 1 })));
        }
        _ => out.push_str(&format!("error: '{}' is not a valid command.\n", words.first().unwrap_or(&""))),
    }
    out
}

/// Start the scripted adapter on a thread; returns the client side and the world.
fn fake(
    console: bool,
    source: PathBuf,
) -> (DapClient, crossbeam_channel::Receiver<codecraft_dap::DapEvent>, Shared, Arc<Mutex<std::io::PipeWriter>>) {
    let (client_reader, adapter_writer) = std::io::pipe().unwrap();
    let (adapter_reader, client_writer) = std::io::pipe().unwrap();
    let (client, events) = DapClient::connect("fake", client_reader, client_writer).unwrap();
    let mut memory: Vec<u8> = (0..=255u8).collect();
    memory[0] = 41;
    let world =
        Arc::new(Mutex::new(World { memory, stops: 0, requests: Vec::new(), console, source, lldb_watchpoints: 0 }));
    let writer = Arc::new(Mutex::new(adapter_writer));
    let (w2, writer2) = (Arc::clone(&world), Arc::clone(&writer));
    std::thread::spawn(move || {
        let mut reader = BufReader::new(adapter_reader);
        while let Some(request) = read_message(&mut reader) {
            let command = request["command"].as_str().unwrap_or_default().to_owned();
            let mut queued = Vec::new();
            let result = answer(&w2, &command, &request["arguments"], &mut queued);
            let response = match result {
                Ok(body) => json!({ "seq": 0, "type": "response", "request_seq": request["seq"], "command": command,
                                    "success": true, "body": body }),
                Err(e) => json!({ "seq": 0, "type": "response", "request_seq": request["seq"], "command": command,
                                  "success": false, "message": e }),
            };
            write_message(&writer2, &response);
            for e in queued {
                write_message(&writer2, &e);
            }
        }
    });
    (client, events, world, writer)
}

fn pump_until(s: &mut Session, what: &str, cond: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}; views: {:?}", s.debug.views);
        s.pump();
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn settled(s: &Session) -> bool {
    s.debug.is_stopped() && !s.debug.settling()
}

fn session(console: bool) -> (Session, Shared, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("main.c");
    std::fs::write(&source, "int main(void) {\n  int x = 41;\n  x++;\n}\n").unwrap();
    let (client, events, world, writer) = fake(console, source);
    let mut s = Session::new();
    s.lsp_enabled = false;
    let capabilities = if console {
        Capabilities { supports_disassemble_request: true, ..Capabilities::default() }
    } else {
        Capabilities {
            supports_read_memory_request: true,
            supports_write_memory_request: true,
            supports_disassemble_request: true,
            supports_data_breakpoints: true,
            supports_stepping_granularity: true,
            ..Capabilities::default()
        }
    };
    crate::debug::install_fake(&mut s, client, events, AdapterKind::LldbDap, capabilities);
    write_message(&writer, &event("stopped", json!({ "reason": "breakpoint", "threadId": 1 })));
    pump_until(&mut s, "the first stop", settled);
    (s, world, tmp)
}

fn requests(world: &Shared, command: &str) -> Vec<Value> {
    world.lock().unwrap().requests.iter().filter(|(c, _)| c == command).map(|(_, a)| a.clone()).collect()
}

fn exercise(console: bool) {
    let (mut s, world, _tmp) = session(console);

    // Memory: `&x` evaluates to the variable's address.
    s.execute("debug.memory.show", &json!({ "address": "&x" })).unwrap();
    assert_eq!(s.debug.tab, DebugTab::Memory);
    pump_until(&mut s, "the memory page", |s| settled(s) && !s.debug.views.memory.loading);
    let m = &s.debug.views.memory;
    assert_eq!(m.error, None);
    assert_eq!(m.address, Some(BASE));
    assert_eq!(m.bytes.len(), MEMORY_PAGE);
    assert_eq!((m.byte_at(BASE), m.byte_at(BASE + 5)), (Some(41), Some(5)));
    assert_eq!(m.via_console, console);
    assert_eq!(m.rows().len(), MEMORY_PAGE / BYTES_PER_ROW);
    assert_eq!(hex_row(&m.rows()[0].1[..3]), "29 01 02");

    // A plain value (not a pointer) shows where it lives (`&(x)`).
    s.execute("debug.memory.goto", &json!({ "address": "x" })).unwrap();
    pump_until(&mut s, "the variable's memory", |s| settled(s) && !s.debug.views.memory.loading);
    assert_eq!(s.debug.views.memory.address, Some(BASE));

    // Edit bytes: the page and the variables are read again; changed bytes are marked.
    let r = s.execute("debug.memory.write", &json!({ "address": "0x2000", "bytes": "2a" })).unwrap();
    assert_eq!(r["count"], 1);
    pump_until(&mut s, "the write", |s| settled(s) && s.debug.views.memory.byte_at(BASE) == Some(42));
    assert!(s.debug.views.memory.changed.contains(&0), "{:?}", s.debug.views.memory.changed);
    pump_until(&mut s, "the variables", |s| {
        settled(s)
            && s.debug
                .session
                .as_ref()
                .is_some_and(|x| x.variables.get(&1).is_some_and(|v| v.first().is_some_and(|v| v.value == "42")))
    });

    // Paging past the readable memory.
    s.execute("debug.memory.page", &json!({ "delta": 1 })).unwrap();
    pump_until(&mut s, "the next page", |s| settled(s) && !s.debug.views.memory.loading);
    assert_eq!(s.debug.views.memory.address, Some(BASE + MEMORY_PAGE as u64));
    assert!(s.debug.views.memory.error.is_some());
    assert!(s.debug.views.memory.bytes.iter().all(Option::is_none));
    s.execute("debug.memory.page", &json!({ "delta": -1 })).unwrap();
    pump_until(&mut s, "the page back", |s| settled(s) && s.debug.views.memory.byte_at(BASE) == Some(42));

    // Disassembly around the program counter, the source line mixed in.
    s.execute("debug.showTab", &json!({ "tab": "disassembly" })).unwrap();
    pump_until(&mut s, "the disassembly", |s| settled(s) && !s.debug.views.disassembly.rows.is_empty());
    let d = &s.debug.views.disassembly;
    assert_eq!(d.error, None);
    assert_eq!(d.current, Some(IP));
    assert!(d.current_row().is_some(), "{:?}", d.rows);
    assert!(
        d.rows.iter().any(|r| matches!(r, DisassemblyRow::Source { text, .. } if text.contains("int x = 41"))),
        "{:?}",
        d.rows
    );
    assert_eq!(d.via_console, console);

    // Step Over while the Disassembly is shown steps one instruction.
    s.execute("run.debuggingActions.stepOver", &Value::Null).unwrap();
    pump_until(&mut s, "the instruction step", |s| {
        settled(s) && s.debug.views.disassembly.current == Some(IP + 4) && !s.debug.views.disassembly.loading
    });
    if console {
        let commands: Vec<String> =
            requests(&world, "evaluate").iter().filter_map(|a| a["expression"].as_str().map(str::to_owned)).collect();
        assert!(commands.iter().any(|c| c == "thread step-inst-over"), "{commands:?}");
    } else {
        let next = requests(&world, "next");
        assert_eq!(next.last().unwrap()["granularity"], "instruction");
    }
    assert!(s.debug.views.disassembly.current_row().is_some());

    // Registers: grouped; the program counter changed since the previous stop.
    s.execute("debug.registers.show", &Value::Null).unwrap();
    pump_until(&mut s, "the registers", |s| {
        settled(s) && s.debug.views.registers.groups.first().is_some_and(|g| !g.registers.is_empty())
    });
    s.execute("debug.disassembly.stepOverInstruction", &Value::Null).unwrap();
    pump_until(&mut s, "the registers after a step", |s| {
        settled(s)
            && s.debug
                .views
                .registers
                .groups
                .first()
                .is_some_and(|g| g.registers.iter().any(|r| r.name == "pc" && r.value == format!("0x{:016x}", IP + 8)))
    });
    let group = s.debug.views.registers.groups[0].clone();
    assert_eq!(group.name, "General Purpose Registers");
    assert!(group.registers.iter().find(|r| r.name == "pc").unwrap().changed);
    assert!(!group.registers.iter().find(|r| r.name == "x0").unwrap().changed);

    // Watchpoints: added from the Variables view, listed, removable; unknown names fail cleanly.
    let id =
        s.execute("debug.watchpoint.add", &json!({ "name": "x", "reference": 1, "evaluateName": "x" })).unwrap()["id"]
            .as_u64()
            .unwrap();
    assert_eq!(s.debug.tab, DebugTab::Breakpoints);
    pump_until(&mut s, "the watchpoint", |s| {
        settled(s) && s.debug.views.watchpoints.iter().any(|w| w.id == id && w.verified == Some(true))
    });
    if console {
        assert_eq!(s.debug.views.watchpoints[0].lldb_id, Some(1));
    } else {
        let set = requests(&world, "setDataBreakpoints");
        assert_eq!(set.last().unwrap()["breakpoints"][0], json!({ "dataId": "2000/4", "accessType": "write" }));
    }
    s.execute("debug.watchpoint.add", &json!({ "name": "nope", "reference": 1 })).unwrap();
    pump_until(&mut s, "the refusal", |s| settled(s) && s.debug.views.watchpoints.len() == 1);
    assert!(s.status.contains("Cannot watch nope"), "{}", s.status);
    s.execute("debug.watchpoint.setEnabled", &json!({ "id": id, "enabled": false })).unwrap();
    assert!(!s.debug.views.watchpoints[0].enabled);
    assert_eq!(s.execute("debug.watchpoint.remove", &json!({ "id": id })).unwrap()["removed"], true);
    pump_until(&mut s, "the removal", settled);
    assert!(s.debug.views.watchpoints.is_empty());
    if console {
        let commands: Vec<String> =
            requests(&world, "evaluate").iter().filter_map(|a| a["expression"].as_str().map(str::to_owned)).collect();
        assert!(commands.iter().any(|c| c == "watchpoint delete 1"), "{commands:?}");
    } else {
        assert_eq!(requests(&world, "setDataBreakpoints").last().unwrap()["breakpoints"], json!([]));
    }

    // The console keeps a history.
    s.execute("debug.consoleEvaluate", &json!({ "expression": "bt" })).unwrap();
    s.execute("debug.consoleEvaluate", &json!({ "expression": "bt" })).unwrap();
    assert_eq!(s.execute("debug.console.history", &Value::Null).unwrap()["history"], json!(["bt"]));
    pump_until(&mut s, "the console", settled);

    // The snapshot carries the views for agents.
    let state = s.execute("debug.state", &Value::Null).unwrap();
    assert_eq!(state["views"]["memory"]["address"], json!(BASE));
    s.shutdown();
}

#[test]
fn views_with_dap_requests() {
    exercise(false);
}

#[test]
fn views_through_the_lldb_console() {
    exercise(true);
}

#[test]
fn bad_params_and_states_fail_cleanly() {
    let mut s = Session::new();
    s.lsp_enabled = false;
    // Not debugging.
    for (id, params) in [
        ("debug.memory.goto", json!({ "address": "0x10" })),
        ("debug.memory.page", json!({ "delta": 1 })),
        ("debug.memory.write", json!({ "address": "0x10", "bytes": "00" })),
        ("debug.disassembly.stepOverInstruction", Value::Null),
        ("debug.watchpoint.add", json!({ "name": "x" })),
    ] {
        assert!(s.execute(id, &params).is_err(), "{id}");
    }
    // Showing the views is fine without a session.
    s.execute("debug.memory.show", &Value::Null).unwrap();
    s.execute("debug.disassembly.show", &Value::Null).unwrap();
    s.execute("debug.registers.show", &Value::Null).unwrap();
    assert!(s.execute("debug.showTab", &json!({ "tab": "nope" })).is_err());
    assert_eq!(s.execute("debug.watchpoint.remove", &json!({ "id": 9 })).unwrap()["removed"], false);
    assert!(s.execute("debug.watchpoint.remove", &json!({})).is_err());
    assert!(s.execute("debug.watchpoint.setEnabled", &json!({ "id": 9 })).is_err());

    let (mut s, _world, _tmp) = session(false);
    for (id, params) in [
        ("debug.memory.goto", json!({ "address": "" })),
        ("debug.memory.goto", json!({})),
        ("debug.memory.write", json!({ "address": "0x2000", "bytes": "zz" })),
        ("debug.memory.write", json!({ "address": "x", "bytes": "00" })),
        ("debug.memory.write", json!({ "address": -5, "bytes": "00" })),
        ("debug.memory.write", json!({ "address": "0x2000", "bytes": "00".repeat(5000) })),
        ("debug.memory.page", json!({ "delta": 1 })),
        ("debug.watchpoint.add", json!({ "name": " " })),
        ("debug.watchpoint.add", json!({ "name": "x", "access": "sideways" })),
    ] {
        assert!(s.execute(id, &params).is_err(), "{id} {params}");
    }
    // An expression that isn't an address reports it.
    s.execute("debug.memory.goto", &json!({ "address": "nonsense + 1" })).unwrap();
    pump_until(&mut s, "the error", |s| settled(s) && s.debug.views.memory.error.is_some());
    // Paging from the top of the address space saturates instead of overflowing.
    s.execute("debug.memory.goto", &json!({ "address": format!("{}", u64::MAX - 10) })).unwrap();
    s.execute("debug.memory.page", &json!({ "delta": i64::MAX })).unwrap();
    assert_eq!(s.debug.views.memory.address, Some(u64::MAX));
    s.execute("debug.memory.page", &json!({ "delta": i64::MIN })).unwrap();
    assert_eq!(s.debug.views.memory.address, Some(0));
    pump_until(&mut s, "the reads", settled);
    s.shutdown();
}

#[test]
fn malformed_answers_do_not_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.debug.views.memory.address = Some(BASE);
    // Answers the adapter could send, fed straight to the handlers.
    on_memory_read(&mut s, BASE, false, Ok(json!({ "address": "0x2000", "data": "@@@" })));
    assert!(s.debug.views.memory.error.is_some());
    on_memory_read(&mut s, BASE, false, Ok(json!({ "address": "0xffffffffffffffff", "data": "AAAA" })));
    assert!(s.debug.views.memory.bytes.iter().all(Option::is_none));
    on_memory_read(&mut s, BASE, true, Ok(json!({ "result": "0x2000: 0x01 0xgg\n" })));
    on_memory_read(&mut s, BASE, false, Ok(json!(42)));
    on_memory_read(&mut s, BASE, false, Err("boom".into()));
    assert_eq!(s.debug.views.memory.error.as_deref(), Some("boom"));
    on_disassemble(&mut s, true, IP, false, Ok(json!({ "instructions": [{ "address": "zz" }, { "line": -4 }] })));
    on_disassemble(&mut s, true, IP, true, Ok(json!({ "result": "error: no frame" })));
    assert_eq!(s.debug.views.disassembly.error.as_deref(), Some("no frame"));
    on_register_scope(&mut s, true, "Registers".into(), Ok(json!({ "variables": "nope" })));
    on_register_group(&mut s, true, 99, true, Ok(json!({})));
    on_watch_info(&mut s, 77, Ok(json!({ "dataId": 5 })));
    on_set_watchpoints(&mut s, vec![1, 2, 3], Ok(json!({ "breakpoints": [{ "verified": true }] })));
    on_watch_command(&mut s, 1, WatchAction::Set, Ok(json!({ "result": "Watchpoint created: Watchpoint x:" })));
    on_memory_eval(&mut s, "p".into(), true, Ok(json!({ "result": "" })));
    assert!(s.debug.views.memory.error.is_some());
    drop(tmp);
}
