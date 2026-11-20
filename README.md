# leaf

Read PDFs, markdown files and web articles in the terminal, and review diffs.

```sh
curl -fsSL https://leaf.dipxsy.app | sh
```

This installs a prebuilt `leaf` for macOS (11 or later) or Linux (glibc 2.35 or later, such as Ubuntu 22.04), on x86_64 or arm64, to `~/.local/bin`. If `leaf` is already installed, from `cargo install` too, it's updated where it is. The download is checked against its published SHA-256 and run once before it replaces anything. Set `LEAF_INSTALL_DIR` to install somewhere else, or `LEAF_VERSION` for a release other than the newest. On Linux, leaf needs the ALSA sound library (`libasound2t64` or `libasound2` on Debian and Ubuntu), which desktops already have.

Or with Homebrew, on macOS or Linux:

```sh
brew install deepanshumishraa/tap/leaf
```

In a Brewfile, that's `brew "deepanshumishraa/tap/leaf"`. `brew upgrade leaf` updates it; `leaf upgrade` leaves a Homebrew install to Homebrew.

`leaf version` prints the version, and `leaf upgrade` installs the newest release in place, checked the same way (`leaf upgrade --check` only says whether there is one). Releases are listed at https://github.com/DeepanshuMishraa/leaf/releases.

To build from source instead, use Cargo:

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
leaf open talk.pptx
leaf open budget.xlsx
leaf open https://example.com/post
```

For zsh completions, from the repository root, link `completions/_leaf` into a directory on your `$fpath`. If you don't have one, add it in `~/.zshrc` before `compinit`:

```sh
mkdir -p ~/.zsh/completions
ln -s "$PWD/completions/_leaf" ~/.zsh/completions/
fpath=(~/.zsh/completions $fpath)   # in ~/.zshrc, before compinit
```

Keys: `←/→` (or `h/l`, `p/n`) page, `j/k` scroll, `ctrl+d`/`u` half screen, `space/b` screen (turns the page at its end), `g/G` first/last, `t` (or `:`) go to page, `i` original/painted images, `<`/`>` narrower/wider column, `=` fit the screen (again to go back), `?` all keys, `q` quit.

On a picture file, `←/→` (or `h/l`, `p/n`) move to the previous or next picture in its folder instead, since a picture has only the one page. Names go in the order a file manager lists them, with numbers counted as numbers (`img2` before `img10`), and past the last picture the first comes round again. The footer shows where you are among them, like `2 / 12`.

Press `f` to find text: type a word or phrase and press enter. leaf turns to the next page that has it and marks every match in orange. `n` and `N` go to the next and previous match, wrapping round the book, and `esc` clears the search. Case doesn't matter, typed quotes and hyphens find curly quotes and Unicode hyphens (`don't` finds `don’t`), and a phrase is found across line breaks.

Drag with the mouse to select text; it's copied to the clipboard when you let go. Press `@` (or `ctrl+enter`) to ask an agent about that selection: type the question and press enter. leaf sends the document name, line numbers, exact selected text, surrounding lines, and question to an agent pane already open in Rex. It submits the question automatically. The selected text is never truncated; surrounding lines are capped at 400 characters each. Leaf quotes the text and marks it as document data, not instructions. Press `y` to highlight the selection, or `y` on a selection that touches a highlight to clear it. `1`–`6` pick the color (yellow, green, blue, pink, purple, red) for new highlights and ink; pressed with a selection, they paint it, or recolor the highlights it touches. Highlights are saved with your reading place.

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

Office files open too, for reading. leaf reads them itself, the same on macOS and Linux, with nothing else installed:

- **Slide decks** (`.pptx`, `.pptm`, `.ppsx`, `.ppsm`, `.potx`, `.potm`) show one slide per page, with speaker notes beneath. `←/→` move between slides.
- **Word documents** (`.docx`, `.docm`, `.dotx`, `.dotm`) show page by page, on the document's own page size and margins, with its lists, tables and pictures.
- **Spreadsheets** (`.xlsx`, `.xlsm`, `.xlsb`, `.xls`, `.xla`, `.xlam`, `.ods`) and **CSV/TSV** files show each sheet in pages of 60 rows and 8 columns. The header row tops every page, and the first column starts every page of a wide sheet. Semicolon-separated CSV is detected.

Decks and Word documents open as designed, drawn as a picture in their own fonts, colors and layout. Spreadsheets open as a grid: `.xlsx` and `.xlsm` workbooks keep their cell fonts, fills, borders, column widths and row heights; other spreadsheets and CSV/TSV files get a plain grid sized to their cells. `+` and `-` zoom (as `>` and `<` do), and `=` fits a page to the screen. `o` switches to the terminal's font and colors, as markdown text and tables; the page stays the same. In the designed look, `i` switches between real colors and the picture painted in the terminal's colors, as with PDFs. The footer shows these keys.

Older formats (`.doc`, `.ppt`, `.rtf`, `.odt`, `.odp`) and Apple's (`.pages`, `.key`, `.numbers`) aren't read; save them as `.docx`, `.pptx` or `.xlsx` from the app that made them.

Local Markdown, PDF, EPUB, office, and picture files reload in place after changes settle. Reloading pauses while you select text or use a dialog. Failed reloads wait five seconds before retrying, unless the file changes again. PDF and EPUB replacements load in the background while you keep reading the current document.

`mermaid` code blocks are drawn as diagrams in the terminal's colors, with [Pele](https://github.com/obsidianmd/pele) run under `bun` or `node`; without either, or for a diagram Pele can't read, the block shows as code.

