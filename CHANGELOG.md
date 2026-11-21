# Changelog

## 0.18.0

- On macOS 13 and later, **Leaf: Open Browser** opens a WebKit browser in a Rex pane beside you (`leaf browser [url]` opens it in the current pane instead, until it closes), built into leaf: nothing is installed in Applications. It follows the pane through splits, resizes, zoom and tab switches, steps aside for Rex's palette and Mission Control, and keeps Rex's shortcuts working. It has find, zoom and downloads, is light or dark like Rex, and the pane's label follows the page title.
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
