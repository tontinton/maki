-- The `@` completion popup: what it shows, the keys it takes while it is up,
-- and the edit that accepts a row.

local ListPicker = require("maki.list_picker")
local Trigger = require("trigger")

local M = {}

local opts = maki.api.register_options({
  max_items = { default = 10, min = 1, desc = "Rows the completion popup shows at once." },
})

-- The keys the popup takes while it is on screen are declared with the
-- handlers that answer them, further down: see {M.BINDINGS}.
local FOOTER = { { "↑/↓", "move" }, { "Enter", "insert" }, { "Esc", "close" } }
-- Above the input box and the transcript, below anything modal.
local ZINDEX = 120
-- Cells the border costs, on both sides and on both ends.
local BORDER = 2
-- One blank cell either side of a row, so text never touches the border.
local PAD = 2
-- Shown in place of the list, so a path that does not exist yet says so
-- without the popup blinking out and back.
local NO_MATCHES = "no matches"
-- An empty list while the first walk is still filling is a different answer
-- from "not there".
local SCANNING = "scanning…"
local TEARDOWN_FAILED = "completion: teardown step failed: "
-- How long the popup waits on a walk that has not reported in. The
-- subscription answers the instant the walk lands, so this only runs out when
-- nothing is coming: the walk failed, or the host let it go to stay under its
-- ceiling of indexed roots. Enter is the key that pays for the wait, so the
-- bound is one pause the user can sit through rather than a dead key.
local SCAN_WAIT_MS = 3000
-- The user pressed Enter on a row and got nothing. Saying so beats leaving
-- them looking at a popup that did not answer the key it advertised.
local EDIT_REFUSED = "completion: the path was not inserted: "

local popup = nil
-- Declared up here because the key handlers below redraw with it.
local lines
-- The `FileIndexReady` subscription while a popup waits on a walk, nil the
-- rest of the time.
local index_sub = nil
-- Bumped by every close, so a round-trip that comes back after the popup it
-- was for has gone cannot repaint the one that replaced it.
local generation = 0
-- Numbers the refreshes. Only the newest may paint, or the loser leaves the
-- previous query's files on screen.
local latest = 0
-- The input the newest refresh was for. The rows on screen can be older than
-- this, because the refresh for the last keystroke may still be ranking.
local latest_input = nil
-- The session's working tree, and the session it was read for. One read per
-- popup rather than one per keystroke: `maki.session.read` is a round trip,
-- and the tree cannot move under a popup that is up, because a `/cd` starts
-- with the `/` that closes one. Keyed by session so the answer read in one
-- tab is never handed to another, and dropped by every close.
local cwd = nil

-- The session the popup belongs to, or nil when there is no popup.
function M.session_id()
  return popup and popup.st.session_id
end

-- Stops listening for the walk. Safe when nothing is listening.
local function unwatch()
  if index_sub then
    maki.api.del_autocmd(index_sub)
    index_sub = nil
  end
end

-- Runs {step} whatever it does, so one failing teardown step cannot strand
-- the others.
local function guarded(step)
  local ok, err = pcall(step)
  if not ok then
    maki.log.warn(TEARDOWN_FAILED .. tostring(err))
  end
end

-- The only teardown. Closing the window is what hands every key back, which is
-- how this plugin can take `<C-p>` while the sessions plugin has it the rest
-- of the time: the claim goes out with the float that drew it.
--
-- Each step is on its own, because an error unsubscribing must not leave the
-- float on screen holding its keys.
function M.close()
  local closing = popup
  popup = nil
  cwd = nil
  generation = generation + 1
  if not closing then
    return
  end
  guarded(unwatch)
  guarded(function()
    closing.win:close()
  end)
end

local function move(delta)
  if not popup then
    return
  end
  local n = #popup.items
  if n > 0 then
    popup.sel = (popup.sel - 1 + delta) % n + 1
    popup.buf:set_lines(lines())
  end
end

-- Insert the highlighted row over the mention it was ranked for.
--
-- The range, the version and the session all came with the snapshot the
-- refresh drew from, and writing that range back is the same edit the user
-- pressed Enter on. Nothing is re-read: `maki.ui.input_edit` weighs that
-- snapshot itself and refuses an edit the input has moved past, which is where
-- staleness belongs.
--
-- The popup closes first, which hands the keys back before the edit fires the
-- `InputChanged` this plugin is listening to.
--
-- With no row to insert, Enter closes the popup and nothing else. The user's
-- next Enter sends the message, which is the standard behaviour and needs no
-- agreement with the host about who holds the key. Not while the walk is still
-- running, though: there is no row yet because nothing has looked, and closing
-- on the press the user made to pick one would take the popup down a moment
-- before its rows arrive. That wait is bounded by {SCAN_WAIT_MS}, after which
-- the rows are all there is going to be and Enter closes the popup the way it
-- does on an empty list.
--
-- A refusal is still the end of the key, but not of the user's question: they
-- pressed Enter on a row and the path is not there. The host knows why, so
-- pass its answer on instead of dropping it.
function M.accept()
  local at = popup
  local choice = at and at.items[at.sel]
  if not choice then
    if at and at.scanning then
      return
    end
    return M.close()
  end
  M.close()
  local ok, err = maki.ui.input_edit({
    start = at.start,
    stop = at.st.cursor,
    text = choice.path .. " ",
    version = at.st.version,
    session_id = at.st.session_id,
  })
  if not ok then
    maki.ui.flash(EDIT_REFUSED .. tostring(err))
  end
