//! A spreadsheet or CSV file: each sheet in turn, split into pages of
//! `ROWS` rows and `COLUMNS` columns so every cell stays readable. The first
//! row heads every page, and the first column starts every page of a wide
//! sheet, so a row can always be told from the next. A page shows as the
//! spreadsheet draws it (see `grid`), or as a table in the terminal's font.

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use calamine::{Data, Reader, SheetType, SheetVisible};
use image::RgbaImage;

use crate::doc::{Doc, Look, Source};
use crate::grid::{Grid, Looks, letters};

/// Width a page is measured at before the screen picks the real one.
const MEASURE_WIDTH: u32 = 1600;
/// Widest a page is drawn, in pixels.
const MAX_WIDTH: u32 = 3200;

#[derive(Debug, thiserror::Error)]
pub enum SheetFailed {
    #[error("Could not read {path} ({cause}). Check that the path is right and the file exists.")]
    Read { path: String, cause: std::io::Error },
    #[error("{0}")]
    Unreadable(String),
}

/// Data rows on a page.
const ROWS: usize = 60;
/// Columns on a page, counting the first column repeated on later ones.
const COLUMNS: usize = 8;

/// One sheet's cells, as text, without empty rows and columns at its edges.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sheet {
    name: String,
    /// The first row and column the cells start at, counting from 0.
    origin: (usize, usize),
    rows: Vec<Vec<String>>,
}

struct Book {
    sheets: Vec<Sheet>,
    pages: Vec<Page>,
    looks: Option<Looks>,
    look: Cell<Look>,
}

impl Book {
    fn grid(&self, index: usize) -> Option<Grid<'_>> {
        let page = self.pages.get(index)?;
        let span = page.span.as_ref()?;
        let sheet = self.sheets.get(page.sheet)?;
        let (head, body) = sheet.rows.split_first()?;
        let (top, left) = sheet.origin;
        let rows = std::iter::once((top, head.as_slice()))
            .chain(
                body[span.rows.0..span.rows.1]
                    .iter()
                    .enumerate()
                    .map(|(i, row)| (top + 1 + span.rows.0 + i, row.as_slice())),
            )
            .collect();
        Some(Grid {
            rows,
            columns: span.columns.iter().map(|&c| (left + c, c)).collect(),
            looks: self.looks.as_ref(),
            sheet: self.looks.as_ref().and_then(|l| l.sheet(&sheet.name)),
        })
    }
}

impl Source for Book {
    fn page(&self, index: usize) -> Rc<str> {
        let Some(page) = self.pages.get(index) else {
            return "".into();
        };
        match (self.look.get(), &page.span) {
            (Look::Original, Some(_)) => format!(
                "{}![]({})",
                page.intro,
                Doc::image_href(&format!("grid-{index}"))
            )
            .into(),
            _ => format!("{}{}", page.intro, page.table).into(),
        }
    }

    fn image_size(&self, id: &str) -> Option<(u32, u32)> {
        let (w, h) = self.grid(id.strip_prefix("grid-")?.parse().ok()?)?.size();
        let ratio = h / w.max(1.0);
        Some((
            MEASURE_WIDTH,
            (MEASURE_WIDTH as f32 * ratio).round().max(1.0) as u32,
        ))
    }

    fn decode(&self, id: &str, width: u32) -> Option<RgbaImage> {
        let svg = self.grid(id.strip_prefix("grid-")?.parse().ok()?)?.svg();
        crate::typeset::rasterize(&svg, width.clamp(1, MAX_WIDTH))
    }

    fn look(&self) -> Option<Look> {
        Some(self.look.get())
    }

    fn set_look(&self, look: Look) {
        self.look.set(look);
    }
}

pub fn open(path: &Path) -> Result<Doc, SheetFailed> {
    let shown = || path.display().to_string();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let sheets = match ext.as_str() {
        "csv" | "tsv" => {
            let bytes = std::fs::read(path).map_err(|cause| SheetFailed::Read {
                path: shown(),
                cause,
            })?;
            let text = String::from_utf8_lossy(&bytes);
            let delimiter = if ext == "tsv" { '\t' } else { delimiter(&text) };
            vec![trim(Sheet {
                name: String::new(),
                origin: (0, 0),
                rows: parse_csv(&text, delimiter),
            })]
        }
        _ => workbook(path, &shown())?,
    };
    let pages = paginate(&sheets);
    Ok(Doc {
        title: path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        page_count: Some(pages.len()),
        source: Box::new(Book {
            looks: Looks::read(path),
            sheets,
            pages,
            look: Cell::new(Look::Original),
        }),
    })
}

