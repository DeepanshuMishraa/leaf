//! Syntax colors for code blocks, from the Highlights WebAssembly lexers.
//!
//! PDF and EPUB code has no language tag. A fence language is used when the
//! block has one; otherwise the source is guessed, and anything uncertain
//! stays plain. Token kinds are the terminal's own palette, not a fixed theme.

use std::sync::{LazyLock, Mutex};

use wasmtime::{Caller, Config, Engine as WasmEngine, Linker, Module, OptLevel, Store, TypedFunc};

/// What a token is, for coloring. Identifiers stay plain so they use the
/// terminal's text color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Comment,
    Keyword,
    String,
    Number,
    Function,
    Type,
    Constant,
    Tag,
    Attribute,
    Punct,
    Added,
    Removed,
    Special,
}

const WASM: &[u8] = include_bytes!("../vendor/highlights/highlights.wasm");
/// Input lives on the second page. Lexers read past the end, so leave slack.
const INPUT: usize = 65_536;
const SLACK: usize = 96;
/// A code block larger than this stays plain. Layout is already the slow part.
const MAX_BYTES: usize = 200_000;

/// One kind per character of `code`, or empty when the block stays plain.
pub fn kinds(code: &str, hint: Option<&str>) -> Vec<Kind> {
    let Some((id, _)) = resolve(hint, code) else {
        return Vec::new();
    };
    if code.len() > MAX_BYTES {
        return Vec::new();
    }
    with_engine(|engine| engine.tokenize(id, code.as_bytes())).unwrap_or_default()
}

/// The language a block will be lexed as, when one is known.
fn resolve(hint: Option<&str>, code: &str) -> Option<(u8, &'static str)> {
    if let Some(hint) = hint {
        let (id, name) = lookup(hint)?;
        return (id != 0).then_some((id, name));
    }
    guess(code)
}

fn lookup(name: &str) -> Option<(u8, &'static str)> {
    let lower = name.to_ascii_lowercase();
    let index = ALIASES
        .binary_search_by_key(&lower.as_str(), |&(alias, _)| alias)
        .ok()?;
    let id = ALIASES[index].1 as usize;
    Some((id as u8, CANON[id]))
}

fn guess(code: &str) -> Option<(u8, &'static str)> {
    if let Some(lang) = shebang(code) {
        return Some(lang);
    }
    let sample = &code[..code.floor_char_boundary(code.len().min(16_384))];
    let scored = [
        score_rust(sample),
        score_python(sample),
        score_js(sample),
        score_ts(sample),
        score_go(sample),
        score_cpp(sample),
        score_c(sample),
        score_java(sample),
        score_sql(sample),
        score_json(sample),
        score_html(sample),
        score_css(sample),
        score_bash(sample),
        score_ruby(sample),
        score_swift(sample),
        score_php(sample),
    ];
    let mut best = 0u32;
    let mut best_at: Option<usize> = None;
    let mut second = 0u32;
    for (i, score) in scored.into_iter().enumerate() {
        if score > best {
            second = best;
            best = score;
            best_at = Some(i);
        } else if score > second {
            second = score;
        }
    }
    if best >= 2 && best > second {
        lookup(LANG_ORDER[best_at?])
    } else if c_like(sample) {
        lookup("js")
    } else {
        None
    }
}

/// Code with no telling keyword can still be plainly C-family: braces,
/// statements ending in `;`, `//` comments, `&&`. The JS lexer colors
/// comments, strings, numbers and control keywords right for all of them.
fn c_like(code: &str) -> bool {
    let mut signals = 0;
    if code.lines().any(|line| line.trim_end().ends_with(';')) {
        signals += 1;
    }
    if code.lines().any(|line| line.trim_end().ends_with('{')) && code.contains('}') {
        signals += 1;
    }
    if code.contains("//") && !code.contains("://") {
        signals += 1;
    }
    if code.contains("&&") || code.contains("||") || code.contains("!=") || code.contains("==") {
        signals += 1;
    }
    if ["if (", "for (", "while (", "return "]
        .iter()
        .any(|word| code.contains(word))
    {
        signals += 1;
    }
    signals >= 3
}

