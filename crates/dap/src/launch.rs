//! `launch` arguments for each adapter. The arguments are adapter-specific (DAP leaves them
//! open); these follow each adapter's public documentation.

use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::AdapterKind;

/// A native program (Rust, C, C++, Zig) under lldb-dap or CodeLLDB.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeLaunch {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// LLDB commands run before the target is created (pretty printers).
    pub init_commands: Vec<String>,
    pub stop_on_entry: bool,
    /// `rust`, `cpp`, … (CodeLLDB uses it for language-specific formatting).
    pub source_languages: Vec<String>,
}

/// A Python script or module under debugpy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PythonLaunch {
    pub name: String,
    pub program: Option<PathBuf>,
    pub module: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// The interpreter for the debuggee (a project virtualenv); the adapter's own if `None`.
    pub python: Option<PathBuf>,
    pub just_my_code: bool,
    pub stop_on_entry: bool,
    /// Debug the processes the program starts too (debugpy's `subProcess`; each child arrives
    /// as a `startDebugging` request).
    pub sub_process: bool,
    /// Django template debugging (breakpoints in templates, `django: true`).
    pub django: bool,
    /// Jinja template debugging (Flask, `jinja: true`).
    pub jinja: bool,
}

/// A local directory and where the same files live on the remote machine (`pathMappings`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PathMapping {
    pub local_root: String,
    pub remote_root: String,
}

/// A Node.js program under js-debug.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeLaunch {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub runtime: Option<PathBuf>,
}

fn env_object(env: &[(String, String)]) -> Value {
    Value::Object(env.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect::<Map<_, _>>())
}

fn cwd_value(cwd: &Option<PathBuf>) -> Value {
    cwd.as_ref().map_or(Value::Null, |c| Value::String(c.display().to_string()))
}

/// Drops `null` members so adapters see absent fields rather than nulls.
fn compact(mut value: Value) -> Value {
    if let Some(map) = value.as_object_mut() {
        map.retain(|_, v| !v.is_null());
    }
    value
}

pub fn native(kind: AdapterKind, launch: &NativeLaunch) -> Value {
    let program = launch.program.display().to_string();
    match kind {
        AdapterKind::CodeLldb => compact(json!({
            "name": launch.name,
            "type": "lldb",
            "request": "launch",
            "program": program,
            "args": launch.args,
            "cwd": cwd_value(&launch.cwd),
            "env": env_object(&launch.env),
            "initCommands": launch.init_commands,
            "stopOnEntry": launch.stop_on_entry,
            "sourceLanguages": launch.source_languages,
            "terminal": "console",
        })),
        // lldb-dap (and anything lldb-vscode compatible). `env` as `K=V` strings works with every
        // lldb-dap version.
        _ => compact(json!({
            "name": launch.name,
            "type": "lldb-dap",
            "request": "launch",
            "program": program,
            "args": launch.args,
            "cwd": cwd_value(&launch.cwd),
            "env": launch.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            "initCommands": launch.init_commands,
            "stopOnEntry": launch.stop_on_entry,
        })),
    }
}

pub fn python(launch: &PythonLaunch) -> Value {
    let mut value = compact(json!({
        "name": launch.name,
        "type": "python",
        "request": "launch",
        "args": launch.args,
        "cwd": cwd_value(&launch.cwd),
        "env": env_object(&launch.env),
        "justMyCode": launch.just_my_code,
        "stopOnEntry": launch.stop_on_entry,
        "console": "internalConsole",
        "redirectOutput": true,
        "showReturnValue": true,
        "subProcess": launch.sub_process,
    }));
    if let Some(map) = value.as_object_mut() {
        if launch.django {
            map.insert("django".into(), true.into());
        }
        if launch.jinja {
            map.insert("jinja".into(), true.into());
        }
        match (&launch.module, &launch.program) {
            (Some(module), _) => {
                map.insert("module".into(), module.clone().into());
            }
            (None, Some(program)) => {
                map.insert("program".into(), program.display().to_string().into());
            }
            (None, None) => {}
        }
        if let Some(python) = &launch.python {
            map.insert("python".into(), python.display().to_string().into());
        }
    }
    value
}

pub fn node(launch: &NodeLaunch) -> Value {
    compact(json!({
        "name": launch.name,
        "type": "pwa-node",
        "request": "launch",
        "program": launch.program.display().to_string(),
        "args": launch.args,
        "cwd": cwd_value(&launch.cwd),
        "env": env_object(&launch.env),
        "console": "internalConsole",
        "outputCapture": "std",
        "runtimeExecutable": launch.runtime.as_ref().map(|r| r.display().to_string()),
    }))
}

