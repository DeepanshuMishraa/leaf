# leaf

leaf is a terminal reader (PDF, EPUB, Markdown, HTML, Office files, pictures, web articles), a diff and pull request reviewer, a whiteboard, and a set of tools agents use to hand work to a person. In Rex on macOS it also opens a WebKit browser and an iOS Simulator in a pane. It's one Rust binary, `leaf` (crate `leaf-reader`, edition 2024), with native macOS helpers written in Swift and a Lua plugin for Rex.

This is a Rust project. Don't use Bun, Node or npm for it. The only JavaScript is the website in `site/`.

## Where things are

- `src/main.rs`: the command line. Each subcommand dispatches from here, and the top-level usage text lives here.
- `src/*.rs`: one module per concept, flat. The biggest are the views: `viewer.rs` (reader), `diffview.rs`, `prview.rs`, `boardview.rs`, `planview.rs`. Each view's keys are in a `*keys.rs` beside it.
- `src/config.rs` and `src/keymap.rs`: `config.toml`, and the key tables every view can rebind.
- `src/*.swift`, `src/browser/`, `src/duo-hinge.m`: the macOS helpers. `build.rs` compiles them (see below), and `src/helper.rs` unpacks them at run time.
- `src/simulator.rs`, `src/simsnap.rs`, `src/simmodels.rs`: `leaf sim`. `src/simagent.swift` is the helper side of agent driving.
- `lua/leaf/init.lua`: Rex's command palette actions. `src/plugin.rs` embeds and installs it.
- `skills/*/SKILL.md`: the skills leaf installs for coding agents. `src/skills.rs` embeds them.
- `completions/_leaf`, `completions/leaf.fish`: shell completions, written by hand.
- `tests/`: integration tests. Most tests are unit tests inside each module.
- `site/`: leaf.dipxsy.app, a Cloudflare Worker with static files. It's excluded from the crate.
- `vendor/`: third-party files leaf embeds (React Grab, syntax highlight queries). Don't edit them.

## Checking a change

```sh
cargo fmt
cargo clippy --all-targets   # add no new warnings; a few old ones exist
cargo test
```

CI runs only `cargo build --locked` on pull requests. `cargo test --locked` runs at release time, so a broken test surfaces late. Run the tests yourself.

Swift and Objective-C:

- `build.rs` compiles the helpers with `xcrun swiftc`. Locally, a failed compile is only a cargo warning ("…failed to compile; it will be unavailable in this build"), and leaf then ships without that pane. In CI it fails the build. After editing a `.swift` file, look for that warning, or typecheck directly:

  ```sh
  xcrun swiftc -swift-version 5 -target arm64-apple-macos13.0 -parse-as-library -typecheck src/pane.swift src/simulator.swift src/simagent.swift
  xcrun swiftc -swift-version 5 -target arm64-apple-macos13.0 -typecheck src/browser.swift src/browser/main.swift
  ```

- A new Swift file must be added to its helper's `sources` in `build.rs`.

Lua: check syntax with `luac -p lua/leaf/init.lua`.

leaf builds for macOS (arm64, x86_64) and Linux (x86_64, arm64). Code that only works on macOS must still compile elsewhere: gate it with `cfg` and say so at run time, as `leaf sim` does.

## How the code is written

- **Modules** start with a `//!` comment that says what the module is and how it fits with the rest. Keep one concept per module. Put logic on the module's main type, not in a utils file.
- **Comments** explain why, or what isn't obvious from the code. Don't narrate the code.
- **Tests** live in `#[cfg(test)] mod tests` at the bottom of the module. Name them as sentences about behavior, such as `refs_survive_the_next_snapshot_and_new_elements_get_new_numbers`. Add or change a test with each behavior change. A test that fails because of a real bug stays failing; don't bend it to pass.
- **Errors** in a domain get a `thiserror` enum. Anything that can fail at run time returns an error with a message, not a panic.
- **Messages to the person** start with `leaf: ` on stderr. Each says what happened, why if known, what was left untouched, and what to do next, naming the exact command. For example: "@e3 (button "General") isn't on the screen now. Run `leaf sim snapshot` to see what is."
- **Platform calls**: private Apple frameworks are looked up by name at run time, and a missing one becomes a message, not a crash. Never block the AppKit main thread on Rex, `simctl` or a lock. Do that work off it (`Rex.background`, a global queue) and come back on the main queue.

## Writing

The README, CHANGELOG, skills, messages and comments share one voice:

- Plain, concrete words and short sentences, in the present tense.
- Say what something does, not how it feels. No marketing words.
- No em dashes. Use a period or a comma.
- Name keys, commands, flags and settings in backticks: `leaf sim snapshot`, `shift+cmd+a`, `[simulator] enabled`.

Match the surrounding text before adding to it.

## When you add…

- **A subcommand:**
  1. Add it to the dispatch and usage text in `main.rs`, or in its module's `USAGE`.
  2. Add it to both completion files.
  3. Add it to the README.
  4. If agents use it, add it to the matching skill.
  5. Add a CHANGELOG entry.
- **A key:** add it to the view's `actions!` table, with its default keys and what it does. `leaf config keys`, the help screens and `config.toml` pick it up from there.
- **A setting:**
  1. Add a field with a serde default in `config.rs`.
  2. Validate it in `config::load`, so `leaf config check` reports a bad value.
  3. Document it in the README's Configuration section.
- **A skill:**
  1. Write `skills/<name>/SKILL.md`, and list it in `SKILLS` in `src/skills.rs` and in the skill names in `src/hook.rs`.
  2. Make sure the front matter is valid YAML. A `: ` inside the description breaks it.
  3. Know that installed copies carry a stamp, so an edit reaches people's agents with the next release.
- **A Rex palette action** goes in `lua/leaf/init.lua`. When the Lua needs to know something leaf decides (a setting, a list), have it ask `leaf` through `rex.exec`, as `leaf config --simulator` does. Don't reimplement Rust logic in Lua.
- **A dependency:** add it with `cargo add` and keep default features off where the crate allows. Check that it builds on Linux too.

## Trying things by hand

- `cargo run -- <args>` runs leaf, for example `cargo run -- examples/sample.epub`.
- The browser and simulator panes need Rex. Without it, `leaf sim` still drives any booted simulator headless:

  ```sh
  xcrun simctl boot <udid>
  xcrun simctl bootstatus <udid> -b
  ```

  Wait about 30 seconds more before the first `leaf sim snapshot`. Reading the screen while iOS boots can leave its accessibility stuck until a reboot.
- Shut down any simulator you booted. Put back any setting you changed in it.

## Changelog and releases

- Every change a person would notice gets a bullet under `## Unreleased` in `CHANGELOG.md`.
  - Describe the behavior from their side, with keys and commands in backticks and the PR number, such as `(#87)`.
  - leaf shows these notes in its "What's new" dialog (`src/news.rs`).
  - A test requires a section for the current version.
- Release only when asked; the steps are in the README's Development section.
- The site deploys with `wrangler deploy` from `site/`. It's production, so ask first.

## Git

- Commit subjects are `type(scope): Capitalized imperative summary`.
  - Types: `feat`, `fix`, `docs`, `perf`, `chore`.
  - The scope is the area: `sim`, `config`, `outline`, `browser`, `plan`.
  - Use bullets in the body when a commit does several things.
  - Releases are `chore(release): Release X.Y.Z`.
- Work on a feature branch and open pull requests against `main`.
- Review bots (cubic, CodeRabbit) comment on pull requests.
  - Check each finding against the code. Fix the valid ones in a commit named like `fix(sim): Answer CodeRabbit's review`.
  - Reply with a reason on any you skip.
