# leaf

Read PDFs, Markdown, office files and web articles in the terminal, review diffs and pull requests, sketch on a whiteboard, and review your coding agent's plans.

## Install

```sh
curl -fsSL https://leaf.dipxsy.app | sh
```

Installs a prebuilt `leaf` to `~/.local/bin` for macOS 11+ or Linux (glibc 2.35+), x86_64 or arm64. The download is checked against its SHA-256. `LEAF_INSTALL_DIR` and `LEAF_VERSION` override the location and release. Linux needs the ALSA sound library (`libasound2t64` or `libasound2`).

Homebrew:

```sh
brew install deepanshumishraa/tap/leaf
```

From source (Linux also needs `build-essential pkg-config libclang-dev libasound2-dev libfontconfig1`; macOS needs `xcode-select --install` for the browser and the simulator pane, and Xcode's iOS Simulator SDK for the iPhone Duo's hinge):

```sh
cargo install leaf-reader
```

`leaf upgrade` updates in place (Homebrew installs are left to `brew upgrade`). Shell completions are in `completions/` (fish: `leaf.fish`, zsh: `_leaf`).

## Reading

```sh
leaf open paper.pdf
leaf open notes.md
leaf open talk.pptx
leaf open https://example.com/post
leaf                       # pick a recent or nearby document
```

Supports PDF, EPUB, Markdown (GitHub extensions, wikilinks, mermaid), saved HTML, pictures, Word, PowerPoint, Excel/ODS and CSV/TSV. Links to PDFs, arXiv papers and raw Markdown open as such; other pages go through Readability.

- `←/→` page, `j/k` scroll, `g/G` first/last, `f` find, `?` all keys, `q` quit
- Drag to select and copy; `y` highlights, `d` draws, `@` asks your agent about the selection
- `a` reads aloud with Microsoft Edge's online voices; `s` picks a voice
- `<`/`>` narrow or widen the column, `i` toggles painted images

Spreadsheets and CSV files open in a grid of cells, with column letters, row numbers and the first row pinned as the header. Move with the arrows or `hjkl`, or click a cell. Drag, or hold shift, to select a range, and the bottom line shows its count, sum and average. Click a column letter or row number to select the whole column or row. `/` finds, `:` goes to a cell such as `B12`, `s` sorts by the column, `F` shows only rows whose cell in the column holds some text, `y` copies the selection as tab-separated text, `[`/`]` switch sheets and `<`/`>` resize the column. The cell bar at the top shows a cell's whole text, and its formula in Excel files. Sorting and filtering change only the view.

CSV and TSV files can be edited. `e` (or `f2`) edits the cell and `enter` keeps it, `x` empties the selected cells, `o`/`O` add a row below or above, `D` deletes the selected rows, `I` adds a column and `X` deletes the selected columns. `u` undoes and `U` redoes. Nothing is written until `ctrl+s` saves, which keeps the file's delimiter, line endings and quoting; quitting with unsaved edits asks first. Excel and OpenDocument files are only read.

Images use the Kitty, iTerm2 or Sixel protocol, falling back to half blocks. Reading place, highlights and drawings are saved in `~/.local/share/leaf/leaf.db`. Local files reload when they change.

### Configuration

Optional, at `$XDG_CONFIG_HOME/leaf/config.toml` (default `~/.config/leaf/config.toml`):

```toml
[reader]
max_width = 100   # reading column width, default 76
colorful_markdown = true   # terminal palette colors for heading levels and links, default false
hide_link_urls = true   # hide URLs printed after clickable link labels, default false

[diff]
layout = "inline"   # "split" or "inline"; left out, a diff opens in the layout last used

[outline]
width = 40   # the outline sidebar's width when it opens, 20 through 80, default 32

[plan]
jump = "top"   # where ], [ and other block jumps in leaf plan leave the block: "top" of the screen, or "nearest" (default), scrolling the least
scrolloff = 3   # rows kept below the cursor as it moves down in leaf plan, at most half the screen, default 0

# Rebind keys. Each line names an action and one key or a list of them.
# Actions you leave out keep their keys; an empty list unbinds one.
[keys.reader]
quit = ["q", "ctrl+q"]
scroll_down = ["j", "down", "ctrl+e"]
pen = []

[keys.diff]
next_file = ["]", "n", "tab"]

[keys.board]
tool_box = "R"
```

Every view has its own key table: `reader`, `ink`, `bookmarks`, `outline`, `diff`, `diff_comments`, `diff_overview`, `confirm_post`, `verdict`, `plan`, `plan_notes`, `plan_quit`, `board`, `pr_list`, `pr` and `sheet`. `leaf config keys` prints them all with every action, its default keys and what it does, ready to copy. Text you type, such as comments, find and filters, keeps its own keys.

