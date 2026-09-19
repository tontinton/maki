local Events = require("events")
local Menu = require("menu")
local Trigger = require("trigger")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

local function find(text, cursor)
  local start, query = Trigger.find(text, cursor or #text)
  return start, query
end

case("trigger_finds_a_mention_at_the_cursor", function()
  local start, query = find("explain @src/ma")
  eq(start, 8, "the `@` is the ninth byte, so offset 8")
  eq(query, "src/ma", "everything after the `@` is the query")
end)

case("trigger_fires_on_a_bare_at_sign", function()
  local start, query = find("@")
  eq(start, 0, "an `@` in an empty input opens a mention at offset 0")
  eq(query, "", "with nothing typed into it yet")
end)

case("trigger_ignores_an_at_sign_after_a_multi_byte_letter", function()
  eq(find("wörld@src"), nil, "the byte before the `@` is a letter, whatever its encoding")
end)

case("trigger_ignores_an_at_sign_mid_word", function()
  eq(find("mail me at nick@example.com"), nil, "an email address is not a mention")
end)

case("trigger_ends_at_a_space", function()
  eq(find("@src/main.rs and then"), nil, "the mention ended when the user typed past it")
end)

case("trigger_takes_the_last_mention_on_the_line", function()
  local start, query = find("@one and @tw")
  eq(start, 9, "the mention being typed is the one at the cursor")
  eq(query, "tw")
end)

case("trigger_stops_at_the_cursor_not_the_end", function()
  local start, query = find("@src/ma and more", 6)
  eq(start, 0)
  eq(query, "src/m", "text past the cursor is not part of the query")
end)

-- Offsets are what the accept writes over, and the host counts them in bytes,
-- so a multi-byte character earlier in the line moves them.
case("trigger_counts_bytes_not_characters", function()
  local start, query = find("héllo wörld @src")
  eq(start, 14, "two two-byte characters put the `@` two bytes further along")
  eq(query, "src")
end)

case("trigger_survives_a_multi_byte_query", function()
  local start, query = find("@日本")
  eq(start, 0)
  eq(query, "日本")
end)

case("trigger_opens_after_a_newline", function()
  local start, query = find("first line\n@sec")
  eq(start, 11, "a newline counts as one byte, like everywhere else")
  eq(query, "sec")
end)

-- `close` runs from four autocmd handlers that a refresh does not hold up, so
-- the popup can go while `maki.ui.input` is in flight.

local MATCH = "src/main.rs"
local OTHER_MATCH = "src/menu.rs"
local SESSION = "session-1"
local OTHER_SESSION = "session-2"
-- What the ranking resolved the session's cwd to, canonical and absolute, the
-- way `FileIndexReady` spells the tree it walked.
local ROOT = "/repo"
local OTHER_ROOT = "/elsewhere"
local INPUT = { text = "explain @src/ma", cursor = 15, version = 3, session_id = SESSION }
local INDEX_EVENT = "FileIndexReady"
local SCANNING = "scanning…"
local NO_MATCHES = "no matches"
local HOST_GONE = "the host went away"

-- Stubs every host call the menu makes and hands {body} the knobs a case
-- turns.
--
-- `h.input` is the chat input as the host last reported it, which a case
-- passes to `Menu.refresh` the way `InputChanged` does. `h.found` is the
-- index's answer. `h.before_rank` runs inside the ranking round-trip, which is
-- where a close lands in the race the menu is written against. `h.on_close`
-- runs inside `win:close`, which is how a case makes teardown fail. `h.edits`,
-- `h.rows`, `h.claimed`, `h.focus`, `h.opens`, `h.closes`, `h.flashes`,
-- `h.subscriptions` and `h.warnings` are what the menu did.
--
-- The fake window answers `recv` with a close, so the popup's key loop ends
-- where it starts and a case drives keys through `Menu.handle_key`, the way
-- the loop does.
--
-- With `h.defer` set, a refresh handed off with `maki.async.run` waits in
-- `h.pending` until `h.flush()`. That is the window the popup is on screen in
-- with rows ranked for the snapshot before the last keystroke, and it cannot
-- be seen at all when every round trip lands inline.
--
-- `h.timers` holds what the menu armed with `maki.defer_fn`, and `h.fire_timers`
-- is the clock running out, which is how a case waits out a walk that never
-- reports in without waiting at all.
local function harness(body)
  local saved = {
    input = maki.ui.input,
    input_edit = maki.ui.input_edit,
    buf = maki.ui.buf,
    open_win = maki.ui.open_win,
    terminal_size = maki.ui.terminal_size,
    flash = maki.ui.flash,
    fuzzy_files = maki.fs.fuzzy_files,
    session = maki.session.read,
    run = maki.async.run,
    create_autocmd = maki.api.create_autocmd,
    del_autocmd = maki.api.del_autocmd,
    defer_fn = maki.defer_fn,
    warn = maki.log.warn,
  }
  local h = {
    input = INPUT,
    found = { complete = true, root = ROOT, items = { { path = MATCH } } },
    edits = {},
    rows = {},
    warnings = {},
    flashes = {},
    -- The keys the window was opened claiming, and whether it took focus.
    claimed = nil,
    focus = nil,
    -- Windows opened and closed, which is what a claim now lives and dies by.
    opens = 0,
    closes = 0,
    -- The live `FileIndexReady` callback, nil while nothing is listening.
    on_index_ready = nil,
    subscriptions = 0,
    -- What the menu spent round trips on. `input` is there to stay at zero:
    -- the event payload already carries what it would answer.
    input_calls = 0,
    session_reads = 0,
    -- Refreshes handed off and not run yet; see `h.defer`.
    pending = {},
    -- Deadlines armed and not run yet; see `h.fire_timers`.
    timers = {},
  }

  function h.flush()
    local queued = h.pending
    h.pending = {}
    for _, fn in ipairs(queued) do
      fn()
    end
  end

  function h.fire_timers()
    local queued = h.timers
    h.timers = {}
    for _, timer in ipairs(queued) do
      timer.run()
    end
  end

  maki.ui.input = function()
    h.input_calls = h.input_calls + 1
    return h.input
  end
  maki.ui.input_edit = function(edit)
    table.insert(h.edits, edit)
    if h.raise_edit then
      error(h.raise_edit)
    end
    if h.refuse_edit then
      return nil, h.refuse_edit
    end
    return true
  end
  maki.ui.buf = function()
    return {
      set_lines = function(_, painted)
        h.rows = painted
      end,
    }
  end
  maki.ui.open_win = function(_, win_opts)
    h.opens = h.opens + 1
    h.claimed = win_opts.keys
    h.focus = win_opts.focus
    return {
      set_config = function() end,
      show = function() end,
      recv = function()
        return { type = "close" }
      end,
      close = function()
        h.closes = h.closes + 1
        if h.on_close then
          h.on_close()
        end
      end,
    }
  end
  maki.ui.terminal_size = function()
    return { cols = 80 }
  end
  maki.ui.flash = function(msg)
    table.insert(h.flashes, msg)
  end
  maki.fs.fuzzy_files = function()
    if h.before_rank then
      h.before_rank()
    end
    return h.found
  end
  maki.session.read = function()
    h.session_reads = h.session_reads + 1
    return { cwd = "." }
  end
  -- The menu hands its round-trips off so autocmd dispatch does not wait on
  -- them. Running them here keeps the cases synchronous, unless the case is
  -- about the window one of them leaves open.
  maki.async.run = function(fn)
    if h.defer then
      table.insert(h.pending, fn)
      return
    end
    fn()
  end
  maki.api.create_autocmd = function(event, opts)
    eq(event, INDEX_EVENT, "the menu only ever waits on the walk")
    h.subscriptions = h.subscriptions + 1
    h.on_index_ready = opts.callback
    return h.subscriptions
  end
  maki.api.del_autocmd = function()
    h.on_index_ready = nil
  end
  maki.defer_fn = function(fn, ms)
    table.insert(h.timers, { run = fn, ms = ms })
    return { stop = function() end }
  end
  maki.log.warn = function(msg)
    table.insert(h.warnings, msg)
  end

  local ok, err = pcall(body, h)

  h.on_close = nil
  Menu.close()
  maki.ui.input, maki.ui.input_edit = saved.input, saved.input_edit
  maki.ui.buf, maki.ui.open_win = saved.buf, saved.open_win
  maki.ui.terminal_size, maki.ui.flash = saved.terminal_size, saved.flash
  maki.fs.fuzzy_files = saved.fuzzy_files
  maki.session.read, maki.async.run = saved.session, saved.run
  maki.api.create_autocmd, maki.api.del_autocmd = saved.create_autocmd, saved.del_autocmd
  maki.defer_fn = saved.defer_fn
  maki.log.warn = saved.warn
  if not ok then
    error(err)
  end
  return h
end

-- The text of the row the popup painted at {i}, border padding included.
local function row(h, i)
  return h.rows[i] and h.rows[i][1][1]
end

-- A walk landing, as the host delivers it: the tree it covered and nothing
-- about the popup waiting on it.
local function walk_landed(h, root)
  h.on_index_ready({ data = { root = root or ROOT } })
end

case("accept_inserts_the_row_it_was_showing", function()
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    Menu.accept()
  end)
  eq(#h.edits, 1)
  eq(h.edits[1].text, MATCH .. " ", "the path and the space after it")
  eq(h.edits[1].start, 8, "the offset of the `@`")
  eq(h.edits[1].stop, INPUT.cursor)
end)

-- The accept writes the snapshot the rows were ranked for, and nothing else.
-- The host dispatches `<CR>` here only while that snapshot is what the user is
-- looking at, and refuses an edit planned against any other, so the plugin
-- needs no staleness check and no second read of the input.

case("accept_writes_the_snapshot_its_rows_were_ranked_for", function()
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    -- The user could only have typed past the row if the host had already
    -- taken the key away, so the edit names the snapshot the row was ranked
    -- for and nothing the input has become since.
    Menu.accept()
  end)
  eq(#h.edits, 1)
  eq(h.edits[1].version, INPUT.version, "the version the row was ranked against")
  eq(h.edits[1].session_id, SESSION)
  eq(h.edits[1].stop, INPUT.cursor, "the mention as it stood, not as it stands")
end)

case("accept_writes_nothing_when_an_autocmd_took_the_popup_first", function()
  local h = harness(function()
    eq(Menu.session_id(), nil)
    Menu.accept()
  end)
  eq(#h.edits, 0, "the key is spent rather than handed back a round trip later")
end)

-- A refused edit is the host saying the input is not the one these rows answer
-- any more. The key is spent either way, but the user pressed Enter on a row
-- and has no path to show for it, so the host's reason is passed on rather
-- than dropped.
case("accept_reports_a_refused_edit_instead_of_dropping_it", function()
  local REFUSED = "input changed since version 3"
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    hh.refuse_edit = REFUSED
    Menu.accept()
  end)
  eq(#h.edits, 1, "the edit was tried")
  eq(Menu.session_id(), nil, "and the popup went either way")
  eq(#h.flashes, 1, "the refusal reached the user")
  th.has(h.flashes[1], REFUSED, "with the host's reason in it")
end)

-- The keys are declared once, on the window, and the host routes them here
-- while it is up. Unfocused is the whole point: the user goes on typing into
-- the chat input under the popup.
case("the_popup_declares_its_keys_on_the_window_it_draws_in", function()
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    eq(hh.focus, false, "the chat input keeps the keyboard")
    th.has(table.concat(hh.claimed, " "), "<CR>")
    th.has(table.concat(hh.claimed, " "), "<Esc>")
    th.has(table.concat(hh.claimed, " "), "<C-p>")
  end)
  eq(h.closes, 1, "and the close is what hands them back")
end)

-- Nothing is re-asserted on a keystroke: the claim lives on the window, so a
-- popup that narrows on screen is still the same window holding the same keys.
case("a_refresh_reuses_the_window_and_its_keys", function()
  local NEWER = { text = "explain @src/main", cursor = 17, version = 9, session_id = SESSION }
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    Menu.refresh(NEWER)
  end)
  eq(h.opens, 1, "one window for both refreshes")
  eq(h.closes, 1, "closed once, at teardown")
end)

-- The window a keystroke opens on. The press lands, the popup is still showing
-- rows ranked for the text before it, and the refresh that answers it has not
-- run yet. The popup holds its keys throughout, so nothing falls through to
-- the chat input under it, and the accept it makes in that moment is weighed
-- by `input_edit` rather than by the plugin.
case("the_popup_keeps_its_keys_while_a_refresh_is_in_flight", function()
  local NEWER = { text = "explain @src/mai", cursor = 16, version = 4, session_id = SESSION }
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    hh.defer = true

    Events.input_changed(NEWER)
    eq(Menu.session_id(), SESSION, "the popup is still up")
    Menu.handle_key("<CR>")

    eq(#hh.edits, 1, "and it answered the key its footer advertises")
    eq(hh.edits[1].version, INPUT.version, "with the snapshot its rows were ranked for")
  end)
  eq(h.opens, 1, "one window throughout")
end)

-- One round trip per keystroke, and it is the ranking. Reading the input back
-- and re-reading the session would be three, and every one of them is time the
-- user spends looking at rows that answer an older keystroke.
case("a_keystroke_costs_one_round_trip", function()
  local NEWER = { text = "explain @src/mai", cursor = 16, version = 4, session_id = SESSION }
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    eq(hh.session_reads, 1, "the first look reads the session's tree")
    Menu.refresh(NEWER)
  end)
  eq(h.input_calls, 0, "the event payload is the input, so nothing reads it back")
  eq(h.session_reads, 1, "and the tree cannot move under a popup that is up")
end)

