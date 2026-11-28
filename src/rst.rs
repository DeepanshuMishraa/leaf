//! reStructuredText (`.rst`), as Python and Sphinx projects write their
//! docs: converted to markdown, then read like any markdown file (see
//! `markdown::from_converted`), so images beside it, code colors and search
//! all work. The conversion covers what docs use most: section titles, lists,
//! literal and code blocks, admonitions, images, tables, field lists and the
//! common inline markup. Directives it doesn't know are left out.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::doc::Doc;

const EXTENSIONS: [&str; 2] = ["rst", "rest"];

#[derive(Debug, thiserror::Error)]
pub enum RstFailed {
    #[error(
        "Could not read {path} because the operating system denied access ({cause}). Allow your terminal to read it in your system's privacy settings, then restart it."
    )]
    Blocked { path: String, cause: std::io::Error },
    #[error("Could not read {path} ({cause}). Check that the path is right and the file exists.")]
    Read { path: String, cause: std::io::Error },
}

pub fn is_rst(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

pub fn open(path: &Path) -> Result<Doc, RstFailed> {
    let shown = || path.display().to_string();
    let text = std::fs::read(path).map_err(|cause| {
        if cause.kind() == std::io::ErrorKind::PermissionDenied {
            RstFailed::Blocked {
                path: shown(),
                cause,
            }
        } else {
            RstFailed::Read {
                path: shown(),
                cause,
            }
        }
    })?;
    let title = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let markdown = to_markdown(&String::from_utf8_lossy(&text));
    Ok(crate::markdown::from_converted(title, &markdown, path))
}

static TARGET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\.\. _([^:`]+|`[^`]+`):(?:\s+(.*))?$").expect("valid regex"));
static DIRECTIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\.\. ([\w:-]+)::(?:\s+(.*))?$").expect("valid regex"));
static FOOTNOTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\.\. \[([^\]]+)\](?:\s+(.*))?$").expect("valid regex"));
static BULLET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([-*+•])( +|$)").expect("valid regex"));
static ENUMERATED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\(?(\d+|#)[.)]( +|$)").expect("valid regex"));
static FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^:([^:\s][^:]*):(?:\s+(.*))?$").expect("valid regex"));
static OPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^:([\w-]+):\s*(.*)$").expect("valid regex"));
static INTERPRETED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?::([\w:.+-]+):)?`([^`]+)`(__?)?").expect("valid regex"));
/// The character after a match is checked in `references`, not matched here,
/// so it stays free to open the next reference.
static BARE_REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[\s(])([A-Za-z][\w-]*)_").expect("valid regex"));
static FOOTNOTE_REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([#*\w]+)\]_").expect("valid regex"));
static LABELED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?s)(.*?)\s*<([^<>]+)>$").expect("valid regex"));

/// The markdown for a reStructuredText document.
pub fn to_markdown(text: &str) -> String {
    let lines: Vec<String> = text
        .replace("\r\n", "\n")
        .split('\n')
        .map(|l| expand_tabs(l).trim_end().to_owned())
        .collect();
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    // A first pass collects the link targets, so references above their
    // target resolve. Collecting while converting skips literal blocks,
    // code and comments, where a target is only an example.
    let mut state = State {
        targets: HashMap::new(),
        levels: Vec::new(),
    };
    convert(&lines, &mut state);
    let mut state = State {
        targets: state.targets,
        levels: Vec::new(),
    };
    let mut out = convert(&lines, &mut state).join("\n");
    out.push('\n');
    out
}

struct State {
    /// Link targets (`.. _name: url`) by lowercased name.
    targets: HashMap<String, String>,
    /// Title adornments in the order they first appear, which sets each
    /// one's heading level: the character and whether it has an overline.
    levels: Vec<(char, bool)>,
}

impl State {
    fn level(&mut self, style: (char, bool)) -> usize {
        match self.levels.iter().position(|&s| s == style) {
            Some(i) => i + 1,
            None => {
                self.levels.push(style);
                self.levels.len()
            }
        }
    }
}

/// `line` with each tab widened to the next eight-column stop.
fn expand_tabs(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut column = 0;
    for c in line.chars() {
        if c == '\t' {
            let next = (column / 8 + 1) * 8;
            out.extend(std::iter::repeat_n(' ', next - column));
            column = next;
        } else {
            out.push(c);
            column += 1;
        }
    }
    out
}

/// Leading spaces. Tabs are expanded by then, and other whitespace, such
/// as a no-break space, is text.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// A title adornment: one punctuation character repeated, such as `=====`.
fn adornment(line: &str) -> Option<char> {
    let first = line.chars().next()?;
    (line.len() >= 2 && first.is_ascii_punctuation() && line.chars().all(|c| c == first))
        .then_some(first)
}

/// `lines` with their shared indent removed.
fn dedent<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    let least = lines
        .iter()
        .filter(|l| !blank(l))
        .map(|l| indent(l))
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| if blank(l) { "" } else { &l[least..] })
        .collect()
}

/// The end of the block starting at `start` whose lines are blank or
/// indented at least `column`, without trailing blank lines.
fn block_end(lines: &[&str], start: usize, column: usize) -> usize {
    let mut end = start;
    let mut last = start;
    while end < lines.len() && (blank(lines[end]) || indent(lines[end]) >= column) {
        end += 1;
        if !blank(lines[end - 1]) {
            last = end;
        }
    }
    last
}

fn fenced(lang: &str, body: &[&str]) -> Vec<String> {
    let code = dedent(body).join("\n");
    let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    vec![format!("{fence}{lang}"), code, fence]
}

/// `lines` each prefixed with `first`, or with `rest` after the first. An
/// entry holding several lines (a paragraph, a code block) gets a prefix on each.
fn hang(lines: Vec<String>, first: &str, rest: &str) -> Vec<String> {
    lines
        .iter()
        .flat_map(|l| l.split('\n'))
        .enumerate()
        .map(|(i, l)| {
            let prefix = if i == 0 { first } else { rest };
            if l.is_empty() {
                prefix.trim_end().to_owned()
            } else {
                format!("{prefix}{l}")
            }
        })
        .collect()
}

fn quoted(lines: Vec<String>) -> Vec<String> {
    hang(lines, "> ", "> ")
}

fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

const ADMONITIONS: [&str; 12] = [
    "note",
    "warning",
    "tip",
    "hint",
    "important",
    "caution",
    "danger",
    "attention",
    "error",
    "seealso",
    "versionadded",
    "deprecated",
];

/// Directives whose first lines are an argument (a path, a language, a
/// title), not content.
const TAKES_ARGUMENT: [&str; 9] = [
    "image",
    "figure",
    "code-block",
    "code",
    "sourcecode",
    "rubric",
    "topic",
    "sidebar",
    "admonition",
];

/// The GitHub alert an admonition maps to, for those that have one.
fn alert(name: &str) -> Option<&'static str> {
    match name {
        "note" => Some("NOTE"),
        "tip" | "hint" => Some("TIP"),
        "important" => Some("IMPORTANT"),
        "warning" | "attention" => Some("WARNING"),
        "caution" | "danger" | "error" => Some("CAUTION"),
        _ => None,
    }
}

/// The markdown for a block of reStructuredText whose base indent is 0.
fn convert(lines: &[&str], state: &mut State) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if blank(line) {
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
            i += 1;
            continue;
        }
        let gap = |out: &mut Vec<String>| {
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
        };

        // An indented block: a definition after its term, or a block quote.
        if indent(line) > 0 {
            let end = block_end(lines, i, 1);
            let body = convert(&dedent(&lines[i..end]), state);
            let after_term = i > 0 && !blank(lines[i - 1]);
            match out.last_mut() {
                Some(term) if after_term && !term.is_empty() => {
                    *term = format!("**{}**", term.trim());
                    out.push(String::new());
                    out.extend(body);
                }
                _ => {
                    gap(&mut out);
                    out.extend(quoted(body));
                }
            }
            i = end;
            continue;
        }

        // Explicit markup: directives, comments, targets, footnotes.
        if line.starts_with("..") && (line == ".." || line.starts_with(".. ")) {
            let end = block_end(lines, i + 1, 1);
            let body = dedent(&lines[i + 1..end]);
            if let Some(c) = TARGET.captures(line) {
                // The URL may sit on the target's line or in the block under
                // it, split over lines that join without spaces.
                let mut url = c.get(2).map_or("", |m| m.as_str()).trim().to_owned();
                url.extend(body.iter().map(|l| l.trim()));
                if !url.is_empty() {
                    let name = c[1].trim_matches('`').to_lowercase();
                    state.targets.insert(name, url);
                }
            } else if let Some(c) = FOOTNOTE.captures(line) {
                gap(&mut out);
                let first = c.get(2).map_or("", |m| m.as_str());
                let mut text = vec![inline(format!("[{}] {first}", &c[1]).trim_end(), state)];
                text.extend(
                    body.iter()
                        .filter(|l| !l.is_empty())
                        .map(|l| inline(l, state)),
                );
                out.push(text.join(" "));
            } else if let Some(c) = DIRECTIVE.captures(line) {
                let name = c[1].to_ascii_lowercase();
                let mut args = c.get(2).map_or("", |m| m.as_str()).trim().to_owned();
                // An argument may start on the next line instead, running
                // until a blank line or the first option.
                let mut body = &body[..];
                if args.is_empty() && TAKES_ARGUMENT.contains(&name.as_str()) {
                    let n = body
                        .iter()
                        .take_while(|l| !l.is_empty() && !OPTION.is_match(l))
                        .count();
                    args = body[..n].join(" ");
                    body = &body[n..];
                }
                let args = args.as_str();
                let options = body.iter().take_while(|l| OPTION.is_match(l)).count();
                let option = |key: &str| {
                    body[..options]
                        .iter()
                        .filter_map(|l| OPTION.captures(l))
                        .find(|c| &c[1] == key)
                        .map(|c| c[2].to_owned())
                };
                let content = &body[options..];
                let content_start = content
                    .iter()
                    .position(|l| !l.is_empty())
                    .unwrap_or(content.len());
                let content = &content[content_start..];
                gap(&mut out);
                match name.as_str() {
                    "code-block" | "code" | "sourcecode" => {
                        out.extend(fenced(args, content));
                    }
                    "math" => {
                        let mut math = vec![args];
                        math.extend_from_slice(content);
                        out.extend(fenced("latex", &math));
                    }
                    "image" | "figure" => {
                        let alt = option("alt").unwrap_or_default();
                        out.push(format!("![{alt}](<{args}>)"));
                        if !content.is_empty() {
                            out.push(String::new());
                            out.extend(convert(content, state));
                        }
                    }
                    "rubric" => out.push(format!("**{}**", inline(args, state))),
                    "topic" | "sidebar" | "admonition" => {
                        let mut quote = vec![format!("**{}**", inline(args, state))];
                        if !content.is_empty() {
                            quote.push(String::new());
                            quote.extend(convert(content, state));
                        }
                        out.extend(quoted(quote));
                    }
                    name if alert(name).is_some() => {
                        // GitHub's alert syntax, which the reader draws
                        // with its own label and color.
                        let mut quote = vec![format!("[!{}]", alert(name).unwrap_or("NOTE"))];
                        if !args.is_empty() {
                            quote.push(inline(args, state));
                        }
                        if !content.is_empty() {
                            if !args.is_empty() {
                                quote.push(String::new());
                            }
                            quote.extend(convert(content, state));
                        }
                        out.extend(quoted(quote));
                    }
                    name if ADMONITIONS.contains(&name) => {
                        let label = match name {
                            "seealso" => "See also".to_owned(),
                            "versionadded" => "New in version".to_owned(),
                            other => title_case(other),
                        };
                        let head = if args.is_empty() {
                            format!("**{label}**")
                        } else {
                            format!("**{label}** {}", inline(args, state))
                        };
                        let mut quote = vec![head];
                        if !content.is_empty() {
                            quote.push(String::new());
                            quote.extend(convert(content, state));
                        }
                        out.extend(quoted(quote));
                    }
                    _ => {}
                }
            }
            i = end.max(i + 1);
            continue;
        }

        // A title with an overline and an underline.
        if let Some(c) = adornment(line)
            && i + 2 < lines.len()
            && !blank(lines[i + 1])
            && lines[i + 2] == line
        {
            let level = state.level((c, true));
            gap(&mut out);
            out.push(format!(
                "{} {}",
                "#".repeat(level.min(6)),
                inline(lines[i + 1].trim(), state)
            ));
            out.push(String::new());
            i += 3;
            continue;
        }

        // A title with an underline.
        if adornment(line).is_none()
            && let Some(c) = lines.get(i + 1).and_then(|l| adornment(l))
            && lines[i + 1].chars().count() >= line.trim().chars().count().min(4)
        {
            let level = state.level((c, false));
            gap(&mut out);
            out.push(format!(
                "{} {}",
                "#".repeat(level.min(6)),
                inline(line.trim(), state)
            ));
            out.push(String::new());
            i += 2;
            continue;
        }

        // A transition between sections.
        if adornment(line).is_some() && line.len() >= 4 && lines.get(i + 1).is_none_or(|l| blank(l))
        {
            gap(&mut out);
            out.push("---".to_owned());
            out.push(String::new());
            i += 1;
            continue;
        }

        // Tables.
        if line.starts_with('+') && line.ends_with('+') && line.contains("-+") {
            let end = (i..lines.len())
                .find(|&j| !(lines[j].starts_with('+') || lines[j].starts_with('|')))
                .unwrap_or(lines.len());
            gap(&mut out);
            out.extend(grid_table(&lines[i..end], state));
            i = end;
            continue;
        }
        if is_simple_border(line) {
            // Rows may have blank lines between them, so the table ends at
            // the border followed by a blank line, not at the first blank.
            let end = (i + 1..lines.len())
                .find(|&j| is_simple_border(lines[j]) && lines.get(j + 1).is_none_or(|l| blank(l)))
                .map(|j| j + 1)
                .or_else(|| (i..lines.len()).find(|&j| blank(lines[j])))
                .unwrap_or(lines.len());
            gap(&mut out);
            out.extend(simple_table(&lines[i..end], state));
            i = end;
            continue;
        }

        // Doctest blocks.
        if line.starts_with(">>> ") {
            let end = (i..lines.len())
                .find(|&j| blank(lines[j]))
                .unwrap_or(lines.len());
            gap(&mut out);
            out.extend(fenced("python", &lines[i..end]));
            i = end;
            continue;
        }

        // List items and field lists, each with a body that hangs under it.
        let item = BULLET
            .find(line)
            .map(|m| (m.end(), "- ".to_owned()))
            .or_else(|| {
                ENUMERATED.captures(line).map(|c| {
                    let number = match &c[1] {
                        "#" => "1".to_owned(),
                        n => n.to_owned(),
                    };
                    (c[0].len(), format!("{number}. "))
                })
            });
        if let Some((offset, marker)) = item {
            // `offset` slices the item's line; `column` is where its text
            // starts on screen, which a bullet like `•` makes differ.
            let column = line[..offset].chars().count();
            let column = if offset == line.len() {
                column.max(2)
            } else {
                column
            };
            let end = block_end(lines, i + 1, column);
            let mut body = vec![line.get(offset..).unwrap_or("")];
            body.extend(
                lines[i + 1..end]
                    .iter()
                    .map(|l| l.get(column..).unwrap_or("")),
            );
            let pad = " ".repeat(marker.len());
            out.extend(hang(convert(&body, state), &marker, &pad));
            i = end;
            continue;
        }
        if let Some(c) = FIELD.captures(line) {
            let end = block_end(lines, i + 1, 1);
            let mut text = vec![c.get(2).map_or("", |m| m.as_str()).to_owned()];
            text.extend(dedent(&lines[i + 1..end]).iter().map(|l| (*l).to_owned()));
            let value = inline(text.join(" ").trim(), state);
            out.push(format!("- **{}:** {value}", inline(&c[1], state)));
            i = end;
            continue;
        }

        // A paragraph, which may introduce a literal block with `::`.
        let mut end = i + 1;
        while end < lines.len()
            && !blank(lines[end])
            && indent(lines[end]) == 0
            && !lines
                .get(end + 1)
                .is_some_and(|l| adornment(l).is_some() && adornment(lines[end]).is_none())
        {
            end += 1;
        }
        let mut text = lines[i..end].join("\n");
        let literal = text.ends_with("::");
        if literal {
            text = match text.strip_suffix("::") {
                Some(rest) if rest.trim().is_empty() => String::new(),
                Some(rest) if rest.ends_with(char::is_whitespace) => rest.trim_end().to_owned(),
                Some(rest) => format!("{rest}:"),
                None => text,
            };
        }
        if !text.is_empty() {
            gap(&mut out);
            out.push(inline(&text, state));
        }
        i = end;
        if literal {
            let start = (i..lines.len()).find(|&j| !blank(lines[j]));
            if let Some(start) = start.filter(|&s| indent(lines[s]) > 0) {
                let end = block_end(lines, start, 1);
                gap(&mut out);
                out.extend(fenced("", &lines[start..end]));
                i = end;
            }
        }
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

