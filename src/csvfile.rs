//! A CSV or TSV file as the grid (`sheetview`) saves it: the delimiter, line
//! ending and byte order mark it was read with, so a save changes only the
//! cells that were edited. Fields are quoted only when they must be.

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum SaveFailed {
    #[error(
        "Could not save {path} ({cause}). Your edits are still on screen; check that the folder can be written to, then save again."
    )]
    Write { path: String, cause: std::io::Error },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvFile {
    path: PathBuf,
    delimiter: char,
    crlf: bool,
    bom: bool,
}

impl CsvFile {
    /// The format of the CSV or TSV file at `path`; `None` for any other
    /// spreadsheet, which leaf doesn't write.
    pub fn detect(path: &Path) -> Option<CsvFile> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        if ext != "csv" && ext != "tsv" {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        Some(CsvFile {
            path: path.to_owned(),
            delimiter: if ext == "tsv" {
                '\t'
            } else {
                crate::sheet::delimiter(&text)
            },
            crlf: text.contains("\r\n"),
            bom: text.starts_with('\u{feff}'),
        })
    }

    /// The file's name, for messages.
    pub fn name(&self) -> String {
        self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// `rows` as the file's text. `origin` puts back the empty rows and
    /// columns the sheet was trimmed of at its top and left.
    pub fn text(&self, rows: &[Vec<String>], origin: (usize, usize)) -> String {
        let newline = if self.crlf { "\r\n" } else { "\n" };
        let mut out = String::new();
        if self.bom {
            out.push('\u{feff}');
        }
        for _ in 0..origin.0 {
            out.push_str(newline);
        }
        let lead = std::iter::repeat_n(String::new(), origin.1);
        for row in rows {
            let fields: Vec<String> = lead
                .clone()
                .chain(row.iter().map(|field| self.field(field)))
                .collect();
            out.push_str(&fields.join(&self.delimiter.to_string()));
            out.push_str(newline);
        }
        out
    }

    fn field(&self, field: &str) -> String {
        let needs_quotes = field.contains([self.delimiter, '"', '\n', '\r'])
            || field.starts_with(' ')
            || field.ends_with(' ');
        if needs_quotes {
            format!("\"{}\"", field.replace('"', "\"\""))
        } else {
            field.to_owned()
        }
    }

    /// Writes `rows` to the file: to a file beside it first, then moved
    /// over it, so a failed save leaves the old file whole.
    pub fn save(&self, rows: &[Vec<String>], origin: (usize, usize)) -> Result<(), SaveFailed> {
        let failed = |cause| SaveFailed::Write {
            path: self.path.display().to_string(),
            cause,
        };
        let mut beside = self.path.clone().into_os_string();
        beside.push(".leaf-saving");
        let beside = PathBuf::from(beside);
        std::fs::write(&beside, self.text(rows, origin)).map_err(failed)?;
        std::fs::rename(&beside, &self.path).map_err(|cause| {
            let _ = std::fs::remove_file(&beside);
            failed(cause)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(r: &[&[&str]]) -> Vec<Vec<String>> {
        r.iter()
            .map(|row| row.iter().map(|c| (*c).to_owned()).collect())
            .collect()
    }

    fn file(delimiter: char, crlf: bool, bom: bool) -> CsvFile {
        CsvFile {
            path: PathBuf::from("x.csv"),
            delimiter,
            crlf,
            bom,
        }
    }

    #[test]
    fn fields_are_quoted_only_when_they_must_be() {
        let text = file(',', false, false).text(
            &rows(&[&["a", "b,c", "say \"hi\"", "two\nlines", " pad", ""]]),
            (0, 0),
        );
        assert_eq!(
            text,
            "a,\"b,c\",\"say \"\"hi\"\"\",\"two\nlines\",\" pad\",\n"
        );
    }

    #[test]
    fn the_files_own_delimiter_line_ending_and_mark_come_back() {
        let text = file(';', true, true).text(&rows(&[&["a", "1,5"], &["b", "2"]]), (0, 0));
        assert_eq!(text, "\u{feff}a;1,5\r\nb;2\r\n");
    }

    #[test]
    fn trimmed_edges_are_put_back() {
        let text = file(',', false, false).text(&rows(&[&["a", "b"]]), (2, 1));
        assert_eq!(text, "\n\n,a,b\n");
    }

    #[test]
    fn a_saved_file_reads_back_as_the_same_cells() {
        let dir = std::env::temp_dir().join(format!("leaf-csvfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.csv");
        std::fs::write(&path, "name;note\r\nAda;\"x;y\"\r\n").unwrap();
        let csv = CsvFile::detect(&path).unwrap();
        assert_eq!(
            csv,
            CsvFile {
                path: path.clone(),
                delimiter: ';',
                crlf: true,
                bom: false,
            }
        );
        let cells = rows(&[
            &["name", "note"],
            &["Ada", "x;y"],
            &["Grace", "said \"no\""],
        ]);
        csv.save(&cells, (0, 0)).unwrap();
        let back = crate::sheet::load(&path).unwrap();
        assert_eq!(back[0].rows, cells);
        assert!(!dir.join("data.csv.leaf-saving").exists());
        assert_eq!(CsvFile::detect(&dir.join("book.xlsx")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
