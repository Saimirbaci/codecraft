//! SQL (ANSI plus common PostgreSQL, MySQL and SQLite syntax): case-insensitive keywords and
//! data types, strings (`''` escapes, `E''`, `N''`, `B''`, `X''`, `U&''`), dollar quoting,
//! quoted identifiers, parameters, nested comments; folds for parenthesized groups,
//! `BEGIN`…`END`/`CASE`/`IF`/`LOOP` blocks, function bodies and comments; an outline of
//! `CREATE` statements.
//!
//! A dollar-quoted body right after `AS` or `DO` (`AS $$ … $$`) is lexed as code, so function
//! bodies are highlighted and folded; other dollar-quoted text is a string.

use crate::common::{self, Bracket, Doc, Span};
use crate::lex::{self, Lexed, TokSink, Tt};
use crate::{FoldKind, FoldRange, HighlightKind as K, OutlineItem, SymbolKind};

struct Analysis<'a> {
    lexed: Lexed<'a>,
    /// Dollar-quoted code bodies: (offset of the opening tag, offset of the closing tag).
    bodies: Vec<(usize, usize)>,
}

fn analyze(doc: &Doc) -> Analysis<'_> {
    let ascii = lex::ascii(doc, true);
    let (toks, bodies) = {
        let mut lexer = Lexer {
            doc,
            ascii: &ascii,
            sink: TokSink::with_capacity(doc.len() / 4),
            open_bodies: Vec::new(),
            bodies: Vec::new(),
        };
        lexer.run();
        (lexer.sink.toks, lexer.bodies)
    };
    let mut lexed = Lexed::new(doc, ascii, toks);
    classify(&mut lexed);
    Analysis { lexed, bodies }
}

pub(crate) fn spans(doc: &Doc) -> Vec<Span> {
    analyze(doc).lexed.spans()
}

pub(crate) fn brackets(doc: &Doc) -> Vec<Bracket> {
    analyze(doc).lexed.brackets()
}

struct Lexer<'a> {
    doc: &'a Doc,
    /// Lower-cased ASCII projection.
    ascii: &'a [u8],
    sink: TokSink,
    /// Open dollar-quoted code bodies: (tag including both `$`, offset of the opening tag).
    open_bodies: Vec<(Vec<char>, usize)>,
    bodies: Vec<(usize, usize)>,
}

fn is_word_char(c: char) -> bool {
    lex::is_ident_char(c) || c == '$'
}

