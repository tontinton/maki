-- The completion popup: what it shows, the keys it takes while it is up,
-- and the edit that accepts a row.

local ListPicker = require("maki.list_picker")
local Sources = require("sources")
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
-- Rows fill in as answers land, so this is not a delay. It only bounds how
-- long a source that never answers can keep `scanning…` up and hold Enter.
-- Past it, that source's rows from the keystroke before are dropped, so they
-- cannot pass for current.
local SOURCE_WAIT_MS = 1000
-- The user pressed Enter on a row and got nothing. Saying so beats leaving
-- them looking at a popup that did not answer the key it advertised.
local EDIT_REFUSED = "completion: the path was not inserted: "

-- `{ win, buf, sel, view }`. A view is what one refresh drew: the input
-- snapshot, the mention, the rows, and what is still on its way. Every row
-- keeps the snapshot it was ranked for, also when a later view carries it
-- over, so an accept never writes one keystroke's row over another
-- keystroke's range.
local popup = nil
-- Declared up here because the key handlers below redraw with it.
local lines

-- Rows may still be on their way: the files are ranking, the walk behind
-- them has not landed, or a source has not answered. An empty list then means
-- "not looked yet", so it reads `scanning…` and Enter waits.
local function scanning(view)
  return view.ranking or view.walking or next(view.waiting) ~= nil
end
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
  return popup and popup.view.st.session_id
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
-- of the time: the popup's keys are bound to its buffer, and a buffer's keys
-- only fire while a window showing it is on screen.
--
-- The bindings go too, because their handlers hold the popup, buffer and all,
-- and would keep it alive for as long as they are bound.
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
  guarded(function()
    for _, binding in ipairs(M.BINDINGS) do
      maki.keymap.del("i", binding.lhs, { buffer = closing.buf })
    end
  end)
end

local function move(delta)
  if not popup then
    return
  end
  local n = #popup.view.items
  if n > 0 then
    popup.sel = (popup.sel - 1 + delta) % n + 1
    popup.buf:set_lines(lines())
  end
end

-- Insert the highlighted row over the mention it was ranked for.
--
-- The range, the version and the session all came with the snapshot the row
-- was ranked for, and writing that range back is the same edit the user
-- pressed Enter on. Nothing is re-read: `maki.ui.input_edit` weighs that
-- snapshot itself, which is where staleness belongs. It moves the edit past
-- text typed elsewhere and refuses it once the mention itself was touched.
--
-- The popup closes first, which hands the keys back before the edit fires the
-- `InputChanged` this plugin is listening to.
--
-- With no row to insert, Enter closes the popup and nothing else. The user's
-- next Enter sends the message, which is the standard behaviour and needs no
-- agreement with the host about who holds the key. Not while {scanning},
-- though: there is no row yet because nothing has looked, and closing on the
-- press the user made to pick one would take the popup down a moment before
-- its rows arrive. That wait is bounded by {SCAN_WAIT_MS} and
-- {SOURCE_WAIT_MS}, after which the rows are all there is going to be and
-- Enter closes the popup the way it does on an empty list.
--
-- A refusal is still the end of the key, but not of the user's question: they
-- pressed Enter on a row and the path is not there. The host knows why, so
-- pass its answer on instead of dropping it.
function M.accept()
  local view = popup and popup.view
  local choice = view and view.items[popup.sel]
  if not choice then
    if view and scanning(view) then
      return
    end
    return M.close()
  end
  M.close()
  local edited, err = maki.ui.input_edit({
    start = view.start,
    stop = choice.st.cursor,
    text = choice.insert .. " ",
    version = choice.st.version,
    session_id = choice.st.session_id,
  })
  if not edited then
    maki.ui.flash(EDIT_REFUSED .. tostring(err))
  end
end

-- Backs the `completion.accept` action, which is `<CR>` on the popup for a
-- user who wants it on another key. Without a popup it answers false, so
-- their key can go on and do what it does otherwise.
function M.accept_if_open()
  if not popup then
    return false
  end
  M.accept()
  return true
end

local function next_row()
  move(1)
end

local function prev_row()
  move(-1)
end

