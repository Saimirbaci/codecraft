//! CodeCraft profiler model: samply/gecko, speedscope and collapsed-stack parsing into call trees
//! and flame graphs.
//!
//! [`load`] reads a snapshot of any supported format (gzip-compressed or not) into a
//! [`Profile`]: interned [`Frame`]s, a prefix tree of stacks and per-thread merged sample
//! weights. [`Analysis`] derives what the Profiler tool window shows for a thread selection:
//! the top-down [`CallTree`], the [`FlameGraph`] layout and the bottom-up method list.
//!
//! Every parser treats its input as hostile: indices are bounds-checked, stack tables with
//! cycles or absurd depths are cut, and sizes are capped. Nothing here panics.

mod analysis;
pub mod callgraph;
pub mod collapsed;
pub mod gecko;
mod model;
pub mod perf;
pub mod python;
pub mod speedscope;
pub mod xctrace;
pub mod xml;

use std::io::Read;
use std::path::Path;

pub use analysis::{
    Analysis, CallNode, CallTree, Edge, FlameGraph, FlameRect, FunctionDelta, MethodStat, callees, callers, compare,
    matched_weight, methods, outermost_matches, path_hashes, path_weights, search, subtree_methods,
};
pub use model::{
    Format, Frame, LineStat, MAX_DEPTH, Profile, ProfileBuilder, StackNode, TIMELINE_BUCKETS, Thread, Unit,
    fold_lines, is_address,
};

/// Largest snapshot read from disk (compressed size).
pub const MAX_FILE_BYTES: u64 = 1 << 30;
/// Largest decompressed snapshot.
pub const MAX_DECOMPRESSED_BYTES: u64 = 2 << 30;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("cannot read the snapshot: {0}")]
    Io(String),
    #[error("not a valid profile: {0}")]
    Format(String),
    #[error("the profile is too large: {0}")]
    TooLarge(String),
    #[error("the profile has no samples")]
    Empty,
    #[error(
        "unrecognized profile format (supported: samply / Firefox Profiler JSON, speedscope JSON, collapsed stacks, perf script output, DTrace aggregations, xctrace Time Profiler exports, macOS sample / malloc_history call graphs, CodeCraft cProfile / tracemalloc JSON)"
    )]
    Unrecognized,
}