impl Lexer<'_> {
    fn push(&mut self, start: usize, end: usize, tt: Tt, kind: K) {
        self.sink.push(start, end, tt, kind);
    }

    fn run(&mut self) {
        let doc = self.doc;
        let n = doc.len();
        let mut i = 0;
        while i < n {
            let c = doc.at(i);
            let next = match c {
                c if c.is_whitespace() => i + 1,
                '-' if doc.at(i + 1) == '-' => {
                    let e = doc.line_end_at(i);
                    self.push(i, e, Tt::LineComment, K::Comment);
                    e
                }
                '/' if doc.at(i + 1) == '*' => {
                    let (e, _) = lex::block_comment_end(doc, i, true);
                    self.push(i, e, Tt::BlockComment, K::Comment);
                    e
                }
                '\'' => self.string(i, i, false),
                '"' | '`' => {
                    // Quoted identifier; a doubled quote escapes it.
                    let (e, ok) = doubled_quote_end(doc, i + 1, c, false, false);
                    self.push(i, e, Tt::Other, if ok { K::Ident } else { K::Invalid });
                    e
                }
                '$' => self.dollar(i),
                c if c.is_ascii_digit() => self.number(i),
                '.' if lex::dot_number(doc, i) => self.number(i),
                c if lex::is_ident_start(c) => self.word(i),
                ':' if doc.at(i + 1) == ':' => {
                    self.push(i, i + 1, Tt::Sym(':'), K::Operator);
                    self.push(i + 1, i + 2, Tt::Sym(':'), K::Operator);
                    i + 2
                }
                ':' if lex::is_ident_start(doc.at(i + 1))
                    && i.checked_sub(1)
                        .is_none_or(|k| !(lex::is_ident_char(doc.at(k)) || matches!(doc.at(k), ')' | ']'))) =>
                {
                    let e = doc.scan_while(i + 1, lex::is_ident_char);
                    self.push(i, e, Tt::Other, K::Constant);
                    e
                }
                '?' if !matches!(doc.at(i + 1), '|' | '&') => {
                    self.push(i, i + 1, Tt::Other, K::Constant);
                    i + 1
                }
                '@' if lex::is_ident_start(doc.at(i + 1)) || doc.at(i + 1) == '@' => {
                    let e = doc.scan_while(doc.scan_while(i + 1, |c| c == '@'), is_word_char);
                    self.push(i, e, Tt::Other, K::Constant);
                    e
                }
                c if c.is_ascii_punctuation() => {
                    // `:` only appears in `:=` and slices: an operator.
                    let kind = if c == ':' { K::Operator } else { lex::sym_kind(c) };
                    self.push(i, i + 1, Tt::Sym(c), kind);
                    i + 1
                }
                _ => {
                    self.push(i, i + 1, Tt::Other, K::Invalid);
                    i + 1
                }
            };
            i = next.max(i + 1);
        }
    }

    fn number(&mut self, i: usize) -> usize {
        let e = lex::number_end(self.doc, i, false);
        self.push(i, e, Tt::Lit, K::Number);
        e
    }

    /// A `'…'` string whose prefix starts at `start` and quote is at `q`.
    fn string(&mut self, start: usize, q: usize, backslash: bool) -> usize {
        let (e, ok) = doubled_quote_end(self.doc, q + 1, '\'', backslash, true);
        self.push(start, e, Tt::Lit, if ok { K::String } else { K::Invalid });
        e
    }

    fn word(&mut self, i: usize) -> usize {
        let doc = self.doc;
        let e = doc.scan_while(i, is_word_char);
        let w = self.ascii.get(i..e).unwrap_or(&[]);
        match (w, doc.at(e)) {
            (b"e", '\'') => return self.string(i, e, true),
            (b"n" | b"b" | b"x", '\'') => return self.string(i, e, false),
            (b"u", '&') if doc.at(e + 1) == '\'' => return self.string(i, e + 1, false),
            (b"u", '&') if doc.at(e + 1) == '"' => {
                let (end, ok) = doubled_quote_end(doc, e + 2, '"', false, false);
                self.push(i, end, Tt::Other, if ok { K::Ident } else { K::Invalid });
                return end;
            }
            _ => {}
        }
        self.push(i, e, Tt::Word, K::Ident);
        e
    }

    /// `$1` parameters, `$tag$` / `$$` dollar quoting.
    fn dollar(&mut self, i: usize) -> usize {
        let doc = self.doc;
        if doc.at(i + 1).is_ascii_digit() {
            let e = doc.scan_while(i + 1, |c| c.is_ascii_digit());
            self.push(i, e, Tt::Other, K::Constant);
            return e;
        }
        let name_end =
            if lex::is_ident_start(doc.at(i + 1)) { doc.scan_while(i + 1, lex::is_ident_char) } else { i + 1 };
        if doc.at(name_end) != '$' {
            self.push(i, i + 1, Tt::Other, K::Invalid);
            return i + 1;
        }
        let tag_end = name_end + 1;
        let tag = doc.slice(i, tag_end);
        if self.open_bodies.last().is_some_and(|(open, _)| open.as_slice() == tag) {
            if let Some((_, start)) = self.open_bodies.pop() {
                self.bodies.push((start, i));
            }
            self.push(i, tag_end, Tt::Lit, K::String);
            return tag_end;
        }
        let code_body =
            self.sink.recent(0).is_some_and(|t| matches!(self.ascii.get(t.start..t.end), Some(b"as" | b"do")));
        if code_body {
            self.open_bodies.push((tag.to_vec(), i));
            self.push(i, tag_end, Tt::Lit, K::String);
            return tag_end;
        }
        let close: String = tag.iter().collect();
        match doc.find_str(tag_end, &close) {
            Some(j) => {
                let e = j + tag.len();
                self.push(i, e, Tt::Lit, K::String);
                e
            }
            None => {
                self.push(i, doc.len(), Tt::Lit, K::Invalid);
                doc.len()
            }
        }
    }
}

