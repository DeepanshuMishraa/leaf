# Margin: review an agent's plans and diffs in leaf

Margin is leaf's review loop for coding agents. An agent hands leaf a plan or a set of changes. You read it in the terminal, leave notes, and send one review back. The agent gets it as its next message, in the same conversation, and keeps working.

People can start a review too. `leaf plan` and `leaf review` work the same whether an agent opened them or you did.

## Goals

1. Agents can drive it. A skill tells the agent what to run, the command returns at once, and the review arrives as the agent's next message.
2. One review per round. Notes collect as drafts and go out together with a verdict, not one message per comment.
3. The answer goes back to the agent session that asked. In Rex, leaf knows the asking pane (`REX_BLOCK`), so it never starts a fresh agent.
4. It all happens in the terminal: no browser, no local server, no open port, and it works over SSH through Rex.
5. One setup command finds every agent CLI installed on the machine and wires Margin into each.
6. Plans are reviewed block by block and diffs file by file, with a global note on both, all from the keyboard.

Out of scope for the first cut: team sharing, hosted links, GitLab and jj.

## What ships

| Piece | Command | What it does |
|---|---|---|
| Navigation skill | `leaf skills install` | Teaches an agent what leaf is, how to open documents, diffs and PRs, and every key. |
| Plan review | `leaf plan <file.md>` | Splits the plan into blocks. You note blocks, passages or the whole plan, then send a verdict. |
| Code review | `leaf review [diff args]` | An overview of every changed file, then file by file: notes on lines, on a file, or global, then a verdict. |
| Agent setup | `leaf hook install` | Finds every installed agent CLI and installs the plan-mode hook, the skills or the commands each one supports, in one go. |
| Hook entry point | `leaf hook <agent>` | Called by the agent's hook. It opens Margin, waits, and answers in that agent's format. |
| Loop skills | `leaf-plan`, `leaf-review` | Tell the agent when and how to hand work to Margin. |
| Plan rounds | automatic | Each submission of the same plan is kept. A later round opens on what changed since the last one. |

## How a round runs

### Agent opens it

```
agent pane (claude, REX_BLOCK=b1)          new pane beside b1
---------------------------------          ----------------------------------
$ leaf plan docs/plans/auth.md
  stdout is not a TTY and REX_SESSION
  is set, so leaf splits a pane:     -->   leaf plan --reply-to b1 docs/plans/auth.md
  prints "Opened for review beside          you read, add notes, press S,
  you. The review arrives as your           pick a verdict
  next message." and exits 0
agent ends its turn
                                     <--   leaf pastes the review into b1 and
next user message = the review              presses enter, then closes its pane
```

- `--reply-to <block>` sends the answer to the asking pane. Without it, leaf uses the lookup it already has (`agent::deliver`): `LEAF_AGENT_PANE`, then the focused agent pane, then one in the same window.
- `--wait` blocks instead. The child pane writes the result to `$XDG_STATE_HOME/leaf/margin/<id>.json`. The waiting process prints the review to stdout and exits. Hooks use this, and so can any agent without Rex panes that's able to wait on a command.
- Outside Rex with no TTY, leaf can't open a pane. It exits 2 and tells the agent to ask the person to run `leaf plan <file>` themselves.

### A person opens it

Run `leaf plan plan.md` in a terminal and it opens in place. The review goes to the agent pane found by the usual lookup. With no agent pane, it's copied to the clipboard (OSC 52), as asks are today. `--print` writes it to stdout instead.

## Agent setup: `leaf hook install`

### Detection

`leaf hook install` checks for each agent and gathers the results before writing anything:

- It looks for the binary with `agent::installed()`, the same search `leaf agents` uses (PATH plus the folders installers use).
- It looks for the agent's config directory: `~/.claude`, `~/.codex`, `~/.gemini`, `~/.config/opencode`, `~/.copilot`, `~/.pi`, `~/.factory`, `~/.config/amp`. This catches an agent that's configured but not on PATH right now.

It then prints one table of what it will change for each agent, with a diff of every config file it touches, and asks once. `--yes` skips the question, so agents can run it. `--dry-run` prints the table only. `leaf hook uninstall` removes exactly what was added. `leaf hook status` lists each agent's state: installed, out of date, absent, or not supported.

### Adapters

Each agent has an adapter in `src/hook/<agent>.rs` behind one small trait:

```rust
pub trait Adapter {
    fn name(&self) -> &'static str;
    /// Finds the agent and its config. None when it isn't here.
    fn detect(&self, home: &Path) -> Option<Found>;
    /// The edits that install Margin, as file changes to show and apply.
    fn plan(&self, found: &Found) -> Result<Vec<FileChange>, SetupError>;
    /// Reads the hook's stdin and returns the plan text and session id.
    fn parse(&self, stdin: &str) -> Result<HookInput, HookError>;
    /// Writes the outcome back in the agent's own format.
    fn answer(&self, outcome: &Outcome) -> HookAnswer;
}
```