/// Read and parse a snapshot file.
pub fn load(path: &Path) -> Result<Profile, ProfileError> {
    let file = std::fs::File::open(path).map_err(|e| ProfileError::Io(format!("{}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).map_err(|e| ProfileError::Io(e.to_string()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(ProfileError::TooLarge(format!("more than {} MB on disk", MAX_FILE_BYTES >> 20)));
    }
    parse(&bytes)
}

/// Parse a snapshot from bytes, detecting the format from the content.
pub fn parse(bytes: &[u8]) -> Result<Profile, ProfileError> {
    let unpacked;
    let bytes = if bytes.starts_with(&[0x1f, 0x8b]) {
        unpacked = gunzip(bytes)?;
        unpacked.as_slice()
    } else {
        bytes
    };
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let first = bytes.iter().find(|b| !b.is_ascii_whitespace()).copied();
    if matches!(first, Some(b'{' | b'[')) {
        let v: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| ProfileError::Format(e.to_string()))?;
        match python::kind(&v) {
            Some("cprofile") => return python::parse_cprofile(&v),
            Some("tracemalloc") => return python::parse_tracemalloc(&v),
            _ => {}
        }
        if speedscope::looks_speedscope(&v) {
            // py-spy and memray record the line each sample was at.
            return speedscope::parse(&v).map(fold_lines);
        }
        if gecko::looks_gecko(&v) {
            return gecko::parse(&v);
        }
        if v.is_array() || v.get("traceEvents").is_some() {
            return Err(ProfileError::Format(
                "Chrome trace event files aren't supported; convert to speedscope or collapsed stacks".into(),
            ));
        }
        return Err(ProfileError::Unrecognized);
    }
    let text = String::from_utf8_lossy(bytes);
    if text.trim().is_empty() {
        return Err(ProfileError::Empty);
    }
    if first == Some(b'<') && xctrace::looks_xctrace(&text) {
        return xctrace::parse(&text);
    }
    if callgraph::looks_callgraph(&text) {
        return callgraph::parse(&text);
    }
    if perf::looks_perf_script(&text) {
        return perf::parse(&text);
    }
    if collapsed::looks_collapsed(&text) {
        return collapsed::parse(&text).map(fold_lines);
    }
    if collapsed::looks_dtrace(&text) {
        return collapsed::parse_dtrace(&text);
    }
    Err(ProfileError::Unrecognized)
}

fn gunzip(bytes: &[u8]) -> Result<Vec<u8>, ProfileError> {
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(bytes)
        .take(MAX_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|e| ProfileError::Format(format!("bad gzip data: {e}")))?;
    if out.len() as u64 > MAX_DECOMPRESSED_BYTES {
        return Err(ProfileError::TooLarge(format!("more than {} MB uncompressed", MAX_DECOMPRESSED_BYTES >> 20)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn detects_every_format() {
        let processed = serde_json::to_vec(&serde_json::json!({
            "meta": { "preprocessedProfileVersion": 47 },
            "shared": { "stringArray": ["main"] },
            "threads": [{ "name": "t", "samples": { "stack": [0] }, "stackTable": { "prefix": [null], "frame": [0] },
                          "frameTable": { "func": [0] }, "funcTable": { "name": [0] } }]
        }))
        .unwrap();
        assert_eq!(parse(&processed).unwrap().format, Format::Processed);
        let ss =
            br#"{"$schema":"https://www.speedscope.app/file-format-schema.json","shared":{"frames":[{"name":"a"}]},
                      "profiles":[{"type":"sampled","unit":"none","samples":[[0]],"weights":[3]}]}"#;
        assert_eq!(parse(ss).unwrap().format, Format::Speedscope);
        assert_eq!(parse(b"\xEF\xBB\xBFmain;a 1\n").unwrap().format, Format::Collapsed);
        assert_eq!(parse(b"app 1/1 0.1: x\n\t10 f (/a)\n").unwrap().format, Format::PerfScript);
        assert_eq!(parse(b"\n  a`f\n  3\n").unwrap().format, Format::Dtrace);
        let xc = include_bytes!("../tests/fixtures/xctrace-time-profile.xml");
        assert_eq!(parse(xc).unwrap().format, Format::Xctrace);
        let cg = include_bytes!("../tests/fixtures/sample-callgraph.txt");
        assert_eq!(parse(cg).unwrap().format, Format::CallGraph);
        let cp = br#"{"codecraft":"cprofile","functions":[{"name":"f","file":"a.py","line":1,"tt":0.5,"ct":0.5,"callers":[]}]}"#;
        assert_eq!(parse(cp).unwrap().format, Format::CProfile);
        let tm = br#"{"codecraft":"tracemalloc","traces":[{"size":5,"frames":[{"name":"f","file":"a.py","line":3}]}]}"#;
        assert_eq!(parse(tm).unwrap().format, Format::Tracemalloc);
        // py-spy collapsed frames carry lines: folded into one function with line statistics.
        let p = parse(b"main (a.py:1);work (a.py:5) 3\nmain (a.py:1);work (a.py:6) 1\n").unwrap();
        assert_eq!(p.frames.len(), 2);
        assert_eq!(p.lines.len(), 3);
        assert_eq!(p.frames[1].line, Some(5));
    }

    #[test]
    fn rejects_garbage_cleanly() {
        assert!(matches!(parse(b""), Err(ProfileError::Empty)));
        assert!(matches!(parse(b"hello"), Err(ProfileError::Unrecognized)));
        assert!(matches!(parse(b"{\"a\": 1}"), Err(ProfileError::Unrecognized)));
        assert!(matches!(parse(b"[{\"ph\":\"X\"}]"), Err(ProfileError::Format(_))));
        assert!(matches!(parse(b"{\"threads\": [ {\"name\": "), Err(ProfileError::Format(_))));
        assert!(matches!(parse(&[0x1f, 0x8b, 0, 1, 2]), Err(ProfileError::Format(_))));
        assert!(parse(&[0xff, 0xfe, 0x00]).is_err());
        let nested = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(parse(nested.as_bytes()).is_err());
    }

    #[test]
    fn loads_gzip_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.json.gz");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(b"main;work 5\nmain 1\n").unwrap();
        std::fs::write(&path, enc.finish().unwrap()).unwrap();
        let p = load(&path).unwrap();
        assert_eq!(p.total(), 6);
        assert!(matches!(load(&dir.path().join("missing")), Err(ProfileError::Io(_))));
    }
}
