use crate::test_util::{assert_kind, checked, kind_at, offset_of, spans};
use crate::{
    FoldKind, FoldRange, HighlightKind as K, Language, OutlineItem, SymbolKind, folding_ranges, matching_bracket,
    outline,
};

const TS: Language = Language::TypeScript;
const JS: Language = Language::JavaScript;

fn fold(start_line: usize, end_line: usize, kind: FoldKind) -> FoldRange {
    FoldRange { start_line, end_line, kind }
}

fn names(language: Language, text: &str) -> Vec<(String, SymbolKind, usize)> {
    outline(language, text).into_iter().map(|i| (i.name, i.kind, i.depth)).collect()
}

// ------------------------------------------------------------------------------------------
// Highlighting

#[test]
fn keywords_functions_types() {
    let text = "export async function load(id: string): Promise<User | null> { const x = await api.get(id); return x ?? undefined; }";
    for kw in ["export", "async", "function", "const", "await", "return"] {
        assert_kind(TS, text, kw, K::Keyword);
    }
    assert_kind(TS, text, "load", K::Function);
    assert_kind(TS, text, "string", K::Type);
    assert_kind(TS, text, "Promise", K::Type);
    assert_kind(TS, text, "User", K::Type);
    assert_kind(TS, text, "null", K::Boolean);
    assert_kind(TS, text, "undefined", K::Boolean);
    assert_kind(TS, text, "api", K::Ident);
    assert_kind(TS, text, "get", K::Function);
    // In JavaScript `string` is just a name.
    assert_kind(JS, "let string = 1", "string", K::Ident);
}

#[test]
fn contextual_keywords() {
    let text = "import { a as b } from './m';\ntype T = keyof U;\nconst type = 1; obj.from(x); let of = 2; for (const v of list) {}";
    assert_kind(TS, text, "as", K::Keyword);
    assert_kind(TS, text, "from", K::Keyword);
    assert_eq!(kind_at(TS, text, 1, 0), K::Keyword);
    assert_kind(TS, text, "keyof", K::Keyword);
    assert_eq!(kind_at(TS, text, 2, 6), K::Ident); // `type` as a name
    assert_eq!(kind_at(TS, text, 2, 21), K::Function); // obj.from(
    assert_eq!(kind_at(TS, text, 2, 34), K::Ident); // `of` as a name
    assert_eq!(kind_at(TS, text, 2, 54), K::Keyword); // `of` in for…of
    // `type` is not a keyword in JavaScript.
    assert_kind(JS, "type T = 1", "type", K::Ident);
}

#[test]
fn constants_fields_this_and_arrow_functions() {
    let text = "const MAX_ITEMS = 10; this.items.push(MAX_ITEMS); const add = (a, b) => a + b; let neg = x => -x; const h = async function () {}; const g: Fn = (a): number => a;";
    assert_kind(JS, text, "MAX_ITEMS", K::Constant);
    assert_kind(JS, text, "this", K::SelfKeyword);
    assert_kind(JS, text, "items", K::Field);
    assert_kind(JS, text, "push", K::Function);
    assert_kind(JS, text, "add", K::Function);
    assert_kind(JS, text, "neg", K::Function);
    assert_kind(JS, text, "h", K::Function);
    assert_kind(TS, text, "g", K::Function);
    assert_kind(JS, "let total = a + b;", "total", K::Ident);
    assert_kind(JS, "obj?.prop", "prop", K::Field);
    assert_kind(JS, "f(...args)", "args", K::Ident);
    assert_kind(JS, "html`<b>${x}</b>`", "html", K::Function);
}

#[test]
fn strings_and_template_literals() {
    let text = "const s = 'it\\'s' + \"q\" + `a ${user.name + `in ${deep}`} b`;";
    let all = spans(JS, text);
    assert!(all.contains(&("'it\\'s'".into(), K::String)), "{all:?}");
    assert!(all.contains(&("\"q\"".into(), K::String)), "{all:?}");
    assert!(all.contains(&("`a ".into(), K::String)), "{all:?}");
    assert!(all.contains(&("$".into(), K::Punctuation)), "{all:?}");
    assert!(all.contains(&("name".into(), K::Field)), "{all:?}");
    assert!(all.contains(&("`in ".into(), K::String)), "{all:?}");
    assert!(all.contains(&("deep".into(), K::Ident)), "{all:?}");
    assert!(all.contains(&(" b`".into(), K::String)), "{all:?}");
    // Braces inside a substitution nest; the template resumes after the matching `}`.
    let t = "`${ {a: 1}.a } tail {not code}`";
    assert_kind(JS, t, " tail {not code}`", K::String);
    assert_eq!(matching_bracket(JS, t, 2), Some(offset_of(t, "} tail", 0)));
    // Templates span lines; an unterminated one runs to the end.
    assert_eq!(kind_at(JS, "x = `a\nb`; y", 1, 0), K::String);
    assert_eq!(kind_at(JS, "x = `a\nb", 1, 0), K::Invalid);
    assert_kind(JS, "x = 'open\ny", "'open", K::Invalid);
}