const LANG_ORDER: [&str; 16] = [
    "rust", "python", "js", "ts", "go", "cpp", "c", "java", "sql", "json", "html", "css", "bash",
    "ruby", "swift", "php",
];

fn shebang(code: &str) -> Option<(u8, &'static str)> {
    let rest = code
        .lines()
        .next()?
        .trim_start()
        .strip_prefix("#!")?
        .to_ascii_lowercase();
    let name = if rest.contains("python") {
        "python"
    } else if rest.contains("node") || rest.contains("bun") || rest.contains("deno") {
        "js"
    } else if rest.contains("ruby") {
        "ruby"
    } else if rest.contains("perl") {
        "perl"
    } else if rest.contains("php") {
        "php"
    } else if rest.contains("bash") || rest.contains("zsh") || rest.contains("/sh") {
        "bash"
    } else {
        return None;
    };
    lookup(name)
}

fn score_rust(code: &str) -> u32 {
    let mut n = 0;
    if code.contains("fn ") || code.contains("fn(") {
        n += 2;
    }
    if code.contains("impl ")
        || code.contains("pub ")
        || code.contains("struct ")
        || code.contains("enum ")
        || code.contains("trait ")
    {
        n += 2;
    }
    if code.contains("let mut") || code.contains(" mut ") {
        n += 2;
    } else if code.contains("let ") {
        n += 1;
    }
    if code.contains("println!") || code.contains("vec![") || code.contains("#[") {
        n += 2;
    }
    if code.contains("const ") && code.contains(':') {
        n += 2;
    }
    n
}

fn score_python(code: &str) -> u32 {
    let mut n = 0;
    if code.contains("def ") {
        n += 2;
    }
    if code.contains("elif ") || code.contains("self.") || code.contains("None") {
        n += 2;
    }
    if code.contains("import ") || code.contains("from ") {
        n += 2;
    }
    if code.contains("print(") {
        n += 1;
    }
    n
}

fn score_js(code: &str) -> u32 {
    let mut n = 0;
    if code.contains("function ") || code.contains("function(") || code.contains("=>") {
        n += 2;
    }
    if code.contains("console.") || code.contains("document.") {
        n += 2;
    }
    if code.contains("var ") {
        n += 2;
    }
    if !code.contains("mut ") && (code.contains("const ") || code.contains("let ")) {
        n += 2;
    }
    if code.contains("export ") {
        n += 1;
    }
    n
}

fn score_ts(code: &str) -> u32 {
    let mut n = 0;
    if code.contains(": string") || code.contains(": number") || code.contains(": boolean") {
        n += 3;
    }
    if code.contains("interface ") && !code.contains("public interface") {
        n += 3;
    }
    n
}

fn score_go(code: &str) -> u32 {
    let mut n = 0;
    if code.contains("func ") {
        n += 2;
    }
    if code.contains("package ") || code.contains("fmt.") {
        n += 2;
    }
    if code.contains(":=") {
        n += 1;
    }
    n
}

fn score_cpp(code: &str) -> u32 {
    let mut n = 0;
    if code.contains("std::")
        || code.contains("cout")
        || code.contains("template<")
        || code.contains("namespace ")
    {
        n += 3;
    }
    if code.contains("#include") {
        n += 1;
    }
    n
}

fn score_c(code: &str) -> u32 {
    let mut n = 0;
    if code.contains("#include") {
        n += 2;
    }
    if code.contains("printf(") || code.contains("int main") {
        n += 2;
    }
    n
}

fn score_java(code: &str) -> u32 {
    let mut n = 0;
    if code.contains("public class")
        || code.contains("public static")
        || code.contains("System.out")
        || code.contains("public interface")
    {
        n += 3;
    }
    n
}

fn score_sql(code: &str) -> u32 {
    let lower = code.to_ascii_lowercase();
    let mut n = 0;
    if has_word(&lower, "select") {
        n += 2;
    }
    if has_word(&lower, "from") {
        n += 2;
    }
    if has_word(&lower, "where") || has_word(&lower, "insert") || has_word(&lower, "update") {
        n += 1;
    }
    n
}

fn score_json(code: &str) -> u32 {
    let trimmed = code.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && (code.contains("\":") || code.contains("\": "))
    {
        3
    } else {
        0
    }
}