/// End of a literal whose body starts at `body` and ends at `quote`, where a doubled quote is
/// an escaped one (and, with `backslash`, `\x` escapes). Without `multiline`, the line end
/// stops it unterminated. Returns (end, terminated).
fn doubled_quote_end(doc: &Doc, body: usize, quote: char, backslash: bool, multiline: bool) -> (usize, bool) {
    let n = doc.len();
    let mut j = body;
    while j < n {
        match doc.at(j) {
            '\\' if backslash => j += 2,
            c if c == quote => {
                if doc.at(j + 1) == quote {
                    j += 2;
                } else {
                    return (j + 1, true);
                }
            }
            '\n' if !multiline => return (j, false),
            _ => j += 1,
        }
    }
    (n, false)
}

// ---------------------------------------------------------------------------------------------
// Classification

fn is_keyword(w: &[u8]) -> bool {
    matches!(
        w,
        b"abort"
            | b"action"
            | b"add"
            | b"after"
            | b"all"
            | b"alter"
            | b"always"
            | b"analyze"
            | b"and"
            | b"any"
            | b"array"
            | b"as"
            | b"asc"
            | b"attach"
            | b"auto_increment"
            | b"autoincrement"
            | b"before"
            | b"begin"
            | b"between"
            | b"by"
            | b"call"
            | b"cascade"
            | b"case"
            | b"cast"
            | b"check"
            | b"close"
            | b"cluster"
            | b"collate"
            | b"column"
            | b"comment"
            | b"commit"
            | b"concurrently"
            | b"conflict"
            | b"constraint"
            | b"continue"
            | b"copy"
            | b"create"
            | b"cross"
            | b"current"
            | b"current_date"
            | b"current_time"
            | b"current_timestamp"
            | b"current_user"
            | b"cursor"
            | b"database"
            | b"deallocate"
            | b"declare"
            | b"default"
            | b"deferrable"
            | b"deferred"
            | b"definer"
            | b"delete"
            | b"delimiter"
            | b"desc"
            | b"describe"
            | b"deterministic"
            | b"distinct"
            | b"do"
            | b"domain"
            | b"drop"
            | b"each"
            | b"else"
            | b"elseif"
            | b"elsif"
            | b"end"
            | b"engine"
            | b"escape"
            | b"except"
            | b"exception"
            | b"exclusive"
            | b"execute"
            | b"exists"
            | b"exit"
            | b"explain"
            | b"extension"
            | b"fetch"
            | b"filter"
            | b"first"
            | b"following"
            | b"for"
            | b"foreign"
            | b"from"
            | b"full"
            | b"function"
            | b"generated"
            | b"glob"
            | b"grant"
            | b"group"
            | b"handler"
            | b"having"
            | b"identity"
            | b"if"
            | b"ignore"
            | b"ilike"
            | b"immediate"
            | b"immutable"
            | b"in"
            | b"index"
            | b"inherits"
            | b"initially"
            | b"inner"
            | b"inout"
            | b"insert"
            | b"instead"
            | b"intersect"
            | b"into"
            | b"invoker"
            | b"is"
            | b"isnull"
            | b"isolation"
            | b"join"
            | b"key"
            | b"language"
            | b"last"
            | b"lateral"
            | b"leave"
            | b"left"
            | b"level"
            | b"like"
            | b"limit"
            | b"listen"
            | b"local"
            | b"lock"
            | b"loop"
            | b"materialized"
            | b"match"
            | b"merge"
            | b"natural"
            | b"next"
            | b"no"
            | b"not"
            | b"nothing"
            | b"notify"
            | b"notnull"
            | b"nowait"
            | b"nulls"
            | b"of"
            | b"offset"
            | b"on"
            | b"only"
            | b"open"
            | b"or"
            | b"order"
            | b"out"
            | b"outer"
            | b"over"
            | b"owner"
            | b"partition"
            | b"perform"
            | b"policy"
            | b"pragma"
            | b"preceding"
            | b"precision"
            | b"prepare"
            | b"primary"
            | b"procedure"
            | b"raise"
            | b"range"
            | b"read"
            | b"recursive"
            | b"references"
            | b"refresh"
            | b"regexp"
            | b"release"
            | b"rename"
            | b"repeat"
            | b"replace"
            | b"restrict"
            | b"return"
            | b"returning"
            | b"returns"
            | b"revoke"
            | b"right"
            | b"role"
            | b"rollback"
            | b"row"
            | b"rows"
            | b"savepoint"
            | b"schema"
            | b"security"
            | b"select"
            | b"sequence"
            | b"set"
            | b"setof"
            | b"show"
            | b"signed"
            | b"similar"
            | b"some"
            | b"stable"
            | b"start"
            | b"stored"
            | b"strict"
            | b"table"
            | b"tablespace"
            | b"temp"
            | b"temporary"
            | b"then"
            | b"to"
            | b"top"
            | b"transaction"
            | b"trigger"
            | b"truncate"
            | b"type"
            | b"unbounded"
            | b"union"
            | b"unique"
            | b"unlogged"
            | b"unsigned"
            | b"until"
            | b"update"
            | b"use"
            | b"user"
            | b"using"
            | b"vacuum"
            | b"values"
            | b"variadic"
            | b"view"
            | b"virtual"
            | b"volatile"
            | b"when"
            | b"where"
            | b"while"
            | b"window"
            | b"with"
            | b"within"
            | b"without"
            | b"work"
            | b"zerofill"
            | b"zone"
    )
}

