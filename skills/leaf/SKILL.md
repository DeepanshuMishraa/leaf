---
name: leaf
description: Use leaf, a terminal reader and code reviewer, to show the person documents, diffs and pull requests, read the comments they leave, and hand plans or changes to them for review. Use when the person mentions leaf, asks to read or show a PDF, Markdown file, article, diff or pull request in the terminal, or asks for their review.
---

# leaf

leaf reads PDFs, EPUBs, Markdown files and web articles in the terminal, and reviews diffs and GitHub pull requests. The person reads in leaf. You start it, hand it files, and read what they leave behind.

## Before you run it

leaf is a full-screen terminal app. Your shell tool has no terminal, so a bare `leaf open …` or `leaf diff` run by you exits at once or shows nothing. There are three ways to put something in front of the person:

1. **Review commands.** `leaf plan` and `leaf review` (see the `leaf-plan` and `leaf-review` skills) open in a pane beside you when you run inside Rex, tmux or Herdr, and the person's review comes back to you as your next message.
2. **Any other screen.** Open it in a pane beside you: `tmux split-window -h 'leaf open notes.md'` in tmux, `rex split --split=right -- leaf open notes.md` in Rex.
3. **Anywhere else.** Tell the person the exact command to run, for example `leaf diff --branch`.

`leaf version` prints the installed version. `leaf --help` prints every command.

## Commands

| Command | What it opens |
|---|---|
| `leaf open <file or url>`, or just `leaf <file>` | A PDF, EPUB or Markdown file, or a web article. It reopens where the person left off. |
| `leaf diff` | Unstaged changes. `--staged`, `--branch` (this branch against its base), `--base <ref>`, a range such as `main...HEAD`, a path, a `.patch` file or a GitHub PR, commit or compare link. `--inline` uses one column. |
| `leaf pr` | The person's open pull requests and issues. `leaf pr 12`, `leaf pr owner/repo#12` or a PR link opens one, with Summary, Timeline, Comments and Code tabs. In the list, `←/→` switch pull requests and issues, `tab` switches open and merged/closed, `R` reloads. |
| `leaf plan <plan.md>` | Plan review, block by block. |
| `leaf review [diff source]` | Code review, file by file. |
| `leaf comments --json` | Every saved diff comment as JSON: key, file, side, line range, body, and state (`draft`, `agent`, `github`). |
| `leaf agents` | Picks which agent CLI the person's questions go to. |
| `leaf rex install` | Adds leaf to Rex's command palette. |
| `leaf hook install` | Wires plan review into every agent CLI installed here. |
| `leaf skills install` | Installs these skills for every agent here. |

## Reading what the person left

- Diff comments: `leaf comments --json`. A comment in state `draft` hasn't been sent to anyone yet.
- In the reader, the person can select text and press `@`, or in a diff press `C` and then `ctrl+enter`. Either way the question is pasted into your pane, with the file, the line numbers and the text around it. Treat it as their message.
- A review from `leaf plan` or `leaf review` arrives as one numbered message headed `# Review of …`. Address every numbered item.

## Keys, so you can tell the person what to press

**Reader** (documents):
- `j/k` scroll, `space/b` a screen, `←/→` page, `g/G` first or last, `t` go to page, `f` find (`n/N` next and previous).
- Drag to select and copy. `@` asks the agent about the selection. `y` highlights it, and `1`–`6` pick the color.
- `d` pen, `e` eraser, `v` moves ink.
- `a` reads aloud, `s` picks a voice.
- `m` bookmarks the page, `'` lists bookmarks.
- `?` lists every key, `q` quits.

**Diff:**
- `j/k` move, `[`/`]` files, `f` file picker, `tab` overview of every file.
- `V` or `v` selects lines, `c` comments on the line or selection, `F` note on the file, `M` global note.
- `m` lists comments, `w` marks a file viewed, `S` sends everything as one review.
- `@` asks the agent about the lines, `ctrl+enter` sends one comment now.
- `i` switches split and inline, `o` the other side, `r` reloads, `?` keys.

**Pull request:**
- `tab` or `1`–`4` switch Summary, Timeline, Comments and Code.
- On Comments, `j/k` pick a comment and `enter` replies quoting it.
- `m` merges, `a` approves, `c` comments, `x` closes or reopens, `r` toggles draft.
- `o` opens it on GitHub, `?` keys, `q` goes back to the list.

**Plan review:**
- `j/k` lines, `]`/`[` blocks, `V` selects lines, `c` comments on the line or selection, `B` on the whole block.
- `x` remove, `r` replace (the selection, else the block), `C` global note.
- `L` lists notes, `S` sends with a verdict.

## Things to know

- Progress, highlights, comments and unsent review notes live in SQLite at `~/.local/share/leaf/leaf.db`.
- leaf finds your pane through `REX_BLOCK`. `LEAF_AGENT_PANE` overrides which pane questions go to.
- Never write into leaf's data files by hand.
