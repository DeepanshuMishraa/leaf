# leaf

Read PDFs and web articles in the terminal.

```sh
cargo install --path .   # puts `leaf` on your PATH
ln -s (pwd)/completions/leaf.fish ~/.config/fish/completions/   # fish: tab-completes .pdf files
leaf open paper.pdf
leaf open https://example.com/post
```

Keys: `←/→` (or `h/l`, `p/n`) page, `j/k` scroll, `d/u` half screen, `space/b` screen (turns the page at its end), `g/G` first/last, `t` (or `:`) go to page, `i` original/painted images, `?` all keys, `q` quit.

Drag with the mouse to select text; it's copied to the clipboard when you let go. Press `y` to highlight the selection in yellow, or `y` on a selection that touches a highlight to clear it. Highlights are saved with your reading place.

The reader uses your terminal's own colors. Painted mode draws each image in your text color with a transparent background, so diagrams sit on the terminal like text.

The screen shows only the text and one dim footer: page, total and percent read (`15 / 613 · 2%`). The title goes to the terminal tab.

Each PDF, local or linked, reopens at the page you left. Progress lives in SQLite at `$XDG_DATA_HOME/leaf/leaf.db` (default `~/.local/share/leaf/leaf.db`), keyed by the file's real path or the URL. The image mode (`i`) is saved there too and applies to every document.

Links that serve a PDF (checked by content type, so `/pdf/2401.00001` works) open as PDFs. arXiv `/abs/` links open the full paper.

Windows at least 100×24 show one PDF page per screen; smaller ones scroll through all pages, with the page number between them.

PDFs are converted to markdown (headings from font size, paragraphs rejoined, running headers dropped) with embedded images shown inline. Articles go through Readability; links show their URL.

Images use the Kitty, iTerm2 or Sixel graphics protocol, whichever the terminal answers to, and fall back to colored half blocks.

Built in Rust with ratatui and mupdf.

```sh
cargo test
cargo clippy
```
