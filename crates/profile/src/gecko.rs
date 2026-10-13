//! Firefox Profiler formats: the "processed" profile samply saves (column tables: `stackTable`,
//! `frameTable`, `funcTable`, `resourceTable`, strings in `stringArray` or `shared.stringArray`)
//! and the Gecko profile (row tables with a `schema` and `data`, strings in `stringTable`,
//! subprocesses in `processes`).

use serde_json::{Map, Value};

use crate::ProfileError;
use crate::model::{Format, Frame, MAX_DEPTH, Profile, ProfileBuilder, Unit, scaled_weight};

/// Nested `processes` followed at most this deep.
const MAX_PROCESS_DEPTH: usize = 16;

/// Whether a JSON document looks like a Firefox Profiler / Gecko profile.
pub fn looks_gecko(v: &Value) -> bool {
    v.get("threads").is_some_and(Value::is_array) || v.get("processes").is_some_and(Value::is_array)
}

pub fn parse(v: &Value) -> Result<Profile, ProfileError> {
    let processed = v.pointer("/meta/preprocessedProfileVersion").is_some()
        || v.get("shared").is_some()
        || v.pointer("/threads/0/frameTable/func").is_some();
    let mut b = ProfileBuilder::new(if processed { Format::Processed } else { Format::Gecko }, Unit::Samples);
    let mut unit_set = false;
    let shared = v.get("shared");
    let mut work: Vec<(&Value, usize)> = vec![(v, 0)];
    while let Some((process, depth)) = work.pop() {
        let libs = process.get("libs").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        for t in process.get("threads").and_then(Value::as_array).into_iter().flatten() {
            read_thread(&mut b, t, shared, libs, &mut unit_set)?;
        }
        if depth < MAX_PROCESS_DEPTH {
            for p in process.get("processes").and_then(Value::as_array).into_iter().flatten().rev() {
                work.push((p, depth + 1));
            }
        }
    }
    b.finish()
}

/// A table in either layout.
enum Table<'a> {
    /// `{col: [..], length}` (processed).
    Columns(&'a Map<String, Value>),
    /// `{schema: {col: index}, data: [[..]]}` (Gecko).
    Rows { schema: &'a Map<String, Value>, data: &'a [Value] },
}

impl<'a> Table<'a> {
    fn of(v: Option<&'a Value>) -> Option<Self> {
        let obj = v?.as_object()?;
        match (obj.get("schema").and_then(Value::as_object), obj.get("data").and_then(Value::as_array)) {
            (Some(schema), Some(data)) => Some(Table::Rows { schema, data }),
            _ => Some(Table::Columns(obj)),
        }
    }

    /// Rows actually present (never the declared `length` alone, which input controls).
    fn len(&self) -> usize {
        match self {
            Table::Rows { data, .. } => data.len(),
            Table::Columns(obj) => {
                let longest = obj.values().filter_map(Value::as_array).map(Vec::len).max().unwrap_or(0);
                match obj.get("length").and_then(Value::as_u64) {
                    Some(n) => usize::try_from(n).unwrap_or(usize::MAX).min(longest),
                    None => longest,
                }
            }
        }
    }

    fn get(&self, col: &str, row: usize) -> Option<&'a Value> {
        match self {
            Table::Rows { schema, data } => {
                let i = usize::try_from(schema.get(col)?.as_u64()?).ok()?;
                data.get(row)?.as_array()?.get(i)
            }
            Table::Columns(obj) => obj.get(col)?.as_array()?.get(row),
        }
        .filter(|v| !v.is_null())
    }

    fn index(&self, col: &str, row: usize) -> Option<usize> {
        self.get(col, row)?.as_u64().and_then(|n| usize::try_from(n).ok())
    }

    fn has(&self, col: &str) -> bool {
        match self {
            Table::Rows { schema, .. } => schema.contains_key(col),
            Table::Columns(obj) => obj.contains_key(col),
        }
    }
}

