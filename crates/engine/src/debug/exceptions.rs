//! Exception breakpoints per language (View Breakpoints › exception breakpoints): Rust panics
//! (a function breakpoint on `rust_panic`, `rust_begin_unwind` when that symbol is missing), C++
//! `throw`/`catch` (lldb-dap's `cpp_throw`/`cpp_catch` filters) and Python raised / uncaught /
//! user-uncaught exceptions (debugpy's `raised`/`uncaught`/`userUnhandled` filters). The
//! settings are the project's (saved with the breakpoints) and apply to every adapter that offers
//! the filter; changing them during a session sends them at once.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_dap::{self as dap, AdapterKind, DapClient};

use super::{Pending, Runtime, RunSpec};
use crate::{CommandError, Session};

/// The function a Rust panic always goes through (`std::panicking::rust_panic`).
pub const RUST_PANIC: &str = "rust_panic";
/// The `#[panic_handler]` of `std`, for binaries where `rust_panic` is not a symbol.
pub const RUST_BEGIN_UNWIND: &str = "rust_begin_unwind";

/// Which exceptions stop the program (besides Rust panics, `DebugState::break_on_panic`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExceptionBreakpoints {
    /// C++: stop where an exception is thrown (lldb `cpp_throw`).
    pub cpp_throw: bool,
    /// C++: stop where an exception is caught (lldb `cpp_catch`).
    pub cpp_catch: bool,
    /// Python: stop on every raised exception (debugpy `raised`).
    pub python_raised: bool,
    /// Python: stop on exceptions nothing catches (debugpy `uncaught`, on by default).
    pub python_uncaught: bool,
    /// Python: stop when an exception raised in your code escapes into library code (debugpy
    /// `userUnhandled`).
    pub python_user_uncaught: bool,
}

impl Default for ExceptionBreakpoints {
    fn default() -> Self {
        Self { cpp_throw: false, cpp_catch: false, python_raised: false, python_uncaught: true, python_user_uncaught: false }
    }
}

impl ExceptionBreakpoints {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The adapter filter ids these settings turn on.
    fn wanted(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (on, filter) in [
            (self.cpp_throw, "cpp_throw"),
            (self.cpp_catch, "cpp_catch"),
            (self.python_raised, "raised"),
            (self.python_uncaught, "uncaught"),
            (self.python_user_uncaught, "userUnhandled"),
        ] {
            if on {
                out.push(filter);
            }
        }
        out
    }

    /// Apply `{cppThrow?, cppCatch?, pythonRaised?, pythonUncaught?, pythonUserUncaught?}`.
    fn fill(&mut self, p: &Value) -> Result<bool, CommandError> {
        let mut changed = false;
        for (key, field) in [
            ("cppThrow", &mut self.cpp_throw),
            ("cppCatch", &mut self.cpp_catch),
            ("pythonRaised", &mut self.python_raised),
            ("pythonUncaught", &mut self.python_uncaught),
            ("pythonUserUncaught", &mut self.python_user_uncaught),
        ] {
            match p.get(key) {
                None | Some(Value::Null) => {}
                Some(Value::Bool(b)) => {
                    changed |= *field != *b;
                    *field = *b;
                }
                Some(_) => return Err(CommandError::BadParams(format!("`{key}` must be true or false"))),
            }
        }
        Ok(changed)
    }
}

/// The adapter's own Rust panic filter (CodeLLDB's `rust_panic`; LLDB builds with Rust support).
const RUST_PANIC_FILTER: &str = "rust_panic";

/// The filters to send to an adapter: the settings' filters it offers (and its Rust panic filter
/// when panics stop). Adapters CodeCraft has no settings for (js-debug, others) get their own
/// defaults.
pub(crate) fn filters_for(
    kind: AdapterKind,
    capabilities: &dap::Capabilities,
    settings: &ExceptionBreakpoints,
    rust_panics: bool,
) -> Vec<String> {
    let offered = &capabilities.exception_breakpoint_filters;
    match kind {
        AdapterKind::LldbDap | AdapterKind::CodeLldb | AdapterKind::Debugpy => {
            let mut wanted = settings.wanted();
            if rust_panics {
                wanted.push(RUST_PANIC_FILTER);
            }
            offered.iter().filter(|f| wanted.contains(&f.filter.as_str())).map(|f| f.filter.clone()).collect()
        }
        _ => offered.iter().filter(|f| f.default).map(|f| f.filter.clone()).collect(),
    }
}