#[test]
fn regex_versus_division() {
    let text =
        "const r = /ab+c[/]\\//gi; const d = a / b / c; if (/x/.test(s)) {} x = (1) / 2; y = i++ / 2; return /re/;";
    let all = spans(JS, text);
    assert!(all.contains(&("/ab+c[/]\\//gi".into(), K::String)), "{all:?}");
    assert!(all.contains(&("/x/".into(), K::String)), "{all:?}");
    assert!(all.contains(&("/re/".into(), K::String)), "{all:?}");
    assert_eq!(all.iter().filter(|(t, k)| t == "/" && *k == K::Operator).count(), 4, "{all:?}");
    // An unclosed "regex" is a division.
    assert_kind(JS, "x = (a\n/ 2)", "/", K::Operator);
    // Brackets inside a regex do not count.
    let t = "f(/[)]/)";
    assert_eq!(matching_bracket(JS, t, 1), Some(7));
}

#[test]
fn numbers_comments_decorators_private_names() {
    let text = "#!/usr/bin/env node\n/** Doc */\n/// <reference path=\"x\" />\n// plain\n@Component({ a: 1 })\nclass A { #secret = 0x_ff + 1_000n + .5e3; get #x() { return this.#secret; } }";
    assert_kind(TS, text, "#!/usr/bin/env node", K::Comment);
    assert_kind(TS, text, "/** Doc */", K::DocComment);
    assert_kind(TS, text, "/// <reference path=\"x\" />", K::DocComment);
    assert_kind(TS, text, "// plain", K::Comment);
    assert_kind(TS, text, "@Component", K::Attribute);
    assert_kind(TS, text, "#secret", K::Field);
    assert_kind(TS, text, "0x_ff", K::Number);
    assert_kind(TS, text, "1_000n", K::Number);
    assert_kind(TS, text, ".5e3", K::Number);
    assert_kind(TS, text, "get", K::Keyword);
}

#[test]
fn jsx_elements() {
    let text = "const el = <div className=\"box\" onClick={() => go(1)}>\n  Hello, {user.name} (friend)\n  <Avatar.Image size={2} />\n</div>;";
    let all = spans(JS, text);
    assert!(all.contains(&("div".into(), K::Type)), "{all:?}");
    assert!(all.contains(&("className".into(), K::Field)), "{all:?}");
    assert!(all.contains(&("\"box\"".into(), K::String)), "{all:?}");
    assert!(all.contains(&("onClick".into(), K::Field)), "{all:?}");
    assert!(all.contains(&("go".into(), K::Function)), "{all:?}");
    assert!(all.contains(&("name".into(), K::Field)), "{all:?}");
    assert!(all.contains(&("Avatar.Image".into(), K::Type)), "{all:?}");
    assert!(all.contains(&("size".into(), K::Field)), "{all:?}");
    // Text is plain, and its parentheses are not brackets.
    assert_eq!(kind_at(JS, text, 1, 2), K::Plain);
    assert_eq!(matching_bracket(JS, text, offset_of(text, "(friend", 0)), None);
    // Code resumes after the element.
    assert_eq!(kind_at(JS, text, 3, 6), K::Punctuation);
    // Fragments and nesting through expressions.
    let t = "return (<>\n  {items.map(i => <li key={i}>{i}</li>)}\n</>);";
    let all = spans(JS, t);
    assert!(all.contains(&("li".into(), K::Type)), "{all:?}");
    assert!(all.contains(&("key".into(), K::Field)), "{all:?}");
    assert!(all.contains(&("map".into(), K::Function)), "{all:?}");
}

#[test]
fn comparisons_and_generics_are_not_jsx() {
    let a = "if (a <b) { c(); } // </x>";
    assert_kind(JS, a, "c", K::Function);
    let g = "const id = <T,>(x: T) => x; const f = <T>(x: T): T => x; let v = new Map<string, number>(); // </x>";
    assert_kind(TS, g, "id", K::Function);
    assert_kind(TS, g, "f", K::Function);
    assert_kind(TS, g, "Map", K::Type);
    assert_kind(TS, g, "number", K::Type);
    // Without any closing tag nearby a `<` stays an operator.
    assert_kind(TS, "x = <Foo>bar;", "Foo", K::Type);
    assert_kind(TS, "x = <Foo>bar;", "bar", K::Ident);
}