What each adapter installs:

| Agent | Plan-mode hook | Skills | Notes |
|---|---|---|---|
| Claude Code | `PermissionRequest` matcher `ExitPlanMode` in `~/.claude/settings.json` | `~/.claude/skills/leaf*` | Full loop. |
| Codex | `Stop` hook in `~/.codex/config.toml`, where the installed version supports hooks | `~/.agents/skills/leaf*` | If hooks aren't supported, it gets skills only and the table says so. |
| Gemini CLI | `BeforeTool` hook on its plan-exit tool in `~/.gemini/settings.json` | `~/.gemini/commands/leaf-*.toml` | Requires a version with hooks. |
| OpenCode | plugin file in `~/.config/opencode/plugin/leaf.ts` that runs `leaf hook opencode` before plan exit | `~/.agents/skills` | |
| Copilot CLI | plan-mode hook in its config, where supported | skills | |
| Pi | extension hook where supported | skills | |
| Others found (amp, droid, cursor-agent, grok, aider…) | none | `~/.agents/skills` | Run `leaf plan` and `leaf review` through the skills. |

Rules for every adapter:

- It merges into existing config and never replaces it. Config files are parsed and written back with their other keys untouched. A file leaf can't parse is left alone, and the table names it, why it was skipped, and how to fix it.
- Leaf's entries carry a marker (`leaf-margin`), so reinstalling updates them in place and uninstalling finds them.
- Every exact hook schema is checked against the agent's current docs before its adapter lands. Claude Code's comes first because its schema is known. Other adapters land one at a time, and until an adapter has been checked, that agent gets skills only.

### Claude Code hook in detail

```json
{ "hooks": { "PermissionRequest": [ { "matcher": "ExitPlanMode",
  "hooks": [ { "type": "command", "command": "leaf hook claude", "timeout": 86400 } ] } ] } }
```

`leaf hook claude` reads the hook JSON on stdin and takes `tool_input.plan`. It saves the plan as a round under `$XDG_STATE_HOME/leaf/margin/plans/<session_id>/<n>.md`, then runs the `--wait` flow. It answers with one of these:

- Approve: `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}`
- Approve with notes: `allow`, and the notes go to the agent pane as a follow-up message.
- Request changes: `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"<review>"}}}`
- Closed without a verdict: exit 0 with no output, so Claude Code asks as it normally does.

If leaf can't open a pane (not in Rex), the hook prints nothing, exits 0 and explains on stderr. The plan stays saved.

## Plan review: blocks

### Splitting the plan

`plan_blocks(source) -> Vec<Block>` splits the Markdown source into blocks, using the same parser leaf renders with (`markdown.rs`). Each of these is one block:

- a heading, together with the paragraphs under it up to the next heading or list
- each top-level list item, with its nested items
- a code fence, table, block quote or mermaid diagram
- a loose paragraph

```rust
pub struct Block {
    pub index: usize,             // 1-based, shown in the gutter
    pub lines: (usize, usize),    // source lines, 1-based inclusive
    pub heading: Vec<String>,     // the heading path, e.g. ["Phase 2", "Migrations"]
    pub kind: BlockKind,          // Section | Item | Code | Table | Quote | Diagram | Paragraph
    pub text: String,             // the block's source
}
```

### What the screen shows

- The plan renders as it does now, with a thin gutter that marks block boundaries and each block's number.
- The block under the cursor is tinted. Each block with notes shows a dot in its note color and a count.
- A **Global** row sits above block 1, for notes on the whole plan.

### Keys

Everything works from the keyboard. The mouse adds passage selection.

| Key | Action |
|---|---|
| `j/k` or `↓/↑` | next or previous block, scrolling to keep it in view |
| `J/K` | next or previous block with notes |
| `g/G` | Global row, last block |
| `c` | comment on the block under the cursor, or on the selected passage if there is one |
| `x` | mark the block, or the selected passage, for removal |
| `r` | replace the block or passage with typed text (it starts pre-filled with the current text) |
| `C` | write or edit the global note |
| `enter` | open the block's notes inline to read, edit (`e`) or delete (`x`) |
| `L` | list every note: `enter` jumps, `e` edits, `x` deletes |
| `S` | send: `a` approve, `n` approve with notes, `c` request changes, `esc` back |
| `D` | changes since the last round (round 2 and later) |
| `q` | quit. With unsent notes it asks first, and the notes stay saved. |

