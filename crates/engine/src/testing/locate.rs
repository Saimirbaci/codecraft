//! Where tests are defined: Rust tests by walking the module path from the target's crate root
//! (`a::b::tests::name` → `src/a/b.rs`, inline `mod tests`, `fn name`), doc tests from their
//! `file - item (line N)` name, pytest node ids, Zig `test "name"` blocks. Runs on a worker thread
//! (it reads source files); results are applied in `testing::pump`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{Framework, SuiteKind, TestLocation};

/// Files read for one locate job (a fallback scan of a big tree stays bounded).
const MAX_FILES_READ: usize = 2_000;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// A crate target the tests may belong to.
#[derive(Clone, Debug)]
pub(crate) struct TargetInfo {
    /// Crate name as in binary names (`-` → `_`).
    pub crate_name: String,
    pub src_path: PathBuf,
    /// The package directory (where `Cargo.toml` is).
    pub package_dir: PathBuf,
    pub is_lib: bool,
}

/// One test to locate.
#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub suite: usize,
    pub test: usize,
    pub name: String,
    /// The suite's name in the tree (GoogleTest suite, Boost.Test suite, …).
    pub suite_name: String,
    pub suite_kind: SuiteKind,
    pub suite_source: Option<PathBuf>,
    pub crate_name: Option<String>,
    /// Failure location as printed (often relative); resolved to an existing file or dropped.
    pub failure: Option<TestLocation>,
}

#[derive(Clone, Debug)]
pub(crate) struct Job {
    pub framework: Framework,
    /// Base directories for relative paths, most specific first (run cwd, workspace root, project root).
    pub bases: Vec<PathBuf>,
    pub targets: Vec<TargetInfo>,
    /// For `zig test file.zig` runs: the file.
    pub zig_file: Option<PathBuf>,
    pub requests: Vec<Request>,
}

/// (suite, test, name, definition, resolved failure location)
pub(crate) type Located = (usize, usize, String, Option<TestLocation>, Option<TestLocation>);

pub(crate) fn run(job: Job) -> Vec<Located> {
    let mut files = Files::default();
    let mut out = Vec::with_capacity(job.requests.len());
    // C++ tests: the project's sources (the project root is the last base), listed once.
    let cpp_sources = match job.bases.last() {
        Some(root) if job.framework.is_cpp() => super::cpp::source_files(root),
        _ => Vec::new(),
    };
    for r in &job.requests {
        let def = match job.framework {
            Framework::Cargo => locate_rust(&job, r, &mut files),
            Framework::Pytest => locate_pytest(&job, r, &mut files),
            Framework::Zig => locate_zig(&job, r, &mut files),
            Framework::CTest => None,
            Framework::GoogleTest | Framework::Catch2 | Framework::Doctest | Framework::BoostTest => {
                super::cpp::locate(job.framework, &cpp_sources, &r.suite_name, &r.name, &mut |p| {
                    files.get(p).map(str::to_string)
                })
            }
        };
        let failure = r.failure.as_ref().and_then(|f| {
            let path = resolve(&job.bases, &f.path, r, &job.targets)?;
            Some(TestLocation { path, ..f.clone() })
        });
        out.push((r.suite, r.test, r.name.clone(), def, failure));
    }
    out
}

/// A cache of source files read during one job.
#[derive(Default)]
struct Files {
    text: HashMap<PathBuf, Option<String>>,
    reads: usize,
}

impl Files {
    fn get(&mut self, path: &Path) -> Option<&str> {
        if !self.text.contains_key(path) {
            let text = if self.reads >= MAX_FILES_READ {
                None
            } else {
                self.reads += 1;
                std::fs::metadata(path)
                    .ok()
                    .filter(|m| m.is_file() && m.len() <= MAX_FILE_BYTES)
                    .and_then(|_| std::fs::read_to_string(path).ok())
            };
            self.text.insert(path.to_path_buf(), text);
        }
        self.text.get(path).and_then(|t| t.as_deref())
    }
}