fn score_html(code: &str) -> u32 {
    let mut n = 0;
    if code.contains("</") {
        n += 2;
    }
    if code.contains("<html")
        || code.contains("<div")
        || code.contains("<span")
        || code.contains("<ul")
        || code.to_ascii_lowercase().contains("<!doctype")
    {
        n += 2;
    }
    n
}

fn score_css(code: &str) -> u32 {
    let props = code
        .lines()
        .filter(|line| {
            let line = line.trim();
            line.ends_with(';') && line.contains(':')
        })
        .count();
    if props >= 2 || (props >= 1 && (code.contains("px") || code.contains("color"))) {
        2
    } else {
        0
    }
}

fn score_bash(code: &str) -> u32 {
    let mut n = 0;
    if code.contains("fi\n") || code.contains("esac") {
        n += 2;
    }
    if code.contains("echo ") && code.contains("then") {
        n += 2;
    }
    n
}

fn score_ruby(code: &str) -> u32 {
    let mut n = 0;
    if code.contains("do |") || code.contains("puts ") {
        n += 2;
    }
    if code.contains("def ") && code.contains("\nend") {
        n += 3;
    }
    n
}

fn score_swift(code: &str) -> u32 {
    if code.contains("func ") && code.contains("->") {
        3
    } else if code.contains("guard ") || code.contains(" nil") {
        2
    } else {
        0
    }
}

fn score_php(code: &str) -> u32 {
    if code.contains("<?php") { 3 } else { 0 }
}

fn has_word(code: &str, word: &str) -> bool {
    code.match_indices(word).any(|(at, _)| {
        let before = code[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        let after = code[at + word.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        before && after
    })
}

enum Slot {
    Empty,
    Ready(Highlighter),
    Failed(String),
}

struct Highlighter {
    store: Store<()>,
    memory: wasmtime::Memory,
    highlight: TypedFunc<(), ()>,
}

static ENGINE: LazyLock<Mutex<Slot>> = LazyLock::new(|| Mutex::new(Slot::Empty));

fn with_engine<T>(f: impl FnOnce(&mut Highlighter) -> Result<T, String>) -> Result<T, String> {
    let mut guard = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(*guard, Slot::Empty) {
        *guard = match Highlighter::load() {
            Ok(engine) => Slot::Ready(engine),
            Err(e) => Slot::Failed(e),
        };
    }
    match &mut *guard {
        Slot::Ready(engine) => f(engine),
        Slot::Failed(e) => Err(e.clone()),
        Slot::Empty => Err("syntax highlighter was not started".to_owned()),
    }
}

/// Compiles the lexers ahead of the first code block. Compiling takes about
/// a tenth of a second, so it runs beside document loading instead of
/// stalling the first frame. A failure is kept and comes back as plain code.
pub fn warm() -> Result<(), String> {
    with_engine(|_| Ok(()))
}

impl Highlighter {
    fn load() -> Result<Self, String> {
        let mut config = Config::new();
        config.wasm_simd(true);
        config.wasm_bulk_memory(true);
        config.cranelift_opt_level(OptLevel::Speed);
        let engine = WasmEngine::new(&config).map_err(|e| failed("start", &e))?;
        let module = Module::new(&engine, WASM).map_err(|e| failed("compile", &e))?;
        let mut linker = Linker::new(&engine);
        linker
            .func_wrap(
                "env",
                "is_id_start",
                |caller: Caller<'_, ()>, ptr: i32, len: i32| id_class(caller, ptr, len, true),
            )
            .map_err(|e| failed("prepare", &e))?;
        linker
            .func_wrap(
                "env",
                "is_id_continue",
                |caller: Caller<'_, ()>, ptr: i32, len: i32| id_class(caller, ptr, len, false),
            )
            .map_err(|e| failed("prepare", &e))?;
        let mut store = Store::new(&engine, ());
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| failed("start", &e))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| failed("start", &"the module has no memory"))?;
        let highlight = instance
            .get_typed_func::<(), ()>(&mut store, "highlight")
            .map_err(|e| failed("start", &e))?;
        Ok(Highlighter {
            store,
            memory,
            highlight,
        })
    }

    /// Mode 3: `(end_utf16, token_id)` pairs. The terminator id marks a newline.
    fn tokenize(&mut self, lang: u8, bytes: &[u8]) -> Result<Vec<Kind>, String> {
        let needed = INPUT + bytes.len() + SLACK;
        let have = self.memory.data_size(&self.store);
        if needed > have {
            let pages = (needed - have).div_ceil(INPUT);
            self.memory
                .grow(&mut self.store, pages as u64)
                .map_err(|e| failed("grow", &e))?;
        }
        self.memory
            .write(&mut self.store, INPUT, bytes)
            .map_err(|e| failed("write", &e))?;
        self.memory
            .write(&mut self.store, INPUT + bytes.len(), &[0u8; SLACK])
            .map_err(|e| failed("write", &e))?;
        let mut head = [0u8; 22];
        head[0] = lang;
        head[1] = 3;
        head[2..6].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        self.memory
            .write(&mut self.store, 0, &head)
            .map_err(|e| failed("write", &e))?;
        self.highlight
            .call(&mut self.store, ())
            .map_err(|e| failed("lex", &e))?;

        let mut meta = [0u8; 8];
        self.memory
            .read(&mut self.store, 6, &mut meta)
            .map_err(|e| failed("read", &e))?;
        let start = u32::from_le_bytes([meta[0], meta[1], meta[2], meta[3]]) as usize;
        let len = u32::from_le_bytes([meta[4], meta[5], meta[6], meta[7]]) as usize;
        if !len.is_multiple_of(8) || start.saturating_add(len) > self.memory.data_size(&self.store)
        {
            return Err(failed("read", &"token records were the wrong size"));
        }
        let mut raw = vec![0u8; len];
        self.memory
            .read(&mut self.store, start, &mut raw)
            .map_err(|e| failed("read", &e))?;
        let mut records = Vec::with_capacity(len / 8);
        for chunk in raw.as_chunks::<8>().0 {
            let [e0, e1, e2, e3, i0, i1, i2, i3] = *chunk;
            let end = u32::from_le_bytes([e0, e1, e2, e3]);
            let id = u32::from_le_bytes([i0, i1, i2, i3]);
            records.push((end, id));
        }
        Ok(paint(bytes, &records))
    }
}

