-- leaf for Rex: read documents in a pane, ask an agent pane about a
-- selection, review diffs, and read aloud in a voice picked from the palette.
--
-- `leaf rex install` loads this from Rex's config (~/.config/rex/init.lua):
--
--   dofile("/path/to/plugins/leaf/init.lua").setup({ bin = { "/path/to/leaf" } })
--
-- Rex runs these actions on this machine even for a session on a remote
-- host; their pane and block calls go to the host. So a pane opened there
-- runs leaf there, installing it with cargo first when the host has none,
-- and commands reach a reader by being typed into its pane: F12, one line,
-- enter. Nothing here touches the files of the machine a document is on.

local M = {}

local SHELL = "com.superlogical.terminal.shell"
local TERMINAL = "com.superlogical.terminal"
local RECORD = "leaf/reader"
local WAKE = "\27[24~"

local DEFAULT_PATH = table.concat({
  (os.getenv("HOME") or "") .. "/.local/bin",
  (os.getenv("HOME") or "") .. "/.cargo/bin",
  "/opt/homebrew/bin",
  "/usr/local/bin",
  "/usr/bin",
  "/bin",
}, ":")

-- Runs leaf on the host, installing it with cargo when it isn't there.
local BOOTSTRAP = [[
leaf=$(command -v leaf 2>/dev/null || true)
[ -n "$leaf" ] || leaf="$HOME/.cargo/bin/leaf"
if [ ! -x "$leaf" ]; then
  cargo=$(command -v cargo 2>/dev/null || echo "$HOME/.cargo/bin/cargo")
  if [ ! -x "$cargo" ]; then
    printf 'leaf isn'"'"'t on %s, and cargo isn'"'"'t either.\n' "$(uname -n)"
    printf 'Install Rust from https://rustup.rs, then: cargo install leaf-reader\n'
    printf 'Press enter to close.'; read -r _; exit 1
  fi
  printf 'leaf isn'"'"'t on %s yet. Installing it with cargo install leaf-reader…\n' "$(uname -n)"
  if ! "$cargo" install leaf-reader; then
    printf '\nleaf couldn'"'"'t be built here. The output above says why.\n'
    printf 'Press enter to close.'; read -r _; exit 1
  fi
fi
exec "$leaf" "$@"
]]