/// `attach` arguments for a running native process (lldb-dap or CodeLLDB).
pub fn native_attach(kind: AdapterKind, name: &str, pid: u32, init_commands: &[String]) -> Value {
    match kind {
        AdapterKind::CodeLldb => json!({
            "name": name,
            "type": "lldb",
            "request": "attach",
            "pid": pid,
            "initCommands": init_commands,
        }),
        _ => json!({
            "name": name,
            "type": "lldb-dap",
            "request": "attach",
            "pid": pid,
            "initCommands": init_commands,
        }),
    }
}

/// `attach` arguments that wait for a process to start (Run › Attach to an Unstarted Process…).
/// `program` is an executable path (the adapter creates the target from it and waits for a
/// process with its name) or a bare process name, which lldb-dap attaches to with the LLDB
/// command `process attach --name <name> --waitfor`.
pub fn native_attach_wait(kind: AdapterKind, name: &str, program: &str, init_commands: &[String]) -> Value {
    let path = std::path::Path::new(program);
    match kind {
        AdapterKind::CodeLldb => json!({
            "name": name,
            "type": "lldb",
            "request": "attach",
            "program": program,
            "waitFor": true,
            "initCommands": init_commands,
        }),
        _ if path.is_absolute() && path.is_file() => json!({
            "name": name,
            "type": "lldb-dap",
            "request": "attach",
            "program": program,
            "waitFor": true,
            "initCommands": init_commands,
        }),
        _ => {
            let process = path.file_name().and_then(|n| n.to_str()).unwrap_or(program);
            json!({
                "name": name,
                "type": "lldb-dap",
                "request": "attach",
                "attachCommands": [format!("process attach --name {} --waitfor", lldb_quoted(process))],
                "initCommands": init_commands,
            })
        }
    }
}

/// A process name for an LLDB command line.
fn lldb_quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A process name (or path) Attach to an Unstarted Process can wait for.
pub fn valid_wait_name(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty() && n.len() <= 4096 && !n.contains(['\n', '\r', '\0'])
}

fn quoted(path: &std::path::Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('\\', "\\\\").replace('"', "\\\""))
}

/// Open a core dump (post-mortem debugging): lldb-dap attaches with `coreFile`; CodeLLDB creates
/// the target with `target create -c`. `program` is the executable that crashed (lldb finds it
/// from the core when `None`).
pub fn native_core(
    kind: AdapterKind,
    name: &str,
    program: Option<&std::path::Path>,
    core: &std::path::Path,
    init_commands: &[String],
) -> Value {
    match kind {
        AdapterKind::CodeLldb => {
            let create = match program {
                Some(p) => format!("target create -c {} {}", quoted(core), quoted(p)),
                None => format!("target create -c {}", quoted(core)),
            };
            json!({
                "name": name,
                "type": "lldb",
                "request": "launch",
                "initCommands": init_commands,
                "targetCreateCommands": [create],
                "processCreateCommands": [],
            })
        }
        _ => compact(json!({
            "name": name,
            "type": "lldb-dap",
            "request": "attach",
            "program": program.map(|p| p.display().to_string()),
            "coreFile": core.display().to_string(),
            "initCommands": init_commands,
        })),
    }
}

/// Connect to a remote stub (`gdbserver host:port`, `lldb-server gdbserver`, `debugserver`)
/// with `gdb-remote`. `program` is a local copy of the remote executable (for symbols).
pub fn native_remote(
    kind: AdapterKind,
    name: &str,
    program: Option<&std::path::Path>,
    address: &str,
    init_commands: &[String],
) -> Value {
    let connect = format!("gdb-remote {}", address.trim());
    match kind {
        AdapterKind::CodeLldb => {
            let create = match program {
                Some(p) => vec![format!("target create {}", quoted(p))],
                None => Vec::new(),
            };
            json!({
                "name": name,
                "type": "lldb",
                "request": "launch",
                "initCommands": init_commands,
                "targetCreateCommands": create,
                "processCreateCommands": [connect],
            })
        }
        _ => compact(json!({
            "name": name,
            "type": "lldb-dap",
            "request": "attach",
            "program": program.map(|p| p.display().to_string()),
            "attachCommands": [connect],
            "initCommands": init_commands,
        })),
    }
}

/// A `host:port` (or `[v6]:port`) a remote stub listens on.
pub fn valid_remote_address(address: &str) -> bool {
    let address = address.trim();
    let Some((host, port)) = address.rsplit_once(':') else { return false };
    !host.is_empty()
        && !host.contains(char::is_whitespace)
        && !host.contains(['"', '\\', ';'])
        && port.parse::<u16>().is_ok_and(|p| p > 0)
}

