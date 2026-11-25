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

From source (Linux also needs `build-essential pkg-config libclang-dev libasound2-dev libfontconfig1`; macOS needs `xcode-select --install` for the browser):

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

Images use the Kitty, iTerm2 or Sixel protocol, falling back to half blocks. Reading place, highlights and drawings are saved in `~/.local/share/leaf/leaf.db`. Local files reload when they change.

### Configuration

Optional, at `$XDG_CONFIG_HOME/leaf/config.toml` (default `~/.config/leaf/config.toml`):

```toml
[reader]
max_width = 100   # reading column width, default 76
```

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

An Excalidraw-compatible board in the terminal. `@` asks your agent, which draws by editing the file; its changes appear live. `E` exports SVG and PNG.

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

### With Amber

With [Amber](https://github.com/rawnly/amber) installed, add Leaf to your Rex config instead:

```lua
local amber = dofile(os.getenv("HOME") .. "/.local/share/rex/amber/amber.lua")

amber.setup({
  plugins = {
    { "DeepanshuMishraa/leaf", main = "leaf" },
  },
})
```

Run `amber install` to install it and reload Rex; loading the config alone does not install.

## Development

```sh
cargo test
cargo clippy
```

To release: bump `version` in `Cargo.toml`, `package.nix` and the `ver` badge in `site/public/index.html`, add notes to `CHANGELOG.md` (the tests require them), run `cargo check`, commit, and push a matching tag (`git tag v0.12.0 && git push origin v0.12.0`). The workflow builds all four targets, publishes the release and updates the Homebrew tap. The site deploys with `cd site && wrangler deploy`.