Keys are written as `q`, `G`, `ctrl+d`, `alt+enter`, `shift+tab`, `space`, `enter`, `esc`, `tab`, `up`, `pagedown`, `home`, `f1` and so on. A key an action didn't have before is taken from any other action that had it, so each key does one thing; restating an action's defaults changes nothing. A reader color can't share a key with a `[keys.ink]` action, since ink keys come first while a stroke is picked. `?` lists the keys as you set them.

`leaf config check` reads the file as leaf would. A file that doesn't read stops the reader, `leaf diff`, `leaf review`, `leaf plan`, `leaf board` and `leaf pr` with a message naming the line. If it breaks while `leaf pr` is open, opening the Code tab shows the error instead and `leaf pr` stays as it was. A misspelled action lists the actions its table has, and a misspelled key says how to write one. `leaf config path` prints where the file is.

## Diffs

```sh
leaf diff                  # unstaged changes
leaf diff --staged
leaf diff --branch         # this branch against its base
leaf diff main...HEAD
leaf diff changes.patch
leaf diff https://github.com/owner/repo/pull/12
```

Side-by-side by default; `i` switches to one column. `c` comments on a line or range, `ctrl+enter` sends it to your agent, `ctrl+g` posts it to GitHub. GitLab merge requests work too (via `glab` or `GITLAB_TOKEN`). `leaf comments --json` exports saved comments.

## Pull requests and issues

```sh
leaf pr                    # your open PRs and issues
leaf pr 12
leaf pr owner/repo#12
```

Tabs for summary, timeline, comments, code and checks. Merge, approve, comment, close and re-run jobs from the terminal. Needs `gh` signed in.

## Whiteboard

```sh
leaf board [file.excalidraw]
```

An Excalidraw-compatible board in the terminal. `@` asks your agent, which draws by editing the file; its changes appear live. `E` exports SVG and PNG. In Rex, **Leaf: Open Whiteboard** in the command palette starts one in the pane's folder.

## Margin: review your agent's plans and changes

```sh
leaf agent setup           # install skills and plan hooks for agents found here
leaf plan plan.md
leaf review --branch
```

Your agent hands you a plan or a diff, you leave notes on lines, blocks or files, and `S` sends one review back as its next message. Hooks are installed for Claude Code, Codex, Cursor, Grok Build, Antigravity and Pi; OpenCode uses the skills. When an agent opens a review it appears in a pane beside it (needs Rex, tmux or Herdr).

## Rex

```sh
leaf rex install
```

Adds Leaf to Rex's command palette (`cmd+shift+p`): open documents, review diffs, browse PRs, pick an agent and control reading aloud. Remote sessions work too.

**Open Browser** (or `leaf browser [url]`) opens a WebKit browser in a pane, macOS 13+. Its start page lists local dev servers. Press the pointer button (`shift+cmd+c`) to pick an element with [React Grab](https://github.com/aidenybai/react-grab) and ask your agent about it.

**Open Simulator…** (or `leaf sim open <udid>`) opens an installed iPhone simulator on a 3D iPhone in a pane, macOS 13+ with Xcode. The 3D iPhones (about 11 MB) aren't part of leaf: run `leaf sim install` (or **Install Simulator…** in the palette) once, which lists them, asks, downloads them and turns the simulator on. The iPhone Duo runs its own iOS and folds and opens with `cmd+f`. Your agent drives it, or any booted iPhone simulator, without screenshots: `leaf sim snapshot` lists the screen's elements from iOS accessibility, each with a ref, and `leaf sim tap @e3`, `fill`, `scroll`, `wait` and `launch` act on them (see `leaf sim help` and the `leaf-sim` skill).

### With Amber

With [Amber](https://github.com/rawnly/amber) installed, add Leaf to your Rex config instead:

```lua
local amber = dofile(os.getenv("HOME") .. "/.local/share/rex/amber/amber.lua")

amber.setup({
  plugins = {
    {
      "DeepanshuMishraa/leaf",
      main = "leaf",
      -- Installs the leaf release matching this checkout, like the install script above.
      build = [[LEAF_VERSION=$(awk -F'"' '/^version/ {print $2; exit}' Cargo.toml) sh site/public/install.sh]],
    },
  },
})
```

Run `amber install`: it clones the plugin, installs the matching `leaf` command and reloads Rex. `amber update` keeps both in step. Loading the config alone does not install.

- Already ran `leaf rex install`? Run `leaf rex uninstall` first, or the palette lists Leaf twice.
- Installed leaf with Homebrew? Leave out `build` and update it with `brew upgrade`.
- leaf not on the default path (`~/.local/bin`, `~/.cargo/bin`, Homebrew)? Pass its location: `opts = { bin = { "/path/to/leaf" } }`.

## Development

```sh
cargo test
cargo clippy
```

To release: bump `version` in `Cargo.toml`, `package.nix` and the `ver` badge in `site/public/index.html`, add notes to `CHANGELOG.md` (the tests require them), run `cargo check`, commit, and push a matching tag (`git tag v0.12.0 && git push origin v0.12.0`). The workflow builds all four targets, publishes the release and updates the Homebrew tap. The site deploys with `cd site && wrangler deploy`.
