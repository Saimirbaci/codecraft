//! Typed access to JSON command params. Missing or mistyped params are `BadParams`, never a panic.

use std::path::PathBuf;

use serde_json::Value;

use crate::CommandError;

pub(crate) fn opt_str<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

pub(crate) fn req_str<'a>(p: &'a Value, key: &str) -> Result<&'a str, CommandError> {
    opt_str(p, key).ok_or_else(|| CommandError::BadParams(format!("missing string param `{key}`")))
}

pub(crate) fn opt_usize(p: &Value, key: &str) -> Result<Option<usize>, CommandError> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| CommandError::BadParams(format!("`{key}` must be a non-negative integer"))),
    }
}

pub(crate) fn req_usize(p: &Value, key: &str) -> Result<usize, CommandError> {
    opt_usize(p, key)?.ok_or_else(|| CommandError::BadParams(format!("missing integer param `{key}`")))
}

pub(crate) fn bool_or(p: &Value, key: &str, default: bool) -> bool {
    p.get(key).and_then(Value::as_bool).unwrap_or(default)
}

pub(crate) fn opt_path(p: &Value, key: &str) -> Option<PathBuf> {
    opt_str(p, key).filter(|s| !s.is_empty()).map(PathBuf::from)
}

pub(crate) fn req_path(p: &Value, key: &str) -> Result<PathBuf, CommandError> {
    opt_path(p, key).ok_or_else(|| CommandError::BadParams(format!("missing path param `{key}`")))
}
