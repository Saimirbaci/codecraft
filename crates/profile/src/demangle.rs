//! Rust symbol names for people: the v0 mangling (`_RNvCs…_4shop3fib` → `shop::fib`), which
//! Instruments and other native tools leave as is, and the legacy mangling (`_ZN4shop3fib17h…E`
//! → `shop::fib`), plus the `::h<hash>` suffix some tools keep after demangling. Written from the
//! public v0 mangling specification (RFC 2603); the output follows rustc's own alternate
//! (hash-free) format closely enough to read. Input is hostile: every read is bounds-checked,
//! recursion and output are capped, and anything that does not parse is left alone.

/// Deepest nesting followed (paths, types, backreferences).
const MAX_DEPTH: u32 = 64;
/// Longest demangled name.
const MAX_OUTPUT: usize = 4096;

/// The demangled form of a Rust symbol, or `None` when `sym` is not one (or does not parse).
pub fn demangle(sym: &str) -> Option<String> {
    let s = sym.trim();
    if let Some(rest) = s.strip_prefix("__R").or_else(|| s.strip_prefix("_R")) {
        return v0(rest);
    }
    if let Some(rest) = s.strip_prefix("__ZN").or_else(|| s.strip_prefix("_ZN")) {
        return legacy(rest);
    }
    strip_hash(s).map(str::to_string)
}

/// `shop::fib::h0123456789abcdef` → `shop::fib` (a legacy name some tools demangle with its hash).
fn strip_hash(s: &str) -> Option<&str> {
    let (head, hash) = s.rsplit_once("::h")?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && !head.is_empty()).then_some(head)
}

// ---------------------------------------------------------------- legacy

fn legacy(rest: &str) -> Option<String> {
    let b = rest.as_bytes();
    let mut i = 0usize;
    let mut parts: Vec<String> = Vec::new();
    loop {
        match b.get(i) {
            Some(b'E') => break,
            Some(c) if c.is_ascii_digit() => {}
            _ => return None,
        }
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let len: usize = rest.get(start..i)?.parse().ok()?;
        let end = i.checked_add(len)?;
        let part = rest.get(i..end)?;
        i = end;
        parts.push(unescape_legacy(part));
        if parts.len() > 256 {
            return None;
        }
    }
    if parts
        .last()
        .is_some_and(|p| p.len() == 17 && p.starts_with('h') && p[1..].bytes().all(|c| c.is_ascii_hexdigit()))
    {
        parts.pop();
    }
    if parts.is_empty() {
        return None;
    }
    let out = parts.join("::");
    (out.len() <= MAX_OUTPUT).then_some(out)
}

fn unescape_legacy(part: &str) -> String {
    let part = part.strip_prefix("_$").map_or(part.to_string(), |p| format!("${p}"));
    let mut out = String::with_capacity(part.len());
    let mut rest = part.as_str();
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("..") {
            out.push_str("::");
            rest = r;
            continue;
        }
        if rest.starts_with('$')
            && let Some(close) = rest.get(1..).and_then(|r| r.find('$'))
        {
            let code = rest.get(1..=close).unwrap_or("");
            let rep = match code {
                "SP" => Some("@".to_string()),
                "BP" => Some("*".to_string()),
                "RF" => Some("&".to_string()),
                "LT" => Some("<".to_string()),
                "GT" => Some(">".to_string()),
                "LP" => Some("(".to_string()),
                "RP" => Some(")".to_string()),
                "C" => Some(",".to_string()),
                _ => code
                    .strip_prefix('u')
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .and_then(char::from_u32)
                    .map(String::from),
            };
            if let Some(rep) = rep {
                out.push_str(&rep);
                rest = rest.get(close + 2..).unwrap_or("");
                continue;
            }
        }
        let mut chars = rest.chars();
        if let Some(c) = chars.next() {
            out.push(c);
        }
        rest = chars.as_str();
    }
    out
}

// ---------------------------------------------------------------- v0