/// The adapter stops on Rust panics by itself (a `rust_panic` exception filter).
fn has_panic_filter(capabilities: &dap::Capabilities) -> bool {
    capabilities.exception_breakpoint_filters.iter().any(|f| f.filter == RUST_PANIC_FILTER)
}

/// Whether the session debugs a Rust program (panic breakpoints apply).
pub(super) fn rust_program(rt: &Runtime) -> bool {
    matches!(rt.plan.run, RunSpec::Native { rust: true, .. } | RunSpec::Attach { process: dap::processes::ProcessKind::Rust, .. })
}

/// Send the exception filters and the Rust panic breakpoint (at configuration and whenever the
/// settings change).
pub(super) fn send(rt: &mut Runtime, settings: &ExceptionBreakpoints, break_on_panic: bool) {
    let rust = rust_program(rt);
    if rust && rt.capabilities.supports_function_breakpoints && !has_panic_filter(&rt.capabilities) {
        let breakpoints: Vec<dap::FunctionBreakpoint> = if break_on_panic {
            vec![dap::FunctionBreakpoint { name: RUST_PANIC.into(), ..Default::default() }]
        } else {
            Vec::new()
        };
        rt.request(Pending::PanicBreakpoint { fallback: !break_on_panic }, |c| c.set_function_breakpoints(&breakpoints));
    }
    if !rt.capabilities.exception_breakpoint_filters.is_empty() {
        let filters = filters_for(rt.kind, &rt.capabilities, settings, rust && break_on_panic);
        rt.request(Pending::Setup, |c| c.set_exception_breakpoints(&filters));
    }
}

/// The same configuration for a child connection (debugpy subprocesses).
pub(crate) fn send_to(client: &DapClient, kind: AdapterKind, capabilities: &dap::Capabilities, settings: &ExceptionBreakpoints) {
    if !capabilities.exception_breakpoint_filters.is_empty() {
        let _ = client.set_exception_breakpoints(&filters_for(kind, capabilities, settings, false));
    }
}

/// The answer to the panic breakpoint: when `rust_panic` could not be set (no such symbol), try
/// `rust_begin_unwind` once.
pub(crate) fn on_panic_breakpoint(s: &mut Session, fallback: bool, result: Result<Value, String>) {
    let body = result.as_ref().ok().and_then(|b| dap::types::parse::<dap::BreakpointsBody>(b).ok());
    if let (Some(rt), Some(body)) = (s.debug.rt.as_mut(), &body) {
        rt.panic_breakpoints = body.breakpoints.iter().filter_map(|b| b.id).collect();
    }
    let verified = match result {
        Ok(_) => body.and_then(|b| b.breakpoints.first().map(|bp| bp.verified)).unwrap_or(true),
        Err(e) => {
            log::debug!("panic breakpoint: {e}");
            false
        }
    };
    if verified || fallback || !s.debug.break_on_panic {
        return;
    }
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let unwind = [dap::FunctionBreakpoint { name: RUST_BEGIN_UNWIND.into(), ..Default::default() }];
    rt.request(Pending::PanicBreakpoint { fallback: true }, |c| c.set_function_breakpoints(&unwind));
}

/// The settings as JSON (with the live adapter's offered filters).
pub(crate) fn state(s: &Session) -> Value {
    let available: Vec<Value> = s
        .debug
        .rt
        .as_ref()
        .map(|rt| {
            rt.capabilities
                .exception_breakpoint_filters
                .iter()
                .map(|f| json!({ "filter": f.filter, "label": f.label }))
                .collect()
        })
        .unwrap_or_default();
    let e = &s.debug.exceptions;
    json!({
        "rustPanic": s.debug.break_on_panic,
        "cppThrow": e.cpp_throw,
        "cppCatch": e.cpp_catch,
        "pythonRaised": e.python_raised,
        "pythonUncaught": e.python_uncaught,
        "pythonUserUncaught": e.python_user_uncaught,
        "adapterFilters": available,
    })
}