/// A thread's table, else the profile's shared one.
fn table<'a>(thread: &'a Value, shared: Option<&'a Value>, name: &str) -> Option<Table<'a>> {
    Table::of(thread.get(name)).or_else(|| Table::of(shared.and_then(|s| s.get(name))))
}

#[derive(Clone, Copy)]
enum Memo {
    Unvisited,
    InProgress,
    Done(Option<u32>),
}

fn read_thread(
    b: &mut ProfileBuilder,
    t: &Value,
    shared: Option<&Value>,
    libs: &[Value],
    unit_set: &mut bool,
) -> Result<(), ProfileError> {
    let strings: Vec<&str> = t
        .get("stringArray")
        .or_else(|| t.get("stringTable"))
        .or_else(|| shared.and_then(|s| s.get("stringArray")))
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|s| s.as_str().unwrap_or("")).collect())
        .unwrap_or_default();
    let (Some(stacks), Some(frames), Some(samples)) =
        (table(t, shared, "stackTable"), table(t, shared, "frameTable"), Table::of(t.get("samples")))
    else {
        return Ok(());
    };
    let funcs = table(t, shared, "funcTable");
    let resources = table(t, shared, "resourceTable");
    let sources = table(t, shared, "sources");
    let ctx = FrameCtx {
        strings: &strings,
        frames: &frames,
        funcs: funcs.as_ref(),
        resources: resources.as_ref(),
        sources: sources.as_ref(),
        libs,
    };

    let name = t.get("name").and_then(Value::as_str).unwrap_or("Thread");
    let name =
        match t.get("tid").and_then(|v| v.as_u64().map(|n| n.to_string()).or_else(|| v.as_str().map(str::to_string))) {
            Some(tid) => format!("{name} ({tid})"),
            None => name.to_string(),
        };
    let thread = b.thread(name)?;

    let (unit, scale) =
        match samples.get("weightType", 0).or_else(|| t.pointer("/samples/weightType")).and_then(Value::as_str) {
            Some("tracing-ms") => (Unit::Nanoseconds, 1e6),
            Some("bytes") => (Unit::Bytes, 1.0),
            _ => (Unit::Samples, 1.0),
        };
    if !*unit_set {
        b.set_unit(unit);
        *unit_set = true;
    }
    // The weight column is either per sample (array) or absent/null (every sample weighs 1).
    let weights = t.pointer("/samples/weight").and_then(Value::as_array);
    let row_weight = samples.has("weight") && matches!(samples, Table::Rows { .. });

    let mut frame_ids: Vec<Option<Option<u32>>> = vec![None; frames.len()];
    let mut memo: Vec<Memo> = vec![Memo::Unvisited; stacks.len()];
    let mut chain: Vec<usize> = Vec::new();
    for row in 0..samples.len() {
        let Some(stack_row) = samples.index("stack", row) else { continue };
        let weight = match (weights, row_weight) {
            (Some(w), _) => w.get(row).and_then(Value::as_f64).map_or(0, |x| scaled_weight(x, scale)),
            (None, true) => samples.get("weight", row).and_then(Value::as_f64).map_or(1, |x| scaled_weight(x, scale)),
            (None, false) => 1,
        };
        if weight == 0 {
            continue;
        }
        let stack = resolve(b, &stacks, &ctx, &mut frame_ids, &mut memo, &mut chain, stack_row)?;
        if let Some(s) = stack {
            b.sample(thread, s, weight);
        }
    }
    Ok(())
}