fn failed(what: &str, cause: &dyn std::fmt::Display) -> String {
    format!("Could not {what} the syntax highlighter ({cause}). Code stays uncolored.")
}

fn id_class(mut caller: Caller<'_, ()>, ptr: i32, len: i32, start: bool) -> i32 {
    if ptr < 0 || !(1..=4).contains(&len) {
        return 0;
    }
    let Some(memory) = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
    else {
        return 0;
    };
    let mut buf = [0u8; 4];
    let len = len as usize;
    if memory.read(&caller, ptr as usize, &mut buf[..len]).is_err() {
        return 0;
    }
    let Ok(text) = std::str::from_utf8(&buf[..len]) else {
        return 0;
    };
    let Some(ch) = text.chars().next() else {
        return 0;
    };
    let yes = if start {
        unicode_ident::is_xid_start(ch)
    } else {
        matches!(ch, '\u{200c}' | '\u{200d}') || unicode_ident::is_xid_continue(ch)
    };
    i32::from(yes)
}

/// Records are UTF-16 ends. Characters take one unit, and a non-BMP character takes two.
fn paint(bytes: &[u8], records: &[(u32, u32)]) -> Vec<Kind> {
    let Ok(code) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let mut kinds = Vec::with_capacity(code.chars().count());
    let mut at = 0u32;
    let mut index = 0usize;
    for ch in code.chars() {
        while index < records.len() && records[index].0 <= at {
            index += 1;
        }
        let id = records.get(index).map(|(_, id)| *id).unwrap_or(u32::MAX);
        kinds.push(if id == u32::MAX {
            Kind::Plain
        } else {
            kind_of(id)
        });
        at += ch.len_utf16() as u32;
    }
    kinds
}