/// Every worksheet in a workbook leaf can read itself (Excel, OpenDocument).
/// Hidden sheets are left out unless every sheet is hidden.
fn workbook(path: &Path, shown: &str) -> Result<Vec<Sheet>, SheetFailed> {
    let mut book = calamine::open_workbook_auto(path).map_err(|e| {
        SheetFailed::Unreadable(format!(
            "Could not read the spreadsheet {shown} ({e}). It may be password-protected or damaged; open it in a spreadsheet app to check. The file was not changed."
        ))
    })?;
    let listed: Vec<(String, bool)> = book
        .sheets_metadata()
        .iter()
        .filter(|s| s.typ == SheetType::WorkSheet)
        .map(|s| (s.name.clone(), s.visible == SheetVisible::Visible))
        .collect();
    let any_shown = listed.iter().any(|(_, visible)| *visible);
    let mut sheets = Vec::new();
    for (name, visible) in listed {
        if any_shown && !visible {
            continue;
        }
        let Ok(range) = book.worksheet_range(&name) else {
            sheets.push(Sheet {
                name,
                origin: (0, 0),
                rows: vec![vec!["(leaf could not read this sheet)".into()]],
            });
            continue;
        };
        let origin = range
            .start()
            .map(|(r, c)| (r as usize, c as usize))
            .unwrap_or_default();
        let rows = range
            .rows()
            .map(|row| row.iter().map(cell).collect())
            .collect();
        sheets.push(trim(Sheet { name, origin, rows }));
    }
    Ok(sheets)
}

/// A cell as it reads: whole numbers without a decimal point, dates as dates.
fn cell(data: &Data) -> String {
    match data {
        Data::Float(f) => number(*f),
        Data::DateTime(d) if d.is_duration() => duration(d.as_f64()),
        Data::DateTime(d) => date(d.as_f64()),
        other => other.to_string(),
    }
}