/// Our stack id for an input stack row, interning its whole prefix chain. Cycles and chains
/// deeper than [`MAX_DEPTH`] are cut; rows out of range end the chain.
fn resolve(
    b: &mut ProfileBuilder,
    stacks: &Table<'_>,
    ctx: &FrameCtx<'_>,
    frame_ids: &mut [Option<Option<u32>>],
    memo: &mut [Memo],
    chain: &mut Vec<usize>,
    row: usize,
) -> Result<Option<u32>, ProfileError> {
    chain.clear();
    let mut base: Option<u32> = None;
    let mut cur = Some(row);
    while let Some(r) = cur {
        match memo.get(r).copied() {
            None | Some(Memo::InProgress) => break,
            Some(Memo::Done(id)) => {
                base = id;
                break;
            }
            Some(Memo::Unvisited) => {
                if chain.len() >= MAX_DEPTH {
                    break;
                }
                if let Some(m) = memo.get_mut(r) {
                    *m = Memo::InProgress;
                }
                chain.push(r);
                cur = stacks.index("prefix", r);
            }
        }
    }
    for &r in chain.iter().rev() {
        let frame = match stacks.index("frame", r) {
            Some(f) => match frame_ids.get(f).copied() {
                Some(Some(id)) => id,
                Some(None) => {
                    let id = match ctx.frame(f) {
                        Some(fr) => Some(b.frame(fr)?),
                        None => None,
                    };
                    if let Some(slot) = frame_ids.get_mut(f) {
                        *slot = Some(id);
                    }
                    id
                }
                None => None,
            },
            None => None,
        };
        if let Some(f) = frame {
            base = Some(b.stack(base, f)?);
        }
        if let Some(m) = memo.get_mut(r) {
            *m = Memo::Done(base);
        }
    }
    Ok(base)
}

struct FrameCtx<'a> {
    strings: &'a [&'a str],
    frames: &'a Table<'a>,
    funcs: Option<&'a Table<'a>>,
    resources: Option<&'a Table<'a>>,
    /// Newer processed profiles name files through `shared.sources` (`funcTable.source`).
    sources: Option<&'a Table<'a>>,
    libs: &'a [Value],
}