-- Enter on nothing closes the popup and sends no message. The user's next
-- Enter is the chat input's again, because the keys went with the window: no
-- agreement with the host about who holds `<CR>` this frame, and no race.
case("enter_with_nothing_to_insert_closes_the_popup_and_writes_nothing", function()
  local h = harness(function(hh)
    hh.found = { complete = true, items = {} }
    Menu.refresh(hh.input)
    Menu.handle_key("<CR>")
    eq(Menu.session_id(), nil, "the popup went")
  end)
  eq(#h.edits, 0, "and nothing was inserted")
  eq(h.closes, 1)
end)

-- On a large tree the first look lands before the walk does, so an empty list
-- is not an answer yet. Closing on it would cost the user the popup a
-- keystroke before the rows they asked for arrive, and the `scanning…` row is
-- the popup saying so.
case("enter_while_the_walk_is_still_running_holds_the_popup_open", function()
  local h = harness(function(hh)
    hh.found = { complete = false, root = ROOT, items = {} }
    Menu.refresh(hh.input)
    eq(row(hh, 1), " " .. SCANNING)

    Menu.handle_key("<CR>")
    eq(Menu.session_id(), SESSION, "the popup is still up for the rows on their way")
  end)
  eq(#h.edits, 0, "and nothing was inserted")
  eq(h.closes, 1, "closed once, at teardown")
end)

-- A refresh that lost to a close must paint nothing and open nothing. Its
-- rows would be for a popup that is no longer there.
case("a_close_during_a_refresh_leaves_nothing_behind", function()
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    hh.before_rank = Menu.close
    Menu.refresh(hh.input)
  end)
  eq(Menu.session_id(), nil)
  eq(h.opens, 1, "no window is opened for a popup that went")
  eq(h.closes, 1)
end)

-- Moving the caret out of the mention changes no text at all. Until the host
-- reported cursor-only moves the popup stayed up over a mention the user had
-- left, holding `<Tab>`, `<C-p>`, `<Esc>` and `<CR>` with it.

case("the_popup_closes_when_the_caret_leaves_the_mention", function()
  harness(function(h)
    Menu.refresh(h.input)
    eq(Menu.session_id(), SESSION, "the popup is up on the mention")

    Menu.refresh({ text = INPUT.text, cursor = 0, version = INPUT.version, session_id = SESSION })
    eq(Menu.session_id(), nil, "the same text, and the popup gave its keys back")
  end)
end)

-- A teardown step that raises must not strand the others: an error
-- unsubscribing used to leave the float on screen holding five keys.

case("a_failed_teardown_step_still_takes_the_popup_down", function()
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    hh.on_close = function()
      error(HOST_GONE)
    end
    Menu.close()
    hh.on_close = nil
  end)
  eq(h.closes, 1, "the window was told to go")
  eq(Menu.session_id(), nil)
  eq(#h.warnings, 1, "and the failure was reported rather than swallowed")
end)

-- A keypress that arrives for a popup that has gone belongs to nothing: the
-- key loop reads it after the close it was queued behind.
case("a_key_for_a_popup_that_closed_does_nothing", function()
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    Menu.close()
    Menu.handle_key("<CR>")
  end)
  eq(#h.edits, 0)
  eq(Menu.session_id(), nil)
end)

-- The row the popup is highlighting, which is what a navigation key moves and
-- what Enter inserts.
local function selected(h)
  for i, painted in ipairs(h.rows) do
    if painted[1][2] == "selected" then
      return i
    end
  end
  return nil
end

-- A claim the popup never answers is a key taken from the user and dropped,
-- so this presses every binding and checks the window claimed it too.
case("every_claimed_key_is_answered", function()
  for _, binding in ipairs(Menu.BINDINGS) do
    harness(function(hh)
      hh.found = {
        complete = true,
        root = ROOT,
        items = { { path = MATCH }, { path = OTHER_MATCH } },
      }
      Menu.refresh(hh.input)
      th.has(table.concat(hh.claimed, " "), binding.claim, "the window claims " .. binding.claim)

      local before = selected(hh)
      Menu.handle_key(binding.claim)
      local answered = Menu.session_id() == nil or selected(hh) ~= before
      eq(answered, true, binding.claim .. " reached a handler")
    end)
  end
end)

-- Navigation is the other half of what the claimed keys are for.
case("the_navigation_keys_move_the_highlight", function()
  local h = harness(function(hh)
    hh.found = {
      complete = true,
      root = ROOT,
      items = { { path = MATCH }, { path = OTHER_MATCH } },
    }
    Menu.refresh(hh.input)
    Menu.handle_key("<Tab>")
    Menu.handle_key("<CR>")
  end)
  eq(h.edits[1].text, OTHER_MATCH .. " ", "Tab moved the highlight down a row")
end)

-- Esc is the key the popup shares with the host: while the agent streams the
-- host arms its cancel on Esc, and the popup in front takes the first press.
case("esc_closes_the_popup", function()
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    Menu.handle_key("<Esc>")
    eq(Menu.session_id(), nil)
  end)
  eq(h.closes, 1)
end)