fn is_type(w: &[u8]) -> bool {
    matches!(
        w,
        b"int"
            | b"integer"
            | b"smallint"
            | b"bigint"
            | b"tinyint"
            | b"mediumint"
            | b"int2"
            | b"int4"
            | b"int8"
            | b"serial"
            | b"smallserial"
            | b"bigserial"
            | b"decimal"
            | b"numeric"
            | b"number"
            | b"real"
            | b"float"
            | b"float4"
            | b"float8"
            | b"double"
            | b"money"
            | b"char"
            | b"character"
            | b"varchar"
            | b"varchar2"
            | b"nchar"
            | b"nvarchar"
            | b"nvarchar2"
            | b"varying"
            | b"text"
            | b"tinytext"
            | b"mediumtext"
            | b"longtext"
            | b"citext"
            | b"clob"
            | b"nclob"
            | b"blob"
            | b"tinyblob"
            | b"mediumblob"
            | b"longblob"
            | b"bytea"
            | b"binary"
            | b"varbinary"
            | b"bit"
            | b"varbit"
            | b"boolean"
            | b"bool"
            | b"date"
            | b"time"
            | b"timetz"
            | b"timestamp"
            | b"timestamptz"
            | b"datetime"
            | b"year"
            | b"interval"
            | b"uuid"
            | b"json"
            | b"jsonb"
            | b"xml"
            | b"inet"
            | b"cidr"
            | b"macaddr"
            | b"macaddr8"
            | b"tsvector"
            | b"tsquery"
            | b"hstore"
            | b"ltree"
            | b"point"
            | b"polygon"
            | b"circle"
            | b"geometry"
            | b"geography"
            | b"oid"
            | b"regclass"
            | b"regtype"
            | b"record"
            | b"void"
            | b"enum"
    )
}