impl FrameCtx<'_> {
    fn string(&self, i: Option<usize>) -> Option<&str> {
        self.strings.get(i?).copied().filter(|s| !s.is_empty())
    }

    fn lib_name(&self, lib: usize) -> Option<String> {
        let l = self.libs.get(lib)?;
        l.get("name").or_else(|| l.get("debugName")).and_then(Value::as_str).map(str::to_string)
    }

    /// The library whose `[start, end)` holds an address (Gecko libs carry their ranges).
    fn lib_for_address(&self, addr: u64) -> Option<String> {
        self.libs.iter().find_map(|l| {
            let (start, end) = (l.get("start")?.as_u64()?, l.get("end")?.as_u64()?);
            (start <= addr && addr < end).then(|| l.get("name").and_then(Value::as_str).map(str::to_string)).flatten()
        })
    }

    fn frame(&self, row: usize) -> Option<Frame> {
        let fr = self.frames;
        if let (Some(funcs), Some(func)) = (self.funcs, fr.index("func", row)) {
            // Processed: the function holds the name, file and library.
            let mut frame = Frame {
                name: self.string(funcs.index("name", func)).unwrap_or("").to_string(),
                module: None,
                file: self
                    .string(funcs.index("fileName", func))
                    .or_else(|| {
                        let src = funcs.index("source", func)?;
                        let sources = self.sources?;
                        self.string(sources.index("filename", src).or_else(|| sources.index("fileName", src)))
                    })
                    .map(str::to_string),
                line: funcs.get("lineNumber", func).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()),
            };
            if let (Some(res), Some(r)) = (self.resources, funcs.index("resource", func)) {
                frame.module = res
                    .index("lib", r)
                    .and_then(|l| self.lib_name(l))
                    .or_else(|| self.string(res.index("name", r)).map(str::to_string));
            }
            if frame.name.is_empty() {
                frame.name = match fr.get("address", row).and_then(Value::as_u64) {
                    Some(a) => format!("0x{a:x}"),
                    None => "[unknown]".to_string(),
                };
            }
            return Some(frame);
        }
        // Gecko: a location string.
        let location = self.string(fr.index("location", row))?;
        let mut frame = crate::collapsed::parse_frame(location);
        if frame.module.is_none()
            && let Some(hex) = frame.name.strip_prefix("0x")
            && let Ok(addr) = u64::from_str_radix(hex, 16)
        {
            frame.module = self.lib_for_address(addr);
        }
        if frame.line.is_none() && frame.file.is_some() {
            frame.line = fr.get("line", row).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
        }
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A processed profile like samply writes: shared strings, column tables, one lib.
    pub(crate) fn processed() -> Value {
        json!({
            "meta": { "version": 24, "preprocessedProfileVersion": 47, "interval": 1.0 },
            "libs": [{ "name": "app", "path": "/bin/app" }, { "name": "libsystem_c.dylib" }],
            "shared": { "stringArray": ["main", "compute", "0x1f00", "src/main.rs", "app"] },
            "threads": [
                {
                    "name": "main", "tid": 77,
                    "samples": { "length": 4, "stack": [2, 2, 1, null], "weight": null, "weightType": "samples" },
                    "stackTable": { "length": 3, "prefix": [null, 0, 1], "frame": [0, 1, 2] },
                    "frameTable": { "length": 3, "func": [0, 1, 2], "address": [16, 32, 7936], "line": [null, 12, null] },
                    "funcTable": { "length": 3, "name": [0, 1, 2], "resource": [0, 0, 1], "fileName": [3, 3, null], "lineNumber": [1, 10, null] },
                    "resourceTable": { "length": 2, "lib": [0, 1], "name": [4, 4] }
                },
                {
                    "name": "worker", "tid": 78,
                    "samples": { "length": 2, "stack": [0, 0], "weight": [3, 4] },
                    "stackTable": { "length": 1, "prefix": [null], "frame": [0] },
                    "frameTable": { "length": 1, "func": [0], "address": [16] },
                    "funcTable": { "length": 1, "name": [0], "resource": [0], "fileName": [3], "lineNumber": [1] },
                    "resourceTable": { "length": 1, "lib": [0], "name": [4] }
                }
            ]
        })
    }

    #[test]
    fn processed_profile() {
        let p = parse(&processed()).unwrap();
        assert_eq!(p.format, Format::Processed);
        let names: Vec<&str> = p.threads.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["main (77)", "worker (78)"]);
        assert_eq!(p.threads[0].total(), 3);
        assert_eq!(p.threads[1].total(), 7);
        let compute = p.frames.iter().find(|f| f.name == "compute").unwrap();
        assert_eq!(compute.module.as_deref(), Some("app"));
        assert_eq!(compute.location().as_deref(), Some("src/main.rs:10"));
        let unsym = p.frames.iter().find(|f| f.name == "0x1f00").unwrap();
        assert_eq!(unsym.module.as_deref(), Some("libsystem_c.dylib"));
        // `main` from both threads is one frame, one stack.
        assert_eq!(p.frames.iter().filter(|f| f.name == "main").count(), 1);
        assert_eq!(p.threads[1].weights[0].0, 0);
    }

    #[test]
    fn processed_without_names_shows_addresses() {
        let v = json!({
            "meta": { "preprocessedProfileVersion": 50 },
            "libs": [{ "name": "libfoo.so" }],
            "threads": [{
                "name": "t", "stringArray": [""],
                "samples": { "stack": [0] },
                "stackTable": { "prefix": [null], "frame": [0] },
                "frameTable": { "func": [0], "address": [4096] },
                "funcTable": { "name": [0], "resource": [0] },
                "resourceTable": { "lib": [0] }
            }]
        });
        let p = parse(&v).unwrap();
        assert_eq!(p.frames[0].name, "0x1000");
        assert_eq!(p.frames[0].label(), "0x1000 (libfoo.so)");
    }

    #[test]
    fn processed_with_shared_sources() {
        let v = json!({
            "meta": { "preprocessedProfileVersion": 55 },
            "shared": { "stringArray": ["work", "/src/lib.rs"], "sources": { "length": 1, "filename": [1] } },
            "threads": [{
                "name": "t",
                "samples": { "stack": [0], "weight": null },
                "stackTable": { "prefix": [null], "frame": [0] },
                "frameTable": { "func": [0] },
                "funcTable": { "name": [0], "source": [0], "lineNumber": [12] }
            }]
        });
        let p = parse(&v).unwrap();
        assert_eq!(p.frames[0].location().as_deref(), Some("/src/lib.rs:12"));
    }

    #[test]
    fn gecko_profile_with_subprocess() {
        let v = json!({
            "meta": { "version": 27, "interval": 1 },
            "libs": [{ "name": "libxul.so", "start": 4096, "end": 8192 }],
            "threads": [{
                "name": "GeckoMain", "tid": 1,
                "stringTable": ["main (in firefox)", "0x1100", "run (app.js:7:3)"],
                "samples": { "schema": { "stack": 0, "time": 1 }, "data": [[1, 0.0], [2, 1.0], [null, 2.0]] },
                "stackTable": { "schema": { "prefix": 0, "frame": 1 }, "data": [[null, 0], [0, 1], [0, 2]] },
                "frameTable": { "schema": { "location": 0, "line": 4 }, "data": [[0, false, 0, null, null], [1], [2, false, 0, null, 9]] }
            }],
            "processes": [{
                "libs": [],
                "threads": [{
                    "name": "Child", "stringTable": ["x"],
                    "samples": { "schema": { "stack": 0, "weight": 1 }, "data": [[0, 5]] },
                    "stackTable": { "schema": { "prefix": 0, "frame": 1 }, "data": [[null, 0]] },
                    "frameTable": { "schema": { "location": 0 }, "data": [[0]] }
                }]
            }]
        });
        assert!(looks_gecko(&v));
        let p = parse(&v).unwrap();
        assert_eq!(p.format, Format::Gecko);
        assert_eq!(p.threads.len(), 2);
        assert_eq!(p.threads[0].total(), 2);
        assert_eq!(p.threads[1].total(), 5);
        let main = p.frames.iter().find(|f| f.name == "main").unwrap();
        assert_eq!(main.module.as_deref(), Some("firefox"));
        let addr = p.frames.iter().find(|f| f.name == "0x1100").unwrap();
        assert_eq!(addr.module.as_deref(), Some("libxul.so"));
        let js = p.frames.iter().find(|f| f.name == "run").unwrap();
        assert_eq!((js.file.as_deref(), js.line), (Some("app.js"), Some(7)));
    }

    #[test]
    fn hostile_tables_never_panic() {
        // A prefix cycle (0 → 1 → 0), self-loops, huge and negative indices, wrong types.
        let v = json!({
            "shared": { "stringArray": ["a", "b"] },
            "threads": [{
                "samples": { "length": 1_000_000_000_000u64, "stack": [0, 1, 2, 99999999999u64, -1, "x", 3], "weight": [1, 1, 1, 1, 1, 1, 1e308] },
                "stackTable": { "length": 4_000_000_000u64, "prefix": [1, 0, 2, 0], "frame": [0, 1, 0, 99] },
                "frameTable": { "length": 2, "func": [0, 1], "address": "nope" },
                "funcTable": { "name": [0, 1, 7], "resource": [5, -1] },
                "resourceTable": { "lib": [99] }
            }, { "samples": 5 }, 7, { "stackTable": [], "frameTable": {}, "samples": {} }]
        });
        let p = parse(&v).unwrap();
        assert!(p.total() > 0);
        for s in 0..p.stacks.len() {
            assert!(p.stack_frames(u32::try_from(s).unwrap()).len() <= 4);
        }
        assert!(parse(&json!({ "threads": [] })).is_err());
        assert!(parse(&json!({ "threads": "x" })).is_err());
        // Deep chain: cut at MAX_DEPTH.
        let n = 5000usize;
        let prefix: Vec<Value> = (0..n).map(|i| if i == 0 { Value::Null } else { json!(i - 1) }).collect();
        let v = json!({ "shared": { "stringArray": ["f"] }, "threads": [{
            "samples": { "stack": [n - 1] },
            "stackTable": { "prefix": prefix, "frame": vec![0; n] },
            "frameTable": { "func": [0] }, "funcTable": { "name": [0] }
        }]});
        let p = parse(&v).unwrap();
        assert_eq!(p.stacks.len(), MAX_DEPTH);
    }
}
