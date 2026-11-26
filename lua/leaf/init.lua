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
local DIFF_RECORD = "leaf/diff"
-- Asks the agent pane about the selection, in a document or a diff.
local ASK_KEY = "shift+cmd+a"
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

  local function file_path(url)
    if type(url) ~= "string" then return nil end
    local path = url:match("^file://[^/]*(/.*)$")
    if path == nil then return nil end
    return (path:gsub("%%(%x%x)", function(hex)
      return string.char(tonumber(hex, 16))
    end))
  end

  -- Where the program in front works, which is right for a reader too; or
  -- else what the shell last reported.
  local function cwd(ctx)
    if ctx.block_id == nil then return nil end
    local info = rex.block.call(TERMINAL, "process", { block_id = ctx.block_id })
    if type(info) == "table" and type(info.foreground) == "table"
      and type(info.foreground.cwd) == "string" and info.foreground.cwd ~= "" then
      return info.foreground.cwd
    end
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

  -- Runs leaf in the pane the palette was called from: typed at its prompt,
  -- or in place of the document it shows; beside it when it's busy. On a
  -- remote host, a new pane beside, which can install leaf there first.
  local function place(ctx, label, ...)
    if ctx.server ~= nil or ctx.block_id == nil then
      open_pane(ctx, label, pane_command(ctx, ...), cwd(ctx))
      return
    end
    -- Detached: closing a reader there reloads this config, which would
    -- stop a command this action is still waiting on.
    local merged = { REX_SESSION = ctx.session_id or "" }
    for key, value in pairs(env) do merged[key] = value end
    local _, err = rex.exec.start(argv("rex", "place", ctx.block_id, label, "--", ...),
      { env = merged, detach = true })
    if err ~= nil then
      error("leaf: couldn't start leaf (" .. tostring(err) .. "). Install it with `cargo install leaf-reader`.", 0)
    end
  end

  -- A picker floating over the window, like Rex's own; it closes when its
  -- program exits.
  local function popover(ctx, label, command, dir)
    local options = { command = command }
    if dir ~= nil and dir ~= "" then options.cwd = dir end
    local _, err = rex.session.new_layer{
      bounds = { 0.22, 0.1, 0.56, 0.62 },
      focus = true,
      layout = { block = { flavor = SHELL, label = label, options = options } },
    }
    if err ~= nil then error("leaf: couldn't open the picker: " .. tostring(err), 0) end
  end

  -- Asking in Rex's palette -------------------------------------------------
  --
  -- Rex 17 and later can ask a question in its own command palette
  -- (`rex.ui.pick`). Pickers ask there, and fall back to leaf's popover when
  -- no app can show the question or the Rex here is older. leaf prints the
  -- rows as JSON. On a remote host the files and agents are the host's, so
  -- those keep the popover, which runs leaf there.

  -- Reasons a prompt couldn't be shown, for which leaf's own picker stands in.
  local FALL_BACK = { unsupported = true, failed = true }

  -- Whether this Rex has palette prompts at all, checked before listing
  -- anything slow, so an older Rex goes straight to leaf's picker.
  local function can_ask()
    return type(rex.ui) == "table" and type(rex.ui.pick) == "function"
  end

  -- The answer; or nil and a reason, `fallback` when leaf's picker should ask.
  local function ask(spec)
    if not can_ask() then return nil, "fallback" end
    local answer, reason, detail = rex.ui.pick(spec)
    if answer ~= nil then return answer end
    if FALL_BACK[reason] then
      if detail ~= nil then rex.log("warn", "leaf: palette prompt failed: " .. tostring(detail)) end
      return nil, "fallback"
    end
    if reason == "busy" then
      error("leaf: Rex is already asking something else. Answer or close that, then try again.", 0)
    end
    -- cancelled, timeout, or no app connected to answer: nothing to do.
    return nil, reason
  end

  -- What leaf prints as JSON for `...`, or nil and why it couldn't.
  local function leaf_json(...)
    local result = rex.exec(argv(...), { env = env, timeout = 15 })
    if result == nil then return nil, "leaf didn't start" end
    if not result.ok then
      local why = (result.stderr or ""):gsub("^%s+", ""):gsub("%s+$", "")
      return nil, why ~= "" and why or "leaf exited with an error"
    end
    local ok, value = pcall(rex.json.decode, result.stdout or "")
    if not ok or type(value) ~= "table" then
      return nil, "leaf printed something that isn't a list (" .. tostring(value) .. ")"
    end
    return value
  end

  -- Asks with the rows `leaf <command> --list` prints; `fallback` when the
  -- palette can't ask or the list didn't come, so leaf's picker still opens.
  local function ask_from(command, spec)
    if not can_ask() then return nil, "fallback" end
    local items, err = leaf_json(command, "--list")
    if items == nil then
      rex.log("warn", "leaf: couldn't list " .. command .. ": " .. tostring(err))
      return nil, "fallback"
    end
    spec.items = items
    return ask(spec)
  end

  -- Browses for a document in the palette, from `dir`: folders open in place,
  -- and a path or link can be typed. The target; or nil and a reason.
  -- A first listing that fails (the pane's folder is gone, say) hands over to
  -- leaf's home screen; a later one keeps the folder shown and says why.
  local function pick_document(dir)
    if not can_ask() then return nil, "fallback" end
    local listing, err = leaf_json("open", "--list", dir, "--recent")
    if listing == nil then
      rex.log("warn", "leaf: couldn't list " .. tostring(dir) .. ": " .. tostring(err))
      return nil, "fallback"
    end
    local title = listing.title
    while true do
      if listing.kind == "document" then return listing.target end
      local answer, reason = ask{
        title = title,
        placeholder = "Filter, or type a path or a link",
        items = listing.items,
        allow_other = true,
      }
      if answer == nil then return nil, reason end
      local next_path
      if type(answer) == "table" then
        local text = (answer.text or ""):match("^%s*(.-)%s*$")
        if text:match("^[Hh][Tt][Tt][Pp][Ss]?://") then return text end
        if text == "" then return nil, "cancelled" end
        local first = text:sub(1, 1)
        next_path = (first == "/" or first == "~") and text or (listing.dir .. "/" .. text)
      elseif answer:sub(1, 4) == "dir:" then
        next_path = answer:sub(5)
      else
        return (answer:gsub("^open:", ""))
      end
      local next_listing, next_err = leaf_json("open", "--list", next_path)
      if next_listing == nil then
        title = tostring(next_err):gsub("^leaf: ", "")
      else
        listing, title = next_listing, next_listing.title
      end
    end
  end

  -- Readers -----------------------------------------------------------------

  -- A pane shows a leaf document (or, with `diffs`, a diff) while the record
  -- leaf left is owned by the process in front.
  local function reader_in(block_id, diffs)
    local status = rex.block.call(TERMINAL, "program_status", { block_id = block_id })
    if type(status) ~= "table" or type(status.records) ~= "table" then return nil end
    local record
    for _, r in ipairs(status.records) do
      if r.id == RECORD or (diffs and r.id == DIFF_RECORD) then record = r end
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
  local function find_reader(ctx, diffs)
    if ctx.block_id ~= nil then
      local here = reader_in(ctx.block_id, diffs)
      if here ~= nil then return here end
    end
    local listed = rex.session.list_blocks{ session_id = ctx.session_id }
    if type(listed) ~= "table" or type(listed.blocks) ~= "table" then return nil end
    for _, block in ipairs(listed.blocks) do
      if block.block_id ~= nil and block.block_id ~= ctx.block_id then
        local found = reader_in(block.block_id, diffs)
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
    name = "leaf_browser",
    title = "Leaf: Open Browser",
    description = "Open a WebKit browser in a pane beside this one.",
    category = "Leaf",
    keywords = "leaf browser web webkit safari website url localhost",
    args = { url = "string?" },
    run = function(ctx, args)
      if ctx.server ~= nil then
        error("leaf: the browser runs on this Mac, so it isn't available in a session on a remote host.", 0)
      end
      local url = args and args.url
      if url ~= nil and url ~= "" then
        open_pane(ctx, "browser", argv("browser", url), cwd(ctx))
      else
        open_pane(ctx, "browser", argv("browser"), cwd(ctx))
      end
    end,
  }

  rex.action{
    name = "leaf_open",
    title = "Leaf: Open Document…",
    description = "Open leaf's home in this pane: today's reading, a heat map of the days you read,"
      .. " and a search over recent documents and this folder's PDFs, EPUBs, markdown and office files.",
    category = "Leaf",
    keywords = "leaf pdf markdown epub word powerpoint excel slides spreadsheet read document article book file picker",
    args = { target = "string?" },
    run = function(ctx, args)
      local target = args.target or ""
      -- Nothing named: browse in Rex's palette; or leaf's home screen, in
      -- this pane (beside it when busy), when the palette can't ask.
      if target == "" then
        if ctx.server == nil then
          local picked, reason = pick_document(cwd(ctx) or os.getenv("HOME") or "/")
          if picked == nil and reason ~= "fallback" then return end
          target = picked or ""
        end
        if target == "" then
          place(ctx, "leaf", "open")
          return
        end
      end
      place(ctx, target:match("([^/]+)$") or "leaf", "open", target)
    end,
  }

  rex.action{
    name = "leaf_board",
    title = "Leaf: Open Whiteboard",
    description = "Start a new whiteboard in this pane's folder (beside it when busy)."
      .. " Press @ on the board to ask your agent to draw.",
    category = "Leaf",
    keywords = "leaf whiteboard board excalidraw sketch draw diagram canvas",
    run = function(ctx)
      place(ctx, "board", "board")
    end,
  }

  rex.action{
    name = "leaf_diff",
    title = "Leaf: Review Diff",
    description = "Review the working tree, a revision, a patch, or a GitHub link in this pane."
      .. " Comments go to the agent pane with ctrl+enter.",
    category = "Leaf",
    keywords = "leaf diff review pull request patch git changes",
    args = { target = "string?" },
    run = function(ctx, args)
      local target = args.target
      if target ~= nil and target ~= "" then
        place(ctx, "diff", "diff", target)
      else
        place(ctx, "diff", "diff")
      end
    end,
  }

  rex.action{
    name = "leaf_review_staged",
    title = "Leaf: Review Staged Changes",
    description = "Review what's staged for the next commit in this pane.",
    category = "Leaf",
    keywords = "leaf diff staged index commit review",
    run = function(ctx) place(ctx, "staged", "diff", "--staged") end,
  }

  rex.action{
    name = "leaf_review_branch",
    title = "Leaf: Review This Branch",
    description = "Review this branch against its base branch in this pane.",
    category = "Leaf",
    keywords = "leaf diff branch pull request pr review",
    run = function(ctx) place(ctx, "branch", "diff", "--branch") end,
  }

  rex.action{
    name = "leaf_review_plan",
    title = "Leaf: Review Plan…",
    description = "Review a Markdown plan block by block: comment on, remove or replace blocks,"
      .. " add a global note, and send one review with a verdict to the agent pane.",
    category = "Leaf",
    keywords = "leaf plan review margin spec design agent approve request changes",
    args = { target = "string" },
    run = function(ctx, args)
      place(ctx, args.target:match("([^/]+)$") or "plan", "plan", args.target)
    end,
  }

  rex.action{
    name = "leaf_review_changes",
    title = "Leaf: Review Changes",
    description = "Review the working tree file by file, starting from an overview of every file:"
      .. " notes on lines, files and the whole change go to the agent pane as one review.",
    category = "Leaf",
    keywords = "leaf review margin changes files agent approve request changes overview",
    args = { target = "string?" },
    run = function(ctx, args)
      local target = args.target
      if target ~= nil and target ~= "" then
        place(ctx, "review", "review", target)
      else
        place(ctx, "review", "review")
      end
    end,
  }

  rex.action{
    name = "leaf_pr",
    title = "Leaf: Pull Requests & Issues",
    description = "Your open pull requests and issues across GitHub, newest first, in this"
      .. " pane. Pick one to read its summary and timeline; pull requests also show a diff"
      .. " and can be merged, closed, approved or commented, and issues can be commented"
      .. " on, replied to, closed or reopened.",
    category = "Leaf",
    keywords = "leaf pr pull request issue issues github review merge squash rebase checks timeline branch",
    args = { target = "string?" },
    run = function(ctx, args)
      local target = args.target
      if target ~= nil and target ~= "" then
        place(ctx, "pr", "pr", target)
      else
        place(ctx, "pull requests & issues", "pr")
      end
    end,
  }

  rex.action{
    name = "leaf_agent",
    title = "Leaf: Choose Agent…",
    description = "Pick which installed agent CLI gets questions about a selection in a Leaf"
      .. " document or diff. Its open pane gets them; with none open, leaf starts it beside.",
    category = "Leaf",
    keywords = "leaf agent claude codex opencode gemini grok pi cursor default send ask picker",
    run = function(ctx)
      if ctx.server == nil then
        local picked, reason = ask_from("agents", {
          title = "Send questions to",
          placeholder = "Search installed agents",
        })
        if picked ~= nil then
          local result = rex.exec(argv("agents", "--set", picked), { env = env, timeout = 5 })
          if result == nil or not result.ok then
            error("leaf: couldn't save the agent (" .. tostring(result and result.stderr)
              .. "). Questions still go to the agent picked before.", 0)
          end
          return
        end
        if reason ~= "fallback" then return end
      end
      popover(ctx, "Agent", pane_command(ctx, "agents"), cwd(ctx))
    end,
  }

  -- While a document or a diff is open --------------------------------------------
  --
  -- Rex can't hide an action per pane, so these are only defined while leaf
  -- shows something on this machine: leaf reloads Rex's config when what's
  -- open changes, and this asks it what that is ("document diff", or "idle").

  local function open_kinds()
    local result = rex.exec(argv("rex", "state"), { env = env, timeout = 5 })
    local kinds = {}
    if result ~= nil and result.ok then
      for word in (result.stdout or ""):gmatch("%a+") do kinds[word] = true end
    end
    return kinds
  end

  local open = open_kinds()

  -- Rex's palette has no checkbox, so the title says whether the page is
  -- drawn edge to edge. `leaf browser bar toggle` saves the choice, every
  -- open browser pane picks it up, and leaf reloads this config so the
  -- title follows. Off unless leaf says it's on.
  local minimal_bar = open.minimal == true
  rex.action{
    name = "leaf_browser_bar",
    title = minimal_bar and "Leaf: Minimal Address Bar ✓" or "Leaf: Minimal Address Bar (Off)",
    description = minimal_bar
      and "The address bar stays hidden until the pointer is at the top of the page, or you press cmd+l."
        .. " Run to show it all the time."
      or "The address bar is always visible. Run to hide it until the pointer is at the top of the page.",
    category = "Leaf",
    keywords = "leaf browser address bar url chrome toolbar hide hover minimal compact omnibox",
    run = function()
      -- Detached: switching reloads this config, which would stop a command
      -- this action is still waiting on.
      local _, err = rex.exec.start(argv("browser", "bar", "toggle"), { env = env, detach = true })
      if err ~= nil then
        error("leaf: couldn't switch the address bar (" .. tostring(err) .. "). It is unchanged.", 0)
      end
    end,
  }

  if open.document or open.diff then
    rex.action{
      name = "leaf_ask",
      title = "Leaf: Ask Agent About Selection",
      description = "Ask the agent pane about the selection in a Leaf document or diff, sent with"
        .. " the file, the page or side, the line numbers and the lines around it."
        .. " Without question, leaf asks you to type one. In leaf itself: @ or ctrl+enter.",
      category = "Leaf",
      keywords = "leaf agent claude codex grok pi selection ask quote explain diff",
      args = { question = "string?" },
      run = function(ctx, args)
        local question = args.question or ""
        local reader = find_reader(ctx, true)
        if reader == nil then
          error("leaf: no document or diff is open in a Leaf pane in this session.", 0)
        end
        tell(reader, "ask " .. question)
        if question == "" then focus(reader.block_id) end
      end,
    }
    rex.bind(ASK_KEY, "leaf_ask")
  end

  local function reading_actions()

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
  rex.action{
    name = "leaf_voice",
    title = "Leaf: Change Voice…",
    description = "Pick who reads aloud from every voice Microsoft offers, searchable by name, language or gender.",
    category = "Leaf",
    keywords = "leaf voice speech tts language accent picker",
    run = function(ctx)
      local reader = find_reader(ctx)
      if reader == nil then error(NO_READER, 0) end
      if ctx.server == nil then
        local picked, reason = ask_from("voices", {
          title = "Read aloud in",
          placeholder = "Search by name, language or gender",
        })
        -- The reader switches, and saves it as the default.
        if picked ~= nil then tell(reader, "voice " .. picked) return end
        if reason ~= "fallback" then return end
      end
      popover(ctx, "Voice", pane_command(ctx, "voices", "--reader", reader.block_id))
    end,
  }

  end -- reading_actions

  if open.document then reading_actions() end

  -- The look ------------------------------------------------------------------
  --
  -- Rex's palette has no checkbox, so the title says whether the dither UI
  -- is on. `leaf ui toggle` saves the choice, every open leaf picks it up,
  -- and leaf reloads this config so the title follows.

  -- Off unless leaf says it's on: the classic look is the default.
  local dither = open.dither == true
  rex.action{
    name = "leaf_ui",
    title = dither and "Leaf: Dither UI ✓" or "Leaf: Dither UI (Off)",
    description = dither
      and "The dithered look is on: tinted bars, dithered margins and panels in your theme's colors."
        .. " Run to switch to the classic look."
      or "The classic look is on. Run to turn on the dithered look.",
    category = "Leaf",
    keywords = "leaf dither ui look style classic old toggle appearance theme on off",
    run = function()
      -- Detached: switching reloads this config, which would stop a command
      -- this action is still waiting on.
      local _, err = rex.exec.start(argv("ui", "toggle"), { env = env, detach = true })
      if err ~= nil then
        error("leaf: couldn't switch the look (" .. tostring(err) .. "). It is unchanged.", 0)
      end
    end,
  }

  for short, key in pairs(opts.keys or {}) do
    rex.bind(key, "leaf_" .. short)
  end
end

return M