Reader keys that don't clash (`f` find, `a` read aloud, `?` keys, `@` ask) keep working. In review mode, `j/k` move by block instead of by line. Ctrl+d/u and space still scroll by screen.

## Code review: file by file

### Overview first

`leaf review` opens on an overview of the whole change before the first file:

```
 Review: feat/review-loop against main          12 files  +840 −112

 Global  Split the hook adapters into one file each.            1 note

  ✓ src/review.rs              +210        2 notes
  ● src/margin.rs              +188 −4     1 note
    src/viewer.rs              +96  −20
    src/diffview.rs            +71  −8
    ...
 enter open · C global note · c file note · S send · q quit
```

- Each row shows the file's status, its size, its note count, and whether it's been viewed. `✓` means viewed, `●` means it has notes.
- `enter` opens the file in the diff view as it is today, scrolled to its first hunk. `tab` jumps back to the overview from any file.
- `]`/`[` in a file go to the next or previous file, as they do today. Going past the last file returns to the overview.
- `V` marks a file viewed. Opening a file and scrolling to its end marks it too. If a file changes after a reload, its mark is cleared.

### Notes on three levels

| Level | Where | Key |
|---|---|---|
| Line or range | in a file, on the cursor line or a `v` selection | `C` (as today) |
| File | on the overview row, or anywhere in a file | `c` in the overview, `F` in a file |
| Global | overview, or anywhere | `G` |

`S` sends every draft line note, file note and the global note as one review, with a verdict, from any screen. Line notes that go out move to `State::Agent`. Plain `leaf diff` gets the overview and `S` too.

## Review format (what the agent receives)

Plan:

```markdown
# Review of docs/plans/auth.md (round 2): changes requested

Global: Split the migration into its own step before touching the API.

## 1. Block 4, "Phase 2 > Tokens" (lines 12-18): comment
> Rotate the token on every privilege change

That logs people out mid-session. Rotate on the next request instead.

## 2. Block 9, "Phase 4" (lines 30-34): remove

## 3. Line 41: replace
> use bcrypt
with:
> use argon2id (already a dependency)

Address every numbered item, then continue.
```

Code:

```markdown
# Review of feat/review-loop against main: changes requested

Global: Split the hook adapters into one file each.

## src/margin.rs
File: The result file should be removed on every exit path.

### 1. Lines 40-52 (new side): comment
```diff
<hunk around the range>
```
This leaks the file when the pane is closed.

Address every numbered item, then continue.
```

Items are grouped by file in diff order. The verdict wording is fixed: "Approved. Go ahead.", "Approved with notes. Go ahead and apply these.", "Changes requested. Revise, then submit again."

## Architecture

### New modules

```
src/review.rs        Review domain: Verdict, Note, Target, Edit, Review::render. Pure, fully tested.
src/planblocks.rs    Splits plan Markdown into Blocks; finds a quoted passage's source lines.
src/margin.rs        Running a round: open beside, in place or waiting; reply-to; result files; plan rounds on disk.
src/hook/mod.rs      Adapter trait, detection, the install table, applying and undoing FileChanges.
src/hook/claude.rs   One file per agent adapter (claude, codex, gemini, opencode, copilot, pi).
src/skills.rs        Skill files embedded with include_str!; installed through the adapters' skill folders.
skills/leaf/SKILL.md, skills/leaf-plan/SKILL.md, skills/leaf-review/SKILL.md
```

### Changed modules

- `main.rs` adds the `plan`, `review`, `hook` and `skills` subcommands and their usage text.
- `viewer.rs` gets an optional review mode (`ViewerOptions::review: Option<PlanReview>`). It adds block navigation, the gutter, the Global row, notes and the send dialog. Without review mode, the reader is unchanged.
- `diffview.rs` gets the overview screen, file and global notes, viewed marks, and `S`. The line comments that exist today become line notes.
- `agent.rs` gets `deliver_to(block, text)` and `split_beside(block, argv)`, factored out of `deliver_rex` and `start_rex`.
- `progress.rs` gets the `plan_notes`, `file_notes` and `viewed` tables, plus a `global_notes` table keyed by review key.
- `lua/leaf/init.lua` adds palette actions: **Review Plan…** and **Review Changes**.
- `README.md`, `completions/*` and `CHANGELOG.md` are updated.

### Domain types (`review.rs`)