end

local function next_row()
  move(1)
end

local function prev_row()
  move(-1)
end

-- Every key the popup takes while it is on screen. The popup is unfocused,
-- because the user goes on typing into the chat input beneath it, so the host
-- has to be told which keys are the popup's. Everything else reaches the input
-- as it always did. The list belongs to the window and dies with it, so there
-- is nothing to release and nothing that can outlive what the user can see.
--
-- One spelling: the notation a key is claimed in is the notation `win:recv`
-- reports the press under, so `claim` is also the handler's key.
--
-- Public so the spec can press every binding the popup declares.
--
-- No `<Tab>` here. It toggles the mode, and it should do that whether a popup
-- is up or not.
M.BINDINGS = {
  { claim = "<Down>", run = next_row },
  { claim = "<Up>", run = prev_row },
  { claim = "<C-n>", run = next_row },
  { claim = "<C-p>", run = prev_row },
  { claim = "<Esc>", run = M.close },
  { claim = "<CR>", run = M.accept },
}

-- Derived, not written out a second time: a key on one list and not the other
-- would be claimed, consumed, and do nothing.
local KEYS, HANDLERS = {}, {}
for i, binding in ipairs(M.BINDINGS) do
  KEYS[i] = binding.claim
  HANDLERS[binding.claim] = binding.run
end

function M.handle_key(key)
  local handler = HANDLERS[key]
  if handler then
    handler()
  end
end

-- The popup's key loop, one per window. It ends with the window: `close` takes
-- the float down and the host answers with a `close` event, so nothing has to
-- be kept in step and no loop can outlive the popup it reads for.
--
-- A key queued before a close can land after a newer popup has opened, and it
-- says nothing about that one's rows, so {mine} has to still be the popup.
local function read_keys(mine)
  maki.async.run(function()
    while true do
      local ev = mine.win:recv()
      if not ev or ev.type == "close" then
        return
      end
      if ev.type == "key" and popup == mine then
        M.handle_key(ev.key)
      end
    end
  end)
end

-- The ranked paths, or the one placeholder row that stands in for them.
local function rows()
  if #popup.items == 0 then
    return { { path = popup.scanning and SCANNING or NO_MATCHES, highlights = {} } }, true
  end
  return popup.items, false
end

-- Matches are drawn the way the `Ctrl+S` file picker draws them, so both
-- lists read alike.
function lines()
  local shown, empty = rows()
  local out = {}
  for i, item in ipairs(shown) do
    local selected = not empty and i == popup.sel
    local base = empty and "dim" or (selected and "selected" or "item")
    local matched = selected and "match_selected" or "match"
    out[i] = ListPicker.range_spans(item.path, item.highlights, base, matched)
    table.insert(out[i], 1, { " ", base })
  end
  return out
end