-- Every key the popup takes while it is on screen. The popup is unfocused,
-- because the user goes on typing into the chat input beneath it, so its keys
-- are bound to its buffer: the host asks them only while a window showing it
-- is on screen, and before any global binding. Everything else reaches the
-- input as it always did. They are bound in insert mode, so a vim plugin
-- holding the input in normal mode keeps its own `<Esc>` and arrows.
--
-- Public so the spec can press every binding the popup declares.
--
-- No `<Tab>` here. It toggles the mode, and it should do that whether a popup
-- is up or not. A user who wants `<Tab>` to accept binds it in `init.lua` to
-- the `completion.accept` action, falling back to the mode toggle.
M.BINDINGS = {
  { lhs = "<Down>", run = next_row, desc = "Next completion" },
  { lhs = "<Up>", run = prev_row, desc = "Previous completion" },
  { lhs = "<C-n>", run = next_row, desc = "Next completion" },
  { lhs = "<C-p>", run = prev_row, desc = "Previous completion" },
  { lhs = "<Esc>", run = M.close, desc = "Close completions" },
  { lhs = "<CR>", run = M.accept, desc = "Insert completion" },
}

-- A key pressed on one popup can be handled after a newer popup opened, and
-- it says nothing about that one's rows, so {mine} has to still be the popup.
local function bind_keys(mine)
  for _, binding in ipairs(M.BINDINGS) do
    maki.keymap.set("i", binding.lhs, function()
      if popup == mine then
        binding.run()
      end
    end, { buffer = mine.buf, desc = binding.desc })
  end
end

-- The ranked rows, or the one placeholder row that stands in for them.
local function rows()
  local view = popup.view
  if #view.items == 0 then
    return { { label = scanning(view) and SCANNING or NO_MATCHES, highlights = {} } }, true
  end
  return view.items, false
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
    out[i] = ListPicker.range_spans(item.label, item.highlights, base, matched)
    table.insert(out[i], 1, { " ", base })
    if item.detail then
      table.insert(out[i], { item.detail, "dim" })
    end
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
    width = math.max(width, maki.ui.display_width(item.label .. (item.detail or "")) + PAD + BORDER)
  end
  popup.buf:set_lines(lines())
  popup.win:set_config({ width = math.min(width, size.cols), height = #shown + BORDER })
  popup.win:show()
end

-- Opened hidden, so the first frame never paints an empty popup before the
-- rows it is for have arrived.
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
  })
  popup = {
    win = win,
    buf = buf,
    sel = 1,
  }
  bind_keys(popup)
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
  local sub
  maki.defer_fn(function()
    -- A newer wait took over, or this one already ended. Either way there is
    -- nothing left for this deadline to guard.
    if index_sub ~= sub then
      return
    end
    unwatch()
    if generation ~= token or not popup or not popup.view.walking then
      return
    end
    -- Nothing is coming. What is on screen is every row there is, so the popup
    -- stops saying otherwise and hands Enter the same answer an empty list
    -- gives it.
    popup.view.walking = false
    render(maki.ui.terminal_size())
  end, SCAN_WAIT_MS)
  sub = maki.api.create_autocmd("FileIndexReady", {
    callback = function(ev)
      -- A close, a newer popup or a session switch can land between the walk
      -- ending and this, and each of them owns the popup more than the walk
      -- does.
      if generation ~= token or not popup then
        return unwatch()
      end
      -- Another tree's walk says nothing about these rows, and the walk this
      -- popup is waiting on has still not landed.
      if ev.data.root ~= popup.view.root then
        return
      end
      rerank()
    end,
  })
  index_sub = sub
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

-- The popup opens here and nowhere else, so it never shows up before it has
-- something to say.
--
-- The highlight stays on its row while that row is still listed, so a
-- keystroke that only drops candidates, or a source answering late, does not
-- move the selection out from under the user.
local function paint(view)
  ensure_popup()
  local was = popup.view and popup.view.items[popup.sel]
  view.items = Sources.merge(view.files, view.answers, view.order, opts.max_items)
  popup.view, popup.sel = view, 1
  for i, item in ipairs(view.items) do
    if was and item.insert == was.insert then
      popup.sel = i
    end
  end
  render(maki.ui.terminal_size())
end

-- Stamps {rows} with the snapshot they answer. See {popup}.
local function ranked_for(rows, st)
  for _, row in ipairs(rows) do
    row.st = st
  end
  return rows
end