```rust
pub enum Verdict { Approve, ApproveWithNotes, RequestChanges }

/// What a plan note asks for. Code notes are always comments.
pub enum PlanEdit { Comment(String), Remove, Replace(String) }

pub enum Target {
    Block { index: usize, heading: Vec<String>, lines: (usize, usize) },
    Passage { quote: String, block: usize, lines: Option<(usize, usize)> },
    File { path: String },
    Lines { anchor: comment::Anchor, hunk: String },
}

pub enum Note {
    Plan { target: PlanTarget, edit: PlanEdit },   // PlanTarget = Block | Passage
    Code { target: CodeTarget, body: String },     // CodeTarget = File | Lines
}

pub struct Review { subject: String, round: Option<u32>, global: Option<String>, notes: Vec<Note> }

pub enum Outcome { Sent { verdict: Verdict, text: String }, Closed }
```

- Remove and replace only exist on plan targets. The types make a "remove this file" note impossible to build.
- If you send Approve while notes exist, it's promoted to `ApproveWithNotes` when the review is built.
- Passage lines come from `planblocks::locate(source, quote)`, which ignores Markdown markup and whitespace. When it can't find the passage, it falls back to the enclosing block's lines.

### Storage

- `plan_notes(id, key, block, quote, kind, body, round)`, where `key` is the plan's real path. A half-reviewed plan reopens with its notes.
- `file_notes(id, key, path, body, state)` and `global_notes(key, body, state)`, where `key` is the diff key that already exists.
- `viewed(key, path, blob)`: a file's viewed mark, with its blob hash, which clears the mark when the content changes.
- Plan rounds: `$XDG_STATE_HOME/leaf/margin/plans/<key-hash>/<n>.md`, saved only when the content differs from the last round.
- Round results: `$XDG_STATE_HOME/leaf/margin/<id>.json` holds `{ outcome, verdict, text }`. The waiting process deletes it after reading.

## Delivery order (tracer bullet first)

1. **`review.rs` and `planblocks.rs` with tests.** Verdicts, notes, rendering, promoting approvals, splitting blocks, `locate`.
2. **Thin slice.** `leaf plan file.md` opens in place with blocks and `j/k`. `c` comments on a block, and `S` sends a verdict through `agent::deliver`. A viewer test runs from the first key press to the sent text.
3. **Agent-native.** Splitting a pane beside the agent when there's no TTY, `--reply-to`, the `--wait` result files, `deliver_to` and `split_beside`.
4. **The rest of plan review.** Remove and replace, passages from mouse selection, the Global row, the inline note view, the `L` list, the quit guard, and saved notes.
5. **Code review.** The overview screen, file notes, the global note, viewed marks, `S` with grouped rendering, and `leaf review`.
6. **Skills.** The three SKILL.md files, and `leaf skills install|uninstall|print`.
7. **Agent setup.** The adapter trait, detection, the install table, apply and undo, and the Claude Code adapter.
8. **More adapters.** Codex, Gemini CLI, OpenCode, Copilot and Pi, each after checking its current hook schema.
9. **Plan rounds.** Storing versions, and `D` for changes since the last round.
10. **Docs.** README sections, completions, palette actions and the changelog.

After each step: `cargo fmt`, `cargo clippy --all-targets` with no warnings, and `cargo test`.

## Tests

- `review.rs`: how each verdict renders, numbering, grouping by file, the global note, multi-line quotes, approvals with notes being promoted.
- `planblocks.rs`: blocks for headings, nested lists, fences, tables and front matter, and that the blocks cover every non-blank source line exactly once. A property test checks that `locate` finds any line copied verbatim from the source.
- Viewer review mode, through the existing `Screen` harness: `j j c` + text + `enter` + `S c` sends the exact review to a stub sink. `x` marks a removal. `C` sets the global note. `q` with unsent notes asks first.
- Diffview review mode: the overview lists every file with its counts. `enter` opens a file and `tab` returns. A line note, a file note and a global note plus `S` send one message, and the line notes end in `State::Agent`. A viewed mark clears when the file changes.
- `hook`: detection against a temporary HOME with fake agent folders. Each adapter's merge keeps unrelated keys. Running install twice gives the same files. Uninstall restores the original. An unparseable config is reported and left alone. Claude's decision JSON for each outcome.
- `margin.rs`: the result file round trip, and that `--wait` treats a closed result as no verdict.
- `skills.rs`: install writes the files, a file edited by hand is left alone, and uninstall removes only leaf's files.

## Risks and open questions

1. **Hook timeouts.** Agents kill a hook after its timeout. We set the longest each agent allows. If the hook is killed, the plan is still on disk.
2. **Other agents' hook schemas change often.** Each adapter is pinned to a checked schema, `leaf hook status` reports when one is out of date, and an agent whose adapter isn't checked gets skills only.
3. **Block splitting on unusual Markdown.** HTML blocks and deeply nested lists fall back to one block per top-level node. A block can't be smaller than one source line.
4. **Editing other tools' config files.** Every change is shown before it's written, a backup goes to `<file>.leaf-backup` the first time, and `leaf hook uninstall` undoes it.
