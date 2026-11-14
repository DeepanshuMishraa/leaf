# Changelog

After an update, leaf shows the current version's notes once, over whatever
it opens first. Each release needs a `## <version>` section with one line
per change, starting with `- `.

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
