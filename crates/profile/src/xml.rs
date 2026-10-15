//! A small, tolerant XML tokenizer for the tables `xctrace export` writes.
//!
//! It yields start tags (with attributes), end tags and text, decodes the five predefined
//! entities and numeric character references, and skips the prolog, comments, DOCTYPE and
//! processing instructions. It does not validate nesting; malformed input ends the stream (an
//! unterminated tag) instead of panicking. All delimiters are ASCII, so every slice is taken at
//! a char boundary.

use std::borrow::Cow;

/// One token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// `<name a="v">` (`empty` for `<name/>`, which gets no matching [`Event::End`]).
    Start { name: &'a str, attrs: Vec<(&'a str, Cow<'a, str>)>, empty: bool },
    /// `</name>`.
    End { name: &'a str },
    /// Character data between tags (entity-decoded; whitespace-only text is skipped).
    Text(Cow<'a, str>),
}

impl Event<'_> {
    /// An attribute of a start tag.
    pub fn attr(&self, key: &str) -> Option<&str> {
        match self {
            Event::Start { attrs, .. } => attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_ref()),
            _ => None,
        }
    }
}

/// Iterates over the tokens of a document.
pub struct Reader<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(s: &'a str) -> Self {
        Self { s, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        self.s.get(self.pos..).unwrap_or("")
    }

    /// Skip past `end` (or to the end of the input).
    fn skip_past(&mut self, end: &str) {
        match self.rest().find(end) {
            Some(i) => self.pos = self.pos.saturating_add(i).saturating_add(end.len()),
            None => self.pos = self.s.len(),
        }
    }
}

impl<'a> Iterator for Reader<'a> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        loop {
            let rest = self.rest();
            if rest.is_empty() {
                return None;
            }
            if !rest.starts_with('<') {
                let end = rest.find('<').unwrap_or(rest.len());
                let text = rest.get(..end).unwrap_or("");
                self.pos = self.pos.saturating_add(end);
                if text.trim().is_empty() {
                    continue;
                }
                return Some(Event::Text(unescape(text)));
            }
            if rest.starts_with("<?") {
                self.skip_past("?>");
                continue;
            }
            if rest.starts_with("<!--") {
                self.skip_past("-->");
                continue;
            }
            if let Some(body) = rest.strip_prefix("<![CDATA[") {
                let end = body.find("]]>").unwrap_or(body.len());
                let text = body.get(..end).unwrap_or("");
                self.pos = self.pos.saturating_add(9).saturating_add(end).saturating_add(3).min(self.s.len());
                return Some(Event::Text(Cow::Borrowed(text)));
            }
            if rest.starts_with("<!") {
                self.skip_past(">");
                continue;
            }
            if let Some(body) = rest.strip_prefix("</") {
                let end = body.find('>')?;
                let name = body.get(..end).unwrap_or("").trim();
                self.pos = self.pos.saturating_add(2).saturating_add(end).saturating_add(1);
                return Some(Event::End { name });
            }
            return self.start_tag();
        }
    }
}