fn is_simple_border(line: &str) -> bool {
    line.starts_with('=')
        && line.split_whitespace().count() >= 2
        && line.split_whitespace().all(|w| w.chars().all(|c| c == '='))
}

/// A markdown table, from rows of cell text with the header first.
fn table(rows: Vec<Vec<String>>, state: &State) -> Vec<String> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = Vec::new();
    for (n, row) in rows.iter().enumerate() {
        let cells: Vec<String> = (0..columns)
            .map(|c| {
                let cell = row.get(c).map_or("", String::as_str);
                inline(cell, state).replace('|', "\\|")
            })
            .collect();
        out.push(format!("| {} |", cells.join(" | ")));
        if n == 0 {
            out.push(format!("|{}|", vec!["---"; columns].join("|")));
        }
    }
    out
}

/// A grid table (`+---+---+`). One whose cells span columns or rows, which
/// markdown can't show, comes out as it was written.
fn grid_table(lines: &[&str], state: &State) -> Vec<String> {
    let border = lines[0];
    let edges: Vec<usize> = border.match_indices('+').map(|(i, _)| i).collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<Vec<String>> = Vec::new();
    let mut header = None;
    for line in &lines[1..] {
        if line.starts_with('+') {
            if line
                .match_indices('+')
                .map(|(i, _)| i)
                .ne(edges.iter().copied())
            {
                return fenced("", lines);
            }
            if !current.is_empty() {
                rows.push(
                    current
                        .drain(..)
                        .map(|parts| parts.join(" ").trim().to_owned())
                        .collect(),
                );
            }
            if line.contains('=') {
                header = Some(rows.len());
            }
            continue;
        }
        let bars: Vec<usize> = line.match_indices('|').map(|(i, _)| i).collect();
        if !edges.iter().all(|e| bars.contains(e)) {
            return fenced("", lines);
        }
        current.resize(edges.len() - 1, Vec::new());
        for (c, pair) in edges.windows(2).enumerate() {
            let cell = line.get(pair[0] + 1..pair[1]).unwrap_or("").trim();
            if !cell.is_empty() {
                current[c].push(cell.to_owned());
            }
        }
    }
    if header.is_some_and(|h| h != 1) {
        return fenced("", lines);
    }
    table(rows, state)
}