fn kind_of(id: u32) -> Kind {
    let name = TOKEN_NAMES.get(id as usize).copied().unwrap_or("");
    if name.is_empty()
        || matches!(
            name,
            "none"
                | "primary"
                | "embedded"
                | "predictive"
                | "text.jsx"
                | "text.literal"
                | "background"
                | "foreground"
                | "variable"
        )
    {
        Kind::Plain
    } else if name.starts_with("comment") {
        Kind::Comment
    } else if name == "string.escape" {
        Kind::Number
    } else if name.starts_with("string") {
        Kind::String
    } else if name.starts_with("keyword") || matches!(name, "preproc" | "label") {
        Kind::Keyword
    } else if name == "number"
        || name == "boolean"
        || name.starts_with("constant")
        || matches!(name, "enum" | "variant")
    {
        Kind::Constant
    } else if name.starts_with("function") || matches!(name, "constructor" | "link_uri") {
        Kind::Function
    } else if name.starts_with("type") || matches!(name, "namespace" | "title") {
        Kind::Type
    } else if name.starts_with("tag") || name.starts_with("selector") {
        Kind::Tag
    } else if name.starts_with("attribute") || name.starts_with("property") {
        Kind::Attribute
    } else if name.starts_with("punctuation") || name == "operator" {
        Kind::Punct
    } else if name == "diff.plus" {
        Kind::Added
    } else if name == "diff.minus" {
        Kind::Removed
    } else {
        Kind::Special
    }
}

const TOKEN_NAMES: &[&str] = &[
    "none",
    "attribute",
    "attribute.jsx",
    "boolean",
    "comment",
    "comment.doc",
    "constant",
    "constant.builtin",
    "constructor",
    "diff.minus",
    "diff.plus",
    "embedded",
    "emphasis",
    "emphasis.strong",
    "function",
    "function.definition",
    "function.method",
    "keyword",
    "keyword.control",
    "keyword.declaration",
    "keyword.import",
    "keyword.operator",
    "label",
    "namespace",
    "number",
    "operator",
    "predictive",
    "preproc",
    "primary",
    "property",
    "property.json_key",
    "punctuation",
    "punctuation.bracket",
    "punctuation.bracket.html",
    "punctuation.bracket.jsx",
    "punctuation.delimiter",
    "punctuation.delimiter.html",
    "punctuation.delimiter.jsx",
    "punctuation.list_marker",
    "punctuation.markup",
    "punctuation.special",
    "selector",
    "selector.class",
    "selector.id",
    "string",
    "string.escape",
    "string.regex",
    "string.special",
    "string.special.symbol",
    "tag",
    "tag.doctype",
    "tag.jsx",
    "text.jsx",
    "text.literal",
    "title",
    "type",
    "type.builtin",
    "type.class",
    "variable",
    "variable.parameter",
    "variable.special",
    "enum",
    "hint",
    "keyword.jsdoc",
    "link_text",
    "link_uri",
    "selector.pseudo",
    "tag.component.jsx",
    "type.jsdoc",
    "variable.jsdoc",
    "variant",
    "background",
    "foreground",
];

const CANON: [&str; 81] = [
    "plain",
    "angular-html",
    "asm",
    "astro",
    "bash",
    "batch",
    "c",
    "c3",
    "clojure",
    "cmake",
    "cpp",
    "csharp",
    "css",
    "cuda",
    "dart",
    "diff",
    "dockerfile",
    "elixir",
    "elm",
    "erlang",
    "fortran",
    "fortran-fixed-form",
    "fsharp",
    "gdresource",
    "gdscript",
    "gdshader",
    "gleam",
    "glsl",
    "go",
    "graphql",
    "groovy",
    "haskell",
    "hlsl",
    "html",
    "java",
    "js",
    "json",
    "json5",
    "jsonl",
    "jsx",
    "julia",
    "kotlin",
    "latex",
    "less",
    "lisp",
    "lua",
    "makefile",
    "markdown",
    "matlab",
    "mdx",
    "nim",
    "nix",
    "objc",
    "ocaml",
    "pascal",
    "perl",
    "php",
    "powershell",
    "proto",
    "python",
    "r",
    "ruby",
    "rust",
    "sass",
    "scala",
    "scss",
    "solidity",
    "sql",
    "svelte",
    "swift",
    "terraform",
    "toml",
    "ts",
    "tsrx",
    "tsx",
    "vue",
    "wat",
    "wgsl",
    "xml",
    "yaml",
    "zig",
];