fn v0(rest: &str) -> Option<String> {
    let mut p = Parser { s: rest.as_bytes(), pos: 0, depth: 0, out: String::new() };
    // An optional encoding version (decimal) before the path.
    if p.peek().is_some_and(|c| c.is_ascii_digit()) {
        p.decimal()?;
    }
    p.path(true)?;
    // An optional instantiating crate, then a vendor suffix (`.llvm.1234`).
    if p.peek().is_some_and(|c| c.is_ascii_uppercase()) {
        let mut scratch = Parser { s: p.s, pos: p.pos, depth: 0, out: String::new() };
        if scratch.path(false).is_some() {
            p.pos = scratch.pos;
        }
    }
    match p.peek() {
        None | Some(b'.') | Some(b'$') => {}
        Some(_) => return None,
    }
    (p.out.len() <= MAX_OUTPUT && !p.out.is_empty()).then_some(p.out)
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
    depth: u32,
    out: String,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn push(&mut self, s: &str) -> Option<()> {
        if self.out.len().saturating_add(s.len()) > MAX_OUTPUT {
            return None;
        }
        self.out.push_str(s);
        Some(())
    }

    fn enter(&mut self) -> Option<()> {
        self.depth += 1;
        (self.depth <= MAX_DEPTH).then_some(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn decimal(&mut self) -> Option<u64> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        std::str::from_utf8(self.s.get(start..self.pos)?).ok()?.parse().ok()
    }

    /// `<base-62-number>`: `_` is 0, otherwise digits then `_`, plus one.
    fn base62(&mut self) -> Option<u64> {
        if self.eat(b'_') {
            return Some(0);
        }
        let mut v: u64 = 0;
        loop {
            let c = self.next()?;
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'z' => c - b'a' + 10,
                b'A'..=b'Z' => c - b'A' + 36,
                b'_' => return v.checked_add(1),
                _ => return None,
            };
            v = v.checked_mul(62)?.checked_add(u64::from(d))?;
        }
    }

    /// `s<base-62>` → value + 1, or 0 without one.
    fn disambiguator(&mut self) -> Option<u64> {
        if self.eat(b's') { self.base62()?.checked_add(1) } else { Some(0) }
    }

    fn ident(&mut self) -> Option<String> {
        let _ = self.disambiguator()?;
        self.undisambiguated_ident()
    }

    fn undisambiguated_ident(&mut self) -> Option<String> {
        let puny = self.eat(b'u');
        let len = usize::try_from(self.decimal()?).ok()?;
        self.eat(b'_');
        let end = self.pos.checked_add(len)?;
        let bytes = self.s.get(self.pos..end)?;
        self.pos = end;
        let text = std::str::from_utf8(bytes).ok()?.to_string();
        // Punycode (non-ASCII identifiers) is shown encoded.
        Some(if puny { text.replace('_', "-") } else { text })
    }

    /// Re-parse what a backreference points at, printing it here.
    fn backref(&mut self, f: impl FnOnce(&mut Self) -> Option<()>) -> Option<()> {
        let start = self.pos.checked_sub(1)?;
        let target = usize::try_from(self.base62()?).ok()?;
        if target >= start {
            return None;
        }
        self.enter()?;
        let resume = self.pos;
        self.pos = target;
        let r = f(self);
        self.pos = resume;
        self.leave();
        r
    }

    fn path(&mut self, value: bool) -> Option<()> {
        self.enter()?;
        let r = self.path_inner(value);
        self.leave();
        r
    }

    fn path_inner(&mut self, value: bool) -> Option<()> {
        match self.next()? {
            b'C' => {
                let name = self.ident()?;
                self.push(&name)
            }
            b'M' => {
                self.impl_path()?;
                self.push("<")?;
                self.ty()?;
                self.push(">")
            }
            b'X' => {
                self.impl_path()?;
                self.push("<")?;
                self.ty()?;
                self.push(" as ")?;
                self.path(false)?;
                self.push(">")
            }
            b'Y' => {
                self.push("<")?;
                self.ty()?;
                self.push(" as ")?;
                self.path(false)?;
                self.push(">")
            }
            b'N' => {
                let ns = self.next()?;
                self.path(value)?;
                let dis = self.disambiguator()?;
                let name = self.undisambiguated_ident()?;
                if ns.is_ascii_uppercase() {
                    let kind = match ns {
                        b'C' => "closure".to_string(),
                        b'S' => "shim".to_string(),
                        other => char::from(other).to_string(),
                    };
                    let named = if name.is_empty() { String::new() } else { format!(":{name}") };
                    self.push(&format!("::{{{kind}{named}#{dis}}}"))
                } else {
                    self.push("::")?;
                    self.push(&name)
                }
            }
            b'I' => {
                self.path(value)?;
                self.push(if value { "::<" } else { "<" })?;
                let mut first = true;
                while !self.eat(b'E') {
                    if !first {
                        self.push(", ")?;
                    }
                    first = false;
                    self.generic_arg()?;
                }
                self.push(">")
            }
            b'B' => self.backref(|p| p.path(value)),
            _ => None,
        }
    }

    /// The impl's own path is not printed (rustc prints only the self type).
    fn impl_path(&mut self) -> Option<()> {
        let _ = self.disambiguator()?;
        let saved = std::mem::take(&mut self.out);
        let r = self.path(false);
        self.out = saved;
        r
    }

    fn generic_arg(&mut self) -> Option<()> {
        if self.eat(b'L') {
            let _ = self.base62()?;
            return self.push("'_");
        }
        if self.eat(b'K') {
            return self.konst();
        }
        self.ty()
    }

    fn basic(c: u8) -> Option<&'static str> {
        Some(match c {
            b'a' => "i8",
            b'b' => "bool",
            b'c' => "char",
            b'd' => "f64",
            b'e' => "str",
            b'f' => "f32",
            b'h' => "u8",
            b'i' => "isize",
            b'j' => "usize",
            b'l' => "i32",
            b'm' => "u32",
            b'n' => "i128",
            b'o' => "u128",
            b's' => "i16",
            b't' => "u16",
            b'u' => "()",
            b'v' => "...",
            b'x' => "i64",
            b'y' => "u64",
            b'z' => "!",
            b'p' => "_",
            _ => return None,
        })
    }

    fn ty(&mut self) -> Option<()> {
        self.enter()?;
        let r = self.ty_inner();
        self.leave();
        r
    }

    fn ty_inner(&mut self) -> Option<()> {
        let c = self.peek()?;
        if let Some(b) = Self::basic(c) {
            self.pos += 1;
            return self.push(b);
        }
        match c {
            b'A' | b'S' => {
                self.pos += 1;
                self.push("[")?;
                self.ty()?;
                if c == b'A' {
                    self.push("; ")?;
                    self.konst()?;
                }
                self.push("]")
            }
            b'T' => {
                self.pos += 1;
                self.push("(")?;
                let mut n = 0;
                while !self.eat(b'E') {
                    if n > 0 {
                        self.push(", ")?;
                    }
                    self.ty()?;
                    n += 1;
                }
                if n == 1 {
                    self.push(",")?;
                }
                self.push(")")
            }
            b'R' | b'Q' => {
                self.pos += 1;
                if self.eat(b'L') {
                    let _ = self.base62()?;
                }
                self.push(if c == b'R' { "&" } else { "&mut " })?;
                self.ty()
            }
            b'P' | b'O' => {
                self.pos += 1;
                self.push(if c == b'P' { "*const " } else { "*mut " })?;
                self.ty()
            }
            b'F' => {
                self.pos += 1;
                self.fn_sig()
            }
            b'D' => {
                self.pos += 1;
                self.dyn_bounds()?;
                if self.eat(b'L') {
                    let _ = self.base62()?;
                }
                Some(())
            }
            b'B' => {
                self.pos += 1;
                self.backref(Self::ty)
            }
            _ => self.path(false),
        }
    }

    fn binder(&mut self) -> Option<()> {
        if self.eat(b'G') {
            let _ = self.base62()?;
        }
        Some(())
    }

    fn fn_sig(&mut self) -> Option<()> {
        self.binder()?;
        if self.eat(b'U') {
            self.push("unsafe ")?;
        }
        if self.eat(b'K') {
            if self.eat(b'C') {
                self.push("extern \"C\" ")?;
            } else {
                let abi = self.undisambiguated_ident()?;
                self.push(&format!("extern \"{}\" ", abi.replace('_', "-")))?;
            }
        }
        self.push("fn(")?;
        let mut first = true;
        while !self.eat(b'E') {
            if !first {
                self.push(", ")?;
            }
            first = false;
            self.ty()?;
        }
        self.push(")")?;
        if self.peek() == Some(b'u') {
            self.pos += 1;
            return Some(());
        }
        self.push(" -> ")?;
        self.ty()
    }

    fn dyn_bounds(&mut self) -> Option<()> {
        self.binder()?;
        self.push("dyn ")?;
        let mut first = true;
        while !self.eat(b'E') {
            if !first {
                self.push(" + ")?;
            }
            first = false;
            self.path(false)?;
            while self.eat(b'p') {
                let name = self.undisambiguated_ident()?;
                self.push(&format!("<{name} = "))?;
                self.ty()?;
                self.push(">")?;
            }
        }
        Some(())
    }

    fn konst(&mut self) -> Option<()> {
        if self.eat(b'p') {
            return self.push("_");
        }
        if self.eat(b'B') {
            return self.backref(Self::konst);
        }
        let ty = self.next()?;
        let neg = self.eat(b'n');
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
            self.pos += 1;
        }
        let hex = std::str::from_utf8(self.s.get(start..self.pos)?).ok()?;
        if !self.eat(b'_') {
            return None;
        }
        let v = if hex.is_empty() { Some(0) } else { u128::from_str_radix(hex, 16).ok() };
        let text = match (ty, v) {
            (b'b', Some(0)) => "false".to_string(),
            (b'b', Some(_)) => "true".to_string(),
            (b'c', Some(v)) => {
                u32::try_from(v).ok().and_then(char::from_u32).map_or(format!("0x{hex}"), |c| format!("{c:?}"))
            }
            (_, Some(v)) => format!("{}{v}", if neg { "-" } else { "" }),
            (_, None) => format!("0x{hex}"),
        };
        self.push(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::demangle;

    #[test]
    fn v0_symbols() {
        // Taken from an Instruments trace of a debug build (QA M14).
        assert_eq!(demangle("_RNvCsczPlFL8jVDe_4shop3fib").as_deref(), Some("shop::fib"));
        assert_eq!(
            demangle("_RINvNtCs1SSuMAdGM58_3std2rt10lang_startuECsgAaLRprSyg3_4shop").as_deref(),
            Some("std::rt::lang_start::<()>")
        );
        assert_eq!(
            demangle("_RNCINvNtCs1SSuMAdGM58_3std2rt10lang_startuE0CsgAaLRprSyg3_4shop").as_deref(),
            Some("std::rt::lang_start::<()>::{closure#0}")
        );
        assert_eq!(
            demangle(
                "_RINvNtNtCs1SSuMAdGM58_3std3sys9backtrace28___rust_begin_short_backtraceFEuuECsgAaLRprSyg3_4shop"
            )
            .as_deref(),
            Some("std::sys::backtrace::__rust_begin_short_backtrace::<fn(), ()>")
        );
        // Mach-O's extra underscore, an LLVM suffix.
        assert_eq!(demangle("__RNvCsczPlFL8jVDe_4shop3fib.llvm.12345").as_deref(), Some("shop::fib"));
        // <T as Trait>::method with a backreference to the crate.
        assert_eq!(
            demangle("_RNvXs_NtCs4fqI2P2rA04_4core3fmtNtB4_9ArgumentsNtB4_7Display3fmt").as_deref(),
            Some("<core::fmt::Arguments as core::fmt::Display>::fmt")
        );
        // An inherent impl, slices, references and tuples.
        assert_eq!(demangle("_RNvMCs1234_3appNtB2_5Point3new").as_deref(), Some("<app::Point>::new"));
        assert_eq!(demangle("_RINvCs1234_3app4takeRShTmbEE").as_deref(), Some("app::take::<&[u8], (u32, bool)>"));
    }

    #[test]
    fn legacy_symbols() {
        assert_eq!(demangle("_ZN4shop3fib17h0123456789abcdefE").as_deref(), Some("shop::fib"));
        assert_eq!(
            demangle("__ZN49_$LT$app..Point$u20$as$u20$core..fmt..Display$GT$3fmt17h0123456789abcdefE").as_deref(),
            Some("<app::Point as core::fmt::Display>::fmt")
        );
        assert_eq!(demangle("shop::fib::h0123456789abcdef").as_deref(), Some("shop::fib"));
    }

    #[test]
    fn other_names_and_garbage_are_left_alone() {
        for s in [
            "main",
            "fib",
            "Run",
            "_RN",
            "_R",
            "_RNvC",
            "_RNvCs_99999999999999999999999shop3fib",
            "_RB_",
            "_RBzzzzzzzzzzz_",
            "_ZN",
            "_ZN99999999999999999999E",
            "_ZN4shopE",
            "std::vec::Vec<T>::push",
            "objc_msgSend",
            "-[NSObject init]",
        ] {
            let d = demangle(s);
            if s == "_ZN4shopE" {
                assert_eq!(d.as_deref(), Some("shop"));
            } else {
                assert!(d.is_none(), "{s} → {d:?}");
            }
        }
        // Deep nesting is cut, not a stack overflow.
        let deep = format!("_R{}Cs_1aE", "INvC1a1b".repeat(5000));
        assert!(demangle(&deep).is_none());
        let backrefs = format!("_R{}", "B_".repeat(100));
        assert!(demangle(&backrefs).is_none());
    }
}