-- A handler that raises is the host's problem: it logs and the key is spent,
-- because replaying a keystroke because a plugin has a bug lands it in a UI
-- that has moved on. What the plugin owes is that raising wedges nothing, so
-- the next keystroke still finds a working popup.

case("a_handler_that_raises_leaves_the_popup_usable", function()
  local h = harness(function(hh)
    Menu.refresh(hh.input)
    hh.raise_edit = HOST_GONE
    eq(pcall(Menu.accept), false, "the raise reaches the host")

    hh.raise_edit = nil
    Menu.refresh(hh.input)
    eq(Menu.session_id(), SESSION, "and the popup is back on the next keystroke")
  end)
  eq(row(h, 1), " " .. MATCH)
end)

-- The index answers out of a half-filled walk, so a popup that only looks on
-- a keystroke shows `scanning…` for as long as the user waits to see what is
-- there. `FileIndexReady` is what tells it the walk is in.

case("the_scanning_row_fills_in_without_another_keystroke", function()
  local h = harness(function(hh)
    hh.found = { complete = false, root = ROOT, items = {} }
    Menu.refresh(hh.input)
    eq(row(hh, 1), " " .. SCANNING)

    hh.found = { complete = true, root = ROOT, items = { { path = MATCH } } }
    walk_landed(hh)
    eq(row(hh, 1), " " .. MATCH, "the walk landed and the popup repainted itself")
  end)
  eq(h.subscriptions, 1, "one subscription, and it went when the walk was in")
  eq(h.on_index_ready, nil)
end)