-- Only the size. The caret anchor decides where the popup goes and the host
-- redoes that every frame, so it follows the caret through wraps and resizes.
-- The host already widens the window to fit its footer, so only rows count.
local function render(size)
  local shown = rows()
  local width = 0
  for _, item in ipairs(shown) do
    width = math.max(width, maki.ui.display_width(item.path) + PAD + BORDER)
  end
  popup.buf:set_lines(lines())
  popup.win:set_config({ width = math.min(width, size.cols), height = #shown + BORDER })
  popup.win:show()
end

-- Opened hidden, so the first frame never paints an empty popup before the
-- files it is for have been read.
local function ensure_popup()
  if popup then
    return
  end
  local buf = maki.ui.buf()
  local win = maki.ui.open_win(buf, {
    width = PAD + BORDER,
    height = BORDER + 1,
    anchor = "input_caret",
    border = "rounded",
    footer = FOOTER,
    zindex = ZINDEX,
    focus = false,
    visible = false,
    keys = KEYS,
  })
  popup = {
    win = win,
    buf = buf,
    sel = 1,
    items = {},
  }
  read_keys(popup)
end

-- Ranks again, later, whatever the user typed last by the time it runs.
-- Keystrokes can queue up behind it, and ranking the snapshot it was asked for
-- would cancel the ranking of the newest one and leave old rows up for good.
-- A close in between wins, or an `<Esc>` pressed mid-scan would be undone.
local function rerank()
  local token = generation
  maki.async.run(function()
    if generation == token then
      M.refresh(latest_input)
    end
  end)
end

-- Asks the index again once the walk behind a `scanning…` row lands.
--
-- `maki.fs.fuzzy_files` answers out of whatever the walk has reached so far,
-- so on a large tree the rows stay `scanning…` until something asks again.
-- Nobody types while waiting to see what is there, so the popup listens.
--
-- Only the walk of the tree these rows came from. The ranking answers with the
-- root it resolved, spelled the way the event spells it, so a walk of another
-- session's tree is ignored instead of costing this popup a ranking it would
-- throw away. Comparing the session's own cwd would be wrong across a symlink;
-- these two are both canonical.
--
-- The wait is bounded, because a walk that never reports in would leave
-- `scanning…` up and Enter consumed with nothing to show for it. The deadline
-- and the subscription live and die together: whichever answers first stops
-- listening, and a later keystroke that still finds the walk unfinished starts
-- one fresh pair. No polling, one timer.
--
-- Returns whether this call is what started listening, because a walk that
-- landed between the ranking above and here fired `FileIndexReady` with nobody
-- subscribed. One more look answers that, and the docs promise the rows fill in
-- on their own.
local function watch_the_walk(token)
  if index_sub then
    return false
  end
  maki.defer_fn(function()
    if generation ~= token or not popup or not popup.scanning then
      return
    end
    -- Nothing is coming. What is on screen is every row there is, so the popup
    -- stops saying otherwise and hands Enter the same answer an empty list
    -- gives it.
    popup.scanning = false
    unwatch()
    render(maki.ui.terminal_size())
  end, SCAN_WAIT_MS)
  index_sub = maki.api.create_autocmd("FileIndexReady", {
    callback = function(ev)
      -- A close, a newer popup or a session switch can land between the walk
      -- ending and this, and each of them owns the popup more than the walk
      -- does.
      if generation ~= token or not popup then
        return unwatch()
      end
      -- Another tree's walk says nothing about these rows, and the walk this
      -- popup is waiting on has still not landed.
      if ev.data.root ~= popup.root then
        return
      end
      rerank()
    end,
  })
  return true
end

-- The session's working tree, not the process's. They differ the moment a
-- second session is opened elsewhere, and offering files from the wrong tree
-- is worse than offering none. Read once per popup; see {cwd}.
local function tree_of(session_id)
  if cwd and cwd.session_id == session_id then
    return cwd.root
  end
  local session = maki.session.read()
  if not session or not session.cwd then
    return nil
  end
  cwd = { session_id = session_id, root = session.cwd }
  return cwd.root
end

-- Hands a refresh off to run on its own. Autocmd dispatch waits on its
-- handlers, so every other plugin's events would queue behind this one.
function M.refresh_later(st)
  maki.async.run(function()
    M.refresh(st)
  end)
end

-- The one place that decides whether there should be a popup at all, and what
-- is in it.
--
-- {st} is the chat input as the host last reported it: the `InputChanged`
-- payload carries the text, the caret, the version and the session, which is
-- every field `maki.ui.input` would answer with and one round trip less. The
-- rows are what the user reads before pressing Enter, so the sooner they
-- answer the keystroke that asked for them the better.
function M.refresh(st)
  local token = generation
  latest = latest + 1
  local mine = latest
  latest_input = st
  -- True once this refresh has been overtaken, either by a newer keystroke or
  -- by a close.
  local function stale()
    return generation ~= token or latest ~= mine
  end

  local start, query = Trigger.find(st.text, st.cursor)
  -- A slash command owns the input while the command palette is up, so there
  -- is no mention to complete in one. The palette's keys are the host's to
  -- route, above every claim, so this is about the rows and not about them.
  if not start or st.text:sub(1, 1) == "/" then
    return M.close()
  end

  local root = tree_of(st.session_id)
  if stale() then
    return
  end
  if not root then
    return M.close()
  end

  -- One ranking, not one walk: the host indexed this tree once and shares it
  -- with its own file picker, and only `limit` paths come back. A nil answer
  -- is a newer keystroke cancelling this call, which is how most refreshes
  -- end while the user types.
  local found = maki.fs.fuzzy_files({ query = query, limit = opts.max_items, path = root, highlights = true })
  if stale() or not found then
    return
  end

  ensure_popup()
  -- The snapshot the rows below answer, and the range an accept writes over.
  -- `maki.ui.input_edit` weighs it again when the accept comes, so a row drawn
  -- for text the user has typed past is refused rather than written.
  popup.st = st
  popup.start = start
  -- A walk that crashed or hit the host's ceiling has ended, so what is on
  -- screen is all there is going to be and saying `scanning…` would be a lie.
  popup.scanning = not found.complete
  -- The tree these rows came from, as the host spells it, which is what tells
  -- the walk this popup waits on from every other one.
  popup.root = found.root
  -- The highlight follows the path it was on while that path is still listed,
  -- so a keystroke that only drops candidates does not move the selection out
  -- from under the user.
  local was = popup.items[popup.sel]
  popup.items, popup.sel = found.items, 1
  for i, item in ipairs(found.items) do
    if was and item.path == was.path then
      popup.sel = i
    end
  end
  render(maki.ui.terminal_size())
  if not popup.scanning then
    return unwatch()
  end
  if watch_the_walk(token) then
    rerank()
  end
end

return M