Code is colored with [Highlights](https://diffs.com/highlights), whose lexers are WebAssembly. A fence's language is used. PDF and EPUB blocks have no language, so they are colored only when the source is clearly one language; anything uncertain stays plain. Colors are the terminal's own palette.

Images use the Kitty, iTerm2 or Sixel graphics protocol, whichever the terminal answers to, and fall back to colored half blocks.

## Configuration

Leaf reads an optional settings file at `$XDG_CONFIG_HOME/leaf/config.toml`. When `XDG_CONFIG_HOME` is unset or empty, it uses `$HOME/.config/leaf/config.toml`. If neither base is available, it uses defaults without searching the current folder. Leaf never creates or rewrites the settings file.

To change the maximum reading width, add:

```toml
[reader]
max_width = 100
```

`max_width` accepts integers from 1 through 65535 and defaults to 76. The column stays centered and fits within the terminal, retaining the existing four-column margin budget and minimum one-column layout. This changes wrapping, not the terminal font size. Images fit within the current reading width; their row limit is unchanged. While reading, `<` and `>` narrow or widen the column by 8, with `<` stopping at 20 columns, and `=` fits it to the screen or returns to `max_width`. These last until you quit.

Missing or empty files and omitted settings use defaults. Unknown keys, invalid values, and unreadable files stop document opening before fetching or converting the document, with an error identifying the file and cause. Reopen the reader after editing the file; picture navigation keeps the settings loaded at the start of the session.

These settings apply only to the document reader, not plans, diffs, pull requests, issues, or the home screen. Existing preferences and reading progress remain in SQLite.

## Diffs

`leaf diff` shows the working tree's changes. Pass a path, revision, range, patch file or GitHub pull request or GitLab merge request link to review something else:

```sh
leaf diff                          # unstaged changes
leaf diff --staged                 # staged changes
leaf diff --branch                 # this branch against its base
leaf diff main...HEAD              # any revision or range
leaf diff src/viewer.rs            # one file
leaf diff changes.patch            # a .diff or .patch file, anywhere
leaf diff --inline                 # one column instead of side by side
leaf diff https://github.com/owner/repo/pull/12
```

Local diffs need a Git repository. A patch file and a GitHub or GitLab link open without one: a pull request or merge request, a commit, or a comparison. `o` flips between the working tree and the base branch, and `r` reloads.

A self-hosted GitLab is recognized by the `/-/` in its links, so a link opens whatever its host is called; an older link without it, which GitLab itself redirects, opens too. `GITLAB_HOST` names one whose remote doesn't say `gitlab` in its name: one or more hosts, comma separated, as a URL or a bare name.

Leaf trusts a host with a credential only when the host names itself GitLab (`gitlab.com`, or a name with a `gitlab` label) or when `GITLAB_HOST` names it, so a link pasted from anywhere else is read without one; a host that names itself GitHub never qualifies, so a stray `GITLAB_HOST` can't send a token there. A merge request's patch is asked of `glab` first when it's installed, which opens a private project with the sign-in it already holds. Failing that, a merge request, a commit and a comparison are all asked of GitLab's API when `GITLAB_TOKEN` holds a personal access token with `read_api`, since the page routes that serve a patch refuse a token and answer with the sign-in page. The page itself is asked when no token is set, and again when the API doesn't answer -- an expired token shouldn't hide a public project. No credential is needed to read a public project. A page the API answers with only part of -- a merge request past a thousand files, or a comparison that ran out of time -- is refused rather than shown with files missing; the page routes carry no such warning, so a cut-short answer from them can't be told apart from a whole one. Links are fetched over HTTPS. Comments are still GitHub's alone, so a merge request's diff opens to read and to note on, not to post.

Local diffs refresh every second; diffs from GitHub and GitLab refresh every 10 seconds, including the PR Code tab. Unchanged content keeps your position. Refresh waits while you write notes or select text. If annotated files change, Leaf keeps the reviewed snapshot until you clear their line comments. Posting that stale snapshot to GitHub is blocked.

Only the lines on screen are drawn, so a diff of any size scrolls without pre-measuring it, and syntax highlighting fills in for the visible hunks. The diff is split side by side: removed code stays on the left, added code on the right, each with its own line number, and paired changes share a row. `i` switches to one column, removed lines above added ones as `git diff` shows them, with both line numbers. The layout you pick is remembered, so the next diff, review or PR Code tab opens the same way; `leaf diff --inline` opens in one column regardless. Comments and the changed-word highlighting work the same in both. Added and removed lines are tinted, with the changed words picked out. The tints follow the terminal's light or dark theme and switch with it while the diff is open in Rex (or another terminal that answers the `CSI ? 996 n` color-scheme query); other terminals are matched by their background color once. `LEAF_THEME=light` or `LEAF_THEME=dark` sets it outright.

Keys: `j/k` move between code lines, `d/u` half page, `space/b` page, `f` opens the changed-file picker (type to filter the paths, `enter` opens one), `[`/`]` steps between files, `g/G` first/last. `V` (or `v`) enters visual mode and moving extends the selection; `esc` leaves visual mode and drops the range. `@` (or `ctrl+enter`) asks the agent pane about the range, or the current line: type the question and press enter. `c` (or `C`) comments on the range, or on the current line when there is no selection. Type the comment and press enter to save it; `alt+enter` adds a line, `ctrl+enter` sends it to an agent pane open in Rex, `ctrl+g` posts it to GitHub, and `esc` cancels. `m` lists saved comments to jump to, resend, delete or post; `y` copies a prompt for any agent and `Y` copies every comment. `leaf comments --json` prints every saved comment with its diff key, file, side, line range, body and state for scripts and agents.

The message names the file, the side, and the line range, and includes the hunk around that range, then the comment. It is sent, not left sitting in the agent's input. An agent pane is found from `LEAF_AGENT_PANE`, otherwise by looking for a pane running `claude`, `codex`, `opencode`, `gemini`, `grok`, `pi` or another known agent. With no Rex session or no agent, the prompt is copied through OSC 52 instead. Posting to GitHub asks first and uses `gh` when it is signed in, or `GH_TOKEN`/`GITHUB_TOKEN` for the pull request behind the current branch or link. Comments are saved in the same SQLite file as reading progress.

## Whiteboard

`leaf board [file.excalidraw]` turns the terminal into a whiteboard you share with your agent. With no file named it starts a new board in the current folder under a fresh name like `board-misty-harbor.excalidraw`, saved the moment it opens; `leaf sketch.excalidraw` opens one directly. The board is a plain Excalidraw file, so it also opens on excalidraw.com.

Pick a tool from the floating top bar or by key: `v` select, `h` hand, `r` box, `d` diamond, `o` ellipse, `a` arrow, `l` line, `p` pen, `t` text, `F` frame, `e` eraser. The lock on its left (or `L`) locks the canvas so it can't be panned or zoomed. Below the bar, click a stroke color, a background color, or the fill, line and width to change them for the selection and for what's drawn next. Drag to draw; drawing a shape goes back to select, and a selected shape can be dragged even while a shape tool is still out. An arrow drawn from one shape to another stays attached when either moves. With select, click or drag a box around things to pick them (shift adds), drag to move them, drag a handle to resize (shift on a corner keeps the proportions), and drag the knob above to turn (shift snaps to 15°). Double-click or `enter` writes text, inside a shape when one is picked. `x` deletes.

`1`–`6` set the ink of the selection and new strokes, `0` goes back to plain ink; `b` cycles the fill (hachure, cross-hatch, solid, none), `s` dashes, `w` stroke width, `k` how hand-drawn it looks, `m` arrowheads, `n` the font, `<`/`>` text size. `[` `]` `{` `}` move things through the stack, `g`/`G` group and ungroup, `ctrl+d` duplicates, `y`, `ctrl+x` and `ctrl+v` copy, cut and paste (Excalidraw clipboard JSON too), and `q` or `ctrl+c` quits. Pasting or dropping a picture's path puts it on the board. `u`/`U` undo and redo. Arrows nudge the selection, or pan with nothing picked; scrolling or a right-drag pan; `ctrl`+scroll, `option`+scroll or `+`/`-` zoom, and `z` makes plain scrolling zoom (terminals keep trackpad pinches to themselves, so this is how a trackpad zooms); `f` fits the whole board.

In kitty, Ghostty, WezTerm, iTerm2 and other terminals that show pictures, the board is painted as a picture with hand-drawn strokes, fills and pictures; elsewhere it's drawn in Braille dots with tinted fills and half-block pictures. In a terminal that shows pictures, `i` switches between the two. `E` writes the board beside its file as SVG and PNG; from the shell, `leaf board file.excalidraw --export out.png` (or `out.svg`) writes the one picture named.

`@` (or `c`) asks the agent pane: type a question and press enter. The agent gets the board's path, how the file is laid out, and what you selected, and it draws by editing the file. Every change you make is saved at once, and the file is checked every 30 ms, so the agent's edits appear as it writes them; `u` undoes one.

## Pull requests and issues

`leaf pr` lists the pull requests and issues that concern you, in two tabs; `←/→` switches. **Pull requests** holds yours across GitHub, the ones in repositories you own, the ones waiting on your review, the ones assigned to you, and this repository's. **Issues** holds the ones you opened, the ones opened in repositories you own, and the ones assigned to you. Each tab is one list, newest first, and each row's dot says whose it is: green is yours in your own repository, blue is yours in someone else's, yellow is someone else's in yours, and grey is the rest. The legend beside the filter names the colors on screen.

`tab` flips each tab between **Open** and **Merged** (pull requests) or **Closed** (issues); the switch at the top right shows which, and a click flips it too. `@` (or a click on the switch beside it) narrows both tabs to **Assigned**, only what's assigned to you, and back to **All**. The done lists are grouped by when each one ended (Today, Yesterday, This week, This month, This year, Older), newest first. A purple `◆` is merged, a purple `✓` closed as completed, a grey `⊘` closed as not planned, and the author's name keeps its color. A sparkline beside the filter counts how many were merged or closed in each of the last twelve weeks. Done lists show what was found last time and are asked again the first time you open them in a run.

Type to filter by title, repository, number or author. `enter` opens either one here in leaf. Name a pull request to open it directly:

```sh
leaf pr                                  # pick from your open pull requests and issues
leaf pr 12                               # #12 in this repository
leaf pr owner/repo#12                    # any repository
leaf pr https://github.com/owner/repo/pull/12
```

The list shows what it found last time at once and refreshes behind it as each answer arrives. Each row takes two lines, its title over who opened it and when; `↑/↓` or the wheel move, a click picks a row and a second click opens it when you let go (a drag doesn't open it), and `ctrl+y` copies the picked row's number, pull request or issue. `R` (or `ctrl+r` while filtering) asks every search again now instead of waiting. The item under the cursor is read ahead, so `enter` opens it without waiting, and one read before opens at once while it's read again. Answers, diffs and pictures are cached in `$XDG_CACHE_HOME/leaf/pr` (default `~/.cache/leaf/pr`).

A pull request opens on its title, one line of state, number, author and size, and its tabs. **Summary** shows whether it can merge, the branch, reviewers and labels when it has them, checks that are failing or running, and the description. **Timeline** shows commits, comments, reviews and line comments in order; long bot comments are cut short until `e` expands them. **Comments** holds only the comments and reviews, each in full: `j`/`k` pick one, `enter` replies quoting it, and `c` comments on the pull request. **Code** opens the diff in the reviewer above. The diff is fetched while the page loads and cached by head commit, so Code can open without waiting. While Code is open, its reviewer refreshes the diff every 10 seconds. In Code, `1`, `2`, `3` and `5` go to Summary, Timeline, Comments and Checks, as do `←/→`, `tab` and `shift+tab` (`h`/`l` scroll sideways there). **Checks** lists every check under its workflow, failing ones first, with how long each took. `↑/↓` pick one, and `enter` opens a GitHub Actions job as its steps, like GitHub's job page: failed steps open with line numbers, errors and warnings marked, and the log's own colors, scrolled to the last error. There, `↑/↓` pick a step or one of the runner's groups in an open step, `space` or `enter` opens or closes it, `e` opens or closes all groups, and `esc` goes back to the list. A running job's steps update every 5 seconds; GitHub has its log once it finishes. In the list and in a job, `r` offers GitHub's re-runs for the picked job's workflow run, then asks to confirm: `f` re-runs failed jobs, `j` this job, `a` all jobs (on this tab `r` doesn't mark the pull request ready). `o` opens the picked check on GitHub. Visible PR and issue pages and the shown list refresh every 10 seconds. Refresh waits while you compose a comment, filter the list, or select text.

An issue opens on the same page, with **Summary** (its state, assignees, labels and description) **Timeline** (its history) and **Comments** (each comment in full, `j`/`k` to pick one); it has no Code or Checks tab. `c` comments, and `]`/`[` picks a comment for `enter` to reply to, quoting it. `x` closes an open issue as completed or not planned, or reopens a closed one, and `o` opens it on GitHub.

Pictures in the description are drawn in place on terminals with the Kitty graphics protocol (Kitty, Ghostty, WezTerm, Rex; not inside tmux), as the reader draws its figures. Images in a table stack under their column's heading. SVG images are drawn too, and an animated GIF plays on its own, looping (a GIF too large for the memory set aside for GIFs shows its first frame). A video shows its first frame under a play button. Click it to play it right there, with its sound; a ring turns over the frame until the video has loaded: `ffmpeg` decodes it at the video's own pace, the frames are drawn like any picture and the sound goes to the same output as reading aloud. A click or `esc` stops it, as does leaving the Summary. Without `ffmpeg` a click opens the video in the browser instead. A click on an image shows it full size in the browser. Elsewhere each picture is a one-line placeholder.

Drag over text in Summary or Timeline to select it; it's copied when you let go, and `esc` clears the selection.

Keys: `←/→`, `tab` (`shift+tab` back), `1` to `5` switch tabs (a click on a tab too), `j/k`, `ctrl+d/u`, `space/b`, `g/G` scroll. `m` merges: pick squash (`s`), merge commit (`m`) or rebase (`r`), then confirm. `x` closes, or reopens a closed one, `r` marks a draft ready or converts it back, `a` approves, `c` comments (`enter` posts, `alt+enter` adds a line). `]`/`[` picks a comment and `enter` replies to it, quoting it. `o` opens it on GitHub, `y` copies `gh pr checkout N`, `Y` the link, `n` the number (or click the number in the header), `R` reloads, `?` lists the keys, `q` goes back to the list. The footer lists the main keys. Merging and closing ask first.

Everything goes through the GitHub CLI, so `gh` must be installed and signed in (`gh auth login`).

## Margin: review your agent's plans and changes

Margin is how you and a coding agent pass work back and forth in leaf. The agent hands over a plan or a set of changes, you leave notes, and they go back as one review with a verdict. The agent gets it as its next message, in the conversation it's already having.

```sh
leaf agent setup      # set up every agent CLI here (asks first; --dry-run shows the changes)
```

`leaf agent setup` finds the coding agents on this machine, shows what it will change, and asks once. It installs the leaf skills and adds a plan hook where the agent supports one. The skills also land in `~/.claude/skills` and `~/.agents/skills`, ready for an agent you install later. With a hook, a plan opens in leaf at the moment the agent would ask you to approve it. Request changes and the agent revises; approve and it goes ahead.

| Agent | How it hands you a plan |
|---|---|
| Claude Code | `PermissionRequest` hook on `ExitPlanMode` |
| Grok Build | `PreToolUse` hook on `exit_plan_mode` (Grok then asks you to confirm once more) |
| Codex | `Stop` hook on plan mode's `<proposed_plan>`. Trust it once in Codex's `/hooks`. |
| Antigravity | `Stop` hook that finds a new `implementation_plan.md` |
| Cursor | `preToolUse` hook on its plan tool |
| Pi | an extension with `leaf_review_plan` and `leaf_review_changes` tools |
| OpenCode | the skills, since it has no plan hook; it runs `leaf plan` itself |

Config files are merged, never replaced, and the first change to a JSON file keeps a `.leaf-backup` beside it. Run `leaf agent setup` again to refresh leaf's hooks and skills; files edited by hand are left alone. To remove setup, delete the leaf skill folders and remove the lines or entries marked `leaf-margin` from the agent configs.

### Plans

`leaf plan plan.md` splits a Markdown plan into numbered blocks: each heading with everything under it, up to the next heading. Text before the first heading, or after a `---` rule, is a block of its own. `j/k` move line by line, `]`/`[` jump between blocks and `J/K` jump to blocks with notes. Then:

- `c` comments on the cursor's line. `V` starts a selection, `j/k` extend it inside the block, and `c` comments on the selected lines.
- `B` comments on the whole block.
- `x` removes the selection, or marks the whole block for removal (press it again to keep the block).
- `r` replaces the selection or the block with your text, starting from its own.

Dragging over text selects a passage the same way. `C` writes a global note on the whole plan, `L` lists every note to jump to, edit or delete, and `X` clears a block's notes. `S` sends the review: `a` approve, `n` approve with notes, `c` request changes.

Plans update in place when their Markdown file changes. Refresh waits while you write notes or select text, and notes follow unchanged blocks. If annotated blocks change or cannot be matched unambiguously, Leaf keeps the reviewed snapshot until you send or clear their notes.

Notes you haven't sent are kept and come back next time. A note on a block that has since been rewritten is dropped, and leaf tells you. Each new version of a plan is a round. From the second round on, blocks that changed are marked `+`, the header counts the changed and removed blocks, and `D` steps through what changed.

### Changes

`leaf review` takes the same sources as `leaf diff` (`--staged`, `--branch`, a range, a patch or a GitHub link). It opens on an overview of every changed file, with its size, its notes and whether you've viewed it. `enter` opens a file, and `tab` comes back to the overview. Past the last file, `]` comes back too. Notes go on three levels:

- lines: `c` on the cursor line, or press `V` to select lines with `j/k`, then `c`
- a whole file: `F`, or `c` in the overview
- the whole change: `M`, or `C` in the overview

`w` marks a file viewed. The mark clears if the file changes. `S` sends the draft line comments and the notes as one review, grouped by file in diff order, with each line comment's hunk. Plain `leaf diff` has the overview, the notes and `S` as well, and sends the review straight to the agent pane.

### Who opens it

A person can run `leaf plan` or `leaf review` in a terminal. The review goes to the agent pane leaf finds, as questions do, or to stdout with `--print`.

An agent can run them too, and hooks do. The agent has no terminal, so leaf opens the review in a pane beside it. That needs one of Rex, tmux or Herdr: start your agent inside one of them. leaf returns at once, and the review goes back into the agent's pane when you send it. `--wait` (what hooks use) blocks instead and prints the review. Outside all three, leaf tells the agent to ask you to run the command, and a hook steps aside so the agent asks you as it normally would.

## Rex

`leaf rex install` adds Leaf to Rex's command palette. Press `cmd+shift+p` and type Leaf.

Updating leaf updates the plugin too: after `cargo install leaf-reader` installs a new version, the plugin brings itself up to date the next time leaf runs. That includes when Rex loads its config, which runs leaf to ask what's open. There's no need to run `leaf rex install` again. Only the leaf the plugin was installed for does this; a development build run by hand leaves it alone.

Documents and diffs open in the pane the palette was called from. leaf runs in a layer laid exactly over that pane, so nothing is typed into its shell: quitting leaf leaves the shell as it was, with no command on screen or in its history. A document already open there gives way to the new one. A pane busy with something else, such as an editor or an agent, keeps it, and leaf opens beside it.

Always there:

- **Open Document…** floats a picker over the window, like Rex's Go to Directory. The field holds a path, starting at the pane's folder; type to narrow that folder's folders and documents, `tab` or `enter` on a folder goes into it, and `enter` on a document reads it. Documents read lately come first, with how far in. A typed `https://` link opens as it is. `leaf open` with no file, or plain `leaf`, shows the same picker in place.
- **Review Diff**, **Review Staged Changes** and **Review This Branch** open the diff reviewer.
- **Pull Requests & Issues** opens `leaf pr` in the pane; pass `target` (a number, `owner/repo#N` or a link) to open one directly.
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

To release, set `version` in `Cargo.toml` (and `package.nix`, and the `ver` badge in `site/public/index.html`), add its notes to `CHANGELOG.md` (leaf shows them once after the update, and the tests fail without them), run `cargo check` so `Cargo.lock` follows, commit, then push a matching tag: `git tag v0.12.0 && git push origin v0.12.0`. The release workflow checks the tag against `Cargo.toml` and runs the tests. It then builds and runs leaf for each of the four targets, and only once all four pass publishes a GitHub release with `leaf-<target>.tar.gz`, a `.sha256` for each and `SHA256SUMS`. Last, it installs that release with the install script on macOS and Linux. A stable release also bumps `Formula/leaf.rb` in [DeepanshuMishraa/homebrew-tap](https://github.com/DeepanshuMishraa/homebrew-tap), written by `.github/homebrew-formula.sh` and pushed with the `HOMEBREW_TAP_DEPLOY_KEY` secret (the private half of a write deploy key on the tap). The workflow runs only for a pushed tag or a published release. The site, which serves the install script, deploys with `cd site && wrangler deploy`.