/// A printed path → an existing file: absolute as is, else under the package of the suite's crate,
/// else under each base.
fn resolve(bases: &[PathBuf], path: &Path, r: &Request, targets: &[TargetInfo]) -> Option<PathBuf> {
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    let package_dirs = targets
        .iter()
        .filter(|t| r.crate_name.as_deref() == Some(t.crate_name.as_str()))
        .map(|t| t.package_dir.clone());
    package_dirs
        .chain(bases.iter().cloned())
        .chain(targets.iter().map(|t| t.package_dir.clone()))
        .map(|d| d.join(path))
        .find(|p| p.is_file())
}

/// Line index of the first line at or after `from` matching `pred`.
fn find_line(text: &str, from: usize, pred: impl Fn(&str) -> bool) -> Option<usize> {
    text.lines().enumerate().skip(from).find(|(_, l)| pred(l)).map(|(i, _)| i)
}

/// Whether `line` declares `kw name` (`fn name(`, `mod name {`, `async fn name<T>(`, …).
fn declares(line: &str, kw: &str, name: &str) -> bool {
    let mut rest = line;
    while let Some(i) = rest.find(kw) {
        let before_ok = rest.get(..i).is_none_or(|b| b.is_empty() || b.ends_with(|c: char| c.is_whitespace()));
        let after = rest.get(i + kw.len()..).unwrap_or("");
        if before_ok && after.starts_with(char::is_whitespace) {
            let ident = after.trim_start();
            if let Some(tail) = ident.strip_prefix(name)
                && !tail.starts_with(|c: char| c.is_alphanumeric() || c == '_')
            {
                return true;
            }
        }
        rest = rest.get(i + kw.len()..).unwrap_or("");
    }
    false
}

// ---------------------------------------------------------------- Rust

fn locate_rust(job: &Job, r: &Request, files: &mut Files) -> Option<TestLocation> {
    if r.suite_kind == SuiteKind::Doc {
        // `src/lib.rs - Thing::new (line 12)`
        let (file, rest) = r.name.split_once(" - ")?;
        let line: usize = rest.rsplit_once("(line ")?.1.strip_suffix(')')?.trim().parse().ok()?;
        let path = resolve(&job.bases, Path::new(file), r, &job.targets)?;
        return Some(TestLocation { path, line: line.saturating_sub(1), column: 0 });
    }
    let root = crate_root(job, r)?;
    let mut segs: Vec<&str> = r.name.split("::").collect();
    let name = segs.pop()?;
    // Walk out-of-line modules (`mod a;` → a.rs / a/mod.rs); the rest are inline in `file`.
    let mut file = root.clone();
    let mut dir = root.parent()?.to_path_buf();
    let mut inline: Vec<&str> = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        let text = files.get(&file)?;
        let has_decl = text.lines().any(|l| declares(l, "mod", seg));
        let inline_here = text.lines().any(|l| declares(l, "mod", seg) && l.contains('{'));
        let next = [dir.join(format!("{seg}.rs")), dir.join(seg).join("mod.rs")].into_iter().find(|p| p.is_file());
        match next {
            Some(next) if has_decl && !inline_here => {
                dir = if next.file_name().is_some_and(|n| n == "mod.rs") {
                    next.parent().map(Path::to_path_buf).unwrap_or_default()
                } else {
                    dir.join(seg)
                };
                file = next;
            }
            _ => {
                inline = segs.get(i..).unwrap_or(&[]).to_vec();
                break;
            }
        }
    }
    if let Some(line) = find_in_file(files, &file, &inline, "fn", name) {
        return Some(TestLocation { path: file, line, column: 0 });
    }
    // Fallback: any file of the crate that declares `fn name` (macros, `#[path]`, odd layouts).
    let crate_dir = root.parent()?.to_path_buf();
    let mut candidates = Vec::new();
    collect_rs(&crate_dir, &mut candidates, 0);
    candidates.sort();
    for path in candidates {
        if let Some(line) = find_in_file(files, &path, &[], "fn", name) {
            return Some(TestLocation { path, line, column: 0 });
        }
    }
    None
}