impl<'a> Reader<'a> {
    fn start_tag(&mut self) -> Option<Event<'a>> {
        let s = self.s;
        let bytes = s.as_bytes();
        let mut i = self.pos.saturating_add(1);
        let name_start = i;
        while let Some(b) = bytes.get(i) {
            if b.is_ascii_whitespace() || *b == b'/' || *b == b'>' {
                break;
            }
            i += 1;
        }
        let name = s.get(name_start..i)?;
        let mut attrs = Vec::new();
        loop {
            while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            match bytes.get(i)? {
                b'>' => {
                    self.pos = i + 1;
                    return Some(Event::Start { name, attrs, empty: false });
                }
                b'/' => {
                    // `/>` (a stray `/` is skipped).
                    if bytes.get(i + 1) == Some(&b'>') {
                        self.pos = i + 2;
                        return Some(Event::Start { name, attrs, empty: true });
                    }
                    i += 1;
                }
                _ => {
                    let key_start = i;
                    while let Some(b) = bytes.get(i) {
                        if b.is_ascii_whitespace() || *b == b'=' || *b == b'>' || *b == b'/' {
                            break;
                        }
                        i += 1;
                    }
                    let key = s.get(key_start..i)?;
                    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
                        i += 1;
                    }
                    if bytes.get(i) != Some(&b'=') {
                        // A bare attribute: no value.
                        if !key.is_empty() {
                            attrs.push((key, Cow::Borrowed("")));
                        } else {
                            i += 1;
                        }
                        continue;
                    }
                    i += 1;
                    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
                        i += 1;
                    }
                    let quote = *bytes.get(i)?;
                    if quote == b'"' || quote == b'\'' {
                        let value_start = i + 1;
                        let len = s.get(value_start..)?.bytes().position(|b| b == quote)?;
                        let value = s.get(value_start..value_start + len)?;
                        attrs.push((key, unescape(value)));
                        i = value_start + len + 1;
                    } else {
                        let value_start = i;
                        while let Some(b) = bytes.get(i) {
                            if b.is_ascii_whitespace() || *b == b'>' {
                                break;
                            }
                            i += 1;
                        }
                        attrs.push((key, unescape(s.get(value_start..i)?)));
                    }
                }
            }
        }
    }
}

/// Decode `&lt;` `&gt;` `&amp;` `&quot;` `&apos;` and `&#…;` references; unknown ones stay.
pub fn unescape(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(rest.get(..amp).unwrap_or(""));
        let after = rest.get(amp..).unwrap_or("");
        let semi = after.get(..after.len().min(12)).and_then(|w| w.find(';'));
        let decoded = semi.and_then(|semi| {
            let entity = after.get(1..semi)?;
            let c = match entity {
                "lt" => '<',
                "gt" => '>',
                "amp" => '&',
                "quot" => '"',
                "apos" => '\'',
                _ => {
                    let code = if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
                        u32::from_str_radix(hex, 16).ok()?
                    } else {
                        entity.strip_prefix('#')?.parse::<u32>().ok()?
                    };
                    char::from_u32(code)?
                }
            };
            Some((c, semi))
        });
        match decoded {
            Some((c, semi)) => {
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
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let doc = "<?xml version=\"1.0\"?>\n<!-- c --><a x='1' y=\"&lt;&amp;&#65;&#x42;\"><b/>t&gt;<c z=v>q</c></a>";
        let ev: Vec<Event> = Reader::new(doc).collect();
        assert_eq!(ev.len(), 7, "{ev:?}");
        assert_eq!(ev[0].attr("x"), Some("1"));
        assert_eq!(ev[0].attr("y"), Some("<&AB"));
        assert!(matches!(ev[1], Event::Start { name: "b", empty: true, .. }));
        assert_eq!(ev[2], Event::Text(Cow::Owned("t>".into())));
        assert_eq!(ev[3].attr("z"), Some("v"));
        assert_eq!(ev[6], Event::End { name: "a" });
    }

    #[test]
    fn hostile_input_ends_cleanly() {
        for doc in [
            "<",
            "<a",
            "<a x=",
            "<a x='1",
            "</a",
            "<a x=\"é\" ü>é<",
            "<![CDATA[x",
            "&#xFFFFFFFF; &#; &bogus; &",
            "<a / / >",
            "<a =1>",
            "<!DOCTYPE x",
            "<?pi",
        ] {
            let _ = Reader::new(doc).count();
        }
        assert_eq!(unescape("&#xFFFFFFFF;&lt"), "&#xFFFFFFFF;&lt");
        assert_eq!(unescape("a&amp;b"), "a&b");
        let ev: Vec<Event> = Reader::new("<![CDATA[<x>]]><y/>").collect();
        assert_eq!(ev[0], Event::Text(Cow::Borrowed("<x>")));
        assert_eq!(ev.len(), 2);
    }
}