/// Run the program on a remote machine through an `lldb-server platform` (`platform select`,
/// `platform connect`), then debug it there. `url` is `connect://host:port`; `platform` is
/// `remote-linux`, `remote-macosx`, `remote-android`, … `program` is the local executable, which
/// LLDB uploads to the platform's working directory.
pub fn native_platform(
    kind: AdapterKind,
    name: &str,
    platform: &str,
    url: &str,
    program: &std::path::Path,
    args: &[String],
    init_commands: &[String],
) -> Value {
    let mut init = init_commands.to_vec();
    init.push(format!("platform select {}", platform.trim()));
    init.push(format!("platform connect {}", url.trim()));
    match kind {
        AdapterKind::CodeLldb => json!({
            "name": name,
            "type": "lldb",
            "request": "launch",
            "program": program.display().to_string(),
            "args": args,
            "initCommands": init,
        }),
        _ => json!({
            "name": name,
            "type": "lldb-dap",
            "request": "launch",
            "program": program.display().to_string(),
            "args": args,
            "initCommands": init,
        }),
    }
}

/// An `lldb-server platform` URL (`connect://host:port`).
pub fn valid_platform_url(url: &str) -> bool {
    url.trim().strip_prefix("connect://").is_some_and(valid_remote_address)
}

/// An LLDB platform name (`remote-linux`).
pub fn valid_platform_name(platform: &str) -> bool {
    let p = platform.trim();
    !p.is_empty() && p.len() <= 64 && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `attach` arguments for a Python program started with `python -m debugpy --listen host:port`
/// (or `debugpy.listen()`), possibly on another machine: the adapter connects to it. Path
/// mappings translate local file paths to the remote machine's.
pub fn python_remote(name: &str, host: &str, port: u16, mappings: &[PathMapping], just_my_code: bool) -> Value {
    let mut value = json!({
        "name": name,
        "type": "python",
        "request": "attach",
        "connect": { "host": host.trim(), "port": port },
        "justMyCode": just_my_code,
        "redirectOutput": true,
        "showReturnValue": true,
    });
    if !mappings.is_empty()
        && let Some(map) = value.as_object_mut()
    {
        map.insert("pathMappings".into(), json!(mappings));
    }
    value
}

/// `attach` arguments for a running Python process (debugpy injects itself by process id).
pub fn python_attach(name: &str, pid: u32, just_my_code: bool) -> Value {
    json!({
        "name": name,
        "type": "python",
        "request": "attach",
        "processId": pid,
        "justMyCode": just_my_code,
        "redirectOutput": true,
        "showReturnValue": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_arguments() {
        let v = native_attach(AdapterKind::LldbDap, "app", 42, &["command script import x".into()]);
        assert_eq!(v["request"], json!("attach"));
        assert_eq!(v["pid"], json!(42));
        assert_eq!(v["type"], json!("lldb-dap"));
        assert_eq!(v["initCommands"][0], json!("command script import x"));
        assert_eq!(native_attach(AdapterKind::CodeLldb, "app", 1, &[])["type"], json!("lldb"));
        let p = python_attach("py", 7, true);
        assert_eq!(p["processId"], json!(7));
        assert_eq!(p["request"], json!("attach"));
    }

    #[test]
    fn core_and_remote_arguments() {
        let core = native_core(
            AdapterKind::LldbDap,
            "crash",
            Some(std::path::Path::new("/p/app")),
            std::path::Path::new("/c/core"),
            &[],
        );
        assert_eq!(core["request"], json!("attach"));
        assert_eq!(core["coreFile"], json!("/c/core"));
        assert_eq!(core["program"], json!("/p/app"));
        let no_program = native_core(AdapterKind::LldbDap, "crash", None, std::path::Path::new("/c/core"), &[]);
        assert!(no_program.get("program").is_none());
        let code = native_core(AdapterKind::CodeLldb, "crash", None, std::path::Path::new("/c/my \"core\""), &[]);
        assert_eq!(code["targetCreateCommands"][0], json!("target create -c \"/c/my \\\"core\\\"\""));
        let remote = native_remote(AdapterKind::LldbDap, "r", None, " 127.0.0.1:1234 ", &[]);
        assert_eq!(remote["attachCommands"], json!(["gdb-remote 127.0.0.1:1234"]));
        assert_eq!(remote["request"], json!("attach"));
        let code = native_remote(AdapterKind::CodeLldb, "r", Some(std::path::Path::new("/p/app")), "h:1", &[]);
        assert_eq!(code["processCreateCommands"], json!(["gdb-remote h:1"]));
        assert!(valid_remote_address("localhost:1234"));
        assert!(valid_remote_address("[::1]:9"));
        for bad in ["", "host", "host:", ":12", "host:0", "host:99999", "a b:1", "h;x:1"] {
            assert!(!valid_remote_address(bad), "{bad}");
        }
    }

    #[test]
    fn attach_wait_arguments() {
        let by_name = native_attach_wait(AdapterKind::LldbDap, "w", "my \"app\"", &[]);
        assert_eq!(by_name["request"], json!("attach"));
        assert_eq!(by_name["attachCommands"], json!(["process attach --name \"my \\\"app\\\"\" --waitfor"]));
        assert!(by_name.get("waitFor").is_none());
        let exe = std::env::current_exe().unwrap();
        let by_path = native_attach_wait(AdapterKind::LldbDap, "w", &exe.display().to_string(), &[]);
        assert_eq!(by_path["waitFor"], json!(true));
        assert_eq!(by_path["program"], json!(exe.display().to_string()));
        // A path that does not exist (yet) waits by its file name.
        let missing = native_attach_wait(AdapterKind::LldbDap, "w", "/nowhere/server", &[]);
        assert_eq!(missing["attachCommands"], json!(["process attach --name \"server\" --waitfor"]));
        let code = native_attach_wait(AdapterKind::CodeLldb, "w", "server", &[]);
        assert_eq!((code["type"].clone(), code["waitFor"].clone()), (json!("lldb"), json!(true)));
        assert!(valid_wait_name("app"));
        for bad in ["", "  ", "a\nb", "x\0"] {
            assert!(!valid_wait_name(bad));
        }
    }

    #[test]
    fn lldb_dap_env_is_a_list_and_codelldb_env_an_object() {
        let launch = NativeLaunch {
            name: "app".into(),
            program: "/p/app".into(),
            env: vec![("RUST_BACKTRACE".into(), "1".into())],
            ..Default::default()
        };
        let lldb = native(AdapterKind::LldbDap, &launch);
        assert_eq!(lldb["env"], json!(["RUST_BACKTRACE=1"]));
        assert_eq!(lldb["program"], json!("/p/app"));
        assert!(lldb.get("cwd").is_none());
        let code = native(AdapterKind::CodeLldb, &launch);
        assert_eq!(code["env"], json!({"RUST_BACKTRACE": "1"}));
        assert_eq!(code["type"], json!("lldb"));
    }

    #[test]
    fn platform_and_python_remote_arguments() {
        let v = native_platform(
            AdapterKind::LldbDap,
            "p",
            "remote-linux",
            "connect://10.0.0.2:1234",
            std::path::Path::new("/p/app"),
            &["--x".into()],
            &["settings set a b".into()],
        );
        assert_eq!(v["request"], json!("launch"));
        assert_eq!(
            v["initCommands"],
            json!(["settings set a b", "platform select remote-linux", "platform connect connect://10.0.0.2:1234"])
        );
        assert_eq!(v["args"], json!(["--x"]));
        assert!(valid_platform_url("connect://host:1"));
        assert!(!valid_platform_url("host:1"));
        assert!(!valid_platform_url("connect://host"));
        assert!(valid_platform_name("remote-linux"));
        assert!(!valid_platform_name("remote linux"));
        assert!(!valid_platform_name(""));
        let mapping = PathMapping { local_root: "/home/me/app".into(), remote_root: "/srv/app".into() };
        let r = python_remote("r", " 127.0.0.1 ", 5678, &[mapping], false);
        assert_eq!(r["connect"], json!({"host": "127.0.0.1", "port": 5678}));
        assert_eq!(r["pathMappings"], json!([{"localRoot": "/home/me/app", "remoteRoot": "/srv/app"}]));
        assert_eq!(r["justMyCode"], json!(false));
        assert!(python_remote("r", "h", 1, &[], true).get("pathMappings").is_none());
    }

    #[test]
    fn python_flags() {
        let v = python(&PythonLaunch { sub_process: true, django: true, ..Default::default() });
        assert_eq!((v["subProcess"].clone(), v["django"].clone()), (json!(true), json!(true)));
        assert!(v.get("jinja").is_none());
        let v = python(&PythonLaunch { jinja: true, ..Default::default() });
        assert_eq!((v["subProcess"].clone(), v["jinja"].clone()), (json!(false), json!(true)));
    }

    #[test]
    fn python_module_wins_over_program() {
        let launch = PythonLaunch {
            program: Some("/a.py".into()),
            module: Some("pkg.main".into()),
            just_my_code: true,
            ..Default::default()
        };
        let v = python(&launch);
        assert_eq!(v["module"], json!("pkg.main"));
        assert!(v.get("program").is_none());
        assert_eq!(v["justMyCode"], json!(true));
        assert_eq!(v["console"], json!("internalConsole"));
    }
}