/// A simple table (`=====  =====`), its columns set by the border.
fn simple_table(lines: &[&str], state: &State) -> Vec<String> {
    let border = lines[0];
    let mut columns: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    for (i, c) in border
        .char_indices()
        .chain(std::iter::once((border.len(), ' ')))
    {
        match (c == '=', start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                columns.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    // The border is ASCII, so its byte offsets are character columns. A row
    // may not be, so its columns are turned into byte offsets before slicing.
    let cell = |line: &str, c: usize| {
        let byte = |column: usize| {
            line.char_indices()
                .nth(column)
                .map_or(line.len(), |(b, _)| b)
        };
        let (from, to) = columns[c];
        let to = if c + 1 == columns.len() {
            line.len()
        } else {
            byte(to)
        };
        line[byte(from)..to].trim().to_owned()
    };
    let mut rows: Vec<Vec<String>> = Vec::new();
    // A line with an empty first column continues the row above it, but
    // never across a border.
    let mut continues = false;
    for line in &lines[1..] {
        if is_simple_border(line) {
            continues = false;
            continue;
        }
        if blank(line) || line.starts_with("---") {
            continue;
        }
        let cells: Vec<String> = (0..columns.len()).map(|c| cell(line, c)).collect();
        match rows.last_mut() {
            Some(row) if continues && cells[0].is_empty() => {
                for (cell, more) in row.iter_mut().zip(cells) {
                    if !more.is_empty() {
                        if !cell.is_empty() {
                            cell.push(' ');
                        }
                        cell.push_str(&more);
                    }
                }
            }
            _ => rows.push(cells),
        }
        continues = true;
    }
    table(rows, state)
}

/// Inline markup: literals, interpreted text and roles, links and references.
fn inline(text: &str, state: &State) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("``") {
        out.push_str(&spans(&rest[..start], state));
        let after = &rest[start + 2..];
        match after.find("``") {
            Some(end) => {
                let code = &after[..end];
                if code.contains('`') {
                    out.push_str(&format!("`` {code} ``"));
                } else {
                    out.push_str(&format!("`{code}`"));
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str("``");
                rest = after;
            }
        }
    }
    out.push_str(&spans(rest, state));
    out
}

fn spans(text: &str, state: &State) -> String {
    let mut out = String::new();
    let mut last = 0;
    for c in INTERPRETED.captures_iter(text) {
        let whole = c.get(0).map_or(0..0, |m| m.range());
        out.push_str(&references(&text[last..whole.start], state));
        last = whole.end;
        let body = &c[2];
        let labeled = LABELED.captures(body);
        let label = labeled
            .as_ref()
            .map_or(body, |l| l.get(1).map_or("", |m| m.as_str()));
        if c.get(3).is_some() {
            let url = match &labeled {
                Some(l) => {
                    let target = &l[2];
                    match target.strip_suffix('_') {
                        Some(name) => state.targets.get(&name.to_lowercase()).cloned(),
                        None => Some(target.to_owned()),
                    }
                }
                None => state.targets.get(&body.to_lowercase()).cloned(),
            };
            let label = if label.is_empty() { body } else { label };
            match url {
                Some(url) => out.push_str(&format!("[{label}](<{url}>)")),
                None => out.push_str(label),
            }
            continue;
        }
        match c.get(1).map(|m| m.as_str()) {
            Some("ref" | "doc" | "term" | "numref" | "any" | "abbr") => {
                out.push_str(if label.is_empty() { body } else { label });
            }
            Some("emphasis" | "title-reference" | "title" | "t") => {
                out.push_str(&format!("*{body}*"));
            }
            Some("strong") => out.push_str(&format!("**{body}**")),
            Some(_) => out.push_str(&format!("`{label}`")),
            None => out.push_str(&format!("*{body}*")),
        }
    }
    out.push_str(&references(&text[last..], state));
    out
}

/// `name_` references to known targets, and footnote references.
fn references(text: &str, state: &State) -> String {
    let text = FOOTNOTE_REFERENCE.replace_all(text, "[$1]");
    BARE_REFERENCE
        .replace_all(&text, |c: &regex::Captures| {
            let end = c.get(0).map_or(0, |m| m.end());
            let ends_word = text[end..]
                .chars()
                .next()
                .is_none_or(|n| n.is_whitespace() || ".,;:!?)".contains(n));
            match state.targets.get(&c[2].to_lowercase()) {
                Some(url) if ends_word => format!("{}[{}](<{url}>)", &c[1], &c[2]),
                _ => c[0].to_owned(),
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_take_levels_in_the_order_their_adornments_appear() {
        let md = to_markdown("=====\nTitle\n=====\n\nIntro\n-----\n\nText.\n\nMore\n----\n");
        assert_eq!(md, "# Title\n\n## Intro\n\nText.\n\n## More\n");
    }

    #[test]
    fn a_double_colon_starts_a_literal_block() {
        let md = to_markdown("Run this::\n\n    pip install leaf\n    leaf --help\n\nDone.\n");
        assert_eq!(
            md,
            "Run this:\n\n```\npip install leaf\nleaf --help\n```\n\nDone.\n"
        );
        let md = to_markdown("Example ::\n\n  x = 1\n");
        assert_eq!(md, "Example\n\n```\nx = 1\n```\n");
    }

    #[test]
    fn code_blocks_keep_their_language_and_drop_their_options() {
        let md =
            to_markdown(".. code-block:: python\n   :linenos:\n\n   def f():\n       return 1\n");
        assert_eq!(md, "```python\ndef f():\n    return 1\n```\n");
    }

    #[test]
    fn admonitions_become_alerts_or_labeled_quotes() {
        let md = to_markdown(".. warning::\n\n   Back up *first*.\n");
        assert_eq!(md, "> [!WARNING]\n> Back up *first*.\n");
        let md = to_markdown(".. tip:: Short one.\n");
        assert_eq!(md, "> [!TIP]\n> Short one.\n");
        let md = to_markdown(".. seealso::\n\n   The README.\n");
        assert_eq!(md, "> **See also**\n>\n> The README.\n");
    }

    #[test]
    fn lists_keep_their_items_and_nested_bodies() {
        let md = to_markdown("- one\n- two\n  continues\n\n  - nested\n\n#. first\n#. second\n");
        assert_eq!(
            md,
            "- one\n- two\n  continues\n\n  - nested\n\n1. first\n1. second\n"
        );
    }

    #[test]
    fn inline_markup_becomes_markdown() {
        let state = State {
            targets: HashMap::from([("docs".to_owned(), "https://d.io".to_owned())]),
            levels: Vec::new(),
        };
        assert_eq!(
            inline("use ``leaf open`` now", &state),
            "use `leaf open` now"
        );
        assert_eq!(
            inline("see `the site <https://x.io>`_", &state),
            "see [the site](<https://x.io>)"
        );
        assert_eq!(
            inline("read docs_.", &state),
            "read [docs](<https://d.io>)."
        );
        assert_eq!(inline("call :func:`os.walk`", &state), "call `os.walk`");
        assert_eq!(inline(":ref:`Setup <setup>`", &state), "Setup");
        assert_eq!(inline("a `title`", &state), "a *title*");
        assert_eq!(inline("snake_case stays", &state), "snake_case stays");
    }

    #[test]
    fn link_targets_are_dropped_and_resolve_references() {
        let md = to_markdown("Read `Python`_.\n\n.. _Python: https://python.org\n");
        assert_eq!(md, "Read [Python](<https://python.org>).\n");
    }

    #[test]
    fn grid_and_simple_tables_become_markdown_tables() {
        let grid =
            "+------+-----+\n| Name | Age |\n+======+=====+\n| Ada  | 36  |\n+------+-----+\n";
        assert_eq!(
            to_markdown(grid),
            "| Name | Age |\n|---|---|\n| Ada | 36 |\n"
        );
        let simple = "=====  =====\nA      B\n=====  =====\n1      2\n3      4\n=====  =====\n";
        assert_eq!(
            to_markdown(simple),
            "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n"
        );
    }

    #[test]
    fn spanning_grid_tables_come_out_as_written() {
        let grid = "+---+---+\n| a     |\n+---+---+\n";
        assert_eq!(
            to_markdown(grid),
            format!("```\n{}\n```\n", grid.trim_end())
        );
    }

    #[test]
    fn definitions_bold_their_term_and_quotes_stay_quotes() {
        let md = to_markdown("term\n   what it means\n\nPlain.\n\n   Quoted.\n");
        assert_eq!(md, "**term**\n\nwhat it means\n\nPlain.\n\n> Quoted.\n");
    }

    #[test]
    fn fields_images_and_comments() {
        let md = to_markdown(
            ":Author: Ada\n:Version: 2\n\n.. image:: pic.png\n   :alt: A picture\n\n.. a comment\n   that goes on\n\nEnd.\n",
        );
        assert_eq!(
            md,
            "- **Author:** Ada\n- **Version:** 2\n\n![A picture](<pic.png>)\n\nEnd.\n"
        );
    }

    #[test]
    fn a_footnote_body_can_start_on_the_next_line() {
        let md = to_markdown(".. [1]\n   Important detail.\n");
        assert_eq!(md, "[1] Important detail.\n");
    }

    #[test]
    fn adjacent_references_both_resolve() {
        let state = State {
            targets: HashMap::from([("docs".to_owned(), "https://d.io".to_owned())]),
            levels: Vec::new(),
        };
        assert_eq!(
            inline("docs_ docs_", &state),
            "[docs](<https://d.io>) [docs](<https://d.io>)"
        );
    }

    #[test]
    fn tabs_advance_to_the_next_eight_column_stop() {
        let md = to_markdown("-\tfirst\n\tcontinued\n");
        assert_eq!(md, "- first\n  continued\n");
    }

    #[test]
    fn targets_inside_literal_blocks_are_examples_not_targets() {
        let md = to_markdown(
            "Read docs_.\n\n.. _docs: https://real.example\n\n::\n\n    .. _docs: https://example.invalid\n",
        );
        assert_eq!(
            md,
            "Read [docs](<https://real.example>).\n\n```\n.. _docs: https://example.invalid\n```\n"
        );
    }

    #[test]
    fn a_target_url_can_start_on_the_next_line() {
        let md = to_markdown("Read docs_.\n\n.. _docs:\n   https://example.com\n");
        assert_eq!(md, "Read [docs](<https://example.com>).\n");
    }

    #[test]
    fn non_ascii_whitespace_in_code_is_kept_as_code() {
        let md = to_markdown(".. code-block:: text\n\n   \u{a0}x\n    y\n");
        assert_eq!(md, "```text\n\u{a0}x\n y\n```\n");
    }

    #[test]
    fn a_directive_argument_can_start_on_the_next_line() {
        let md = to_markdown(".. image::\n   pic.png\n   :alt: A\n");
        assert_eq!(md, "![A](<pic.png>)\n");
    }

    #[test]
    fn simple_tables_keep_rows_after_a_blank_line() {
        let md = to_markdown(
            "=====  =====\nA      B\n=====  =====\n1      x\n\n2      y\n=====  =====\n\nAfter.\n",
        );
        assert_eq!(md, "| A | B |\n|---|---|\n| 1 | x |\n| 2 | y |\n\nAfter.\n");
    }

    #[test]
    fn a_bullet_wider_in_bytes_than_columns_keeps_its_continuation() {
        let md = to_markdown("• first\n  continued\n");
        assert_eq!(md, "- first\n  continued\n");
    }

    #[test]
    fn simple_table_columns_count_characters_not_bytes() {
        let md = to_markdown("=====  =====\nA      B\n=====  =====\nééééé  1\n=====  =====\n");
        assert_eq!(md, "| A | B |\n|---|---|\n| ééééé | 1 |\n");
    }

    #[test]
    fn a_simple_table_line_with_an_empty_first_column_continues_the_row() {
        let md = to_markdown(
            "=====  =====\nA      B\n=====  =====\n1      first\n       second\n=====  =====\n",
        );
        assert_eq!(md, "| A | B |\n|---|---|\n| 1 | first second |\n");
    }

    #[test]
    fn unknown_directives_are_left_out() {
        let md = to_markdown(".. toctree::\n   :maxdepth: 2\n\n   intro\n\nAfter.\n");
        assert_eq!(md, "After.\n");
    }
}
