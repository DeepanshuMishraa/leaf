# leaf

WOW
Read PDFs, markdown files and web articles in the terminal, and review diffs.

```sh
cargo install leaf-reader   # installs the `leaf` command to ~/.cargo/bin
```

On Linux, install a C toolchain, the libclang development files, the ALSA headers and fontconfig first. Debian and Ubuntu:

```sh
sudo apt install build-essential pkg-config libclang-dev libasound2-dev libfontconfig1
```

If `leaf` isn't found after installing, add `~/.cargo/bin` to your PATH (fish: `fish_add_path ~/.cargo/bin`). Then:

```sh
ln -s (pwd)/completions/leaf.fish ~/.config/fish/completions/   # fish: tab-completes .pdf and .md files
leaf open paper.pdf
leaf open README.md
leaf open https://example.com/post
```

Keys: `←/→` (or `h/l`, `p/n`) page, `j/k` scroll, `ctrl+d`/`u` half screen, `space/b` screen (turns the page at its end), `g/G` first/last, `t` (or `:`) go to page, `i` original/painted images, `?` all keys, `q` quit.

Press `f` to find text: type a word or phrase and press enter. leaf turns to the next page that has it and marks every match in orange. `n` and `N` go to the next and previous match, wrapping round the book, and `esc` clears the search. Case doesn't matter, typed quotes and hyphens find curly quotes and Unicode hyphens (`don't` finds `don’t`), and a phrase is found across line breaks.

Drag with the mouse to select text; it's copied to the clipboard when you let go. Press `@` (or `ctrl+enter`) to ask an agent about that selection: type the question and press enter. leaf sends the document, the line, the lines around the selection, and the question to an agent pane already open in Rex, and submits it. Press `y` to highlight the selection, or `y` on a selection that touches a highlight to clear it. `1`–`6` pick the color (yellow, green, blue, pink, purple, red) for new highlights and ink; pressed with a selection, they paint it, or recolor the highlights it touches. Highlights are saved with your reading place.

Press `d` for the pen: dragging the mouse inks a line over the page in the picked color, anywhere on screen. `e` takes the eraser, which rubs out any stroke it passes over. `esc` goes back to selecting text. `!` shows a short guide to highlighting, colors and drawing. When you let go, a stroke that was meant as a shape is tidied into it: a wobbly line comes out straight (and level or upright when it's close), a loop with three or four corners becomes a triangle or rectangle, and a round loop becomes an ellipse, or a circle when it's nearly round. Handwriting, ticks and scribbles stay as drawn.

Press `v` for the move tool and click a stroke to pick it: a frame with a handle at each corner appears around it. Drag inside the frame to move it, or drag a corner to resize it while the opposite corner stays put. The arrow keys (or `hjkl`) nudge it a cell, `+` and `-` make it bigger or smaller, `1`–`6` recolor it, `x` deletes it, and `esc` lets go. A stroke moved onto other text is pinned to that text from then on.

Each stroke is pinned to the word it starts on, so it moves with that word when the window is resized and the text wraps differently. Strokes are saved with your highlights. Drawing needs the Kitty graphics protocol (Kitty, Ghostty, WezTerm) and doesn't work inside tmux; elsewhere leaf says so in the footer, and saved drawings stay hidden until you open the document in such a terminal.

Press `a` to have the document read aloud. Reading starts at the selection if there is one, otherwise where it last stopped if that's on screen, otherwise at the top of the screen. The word being spoken is marked as it's said, and the screen turns to keep it in view. Scroll away and it stops following until the word is back on screen. `a` pauses and carries on, and `A` or `esc` stops. `]` and `[` step the speed between 0.5× and 4× without changing the voice's pitch. `.` and `,` skip to the next sentence or back to the start of this one (or the one before, if it has only just started). `s` opens the voice picker. Its search box takes a name, language, region or gender (`hindi female`, `british`, `sonia`) and narrows Microsoft's few hundred voices as you type. Move with `↑/↓` and press `enter`. The list shows once it has fully loaded; until then the picker says it's loading, and you can type the search already. A voice picked while reading takes over from the word being read. Connections to the service are kept open between sentences, so the new voice starts in about half a second. The voice and speed are remembered. Speech comes from Microsoft Edge's online read-aloud voices, so reading aloud needs a network connection; the next few sentences are fetched ahead so the voice doesn't stop to wait.

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

Only the lines on screen are drawn, so a diff of any size scrolls without pre-measuring it, and syntax highlighting fills in for the visible hunks. The diff is split side by side: removed code stays on the left, added code on the right, each with its own line number, and paired changes share a row. Added and removed lines are tinted, with the changed words picked out. The tints follow the terminal's light or dark theme and switch with it while the diff is open in Rex (or another terminal that answers the `CSI ? 996 n` color-scheme query); other terminals are matched by their background color once. `LEAF_THEME=light` or `LEAF_THEME=dark` sets it outright.

Keys: `j/k` move between code lines, `d/u` half page, `space/b` page, `f` opens the changed-file picker (type to filter the paths, `enter` opens one), `[`/`]` steps between files, `g/G` first/last. `v` enters visual mode and moving extends the selection; `esc` leaves visual mode and drops the range. `@` (or `ctrl+enter`) asks the agent pane about the range, or the current line: type the question and press enter. `C` comments on the range, or on the current line when there is no selection. Type the comment and press enter to save it; `alt+enter` adds a line, `ctrl+enter` sends it to an agent pane open in Rex, `ctrl+g` posts it to GitHub, and `esc` cancels. `m` lists saved comments to jump to, resend, delete or post; `y` copies a prompt for any agent and `Y` copies every comment. `leaf comments --json` prints every saved comment with its diff key, file, side, line range, body and state for scripts and agents.

The message names the file, the side, and the line range, and includes the hunk around that range, then the comment. It is sent, not left sitting in the agent's input. An agent pane is found from `LEAF_AGENT_PANE`, otherwise by looking for a pane running `claude`, `codex`, `opencode`, `gemini`, `grok`, `pi` or another known agent. With no Rex session or no agent, the prompt is copied through OSC 52 instead. Posting to GitHub asks first and uses `gh` when it is signed in, or `GH_TOKEN`/`GITHUB_TOKEN` for the pull request behind the current branch or link. Comments are saved in the same SQLite file as reading progress.

## Pull requests

`leaf pr` lists the open pull requests that concern you: this branch's first, then this repository's, yours across GitHub, the ones waiting on your review, and the rest in repositories you own. Type to filter by title, repository, number or author, and press `enter` to open one. Name one to open it directly:

```sh
leaf pr                                  # pick from your open pull requests
leaf pr 12                               # #12 in this repository
leaf pr owner/repo#12                    # any repository
leaf pr https://github.com/owner/repo/pull/12
```

The list shows what it found last time at once and refreshes behind it, each heading as its answer arrives. Each pull request takes two lines, its title over who opened it and when; `↑/↓` or the wheel move, a click picks a row and a second click opens it when you let go (a drag doesn't open it), and `ctrl+y` copies the picked pull request's number. The pull request under the cursor is read ahead, so `enter` opens it without waiting, and one read before opens at once while it's read again. Answers, diffs and pictures are cached in `$XDG_CACHE_HOME/leaf/pr` (default `~/.cache/leaf/pr`).

A pull request opens on its title, one line of state, number, author and size, and its tabs. **Summary** shows whether it can merge, the branch, reviewers and labels when it has them, checks that are failing or running, and the description. **Timeline** shows commits, comments, reviews and line comments in order; long bot comments are cut short until `e` expands them. **Code** opens the diff in the reviewer above. The diff is fetched while the page loads and kept against the head commit, so Code opens at once and only fetches again after a push. In Code, `1`, `2` and `←` go back to Summary and Timeline, as do `tab` and `shift+tab` (`h`/`l` scroll sideways there).

Pictures in the description are drawn in place on terminals with the Kitty graphics protocol (Kitty, Ghostty, WezTerm, Rex; not inside tmux), as the reader draws its figures. Images in a table stack under their column's heading. SVG images are drawn too, and an animated GIF plays on its own, looping. A video shows its first frame under a play button. Click it to play it right there, with its sound; a ring turns over the frame until the video has loaded: `ffmpeg` decodes it at the video's own pace, the frames are drawn like any picture and the sound goes to the same output as reading aloud. A click or `esc` stops it, as does leaving the Summary. Without `ffmpeg` a click opens the video in the browser instead. A click on an image shows it full size in the browser. Elsewhere each picture is a one-line placeholder.

Drag over text in Summary or Timeline to select it; it's copied when you let go, and `esc` clears the selection.

Keys: `←/→`, `tab` (`shift+tab` back), `1`, `2`, `3` switch tabs (a click on a tab too), `j/k`, `ctrl+d/u`, `space/b`, `g/G` scroll. `m` merges: pick squash (`s`), merge commit (`m`) or rebase (`r`), then confirm. `x` closes, or reopens a closed one, `r` marks a draft ready or converts it back, `a` approves, `c` comments (`enter` posts, `alt+enter` adds a line). `o` opens it on GitHub, `y` copies `gh pr checkout N`, `Y` the link, `n` the number (or click the number in the header), `R` reloads, `?` lists the keys, `q` goes back to the list. The footer lists the main keys. Merging and closing ask first.

Everything goes through the GitHub CLI, so `gh` must be installed and signed in (`gh auth login`).

## Rex

`leaf rex install` adds Leaf to Rex's command palette. Press `cmd+shift+p` and type Leaf.

Updating leaf updates the plugin too: after `cargo install leaf-reader` installs a new version, the plugin brings itself up to date the next time leaf runs. That includes when Rex loads its config, which runs leaf to ask what's open. There's no need to run `leaf rex install` again. Only the leaf the plugin was installed for does this; a development build run by hand leaves it alone.

Documents and diffs open in the pane the palette was called from. leaf runs in a layer laid exactly over that pane, so nothing is typed into its shell: quitting leaf leaves the shell as it was, with no command on screen or in its history. A document already open there gives way to the new one. A pane busy with something else, such as an editor or an agent, keeps it, and leaf opens beside it.

Always there:

- **Open Document…** floats a picker over the window, like Rex's Go to Directory. The field holds a path, starting at the pane's folder; type to narrow that folder's folders and documents, `tab` or `enter` on a folder goes into it, and `enter` on a document reads it. Documents read lately come first, with how far in. A typed `https://` link opens as it is. `leaf open` with no file, or plain `leaf`, shows the same picker in place.
- **Review Diff**, **Review Staged Changes** and **Review This Branch** open the diff reviewer.
- **Pull Requests** opens `leaf pr` in the pane; pass `target` (a number, `owner/repo#N` or a link) to open one directly.
- **Choose Agent…** floats a list of the agent CLIs installed here (claude, codex, opencode, gemini, grok, pi…), with the one picked now ticked. Questions then go to the pane already running that agent, so its conversation goes on; with none open, leaf starts it in a new pane beside, already asked, and later questions go there. **Any agent pane** sends to whichever agent pane is open: the focused one, else one in the same tab. `leaf agents` shows the same list in place.

While a document or a diff is open:

- **Ask Agent About Selection**, `⇧⌘A`, asks the agent pane open in the session about the selection: in a document, with the file, the page, the line numbers and the lines around it; in a diff, with the file, the side, the line range and the hunk around it. Pass `question` to send it straight away; without one, leaf asks you to type it. Inside leaf, `@` or `ctrl+enter` does the same, and `?` lists it.

While a document is open:

- **Read Aloud / Pause**, **Stop Reading Aloud**, **Read Faster**, **Read Slower**, **Next Sentence** and **Previous Sentence** drive reading aloud.
- **Change Voice…** floats a list of every Microsoft voice over the window, searchable by name, language or gender, with the one reading now ticked. `enter` switches the reader to it.

Rex can't hide an action per pane, so the plugin defines these only while leaf shows a document or a diff on this machine. When that changes, leaf rewrites the `-- open:` line in its block of `init.lua` and reloads the config, quietly; the plugin then asks `leaf rex state` which actions to define. The line has to change: the Rex app fetches its actions again only when `init.lua` itself changed. `leaf voice NAME` sets the default voice from a shell.

Rex's own pickers (themes, Go to Directory) are built into the app, and this version gives a plugin no way to fill one with its own list, so leaf's pickers are drawn to match them in a floating layer.

Remote sessions work too. Rex runs the actions on your Mac and sends pane and block calls to the host, so documents and diffs open on the host, from the host's files. A host without leaf builds it with `cargo install leaf-reader` the first time; it needs Rust there. Commands reach a reader by being typed into its pane (F12, then one line), so they don't depend on which machine it's on. A reader tells Rex it's there with a program-status record (`leaf/reader`). A reader on a remote host can't reload your Mac's config, so the reading actions show there only while a reader is also open on this machine; the keys inside the reader always work.

Rex puts the `rex` command on `PATH` only inside its own panes. leaf also looks in `REX_BIN_DIR` and in Rex's application folder, and `LEAF_REX` names the binary when neither is available. `LEAF_AGENT_PANE` names the block to send to.

`leaf rex uninstall` removes the palette actions. Run `leaf rex install` again after `cargo install` updates leaf or you move the binary. (`leaf plugin install` still works.)

Built in Rust with ratatui and mupdf.

```sh
cargo test
cargo clippy
```