/// The crate root file of the test's suite.
fn crate_root(job: &Job, r: &Request) -> Option<PathBuf> {
    let source = r.suite_source.as_deref();
    let matching = |t: &&TargetInfo| r.crate_name.as_deref().is_none_or(|c| c == t.crate_name);
    let by_source = job
        .targets
        .iter()
        .filter(matching)
        .find(|t| source.is_some_and(|s| t.src_path.ends_with(s)))
        .map(|t| t.src_path.clone());
    by_source
        .or_else(|| {
            let s = source?;
            if s.is_absolute() {
                return s.is_file().then(|| s.to_path_buf());
            }
            job.bases.iter().map(|b| b.join(s)).find(|p| p.is_file())
        })
        .or_else(|| job.targets.iter().filter(matching).find(|t| t.is_lib).map(|t| t.src_path.clone()))
}

/// The line of `kw name` in `path`, after the nested inline modules `mods` when they are found.
fn find_in_file(files: &mut Files, path: &Path, mods: &[&str], kw: &str, name: &str) -> Option<usize> {
    let text = files.get(path)?;
    let mut from = 0;
    for m in mods {
        match find_line(text, from, |l| declares(l, "mod", m)) {
            Some(l) => from = l,
            None => break,
        }
    }
    find_line(text, from, |l| declares(l, kw, name)).or_else(|| find_line(text, 0, |l| declares(l, kw, name)))
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 12 || out.len() >= MAX_FILES_READ {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" || name.starts_with("target-") || name == "node_modules" {
            continue;
        }
        match e.file_type() {
            Ok(t) if t.is_dir() => collect_rs(&p, out, depth + 1),
            Ok(t) if t.is_file() && name.ends_with(".rs") => out.push(p),
            _ => {}
        }
        if out.len() >= MAX_FILES_READ {
            return;
        }
    }
}

// ---------------------------------------------------------------- pytest / Zig

fn locate_pytest(job: &Job, r: &Request, files: &mut Files) -> Option<TestLocation> {
    let source = r.suite_source.as_deref()?;
    let path = resolve(&job.bases, source, r, &[])?;
    let mut parts: Vec<&str> = r.name.split("::").collect();
    let leaf = parts.pop()?;
    let func = leaf.split('[').next().unwrap_or(leaf);
    let text = files.get(&path)?;
    let mut from = 0;
    for class in &parts {
        if let Some(l) = find_line(text, from, |l| declares(l, "class", class)) {
            from = l;
        }
    }
    let line = find_line(text, from, |l| declares(l, "def", func))?;
    Some(TestLocation { path, line, column: 0 })
}

