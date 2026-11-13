# Changelog

After an update, leaf shows the current version's notes once, over whatever
it opens first. Each release needs a `## <version>` section with one line
per change, starting with `- `.

## 0.16.0

- Review your agent's plans: `leaf plan plan.md` shows a plan block by block. Comment on, remove or replace a block or a passage, add a note on the whole plan, then send one review with a verdict.
- Review your agent's changes: `leaf review` starts from an overview of every changed file. Note lines, a whole file or the whole change, mark files viewed, and send one review.
- Agents can ask for a review themselves. In Rex, tmux or Herdr the review opens beside the agent, and your verdict comes back to it as its next message.
- `leaf hook install` sets up Claude Code, Codex, Antigravity, Grok Build, Cursor, Pi and OpenCode to hand you their plans before they build.
- `leaf skills install` teaches agents to use leaf.
- A resubmitted plan marks the blocks that changed since you last reviewed it, and `D` steps through them.
- `leaf diff` has the overview (`tab`), file and whole-change notes, and `S` to send everything as one review.
- Asking an agent about a selection (`@`) works in tmux and Herdr as well as Rex.

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