fn number(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        return format!("{}", f as i64);
    }
    // Ten places hide binary rounding (0.1 + 0.2) without losing real digits.
    let text = format!("{f:.10}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// An Excel serial date (days since 1899-12-30) as `YYYY-MM-DD`, with the
/// time when it has one.
fn date(serial: f64) -> String {
    let days = serial.floor() as i64;
    let seconds = ((serial - serial.floor()) * 86_400.0).round() as i64;
    // Days from 1970-01-01, then Howard Hinnant's civil-from-days.
    let z = days - 25_569 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let date = format!("{year:04}-{month:02}-{day:02}");
    match seconds {
        0 => date,
        s if s >= 86_400 => date,
        s => format!("{date} {:02}:{:02}", s / 3600, s / 60 % 60),
    }
}

fn duration(days: f64) -> String {
    let total = (days * 86_400.0).round() as i64;
    let sign = if total < 0 { "-" } else { "" };
    let total = total.abs();
    format!(
        "{sign}{}:{:02}:{:02}",
        total / 3600,
        total / 60 % 60,
        total % 60
    )
}

/// Comma, unless the first line has more semicolons (as exported where a
/// comma is the decimal point) or tabs.
fn delimiter(text: &str) -> char {
    let first = text.lines().next().unwrap_or("");
    [',', ';', '\t']
        .into_iter()
        .max_by_key(|d| (first.matches(*d).count(), *d == ','))
        .unwrap_or(',')
}

/// RFC 4180 CSV: quoted fields may hold the delimiter, newlines and doubled
/// quotes. A byte order mark is skipped.
fn parse_csv(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                c => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            c if c == delimiter => row.push(std::mem::take(&mut field)),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Drops empty rows and columns from the sheet's edges, moving its origin.
fn trim(mut sheet: Sheet) -> Sheet {
    let blank = |s: &String| s.trim().is_empty();
    while sheet.rows.last().is_some_and(|r| r.iter().all(blank)) {
        sheet.rows.pop();
    }
    let top = sheet
        .rows
        .iter()
        .take_while(|r| r.iter().all(blank))
        .count();
    sheet.rows.drain(..top);
    sheet.origin.0 += top;
    let width = sheet
        .rows
        .iter()
        .map(|r| r.iter().rposition(|c| !blank(c)).map_or(0, |i| i + 1))
        .max()
        .unwrap_or(0);
    let left = sheet
        .rows
        .iter()
        .filter_map(|r| r.iter().position(|c| !blank(c)))
        .min()
        .unwrap_or(0);
    for row in &mut sheet.rows {
        row.resize(width, String::new());
        row.drain(..left.min(row.len()));
    }
    sheet.origin.1 += left;
    sheet
}

/// Cell text that reads as written inside a table.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.trim().chars() {
        match c {
            '\n' | '\r' | '\t' => out.push(' '),
            c if "\\`*_[]<>|~&$#".contains(c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A page of a sheet: what heads it, its cells as a table, and which of the
/// sheet's rows and columns it holds.
#[derive(Debug, Clone)]
struct Page {
    sheet: usize,
    /// The sheet's name and where the page sits in it, as markdown.
    intro: String,
    table: String,
    span: Option<Span>,
}

/// Rows of a sheet's body (after its first row) and its columns, counting from 0.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    rows: (usize, usize),
    columns: Vec<usize>,
}

/// Every sheet's pages, in order.
fn paginate(sheets: &[Sheet]) -> Vec<Page> {
    let mut pages = Vec::new();
    for (i, sheet) in sheets.iter().enumerate() {
        pages.extend(
            sheet_parts(sheet, sheets.len() > 1)
                .into_iter()
                .map(|mut p| {
                    p.sheet = i;
                    p
                }),
        );
    }
    if pages.is_empty() {
        pages.push(Page {
            sheet: 0,
            intro: "*This file has no cells with anything in them.*".to_owned(),
            table: String::new(),
            span: None,
        });
    }
    pages
}

#[cfg(test)]
fn sheet_pages(sheet: &Sheet, named: bool) -> Vec<String> {
    sheet_parts(sheet, named)
        .into_iter()
        .map(|p| p.intro + &p.table)
        .collect()
}

fn sheet_parts(sheet: &Sheet, named: bool) -> Vec<Page> {
    let heading = if named && !sheet.name.is_empty() {
        format!("## {}\n\n", escape(&sheet.name))
    } else {
        String::new()
    };
    let Some((head, body)) = sheet.rows.split_first() else {
        return vec![Page {
            sheet: 0,
            intro: format!("{heading}*This sheet is empty.*"),
            table: String::new(),
            span: None,
        }];
    };
    let width = head.len();
    // Later column pages repeat the first column, so they hold one fewer new one.
    let mut column_spans = vec![(0, width.min(COLUMNS))];
    while let Some(&(_, end)) = column_spans.last()
        && end < width
    {
        column_spans.push((end, (end + COLUMNS - 1).min(width)));
    }
    let row_spans: Vec<(usize, usize)> = if body.is_empty() {
        vec![(0, 0)]
    } else {
        (0..body.len())
            .step_by(ROWS)
            .map(|start| (start, (start + ROWS).min(body.len())))
            .collect()
    };
    let paged = row_spans.len() > 1 || column_spans.len() > 1;
    let mut pages = Vec::new();
    for &(row_start, row_end) in &row_spans {
        for &(col_start, col_end) in &column_spans {
            let columns: Vec<usize> = (col_start > 0)
                .then_some(0)
                .into_iter()
                .chain(col_start..col_end)
                .collect();
            let line = |row: &[String]| {
                let cells: Vec<String> = columns
                    .iter()
                    .map(|&c| escape(row.get(c).map_or("", String::as_str)))
                    .collect();
                format!("| {} |", cells.join(" | "))
            };
            let mut intro = heading.clone();
            if paged {
                let first_row = sheet.origin.0 + 2;
                let col = |c: usize| letters(sheet.origin.1 + c);
                intro.push_str(&format!(
                    "*Rows {}–{} of {} · columns {}–{} of {}*\n\n",
                    thousands(first_row + row_start),
                    thousands(first_row + row_end.max(row_start + 1) - 1),
                    thousands(body.len()),
                    col(col_start),
                    col(col_end - 1),
                    width,
                ));
            }
            let mut table = line(head);
            table.push('\n');
            table.push_str(&format!("|{}\n", " --- |".repeat(columns.len())));
            for row in &body[row_start..row_end] {
                table.push_str(&line(row));
                table.push('\n');
            }
            pages.push(Page {
                sheet: 0,
                intro,
                table,
                span: Some(Span {
                    rows: (row_start, row_end),
                    columns,
                }),
            });
        }
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_hold_delimiters_newlines_and_quotes() {
        let rows = parse_csv(
            "\u{feff}a,\"b,c\"\r\n\"say \"\"hi\"\"\",\"two\nlines\"\nlast",
            ',',
        );
        assert_eq!(
            rows,
            vec![
                vec!["a", "b,c"],
                vec!["say \"hi\"", "two\nlines"],
                vec!["last"],
            ]
        );
    }

    #[test]
    fn semicolon_csv_is_recognised() {
        assert_eq!(delimiter("name;price\nx;1,5"), ';');
        assert_eq!(delimiter("name,price\nx,1"), ',');
        assert_eq!(delimiter("single"), ',');
    }

    #[test]
    fn numbers_and_dates_read_as_a_spreadsheet_shows_them() {
        assert_eq!(number(42.0), "42");
        assert_eq!(number(7.5), "7.5");
        assert_eq!(number(0.1 + 0.2), "0.3");
        assert_eq!(date(45_000.0), "2023-03-15");
        assert_eq!(date(45_000.5), "2023-03-15 12:00");
        assert_eq!(date(1.0), "1899-12-31");
        assert_eq!(duration(1.5), "36:00:00");
    }

    #[test]
    fn column_letters_carry_past_z() {
        assert_eq!(letters(0), "A");
        assert_eq!(letters(25), "Z");
        assert_eq!(letters(26), "AA");
        assert_eq!(letters(701), "ZZ");
        assert_eq!(letters(702), "AAA");
    }

    #[test]
    fn empty_edges_are_trimmed_and_the_origin_follows() {
        let s = |r: &[&[&str]]| -> Vec<Vec<String>> {
            r.iter()
                .map(|row| row.iter().map(|c| c.to_string()).collect())
                .collect()
        };
        let sheet = trim(Sheet {
            name: "S".into(),
            origin: (0, 0),
            rows: s(&[
                &["", "", ""],
                &["", "a", "b"],
                &["", "c", ""],
                &["", "", ""],
            ]),
        });
        assert_eq!(sheet.origin, (1, 1));
        assert_eq!(sheet.rows, s(&[&["a", "b"], &["c", ""]]));
    }

    #[test]
    fn a_small_sheet_is_one_plain_table() {
        let sheet = Sheet {
            name: "Scores".into(),
            origin: (0, 0),
            rows: vec![
                vec!["Name".into(), "Score".into()],
                vec!["A|da".into(), "42".into()],
            ],
        };
        assert_eq!(
            sheet_pages(&sheet, false),
            vec!["| Name | Score |\n| --- | --- |\n| A\\|da | 42 |\n"]
        );
        assert!(sheet_pages(&sheet, true)[0].starts_with("## Scores\n\n| Name"));
    }

    #[test]
    fn a_big_sheet_splits_with_its_header_and_first_column_on_every_page() {
        let width = COLUMNS + 3;
        let rows: Vec<Vec<String>> = (0..=ROWS + 1)
            .map(|r| (0..width).map(|c| format!("r{r}c{c}")).collect())
            .collect();
        let pages = sheet_pages(
            &Sheet {
                name: "Big".into(),
                origin: (0, 0),
                rows,
            },
            false,
        );
        // Two row spans × two column spans.
        assert_eq!(pages.len(), 4);
        assert!(pages[0].starts_with(&format!(
            "*Rows 2–{} of {} · columns A–H of {width}*",
            ROWS + 1,
            ROWS + 1
        )));
        let second_columns = &pages[1];
        assert!(second_columns.contains("| r0c0 | r0c8 | r0c9 | r0c10 |"));
        let second_rows = &pages[2];
        assert!(second_rows.contains("| r0c0 |"));
        assert!(second_rows.contains(&format!("r{}c0", ROWS + 1)));
        assert!(!second_rows.contains("| r1c0 |"));
    }

    #[test]
    fn opens_a_csv_file() {
        let dir = std::env::temp_dir().join(format!("leaf-sheet-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("people.csv");
        std::fs::write(&path, "name,age\nAda,36\n").unwrap();
        let doc = open(&path).unwrap();
        assert_eq!(doc.title, "people.csv");
        assert_eq!(doc.page_count, Some(1));
        assert_eq!(doc.source.look(), Some(Look::Original));
        assert_eq!(&*doc.source.page(0), "![](leaf-image:grid-0)");
        let (w, h) = doc.source.image_size("grid-0").unwrap();
        assert!(w > h, "a two-row sheet is wider than tall");
        let image = doc.source.decode("grid-0", 500).unwrap();
        assert_eq!(image.width(), 500);
        doc.source.set_look(Look::Terminal);
        assert_eq!(
            &*doc.source.page(0),
            "| name | age |\n| --- | --- |\n| Ada | 36 |\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