fn locate_zig(job: &Job, r: &Request, files: &mut Files) -> Option<TestLocation> {
    let path = job.zig_file.clone()?;
    let text = files.get(&path)?;
    let quoted = format!("test \"{}\"", r.name);
    let line = find_line(text, 0, |l| l.trim_start().starts_with(&quoted))?;
    Some(TestLocation { path, line, column: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn req(name: &str, kind: SuiteKind, source: &str, krate: &str) -> Request {
        Request {
            suite: 0,
            test: 0,
            name: name.into(),
            suite_name: String::new(),
            suite_kind: kind,
            suite_source: Some(PathBuf::from(source)),
            crate_name: Some(krate.into()),
            failure: None,
        }
    }

    #[test]
    fn rust_module_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "Cargo.toml", "");
        write(
            root,
            "src/lib.rs",
            "pub mod util;\nmod a;\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() {}\n    mod nested {\n        #[test]\n        fn adds() {}\n    }\n}\n",
        );
        write(
            root,
            "src/util.rs",
            "pub fn x() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn twice_works() {}\n}\n",
        );
        write(root, "src/a/mod.rs", "mod b;\n");
        write(root, "src/a/b.rs", "\n\n#[test]\nfn deep_test() {}\n");
        write(root, "src/macro_made.rs", "fn generated_elsewhere() {}\n");
        write(root, "tests/integration.rs", "#[test]\nfn it_works() {}\n");
        let targets = vec![
            TargetInfo {
                crate_name: "demo".into(),
                src_path: root.join("src/lib.rs"),
                package_dir: root.into(),
                is_lib: true,
            },
            TargetInfo {
                crate_name: "integration".into(),
                src_path: root.join("tests/integration.rs"),
                package_dir: root.into(),
                is_lib: false,
            },
        ];
        let mut failure = req("tests::adds", SuiteKind::Unit, "src/lib.rs", "demo");
        failure.failure = Some(TestLocation { path: "src/lib.rs".into(), line: 6, column: 4 });
        let job = Job {
            framework: Framework::Cargo,
            bases: vec![root.into()],
            targets,
            zig_file: None,
            requests: vec![
                failure,
                req("tests::nested::adds", SuiteKind::Unit, "src/lib.rs", "demo"),
                req("util::tests::twice_works", SuiteKind::Unit, "src/lib.rs", "demo"),
                req("a::b::deep_test", SuiteKind::Unit, "src/lib.rs", "demo"),
                req("generated_elsewhere", SuiteKind::Unit, "src/lib.rs", "demo"),
                req("it_works", SuiteKind::Integration, "tests/integration.rs", "integration"),
                req("src/lib.rs - x (line 3)", SuiteKind::Doc, "", "demo"),
                req("missing::nope", SuiteKind::Unit, "src/lib.rs", "demo"),
            ],
        };
        let got: Vec<Option<(String, usize)>> = run(job.clone())
            .into_iter()
            .map(|(_, _, _, d, _)| d.map(|d| (d.path.strip_prefix(root).unwrap().display().to_string(), d.line)))
            .collect();
        let want = [
            Some(("src/lib.rs", 6)),
            Some(("src/lib.rs", 9)),
            Some(("src/util.rs", 4)),
            Some(("src/a/b.rs", 3)),
            Some(("src/macro_made.rs", 0)),
            Some(("tests/integration.rs", 1)),
            Some(("src/lib.rs", 2)),
            None,
        ];
        let want: Vec<Option<(String, usize)>> = want.iter().map(|w| w.map(|(p, l)| (p.to_string(), l))).collect();
        assert_eq!(got, want);
        let failure = run(job).into_iter().next().and_then(|(_, _, _, _, f)| f).unwrap();
        assert_eq!((failure.path, failure.line, failure.column), (root.join("src/lib.rs"), 6, 4));
    }

    #[test]
    fn pytest_and_zig() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "tests/test_math.py",
            "def test_add():\n    pass\n\nclass TestGroup:\n    def test_add(self):\n        pass\n",
        );
        write(root, "a.zig", "const std = @import(\"std\");\ntest \"adds\" {}\n");
        let job = Job {
            framework: Framework::Pytest,
            bases: vec![root.into()],
            targets: vec![],
            zig_file: None,
            requests: vec![
                req("test_add", SuiteKind::File, "tests/test_math.py", ""),
                req("TestGroup::test_add[1-2]", SuiteKind::File, "tests/test_math.py", ""),
            ],
        };
        let lines: Vec<Option<usize>> = run(job).into_iter().map(|(_, _, _, d, _)| d.map(|d| d.line)).collect();
        assert_eq!(lines, [Some(0), Some(4)]);
        let job = Job {
            framework: Framework::Zig,
            bases: vec![root.into()],
            targets: vec![],
            zig_file: Some(root.join("a.zig")),
            requests: vec![req("adds", SuiteKind::File, "a", "")],
        };
        assert_eq!(run(job).first().and_then(|r| r.3.as_ref()).map(|d| d.line), Some(1));
    }

    #[test]
    fn declarations() {
        assert!(declares("    fn adds() {}", "fn", "adds"));
        assert!(declares("pub async fn adds<T>()", "fn", "adds"));
        assert!(!declares("fn adds_more()", "fn", "adds"));
        assert!(!declares("fnadds()", "fn", "adds"));
        assert!(declares("mod tests {", "mod", "tests"));
        assert!(!declares("// see mod testsuite", "mod", "tests"));
        assert!(!declares("", "fn", ""));
    }
}