-- Every session indexes its own tree and every walk fires the same event. The
-- ranking names the root it resolved, so a walk of somebody else's tree is one
-- the popup can leave alone instead of paying for a ranking that answers the
-- same `scanning…` row back.

case("a_walk_of_another_tree_leaves_the_rows_waiting", function()
  local h = harness(function(hh)
    hh.found = { complete = false, root = ROOT, items = {} }
    Menu.refresh(hh.input)
    eq(row(hh, 1), " " .. SCANNING)

    hh.found = { complete = true, root = ROOT, items = { { path = MATCH } } }
    walk_landed(hh, OTHER_ROOT)
    eq(row(hh, 1), " " .. SCANNING, "that walk was not of the tree these rows came from")
    eq(type(hh.on_index_ready), "function", "and the popup still waits on its own")

    walk_landed(hh, ROOT)
    eq(row(hh, 1), " " .. MATCH, "which fills the rows in when it lands")
  end)
  eq(h.subscriptions, 1, "one subscription for both walks")
  eq(h.on_index_ready, nil)
end)

-- The walk can land between the ranking and the subscription, and that event
-- has no subscriber. Without one more look the rows then say `scanning…` until
-- the user presses a key, which is the opposite of what the docs promise.

case("a_walk_that_landed_before_the_subscription_still_fills_the_rows", function()
  local h = harness(function(hh)
    local looks = 0
    maki.fs.fuzzy_files = function()
      looks = looks + 1
      if looks == 1 then
        return { complete = false, root = ROOT, items = {} }
      end
      return { complete = true, root = ROOT, items = { { path = MATCH } } }
    end
    Menu.refresh(hh.input)
    eq(row(hh, 1), " " .. MATCH, "the look after subscribing found the walk already in")
  end)
  eq(h.subscriptions, 1, "it subscribed once")
  eq(h.on_index_ready, nil, "and stopped listening once the walk was in")
end)

