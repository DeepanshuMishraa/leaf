---
name: leaf-config
description: Change leaf's settings and key bindings in its config.toml for the person. Use when they want a key rebound or unbound in leaf (the reader, leaf diff, leaf review, leaf plan, leaf board or leaf pr), ask what a key does or which keys are free, want the diff to open split or inline, want the outline sidebar wider, want leaf plan's block jumps to put the block at the top of the screen or keep rows below the cursor (scrolloff), or when leaf stopped with "Could not load settings".
---

# leaf-config: settings and keys

leaf reads one file, `config.toml`, when a view starts. It writes the file only for `leaf sim install`, which sets `[simulator] enabled = true` and leaves the rest as it was. Everything else you edit by hand. Changes apply the next time the person opens leaf. A view that's already open keeps the keys it started with.

## Find out first

```bash
leaf config path    # where the file is: $XDG_CONFIG_HOME/leaf/config.toml, else ~/.config/leaf/config.toml
leaf config keys    # every key table, every action, its default keys and what it does
leaf config check   # reads the file as leaf would; exits 1 and names the problem when it can't
```

Run `leaf config keys` before you name an action. The names come from leaf itself, so don't guess them from memory. If the file exists, read it before you edit and keep what the person already set.

## The file

```toml
[reader]
max_width = 100            # reading column, default 76
colorful_markdown = true   # heading and link colors from the terminal palette, default false
hide_link_urls = true      # no URL after a link's label, default false

[diff]
layout = "inline"          # "split" or "inline"; left out, a diff opens in the layout last used

[outline]
width = 40                 # outline sidebar width when it opens, 20 through 80, default 32

[plan]
jump = "top"               # where ], [ and other block jumps in leaf plan leave the block: "top" of the screen, or "nearest" (default), scrolling the least
scrolloff = 3              # rows kept below the cursor as it moves down in leaf plan, at most half the screen, default 0

[keys.reader]
quit = ["q", "ctrl+q"]     # one key as a string, or a list
pen = []                   # an empty list unbinds the action

[keys.diff]
next_file = ["]", "n", "tab"]
```

Every table is optional, and so is every line in it. An action you leave out keeps its default keys. Unknown tables, settings or action names are errors, not ignored.

## Key tables

`leaf config keys` prints all of them. Here's where each one works:

| Table | Where |
|---|---|
| `keys.reader` | reading a document; also Esc and the 1–6 colors |
| `keys.ink` | the reader's move tool while a stroke is picked |
| `keys.bookmarks` | the reader's bookmark list (`'`) |
| `keys.outline` | the outline (`t`/`T`) in the reader and the diff |
| `keys.diff` | `leaf diff`, `leaf review`, a pull request's Code tab |
| `keys.diff_comments` | the diff's comment list (`m`) |
| `keys.diff_overview` | the diff's overview of files (`tab`) |
| `keys.confirm_post` | "Post this comment to GitHub?" |
| `keys.verdict` | picking a verdict (`S`), in the diff and in `leaf plan` |
| `keys.plan`, `keys.plan_notes`, `keys.plan_quit` | `leaf plan`, its note list, its "quit with notes unsent?" |
| `keys.board` | `leaf board` |
| `keys.pr_list`, `keys.pr` | `leaf pr`'s list, and a pull request or issue |

Text you type keeps its own keys and can't be rebound. That covers comment and note boxes, find, go to page, the ask box, the voice picker, the outline's filter, the list filters in `leaf pr` and the file picker, board text, and the merge, close and re-run menus. Letters type there.

## Writing keys

- A character as itself: `"q"`, `"G"`, `"?"`, `"+"`, `"<"`. Case matters, so `"G"` is shift+g. `"shift+g"` means the same thing.
- Named keys: `space`, `enter`, `esc`, `tab`, `backtab` (or `shift+tab`), `backspace`, `delete`, `insert`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `f1` to `f12`.
- Modifiers go before the key with `+`: `ctrl+d`, `alt+enter`, `ctrl+shift+z` (same as `ctrl+Z`). Use `ctrl`, `alt` (or `option`) and `shift`. `cmd` isn't one: the terminal doesn't pass it to leaf.
- `ctrl++` is ctrl with plus.

## How bindings combine

- Setting an action replaces that action's keys. The rest of the table stays at its defaults.
- When you give an action a key it didn't have, any other action that had that key by default loses it. That way each key does one thing. Keys an action already had stay where they were, so restating `next_match = "n"` leaves `n` on `next_page` too.
- A reader color (`color_1` to `color_6`) can't share a key with a `keys.ink` action: with a stroke picked, ink keys are heard first.
- Naming the same new key for two actions in one table is an error. Some keys are shared by default and that's fine: in the reader `n` finds the next match while a search has one on the page and turns the page otherwise. Restating defaults never changes anything.
- A key with modifiers wins over the plain key, so `ctrl+d` and `d` can do different things. A plain key also answers with ctrl or alt held when nothing else claims that combination.
- Tables are separate. Rebinding `quit` in `keys.reader` doesn't touch `keys.diff`.
- `?` in each view lists the keys as configured, and hints at the bottom follow them too.

## Doing it

1. Run `leaf config path` and read the file if it exists.
2. Run `leaf config keys` and find the table and action. If the person names a key rather than an action, search the output for that key in the right table.
3. Check what the new key does now in that table. If another action has it, say which action loses it before you edit.
4. Edit only the lines you need, under the right `[keys.<table>]` header. Create the file and its folder if they don't exist.
5. Run `leaf config check`. If it fails, fix the line it names and run it again.
6. Tell the person what changed, which key the other action lost if any, and that it applies the next time they open leaf.

## When leaf won't start

`Could not load settings in <path>` means the file didn't read, and leaf opens nothing until it does. If the file breaks while `leaf pr` is already open, its Code tab shows the error and stays closed. The message names the table, action and key, for example ``keys.reader.quit: `ctl+q` is not a key: `ctl` is not a modifier; use ctrl, alt or shift``. A misspelled action lists the actions the table has. Fix that line, or delete it to go back to the default, then run `leaf config check`.
