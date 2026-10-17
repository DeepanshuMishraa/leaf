# leaf

Read PDFs and web articles in the terminal.

```sh
bun install
bun link            # puts `leaf` on your PATH
leaf open paper.pdf
leaf open https://example.com/post
```

Keys: `←/→` (or `h/l`, `p/n`) page, `j/k` scroll, `d/u` half screen, `space/b` screen (turns the page at its end), `g/G` first/last, `:` go to page, `i` original/painted images, `?` all keys, `q` quit.

The reader uses your terminal's own colors. Painted mode draws each image in your text color with a transparent background, so diagrams sit on the terminal like text.

The screen shows only the text and one dim footer: page, total and percent read (`15 / 613 · 2%`). The title goes to the terminal tab.

Each PDF, local or linked, reopens at the page you left. Progress lives in SQLite at `$XDG_DATA_HOME/leaf/leaf.db` (default `~/.local/share/leaf/leaf.db`), keyed by the file's real path or the URL. The image mode (`i`) is saved there too and applies to every document.

Links that serve a PDF (checked by content type, so `/pdf/2401.00001` works) open as PDFs. arXiv `/abs/` links open the full paper.

Windows at least 100×24 show one PDF page per screen; smaller ones scroll through all pages, with the page number between them.

PDFs are converted to markdown (headings from font size, paragraphs rejoined, running headers dropped) with embedded images shown inline. Articles go through Readability and keep clickable links.

Images use the Kitty graphics protocol when the terminal supports it. Force a protocol with `OPENTUI_IMAGE_PROTOCOL=kitty|sixel|blocks`. Force OSC 8 links with `FORCE_HYPERLINK=1`.

```sh
bun run check   # typecheck
bun test
```
