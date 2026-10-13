//! CodeCraft HTTP Client: `.http`/`.rest` request files, environments, cURL and Postman
//! conversion, and running requests.
//!
//! The file format follows the behaviour of JetBrains' HTTP Client as described in its public
//! documentation: requests separated by `###`, a request line (`METHOD URL [HTTP/version]`),
//! headers, a body after a blank line, `{{variables}}` from `http-client.env.json` and
//! `http-client.private.env.json`, file variables (`@name = value`), body files (`< ./file`),
//! request names (`# @name`) and comments (`#`, `//`). Response handler scripts (`> {% … %}`)
//! are parsed and kept but never run.
//!
//! Everything here accepts arbitrary input and never panics. Line numbers are 0-based.
#![deny(clippy::indexing_slicing, clippy::string_slice)]

pub mod client;
pub mod curl;
pub mod env;
pub mod format;
pub mod history;
pub mod parse;
pub mod postman;
pub mod runner;
mod util;
pub mod vars;

pub use client::{ExecOptions, Response, execute};
pub use env::Environments;
pub use parse::{BodyPart, FileVariable, HttpFile, LineContext, Request, line_context, parse};
pub use vars::{ResolvedRequest, Variables, resolve};

/// Errors of the HTTP client.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    /// The request could not be built (bad URL, unreadable body file, …).
    #[error("{0}")]
    Request(String),
    /// Connecting, sending or receiving failed.
    #[error("{0}")]
    Transport(String),
    /// Reading or writing a file failed.
    #[error("{0}")]
    Io(String),
    /// Malformed input (cURL command, Postman collection, environment file).
    #[error("{0}")]
    Parse(String),
}

/// Is `path` an HTTP request file (`.http` or `.rest`)?
pub fn is_http_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("http") || e.eq_ignore_ascii_case("rest"))
}

/// Request methods offered by completion and recognised on request lines.
pub const METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT"];

/// Common request header names offered by completion.
pub const HEADERS: &[&str] = &[
    "Accept",
    "Accept-Charset",
    "Accept-Encoding",
    "Accept-Language",
    "Authorization",
    "Cache-Control",
    "Connection",
    "Content-Disposition",
    "Content-Encoding",
    "Content-Length",
    "Content-Type",
    "Cookie",
    "Date",
    "Expect",
    "Forwarded",
    "From",
    "Host",
    "If-Match",
    "If-Modified-Since",
    "If-None-Match",
    "If-Range",
    "If-Unmodified-Since",
    "Keep-Alive",
    "Origin",
    "Pragma",
    "Prefer",
    "Proxy-Authorization",
    "Range",
    "Referer",
    "TE",
    "Upgrade",
    "User-Agent",
    "Via",
    "X-API-Key",
    "X-Correlation-ID",
    "X-Forwarded-For",
    "X-Forwarded-Host",
    "X-Forwarded-Proto",
    "X-Request-ID",
    "X-Requested-With",
];

/// Common media types offered after `Content-Type:` and `Accept:`.
pub const MEDIA_TYPES: &[&str] = &[
    "application/json",
    "application/xml",
    "application/x-www-form-urlencoded",
    "application/octet-stream",
    "application/graphql",
    "application/javascript",
    "application/pdf",
    "multipart/form-data",
    "text/plain",
    "text/html",
    "text/csv",
    "text/xml",
    "image/png",
    "image/jpeg",
    "*/*",
];

/// Dynamic variables offered by completion after `{{`.
pub const DYNAMIC_VARIABLES: &[&str] = &[
    "$uuid",
    "$random.uuid",
    "$timestamp",
    "$isoTimestamp",
    "$randomInt",
    "$random.integer(0, 100)",
    "$random.alphabetic(10)",
    "$random.alphanumeric(10)",
    "$random.hexadecimal(10)",
    "$random.email",
    "$processEnv.HOME",
];
