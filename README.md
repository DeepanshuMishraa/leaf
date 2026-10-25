# leaf

Read PDFs, markdown files and web articles in the terminal, and review diffs.

```sh
cargo install leaf-reader   # installs the `leaf` command to ~/.cargo/bin
```

If `leaf` isn't found after installing, add `~/.cargo/bin` to your PATH (fish: `fish_add_path ~/.cargo/bin`). Then:

```sh
ln -s (pwd)/completions/leaf.fish ~/.config/fish/completions/   # fish: tab-completes .pdf and .md files
leaf open paper.pdf
leaf open README.md
leaf open https://example.com/post
```

Keys: `←/→` (or `h/l`, `p/n`) page, `j/k` scroll, `d/u` half screen, `space/b` screen (turns the page at its end), `g/G` first/last, `t` (or `:`) go to page, `i` original/painted images, `?` all keys, `q` quit.

Press `f` to find text: type a word or phrase and press enter. leaf turns to the next page that has it and marks every match in orange. `n` and `N` go to the next and previous match, wrapping round the book, and `esc` clears the search. Case doesn't matter, and a phrase is found across line breaks.

Drag with the mouse to select text; it's copied to the clipboard when you let go. Press `y` to highlight the selection in yellow, or `y` on a selection that touches a highlight to clear it. Highlights are saved with your reading place.

The reader uses your terminal's own colors. Painted mode draws each image in your text color with a transparent background, so diagrams sit on the terminal like text.

The screen shows only the text and one dim footer: page, total and percent read (`15 / 613 · 2%`). The title goes to the terminal tab.

Each PDF, local or linked, reopens at the page you left. Markdown files and articles reopen at the line you left, at any window width, and the footer shows how far down you are. Progress lives in SQLite at `$XDG_DATA_HOME/leaf/leaf.db` (default `~/.local/share/leaf/leaf.db`), keyed by the file's real path or the URL. The image mode (`i`) is saved there too and applies to every document.

Links that serve a PDF (checked by content type, so `/pdf/2401.00001` works) open as PDFs. arXiv `/abs/` links open the full paper. Links that serve markdown (`text/markdown`, or a `.md` file served as plain text) open as markdown, with relative images fetched from beside them. GitHub `blob/…/*.md` links open the raw file.

Windows at least 100×24 show one PDF page per screen; smaller ones scroll through all pages, with the page number between them.

PDFs are converted to markdown (headings from font size, paragraphs rejoined, running headers dropped) with embedded images shown inline. Articles go through Readability; links show their URL.

Markdown files (`.md`, `.markdown`) show as written, with GitHub's extensions: tables, task lists, strikethrough, footnotes and alerts (`> [!NOTE]`), plus definition lists and math (kept as source). Front matter is hidden, HTML blocks such as a README's centered logo are read as markdown, and images next to the file show inline (PNG, JPEG, GIF, WebP).

`mermaid` code blocks are drawn as diagrams in the terminal's colors, with [Pele](https://github.com/obsidianmd/pele) run under `bun` or `node`; without either, or for a diagram Pele can't read, the block shows as code.

Code is colored with [Highlights](https://diffs.com/highlights), whose lexers are WebAssembly. A fence's language is used. PDF and EPUB blocks have no language, so they are colored only when the source is clearly one language; anything uncertain stays plain. Colors are the terminal's own palette.

Images use the Kitty, iTerm2 or Sixel graphics protocol, whichever the terminal answers to, and fall back to colored half blocks.

## Diffs

`leaf diff` shows the working tree's changes. Pass a path, revision, range, patch file or GitHub link to review something else:

```sh
leaf diff                          # unstaged changes
leaf diff --staged                 # staged changes
leaf diff --branch                 # this branch against its base
leaf diff main...HEAD              # any revision or range
leaf diff src/viewer.rs            # one file
leaf diff changes.patch            # a .diff or .patch file, anywhere
leaf diff https://github.com/owner/repo/pull/12
```

Local diffs need a Git repository. A patch file and a GitHub pull request, commit or comparison link open without one. `o` flips between the working tree and the base branch, and `r` reloads.

Only the rows on screen are drawn, so a diff of any size scrolls without pre-measuring it, and syntax highlighting fills in for the visible hunks. The gutter shows both line numbers; added and removed lines are tinted across the width, with the changed words picked out.

Keys: `j/k` move between code lines, `d/u` half page, `space/b` page, `[`/`]` files, `g/G` first/last. `v` starts a selection and moving extends it, `c` comments on it. Type the comment and press enter to paste it into an agent running in another tmux pane; `alt+enter` adds a line and `esc` keeps a draft. The paste is the path and line range, the patch lines in a fenced block, then the comment. `C` lists saved comments to jump to, resend, delete or post; `y` copies a prompt for any agent and `Y` copies every comment.

An agent pane is found from `LEAF_AGENT_PANE=%3`, otherwise by looking for a pane running `claude`, `codex`, `opencode`, `gemini` or another known agent. Pasting is bracketed, so it fills the prompt without submitting it. With no agent or no tmux, the prompt is copied through OSC 52 instead. Posting to GitHub asks first and uses `gh` when it is signed in, or `GH_TOKEN`/`GITHUB_TOKEN` for the pull request behind the current branch or link. Comments are saved in the same SQLite file as reading progress.

Built in Rust with ratatui and mupdf.

```sh
cargo test
cargo clippy
```