fn classify(lx: &mut Lexed<'_>) {
    let kinds: Vec<(usize, K)> = (0..lx.len()).filter(|&p| lx.is_word_tok(p)).map(|p| (p, word_kind(lx, p))).collect();
    for (p, kind) in kinds {
        lx.set(p, kind);
    }
}

fn word_kind(lx: &Lexed<'_>, p: usize) -> K {
    let w = lx.word(p);
    if lx.after_member_access(p, false) {
        return if lx.is_sym(p + 1, '(') { K::Function } else { K::Field };
    }
    if matches!(w, b"true" | b"false" | b"null") {
        return K::Boolean;
    }
    if is_type(w) {
        return K::Type;
    }
    if is_keyword(w) {
        return K::Keyword;
    }
    // `name(` calls a function, except a table (or CTE) name before its column list.
    let table = p.checked_sub(1).is_some_and(|q| {
        matches!(lx.word(q), b"table" | b"into" | b"references" | b"on" | b"exists" | b"with" | b"recursive" | b"only")
    });
    if lx.is_sym(p + 1, '(') && !table {
        return K::Function;
    }
    K::Ident
}

// ---------------------------------------------------------------------------------------------
// Folding

pub(crate) fn folding_ranges(doc: &Doc) -> Vec<FoldRange> {
    let Analysis { lexed: lx, bodies } = analyze(doc);
    let mut out = Vec::new();
    lx.bracket_folds(&mut out);
    lx.comment_folds(&mut out);
    for (start, end) in bodies {
        common::push_fold(&mut out, doc.index.line_of(start), doc.index.line_of(end), FoldKind::Block);
    }
    for t in &lx.toks {
        if t.tt == Tt::Lit && t.kind == K::String && doc.at(t.start) == '$' {
            let start = doc.index.line_of(t.start);
            common::push_fold(
                &mut out,
                start,
                doc.index.line_of(t.end.saturating_sub(1).max(t.start)),
                FoldKind::Block,
            );
        }
    }
    block_folds(&lx, &mut out);
    common::finish_folds(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Begin,
    Case,
    If,
    Loop,
}

/// `BEGIN`…`END`, `CASE`…`END [CASE]`, `IF`…`END IF`, `LOOP`…`END LOOP`.
fn block_folds(lx: &Lexed<'_>, out: &mut Vec<FoldRange>) {
    let mut open: Vec<(Block, usize)> = Vec::new();
    let close =
        |open: &mut Vec<(Block, usize)>, pred: &dyn Fn(Block) -> bool, line: usize, out: &mut Vec<FoldRange>| {
            if let Some(at) = open.iter().rposition(|&(b, _)| pred(b)) {
                if let Some(&(_, start)) = open.get(at) {
                    common::push_fold(out, start, line, FoldKind::Block);
                }
                open.truncate(at);
            }
        };
    for p in 0..lx.len() {
        let prev = p.checked_sub(1).map_or(&[][..], |q| lx.word(q));
        let after_end = prev == b"end";
        match lx.word(p) {
            b"begin" => {
                let transaction = lx.is_sym(p + 1, ';')
                    || p + 1 >= lx.len()
                    || matches!(
                        lx.word(p + 1),
                        b"transaction"
                            | b"work"
                            | b"tran"
                            | b"isolation"
                            | b"deferred"
                            | b"immediate"
                            | b"exclusive"
                            | b"read"
                    );
                if !transaction {
                    open.push((Block::Begin, lx.line(p)));
                }
            }
            b"case" if !after_end => open.push((Block::Case, lx.line(p))),
            b"loop" if !after_end => open.push((Block::Loop, lx.line(p))),
            b"if" if !after_end => {
                let statement = p.checked_sub(1).is_none_or(|q| lx.is_sym(q, ';'))
                    || matches!(prev, b"then" | b"else" | b"loop" | b"begin");
                if statement {
                    open.push((Block::If, lx.line(p)));
                }
            }
            b"end" => {
                let line = lx.line(p);
                match lx.word(p + 1) {
                    b"if" => close(&mut open, &|b| b == Block::If, line, out),
                    b"loop" => close(&mut open, &|b| b == Block::Loop, line, out),
                    b"case" => close(&mut open, &|b| b == Block::Case, line, out),
                    _ => close(&mut open, &|b| matches!(b, Block::Begin | Block::Case), line, out),
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Outline

/// `CREATE … TABLE|VIEW|INDEX|FUNCTION|PROCEDURE|TRIGGER|SCHEMA|… name` as sections named like
/// `TABLE users`.
pub(crate) fn outline(doc: &Doc) -> Vec<OutlineItem> {
    const MAX_MODIFIERS: usize = 8;
    let Analysis { lexed: lx, .. } = analyze(doc);
    let mut items = Vec::new();
    for p in 0..lx.len() {
        if !lx.is_word(p, "create") {
            continue;
        }
        let mut q = p + 1;
        for _ in 0..MAX_MODIFIERS {
            match lx.word(q) {
                b"or" | b"replace" | b"temp" | b"temporary" | b"unlogged" | b"global" | b"local" | b"unique"
                | b"recursive" | b"constraint" | b"definer" | b"algorithm" | b"sql" | b"security" | b"invoker" => {
                    q += 1
                }
                _ if lx.is_sym(q, '=') => q += 2,
                _ => break,
            }
        }
        let object = match lx.word(q) {
            b"materialized" if lx.is_word(q + 1, "view") => {
                q += 1;
                "MATERIALIZED VIEW"
            }
            b"table" => "TABLE",
            b"view" => "VIEW",
            b"index" => "INDEX",
            b"function" => "FUNCTION",
            b"procedure" => "PROCEDURE",
            b"trigger" => "TRIGGER",
            b"schema" => "SCHEMA",
            b"sequence" => "SEQUENCE",
            b"type" => "TYPE",
            b"domain" => "DOMAIN",
            b"extension" => "EXTENSION",
            b"database" => "DATABASE",
            _ => continue,
        };
        q += 1;
        if lx.is_word(q, "concurrently") {
            q += 1;
        }
        if lx.is_word(q, "if") && lx.is_word(q + 1, "not") && lx.is_word(q + 2, "exists") {
            q += 3;
        }
        if !matches!(lx.tt(q), Some(Tt::Word | Tt::Other)) || lx.is_word(q, "on") {
            continue;
        }
        let mut name = lx.text(q);
        let mut r = q;
        while lx.is_sym(r + 1, '.') && matches!(lx.tt(r + 2), Some(Tt::Word | Tt::Other)) && lx.adjacent(r + 1) {
            name.push('.');
            name.push_str(&lx.text(r + 2));
            r += 2;
        }
        if let Some(item) = lx.item(q, format!("{object} {name}"), SymbolKind::Section, 0) {
            items.push(item);
        }
    }
    items
}

// ---------------------------------------------------------------------------------------------
// Tokens for the formatter and the name analysis (`sql_format`, `sql_scope`)

/// The classified tokens of `doc` (comments included) and its dollar-quoted code bodies as
/// (offset of the opening tag, offset of the closing tag).
pub(crate) fn tokens(doc: &Doc) -> (Vec<lex::Tok>, Vec<(usize, usize)>) {
    let Analysis { lexed, bodies } = analyze(doc);
    (lexed.toks, bodies)
}

#[cfg(test)]
mod tests {
    use crate::test_util::{assert_kind, checked, kind_at, offset_of, spans};
    use crate::{
        FoldKind, FoldRange, HighlightKind as K, Language, OutlineItem, SymbolKind, folding_ranges, matching_bracket,
        outline,
    };

    const S: Language = Language::Sql;

    fn fold(start_line: usize, end_line: usize, kind: FoldKind) -> FoldRange {
        FoldRange { start_line, end_line, kind }
    }

    #[test]
    fn keywords_types_functions_case_insensitive() {
        let text = "SELECT u.id, count(*) AS n FROM users u WHERE u.active = TRUE and deleted_at IS NULL;\ncreate table t (id BIGSERIAL primary key, name varchar(80) not null, at timestamp with time zone);";
        for kw in
            ["SELECT", "AS", "FROM", "WHERE", "and", "IS", "create", "table", "primary", "key", "not", "with", "zone"]
        {
            assert_kind(S, text, kw, K::Keyword);
        }
        assert_kind(S, text, "id", K::Field);
        assert_kind(S, text, "count", K::Function);
        assert_kind(S, text, "users", K::Ident);
        // A table name before its column list is not a call.
        assert_eq!(kind_at(S, text, 1, 13), K::Ident);
        assert_kind(S, "INSERT INTO t(a) VALUES (now());", "t", K::Ident);
        assert_kind(S, "INSERT INTO t(a) VALUES (now());", "now", K::Function);
        assert_kind(S, "x := 1;", ":=", K::Operator);
        assert_kind(S, text, "TRUE", K::Boolean);
        assert_kind(S, text, "NULL", K::Boolean);
        assert_kind(S, text, "BIGSERIAL", K::Type);
        assert_kind(S, text, "varchar", K::Type);
        assert_kind(S, text, "80", K::Number);
        assert_kind(S, text, "timestamp", K::Type);
        assert_kind(S, text, "time", K::Type);
    }

    #[test]
    fn strings_and_quoted_identifiers() {
        let text = "SELECT 'it''s', E'a\\'b', N'n', X'ff', U&'\\0041', \"Weird \"\"Col\"\"\", `my col` FROM t;";
        for s in ["'it''s'", "E'a\\'b'", "N'n'", "X'ff'", "U&'\\0041'"] {
            assert_kind(S, text, s, K::String);
        }
        assert_kind(S, text, "\"Weird \"\"Col\"\"\"", K::Ident);
        assert_kind(S, text, "`my col`", K::Ident);
        // Strings may span lines; an unterminated one runs to the end.
        assert_eq!(kind_at(S, "SELECT 'a\nb' FROM t", 1, 0), K::String);
        assert_eq!(kind_at(S, "SELECT 'a\nb FROM t", 1, 0), K::Invalid);
    }

    #[test]
    fn dollar_quoting() {
        let text = "SELECT $tag$it's $$ fine$tag$, $$x$$;";
        assert_kind(S, text, "$tag$it's $$ fine$tag$", K::String);
        assert_kind(S, text, "$$x$$", K::String);
        // A function body after AS is code.
        let body = "CREATE FUNCTION f() RETURNS int AS $$\nBEGIN\n  RETURN 1;\nEND;\n$$ LANGUAGE plpgsql;";
        assert_eq!(kind_at(S, body, 1, 0), K::Keyword);
        assert_eq!(kind_at(S, body, 2, 9), K::Number);
        assert_eq!(kind_at(S, body, 4, 0), K::String);
        assert_eq!(kind_at(S, body, 4, 3), K::Keyword);
        assert_eq!(kind_at(S, "SELECT $a$ never closed\nx", 1, 0), K::Invalid);
    }

    #[test]
    fn parameters_and_operators() {
        let text = "SELECT * FROM t WHERE a = $1 AND b = ? AND c = :name AND d = @var AND e::text = @@version AND f[1:n] AND j ?| k;";
        assert_kind(S, text, "$1", K::Constant);
        assert_kind(S, text, "?", K::Constant);
        assert_kind(S, text, ":name", K::Constant);
        assert_kind(S, text, "@var", K::Constant);
        assert_kind(S, text, "@@version", K::Constant);
        assert_kind(S, text, "::", K::Operator);
        assert_kind(S, text, "text", K::Type);
        assert_kind(S, text, "n", K::Ident);
        assert_kind(S, text, "?|", K::Operator);
    }

    #[test]
    fn comments_nest() {
        let text = "-- line\n/* outer /* inner */ still */ SELECT 1;";
        assert_kind(S, text, "-- line", K::Comment);
        assert_kind(S, text, "/* outer /* inner */ still */", K::Comment);
        assert_kind(S, text, "SELECT", K::Keyword);
    }

    #[test]
    fn non_ascii_and_crlf() {
        let h = checked(S, "SELECT 'ü' -- ä\r\nFROM t;\r\n");
        let cols: Vec<(usize, usize, K)> = h.line(0).iter().map(|s| (s.start, s.end, s.kind)).collect();
        assert_eq!(cols, [(0, 6, K::Keyword), (7, 10, K::String), (11, 16, K::Comment)]);
    }

    #[test]
    fn brackets_skip_strings() {
        let text = "SELECT f(')', \"(\", $$)$$, -- )\n x)";
        assert_eq!(matching_bracket(S, text, offset_of(text, "(", 0)), Some(text.chars().count() - 1));
    }

    #[test]
    fn folds_groups_blocks_and_comments() {
        let text = "\
-- a
-- b
CREATE TABLE t (
  id int
);
CREATE FUNCTION f() RETURNS int AS $$
BEGIN
  IF x THEN
    RETURN CASE
      WHEN a THEN 1
    END;
  END IF;
END;
$$ LANGUAGE plpgsql;
BEGIN;
COMMIT;
";
        assert_eq!(
            folding_ranges(S, text),
            vec![
                fold(0, 1, FoldKind::Comment),
                fold(2, 4, FoldKind::Block),
                fold(5, 13, FoldKind::Block),
                fold(6, 12, FoldKind::Block),
                fold(7, 11, FoldKind::Block),
                fold(8, 10, FoldKind::Block),
            ]
        );
    }

    #[test]
    fn outline_lists_create_statements() {
        let text = "\
CREATE TABLE IF NOT EXISTS public.users (id int);
create or replace view active_users as select 1;
CREATE UNIQUE INDEX CONCURRENTLY idx_users_email ON users (email);
CREATE INDEX ON users (name);
CREATE MATERIALIZED VIEW mv AS SELECT 1;
CREATE FUNCTION add(a int) RETURNS int AS $$ SELECT a $$ LANGUAGE sql;
CREATE TRIGGER trg BEFORE INSERT ON users FOR EACH ROW EXECUTE FUNCTION f();
CREATE SCHEMA \"Billing\";
DROP TABLE x;
";
        let items: Vec<String> = outline(S, text).into_iter().map(|i| i.name).collect();
        assert_eq!(
            items,
            [
                "TABLE public.users",
                "VIEW active_users",
                "INDEX idx_users_email",
                "MATERIALIZED VIEW mv",
                "FUNCTION add",
                "TRIGGER trg",
                "SCHEMA \"Billing\"",
            ]
        );
        assert_eq!(
            outline(S, text)[0],
            OutlineItem { name: "TABLE public.users".into(), kind: SymbolKind::Section, line: 0, column: 27, depth: 0 }
        );
    }

    #[test]
    fn every_function_handles_garbage() {
        for text in
            ["", "'", "$", "$$", "$a", "$a$", "\"", "/*", "/* /*", "E'", "U&", ":", "@", "create", "end", "begin"]
        {
            let _ = checked(S, text);
            let _ = folding_ranges(S, text);
            let _ = outline(S, text);
            let _ = matching_bracket(S, text, 0);
        }
        assert_eq!(spans(S, "").len(), 0);
    }
}