-- Rows drawn for the same mention a keystroke ago stay up until this one's
-- answers replace them, so the popup narrows instead of blinking empty on
-- every key. A different mention starts from nothing, because the old rows
-- would answer a question nobody is asking any more.
--
-- `ranking` starts true for `@`, because a source can answer and paint before
-- the files are ranked. Also returns whether the old rows were carried over.
local function new_view(st, start, trigger, asked)
  local prior = popup and popup.view
  local same = prior and prior.trigger == trigger and prior.start == start
  local view = {
    st = st,
    start = start,
    trigger = trigger,
    ranking = trigger == Trigger.FILES,
    files = same and prior.files or {},
    walking = same and prior.walking or false,
    root = same and prior.root or nil,
    answers = {},
    order = {},
    waiting = {},
    items = {},
  }
  for i, source in ipairs(asked) do
    view.order[i] = source.name
    view.waiting[source.name] = true
    view.answers[source.name] = same and prior.answers[source.name] or nil
  end
  return view, same
end

-- Asks every source at once and paints each answer as it lands. A newer
-- keystroke, a close or the deadline stops the waiting, and from then on a
-- late answer is ignored. `ctx.cancelled()` lets a source that cares notice
-- and stop early.
local function ask_sources(view, asked, query, root, stale)
  if #asked == 0 then
    return
  end
  for _, source in ipairs(asked) do
    local name = source.name
    local function awaited()
      return not stale() and view.waiting[name] ~= nil
    end
    local ctx = {
      trigger = view.trigger,
      session_id = view.st.session_id,
      cwd = root,
      cancelled = function()
        return not awaited()
      end,
    }
    Sources.ask(source, query, ctx, function(rows)
      if not awaited() then
        return
      end
      view.waiting[name] = nil
      view.answers[name] = ranked_for(rows, view.st)
      paint(view)
    end)
  end
  maki.defer_fn(function()
    if stale() or not next(view.waiting) then
      return
    end
    for name in pairs(view.waiting) do
      view.answers[name] = nil
    end
    view.waiting = {}
    paint(view)
  end, SOURCE_WAIT_MS)
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

  -- Gathered on every keystroke rather than kept, so a plugin loaded or
  -- unloaded mid-session counts from the very next key.
  local sources = Sources.collect()
  if stale() then
    return
  end
  local start, query, trigger = Trigger.find(st.text, st.cursor, Trigger.FILES .. Sources.triggers(sources))
  -- A slash command owns the input while the command palette is up, so there
  -- is no mention to complete in one. The palette handles its keys before any
  -- binding, so this is about the rows and not about the keys.
  -- Completion holds no `run`, so Enter would refuse a `!` line it wrote
  -- into. The shell would also read a path with a space or a `;` in it as
  -- more than one word.
  if not start or st.text:match("^%s*[/!]") then
    return M.close()
  end

  local root = tree_of(st.session_id)
  if stale() then
    return
  end
  if not root then
    return M.close()
  end

  local asked = Sources.for_trigger(sources, trigger)
  local view, same = new_view(st, start, trigger, asked)
  -- Asked before the files are ranked, so the two run side by side.
  ask_sources(view, asked, query, root, stale)
  -- A popup still showing another mention's rows would let Enter accept one
  -- of them, so it switches to `scanning…` until this mention's answers land.
  -- When only the files are coming, the old rows stay up for the one frame
  -- ranking takes.
  if popup and not same and next(view.waiting) and not stale() then
    paint(view)
  end
  if trigger ~= Trigger.FILES then
    -- No walk behind these rows. A wait left over from a file mention would
    -- only stop the next file mention from starting its own.
    if not stale() then
      unwatch()
    end
    return
  end

  -- One ranking, not one walk: the host indexed this tree once and shares it
  -- with its own file picker, and only `limit` paths come back. A nil answer
  -- is a newer keystroke cancelling this call, which is how most refreshes
  -- end while the user types.
  local found = maki.fs.fuzzy_files({ query = query, limit = opts.max_items, path = root, highlights = true })
  if stale() then
    return
  end
  view.ranking = false
  if not found then
    -- A ranking that failed adds no files, so a view the sources already
    -- painted stops holding Enter for them.
    if popup and popup.view == view then
      paint(view)
    end
    return
  end
  view.files = ranked_for(Sources.file_rows(found.items), st)
  -- A walk that crashed or hit the host's ceiling has ended, so what is on
  -- screen is all there is going to be and saying `scanning…` would be a lie.
  view.walking = not found.complete
  -- The tree these rows came from, as the host spells it, which is what tells
  -- the walk this popup waits on from every other one.
  view.root = found.root
  paint(view)
  if not view.walking then
    return unwatch()
  end
  if watch_the_walk(token) then
    rerank()
  end
end

return M