-- A walk the host let go under its ceiling of indexed roots never fires, and
-- the popup used to hold `scanning…` and eat every Enter for the rest of its
-- life. The wait is bounded, and the rows on screen when it runs out are the
-- answer.

case("a_walk_that_never_lands_stops_holding_enter", function()
  local h = harness(function(hh)
    hh.found = { complete = false, root = ROOT, items = {} }
    Menu.refresh(hh.input)
    eq(row(hh, 1), " " .. SCANNING)
    eq(#hh.timers, 1, "one deadline, armed with the subscription")

    hh.fire_timers()
    eq(row(hh, 1), " " .. NO_MATCHES, "the popup stops saying rows are on their way")
    eq(hh.on_index_ready, nil, "and stops listening for a walk that is not coming")

    Menu.handle_key("<CR>")
    eq(Menu.session_id(), nil, "Enter closes the popup, the way it does on an empty list")
  end)
  eq(#h.edits, 0, "and nothing was inserted")
end)

case("the_walk_subscription_goes_with_the_popup", function()
  local h = harness(function(hh)
    hh.found = { complete = false, root = ROOT, items = {} }
    Menu.refresh(hh.input)
    eq(type(hh.on_index_ready), "function")
    Menu.close()
  end)
  eq(h.on_index_ready, nil, "nothing is left listening for a popup that is gone")
end)

case("a_walk_that_lands_for_a_closed_popup_repaints_nothing", function()
  local h = harness(function(hh)
    hh.found = { complete = false, root = ROOT, items = {} }
    Menu.refresh(hh.input)
    local stale = hh.on_index_ready
    Menu.close()

    hh.found = { complete = true, root = ROOT, items = { { path = MATCH } } }
    stale({ data = { root = ROOT } })
    eq(Menu.session_id(), nil, "the walk does not reopen what a close took down")
  end)
  eq(h.subscriptions, 1)
end)

-- `init.lua` is wiring; every decision it makes lives in `events.lua`, which
-- is what these exercise.

case("events_ignore_this_plugins_own_edit", function()
  harness(function()
    Events.input_changed({
      text = INPUT.text,
      cursor = INPUT.cursor,
      version = INPUT.version,
      session_id = SESSION,
      source = Events.PLUGIN,
    })
    eq(Menu.session_id(), nil, "an accept must not reopen the popup on what it inserted")
  end)
end)

case("events_ignore_a_caret_move_with_no_popup_up", function()
  harness(function()
    Events.input_changed({
      text = INPUT.text,
      cursor = INPUT.cursor,
      version = INPUT.version,
      session_id = SESSION,
      cursor_only = true,
    })
    eq(Menu.session_id(), nil, "arrowing back over a dismissed `@` must not reopen it")
  end)
end)

case("events_let_a_caret_move_close_a_popup_that_is_up", function()
  harness(function(hh)
    Menu.refresh(hh.input)
    eq(Menu.session_id(), SESSION)

    Events.input_changed({
      text = INPUT.text,
      cursor = 0,
      version = INPUT.version,
      session_id = SESSION,
      cursor_only = true,
    })
    eq(Menu.session_id(), nil, "the caret left the mention the popup was anchored to")
  end)
end)

case("events_close_a_popup_when_another_tab_takes_focus", function()
  harness(function(hh)
    Menu.refresh(hh.input)
    eq(Menu.session_id(), SESSION)

    Events.input_changed({
      text = "no mention here",
      cursor = 15,
      version = 1,
      session_id = OTHER_SESSION,
    })
    eq(Menu.session_id(), nil, "the popup was placed against a caret in the tab being left")
  end)
end)

case("events_close_a_popup_when_the_session_it_belongs_to_resets", function()
  harness(function(hh)
    Menu.refresh(hh.input)
    Events.session_reset({ session_id = OTHER_SESSION })
    eq(Menu.session_id(), SESSION, "`/new` in another tab leaves this one alone")

    Events.session_reset({ session_id = SESSION })
    eq(Menu.session_id(), nil)
  end)
end)

-- A permission prompt takes the chat input off screen, and the host then
-- refuses to edit it. Leaving the popup up leaves it holding keys for an
-- accept that cannot land.
case("events_close_a_popup_when_the_input_goes_off_screen", function()
  harness(function(hh)
    Menu.refresh(hh.input)
    Events.session_status_changed({ focused = false, status = "needs_input" })
    eq(Menu.session_id(), SESSION, "another tab's prompt is not this tab's")

    Events.session_status_changed({ focused = true, status = "needs_input" })
    eq(Menu.session_id(), nil)
  end)
end)

th.report()
