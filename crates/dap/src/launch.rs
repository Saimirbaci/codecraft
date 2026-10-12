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
    }));
    if let Some(map) = value.as_object_mut() {
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

#[cfg(test)]
mod tests {
    use super::*;

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