#[test]
fn non_ascii_and_crlf() {
    let h = checked(JS, "let s = 'ü'; // ä\r\nlet naïve = 1;\r\n");
    let cols: Vec<(usize, usize, K)> = h.line(0).iter().map(|s| (s.start, s.end, s.kind)).collect();
    assert_eq!(
        cols,
        [
            (0, 3, K::Keyword),
            (4, 5, K::Ident),
            (6, 7, K::Operator),
            (8, 11, K::String),
            (11, 12, K::Punctuation),
            (13, 18, K::Comment)
        ]
    );
    assert_kind(JS, "let naïve = 1;", "naïve", K::Ident);
}

#[test]
fn brackets_skip_strings_and_comments() {
    let text = "function f() {\n  const s = '}' + `}` + \"}\"; // }\n}";
    assert_eq!(matching_bracket(JS, text, offset_of(text, "{", 0)), Some(text.chars().count() - 1));
}

// ------------------------------------------------------------------------------------------
// Folding

#[test]
fn folds_blocks_imports_comments_and_jsx() {
    let text = "\
import a from 'a';
import {
  b,
} from 'b';
const c = require('c');
/**
 * Doc
 */
export function App() {
  return (
    <div>
      hi
    </div>
  );
}
";
    assert_eq!(
        folding_ranges(TS, text),
        vec![
            fold(0, 4, FoldKind::Imports),
            fold(1, 3, FoldKind::Block),
            fold(5, 7, FoldKind::Comment),
            fold(8, 14, FoldKind::Block),
            fold(9, 13, FoldKind::Block),
            fold(10, 12, FoldKind::Block),
        ]
    );
}

// ------------------------------------------------------------------------------------------
// Outline

#[test]
fn outline_lists_declarations_and_members() {
    let text = "\
import x from 'x';
export default function main() { function inner() {} }
export const handler = async (req: Req): Promise<void> => { const nested = () => 1; };
let count = 0;
@Injectable()
export abstract class Service<T> extends Base implements I {
  private readonly cache = new Map();
  static instance?: Service<any>;
  constructor(private api: Api) { super(); }
  async fetch<T>(id: string): Promise<T> { return this.api.get(id); }
  get size(): number { return 0; }
  #hidden() {}
  onClick = () => {};
  static { init(); }
}
interface Shape { area(): number }
type Id = string | number;
enum Color { Red, Green }
const enum Flag { A }
declare module 'ext' { export function f(): void; }
namespace App.Models {
  export class User {}
}
function* gen() {}
";
    assert_eq!(
        names(TS, text),
        vec![
            ("main".into(), SymbolKind::Function, 0),
            ("handler".into(), SymbolKind::Function, 0),
            ("Service".into(), SymbolKind::Struct, 0),
            ("constructor".into(), SymbolKind::Function, 1),
            ("fetch".into(), SymbolKind::Function, 1),
            ("size".into(), SymbolKind::Function, 1),
            ("#hidden".into(), SymbolKind::Function, 1),
            ("onClick".into(), SymbolKind::Function, 1),
            ("Shape".into(), SymbolKind::Trait, 0),
            ("Id".into(), SymbolKind::TypeAlias, 0),
            ("Color".into(), SymbolKind::Enum, 0),
            ("Flag".into(), SymbolKind::Enum, 0),
            ("'ext'".into(), SymbolKind::Module, 0),
            ("f".into(), SymbolKind::Function, 1),
            ("App.Models".into(), SymbolKind::Module, 0),
            ("User".into(), SymbolKind::Struct, 1),
            ("gen".into(), SymbolKind::Function, 0),
        ]
    );
    let items = outline(TS, text);
    assert_eq!(
        items[0],
        OutlineItem { name: "main".into(), kind: SymbolKind::Function, line: 1, column: 24, depth: 0 }
    );
}

#[test]
fn outline_handles_statements_without_semicolons() {
    let text = "const a = 1\nconst f = () => 2\nfoo()\nclass K {\n  x = 1\n  m() {}\n}\nconst g = function () {}\n";
    assert_eq!(
        names(JS, text),
        vec![
            ("f".into(), SymbolKind::Function, 0),
            ("K".into(), SymbolKind::Struct, 0),
            ("m".into(), SymbolKind::Function, 1),
            ("g".into(), SymbolKind::Function, 0),
        ]
    );
}

#[test]
fn every_function_handles_garbage() {
    for language in [JS, TS] {
        for text in
            ["", "`", "`${", "`${`${", "/", "/[", "<", "<a", "<a>", "</", "@", "#", "class", "function", "}", "${"]
        {
            let _ = checked(language, text);
            let _ = folding_ranges(language, text);
            let _ = outline(language, text);
            let _ = matching_bracket(language, text, 0);
        }
    }
}
