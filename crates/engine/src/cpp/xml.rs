//! A small, tolerant XML reader for tool reports (Valgrind `--xml=yes`, Catch2 `-r xml`, doctest
//! `-r=xml`, Boost.Test `--log_format=XML`): elements with attributes, text and children.
//!
//! Malformed or truncated input (a crashed test binary leaves its report half-written) yields the
//! elements that could be read: unclosed elements are closed at the end, stray end tags are
//! ignored. Input-sized work is bounded ([`MAX_NODES`], [`MAX_DEPTH`], [`MAX_TEXT`]).

/// Elements read from one document; the rest is dropped.
pub const MAX_NODES: usize = 500_000;
/// Deeper elements are flattened into their ancestor at this depth.
pub const MAX_DEPTH: usize = 200;
/// Text kept per element (bytes).
pub const MAX_TEXT: usize = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    /// The element's own text (CDATA included), entities decoded, children's text excluded.
    pub text: String,
}

impl Element {
    fn new(name: &str) -> Self {
        Self { name: name.to_string(), ..Self::default() }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The trimmed text of the first child called `name`.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }

    /// The first element called `name` in this subtree (depth first, this element included).
    pub fn find(&self, name: &str) -> Option<&Element> {
        if self.name == name {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(name))
    }

    /// An attribute as a number.
    pub fn attr_u64(&self, name: &str) -> Option<u64> {
        self.attr(name).and_then(|v| v.trim().parse().ok())
    }
}

/// Parse a document. The result is a synthetic `#document` element whose children are the
/// top-level elements.
pub fn parse(xml: &str) -> Element {
    let mut stack: Vec<Element> = vec![Element::new("#document")];
    let mut nodes = 0usize;
    // Elements opened beyond MAX_DEPTH (their end tags are skipped).
    let mut flattened = 0usize;
    let mut rest = xml;
    loop {
        let Some(lt) = rest.find('<') else {
            push_text(&mut stack, &decode(rest));
            break;
        };
        push_text(&mut stack, &decode(rest.get(..lt).unwrap_or("")));
        rest = rest.get(lt..).unwrap_or("");
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let (data, after) = r.split_once("]]>").unwrap_or((r, ""));
            push_text(&mut stack, data);
            rest = after;
            continue;
        }
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.split_once("-->").map_or("", |(_, a)| a);
            continue;
        }
        if let Some(r) = rest.strip_prefix("<?") {
            rest = r.split_once("?>").map_or("", |(_, a)| a);
            continue;
        }
        if rest.starts_with("<!") {
            rest = rest.split_once('>').map_or("", |(_, a)| a);
            continue;
        }
        let Some(end) = tag_end(rest) else { break };
        let tag = rest.get(1..end).unwrap_or("");
        rest = rest.get(end + 1..).unwrap_or("");
        if let Some(name) = tag.strip_prefix('/') {
            let name = name.trim();
            if flattened > 0 {
                flattened -= 1;
                continue;
            }
            // Close up to the matching open element; a stray end tag is ignored.
            if let Some(pos) = stack.iter().rposition(|e| e.name == name)
                && pos > 0
            {
                while stack.len() > pos {
                    close(&mut stack);
                }
            }
            continue;
        }
        let self_closing = tag.trim_end().ends_with('/');
        let tag = if self_closing { tag.trim_end().trim_end_matches('/') } else { tag };
        let (name, attrs) = parse_tag(tag);
        if name.is_empty() {
            continue;
        }
        if nodes >= MAX_NODES {
            break;
        }
        nodes += 1;
        if stack.len() > MAX_DEPTH {
            if !self_closing {
                flattened += 1;
            }
            continue;
        }
        let mut element = Element::new(name);
        element.attrs = attrs;
        if self_closing {
            if let Some(parent) = stack.last_mut() {
                parent.children.push(element);
            }
        } else {
            stack.push(element);
        }
    }
    while stack.len() > 1 {
        close(&mut stack);
    }
    stack.pop().unwrap_or_else(|| Element::new("#document"))
}

fn close(stack: &mut Vec<Element>) {
    if stack.len() <= 1 {
        return;
    }
    if let Some(done) = stack.pop()
        && let Some(parent) = stack.last_mut()
    {
        parent.children.push(done);
    }
}