/// Fence names, sorted. The id is the one compiled into highlights.wasm 0.2.0.
const ALIASES: &[(&str, u8)] = &[
    ("angular-html", 1),
    ("angular-ts", 72),
    ("asm", 2),
    ("assembly", 2),
    ("astro", 3),
    ("bash", 4),
    ("bat", 5),
    ("batch", 5),
    ("batchfile", 5),
    ("c", 6),
    ("c#", 11),
    ("c++", 10),
    ("c3", 7),
    ("cc", 10),
    ("cjs", 35),
    ("cl", 44),
    ("clj", 8),
    ("cljc", 8),
    ("cljs", 8),
    ("clojure", 8),
    ("cmake", 9),
    ("cmd", 5),
    ("comp", 27),
    ("containerfile", 16),
    ("cpp", 10),
    ("cs", 11),
    ("csharp", 11),
    ("css", 12),
    ("cts", 72),
    ("cu", 13),
    ("cuda", 13),
    ("cuh", 13),
    ("cxx", 10),
    ("dart", 14),
    ("delphi", 54),
    ("diff", 15),
    ("docker", 16),
    ("dockerfile", 16),
    ("dos", 5),
    ("dpk", 54),
    ("dpr", 54),
    ("edn", 8),
    ("el", 44),
    ("elisp", 44),
    ("elixir", 17),
    ("elm", 18),
    ("emacs-lisp", 44),
    ("erl", 19),
    ("erlang", 19),
    ("ex", 17),
    ("exs", 17),
    ("f", 21),
    ("f#", 22),
    ("f03", 20),
    ("f08", 20),
    ("f77", 21),
    ("f90", 20),
    ("f95", 20),
    ("for", 21),
    ("fortran", 20),
    ("fortran-fixed-form", 21),
    ("fortran-free-form", 20),
    ("frag", 27),
    ("fs", 22),
    ("fsharp", 22),
    ("fsi", 22),
    ("fsx", 22),
    ("gd", 24),
    ("gdresource", 23),
    ("gdscript", 24),
    ("gdshader", 25),
    ("gdshaderinc", 25),
    ("geom", 27),
    ("git-commit", 15),
    ("git-rebase", 15),
    ("gleam", 26),
    ("glsl", 27),
    ("go", 28),
    ("godot-resource", 23),
    ("godot-shader", 25),
    ("golang", 28),
    ("gql", 29),
    ("gradle", 30),
    ("graphql", 29),
    ("groovy", 30),
    ("gsh", 30),
    ("gvy", 30),
    ("gy", 30),
    ("h", 6),
    ("haskell", 31),
    ("hcl", 70),
    ("hh", 10),
    ("hlsl", 32),
    ("hpp", 10),
    ("hrl", 19),
    ("hs", 31),
    ("htm", 33),
    ("html", 33),
    ("hxx", 10),
    ("java", 34),
    ("javascript", 35),
    ("jl", 40),
    ("js", 35),
    ("json", 36),
    ("json5", 37),
    ("jsonc", 36),
    ("jsonl", 38),
    ("jsx", 39),
    ("julia", 40),
    ("kotlin", 41),
    ("kt", 41),
    ("kts", 41),
    ("latex", 42),
    ("less", 43),
    ("lisp", 44),
    ("lpr", 54),
    ("lsp", 44),
    ("lua", 45),
    ("m", 52),
    ("make", 46),
    ("makefile", 46),
    ("markdown", 47),
    ("matlab", 48),
    ("md", 47),
    ("mdx", 49),
    ("mjs", 35),
    ("mk", 46),
    ("ml", 53),
    ("mli", 53),
    ("mm", 52),
    ("mts", 72),
    ("nim", 50),
    ("nix", 51),
    ("objc", 52),
    ("objcpp", 52),
    ("object-pascal", 54),
    ("objective-c", 52),
    ("objective-cpp", 52),
    ("objectivec", 52),
    ("objectpascal", 54),
    ("ocaml", 53),
    ("octave", 48),
    ("pas", 54),
    ("pascal", 54),
    ("patch", 15),
    ("perl", 55),
    ("php", 56),
    ("pl", 55),
    ("plain", 0),
    ("plaintext", 0),
    ("pm", 55),
    ("powershell", 57),
    ("pp", 54),
    ("proto", 58),
    ("protobuf", 58),
    ("ps", 57),
    ("ps1", 57),
    ("psd1", 57),
    ("psm1", 57),
    ("pwsh", 57),
    ("py", 59),
    ("python", 59),
    ("r", 60),
    ("rb", 61),
    ("rs", 62),
    ("rscript", 60),
    ("ruby", 61),
    ("rust", 62),
    ("s", 2),
    ("sass", 63),
    ("sbt", 64),
    ("sc", 64),
    ("scala", 64),
    ("scheme", 44),
    ("scm", 44),
    ("scss", 65),
    ("sh", 4),
    ("shell", 4),
    ("shellscript", 4),
    ("shellsession", 4),
    ("sol", 66),
    ("solidity", 66),
    ("sql", 67),
    ("svelte", 68),
    ("svg", 78),
    ("swift", 69),
    ("terraform", 70),
    ("tex", 42),
    ("text", 0),
    ("tf", 70),
    ("tfvars", 70),
    ("toml", 71),
    ("tres", 23),
    ("ts", 72),
    ("tscn", 23),
    ("tsrx", 73),
    ("tsx", 74),
    ("txt", 0),
    ("typescript", 72),
    ("vert", 27),
    ("vue", 75),
    ("wasm", 76),
    ("wat", 76),
    ("wgsl", 77),
    ("xml", 78),
    ("xsd", 78),
    ("yaml", 79),
    ("yml", 79),
    ("zig", 80),
    ("zon", 80),
    ("zsh", 4),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn name(hint: Option<&str>, code: &str) -> Option<&'static str> {
        resolve(hint, code).map(|(_, name)| name)
    }

    #[test]
    fn aliases_stay_sorted_and_map_fence_names() {
        assert!(ALIASES.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(name(Some("rs"), ""), Some("rust"));
        assert_eq!(name(Some("C++"), ""), Some("cpp"));
        assert_eq!(name(Some("mermaid"), "fn main() {}"), None);
        assert_eq!(name(Some("text"), "fn main() {}"), None);
    }

    #[test]
    fn unlabeled_code_is_guessed_only_when_it_is_clear() {
        assert_eq!(name(None, "a = 1\n  b"), None);
        assert_eq!(name(None, "fn main() {\n    let x = 1;\n}"), Some("rust"));
        assert_eq!(
            name(
                None,
                "for (var i = 0; i < n; i++) {\n    if (x) {\n        go()\n    }\n}"
            ),
            Some("js")
        );
        assert_eq!(
            name(None, "def greet(name):\n    return name\n"),
            Some("python")
        );
        assert_eq!(name(None, "SELECT id FROM users WHERE id = 1"), Some("sql"));
        assert_eq!(name(None, "<ul>\n  <li>\n</ul>"), Some("html"));
        assert_eq!(
            name(
                None,
                "if (user && user.name && !user.first_name) {\n    // Documents written before Dec 8\n    user.first_name = user.name.split(\" \")[0];\n}"
            ),
            Some("js")
        );
        assert_eq!(name(None, "Total: {\n  one\n}"), None);
        assert_eq!(
            name(None, "#!/usr/bin/env python\nprint(1)\n"),
            Some("python")
        );
    }

    #[test]
    fn a_rust_function_and_a_string_are_classified() {
        warm().unwrap();
        let code = "fn main() {\n    let name = \"leaf\";\n}\n";
        let painted = kinds(code, Some("rust"));
        assert_eq!(painted.len(), code.chars().count(), "{painted:?}");
        assert_eq!(painted[0], Kind::Keyword, "{painted:?}");
        let quote = code.chars().position(|c| c == '"').unwrap();
        assert_eq!(painted[quote], Kind::String, "{painted:?}");
        let comment = kinds("// note\nfn x() {}\n", Some("rust"));
        assert_eq!(comment[0], Kind::Comment, "{comment:?}");
        let cafe = "let café = 1\n";
        assert_eq!(kinds(cafe, Some("js")).len(), cafe.chars().count());
    }
}