/// `debug.exceptionBreakpoints`: change any of the settings (and Rust panics with `rustPanic`),
/// save them, and send them to a running session.
pub(crate) fn update(s: &mut Session, p: &Value) -> Result<Value, CommandError> {
    super::ensure_loaded(s);
    let mut changed = s.debug.exceptions.fill(p)?;
    match p.get("rustPanic") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(b)) => {
            changed |= s.debug.break_on_panic != *b;
            s.debug.break_on_panic = *b;
        }
        Some(_) => return Err(CommandError::BadParams("`rustPanic` must be true or false".into())),
    }
    if changed {
        super::save(s);
        resend(s);
    }
    Ok(state(s))
}

/// Send the current settings to a configured session (after a change).
pub(crate) fn resend(s: &mut Session) {
    let settings = s.debug.exceptions.clone();
    let on_panic = s.debug.break_on_panic;
    let Some(rt) = s.debug.rt.as_mut() else { return };
    if !rt.configured || rt.client().is_none() {
        return;
    }
    send(rt, &settings, on_panic);
    let (kind, capabilities) = (rt.kind, rt.capabilities.clone());
    rt.children.each_configured(|client| send_to(client, kind, &capabilities, &settings));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(filters: &[(&str, bool)]) -> dap::Capabilities {
        dap::Capabilities {
            exception_breakpoint_filters: filters
                .iter()
                .map(|(f, d)| dap::ExceptionBreakpointsFilter { filter: (*f).into(), label: (*f).into(), default: *d })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn filters_follow_the_settings_and_what_the_adapter_offers() {
        let lldb = caps(&[("cpp_catch", false), ("cpp_throw", false), ("objc_throw", false)]);
        let py = caps(&[("raised", false), ("uncaught", true), ("userUnhandled", false)]);
        let mut e = ExceptionBreakpoints::default();
        assert!(filters_for(AdapterKind::LldbDap, &lldb, &e, true).is_empty());
        assert_eq!(filters_for(AdapterKind::Debugpy, &py, &e, false), ["uncaught"]);
        // CodeLLDB's own Rust panic filter, when panics stop.
        let code = caps(&[("rust_panic", false), ("cpp_throw", false)]);
        assert_eq!(filters_for(AdapterKind::CodeLldb, &code, &e, true), ["rust_panic"]);
        assert!(filters_for(AdapterKind::CodeLldb, &code, &e, false).is_empty());
        assert!(has_panic_filter(&code) && !has_panic_filter(&lldb));
        e.cpp_throw = true;
        e.python_raised = true;
        e.python_uncaught = false;
        assert_eq!(filters_for(AdapterKind::LldbDap, &lldb, &e, false), ["cpp_throw"]);
        assert_eq!(filters_for(AdapterKind::Debugpy, &py, &e, false), ["raised"]);
        // Other adapters keep their defaults.
        assert_eq!(filters_for(AdapterKind::JsDebug, &py, &e, false), ["uncaught"]);
    }

    #[test]
    fn settings_fill_from_params() {
        let mut e = ExceptionBreakpoints::default();
        assert!(e.fill(&json!({"cppThrow": true, "pythonUncaught": null})).unwrap());
        assert!(e.cpp_throw && e.python_uncaught);
        assert!(!e.fill(&json!({"cppThrow": true})).unwrap());
        assert!(e.fill(&json!({"cppCatch": "yes"})).is_err());
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["cppThrow"], json!(true));
        let back: ExceptionBreakpoints = serde_json::from_value(json!({"pythonRaised": true})).unwrap();
        assert!(back.python_raised && back.python_uncaught, "absent fields keep their defaults");
    }
}