fn push_text(stack: &mut [Element], text: &str) {
    if text.is_empty() {
        return;
    }
    let Some(top) = stack.last_mut() else { return };
    if top.text.len() >= MAX_TEXT {
        return;
    }
    let room = MAX_TEXT - top.text.len();
    if text.len() <= room {
        top.text.push_str(text);
    } else {
        let mut cut = room;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        top.text.push_str(text.get(..cut).unwrap_or(""));
    }
}

/// The index of the `>` ending the tag that starts at `s[0] == '<'` (quotes respected).
fn tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// `name a="1" b='2'` → (name, attributes).
fn parse_tag(tag: &str) -> (&str, Vec<(String, String)>) {
    let tag = tag.trim_start();
    let name_end = tag.find(|c: char| c.is_whitespace()).unwrap_or(tag.len());
    let name = tag.get(..name_end).unwrap_or("");
    let mut attrs = Vec::new();
    let mut rest = tag.get(name_end..).unwrap_or("");
    while let Some(eq) = rest.find('=') {
        let key = rest.get(..eq).unwrap_or("").trim().to_string();
        let after = rest.get(eq + 1..).unwrap_or("").trim_start();
        let Some(q) = after.chars().next() else { break };
        let (value, next) = if q == '"' || q == '\'' {
            let body = after.get(1..).unwrap_or("");
            match body.find(q) {
                Some(end) => (body.get(..end).unwrap_or(""), body.get(end + 1..).unwrap_or("")),
                None => (body, ""),
            }
        } else {
            let end = after.find(char::is_whitespace).unwrap_or(after.len());
            (after.get(..end).unwrap_or(""), after.get(end..).unwrap_or(""))
        };
        if !key.is_empty() && attrs.len() < 256 {
            attrs.push((key, decode(value)));
        }
        rest = next;
    }
    (name, attrs)
}

/// Decode the predefined entities and character references.
pub fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(rest.get(..amp).unwrap_or(""));
        let after = rest.get(amp..).unwrap_or("");
        let semi = after.get(..after.len().min(12)).and_then(|w| w.find(';'));
        let Some(semi) = semi else {
            out.push('&');
            rest = after.get(1..).unwrap_or("");
            continue;
        };
        let entity = after.get(1..semi).unwrap_or("");
        let decoded = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => {
                u32::from_str_radix(e.get(2..).unwrap_or(""), 16).ok().and_then(char::from_u32)
            }
            e if e.starts_with('#') => e.get(1..).and_then(|n| n.parse::<u32>().ok()).and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = after.get(semi + 1..).unwrap_or("");
            }
            None => {
                out.push('&');
                rest = after.get(1..).unwrap_or("");
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_attributes_text_and_cdata() {
        let doc = parse(
            "<?xml version=\"1.0\"?>\n<!-- c --><!DOCTYPE x><root a=\"1 &lt; 2\" b='q\"'><item n=\"x>y\"/>\
             <msg>hi &amp; <![CDATA[<raw>]]> &#65;&#x42;</msg></root>",
        );
        let root = doc.child("root").unwrap();
        assert_eq!(root.attr("a"), Some("1 < 2"));
        assert_eq!(root.attr("b"), Some("q\""));
        assert_eq!(root.child("item").unwrap().attr("n"), Some("x>y"));
        assert_eq!(root.child_text("msg"), Some("hi & <raw> AB"));
        assert_eq!(doc.find("item").map(|e| e.name.as_str()), Some("item"));
    }

    #[test]
    fn truncated_and_malformed_input_never_panics() {
        let doc = parse("<a><b x=\"1\"><c>text");
        assert_eq!(doc.find("c").unwrap().text, "text");
        for bad in ["<", "<<<>>>", "</x></y>", "<a b=>", "<a b=\"unterminated", "&#xFFFFFFFF; &#99999999999;", "<é ü='ö'/>"] {
            let _ = parse(bad);
        }
        // Stray end tags are ignored; unclosed elements close at the end.
        let doc = parse("<a></b><c/></a>");
        assert_eq!(doc.child("a").unwrap().children.len(), 1);
        // Deep nesting stays bounded.
        let deep = "<a>".repeat(10_000);
        let doc = parse(&deep);
        assert_eq!(doc.children.len(), 1);
        // Huge text is capped.
        let doc = parse(&format!("<t>{}</t>", "x".repeat(MAX_TEXT + 10)));
        assert_eq!(doc.child("t").unwrap().text.len(), MAX_TEXT);
        assert_eq!(decode("&bogus; & &lt"), "&bogus; & &lt");
    }
}
