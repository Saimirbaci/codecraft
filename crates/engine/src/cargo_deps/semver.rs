//! Cargo's flavour of semantic versioning: versions, version requirements (`^1.2`, `~1.2.3`,
//! `=1.0.0`, `>=1, <2`, `1.*`, `*`, bare `1.2` = caret) and what "a newer version" means for a
//! requirement. Pure and total: malformed input parses to `None`, never panics.

use std::cmp::Ordering;
use std::fmt;

/// A pre-release identifier (`alpha`, `1`): numeric identifiers sort below alphanumeric ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreIdent {
    Num(u64),
    Alpha(String),
}

impl Ord for PreIdent {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Num(a), Self::Num(b)) => a.cmp(b),
            (Self::Num(_), Self::Alpha(_)) => Ordering::Less,
            (Self::Alpha(_), Self::Num(_)) => Ordering::Greater,
            (Self::Alpha(a), Self::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for PreIdent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `major.minor.patch[-pre][+build]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<PreIdent>,
    /// Build metadata (ignored for ordering).
    pub build: String,
}

impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self { major, minor, patch, pre: Vec::new(), build: String::new() }
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// Parse a full version (`1.2.3`, `1.0.0-beta.2+build`).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() || s.len() > 256 {
            return None;
        }
        let (core, build) = match s.split_once('+') {
            Some((c, b)) => (c, b.to_string()),
            None => (s, String::new()),
        };
        let (nums, pre) = match core.split_once('-') {
            Some((n, p)) => (n, parse_pre(p)?),
            None => (core, Vec::new()),
        };
        let mut it = nums.split('.');
        let major = parse_num(it.next()?)?;
        let minor = parse_num(it.next()?)?;
        let patch = parse_num(it.next()?)?;
        if it.next().is_some() {
            return None;
        }
        Some(Self { major, minor, patch, pre, build })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch)).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A release sorts above its pre-releases.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            let parts: Vec<String> = self
                .pre
                .iter()
                .map(|p| match p {
                    PreIdent::Num(n) => n.to_string(),
                    PreIdent::Alpha(a) => a.clone(),
                })
                .collect();
            write!(f, "-{}", parts.join("."))?;
        }
        if !self.build.is_empty() {
            write!(f, "+{}", self.build)?;
        }
        Ok(())
    }
}

fn parse_num(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || s.len() > 19 {
        return None;
    }
    s.parse().ok()
}

fn parse_pre(s: &str) -> Option<Vec<PreIdent>> {
    if s.is_empty() {
        return None;
    }
    s.split('.')
        .map(|p| {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                None
            } else if p.bytes().all(|b| b.is_ascii_digit()) {
                p.parse().ok().map(PreIdent::Num).or_else(|| Some(PreIdent::Alpha(p.to_string())))
            } else {
                Some(PreIdent::Alpha(p.to_string()))
            }
        })
        .collect()
}

/// A comparison operator of a requirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Exact,
    Greater,
    GreaterEq,
    Less,
    LessEq,
    Tilde,
    Caret,
    Wildcard,
}

impl Op {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Exact => "=",
            Self::Greater => ">",
            Self::GreaterEq => ">=",
            Self::Less => "<",
            Self::LessEq => "<=",
            Self::Tilde => "~",
            Self::Caret | Self::Wildcard => "",
        }
    }
}

/// One comparator (`^1.2`, `<2`, `1.*`): minor and patch may be left out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparator {
    pub op: Op,
    pub major: u64,
    pub minor: Option<u64>,
    pub patch: Option<u64>,
    pub pre: Vec<PreIdent>,
}

/// A version requirement: every comparator must match (`*` has none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionReq {
    pub comparators: Vec<Comparator>,
}

impl VersionReq {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.len() > 512 {
            return None;
        }
        if s == "*" || s.is_empty() {
            return Some(Self { comparators: Vec::new() });
        }
        let comparators: Option<Vec<Comparator>> = s.split(',').map(parse_comparator).collect();
        let comparators = comparators?;
        if comparators.is_empty() {
            return None;
        }
        Some(Self { comparators })
    }

    /// Whether `v` satisfies the requirement (pre-releases only match a comparator naming the same
    /// `major.minor.patch` with a pre-release, like Cargo).
    pub fn matches(&self, v: &Version) -> bool {
        self.comparators.iter().all(|c| c.matches(v))
            && (v.pre.is_empty() || self.comparators.iter().any(|c| c.allows_pre_of(v)))
    }

    /// An upper bound (`<`, `<=`) or several comparators: the author pinned it on purpose.
    pub fn is_pinned_range(&self) -> bool {
        self.comparators.len() > 1 || self.comparators.iter().any(|c| matches!(c.op, Op::Less | Op::LessEq))
    }
}

fn parse_comparator(s: &str) -> Option<Comparator> {
    let s = s.trim();
    let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (Op::GreaterEq, r)
    } else if let Some(r) = s.strip_prefix("<=") {
        (Op::LessEq, r)
    } else if let Some(r) = s.strip_prefix('>') {
        (Op::Greater, r)
    } else if let Some(r) = s.strip_prefix('<') {
        (Op::Less, r)
    } else if let Some(r) = s.strip_prefix('=') {
        (Op::Exact, r)
    } else if let Some(r) = s.strip_prefix('~') {
        (Op::Tilde, r)
    } else if let Some(r) = s.strip_prefix('^') {
        (Op::Caret, r)
    } else {
        (Op::Caret, s)
    };
    let rest = rest.trim();
    if rest == "*" {
        return None;
    }
    let (nums, pre) = match rest.split_once('-') {
        Some((n, p)) => (n, parse_pre(p.split('+').next().unwrap_or(p))?),
        None => (rest.split('+').next().unwrap_or(rest), Vec::new()),
    };
    let mut parts = nums.split('.');
    let major = parse_num(parts.next()?)?;
    let wild = |p: &str| matches!(p.trim(), "*" | "x" | "X");
    let mut op = op;
    let minor = match parts.next() {
        None => None,
        Some(p) if wild(p) => {
            op = if op == Op::Caret { Op::Wildcard } else { op };
            None
        }
        Some(p) => Some(parse_num(p)?),
    };
    let patch = match parts.next() {
        None => None,
        Some(p) if wild(p) => {
            op = if op == Op::Caret { Op::Wildcard } else { op };
            None
        }
        Some(p) => {
            minor?;
            Some(parse_num(p)?)
        }
    };
    if parts.next().is_some() || (!pre.is_empty() && patch.is_none()) {
        return None;
    }
    Some(Comparator { op, major, minor, patch, pre })
}

impl Comparator {
    fn allows_pre_of(&self, v: &Version) -> bool {
        !self.pre.is_empty() && self.major == v.major && self.minor == Some(v.minor) && self.patch == Some(v.patch)
    }

    fn lower(&self) -> Version {
        Version {
            major: self.major,
            minor: self.minor.unwrap_or(0),
            patch: self.patch.unwrap_or(0),
            pre: self.pre.clone(),
            build: String::new(),
        }
    }

    pub fn matches(&self, v: &Version) -> bool {
        let lower = self.lower();
        let core = (v.major, v.minor, v.patch);
        match self.op {
            Op::Exact | Op::Wildcard => {
                v.major == self.major
                    && self.minor.is_none_or(|m| v.minor == m)
                    && self.patch.is_none_or(|p| v.patch == p)
                    && (self.patch.is_none() || v.pre == self.pre)
            }
            Op::Greater => match (self.minor, self.patch) {
                (None, _) => v.major > self.major,
                (Some(m), None) => (v.major, v.minor) > (self.major, m),
                (Some(_), Some(_)) => *v > lower,
            },
            Op::GreaterEq => match (self.minor, self.patch) {
                (None, _) => v.major >= self.major,
                (Some(m), None) => (v.major, v.minor) >= (self.major, m),
                (Some(_), Some(_)) => *v >= lower,
            },
            Op::Less => match (self.minor, self.patch) {
                (None, _) => v.major < self.major,
                (Some(m), None) => (v.major, v.minor) < (self.major, m),
                (Some(_), Some(_)) => *v < lower,
            },
            Op::LessEq => match (self.minor, self.patch) {
                (None, _) => v.major <= self.major,
                (Some(m), None) => (v.major, v.minor) <= (self.major, m),
                (Some(_), Some(_)) => *v <= lower,
            },
            Op::Tilde => {
                if *v < lower {
                    return false;
                }
                match self.minor {
                    None => v.major == self.major,
                    Some(m) => v.major == self.major && v.minor == m,
                }
            }
            Op::Caret => {
                if *v < lower {
                    return false;
                }
                match (self.major, self.minor, self.patch) {
                    (0, None, _) => v.major == 0,
                    (0, Some(0), None) => core.0 == 0 && core.1 == 0,
                    (0, Some(0), Some(p)) => core == (0, 0, p),
                    (0, Some(m), _) => v.major == 0 && v.minor == m,
                    (maj, _, _) => v.major == maj,
                }
            }
        }
    }
}

/// The newest stable version (pre-releases only when there is no stable one).
pub fn latest<'a>(versions: impl IntoIterator<Item = &'a Version>) -> Option<&'a Version> {
    let all: Vec<&Version> = versions.into_iter().collect();
    all.iter().copied().filter(|v| !v.is_prerelease()).max().or_else(|| all.iter().copied().max())
}

/// The version to suggest for a requirement: the newest stable version when the requirement does
/// not allow it and it is newer than everything the requirement allows. `None` when the
/// requirement is up to date, pinned with an upper bound, or unparseable.
pub fn newer_version<'a>(req: &str, versions: &'a [Version]) -> Option<&'a Version> {
    let req = VersionReq::parse(req)?;
    if req.is_pinned_range() {
        return None;
    }
    let newest = versions.iter().filter(|v| !v.is_prerelease()).max()?;
    // A requirement above everything published is not out of date (it matches nothing).
    if req.matches(newest) || req.comparators.first().is_some_and(|c| *newest < c.lower()) {
        return None;
    }
    match versions.iter().filter(|v| req.matches(v)).max() {
        Some(best) if best >= newest => None,
        _ => Some(newest),
    }
}

/// Rewrite a requirement for `new`, keeping its operator, spacing and precision (`"0.7"` →
/// `"0.8"`, `"~1.2.3"` → `"~2.0.1"`, `"1.*"` → `"2.*"`); pre-releases are written in full.
pub fn updated_requirement(req: &str, new: &Version) -> String {
    let lead_len = req.len() - req.trim_start().len();
    let lead = req.get(..lead_len).unwrap_or("");
    let body = req.trim();
    let op_len = body.find(|c: char| c.is_ascii_digit()).unwrap_or(0);
    let op = body.get(..op_len).unwrap_or("");
    let rest = body.get(op_len..).unwrap_or("");
    let core = rest.split(['-', '+']).next().unwrap_or(rest);
    let parts: Vec<&str> = core.split('.').collect();
    let wildcard = parts.iter().any(|p| matches!(p.trim(), "*" | "x" | "X"));
    let precision = parts.iter().take_while(|p| p.trim().bytes().all(|b| b.is_ascii_digit()) && !p.is_empty()).count();
    let full = new.to_string();
    let version = if new.is_prerelease() || rest.contains('-') || precision >= 3 || precision == 0 {
        full
    } else if precision == 2 {
        if wildcard { format!("{}.{}.*", new.major, new.minor) } else { format!("{}.{}", new.major, new.minor) }
    } else if wildcard {
        format!("{}.*", new.major)
    } else {
        new.major.to_string()
    };
    format!("{lead}{op}{version}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn m(req: &str, ver: &str) -> bool {
        VersionReq::parse(req).unwrap().matches(&v(ver))
    }

    #[test]
    fn versions_parse_order_and_print() {
        assert_eq!(v("1.2.3"), Version::new(1, 2, 3));
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") < v("1.0.0-alpha.beta"));
        assert!(v("1.0.0-beta.2") < v("1.0.0-beta.11"));
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert!(v("1.10.0") > v("1.9.9"));
        assert_eq!(v("1.0.0-beta.2+abc").to_string(), "1.0.0-beta.2+abc");
        for bad in ["", "1", "1.2", "1.2.3.4", "a.b.c", "1.2.3-", "-1.2.3", "1.2.3-a..b", "99999999999999999999.0.0"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn caret_tilde_exact_wildcard() {
        // Bare = caret.
        assert!(m("1.2", "1.9.0") && !m("1.2", "2.0.0") && !m("1.2", "1.1.9"));
        assert!(m("^1.2.3", "1.2.3") && m("^1.2.3", "1.99.0") && !m("^1.2.3", "1.2.2"));
        assert!(m("0.7", "0.7.9") && !m("0.7", "0.8.0"));
        assert!(m("^0.0.3", "0.0.3") && !m("^0.0.3", "0.0.4"));
        assert!(m("^0.0", "0.0.9") && !m("^0.0", "0.1.0"));
        assert!(m("0", "0.99.0") && !m("0", "1.0.0"));
        assert!(m("~1.2.3", "1.2.9") && !m("~1.2.3", "1.3.0"));
        assert!(m("~1", "1.9.0") && !m("~1", "2.0.0"));
        assert!(m("=1.0.0", "1.0.0") && !m("=1.0.0", "1.0.1"));
        assert!(m("1.*", "1.5.0") && !m("1.*", "2.0.0"));
        assert!(m("1.2.*", "1.2.7") && !m("1.2.*", "1.3.0"));
        assert!(m("*", "7.0.0") && m("", "0.1.0"));
        assert!(m(">=1.2, <2", "1.5.0") && !m(">=1.2, <2", "2.0.0") && !m(">=1.2, <2", "1.1.0"));
        assert!(m("<=1.2", "1.2.9") && !m("<=1.2", "1.3.0"));
        assert!(m(">1", "2.0.0") && !m(">1", "1.9.0"));
    }

    #[test]
    fn prereleases_need_an_explicit_prerelease_comparator() {
        assert!(!m("1", "1.5.0-beta.1"));
        assert!(m("^1.0.0-beta.1", "1.0.0-beta.2"));
        assert!(m("^1.0.0-beta.1", "1.0.0"));
        assert!(!m("^1.0.0-beta.1", "1.0.1-beta.1"));
        assert!(m("=1.0.0-rc.1", "1.0.0-rc.1"));
    }

    #[test]
    fn bad_requirements_are_none() {
        for bad in ["abc", "1.2.3.4", "^", ">= x", "1.2-beta", ",", "1,,2", "~*"] {
            assert_eq!(VersionReq::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn newer_versions() {
        let vs: Vec<Version> =
            ["0.7.3", "0.8.0", "0.8.5", "0.9.0-alpha.1", "1.0.0-rc.1"].iter().map(|s| v(s)).collect();
        assert_eq!(newer_version("0.7", &vs).map(ToString::to_string).as_deref(), Some("0.8.5"));
        assert_eq!(newer_version("0.8", &vs), None);
        assert_eq!(newer_version("^0.8.1", &vs), None);
        assert_eq!(newer_version("=0.8.0", &vs).map(ToString::to_string).as_deref(), Some("0.8.5"));
        assert_eq!(newer_version("*", &vs), None);
        assert_eq!(newer_version(">=0.7, <0.8", &vs), None, "upper bounds are deliberate");
        // Already on a pre-release newer than every stable version.
        assert_eq!(newer_version("1.0.0-rc.1", &vs), None);
        assert_eq!(newer_version("not a req", &vs), None);
        assert_eq!(newer_version("1", &[]), None);
        assert_eq!(newer_version("9", &vs), None, "ahead of every release");
        assert_eq!(latest(&vs).map(ToString::to_string).as_deref(), Some("0.8.5"));
        let pre_only = vec![v("0.1.0-alpha")];
        assert_eq!(latest(&pre_only).map(ToString::to_string).as_deref(), Some("0.1.0-alpha"));
    }

    #[test]
    fn requirements_keep_their_shape() {
        let new = v("2.3.4");
        assert_eq!(updated_requirement("1", &new), "2");
        assert_eq!(updated_requirement("0.7", &new), "2.3");
        assert_eq!(updated_requirement("1.2.3", &new), "2.3.4");
        assert_eq!(updated_requirement("^1.2.3", &new), "^2.3.4");
        assert_eq!(updated_requirement("~1.2", &new), "~2.3");
        assert_eq!(updated_requirement("=1.0.0", &new), "=2.3.4");
        assert_eq!(updated_requirement("1.*", &new), "2.*");
        assert_eq!(updated_requirement("1.2.*", &new), "2.3.*");
        assert_eq!(updated_requirement(" 1.0", &new), " 2.3");
        assert_eq!(updated_requirement("1.0.0-beta.1", &new), "2.3.4");
        assert_eq!(updated_requirement("", &new), "2.3.4");
        assert_eq!(updated_requirement("1", &v("2.0.0-rc.1")), "2.0.0-rc.1");
    }
}