function M.setup(opts)
  opts = opts or {}
  local bin = opts.bin or { "leaf" }
  if type(bin) == "string" then bin = { bin } end
  local env = { PATH = opts.path or DEFAULT_PATH }

  local function argv(...)
    local out = {}
    for _, part in ipairs(bin) do out[#out + 1] = part end
    for _, part in ipairs({ ... }) do out[#out + 1] = part end
    return out
  end

  -- The command for a pane: this machine's leaf, or the host's.
  local function pane_command(ctx, ...)
    if ctx.server == nil then return argv(...) end
    local out = { "sh", "-c", BOOTSTRAP, "leaf" }
    for _, part in ipairs({ ... }) do out[#out + 1] = part end
    return out
  end

  -- Runs leaf on this machine and returns what it printed.
  local function run(extra, stdin, ...)
    local command = argv(...)
    local merged = {}
    for key, value in pairs(env) do merged[key] = value end
    for key, value in pairs(extra or {}) do merged[key] = value end
    local result, err = rex.exec(command, { env = merged, stdin = stdin, timeout = 20 })
    local shown = table.concat(command, " ")
    if result == nil then
      error("leaf: couldn't start `" .. shown .. "`: " .. tostring(err)
        .. ". Install leaf with `cargo install leaf-reader`, then run `leaf rex install`.", 0)
    end
    if result.timed_out then
      error("leaf: `" .. shown .. "` timed out after 20s.", 0)
    end
    if not result.ok then
      error("leaf: `" .. shown .. "` exited " .. tostring(result.code) .. ":\n"
        .. (result.stdout or "") .. (result.stderr or ""), 0)
    end
    return result.stdout or ""
  end

  local function file_path(url)
    if type(url) ~= "string" then return nil end
    local path = url:match("^file://[^/]*(/.*)$")
    if path == nil then return nil end
    return (path:gsub("%%(%x%x)", function(hex)
      return string.char(tonumber(hex, 16))
    end))
  end

  local function cwd(ctx)
    if ctx.block_id == nil then return nil end
    local pwd = rex.block.call(TERMINAL, "pwd", { block_id = ctx.block_id })
    if type(pwd) ~= "table" then return nil end
    return file_path(pwd.pwd)
  end

  local function open_pane(ctx, label, command, dir)
    local options = { command = command }
    if dir ~= nil and dir ~= "" then options.cwd = dir end
    local layout = { block = { flavor = SHELL, label = label, options = options } }
    if ctx.block_id == nil then
      local _, err = rex.session.new_window{ label = label, layout = layout, focus = true }
      if err ~= nil then error("leaf: couldn't open a tab: " .. tostring(err), 0) end
      return
    end
    local _, err = rex.session.new_split{
      anchor_id = ctx.block_id,
      direction = "horizontal",
      side = "after",
      focus = true,
      layout = layout,
    }
    if err ~= nil then error("leaf: couldn't open a pane: " .. tostring(err), 0) end
  end

  -- Readers -----------------------------------------------------------------

  -- A pane is a reader while the record leaf left is owned by the process in front.
  local function reader_in(block_id)
    local status = rex.block.call(TERMINAL, "program_status", { block_id = block_id })
    if type(status) ~= "table" or type(status.records) ~= "table" then return nil end
    local record
    for _, r in ipairs(status.records) do
      if r.id == RECORD then record = r end
    end
    if record == nil then return nil end
    local info = rex.block.call(TERMINAL, "process", { block_id = block_id })
    local front = type(info) == "table" and info.foreground or nil
    if type(front) ~= "table" or front.name ~= "leaf" then return nil end
    local owner = record.owner
    if type(owner) == "table" and owner.pid ~= front.pid then return nil end
    return { block_id = block_id }
  end

  -- The focused pane when it is a reader, or else the first reader in the session.
  local function find_reader(ctx)
    if ctx.block_id ~= nil then
      local here = reader_in(ctx.block_id)
      if here ~= nil then return here end
    end
    local listed = rex.session.list_blocks{ session_id = ctx.session_id }
    if type(listed) ~= "table" or type(listed.blocks) ~= "table" then return nil end
    for _, block in ipairs(listed.blocks) do
      if block.block_id ~= nil and block.block_id ~= ctx.block_id then
        local found = reader_in(block.block_id)
        if found ~= nil then return found end
      end
    end
    return nil
  end

  local NO_READER = "leaf: no document is open in a Leaf pane in this session."
    .. " Open one with Leaf: Open Document, then try again."

  local function tell(reader, line)
    local _, err = rex.block.call(TERMINAL, "write", {
      block_id = reader.block_id,
      data = WAKE .. line:gsub("[%c]", " ") .. "\r",
    })
    if err ~= nil then
      error("leaf: couldn't reach the Leaf pane (" .. tostring(err) .. "). The document is unchanged.", 0)
    end
  end

  local function focus(block_id)
    rex.session.focus_block{ block_id = block_id }
  end

  -- Opening -----------------------------------------------------------------

  rex.action{
    name = "leaf_open",
    title = "Leaf: Open Document…",
    description = "Pick a PDF, EPUB or markdown file under this folder, or a document read lately,"
      .. " and read it in a pane beside you. Give target to open one directly.",
    category = "Leaf",
    keywords = "leaf pdf markdown epub read document article book file picker",
    args = { target = "string?" },
    run = function(ctx, args)
      local target = args.target or ""
      local dir = cwd(ctx)
      if target == "" then
        open_pane(ctx, "leaf", pane_command(ctx, "open"), dir)
        return
      end
      if target:match("^https?://") == nil then
        local parent = target:match("^(.*)/[^/]+$")
        if parent ~= nil and parent ~= "" and target:sub(1, 1) == "/" then dir = parent end
      end
      open_pane(ctx, target:match("([^/]+)$") or "leaf", pane_command(ctx, "open", target), dir)
    end,
  }

  rex.action{
    name = "leaf_diff",
    title = "Leaf: Review Diff",
    description = "Review the working tree, a revision, a patch, or a GitHub link in a pane beside you."
      .. " Comments go to the agent pane with ctrl+enter.",
    category = "Leaf",
    keywords = "leaf diff review pull request patch git changes",
    args = { target = "string?" },
    run = function(ctx, args)
      local target = args.target
      local command = (target ~= nil and target ~= "") and pane_command(ctx, "diff", target)
        or pane_command(ctx, "diff")
      open_pane(ctx, "diff", command, cwd(ctx))
    end,
  }

  rex.action{
    name = "leaf_review_staged",
    title = "Leaf: Review Staged Changes",
    description = "Review what's staged for the next commit in a pane beside you.",
    category = "Leaf",
    keywords = "leaf diff staged index commit review",
    run = function(ctx)
      open_pane(ctx, "staged", pane_command(ctx, "diff", "--staged"), cwd(ctx))
    end,
  }

  rex.action{
    name = "leaf_review_branch",
    title = "Leaf: Review This Branch",
    description = "Review this branch against its base branch in a pane beside you.",
    category = "Leaf",
    keywords = "leaf diff branch pull request pr review",
    run = function(ctx)
      open_pane(ctx, "branch", pane_command(ctx, "diff", "--branch"), cwd(ctx))
    end,
  }

  -- Asking --------------------------------------------------------------------

  local function selection(ctx)
    if ctx.block_id == nil then return "" end
    local clip = rex.block.call(TERMINAL, "clipboard_read", { block_id = ctx.block_id })
    if type(clip) ~= "table" then return "" end
    return clip.data or ""
  end

  local function block_label(ctx)
    if ctx.block_id == nil then return "this document" end
    local info = rex.session.describe_block{ block_id = ctx.block_id }
    if type(info) ~= "table" then return "this document" end
    local label = (info.block or info).label
    if type(label) == "string" and label ~= "" and label ~= "shell" then return label end
    return "this document"
  end

  rex.action{
    name = "leaf_ask",
    title = "Leaf: Ask Agent About Selection",
    description = "Send the text selected in a Leaf document, with the file, the page, the line numbers"
      .. " and the lines around it, to the agent pane open in this session. Without question,"
      .. " the reader asks you to type one.",
    category = "Leaf",
    keywords = "leaf agent claude codex grok selection ask quote explain",
    args = { question = "string?" },
    run = function(ctx, args)
      local question = args.question or ""
      local reader = find_reader(ctx)
      if reader ~= nil then
        tell(reader, "ask " .. question)
        if question == "" then focus(reader.block_id) end
        return
      end
      -- No reader: the clipboard, with the context the last reader saved.
      if ctx.server ~= nil then error(NO_READER, 0) end
      local text = selection(ctx)
      if text:gsub("%s", "") == "" then
        error("leaf: select some text in a Leaf document first.", 0)
      end
      local output = run({ REX_SESSION = ctx.session_id or "", REX_BLOCK = ctx.block_id or "" },
        text, "ask", "--title", block_label(ctx), "--question", question)
      local block = output:match("LEAF_AGENT_BLOCK=(%S+)")
      if block ~= nil and block:sub(1, 6) == "block:" then focus(block) end
      return { output = (output:gsub("\n?LEAF_AGENT_BLOCK=%S+", "")) }
    end,
  }

  -- Reading aloud -------------------------------------------------------------

  local function reader_action(spec)
    rex.action{
      name = spec.name,
      title = spec.title,
      description = spec.description .. " Needs a document open in a Leaf pane.",
      category = "Leaf",
      keywords = "leaf read aloud speech tts voice " .. (spec.keywords or ""),
      run = function(ctx)
        local reader = find_reader(ctx)
        if reader == nil then error(NO_READER, 0) end
        tell(reader, spec.command)
        if spec.focus then focus(reader.block_id) end
      end,
    }
  end

  reader_action{ name = "leaf_play", title = "Leaf: Read Aloud / Pause", command = "play",
    keywords = "play pause resume listen",
    description = "Read the document aloud from the selection or the top of the screen; run again to pause or carry on." }
  reader_action{ name = "leaf_stop", title = "Leaf: Stop Reading Aloud", command = "stop",
    keywords = "stop silence quiet", description = "Stop reading aloud, remembering where." }
  reader_action{ name = "leaf_faster", title = "Leaf: Read Faster", command = "faster",
    keywords = "speed rate", description = "Read aloud a step faster." }
  reader_action{ name = "leaf_slower", title = "Leaf: Read Slower", command = "slower",
    keywords = "speed rate", description = "Read aloud a step slower." }
  reader_action{ name = "leaf_next_sentence", title = "Leaf: Next Sentence", command = "next",
    keywords = "skip forward", description = "Skip ahead a sentence while reading aloud." }
  reader_action{ name = "leaf_previous_sentence", title = "Leaf: Previous Sentence", command = "previous",
    keywords = "back rewind", description = "Go back a sentence while reading aloud." }
  reader_action{ name = "leaf_voices", title = "Leaf: Search Every Voice…", command = "voices", focus = true,
    keywords = "language accent picker",
    description = "Open the reader's search over every voice Microsoft offers, in every language." }

  -- With no reader open, a voice becomes the one leaf reads in next time.
  local function use_voice(ctx, name)
    local reader = find_reader(ctx)
    if reader ~= nil then
      tell(reader, "voice " .. name)
      return
    end
    if ctx.server ~= nil then error(NO_READER, 0) end
    run(nil, nil, "voice", name)
    return { output = "Leaf will read in " .. name .. " from now on." }
  end

  rex.action{
    name = "leaf_voice",
    title = "Leaf: Use Voice by Name",
    description = "Read in any Edge voice, such as fr-FR-DeniseNeural. With no document open, it becomes the default.",
    category = "Leaf",
    keywords = "leaf voice speech tts language",
    args = { voice = "string" },
    run = function(ctx, args)
      local name = args.voice or ""
      if not name:match("^[%w%-]+$") or not name:find("-", 1, true) then
        error("leaf: name a voice like en-GB-SoniaNeural. Leaf: Search Every Voice… lists them all.", 0)
      end
      return use_voice(ctx, name)
    end,
  }

  for _, voice in ipairs(opts.voices or {}) do
    rex.action{
      name = "leaf_voice_" .. (voice.name:lower():gsub("[^%w]", "_")),
      title = "Leaf: Voice — " .. voice.short,
      description = "Read aloud in " .. voice.short .. " (" .. voice.about .. ")."
        .. " With no document open, it becomes the default.",
      category = "Leaf",
      keywords = "leaf voice speech tts " .. voice.name .. " " .. voice.about,
      run = function(ctx) return use_voice(ctx, voice.name) end,
    }
  end

  for short, key in pairs(opts.keys or {}) do
    rex.bind(key, "leaf_" .. short)
  end
end

return M
