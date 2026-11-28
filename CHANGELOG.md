# Changelog

## Unreleased

- `leaf open` reads any text file: source code, scripts, config files and logs open as code with line numbers, colored by the language their extension or name points to, such as `.py`, `.bat`, `.rs`, `Makefile` or `Dockerfile` (#74, #89). Long files are split into pages of 1,000 lines. A binary file gets a message saying so instead of noise.
- `leaf open` reads reStructuredText (`.rst`) as a formatted document, the way it reads markdown (#74, #90). Section titles, lists, literal and `code-block` blocks with their colors, notes and warnings, images, grid and simple tables, field lists, links and inline markup all show. Notes, tips and warnings get the same colored labels as GitHub alerts in markdown. Directives leaf doesn't know, such as `toctree`, are left out.
- Spreadsheets and CSV files (`.xlsx`, `.xls`, `.ods`, `.csv`, `.tsv` and the rest) open in a grid of cells instead of pages of tables. Column letters and row numbers frame it, the first row stays pinned as the header, and numbers line up on the right. Move with the arrows or `hjkl`, or click a cell; drag or hold shift to select a range, and the bottom line shows its count, sum and average. Clicking a column letter or row number selects the whole column or row. `/` finds text, `:` goes to a cell such as `B12`, `s` sorts by the column, `F` filters rows by the column, `y` copies the selection as tab-separated text, `[` and `]` switch sheets, and `<` and `>` resize a column. The cell bar shows a cell's whole text and, in Excel files, its formula. `[keys.sheet]` rebinds the keys (#91).
- CSV and TSV files can be edited in the grid. `e` or `f2` edits a cell, `x` empties the selection, `o` and `O` add rows, `D` deletes rows, `I` and `X` add and delete columns, and `u` and `U` undo and redo. `ctrl+s` saves, keeping the file's delimiter, line endings and byte order mark, and quoting only the fields that need it. Quitting with unsaved edits asks first. Excel and OpenDocument files stay read-only (#92).
- `@` in the spreadsheet grid asks your agent about the selected cells. The agent gets the file's path, the cell range, the header row and the selected cells as tab-separated text, then your question. A selection over 200 rows sends its first 200 (#93).

## 0.19.2

- Clicks on the simulator's screen land where you click. Before, every click on the iPhone and iPhone 16 models went to the spot mirrored top to bottom, so a tap on one control hit another, and clicks on the iPhone Air and 17 Pro models stopped at the cover glass and did nothing. Swipes from the side edges were the only touches that worked.

## 0.19.1

- `leaf sim install` now swaps **Leaf: Install Simulator…** for **Leaf: Open Simulator…** in Rex's palette right away. Before, the palette kept the old action until Rex's config was reloaded by hand, and picking it failed with "defines no action".

## 0.19.0

- `[plan] jump = "top"` makes `]`, `[` and the other block jumps in `leaf plan` scroll the block they land on to the top of the screen. The default, `"nearest"`, keeps scrolling only as far as it takes to show the block's first line.
- `[plan] scrolloff = 3` keeps that many rows below the cursor as it moves down in `leaf plan`, so the lines ahead stay in view. Like Vim's `scrolloff`, but only at the bottom edge, so a block a jump put at the top stays there. At most half the screen is kept; the default is 0.
- `leaf sponsors` shows everyone who sponsors leaf, biggest backers first: the newest sponsor in a spotlight and everyone else in a grid of round faces, drawn with Kitty graphics where the terminal supports them and half-block pixels elsewhere. Piped, it prints a plain list.
- `leaf browser` lines up with its pane when Rex puts the tab bar at the bottom (#87, #88). Tiled and floating panes used to sit a tab bar's height too high, and floating panes now keep the title area Rex leaves above them.
- **Leaf: Open Simulator…** in Rex's palette, or `leaf sim open <udid>` in a Rex pane, shows an installed iPhone simulator's live screen on a 3D iPhone, on macOS 13 or later with Xcode (#60). Click and drag on the screen to touch it, and type to type on it. Drag beside the phone to turn it, pinch to zoom, and press `cmd+0` to face it front. `shift+cmd+h` is Home and `cmd+l` locks it. A simulator leaf booted shuts down when its pane closes. `leaf sim list` shows the installed iPhones.
- `leaf sim install` downloads the 3D iPhones the pane draws on (about 11 MB, kept out of leaf itself). It lists them, asks first, checks each download, and turns on `[simulator] enabled` in `config.toml`. Until then the palette offers **Leaf: Install Simulator…** instead. The iPhone Air, 17 Pro and Pro Max, 16 and 16 Plus get their own models, and other iPhones share one.
- The iPhone Duo is drawn on a foldable model. `cmd+f` folds and opens it, and iOS moves between the cover screen and the wide inner one as it does on the device.
- `shift+cmd+a` asks your agent about the simulator's screen. Drag to mark part of it, type a question and press `enter`: leaf sends the question with a screenshot, the device and the marked region.
- Agents drive the simulator without screenshots. `leaf sim snapshot` prints the screen's elements from iOS accessibility, one short line each with a ref such as `@e3`. `-i` keeps only what can be acted on, and `--diff` prints only what changed. `leaf sim tap @e3`, `fill @e3 "text"`, `type`, `scroll down`, `wait "Welcome"`, `key cmd+a`, `button home`, `apps` and `launch Settings` act on it, and a snapshot waits for the screen to settle first. These work on any booted iPhone simulator, in a Leaf pane or not, except touches on the iPhone Duo, which need its pane.
- `leaf agent setup` installs a `leaf-sim` skill, so an agent knows how to drive the simulator.

## 0.18.4

- Rebind any view's keys in `config.toml`: the reader, its ink and bookmarks, the outline, the diff and its comments, overview and verdict, `leaf plan`, `leaf board` and `leaf pr`, each under its own `[keys.<view>]` table. Each line names an action and a key or a list of keys, such as `quit = ["q", "ctrl+q"]`. Actions left out keep their keys, an empty list unbinds one, and a new key given to one action is taken from the others. `?` and the hints at the bottom of each view show the keys as you set them. Text you type, such as comments, find and filters, keeps its own keys. The defaults are the keys leaf had before.
- `leaf config keys` prints every key table with each action, its default keys and what it does. `leaf config check` reads `config.toml` as leaf would, and `leaf config path` says where it is.
- A `config.toml` that doesn't read now stops `leaf diff`, `leaf review`, `leaf plan`, `leaf board` and `leaf pr` as it already stopped the reader, with a message naming the table, action and key. If the file breaks while `leaf pr` is open, the Code tab says so and stays closed.
- `[diff] layout = "split"` or `"inline"` sets the layout a diff opens in, over the one last used. `[outline] width` sets the outline sidebar's width when it opens.
- `leaf agent setup` installs a `leaf-config` skill, so an agent can rebind keys and change settings for you and check the file afterwards.
- A review that an agent in tmux opened beside itself delivers its verdict to the agent's pane again when tmux's session environment carries Rex's variables (#85). It used to fail with `block target "%N" not found` and copy the text to the clipboard instead.
- `t` opens an outline beside the text and `T` opens it over the text, in the reader and the diff viewer (#78). Either key closes what it opened or switches to the other form. Go to page moves from `t` to `:`.
- In the reader, the outline lists a PDF's bookmarks or an EPUB's table of contents, and the headings on the page when the document has none (#59). The marked entry follows the text as it scrolls, and moving in the outline scrolls the text to that entry.
- In a diff, the outline lists the files as a tree of folders. Each file shows its status, lines added and removed, and whether it's viewed (`✓`) or has a note (`●`). Moving to a file opens it with its name and first hunk at the top of the screen.
- In the outline, `j`/`k` and `g`/`G` move, `h`/`l` fold and open groups, and `/` filters by the typed characters in order, ignoring case. `tab` and `shift+tab` step between matches, `enter` goes to one, and `esc` cancels. `q` or `esc` closes the outline.
- `<` and `>` narrow or widen the outline sidebar by 4 columns, from 20 to 80, and dragging its right edge resizes it too. The width lasts the session.
- Opening the outline beside a paged document switches it to scroll mode and keeps the text you were reading at the top. When the terminal is too narrow for the sidebar, `t` says so instead of opening and closing it.
- Filtering the outline of a very large diff is fast: on 40,000 files a frame takes about 2.7 ms with a filter typed, in a release build.

## 0.18.3

- Rex's command palette has **Leaf: Open Whiteboard**, which starts a new board in the active pane's folder, beside it when the pane is busy.
- `ctrl+w` deletes the word before the cursor in `leaf plan` comments and in `leaf diff` file notes, like `alt+backspace`.
- In the diff viewer, `h` folds the file under the cursor down to its heading and `l` opens it again. Marking a file viewed with `w` folds it, unmarking opens it, and files already marked viewed open folded. A viewed file's heading is muted and says `✓ viewed`, whether it's folded or open, so a file you only folded out of the way still looks unread. A folded file that changes on reload opens again, and jumping to a file from the overview, the file picker or a comment opens it. Scrolling sideways moves to `H`/`L` (`←`/`→` still work outside a pull request's Code tab).

## 0.18.2

- `leaf browser` no longer flickers when focus moves between Rex panes, including between two browser panes: the page stays above Rex while Rex or another leaf pane is in front, and drops back to normal stacking when another app comes forward. A browser pane that another app's window overlaps, even a floating one, stays behind that window when another leaf pane is activated.
- After Rex's palette or a dialog closes over the browser, or Rex comes back to the front, the browser waits for the pane layout to settle before taking the keyboard, so it no longer takes focus from the pane you clicked.
- In `leaf browser`, unmodified keys (and characters typed with ⌥) typed into the address bar or a text field on the page stay there, even when Rex binds them; shortcuts with ⌘ or ⌃ still reach Rex. Bindings inside a Rex key mode are no longer forwarded. Typing `n` no longer switches tabs, and `enter` navigates again.
- Starting a multi-key Rex shortcut in `leaf browser` and then clicking into the address bar or a page field drops the shortcut, so the next letter is typed instead of completing or being swallowed by it.
- The browser lines up with its pane in Rex's compact density again: it no longer sits 1pt low and overflows the pane on the right.
- leaf finds Rex again in panes from Rex server preview.23 on, which set only `REX_BLOCK`, not `REX_SESSION`. Asking the agent, opening beside you, the browser, and Rex's theme and status all work there again.

## 0.18.1

- **Leaf: Minimal Address Bar** in Rex's command palette hides the browser's address bar until the pointer is at the top of the page. `cmd+l` brings it back. The page fills the pane the rest of the time.
- `leaf sponsor` opens leaf's sponsor page in the browser. Sponsor once, any amount, and your name and avatar go on leaf.dipxsy.app.

## 0.18.0

- On macOS 13 and later, **Leaf: Open Browser** opens a WebKit browser in a Rex pane beside you (`leaf browser [url]` opens it in the current pane instead, until it closes), built into leaf: nothing is installed in Applications. It follows the pane through splits, resizes, zoom and tab switches, steps aside for Rex's palette and Mission Control, and keeps Rex's shortcuts working. It has find, zoom and downloads, is light or dark like Rex, and the pane's label follows the page title. Opened without an address, it starts on a quiet page that lists the web servers running on this Mac.
- Point at any part of a page and ask your agent about it. The pointer button (or `shift+cmd+c`) picks elements with [React Grab](https://github.com/aidenybai/react-grab), including the React components and source files behind them on development builds; type a question and press `enter`. It goes to the agent picked with **Leaf: Choose Agent…**, in its pane if one is open. If not, leaf starts claude, codex, opencode, gemini, cursor-agent, grok, pi, qwen or copilot in a new pane beside the browser; other agents need their pane open first. Building leaf from source needs Apple's Swift toolchain for the browser.
- Read saved web pages: `leaf page.html` opens a local HTML file as a readable document, with its inline images. SVG images render sharp at the width they're shown.
- Posting a review comment to GitHub from the diff viewer no longer leaves the "posting" dialog stuck on screen after it succeeds.

## 0.17.3

- Zooming the board with a trackpad is smoother: each scroll step zooms less, and the sideways part of a zoom swipe no longer pans the board while it zooms. `option+↑` and `option+↓` zoom too.
- Drawing and zooming on the board keep up better in kitty-protocol terminals. Each frame goes to the terminal as compressed pixels, which terminals take in several times faster than PNG, and moving the mouse over the board without clicking no longer redraws it.
- Open Word documents (`.docx`), slide decks (`.pptx`) and spreadsheets (`.xlsx`, `.xls`, `.ods`, `.csv`, `.tsv`). leaf reads them itself on macOS and Linux, with no LibreOffice, Keynote or Numbers. Decks and documents show as designed, in their own fonts, colors and layout, and spreadsheets as a grid; `+`/`-` zoom, `o` switches to the terminal's font, and `i` paints the designed page in the terminal's colors. Decks show one slide per page with speaker notes, Word documents keep their pages, and `.xlsx`/`.xlsm` workbooks keep their cell styles, column widths and row heights.
- Configure the document reader's maximum width in `$XDG_CONFIG_HOME/leaf/config.toml` (default `~/.config/leaf/config.toml`). Omitted settings keep the 76-column layout.
- Keep the reading position within PDF and EPUB pages when resizing in scroll mode.
- `<` and `>` narrow or widen the reading column, and `=` fits it to the screen, so wide diagrams can be read without quitting.
- Diagrams and images no longer vanish after the terminal or pane is resized several times in quick succession.

## 0.17.2

- `leaf board` turns the terminal into a whiteboard you share with your agent. Draw boxes, diamonds, ellipses, arrows, lines, ink, text and frames; move, resize, turn, group and restyle them; and ask the agent to draw with `@`. Boards are plain `.excalidraw` files that also open on excalidraw.com, saved on every change and redrawn when the agent or anyone else edits the file. With no file named, a new board gets a fresh name like `board-misty-harbor.excalidraw`. `E` or `--export out.png` writes SVG or PNG.
- The board's header names the tool in hand and shows the ink, background, fill, line and width new things get, each with the key that changes it. `tab` and `shift+tab` step through the tools. `L` locks the canvas so it can't be panned or zoomed.
- Stroke and background colors are set separately: `1`–`6` set the stroke, and `B` steps the background through none, gray and the six colors. Both apply to the selection and to what you draw next, including frames.
- Arrows and lines let go near a shape snap onto its edge, so a connector drawn a little short still meets the shapes at both ends. Arrows stay attached as the shapes move.
- A selected shape can be dragged while a box, diamond, ellipse or frame tool is still active.
- `ctrl+c` quits the board even with something selected; `y` copies.
- In kitty-protocol terminals, the board picture no longer blinks while you draw. The old picture stays until the new one has had time to show, even when the new one is slow to upload, and a picture shown twice keeps both copies.
- If a new board can't be fully written, its half-written file is removed instead of being left behind.
- `leaf diff`, `leaf review` and the PR Code tab remember whether you last used the split or inline layout.
- `leaf pr` lists pull requests and issues assigned to you. `@` switches the list between All and Assigned.
- A finished job's log that GitHub failed to send (such as HTTP 503) is asked for again every few seconds while you're on it, instead of showing the error until you reopen it.
- `leaf pr` no longer posts an empty comment, or a reply that's only the quoted comment; the composer says why and stays open.
- Asking an agent about a selection sends the exact selected text, even beyond character 400 of a paragraph. Surrounding lines remain capped. Selections are fenced and marked as quoted document data, not instructions.

## 0.17.1

- Closing a review nobody asked for no longer pastes "closed without a verdict" into an unrelated agent pane; the notice goes only to the pane named with `--reply-to`.

## 0.17.0

- Step through pictures in a folder with the arrow keys, wrapping around at either end.

After an update, leaf shows the current version's notes once, over whatever
it opens first. Each release needs a `## <version>` section with one line
per change, starting with `- `.

## 0.16.4

- `d` and `u` scroll half a page in `leaf diff`, as the keys panel says; `ctrl+d` and `ctrl+u` still work.
- Scrolling back up in a diff keeps the first file's name on screen instead of stopping at its first line, and a file's name stays pinned at the top while its lines scroll under it.

## 0.16.3

- GitLab merge request patches use `glab` when available, then the API when `GITLAB_TOKEN` is set, and the page otherwise. Commit and comparison patches use the API when a token is set. A truncated or failed API patch no longer replaces the page patch.

## 0.16.2

- GitLab merge request, commit and comparison links open in `leaf diff`, on gitlab.com and on self-hosted instances, including ones whose host doesn't say `gitlab`: `GITLAB_HOST` names those, for opening a repository's remotes and for the `GITLAB_TOKEN` a private project needs, and never overrides a host that names itself GitHub. A merge request's patch is asked of `glab` first when it's installed; a commit's and a comparison's have no `glab` command, so all three are asked of the API when `GITLAB_TOKEN` is set (GitLab's page routes refuse a token), of the page itself otherwise, and of the page again when the API fails; both a token and `glab` go only to a host that names itself GitLab or that `GITLAB_HOST` names. Comments are still posted to GitHub only.
- Plans and local documents reload in place after file changes settle. Refresh waits while you write or select text.
- When an annotated plan block changes or has duplicate matches, Leaf keeps the reviewed snapshot until you send or clear its notes.
- Saved notes follow moved blocks when you reopen the plan.
- Live plan updates do not repeat startup notices.
- Saved review rounds follow the plan version you reviewed.
- Sending a review cannot overwrite an earlier round when history recording fails.
- Failed file loads wait five seconds before retrying unless the file changes again.
- PDF and EPUB replacements load in the background while you keep reading.
- Saved PDF and EPUB progress follows the replacement's page count.
- Diffs and change reviews refresh every second for local sources and every 10 seconds for GitHub, including a PR's Code tab.
- Unchanged diffs keep your position and notes.
- If your current diff line or file disappears, background refresh keeps your position nearby instead of jumping to the beginning.
- Changed annotated files keep their reviewed snapshot until you clear their line comments. You cannot post comments from stale snapshots to GitHub.
- Background diff reloads stay quiet. Manual reloads confirm unchanged content.
- Visible PR and issue pages and lists refresh every 10 seconds. Refresh keeps the selected item and waits for comments or text selections to finish; running job logs retain their five-second polling.
- Editing a comment on GitHub no longer changes which comment Leaf replies to.

## 0.16.1

- `leaf agent setup` installs Leaf's skills and plan hooks together, shows the planned changes, and reports each file written. Separate skill install and agent status/uninstall commands have been removed; use `leaf agent skill <name>` to print a skill.

## 0.16.0

- Review your agent's plans: `leaf plan plan.md` shows a plan block by block and line by line. `V` selects lines, `c` comments on them or on the cursor line, `B` the whole block, `x`/`r` remove or replace the selection or the block, `C` adds a note on the whole plan, and `S` sends one review with a verdict.
- Review your agent's changes: `leaf review` starts from an overview of every changed file. `V` selects lines to comment on with `c`, `F` notes a whole file, `M` the whole change, `w` marks a file viewed, and `S` sends one review.
- Agents can ask for a review themselves. In Rex, tmux or Herdr the review opens beside the agent, and your verdict comes back to it as its next message.
- `leaf hook install` sets up Claude Code, Codex, Antigravity, Grok Build, Cursor and Pi to hand you their plans before they build; OpenCode gets the skills, so it runs `leaf plan` and `leaf review` itself.
- `leaf skills install` teaches agents to use leaf.
- A resubmitted plan marks the blocks that changed since you last reviewed it, and `D` steps through them.
- `leaf diff` has the overview (`tab`), file and whole-change notes, and `S` to send everything as one review.
- Asking an agent about a selection (`@`) works in tmux and Herdr as well as Rex.
- `leaf pr` has two tabs, pull requests and issues, each one list newest first; a colored dot and the legend say whose it is — green yours, blue yours in someone else's repository, yellow someone else's in yours, grey the rest.
- Issues open in leaf now: read the summary and timeline, comment, reply to a comment quoting it, open the issue on GitHub, close it as completed or not planned, and reopen it.
- Pull requests and issues have a Comments tab: every comment and review in full, `j`/`k` to pick one, `enter` to reply quoting it, `c` to comment. The picked comment is marked with a blue `▶` and rail, there and in the Timeline.
- `R` in the `leaf pr` list reloads both tabs now rather than waiting.
- `tab` in the `leaf pr` list shows merged pull requests and closed issues, grouped by when they ended, with a twelve-week sparkline; `←/→` switch between pull requests and issues.
- Open a picture file on its own: leaf reads PNG, JPEG, GIF, WebP, SVG, HEIC and more, turns photos upright by their EXIF orientation, and `i` switches it between its own colors and the theme's.
- Pull requests have a Checks tab: every check by workflow with its duration, any Actions job's steps and log, and GitHub's re-runs: failed jobs, one job, or all jobs. The page refreshes while checks run.
- A markdown file that changes on disk is read again in place while you have it open.

## 0.15.2

- A calmer home screen: a greeting, today's reading time, the search and your documents, without the heat map, meters or fills.
- Recent documents in the folder you started in no longer repeat its path.

## 0.15.1

- The dithered look is now off by default. Turn it on with `leaf ui dither`, or "Leaf: Dither UI" in Rex's palette.
- After an update, leaf shows what changed once, like this.
- `leaf upgrade` and `leaf rex install` show each step as it runs, with download progress.

## 0.15.0

- A home screen: run `leaf` with nothing named to see today's reading time, your streak, a heat map and recent documents.
- leaf follows Rex theme switches at once.
- Each theme dithers in its own accent colors.
- The pull request header can be selected and copied like the body.
